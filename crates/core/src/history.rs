use crate::config;
use std::path::PathBuf;

/// Input-history recall buffer. Entries share a single backing `String`;
/// `ranges` indexes byte ranges into that buffer, so loading 3 000 entries
/// is one read + one allocation instead of one allocation per entry.
pub struct History {
    buffer: String,
    ranges: Vec<(u32, u32)>,
    cursor: usize,
    draft: String,
    path: PathBuf,
}

const RECORD_SEP: char = '\x1e';
const RECORD_SEP_BYTE: u8 = 0x1e;

impl History {
    pub fn load() -> Self {
        Self::load_from_path(config::state_dir().join("history"))
    }

    pub fn load_from_state_root(state_root: impl AsRef<std::path::Path>) -> Self {
        Self::load_from_path(state_root.as_ref().join("history"))
    }

    fn load_from_path(path: PathBuf) -> Self {
        let buffer = std::fs::read_to_string(&path).unwrap_or_default();
        let ranges = compute_ranges(&buffer);
        let cursor = ranges.len();
        Self {
            buffer,
            ranges,
            cursor,
            draft: String::new(),
            path,
        }
    }

    pub fn push(&mut self, entry: String) {
        if !entry.is_empty() && self.last().is_none_or(|last| last != entry) {
            self.append_to_file(&entry);
            let start = self.buffer.len() as u32;
            self.buffer.push_str(&entry);
            let end = self.buffer.len() as u32;
            self.buffer.push(RECORD_SEP);
            self.ranges.push((start, end));
        }
        self.reset();
    }

    fn append_to_file(&self, entry: &str) {
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            let _ = write!(f, "{}{}", entry, RECORD_SEP);
        }
    }

    fn reset(&mut self) {
        self.cursor = self.ranges.len();
        self.draft.clear();
    }

    fn at(&self, i: usize) -> &str {
        let (s, e) = self.ranges[i];
        &self.buffer[s as usize..e as usize]
    }

    fn last(&self) -> Option<&str> {
        self.ranges
            .last()
            .map(|&(s, e)| &self.buffer[s as usize..e as usize])
    }

    pub fn up(&mut self, current_buf: &str) -> Option<&str> {
        if self.ranges.is_empty() {
            return None;
        }
        if self.cursor == self.ranges.len() {
            self.draft = current_buf.to_string();
        }
        if self.cursor > 0 {
            self.cursor -= 1;
            Some(self.at(self.cursor))
        } else {
            None
        }
    }

    pub fn down(&mut self) -> Option<&str> {
        if self.cursor >= self.ranges.len() {
            return None;
        }
        self.cursor += 1;
        if self.cursor == self.ranges.len() {
            Some(&self.draft)
        } else {
            Some(self.at(self.cursor))
        }
    }

    /// Borrowed view over all entries (oldest first). Each `&str` aliases into
    /// the shared backing buffer; no per-entry heap allocation. Iterator form
    /// so consumers that only need to forward (Lua FFI, search) don't pay for
    /// a transient `Vec<&str>` on every call.
    pub fn entries(&self) -> impl ExactSizeIterator<Item = &str> + '_ {
        self.ranges
            .iter()
            .map(|&(s, e)| &self.buffer[s as usize..e as usize])
    }

    pub fn len(&self) -> usize {
        self.ranges.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ranges.is_empty()
    }
}

