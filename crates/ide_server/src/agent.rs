use crate::protocol::{
    AgentCatalog, AgentInfo, AuthMethodInfo, CatalogAgent, Event, Outgoing, PermissionOption,
    PlanEntry, TerminalLogin, ThreadEntry, ThreadList, ThreadSnapshot, ThreadStatus,
    ThreadSummary,
};
use acp_thread::{
    AcpThread, AcpThreadEvent, AgentConnection, AgentSessionListRequest, AgentThreadEntry,
    AssistantMessageChunk, PermissionOptions, SelectedPermissionOutcome, ToolCallStatus,
};
use agent_client_protocol::schema as acp;
use agent_servers::{AgentServer, AgentServerDelegate, CustomAgentServer};
use anyhow::{Context as _, Result, anyhow};
use async_channel::Sender;
use gpui::{App, AsyncApp, Context, Entity, Subscription, Task, WeakEntity};
use project::agent_server_store::AllAgentServersSettings;
use project::{AgentId, AgentRegistryStore, ExternalAgentSource, Project};
use serde::Serialize;
use settings::SettingsStore;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;
use util::ResultExt as _;
use util::path_list::PathList;

const THREAD_UPDATE_DEBOUNCE: Duration = Duration::from_millis(40);

struct OpenThread {
    agent_id: String,
    thread: Entity<AcpThread>,
    _subscription: Subscription,
    pending_update: Option<Task<()>>,
}

/// Owns the ACP agent connections and open threads for the workspace, and
/// broadcasts thread changes to every attached WebSocket client. Threads
/// outlive individual clients so a page reload does not drop a running turn.
pub struct AgentHub {
    project: Entity<Project>,
    root: PathBuf,
    connections: HashMap<String, Rc<dyn AgentConnection>>,
    threads: HashMap<String, OpenThread>,
    listeners: Vec<Sender<Outgoing>>,
}

impl AgentHub {
    pub fn new(project: Entity<Project>, root: PathBuf) -> Self {
        Self {
            project,
            root,
            connections: HashMap::new(),
            threads: HashMap::new(),
            listeners: Vec::new(),
        }
    }

    pub fn attach(&mut self, listener: Sender<Outgoing>) {
        self.listeners.retain(|listener| !listener.is_closed());
        self.listeners.push(listener);
    }

    pub fn agents(&self, cx: &App) -> Vec<AgentInfo> {
        let store = self.project.read(cx).agent_server_store().read(cx);
        let mut agents: Vec<AgentInfo> = store
            .external_agents()
            .map(|id| AgentInfo {
                id: id.0.to_string(),
                name: store
                    .agent_display_name(id)
                    .map(|name| name.to_string())
                    .unwrap_or_else(|| id.0.to_string()),
                source: match store.agent_source(id) {
                    Some(ExternalAgentSource::Registry) => "registry",
                    Some(ExternalAgentSource::Extension) => "extension",
                    Some(ExternalAgentSource::Custom) | None => "custom",
                },
            })
            .collect();
        agents.sort_by(|left, right| left.name.cmp(&right.name));
        agents
    }

    /// The ACP registry (Claude Agent, Codex, Gemini CLI, …) for adding an
    /// agent from the IDE, marking what `agent_servers` already has. A stale
    /// or empty cache starts a refresh; `fetching` tells the UI to ask again.
    pub fn catalog(&self, cx: &mut Context<Self>) -> AgentCatalog {
        let registry = AgentRegistryStore::global(cx);
        registry.update(cx, |registry, cx| registry.refresh_if_stale(cx));
        let configured = cx
            .global::<SettingsStore>()
            .get::<AllAgentServersSettings>(None);
        let registry = registry.read(cx);
        AgentCatalog {
            agents: registry
                .agents()
                .iter()
                .map(|agent| CatalogAgent {
                    id: agent.id().0.to_string(),
                    name: agent.name().to_string(),
                    description: agent.description().to_string(),
                    supported: agent.supports_current_platform(),
                    added: configured.contains_key(agent.id().0.as_ref()),
                })
                .collect(),
            fetching: registry.is_fetching(),
            error: registry.fetch_error().map(|error| error.to_string()),
        }
    }

