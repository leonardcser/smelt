use crate::config;
use std::path::PathBuf;

pub struct History {
    entries: Vec<String>,
    cursor: usize,
    draft: String,
    path: PathBuf,
}

const RECORD_SEP: char = '\x1e';

impl History {
    pub fn load() -> Self {
        let path = config::state_dir().join("history");
        let entries = std::fs::read_to_string(&path)
            .unwrap_or_default()
            .split(RECORD_SEP)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect::<Vec<_>>();
        let cursor = entries.len();
        Self {
            entries,
            cursor,
            draft: String::new(),
            path,
        }
    }

    pub fn push(&mut self, entry: String) {
        if !entry.is_empty() && self.entries.last().is_none_or(|last| *last != entry) {
            self.entries.push(entry.clone());
            self.append_to_file(&entry);
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
        self.cursor = self.entries.len();
        self.draft.clear();
    }

    pub fn up(&mut self, current_buf: &str) -> Option<&str> {
        if self.entries.is_empty() {
            return None;
        }
        if self.cursor == self.entries.len() {
            self.draft = current_buf.to_string();
        }
        if self.cursor > 0 {
            self.cursor -= 1;
            Some(&self.entries[self.cursor])
        } else {
            None
        }
    }

    pub fn down(&mut self) -> Option<&str> {
        if self.cursor >= self.entries.len() {
            return None;
        }
        self.cursor += 1;
        if self.cursor == self.entries.len() {
            Some(&self.draft)
        } else {
            Some(&self.entries[self.cursor])
        }
    }

    pub fn entries(&self) -> &[String] {
        &self.entries
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn fresh(path: PathBuf) -> History {
        History {
            entries: Vec::new(),
            cursor: 0,
            draft: String::new(),
            path,
        }
    }

    fn loaded(path: PathBuf) -> History {
        let entries = std::fs::read_to_string(&path)
            .unwrap_or_default()
            .split(RECORD_SEP)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect::<Vec<_>>();
        let cursor = entries.len();
        History {
            entries,
            cursor,
            draft: String::new(),
            path,
        }
    }

    #[test]
    fn fresh_history_has_no_entries() {
        let dir = tempdir().unwrap();
        let h = fresh(dir.path().join("history"));
        assert!(h.entries().is_empty());
    }

    #[test]
    fn push_records_entry_and_persists_to_disk() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("history");
        let mut h = fresh(path.clone());
        h.push("hello".into());
        assert_eq!(h.entries(), &["hello"]);
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
        assert_eq!(h.entries(), &["a", "b", "a"]);
    }

    #[test]
    fn push_ignores_empty_entries() {
        let dir = tempdir().unwrap();
        let mut h = fresh(dir.path().join("history"));
        h.push("".into());
        assert!(h.entries().is_empty());
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
        assert_eq!(h.entries(), &["first", "second", "third"]);
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
        assert_eq!(h.entries(), &["a", "b"]);
    }

    #[test]
    fn loaded_missing_file_yields_empty_history() {
        let dir = tempdir().unwrap();
        let h = loaded(dir.path().join("does_not_exist"));
        assert!(h.entries().is_empty());
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
