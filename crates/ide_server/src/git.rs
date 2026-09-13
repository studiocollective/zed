use crate::protocol::{DirStat, GitChange, GitSizes, GitStatus};
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

/// Per-directory mass from `git ls-tree -r -l HEAD`: for every tracked
/// blob, its size feeds a log-scaled mass (matching the UI's file
/// curve) summed into every ancestor directory. This is what lets a
/// collapsed `rust/` weigh like the half-million lines it holds
/// instead of its two visible entries.
pub async fn sizes(root: &Path) -> Result<GitSizes> {
    if run_git(root, &["rev-parse", "--git-dir"]).await.is_err() {
        return Ok(GitSizes { dirs: Vec::new() });
    }
    let raw = run_git(root, &["ls-tree", "-r", "-l", "HEAD"]).await?;
    use std::collections::HashMap;
    let mut mass: HashMap<String, (f64, u64)> = HashMap::new();
    for line in raw.lines() {
        // `<mode> blob <oid> <size>	<path>`
        let Some((meta, path)) = line.split_once('\t') else { continue };
        let mut fields = meta.split_whitespace();
        let (Some(_mode), Some(kind), Some(_oid), Some(size)) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        if kind != "blob" {
            continue;
        }
        let bytes: u64 = size.parse().unwrap_or(0);
        let file_mass = 1.0 + (1.0 + bytes as f64 / 256.0).log2();
        let mut dir = path;
        while let Some(slash) = dir.rfind('/') {
            dir = &dir[..slash];
            let entry = mass.entry(dir.to_string()).or_insert((0.0, 0));
            entry.0 += file_mass;
            entry.1 += bytes;
        }
        let entry = mass.entry(String::new()).or_insert((0.0, 0));
        entry.0 += file_mass;
        entry.1 += bytes;
    }
    let mut dirs: Vec<DirStat> = mass
        .into_iter()
        .map(|(path, (log_mass, bytes))| DirStat {
            path,
            log_mass: (log_mass * 10.0).round() / 10.0,
            bytes,
        })
        .collect();
    dirs.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(GitSizes { dirs })
}
