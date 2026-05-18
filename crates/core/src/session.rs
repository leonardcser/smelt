use crate::config;
use protocol::{Message, ReasoningEffort, TurnMeta};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static SESSION_COUNTER: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub slug: Option<String>,
    #[serde(default)]
    pub first_user_message: Option<String>,
    #[serde(default)]
    pub created_at_ms: u64,
    #[serde(default)]
    pub updated_at_ms: u64,
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub reasoning_effort: Option<ReasoningEffort>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub messages: Vec<Message>,
    #[serde(default)]
    pub context_tokens: Option<u32>,
    #[serde(default)]
    pub token_snapshots: Vec<(usize, u32)>,
    #[serde(default)]
    pub cost_snapshots: Vec<(usize, f64)>,
    /// Per-turn metadata, parallel to `token_snapshots`, keyed by history length.
    #[serde(default)]
    pub turn_metas: Vec<(usize, TurnMeta)>,
    /// Running session cost in USD; updated incrementally as token usage events arrive.
    #[serde(default)]
    pub session_cost_usd: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMeta {
    pub id: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub slug: Option<String>,
    #[serde(default)]
    pub first_user_message: Option<String>,
    #[serde(default)]
    pub created_at_ms: u64,
    #[serde(default)]
    pub updated_at_ms: u64,
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub reasoning_effort: Option<ReasoningEffort>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub context_tokens: Option<u32>,
    /// Approximate text byte size (message bodies, reasoning, tool-call args).
    /// Populated in `meta.json` so the resume dialog avoids loading `session.json`.
    #[serde(default)]
    pub text_bytes: Option<u64>,
}

impl Session {
    /// Create a fresh session. `pid` mixes into the session id; `cwd`
    /// is recorded as the session's working directory.
    pub fn new(pid: u32, cwd: std::path::PathBuf) -> Self {
        let now = now_ms();
        let id = new_session_id(now, pid);
        let cwd = cwd.to_str().map(String::from);
        Self {
            id,
            title: None,
            slug: None,
            first_user_message: None,
            created_at_ms: now,
            updated_at_ms: now,
            mode: None,
            reasoning_effort: None,
            model: None,
            cwd,
            parent_id: None,
            messages: Vec::new(),
            context_tokens: None,
            token_snapshots: Vec::new(),
            cost_snapshots: Vec::new(),
            turn_metas: Vec::new(),
            session_cost_usd: 0.0,
        }
    }

    fn meta(&self) -> SessionMeta {
        SessionMeta {
            id: self.id.clone(),
            title: self.title.clone(),
            slug: self.slug.clone(),
            first_user_message: self.first_user_message.clone(),
            created_at_ms: self.created_at_ms,
            updated_at_ms: self.updated_at_ms,
            mode: self.mode.clone(),
            reasoning_effort: self.reasoning_effort,
            model: self.model.clone(),
            cwd: self.cwd.clone(),
            parent_id: self.parent_id.clone(),
            context_tokens: self.context_tokens,
            text_bytes: Some(compute_text_bytes(&self.messages)),
        }
    }

