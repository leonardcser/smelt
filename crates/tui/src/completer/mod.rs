use crate::fuzzy::score::{query_match_score, split_words};

pub(crate) mod command;
pub(crate) mod file;
pub(crate) mod history;

#[derive(Clone, Default)]
pub(crate) struct CompletionItem {
    pub(crate) label: String,
    pub(crate) description: Option<String>,
    /// When set, paints the row's pill, label, and description in this color.
    pub(crate) ansi_color: Option<u8>,
    /// Extra match terms not shown in the label (e.g. provider key for the model picker).
    pub(crate) search_terms: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum CompleterKind {
    File,
    Command,
    CommandArg,
}

pub(crate) struct Completer {
    /// Byte offset in the buffer where the trigger char starts.
    pub(crate) anchor: usize,
    pub(crate) kind: CompleterKind,
    pub(crate) query: String,
    pub(crate) results: Vec<CompletionItem>,
    pub(crate) selected: usize,
    pub(super) all_items: Vec<CompletionItem>,
    /// Stable selection identity preserved across re-filter.
    pub(super) selected_key: Option<String>,
}

impl Completer {
    /// Replace the item list and re-filter, keeping the current selection stable.
    pub(crate) fn refresh_items(&mut self, items: Vec<CompletionItem>) {
        self.all_items = items;
        self.filter_inner(true);
    }

    pub(crate) fn all_items(&self) -> &[CompletionItem] {
        &self.all_items
    }

    pub(crate) fn selected_item(&self) -> Option<&CompletionItem> {
        self.results.get(self.selected)
    }

    pub(crate) fn max_visible_rows(&self) -> usize {
        5
    }

    fn item_key(item: &CompletionItem) -> &str {
        &item.label
    }

    fn remember_selected_key(&mut self) {
        self.selected_key = self
            .results
            .get(self.selected)
            .map(|item| Self::item_key(item).to_string());
    }

    fn restore_selected_key(&mut self) {
        if let Some(ref key) = self.selected_key {
            if let Some(idx) = self
                .results
                .iter()
                .position(|item| Self::item_key(item) == key)
            {
                self.selected = idx;
                return;
            }
        }
        if self.selected >= self.results.len() {
            self.selected = 0;
        }
    }

    fn search_fields(&self, item: &CompletionItem) -> Vec<String> {
        let mut fields = vec![item.label.to_lowercase()];
        if let Some(t) = item.search_terms.as_deref() {
            fields.push(t.to_lowercase());
        }
        fields
    }

    pub(crate) fn update_query(&mut self, query: String) {
        self.query = query;
        self.selected = 0;
        self.selected_key = None;
        self.filter();
    }

    fn filter(&mut self) {
        self.filter_inner(false);
    }

    fn filter_inner(&mut self, preserve_selection: bool) {
        let _perf = smelt_core::perf::begin("completer:filter");
        if preserve_selection {
            self.remember_selected_key();
        }
        if self.query.is_empty() {
            self.results = self.all_items.clone();
        } else {
            let query = self.query.to_lowercase();
            let query_words = split_words(&query);
            let mut scored: Vec<_> = self
                .all_items
                .iter()
                .enumerate()
                .filter_map(|(i, item)| {
                    let fields = self.search_fields(item);
                    let score = query_match_score(&query, &query_words, &fields)?;
                    Some((score, i, item.clone()))
                })
                .collect();
            scored.sort_by_key(|(s, i, _)| (*s, *i));
            self.results = scored.into_iter().map(|(_, _, item)| item).collect();
        }
        if preserve_selection {
            self.restore_selected_key();
        } else {
            self.selected = 0;
        }
    }

    pub(crate) fn move_up(&mut self) {
        if !self.results.is_empty() {
            self.selected = if self.selected == 0 {
                self.results.len() - 1
            } else {
                self.selected - 1
            };
            self.remember_selected_key();
        }
    }

    pub(crate) fn move_down(&mut self) {
        if !self.results.is_empty() {
            self.selected = (self.selected + 1) % self.results.len();
            self.remember_selected_key();
        }
    }

    pub(crate) fn accept(&self) -> Option<&str> {
        self.results.get(self.selected).map(|i| i.label.as_str())
    }
}

/// Pairs a `Completer` with its picker-overlay leaf. One lifecycle: open → close.
pub(crate) struct CompleterSession {
    pub(crate) completer: Completer,
    pub(crate) picker_win: Option<crate::smelt_term::WinId>,
}

impl CompleterSession {
    pub(crate) fn new(completer: Completer) -> Self {
        Self {
            completer,
            picker_win: None,
        }
    }
}

impl std::ops::Deref for CompleterSession {
    type Target = Completer;
    fn deref(&self) -> &Completer {
        &self.completer
    }
}

impl std::ops::DerefMut for CompleterSession {
    fn deref_mut(&mut self) -> &mut Completer {
        &mut self.completer
    }
}
