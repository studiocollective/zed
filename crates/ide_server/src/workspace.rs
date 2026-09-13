use crate::agent::AgentHub;
use crate::protocol::{BufferContents, EntryKind, WorkspaceInfo, WorktreeEntry};
use anyhow::{Context as _, Result, anyhow};
use gpui::{AppContext as _, AsyncApp, Entity};
use project::{Project, ProjectPath};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use util::rel_path::RelPath;
use worktree::Worktree;

/// The single-root project this server exposes. Cloned per connection.
#[derive(Clone)]
pub struct HeadlessWorkspace {
    project: Entity<Project>,
    worktree: Entity<Worktree>,
    agents: Entity<AgentHub>,
    root: PathBuf,
}

impl HeadlessWorkspace {
    pub async fn open(project: Entity<Project>, root: &Path, cx: &mut AsyncApp) -> Result<Self> {
        let root = root
            .canonicalize()
            .context("canonicalizing workspace root")?;
        let (worktree, _) = cx
            .update(|cx| {
                project.update(cx, |project, cx| {
                    project.find_or_create_worktree(&root, true, cx)
                })
            })
            .await?;
        let scan_complete = worktree.read_with(cx, |worktree, _| {
            worktree
                .as_local()
                .map(|local| local.scan_complete())
                .ok_or_else(|| anyhow!("worktree is not local"))
        })?;
        scan_complete.await;
        let agents = cx.new(|_| AgentHub::new(project.clone(), root.clone()));
        Ok(Self {
            project,
            worktree,
            agents,
            root,
        })
    }

    pub fn info(&self, cx: &AsyncApp) -> WorkspaceInfo {
        WorkspaceInfo {
            root: self.root.to_string_lossy().into_owned(),
            root_name: self
                .worktree
                .read_with(cx, |worktree, _| worktree.root_name_str().to_string()),
        }
    }

    pub async fn entries(&self, path: &str, cx: &mut AsyncApp) -> Result<Vec<WorktreeEntry>> {
        let parent: Arc<RelPath> = RelPath::unix(path)
            .with_context(|| format!("invalid worktree path {path:?}"))?
            .into();

        let needs_expansion = self.worktree.read_with(cx, |worktree, _| {
            let entry = worktree
                .entry_for_path(&parent)
                .with_context(|| format!("no entry at {path:?}"))?;
            anyhow::Ok(entry.is_dir().then_some(entry.id))
        })?;
        if let Some(entry_id) = needs_expansion {
            let expansion = self
                .worktree
                .update(cx, |worktree, cx| worktree.expand_entry(entry_id, cx));
            if let Some(expansion) = expansion {
                expansion.await?;
            }
        }

        let entries = self.worktree.read_with(cx, |worktree, _| {
            worktree
                .child_entries(&parent)
                .map(|entry| WorktreeEntry {
                    path: entry.path.as_unix_str().to_string(),
                    name: entry
                        .path
                        .file_name()
                        .unwrap_or_else(|| entry.path.as_unix_str())
                        .to_string(),
                    kind: if entry.is_dir() {
                        EntryKind::Dir
                    } else {
                        EntryKind::File
                    },
                    is_ignored: entry.is_ignored,
                    size_bytes: entry.size,
                })
                .collect()
        });
        Ok(entries)
    }

    pub async fn read_buffer(&self, path: &str, cx: &mut AsyncApp) -> Result<BufferContents> {
        let rel_path: Arc<RelPath> = RelPath::unix(path)
            .with_context(|| format!("invalid worktree path {path:?}"))?
            .into();
        let (worktree_id, is_file) = self.worktree.read_with(cx, |worktree, _| {
            (
                worktree.id(),
                worktree
                    .entry_for_path(&rel_path)
                    .is_some_and(|entry| entry.is_file()),
            )
        });
        anyhow::ensure!(is_file, "{path:?} is not a file in the worktree");
        let project_path = ProjectPath {
            worktree_id,
            path: rel_path,
        };
        let buffer = cx
            .update(|cx| {
                self.project
                    .update(cx, |project, cx| project.open_buffer(project_path, cx))
            })
            .await?;
        let text = buffer.read_with(cx, |buffer, _| buffer.text());
        Ok(BufferContents {
            path: path.to_string(),
            text,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn agents(&self) -> &Entity<AgentHub> {
        &self.agents
    }
}