    /// Adds `id` under `agent_servers` in the user's Zed settings: an ACP
    /// registry agent, or any ACP command when `command` is set. Zed's
    /// settings writer keeps the file's comments and reloads the settings
    /// store, which re-registers agents — live here without a restart, and
    /// in the user's Zed too.
    pub fn add_agent(
        &mut self,
        id: String,
        command: Option<String>,
        args: Vec<String>,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        // A replaced entry must not keep serving the old process.
        self.connections.remove(&id);
        // The settings-file shape (`settings::…`), not the same-named
        // runtime types in `project::agent_server_store`.
        let entry = match command {
            Some(command) => settings::CustomAgentServerSettings::Custom {
                path: command.into(),
                args,
                env: Default::default(),
                default_mode: None,
                default_model: None,
                favorite_models: Vec::new(),
                default_config_options: Default::default(),
                favorite_config_option_values: Default::default(),
            },
            None => settings::CustomAgentServerSettings::Registry {
                env: Default::default(),
                default_mode: None,
                default_model: None,
                favorite_models: Vec::new(),
                default_config_options: Default::default(),
                favorite_config_option_values: Default::default(),
            },
        };
        self.update_agent_servers(cx, move |servers| {
            servers.insert(id, entry);
        })
    }

    pub fn remove_agent(&mut self, id: String, cx: &mut Context<Self>) -> Task<Result<()>> {
        self.connections.remove(&id);
        self.update_agent_servers(cx, move |servers| {
            servers.remove(&id);
        })
    }

    fn update_agent_servers(
        &self,
        cx: &mut Context<Self>,
        edit: impl 'static + Send + FnOnce(&mut settings::AllAgentServersSettings),
    ) -> Task<Result<()>> {
        let fs = self.project.read(cx).fs().clone();
        cx.spawn(async move |_, cx| {
            // Zed's writer puts its temp file beside settings.json, so a
            // machine that has never run Zed needs the directory first.
            if let Some(dir) = paths::settings_file().parent()
                && !fs.is_dir(dir).await
            {
                fs.create_dir(dir).await?;
            }
            let written = cx.update(|cx| {
                settings::update_settings_file_with_completion(fs, cx, move |content, _| {
                    edit(content.agent_servers.get_or_insert_default());
                })
            });
            written.await.context("settings write was dropped")?
        })
    }

    /// How `agent_id` signs in: its ACP auth methods, with the login command
    /// resolved for methods that run in a terminal (the UI runs those in its
    /// terminal panel; the others go through `authenticate`).
    pub async fn auth_methods(
        this: &Entity<Self>,
        agent_id: String,
        cx: &mut AsyncApp,
    ) -> Result<Vec<AuthMethodInfo>> {
        let connection = Self::connect(this, agent_id, cx).await?;
        let pending = cx.update(|cx| {
            connection
                .auth_methods()
                .iter()
                .map(|method| {
                    (
                        method.id().0.to_string(),
                        method.name().to_string(),
                        method.description().map(str::to_owned),
                        connection.terminal_auth_task(method.id(), cx),
                    )
                })
                .collect::<Vec<_>>()
        });
        let mut methods = Vec::with_capacity(pending.len());
        for (id, name, description, terminal) in pending {
            let terminal = match terminal {
                Some(task) => {
                    let spawn = task.await?;
                    Some(TerminalLogin {
                        command: spawn.command.unwrap_or_default(),
                        args: spawn.args,
                        env: spawn.env.into_iter().collect(),
                        label: spawn.label,
                    })
                }
                None => None,
            };
            methods.push(AuthMethodInfo {
                id,
                name,
                description,
                terminal,
            });
        }
        Ok(methods)
    }

    pub async fn authenticate(
        this: &Entity<Self>,
        agent_id: String,
        method_id: String,
        cx: &mut AsyncApp,
    ) -> Result<()> {
        let connection = Self::connect(this, agent_id, cx).await?;
        cx.update(|cx| connection.authenticate(acp::AuthMethodId::new(method_id), cx))
            .await
    }

    fn connect(
        this: &Entity<Self>,
        agent_id: String,
        cx: &mut AsyncApp,
    ) -> Task<Result<Rc<dyn AgentConnection>>> {
        let existing = this.read_with(cx, |hub, _| hub.connections.get(&agent_id).cloned());
        if let Some(connection) = existing {
            return Task::ready(Ok(connection));
        }
        let (project, has_agent) = this.read_with(cx, |hub, cx| {
            let has_agent = hub
                .project
                .read(cx)
                .agent_server_store()
                .read(cx)
                .external_agents()
                .any(|id| id.0.as_ref() == agent_id);
            (hub.project.clone(), has_agent)
        });
        if !has_agent {
            return Task::ready(Err(anyhow!("unknown agent {agent_id:?}")));
        }
        let connect = cx.update(|cx| {
            let store = project.read(cx).agent_server_store().clone();
            let delegate = AgentServerDelegate::new(store, None);
            let server = CustomAgentServer::new(AgentId(agent_id.clone().into()));
            server.connect(delegate, project, cx)
        });
        let this = this.clone();
        cx.spawn(async move |cx| {
            let connection = connect.await?;
            this.update(cx, |hub, _| {
                hub.connections
                    .insert(agent_id, connection.clone());
            });
            Ok(connection)
        })
    }

