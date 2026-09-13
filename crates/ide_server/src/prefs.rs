use anyhow::{Context as _, Result};
use serde_json::{Map, Value};
use std::path::PathBuf;

/// Flat key/value persistence for the IDE shell (UI preferences,
/// per-worktree build commands, connections). One JSON map on disk;
/// writes are read-modify-write with an atomic rename. The richer
/// sync-engine-channel treatment is tracked separately — this is the
/// bespoke-protocol stopgap the shell needs today.
fn settings_path() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME not set")?;
    Ok(PathBuf::from(home).join(".config/songbird-ide/settings.json"))
}

pub fn all() -> Result<Map<String, Value>> {
    let path = settings_path()?;
    match std::fs::read_to_string(&path) {
        Ok(text) => Ok(serde_json::from_str(&text).unwrap_or_default()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Map::new()),
        Err(error) => Err(error).context("reading settings"),
    }
}

pub fn set(key: String, value: Value) -> Result<()> {
    let path = settings_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).context("creating settings dir")?;
    }
    let mut map = all()?;
    if value.is_null() {
        map.remove(&key);
    } else {
        map.insert(key, value);
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(&map)?).context("writing settings")?;
    std::fs::rename(&tmp, &path).context("committing settings")?;
    Ok(())
}
