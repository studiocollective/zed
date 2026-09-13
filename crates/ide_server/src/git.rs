use crate::protocol::{GitChange, GitStatus};
use anyhow::{Context as _, Result};
use smol::process::Command;
use std::path::Path;

/// The worktree's uncommitted change set plus its current branch, via
/// `git status --porcelain=v1 -z`. Shelling out keeps the dependency
/// surface at zero; ide_server always runs on the developer's machine,
/// where git exists (a non-repo root just reports no branch and no
/// changes rather than erroring).
pub async fn status(root: &Path) -> Result<GitStatus> {
    if run_git(root, &["rev-parse", "--git-dir"]).await.is_err() {
        return Ok(GitStatus {
            branch: None,
            changed: Vec::new(),
        });
    }

    let branch = run_git(root, &["rev-parse", "--abbrev-ref", "HEAD"])
        .await
        .ok()
        .map(|raw| raw.trim().to_string())
        // Detached HEAD prints the literal "HEAD" — no branch to show.
        .filter(|name| !name.is_empty() && name != "HEAD");

    let raw = run_git(
        root,
        &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
    )
    .await?;
    let mut changed = Vec::new();
    let mut fields = raw.split('\0');
    while let Some(field) = fields.next() {
        // Each entry is `XY path`; the two status letters plus a space.
        if field.len() < 4 {
            continue;
        }
        let (code, path) = field.split_at(3);
        let status = code[..2].trim().to_string();
        // Renames/copies carry the origin path in the next NUL field —
        // keep the current path, drop the origin.
        if status.starts_with('R') || status.starts_with('C') {
            let _ = fields.next();
        }
        changed.push(GitChange {
            path: path.to_string(),
            status,
        });
    }
    Ok(GitStatus { branch, changed })
}

async fn run_git(root: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .await
        .context("running git")?;
    anyhow::ensure!(
        output.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    String::from_utf8(output.stdout).context("git output was not utf-8")
}