    pub async fn list_threads(
        this: &Entity<Self>,
        agent_id: String,
        cx: &mut AsyncApp,
    ) -> Result<ThreadList> {
        let connection = Self::connect(this, agent_id.clone(), cx).await?;
        let root = this.read_with(cx, |hub, _| hub.root.clone());
        let session_list = cx.update(|cx| connection.session_list(cx));
        let Some(session_list) = session_list else {
            return Ok(ThreadList {
                agent_id,
                supports_history: false,
                supports_delete: false,
                threads: Vec::new(),
            });
        };
        let (response, supports_delete) = cx.update(|cx| {
            let request = AgentSessionListRequest {
                cwd: Some(root),
                cursor: None,
                meta: None,
            };
            (
                session_list.list_sessions(request, cx),
                session_list.supports_delete(cx),
            )
        });
        let response = response.await?;
        let threads = response
            .sessions
            .into_iter()
            .map(|session| ThreadSummary {
                session_id: session.session_id.to_string(),
                title: session.title.map(|title| title.to_string()),
                updated_at: session.updated_at.map(|time| time.to_rfc3339()),
                work_dirs: session
                    .work_dirs
                    .map(|dirs| {
                        dirs.paths()
                            .iter()
                            .map(|path| path.to_string_lossy().into_owned())
                            .collect()
                    })
                    .unwrap_or_default(),
            })
            .collect();
        Ok(ThreadList {
            agent_id,
            supports_history: true,
            supports_delete,
            threads,
        })
    }

    pub async fn open_thread(
        this: &Entity<Self>,
        agent_id: String,
        session_id: Option<String>,
        cx: &mut AsyncApp,
    ) -> Result<ThreadSnapshot> {
        if let Some(session_id) = &session_id {
            let snapshot = this.read_with(cx, |hub, cx| hub.snapshot(session_id, cx));
            if let Some(snapshot) = snapshot {
                return Ok(snapshot);
            }
        }
        let connection = Self::connect(this, agent_id.clone(), cx).await?;
        let (project, root) = this.read_with(cx, |hub, _| (hub.project.clone(), hub.root.clone()));
        let work_dirs = PathList::new(&[root]);
        let open = cx.update(|cx| match session_id {
            Some(session_id) => connection.load_session(
                acp::SessionId::from(session_id),
                project,
                work_dirs,
                None,
                cx,
            ),
            None => connection.new_session(project, work_dirs, cx),
        });
        let thread = open.await?;
        this.update(cx, |hub, cx| {
            let thread_id = thread.read(cx).session_id().to_string();
            let subscription = cx.subscribe(&thread, Self::handle_thread_event);
            hub.threads.insert(
                thread_id.clone(),
                OpenThread {
                    agent_id,
                    thread,
                    _subscription: subscription,
                    pending_update: None,
                },
            );
            hub.snapshot(&thread_id, cx)
                .ok_or_else(|| anyhow!("thread {thread_id} vanished while opening"))
        })
    }

    pub fn thread_snapshot(&self, thread_id: &str, cx: &App) -> Result<ThreadSnapshot> {
        self.snapshot(thread_id, cx)
            .with_context(|| format!("unknown thread {thread_id}"))
    }

    pub fn send(&mut self, thread_id: &str, text: String, cx: &mut Context<Self>) -> Result<()> {
        let thread = self.thread(thread_id)?.clone();
        let thread_id = thread_id.to_string();
        let prompt = thread.update(cx, |thread, cx| {
            thread.send(vec![acp::ContentBlock::Text(acp::TextContent::new(text))], cx)
        });
        cx.spawn(async move |this, cx| {
            if let Err(error) = prompt.await {
                this.update(cx, |hub, _| {
                    hub.broadcast(Event::ThreadError {
                        thread_id,
                        message: format!("{error:#}"),
                    })
                })
                .log_err();
            }
        })
        .detach();
        Ok(())
    }

