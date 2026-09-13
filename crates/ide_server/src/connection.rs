use crate::agent::AgentHub;
use crate::protocol::{Command, Outgoing, Request, TerminalCreated};
use crate::terminal::TerminalSession;
use crate::workspace::HeadlessWorkspace;
use anyhow::{Context as _, Result};
use async_channel::{Receiver, Sender};
use async_tungstenite::tungstenite::Message;
use futures::{SinkExt, StreamExt as _};
use gpui::{AppContext as _, AsyncApp, Entity, Task};
use util::ResultExt as _;
use serde::Serialize;
use smol::net::TcpStream;
use std::collections::HashMap;

/// One WebSocket client: dispatches requests against the shared workspace and
/// owns the terminals it created.
pub struct Connection {
    workspace: HeadlessWorkspace,
    outgoing: Sender<Outgoing>,
    terminals: HashMap<u64, TerminalSession>,
    next_terminal_id: u64,
}

impl Connection {
    pub async fn serve(
        stream: TcpStream,
        workspace: HeadlessWorkspace,
        cx: &mut AsyncApp,
    ) -> Result<()> {
        let websocket = async_tungstenite::accept_async(stream)
            .await
            .context("websocket handshake")?;
        let (mut sink, mut source) = websocket.split();
        let (outgoing, incoming): (Sender<Outgoing>, Receiver<Outgoing>) =
            async_channel::unbounded();

        let writer = cx.background_spawn(async move {
            while let Ok(message) = incoming.recv().await {
                let text = serde_json::to_string(&message)?;
                sink.send(Message::Text(text.into())).await?;
            }
            SinkExt::close(&mut sink).await?;
            anyhow::Ok(())
        });

        workspace
            .agents()
            .update(cx, |agents, _| agents.attach(outgoing.clone()));
        let mut connection = Self {
            workspace,
            outgoing,
            terminals: HashMap::new(),
            next_terminal_id: 1,
        };

        while let Some(message) = source.next().await {
            match message? {
                Message::Text(text) => connection.handle_text(&text, cx).await?,
                Message::Close(_) => break,
                Message::Ping(_) | Message::Pong(_) | Message::Binary(_) | Message::Frame(_) => {}
            }
        }

        drop(connection);
        writer.await
    }

    async fn handle_text(&mut self, text: &str, cx: &mut AsyncApp) -> Result<()> {
        let request: Request = match serde_json::from_str(text) {
            Ok(request) => request,
            Err(error) => {
                log::warn!("malformed request: {error}");
                return Ok(());
            }
        };
        let id = request.id;
        match self.dispatch_slow(request.command, cx) {
            Ok(slow) => {
                let outgoing = self.outgoing.clone();
                cx.spawn(async move |_| {
                    outgoing.send(respond(id, slow.await)).await.log_err();
                })
                .detach();
                Ok(())
            }
            Err(command) => {
                let response = respond(id, self.dispatch(command, cx).await);
                self.outgoing
                    .send(response)
                    .await
                    .context("connection closed")
            }
        }
    }

    /// Agent connections can take seconds to start; run them off the read
    /// loop so terminal input keeps flowing meanwhile. Returns the command
    /// back when it is not one of those.
    fn dispatch_slow(
        &self,
        command: Command,
        cx: &mut AsyncApp,
    ) -> Result<Task<Result<serde_json::Value>>, Command> {
        let agents = self.agents().clone();
        match command {
            Command::ThreadList { agent_id } => Ok(cx.spawn(async move |cx| {
                to_value(AgentHub::list_threads(&agents, agent_id, cx).await?)
            })),
            Command::ThreadOpen {
                agent_id,
                session_id,
            } => Ok(cx.spawn(async move |cx| {
                to_value(AgentHub::open_thread(&agents, agent_id, session_id, cx).await?)
            })),
            Command::ThreadDelete {
                agent_id,
                session_id,
            } => Ok(cx.spawn(async move |cx| {
                AgentHub::delete_thread(&agents, agent_id, session_id, cx).await?;
                Ok(serde_json::Value::Null)
            })),
            other => Err(other),
        }
    }

    async fn dispatch(&mut self, command: Command, cx: &mut AsyncApp) -> Result<serde_json::Value> {
        match command {
            Command::WorkspaceInfo => to_value(self.workspace.info(cx)),
            Command::WorktreeEntries { path } => to_value(self.workspace.entries(&path, cx).await?),
            Command::BufferRead { path } => to_value(self.workspace.read_buffer(&path, cx).await?),
            Command::TerminalCreate { cols, rows } => {
                let terminal_id = self.next_terminal_id;
                self.next_terminal_id += 1;
                let session = TerminalSession::spawn(
                    terminal_id,
                    self.workspace.root(),
                    cols,
                    rows,
                    self.outgoing.clone(),
                )?;
                self.terminals.insert(terminal_id, session);
                to_value(TerminalCreated { terminal_id })
            }
            Command::TerminalInput { terminal_id, data } => {
                self.terminal(terminal_id)?.input(&data)?;
                Ok(serde_json::Value::Null)
            }
            Command::TerminalResize {
                terminal_id,
                cols,
                rows,
            } => {
                self.terminal(terminal_id)?.resize(cols, rows)?;
                Ok(serde_json::Value::Null)
            }
            Command::TerminalClose { terminal_id } => {
                self.terminals
                    .remove(&terminal_id)
                    .with_context(|| format!("unknown terminal {terminal_id}"))?;
                Ok(serde_json::Value::Null)
            }
            Command::AgentList => {
                to_value(self.agents().read_with(cx, |agents, cx| agents.agents(cx)))
            }
            Command::ThreadGet { thread_id } => to_value(
                self.agents()
                    .read_with(cx, |agents, cx| agents.thread_snapshot(&thread_id, cx))?,
            ),
            Command::ThreadSend { thread_id, text } => {
                self.agents()
                    .update(cx, |agents, cx| agents.send(&thread_id, text, cx))?;
                Ok(serde_json::Value::Null)
            }
            Command::ThreadCancel { thread_id } => {
                self.agents()
                    .update(cx, |agents, cx| agents.cancel(&thread_id, cx))?;
                Ok(serde_json::Value::Null)
            }
            Command::ThreadAuthorize {
                thread_id,
                tool_call_id,
                option_id,
            } => {
                self.agents().update(cx, |agents, cx| {
                    agents.authorize(&thread_id, &tool_call_id, &option_id, cx)
                })?;
                Ok(serde_json::Value::Null)
            }
            Command::ThreadList { .. } | Command::ThreadOpen { .. } | Command::ThreadDelete { .. } => {
                anyhow::bail!("command must be dispatched through dispatch_slow")
            }
            Command::ThreadClose { thread_id } => {
                self.agents()
                    .update(cx, |agents, _| agents.close(&thread_id))?;
                Ok(serde_json::Value::Null)
            }
        }
    }

    fn agents(&self) -> &Entity<AgentHub> {
        self.workspace.agents()
    }

    fn terminal(&mut self, terminal_id: u64) -> Result<&mut TerminalSession> {
        self.terminals
            .get_mut(&terminal_id)
            .with_context(|| format!("unknown terminal {terminal_id}"))
    }
}

fn respond(id: u64, result: Result<serde_json::Value>) -> Outgoing {
    match result {
        Ok(ok) => Outgoing::Ok { id, ok },
        Err(error) => Outgoing::Error {
            id,
            error: format!("{error:#}"),
        },
    }
}

fn to_value<T: Serialize>(value: T) -> Result<serde_json::Value> {
    serde_json::to_value(value).context("serializing response")
}
