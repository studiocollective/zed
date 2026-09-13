use serde::{Deserialize, Serialize};

/// A request from the UI. `id` is echoed back on the matching response.
#[derive(Debug, Deserialize)]
pub struct Request {
    pub id: u64,
    #[serde(flatten)]
    pub command: Command,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "cmd", rename_all = "kebab-case")]
pub enum Command {
    #[serde(rename = "workspace.info")]
    WorkspaceInfo,
    #[serde(rename = "worktree.entries")]
    WorktreeEntries {
        #[serde(default)]
        path: String,
    },
    #[serde(rename = "buffer.read")]
    BufferRead { path: String },
    #[serde(rename = "git.status")]
    GitStatus,
    #[serde(rename = "settings.all")]
    SettingsAll,
    #[serde(rename = "settings.set")]
    SettingsSet { key: String, value: serde_json::Value },
    #[serde(rename = "terminal.create")]
    TerminalCreate { cols: u16, rows: u16 },
    #[serde(rename = "terminal.input", rename_all = "camelCase")]
    TerminalInput { terminal_id: u64, data: String },
    #[serde(rename = "terminal.resize", rename_all = "camelCase")]
    TerminalResize {
        terminal_id: u64,
        cols: u16,
        rows: u16,
    },
    #[serde(rename = "terminal.close", rename_all = "camelCase")]
    TerminalClose { terminal_id: u64 },
    #[serde(rename = "agent.list")]
    AgentList,
    #[serde(rename = "thread.list", rename_all = "camelCase")]
    ThreadList { agent_id: String },
    #[serde(rename = "thread.open", rename_all = "camelCase")]
    ThreadOpen {
        agent_id: String,
        #[serde(default)]
        session_id: Option<String>,
    },
    #[serde(rename = "thread.get", rename_all = "camelCase")]
    ThreadGet { thread_id: String },
    #[serde(rename = "thread.send", rename_all = "camelCase")]
    ThreadSend { thread_id: String, text: String },
    #[serde(rename = "thread.cancel", rename_all = "camelCase")]
    ThreadCancel { thread_id: String },
    #[serde(rename = "thread.authorize", rename_all = "camelCase")]
    ThreadAuthorize {
        thread_id: String,
        tool_call_id: String,
        option_id: String,
    },
    #[serde(rename = "thread.close", rename_all = "camelCase")]
    ThreadClose { thread_id: String },
    #[serde(rename = "thread.delete", rename_all = "camelCase")]
    ThreadDelete { agent_id: String, session_id: String },
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum Outgoing {
    Ok { id: u64, ok: serde_json::Value },
    Error { id: u64, error: String },
    Event(Event),
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "event", rename_all = "kebab-case")]
pub enum Event {
    #[serde(rename = "terminal.output", rename_all = "camelCase")]
    TerminalOutput { terminal_id: u64, data: String },
    #[serde(rename = "terminal.exit", rename_all = "camelCase")]
    TerminalExit { terminal_id: u64, code: Option<u32> },
    #[serde(rename = "thread.updated")]
    ThreadUpdated { thread: ThreadSnapshot },
    #[serde(rename = "thread.error", rename_all = "camelCase")]
    ThreadError { thread_id: String, message: String },
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentInfo {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadList {
    pub agent_id: String,
    pub supports_history: bool,
    pub supports_delete: bool,
    pub threads: Vec<ThreadSummary>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadSummary {
    pub session_id: String,
    pub title: Option<String>,
    pub updated_at: Option<String>,
    pub work_dirs: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadSnapshot {
    pub thread_id: String,
    pub agent_id: String,
    pub title: Option<String>,
    pub status: ThreadStatus,
    pub had_error: bool,
    pub entries: Vec<ThreadEntry>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ThreadStatus {
    Idle,
    Generating,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ThreadEntry {
    User {
        markdown: String,
    },
    Assistant {
        markdown: String,
        thoughts: String,
    },
    #[serde(rename_all = "camelCase")]
    ToolCall {
        id: String,
        title: String,
        tool_kind: String,
        status: String,
        markdown: String,
        permission_options: Vec<PermissionOption>,
    },
    Plan {
        entries: Vec<PlanEntry>,
    },
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionOption {
    pub id: String,
    pub label: String,
    pub option_kind: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanEntry {
    pub content: String,
    pub priority: String,
    pub status: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceInfo {
    pub root: String,
    pub root_name: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorktreeEntry {
    pub path: String,
    pub name: String,
    pub kind: EntryKind,
    pub is_ignored: bool,
    /// On-disk size — the graph sizes file circles by it.
    pub size_bytes: u64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum EntryKind {
    Dir,
    File,
}

#[derive(Debug, Serialize)]
pub struct BufferContents {
    pub path: String,
    pub text: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GitStatus {
    /// `None` when detached or the root is not a git repository.
    pub branch: Option<String>,
    pub changed: Vec<GitChange>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GitChange {
    /// Repo-relative path (the current path for renames).
    pub path: String,
    /// The two-letter porcelain XY code, trimmed (`M`, `A`, `D`, `??`, …).
    pub status: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalCreated {
    pub terminal_id: u64,
}
