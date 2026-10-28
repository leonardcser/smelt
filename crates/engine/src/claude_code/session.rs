use protocol::HistoryItem;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// Most recent fork points kept per session.
const MAX_CHECKPOINTS: usize = 256;

#[derive(Serialize, Deserialize)]
struct SessionLink {
    native_id: String,
    /// The history the whole native session matches, or `None` once it may
    /// hold more than smelt saw.
    #[serde(default)]
    history_digest: Option<String>,
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
    history_digest: String,
    message: String,
}

impl Checkpoint {
    pub(super) fn new(history: &[HistoryItem], message: String) -> Self {
        Self {
            history_digest: history_digest(history),
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

fn digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub(super) fn history_digest(history: &[HistoryItem]) -> String {
    // Mode and environment notes do not alter the native conversation. The cwd
    // and config directory are checked separately before reusing a native session.
    let conversation: Vec<_> = history
        .iter()
        .filter(|item| !matches!(item, HistoryItem::Note(_) | HistoryItem::System { .. }))
        .collect();
    digest(&serde_json::to_vec(&conversation).expect("history is JSON serializable"))
}

fn link_path(root: &Path, id: &str) -> PathBuf {
    root.join("claude-code")
        .join(format!("{}.json", digest(id.as_bytes())))
}

fn read(root: &Path, id: &str) -> Option<SessionLink> {
    let bytes = std::fs::read(link_path(root, id)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn write(root: &Path, id: &str, link: &SessionLink) -> Result<(), String> {
    let bytes = serde_json::to_vec(link).map_err(|e| e.to_string())?;
    crate::paths::write_atomic(&link_path(root, id), &bytes)
        .map_err(|e| format!("could not save Claude Code session link: {e}"))
}

/// Resumes the whole native session when it matches `history`, or forks it at
/// the checkpoint that does, as after a rewind.
pub(super) fn load(root: &Path, id: &str, cwd: &Path, history: &[HistoryItem]) -> Option<Resume> {
    let mut link = read(root, id)?;
    if link.cwd != cwd
        || link.config_dir != std::env::var("CLAUDE_CONFIG_DIR").ok()
        || !valid_native_id(&link.native_id)
    {
        return None;
    }
    let digest = history_digest(history);
    if link.history_digest.as_ref() == Some(&digest) {
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
        .rposition(|checkpoint| checkpoint.history_digest == digest)?;
    link.checkpoints.truncate(index + 1);
    Some(Resume {
        native_id: link.native_id,
        at: Some(link.checkpoints[index].message.clone()),
        plan_path: None,
        checkpoints: link.checkpoints,
    })
}

pub(super) fn save(
    root: &Path,
    id: &str,
    native_id: &str,
    cwd: &Path,
    history: &[HistoryItem],
    plan_path: Option<&Path>,
    checkpoints: &[Checkpoint],
) -> Result<(), String> {
    if !valid_native_id(native_id) {
        return Err("Claude Code returned an invalid session ID".into());
    }
    write(
        root,
        id,
        &SessionLink {
            native_id: native_id.into(),
            history_digest: Some(history_digest(history)),
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
pub(super) fn invalidate(root: &Path, id: &str) {
    match read(root, id) {
        Some(mut link) if !link.checkpoints.is_empty() => {
            link.history_digest = None;
            if write(root, id, &link).is_err() {
                let _ = std::fs::remove_file(link_path(root, id));
            }
        }
        _ => {
            let _ = std::fs::remove_file(link_path(root, id));
        }
    }
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

    #[test]
    fn links_are_bound_to_history_and_workspace() {
        let root = tempfile::tempdir().unwrap();
        let history = vec![HistoryItem::user(protocol::Content::text("one"))];
        save(
            root.path(),
            "../../unsafe",
            NATIVE,
            root.path(),
            &history,
            Some(Path::new("active.md")),
            &[],
        )
        .unwrap();
        let resume = load(root.path(), "../../unsafe", root.path(), &history).unwrap();
        assert_eq!(resume.plan_path.as_deref(), Some(Path::new("active.md")));
        assert_eq!(resume.at, None);
        assert!(load(
            root.path(),
            "../../unsafe",
            Path::new("/different"),
            &history
        )
        .is_none());
        assert!(load(root.path(), "../../unsafe", root.path(), &[]).is_none());
        invalidate(root.path(), "../../unsafe");
        assert!(load(root.path(), "../../unsafe", root.path(), &history).is_none());
    }

    #[test]
    fn earlier_histories_fork_at_their_checkpoint_even_after_cancellation() {
        let root = tempfile::tempdir().unwrap();
        let one = vec![HistoryItem::user(protocol::Content::text("one"))];
        let mut two = one.clone();
        two.push(HistoryItem::user(protocol::Content::text("two")));
        let checkpoints = [
            Checkpoint::new(&one, "first-answer".into()),
            Checkpoint::new(&two, "second-answer".into()),
        ];
        save(
            root.path(),
            "s",
            NATIVE,
            root.path(),
            &two,
            None,
            &checkpoints,
        )
        .unwrap();
        invalidate(root.path(), "s");
        assert!(load(root.path(), "s", root.path(), &two)
            .is_some_and(|resume| resume.at.as_deref() == Some("second-answer")));
        let resume = load(root.path(), "s", root.path(), &one).unwrap();
        assert_eq!(resume.native_id, NATIVE);
        assert_eq!(resume.at.as_deref(), Some("first-answer"));
        assert_eq!(resume.checkpoints, checkpoints[..1]);
        assert!(load(root.path(), "s", root.path(), &[]).is_none());
    }
}