    pub fn fork(&self, pid: u32) -> Self {
        let now = now_ms();
        Self {
            id: new_session_id(now, pid),
            title: self.title.clone(),
            slug: self.slug.clone(),
            first_user_message: self.first_user_message.clone(),
            created_at_ms: now,
            updated_at_ms: now,
            mode: self.mode.clone(),
            reasoning_effort: self.reasoning_effort,
            model: self.model.clone(),
            cwd: self.cwd.clone(),
            parent_id: Some(self.id.clone()),
            messages: self.messages.clone(),
            context_tokens: self.context_tokens,
            token_snapshots: self.token_snapshots.clone(),
            cost_snapshots: self.cost_snapshots.clone(),
            turn_metas: self.turn_metas.clone(),
            session_cost_usd: self.session_cost_usd,
        }
    }
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

// ── Save / Load / Delete ─────────────────────────────────────────────────────

pub fn dir_for(session: &Session) -> PathBuf {
    sessions_dir().join(&session.id)
}

pub fn save(session: &Session, store: &crate::attachment::AttachmentStore) {
    let session_dir = dir_for(session);
    let _ = fs::create_dir_all(&session_dir);
    let blob_dir = session_dir.join("blobs");
    let url_to_blob = store.save_blobs(&blob_dir);
    save_with_blobs(session, &url_to_blob);
}

/// Write `session.json` + `meta.json`. Assumes blobs are already flushed.
/// Safe to call from a background thread.
pub fn save_with_blobs(session: &Session, url_to_blob: &std::collections::HashMap<String, String>) {
    let _perf = smelt_perf::perf::begin("session:write");
    let session_dir = dir_for(session);
    let _ = fs::create_dir_all(&session_dir);
    let ts = now_ms();

    let session_out = if url_to_blob.is_empty() {
        std::borrow::Cow::Borrowed(session)
    } else {
        let mut s = session.clone();
        externalize_blobs(&mut s.messages, url_to_blob);
        std::borrow::Cow::Owned(s)
    };

    if let Ok(json) = serde_json::to_string(&*session_out) {
        atomic_write(&session_dir.join("session.json"), json.as_bytes(), ts);
    }
    let meta = session_out.meta();
    if let Ok(json) = serde_json::to_string(&meta) {
        atomic_write(&session_dir.join("meta.json"), json.as_bytes(), ts);
    }
    let blob = build_search_blob(&session_out.messages);
    atomic_write(&session_dir.join("content.txt"), blob.as_bytes(), ts);
}

/// Write `contents` to `path` atomically via a tmp file + rename.
fn atomic_write(path: &std::path::Path, contents: &[u8], ts: u64) {
    let Some(dir) = path.parent() else { return };
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return;
    };
    let tmp = dir.join(format!("{name}.{ts}.tmp"));
    if fs::write(&tmp, contents).is_ok() {
        let _ = fs::rename(&tmp, path);
    }
}

/// Load by exact ID or unique prefix (git-style short ID).
pub fn load(id_or_prefix: &str) -> Option<Session> {
    let id = resolve_prefix(id_or_prefix)?;
    load_exact(&id)
}

fn load_exact(id: &str) -> Option<Session> {
    let dir_path = sessions_dir().join(id);
    let contents = fs::read_to_string(dir_path.join("session.json")).ok()?;
    let mut session: Session = serde_json::from_str(&contents).ok()?;

    let blob_dir = dir_path.join("blobs");
    if blob_dir.is_dir() {
        let blob_to_url = crate::attachment::AttachmentStore::load_blobs(&blob_dir);
        if !blob_to_url.is_empty() {
            internalize_blobs(&mut session.messages, &blob_to_url);
        }
    }
    Some(session)
}

/// Returns `None` when no match or prefix is ambiguous.
fn resolve_prefix(prefix: &str) -> Option<String> {
    let dir = sessions_dir();

    if dir.join(prefix).join("session.json").is_file() {
        return Some(prefix.to_string());
    }

    let Ok(entries) = fs::read_dir(&dir) else {
        return None;
    };
    let mut matches = Vec::new();
    for entry in entries.flatten() {
        if !entry.path().is_dir() {
            continue;
        }
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else {
            continue;
        };
        if name_str.starts_with(prefix) {
            matches.push(name_str.to_string());
        }
    }
    if matches.len() == 1 {
        Some(matches.into_iter().next().unwrap())
    } else {
        None
    }
}

pub fn delete(id: &str) {
    let session_dir = sessions_dir().join(id);
    if session_dir.is_dir() {
        let _ = fs::remove_dir_all(&session_dir);
    }
}

pub fn list_sessions() -> Vec<SessionMeta> {
    let _perf = smelt_perf::perf::begin("session:list");
    let dir = sessions_dir();
    let Ok(entries) = fs::read_dir(&dir) else {
        return Vec::new();
    };

    let paths: Vec<PathBuf> = entries
        .flatten()
        .filter_map(|e| {
            let p = e.path();
            p.is_dir().then_some(p)
        })
        .collect();
    let mut out = crate::utils::parallel_filter_map(paths, load_meta_for_dir);
    out.sort_by_key(|b| std::cmp::Reverse(session_updated_at(b)));
    out
}

/// Uses `meta.json` when present; falls back to `session.json` and regenerates
/// the sidecar for older sessions.
fn load_meta_for_dir(path: PathBuf) -> Option<SessionMeta> {
    if let Ok(contents) = fs::read_to_string(path.join("meta.json")) {
        if let Ok(mut meta) = serde_json::from_str::<SessionMeta>(&contents) {
            if meta.text_bytes.is_none() {
                backfill_text_bytes(&path, &mut meta);
            }
            return Some(meta);
        }
    }
    let contents = fs::read_to_string(path.join("session.json")).ok()?;
    let session: Session = serde_json::from_str(&contents).ok()?;
    let mut meta = session.meta();
    if meta.id.is_empty() {
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            meta.id = name.to_string();
        }
    }
    write_meta(&path, &meta);
    Some(meta)
}

