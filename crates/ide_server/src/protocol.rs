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
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum Outgoing {
    Ok { id: u64, ok: serde_json::Value },
    Error { id: u64, error: String },
    Event(Event),
}

#[derive(Debug, Serialize)]
#[serde(tag = "event", rename_all = "kebab-case")]
pub enum Event {
    #[serde(rename = "terminal.output", rename_all = "camelCase")]
    TerminalOutput { terminal_id: u64, data: String },
    #[serde(rename = "terminal.exit", rename_all = "camelCase")]
    TerminalExit { terminal_id: u64, code: Option<u32> },
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
pub struct TerminalCreated {
    pub terminal_id: u64,
}