    pub fn cancel(&mut self, thread_id: &str, cx: &mut Context<Self>) -> Result<()> {
        let thread = self.thread(thread_id)?.clone();
        thread.update(cx, |thread, cx| thread.cancel(cx)).detach();
        Ok(())
    }

    pub fn authorize(
        &mut self,
        thread_id: &str,
        tool_call_id: &str,
        option_id: &str,
        cx: &mut Context<Self>,
    ) -> Result<()> {
        let thread = self.thread(thread_id)?.clone();
        let tool_call_id = acp::ToolCallId::from(tool_call_id.to_string());
        let option_kind = thread.read_with(cx, |thread, _| {
            thread.entries().iter().find_map(|entry| match entry {
                AgentThreadEntry::ToolCall(tool_call) if tool_call.id == tool_call_id => {
                    match &tool_call.status {
                        ToolCallStatus::WaitingForConfirmation { options, .. } => {
                            permission_options(options)
                                .into_iter()
                                .find(|option| option.option_id.0.as_ref() == option_id)
                                .map(|option| option.kind)
                        }
                        _ => None,
                    }
                }
                _ => None,
            })
        });
        let option_kind = option_kind.with_context(|| {
            format!("tool call {tool_call_id} is not waiting for option {option_id:?}")
        })?;
        let outcome = SelectedPermissionOutcome::new(
            acp::PermissionOptionId::from(option_id.to_string()),
            option_kind,
        );
        thread.update(cx, |thread, cx| {
            thread.authorize_tool_call(tool_call_id, outcome, cx)
        });
        Ok(())
    }

    pub fn close(&mut self, thread_id: &str) -> Result<()> {
        self.threads
            .remove(thread_id)
            .map(|_| ())
            .with_context(|| format!("unknown thread {thread_id}"))
    }

    pub async fn delete_thread(
        this: &Entity<Self>,
        agent_id: String,
        session_id: String,
        cx: &mut AsyncApp,
    ) -> Result<()> {
        let connection = Self::connect(this, agent_id, cx).await?;
        let delete = cx.update(|cx| {
            let session_list = connection
                .session_list(cx)
                .context("agent does not expose session history")?;
            anyhow::ensure!(
                session_list.supports_delete(cx),
                "agent does not support deleting sessions"
            );
            Ok(session_list.delete_session(&acp::SessionId::from(session_id.clone()), cx))
        })?;
        delete.await?;
        this.update(cx, |hub, _| {
            hub.threads.remove(&session_id);
        });
        Ok(())
    }

    fn thread(&self, thread_id: &str) -> Result<&Entity<AcpThread>> {
        self.threads
            .get(thread_id)
            .map(|open| &open.thread)
            .with_context(|| format!("unknown thread {thread_id}"))
    }

    fn handle_thread_event(
        &mut self,
        thread: Entity<AcpThread>,
        event: &AcpThreadEvent,
        cx: &mut Context<Self>,
    ) {
        let thread_id = thread.read(cx).session_id().to_string();
        if let AcpThreadEvent::LoadError(error) = event {
            self.broadcast(Event::ThreadError {
                thread_id: thread_id.clone(),
                message: error.to_string(),
            });
        }
        let Some(open) = self.threads.get_mut(&thread_id) else {
            return;
        };
        if open.pending_update.is_some() {
            return;
        }
        open.pending_update = Some(cx.spawn(async move |this: WeakEntity<Self>, cx| {
            cx.background_executor().timer(THREAD_UPDATE_DEBOUNCE).await;
            this.update(cx, |hub, cx| {
                if let Some(open) = hub.threads.get_mut(&thread_id) {
                    open.pending_update = None;
                }
                if let Some(snapshot) = hub.snapshot(&thread_id, cx) {
                    hub.broadcast(Event::ThreadUpdated { thread: snapshot });
                }
            })
            .log_err();
        }));
    }

    fn broadcast(&mut self, event: Event) {
        self.listeners.retain(|listener| !listener.is_closed());
        for listener in &self.listeners {
            if let Err(error) = listener.try_send(Outgoing::Event(event.clone())) {
                log::debug!("dropping thread event for closed client: {error}");
            }
        }
    }

    fn snapshot(&self, thread_id: &str, cx: &App) -> Option<ThreadSnapshot> {
        let open = self.threads.get(thread_id)?;
        let thread = open.thread.read(cx);
        Some(ThreadSnapshot {
            thread_id: thread_id.to_string(),
            agent_id: open.agent_id.clone(),
            title: thread.title().map(|title| title.to_string()),
            status: match thread.status() {
                acp_thread::ThreadStatus::Idle => ThreadStatus::Idle,
                acp_thread::ThreadStatus::Generating => ThreadStatus::Generating,
            },
            had_error: thread.had_error(),
            entries: thread
                .entries()
                .iter()
                .map(|entry| thread_entry(entry, cx))
                .collect(),
        })
    }
}