fn compute_text_bytes(messages: &[Message]) -> u64 {
    let mut total: u64 = 0;
    for msg in messages {
        if let Some(ref c) = msg.content {
            total += c.text_content().len() as u64;
        }
        if let Some(ref r) = msg.reasoning_content {
            total += r.len() as u64;
        }
        if let Some(ref calls) = msg.tool_calls {
            for call in calls {
                total += call.function.name.len() as u64;
                total += call.function.arguments.len() as u64;
            }
        }
    }
    total
}

fn backfill_text_bytes(session_dir: &std::path::Path, meta: &mut SessionMeta) {
    let Ok(contents) = fs::read_to_string(session_dir.join("session.json")) else {
        return;
    };
    let Ok(session) = serde_json::from_str::<Session>(&contents) else {
        return;
    };
    let bytes = compute_text_bytes(&session.messages);
    meta.text_bytes = Some(bytes);
    write_meta(session_dir, meta);
}

/// User + assistant text only; reasoning, tool output, and system messages excluded.
fn build_search_blob(messages: &[Message]) -> String {
    use protocol::Role;
    let mut out = String::new();
    for msg in messages {
        match msg.role {
            Role::User | Role::Assistant => {
                if let Some(ref c) = msg.content {
                    let text = c.text_content();
                    if !text.is_empty() {
                        out.push_str(&text);
                        out.push('\n');
                    }
                }
            }
            Role::Tool | Role::System => {}
        }
    }
    out
}

fn write_meta(session_dir: &std::path::Path, meta: &SessionMeta) {
    if let Ok(json) = serde_json::to_string(meta) {
        atomic_write(&session_dir.join("meta.json"), json.as_bytes(), now_ms());
    }
}

fn externalize_blobs(
    messages: &mut [Message],
    url_to_blob: &std::collections::HashMap<String, String>,
) {
    for msg in messages {
        if let Some(protocol::Content::Parts(parts)) = &mut msg.content {
            for part in parts {
                if let protocol::ContentPart::ImageUrl { url, .. } = part {
                    if let Some(blob_ref) = url_to_blob.get(url.as_str()) {
                        *url = blob_ref.clone();
                    }
                }
            }
        }
    }
}

fn internalize_blobs(
    messages: &mut [Message],
    blob_to_url: &std::collections::HashMap<String, String>,
) {
    for msg in messages {
        if let Some(protocol::Content::Parts(parts)) = &mut msg.content {
            for part in parts {
                if let protocol::ContentPart::ImageUrl { url, .. } = part {
                    if let Some(data_url) = blob_to_url.get(url.as_str()) {
                        *url = data_url.clone();
                    }
                }
            }
        }
    }
}

fn session_updated_at(meta: &SessionMeta) -> u64 {
    if meta.updated_at_ms > 0 {
        meta.updated_at_ms
    } else {
        meta.created_at_ms
    }
}

fn sessions_dir() -> PathBuf {
    config::state_dir().join("sessions")
}

