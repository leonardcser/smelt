use protocol::{HistoryIdentity, HistoryItem};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// Most recent fork points kept per session.
const MAX_CHECKPOINTS: usize = 256;

#[derive(Serialize, Deserialize)]
struct SessionLink {
    native_id: String,
    /// Key of the history the whole native session matches, or `None` once it
    /// may hold more than smelt saw.
    #[serde(default)]
    history: Option<String>,
    cwd: PathBuf,
    config_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    plan_path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    checkpoints: Vec<Checkpoint>,
}

/// A history the native session matched up to and including one of its
/// assistant messages. A fork keeps message IDs, so checkpoints stay valid
/// in sessions forked after them.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(super) struct Checkpoint {
    history: String,
    message: String,
}

impl Checkpoint {
    pub(super) fn new(history: &HistoryIdentity, message: String) -> Self {
        Self {
            history: history.key(),
            message,
        }
    }
}

/// How a new process continues a linked native session.
pub(super) struct Resume {
    pub(super) native_id: String,
    /// The assistant message to fork the native session at, for a history
    /// that ends earlier than the native session.
    pub(super) at: Option<String>,
    pub(super) plan_path: Option<PathBuf>,
    pub(super) checkpoints: Vec<Checkpoint>,
}

/// The link lives with the session's other files, so it goes when the session does.
fn link_path(artifact_dir: &Path) -> PathBuf {
    artifact_dir.join("claude-code-session.json")
}

fn read(path: &Path) -> Option<SessionLink> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn write(artifact_dir: &Path, link: &SessionLink) -> Result<(), String> {
    let bytes = serde_json::to_vec(link).map_err(|e| e.to_string())?;
    smelt_paths::write_atomic(&link_path(artifact_dir), &bytes)
        .map_err(|e| format!("could not save Claude Code session link: {e}"))
}

/// Resumes the whole native session when it matches `history`, or forks it at
/// the checkpoint that does, as after a rewind.
pub(super) fn load(artifact_dir: &Path, cwd: &Path, history: &HistoryIdentity) -> Option<Resume> {
    resume(read(&link_path(artifact_dir))?, cwd, &history.key())
}

fn resume(mut link: SessionLink, cwd: &Path, key: &str) -> Option<Resume> {
    if link.cwd != cwd
        || link.config_dir != std::env::var("CLAUDE_CONFIG_DIR").ok()
        || !valid_native_id(&link.native_id)
    {
        return None;
    }
    if link.history.as_deref() == Some(key) {
        return Some(Resume {
            native_id: link.native_id,
            at: None,
            plan_path: link.plan_path,
            checkpoints: link.checkpoints,
        });
    }
    let index = link
        .checkpoints
        .iter()
        .rposition(|checkpoint| checkpoint.history == key)?;
    link.checkpoints.truncate(index + 1);
    Some(Resume {
        native_id: link.native_id,
        at: Some(link.checkpoints[index].message.clone()),
        plan_path: None,
        checkpoints: link.checkpoints,
    })
}

pub(super) fn save(
    artifact_dir: &Path,
    native_id: &str,
    cwd: &Path,
    history: &HistoryIdentity,
    plan_path: Option<&Path>,
    checkpoints: &[Checkpoint],
) -> Result<(), String> {
    if !valid_native_id(native_id) {
        return Err("Claude Code returned an invalid session ID".into());
    }
    write(
        artifact_dir,
        &SessionLink {
            native_id: native_id.into(),
            history: Some(history.key()),
            cwd: cwd.into(),
            config_dir: std::env::var("CLAUDE_CONFIG_DIR").ok(),
            plan_path: plan_path.map(Path::to_path_buf),
            checkpoints: checkpoints[checkpoints.len().saturating_sub(MAX_CHECKPOINTS)..].to_vec(),
        },
    )
}

/// A cancelled or failed native turn may have committed more than its UI
/// mirror. Never blindly resume it and resurrect unseen tool execution; only
/// its checkpoints, which predate that work, stay usable.
pub(super) fn invalidate(artifact_dir: &Path) {
    match read(&link_path(artifact_dir)) {
        Some(mut link) if !link.checkpoints.is_empty() => {
            link.history = None;
            if write(artifact_dir, &link).is_err() {
                let _ = std::fs::remove_file(link_path(artifact_dir));
            }
        }
        _ => {
            let _ = std::fs::remove_file(link_path(artifact_dir));
        }
    }
}

/// COMPAT(claude-code-link-v1): the link of earlier 0.8 releases, which names
/// a history by a digest of its JSON.
#[derive(Deserialize)]
struct LegacyLink {
    native_id: String,
    history_digest: Option<String>,
    cwd: PathBuf,
    config_dir: Option<String>,
    #[serde(default)]
    plan_path: Option<PathBuf>,
}

/// COMPAT(claude-code-link-v1)
fn legacy_link_path(artifact_dir: &Path) -> PathBuf {
    artifact_dir.join("claude-code.json")
}