/// Path-ish strings out of a tool call's raw input: values under
/// path-named keys, plus any slash-containing whitespace-free string.
fn collect_input_paths(value: &serde_json::Value, out: &mut Vec<String>) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, val) in map {
                if let serde_json::Value::String(text) = val {
                    let path_key = matches!(
                        key.as_str(),
                        "file_path" | "filePath" | "path" | "abs_path" | "notebook_path" | "cwd"
                    );
                    if path_key || (text.contains('/') && !text.contains(char::is_whitespace) && text.len() < 300) {
                        out.push(text.clone());
                    }
                } else {
                    collect_input_paths(val, out);
                }
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                collect_input_paths(item, out);
            }
        }
        _ => {}
    }
}

fn thread_entry(entry: &AgentThreadEntry, cx: &App) -> ThreadEntry {
    match entry {
        AgentThreadEntry::UserMessage(message) => ThreadEntry::User {
            markdown: message.content.to_markdown(cx).to_string(),
        },
        AgentThreadEntry::AssistantMessage(message) => {
            let mut markdown = String::new();
            let mut thoughts = String::new();
            for chunk in &message.chunks {
                match chunk {
                    AssistantMessageChunk::Message { block } => {
                        markdown.push_str(block.to_markdown(cx))
                    }
                    AssistantMessageChunk::Thought { block } => {
                        thoughts.push_str(block.to_markdown(cx))
                    }
                }
            }
            ThreadEntry::Assistant { markdown, thoughts }
        }
        AgentThreadEntry::ToolCall(tool_call) => {
            let options = match &tool_call.status {
                ToolCallStatus::WaitingForConfirmation { options, .. } => {
                    permission_options(options)
                        .into_iter()
                        .map(|option| PermissionOption {
                            id: option.option_id.to_string(),
                            label: option.name.clone(),
                            option_kind: snake_case(&option.kind),
                        })
                        .collect()
                }
                _ => Vec::new(),
            };
            let status = match &tool_call.status {
                ToolCallStatus::Pending => "pending",
                ToolCallStatus::WaitingForConfirmation { .. } => "waiting_for_confirmation",
                ToolCallStatus::InProgress => "in_progress",
                ToolCallStatus::Completed => "completed",
                ToolCallStatus::Failed => "failed",
                ToolCallStatus::Rejected => "rejected",
                ToolCallStatus::Canceled => "canceled",
            };
            ThreadEntry::ToolCall {
                id: tool_call.id.to_string(),
                title: tool_call.label.read(cx).source().to_string(),
                tool_kind: snake_case(&tool_call.kind),
                status: status.to_string(),
                markdown: tool_call
                    .content
                    .iter()
                    .map(|content| content.to_markdown(cx))
                    .collect::<Vec<_>>()
                    .join("\n\n"),
                permission_options: options,
                locations: {
                    // Explicit ACP locations when the agent reports
                    // them, plus paths mined from the tool call's raw
                    // JSON input (file_path/path args, grep targets) —
                    // coverage varies by agent, the input never lies.
                    let mut paths: Vec<String> = tool_call
                        .locations
                        .iter()
                        .map(|location| location.path.display().to_string())
                        .collect();
                    if let Some(input) = &tool_call.raw_input {
                        collect_input_paths(input, &mut paths);
                    }
                    paths.dedup();
                    paths
                },
            }
        }
        AgentThreadEntry::CompletedPlan(entries) => ThreadEntry::Plan {
            entries: entries
                .iter()
                .map(|entry| PlanEntry {
                    content: entry.content.read(cx).source().to_string(),
                    priority: snake_case(&entry.priority),
                    status: snake_case(&entry.status),
                })
                .collect(),
        },
    }
}

fn permission_options(options: &PermissionOptions) -> Vec<&acp::PermissionOption> {
    match options {
        PermissionOptions::Flat(options) => options.iter().collect(),
        PermissionOptions::Dropdown(choices)
        | PermissionOptions::DropdownWithPatterns { choices, .. } => choices
            .iter()
            .flat_map(|choice| [&choice.allow, &choice.deny])
            .collect(),
    }
}

/// ACP enums serialize as snake_case strings; reuse that spelling on the wire.
fn snake_case<T: Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| "unknown".to_string())
}