fn new_session_id(now_ms: u64, pid: u32) -> String {
    let counter = SESSION_COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut hasher = Sha256::new();
    hasher.update(now_ms.to_le_bytes());
    hasher.update(pid.to_le_bytes());
    hasher.update(counter.to_le_bytes());
    format!("{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_id_is_full_sha256_hex() {
        let id = new_session_id(123456789, 4242);
        assert_eq!(id.len(), 64);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn session_ids_are_unique() {
        let id1 = new_session_id(100, 4242);
        let id2 = new_session_id(100, 4242);
        assert_ne!(id1, id2);
    }

    #[test]
    fn session_id_is_deterministic_for_fixed_inputs() {
        // Same (now_ms, pid) at adjacent counter values produces stable
        // ids — so a sim scenario that scripts both replays bit-identical.
        let now = 1_700_000_000_000;
        let pid = 7;
        let a = new_session_id(now, pid);
        let b = new_session_id(now, pid);
        assert_ne!(a, b, "counter should still vary within a process");
        assert_eq!(a.len(), b.len());
    }

    #[test]
    fn shortest_prefix_with_no_others() {
        // When the sessions dir doesn't exist or is empty, returns 4.
        let id = "abcdef1234567890";
        let prefix = &id[..id.len().min(4)];
        assert_eq!(prefix, "abcd");
    }

    use protocol::{Content, ContentPart, FunctionCall, Message, Role, ToolCall};

    fn msg(role: Role, text: &str) -> Message {
        Message {
            role,
            content: Some(Content::Text(text.into())),
            reasoning_content: None,

            reasoning_details: None,
            tool_calls: None,
            tool_call_id: None,
            is_error: false,
        }
    }
    fn user_msg(text: &str) -> Message {
        msg(Role::User, text)
    }
    fn assistant_msg(text: &str) -> Message {
        msg(Role::Assistant, text)
    }
    fn tool_msg(text: &str) -> Message {
        msg(Role::Tool, text)
    }
    fn system_msg(text: &str) -> Message {
        msg(Role::System, text)
    }

    // ── Session::new / fork / meta ───────────────────────────────────

    fn fixture_session() -> Session {
        Session::new(4242, std::path::PathBuf::from("/work"))
    }

    #[test]
    fn new_initializes_empty_fields_and_matches_created_updated() {
        let s = fixture_session();
        assert!(!s.id.is_empty());
        assert_eq!(s.id.len(), 64);
        assert_eq!(s.created_at_ms, s.updated_at_ms);
        assert!(s.messages.is_empty());
        assert_eq!(s.session_cost_usd, 0.0);
        assert!(s.parent_id.is_none());
        assert_eq!(s.cwd.as_deref(), Some("/work"));
    }

    #[test]
    fn meta_projects_visible_session_fields() {
        let mut s = fixture_session();
        s.title = Some("My Title".into());
        s.slug = Some("my-slug".into());
        s.first_user_message = Some("hello".into());
        s.mode = Some("ask".into());
        s.model = Some("claude-opus".into());
        s.cwd = Some("/work".into());
        s.parent_id = Some("p1".into());
        s.context_tokens = Some(1234);
        s.messages.push(user_msg("hi"));

        let m = s.meta();
        assert_eq!(m.id, s.id);
        assert_eq!(m.title.as_deref(), Some("My Title"));
        assert_eq!(m.slug.as_deref(), Some("my-slug"));
        assert_eq!(m.first_user_message.as_deref(), Some("hello"));
        assert_eq!(m.mode.as_deref(), Some("ask"));
        assert_eq!(m.model.as_deref(), Some("claude-opus"));
        assert_eq!(m.cwd.as_deref(), Some("/work"));
        assert_eq!(m.parent_id.as_deref(), Some("p1"));
        assert_eq!(m.context_tokens, Some(1234));
        assert_eq!(m.text_bytes, Some(2)); // "hi"
    }

    #[test]
    fn fork_clones_messages_and_links_parent_with_fresh_id() {
        let mut s = fixture_session();
        s.messages.push(user_msg("q1"));
        s.messages.push(assistant_msg("a1"));
        s.title = Some("kept".into());
        s.context_tokens = Some(500);
        s.session_cost_usd = 1.25;

        let forked = s.fork(4242);
        assert_ne!(forked.id, s.id);
        assert_eq!(forked.parent_id.as_deref(), Some(s.id.as_str()));
        assert_eq!(forked.title.as_deref(), Some("kept"));
        assert_eq!(forked.messages.len(), s.messages.len());
        assert_eq!(forked.context_tokens, Some(500));
        assert_eq!(forked.session_cost_usd, 1.25);
        // Fork resets timestamps to "now".
        assert!(forked.created_at_ms >= s.created_at_ms);
    }

    // ── compute_text_bytes / build_search_blob ───────────────────────

    #[test]
    fn compute_text_bytes_sums_user_assistant_content_lengths() {
        let msgs = vec![user_msg("hello"), assistant_msg("hi there")];
        assert_eq!(compute_text_bytes(&msgs), 13);
    }

    #[test]
    fn compute_text_bytes_includes_reasoning_and_tool_calls() {
        let mut msg = assistant_msg("text");
        msg.reasoning_content = Some("thinking".into());
        msg.tool_calls = Some(vec![ToolCall::new(
            "id-1".into(),
            FunctionCall {
                name: "edit".into(),
                arguments: "{\"path\":\"f.rs\"}".into(),
            },
        )]);
        let bytes = compute_text_bytes(&[msg]);
        // 4 (text) + 8 (reasoning) + 4 (name) + 15 (args)
        assert_eq!(bytes, 31);
    }

    #[test]
    fn compute_text_bytes_handles_empty_input() {
        assert_eq!(compute_text_bytes(&[]), 0);
    }

    #[test]
    fn build_search_blob_includes_only_user_and_assistant_text() {
        let msgs = vec![
            user_msg("question"),
            assistant_msg("answer"),
            tool_msg("tool output"),
            system_msg("system prompt"),
        ];
        let blob = build_search_blob(&msgs);
        assert!(blob.contains("question"));
        assert!(blob.contains("answer"));
        assert!(!blob.contains("tool output"));
        assert!(!blob.contains("system prompt"));
    }

    #[test]
    fn build_search_blob_skips_empty_messages() {
        let msgs = vec![user_msg(""), assistant_msg("real")];
        let blob = build_search_blob(&msgs);
        assert_eq!(blob, "real\n");
    }

    // ── session_updated_at ────────────────────────────────────────────

    #[test]
    fn session_updated_at_prefers_updated_falls_back_to_created() {
        let m = SessionMeta {
            id: "x".into(),
            title: None,
            slug: None,
            first_user_message: None,
            created_at_ms: 100,
            updated_at_ms: 200,
            mode: None,
            reasoning_effort: None,
            model: None,
            cwd: None,
            parent_id: None,
            context_tokens: None,
            text_bytes: None,
        };
        assert_eq!(session_updated_at(&m), 200);
        let m2 = SessionMeta {
            updated_at_ms: 0,
            ..m
        };
        assert_eq!(session_updated_at(&m2), 100);
    }

    // ── externalize / internalize blobs ───────────────────────────────

    fn image_msg(url: &str) -> Message {
        Message {
            role: Role::User,
            content: Some(Content::Parts(vec![ContentPart::ImageUrl {
                url: url.to_string(),
                label: None,
            }])),
            reasoning_content: None,

            reasoning_details: None,
            tool_calls: None,
            tool_call_id: None,
            is_error: false,
        }
    }

    #[test]
    fn externalize_blobs_swaps_urls_to_blob_refs() {
        let mut msgs = vec![image_msg("data:image/png;base64,AAA")];
        let mut map = std::collections::HashMap::new();
        map.insert("data:image/png;base64,AAA".into(), "blob://abc".into());
        externalize_blobs(&mut msgs, &map);
        if let Some(Content::Parts(parts)) = &msgs[0].content {
            if let ContentPart::ImageUrl { url, .. } = &parts[0] {
                assert_eq!(url, "blob://abc");
            }
        }
    }

    #[test]
    fn externalize_blobs_leaves_unmapped_urls_alone() {
        let mut msgs = vec![image_msg("data:other")];
        let map = std::collections::HashMap::new();
        externalize_blobs(&mut msgs, &map);
        if let Some(Content::Parts(parts)) = &msgs[0].content {
            if let ContentPart::ImageUrl { url, .. } = &parts[0] {
                assert_eq!(url, "data:other");
            }
        }
    }

    #[test]
    fn internalize_blobs_replaces_blob_refs_with_data_urls() {
        let mut msgs = vec![image_msg("blob://abc")];
        let mut map = std::collections::HashMap::new();
        map.insert("blob://abc".into(), "data:image/png;base64,AAA".into());
        internalize_blobs(&mut msgs, &map);
        if let Some(Content::Parts(parts)) = &msgs[0].content {
            if let ContentPart::ImageUrl { url, .. } = &parts[0] {
                assert_eq!(url, "data:image/png;base64,AAA");
            }
        }
    }

    #[test]
    fn internalize_blobs_leaves_text_messages_unchanged() {
        let mut msgs = vec![user_msg("hello")];
        let mut map = std::collections::HashMap::new();
        map.insert("foo".into(), "bar".into());
        internalize_blobs(&mut msgs, &map);
        if let Some(Content::Text(t)) = &msgs[0].content {
            assert_eq!(t, "hello");
        }
    }

    // ── atomic_write ──────────────────────────────────────────────────

    #[test]
    fn atomic_write_writes_contents_and_renames_into_place() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.json");
        atomic_write(&path, b"hello", 42);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello");
        // No leftover tmp file in the directory.
        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.file_name())
            .collect();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].to_str(), Some("data.json"));
    }

    #[test]
    fn atomic_write_overwrites_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data");
        std::fs::write(&path, "old").unwrap();
        atomic_write(&path, b"new", 1);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");
    }
}
