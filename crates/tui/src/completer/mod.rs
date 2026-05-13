pub(crate) mod command;
pub(crate) mod file;
pub(crate) mod history;

#[derive(Clone)]
pub(crate) struct CompletionItem {
    pub(crate) label: String,
    pub(crate) description: Option<String>,
    /// Paints pill / label / description in this color when set.
    pub(crate) ansi_color: Option<u8>,
}

impl CompletionItem {
    pub(crate) fn new(label: String, description: Option<String>, ansi_color: Option<u8>) -> Self {
        Self {
            label,
            description,
            ansi_color,
        }
    }
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
    /// Indices into `all_items`, ranked best-first.
    pub(crate) results: Vec<usize>,
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
        self.results
            .get(self.selected)
            .and_then(|&i| self.all_items.get(i))
    }

    pub(crate) fn results_iter(&self) -> impl Iterator<Item = &CompletionItem> {
        self.results.iter().map(move |&i| &self.all_items[i])
    }

    pub(crate) fn max_visible_rows(&self) -> usize {
        5
    }

    fn item_key(item: &CompletionItem) -> &str {
        &item.label
    }

    fn remember_selected_key(&mut self) {
        self.selected_key = self
            .selected_item()
            .map(|item| Self::item_key(item).to_string());
    }

    fn restore_selected_key(&mut self) {
        if let Some(ref key) = self.selected_key {
            if let Some(idx) = self
                .results
                .iter()
                .position(|&i| Self::item_key(&self.all_items[i]) == key)
            {
                self.selected = idx;
                return;
            }
        }
        if self.selected >= self.results.len() {
            self.selected = 0;
        }
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
        let _perf = smelt_perf::perf::begin("completer:filter");
        if preserve_selection {
            self.remember_selected_key();
        }
        self.results.clear();
        if self.query.is_empty() {
            self.results.extend(0..self.all_items.len());
        } else {
            let labels: Vec<&str> = self.all_items.iter().map(|it| it.label.as_str()).collect();
            self.results
                .extend(smelt_core::fuzzy::fuzzy_rank(&self.query, &labels));
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
        self.selected_item().map(|i| i.label.as_str())
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