/// COMPAT(claude-code-link-v1)
pub(super) fn has_legacy(artifact_dir: &Path) -> bool {
    legacy_link_path(artifact_dir).exists()
}

/// COMPAT(claude-code-link-v1): resumes the native session of a legacy link
/// whose history is `items`, and removes the link, which is read only once.
/// Its checkpoints name histories by digest, so they are dropped.
pub(super) fn take_legacy(
    artifact_dir: &Path,
    cwd: &Path,
    items: &[HistoryItem],
) -> Option<Resume> {
    let path = legacy_link_path(artifact_dir);
    let bytes = std::fs::read(&path).ok();
    let _ = std::fs::remove_file(&path);
    let legacy: LegacyLink = serde_json::from_slice(&bytes?).ok()?;
    let link = SessionLink {
        native_id: legacy.native_id,
        history: legacy.history_digest,
        cwd: legacy.cwd,
        config_dir: legacy.config_dir,
        plan_path: legacy.plan_path,
        checkpoints: Vec::new(),
    };
    let conversation: Vec<_> = items
        .iter()
        .filter(|item| !matches!(item, HistoryItem::Note(_) | HistoryItem::System { .. }))
        .collect();
    let digest: String = Sha256::digest(serde_json::to_vec(&conversation).ok()?)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    resume(link, cwd, &digest)
}

fn valid_native_id(id: &str) -> bool {
    id.len() == 36
        && id.bytes().enumerate().all(|(i, b)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                b == b'-'
            } else {
                b.is_ascii_hexdigit()
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    const NATIVE: &str = "12345678-1234-1234-1234-123456789abc";

    fn identity(items: &[HistoryItem]) -> HistoryIdentity {
        HistoryIdentity::of(items)
    }

    #[test]
    fn links_are_bound_to_history_and_workspace() {
        let root = tempfile::tempdir().unwrap();
        let history = identity(&[HistoryItem::user(protocol::Content::text("one"))]);
        save(
            root.path(),
            NATIVE,
            root.path(),
            &history,
            Some(Path::new("active.md")),
            &[],
        )
        .unwrap();
        let resume = load(root.path(), root.path(), &history).unwrap();
        assert_eq!(resume.plan_path.as_deref(), Some(Path::new("active.md")));
        assert_eq!(resume.at, None);
        assert!(load(root.path(), Path::new("/different"), &history).is_none());
        assert!(load(root.path(), root.path(), &identity(&[])).is_none());
        invalidate(root.path());
        assert!(load(root.path(), root.path(), &history).is_none());
    }

    #[test]
    fn earlier_histories_fork_at_their_checkpoint_even_after_cancellation() {
        let root = tempfile::tempdir().unwrap();
        let one = vec![HistoryItem::user(protocol::Content::text("one"))];
        let mut two = one.clone();
        two.push(HistoryItem::user(protocol::Content::text("two")));
        let checkpoints = [
            Checkpoint::new(&identity(&one), "first-answer".into()),
            Checkpoint::new(&identity(&two), "second-answer".into()),
        ];
        save(
            root.path(),
            NATIVE,
            root.path(),
            &identity(&two),
            None,
            &checkpoints,
        )
        .unwrap();
        invalidate(root.path());
        assert!(load(root.path(), root.path(), &identity(&two))
            .is_some_and(|resume| resume.at.as_deref() == Some("second-answer")));
        let resume = load(root.path(), root.path(), &identity(&one)).unwrap();
        assert_eq!(resume.native_id, NATIVE);
        assert_eq!(resume.at.as_deref(), Some("first-answer"));
        assert_eq!(resume.checkpoints, checkpoints[..1]);
        assert!(load(root.path(), root.path(), &identity(&[])).is_none());
    }

    #[test]
    fn legacy_links_resume_a_matching_history_once() {
        let root = tempfile::tempdir().unwrap();
        let items = vec![
            HistoryItem::user(protocol::Content::text("one")),
            HistoryItem::note(protocol::HistoryNote::context("cwd")),
        ];
        let write_legacy = |digest_of: &[HistoryItem]| {
            let conversation = &digest_of[..1];
            let digest: String = Sha256::digest(serde_json::to_vec(conversation).unwrap())
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect();
            let link = serde_json::json!({
                "native_id": NATIVE,
                "history_digest": digest,
                "cwd": root.path(),
                "config_dir": std::env::var("CLAUDE_CONFIG_DIR").ok(),
                "checkpoints": [{"history_digest": "old", "message": "answer"}],
            });
            std::fs::write(legacy_link_path(root.path()), link.to_string()).unwrap();
        };
        write_legacy(&items);
        assert!(has_legacy(root.path()));
        let resume = take_legacy(root.path(), root.path(), &items).unwrap();
        assert_eq!((resume.native_id.as_str(), resume.at), (NATIVE, None));
        assert!(resume.checkpoints.is_empty());
        assert!(!has_legacy(root.path()));

        write_legacy(&[HistoryItem::user(protocol::Content::text("other"))]);
        assert!(take_legacy(root.path(), root.path(), &items).is_none());
        assert!(!has_legacy(root.path()));
    }
}
