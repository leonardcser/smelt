use crate::fuzzy::history::HistoryIndex;
use smelt_buffer::attached::AttachedText;

/// What a history step brings back into the prompt.
#[derive(Debug, PartialEq, Eq)]
pub enum Recall<'a> {
    /// A saved prompt.
    Entry(&'a str),
    /// The prompt as it was before browsing began, with its attachments.
    Draft(AttachedText),
}

/// Input-history recall over the prompts saved in the store.
pub struct History {
    entries: Vec<String>,
    cursor: usize,
    /// The prompt set aside by the first step up, returned by the last step down.
    draft: AttachedText,
    store: smelt_store::Store,
    /// Search index over `entries`, built on first search after a change.
    search_index: Option<HistoryIndex>,
}

impl History {
    pub fn load(store: smelt_store::Store) -> Self {
        let entries = store.prompts().unwrap_or_default();
        Self {
            cursor: entries.len(),
            entries,
            draft: AttachedText::default(),
            store,
            search_index: None,
        }
    }

    pub fn push(&mut self, entry: String) {
        if !entry.is_empty() && self.entries.last() != Some(&entry) {
            let _ = self.store.push_prompt(&entry);
            self.entries.push(entry);
            self.search_index = None;
        }
        self.reset();
    }

    fn reset(&mut self) {
        self.cursor = self.entries.len();
        self.draft = AttachedText::default();
    }

    /// Step to the next older entry. The first step sets `current` aside as
    /// the draft; `current` is only built when that happens.
    pub fn up(&mut self, current: impl FnOnce() -> AttachedText) -> Option<&str> {
        if self.entries.is_empty() {
            return None;
        }
        if self.cursor == self.entries.len() {
            self.draft = current();
        }
        if self.cursor > 0 {
            self.cursor -= 1;
            Some(&self.entries[self.cursor])
        } else {
            None
        }
    }

    /// Step to the next newer entry, or past the newest back to the draft.
    pub fn down(&mut self) -> Option<Recall<'_>> {
        if self.cursor >= self.entries.len() {
            return None;
        }
        self.cursor += 1;
        if self.cursor == self.entries.len() {
            Some(Recall::Draft(std::mem::take(&mut self.draft)))
        } else {
            Some(Recall::Entry(&self.entries[self.cursor]))
        }
    }

    /// All entries, oldest first.
    pub fn entries(&self) -> impl ExactSizeIterator<Item = &str> + '_ {
        self.entries.iter().map(String::as_str)
    }

    /// Up to `limit` entry indices (oldest first) matching `query`, best
    /// first. See [`HistoryIndex::search`].
    pub fn search(&mut self, query: &str, limit: usize) -> Vec<usize> {
        let entries = &self.entries;
        self.search_index
            .get_or_insert_with(|| HistoryIndex::build(entries.iter().map(String::as_str)))
            .search(query, limit)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn history() -> (tempfile::TempDir, History) {
        let dir = tempfile::tempdir().unwrap();
        let history = History::load(smelt_store::Store::new(dir.path()));
        (dir, history)
    }

    fn collect(h: &History) -> Vec<&str> {
        h.entries().collect()
    }

    #[test]
    fn push_saves_entries_for_the_next_load() {
        let (dir, mut h) = history();
        h.push("hello".into());
        h.push("".into());
        h.push("world".into());
        let reloaded = History::load(smelt_store::Store::new(dir.path()));
        assert_eq!(collect(&reloaded), ["hello", "world"]);
    }

    #[test]
    fn push_dedupes_consecutive_duplicates() {
        let (_dir, mut h) = history();
        for entry in ["a", "a", "b", "a"] {
            h.push(entry.into());
        }
        assert_eq!(collect(&h), ["a", "b", "a"]);
    }

    #[test]
    fn up_walks_back_through_entries_and_returns_none_at_top() {
        let (_dir, mut h) = history();
        for entry in ["a", "b", "c"] {
            h.push(entry.into());
        }
        assert_eq!(h.up(|| AttachedText::plain("draft")), Some("c"));
        assert_eq!(h.up(|| unreachable!()), Some("b"));
        assert_eq!(h.up(|| unreachable!()), Some("a"));
        assert_eq!(h.up(|| unreachable!()), None);
    }

    #[test]
    fn up_returns_none_when_history_empty() {
        let (_dir, mut h) = history();
        assert_eq!(h.up(|| AttachedText::plain("anything")), None);
    }

    #[test]
    fn down_returns_the_draft_saved_by_the_first_up_with_its_attachments() {
        let (_dir, mut h) = history();
        h.push("a".into());
        h.push("b".into());
        let draft = AttachedText::new(
            format!("look {}", smelt_buffer::ATTACHMENT_MARKER),
            vec![smelt_buffer::attachment::Attachment::image(
                "shot.png".into(),
                "data:image/png;base64,AAAA".into(),
            )],
        );
        let _ = h.up(|| draft.clone());
        let _ = h.up(|| unreachable!());
        assert_eq!(h.down(), Some(Recall::Entry("b")));
        assert_eq!(h.down(), Some(Recall::Draft(draft)));
        assert_eq!(h.down(), None);
    }

    #[test]
    fn push_resets_cursor_back_to_end_after_navigation() {
        let (_dir, mut h) = history();
        h.push("a".into());
        h.push("b".into());
        let _ = h.up(AttachedText::default);
        h.push("c".into());
        assert_eq!(h.up(AttachedText::default), Some("c"));
    }

    #[test]
    fn search_sees_entries_pushed_after_a_previous_search() {
        let (_dir, mut h) = history();
        h.push("deploy staging".into());
        assert_eq!(h.search("deploy", 10), vec![0]);
        h.push("deploy prod".into());
        assert_eq!(h.search("deploy", 10), vec![1, 0]);
    }
}
