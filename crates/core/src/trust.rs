//! Project-local config trust. `.smelt/` is only loaded after the user marks
//! it trusted. Trust is keyed by canonical project-root path and SHA-256 of
//! file contents; editing any file invalidates the hash. Persisted to
//! `<XDG_STATE_HOME>/smelt/trust.json`.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

const TRUST_FILE: &str = "trust.json";

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TrustEntry {
    pub hash: String,
}

#[derive(Debug, Clone)]
pub enum TrustState {
    /// No `.smelt/` content under the project root.
    NoContent,
    /// Content present and the hash matches the stored entry.
    Trusted { hash: String },
    /// Content present but no entry, or hash differs.
    Untrusted { hash: String },
}

pub fn trust_path() -> PathBuf {
    engine::state_dir().join(TRUST_FILE)
}

/// SHA-256 of `.smelt/` files walked in sorted order (deterministic).
/// Returns `None` when no relevant files exist.
pub fn project_content_hash(cwd: &Path) -> Option<String> {
    let dir = cwd.join(".smelt");
    if !dir.exists() {
        return None;
    }
    let mut paths = Vec::new();
    collect_files(&dir, &dir, &mut paths);
    if paths.is_empty() {
        return None;
    }
    paths.sort();
    let mut hasher = Sha256::new();
    for rel in &paths {
        let abs = dir.join(rel);
        let Ok(bytes) = fs::read(&abs) else { continue };
        hasher.update(rel.to_string_lossy().as_bytes());
        hasher.update([0u8]);
        hasher.update(&bytes);
        hasher.update([0u8]);
    }
    Some(format!("{:x}", hasher.finalize()))
}

fn collect_files(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(rel) = path.strip_prefix(root) else {
            continue;
        };
        let rel = rel.to_path_buf();
        let file_type = match entry.file_type() {
            Ok(t) => t,
            Err(_) => continue,
        };
        if file_type.is_dir() {
            // Recurse only into the directories we care about.
            let name = rel.file_name().and_then(|s| s.to_str()).unwrap_or("");
            if matches!(name, "plugins" | "commands" | "runtime") {
                collect_files(root, &path, out);
            }
        } else if file_type.is_file() && relevant_file(&rel) {
            out.push(rel);
        }
    }
}

fn relevant_file(rel: &Path) -> bool {
    let parent = rel
        .parent()
        .and_then(|p| p.to_str())
        .map(|s| s.replace(std::path::MAIN_SEPARATOR, "/"))
        .unwrap_or_default();
    let name = rel.file_name().and_then(|s| s.to_str()).unwrap_or("");
    let ext = rel.extension().and_then(|s| s.to_str()).unwrap_or("");
    if parent.is_empty() && name == "init.lua" {
        return true;
    }
    if parent == "plugins" && ext == "lua" {
        return true;
    }
    if parent == "commands" && ext == "md" {
        return true;
    }
    if parent.starts_with("runtime") && ext == "lua" {
        return true;
    }
    false
}

pub fn project_trust_state(cwd: &Path) -> TrustState {
    let Some(hash) = project_content_hash(cwd) else {
        return TrustState::NoContent;
    };
    let store = load_store();
    let key = canonical_key(cwd);
    match store.get(&key) {
        Some(entry) if entry.hash == hash => TrustState::Trusted { hash },
        _ => TrustState::Untrusted { hash },
    }
}

/// Returns the recorded hash, or `Err` if no relevant content exists.
pub fn mark_trusted(cwd: &Path) -> Result<String, String> {
    let hash = project_content_hash(cwd)
        .ok_or_else(|| "no project content under .smelt/ to trust".to_string())?;
    let mut store = load_store();
    store.insert(canonical_key(cwd), TrustEntry { hash: hash.clone() });
    save_store(&store)?;
    Ok(hash)
}

fn canonical_key(cwd: &Path) -> String {
    fs::canonicalize(cwd)
        .unwrap_or_else(|_| cwd.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

fn load_store() -> BTreeMap<String, TrustEntry> {
    let path = trust_path();
    let Ok(bytes) = fs::read(&path) else {
        return BTreeMap::new();
    };
    serde_json::from_slice(&bytes).unwrap_or_default()
}

fn save_store(store: &BTreeMap<String, TrustEntry>) -> Result<(), String> {
    let path = trust_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("create trust dir: {e}"))?;
    }
    let bytes = serde_json::to_vec_pretty(store).map_err(|e| format!("serialize trust: {e}"))?;
    fs::write(&path, bytes).map_err(|e| format!("write trust file: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{isolate_xdg_state, xdg_state_guard};

    #[test]
    fn no_smelt_dir_yields_no_content() {
        let _g = xdg_state_guard();
        let tmp = tempfile::tempdir().unwrap();
        assert!(matches!(
            project_trust_state(tmp.path()),
            TrustState::NoContent
        ));
    }

    #[test]
    fn untrusted_when_no_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let smelt = tmp.path().join(".smelt");
        fs::create_dir_all(&smelt).unwrap();
        fs::write(smelt.join("init.lua"), "-- noop\n").unwrap();

        let state = tempfile::tempdir().unwrap();
        let _g = isolate_xdg_state(state.path());
        assert!(matches!(
            project_trust_state(tmp.path()),
            TrustState::Untrusted { .. }
        ));
    }

    #[test]
    fn mark_then_trusted_until_edit() {
        let tmp = tempfile::tempdir().unwrap();
        let smelt = tmp.path().join(".smelt");
        fs::create_dir_all(smelt.join("plugins")).unwrap();
        fs::write(smelt.join("init.lua"), "-- v1\n").unwrap();
        fs::write(smelt.join("plugins").join("a.lua"), "-- a\n").unwrap();

        let state = tempfile::tempdir().unwrap();
        let _g = isolate_xdg_state(state.path());

        let hash = mark_trusted(tmp.path()).unwrap();
        match project_trust_state(tmp.path()) {
            TrustState::Trusted { hash: h } => assert_eq!(h, hash),
            other => panic!("expected Trusted, got {other:?}"),
        }

        // Edit a file → mismatch.
        fs::write(smelt.join("init.lua"), "-- v2\n").unwrap();
        assert!(matches!(
            project_trust_state(tmp.path()),
            TrustState::Untrusted { .. }
        ));
    }

    #[test]
    fn relevant_file_rules() {
        assert!(relevant_file(Path::new("init.lua")));
        assert!(relevant_file(Path::new("plugins/foo.lua")));
        assert!(relevant_file(Path::new("commands/bar.md")));
        assert!(relevant_file(Path::new("runtime/widgets/x.lua")));
        assert!(!relevant_file(Path::new("nested/init.lua")));
        assert!(!relevant_file(Path::new("plugins/nested/foo.lua")));
        assert!(!relevant_file(Path::new("plugins/foo.txt")));
    }
}