fn compute_ranges(buffer: &str) -> Vec<(u32, u32)> {
    let bytes = buffer.as_bytes();
    let mut ranges = Vec::with_capacity(bytes.iter().filter(|&&b| b == RECORD_SEP_BYTE).count());
    let mut start = 0usize;
    for (i, &b) in bytes.iter().enumerate() {
        if b == RECORD_SEP_BYTE {
            if start < i {
                ranges.push((start as u32, i as u32));
            }
            start = i + 1;
        }
    }
    if start < bytes.len() {
        ranges.push((start as u32, bytes.len() as u32));
    }
    ranges
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn fresh(path: PathBuf) -> History {
        History {
            buffer: String::new(),
            ranges: Vec::new(),
            cursor: 0,
            draft: String::new(),
            path,
        }
    }

    fn loaded(path: PathBuf) -> History {
        let buffer = std::fs::read_to_string(&path).unwrap_or_default();
        let ranges = compute_ranges(&buffer);
        let cursor = ranges.len();
        History {
            buffer,
            ranges,
            cursor,
            draft: String::new(),
            path,
        }
    }

    fn collect(h: &History) -> Vec<&str> {
        h.entries().collect()
    }

    #[test]
    fn fresh_history_has_no_entries() {
        let dir = tempdir().unwrap();
        let h = fresh(dir.path().join("history"));
        assert!(h.is_empty());
    }

    #[test]
    fn push_records_entry_and_persists_to_disk() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("history");
        let mut h = fresh(path.clone());
        h.push("hello".into());
        assert_eq!(collect(&h), vec!["hello"]);
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert!(on_disk.starts_with("hello"));
        assert!(on_disk.ends_with(RECORD_SEP));
    }

    #[test]
    fn push_dedupes_consecutive_duplicates() {
        let dir = tempdir().unwrap();
        let mut h = fresh(dir.path().join("history"));
        h.push("a".into());
        h.push("a".into());
        h.push("b".into());
        h.push("a".into());
        assert_eq!(collect(&h), vec!["a", "b", "a"]);
    }

    #[test]
    fn push_ignores_empty_entries() {
        let dir = tempdir().unwrap();
        let mut h = fresh(dir.path().join("history"));
        h.push("".into());
        assert!(h.is_empty());
    }

    #[test]
    fn push_dedup_does_not_grow_backing_buffer() {
        // Regression guard: when push() short-circuits on a duplicate, neither the
        // file nor the in-memory buffer should accumulate bytes.
        let dir = tempdir().unwrap();
        let mut h = fresh(dir.path().join("history"));
        h.push("repeat".into());
        let buf_after_first = h.buffer.len();
        let ranges_after_first = h.ranges.len();
        for _ in 0..10 {
            h.push("repeat".into());
        }
        assert_eq!(h.buffer.len(), buf_after_first);
        assert_eq!(h.ranges.len(), ranges_after_first);
    }

    #[test]
    fn up_walks_back_through_entries_and_returns_none_at_top() {
        let dir = tempdir().unwrap();
        let mut h = fresh(dir.path().join("history"));
        h.push("a".into());
        h.push("b".into());
        h.push("c".into());
        assert_eq!(h.up("draft"), Some("c"));
        assert_eq!(h.up("ignored"), Some("b"));
        assert_eq!(h.up("ignored"), Some("a"));
        assert_eq!(h.up("ignored"), None);
    }

    #[test]
    fn up_returns_none_when_history_empty() {
        let dir = tempdir().unwrap();
        let mut h = fresh(dir.path().join("history"));
        assert_eq!(h.up("anything"), None);
    }

    #[test]
    fn up_saves_draft_on_first_call_only() {
        let dir = tempdir().unwrap();
        let mut h = fresh(dir.path().join("history"));
        h.push("a".into());
        h.push("b".into());
        let _ = h.up("draft-buf");
        let _ = h.up("ignored");
        // Walk back to bottom; `down` past last entry surfaces the saved draft.
        let _ = h.down();
        let bottom = h.down();
        assert_eq!(bottom, Some("draft-buf"));
    }

    #[test]
    fn down_returns_draft_when_past_last_entry() {
        let dir = tempdir().unwrap();
        let mut h = fresh(dir.path().join("history"));
        h.push("a".into());
        let _ = h.up("typed");
        let bottom = h.down();
        assert_eq!(bottom, Some("typed"));
    }

    #[test]
    fn down_returns_none_when_already_at_or_past_end() {
        let dir = tempdir().unwrap();
        let mut h = fresh(dir.path().join("history"));
        h.push("a".into());
        assert_eq!(h.down(), None);
    }

    #[test]
    fn down_walks_forward_through_entries() {
        let dir = tempdir().unwrap();
        let mut h = fresh(dir.path().join("history"));
        h.push("a".into());
        h.push("b".into());
        h.push("c".into());
        let _ = h.up("");
        let _ = h.up("");
        let _ = h.up("");
        assert_eq!(h.down(), Some("b"));
        assert_eq!(h.down(), Some("c"));
    }

    #[test]
    fn push_resets_cursor_back_to_end_after_navigation() {
        let dir = tempdir().unwrap();
        let mut h = fresh(dir.path().join("history"));
        h.push("a".into());
        h.push("b".into());
        let _ = h.up("");
        h.push("c".into());
        // After push, cursor is reset; up walks from the new end.
        assert_eq!(h.up(""), Some("c"));
    }

    #[test]
    fn loaded_replays_record_separated_entries_from_disk() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("history");
        std::fs::write(
            &path,
            format!("first{RECORD_SEP}second{RECORD_SEP}third{RECORD_SEP}"),
        )
        .unwrap();
        let h = loaded(path);
        assert_eq!(collect(&h), vec!["first", "second", "third"]);
    }

    #[test]
    fn loaded_skips_empty_records() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("history");
        std::fs::write(
            &path,
            format!("{RECORD_SEP}a{RECORD_SEP}{RECORD_SEP}b{RECORD_SEP}"),
        )
        .unwrap();
        let h = loaded(path);
        assert_eq!(collect(&h), vec!["a", "b"]);
    }

    #[test]
    fn loaded_missing_file_yields_empty_history() {
        let dir = tempdir().unwrap();
        let h = loaded(dir.path().join("does_not_exist"));
        assert!(h.is_empty());
    }

    #[test]
    fn append_creates_parent_directories_as_needed() {
        let dir = tempdir().unwrap();
        let nested = dir.path().join("a").join("b").join("history");
        let mut h = fresh(nested.clone());
        h.push("entry".into());
        assert!(nested.exists());
    }
}
