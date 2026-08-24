use super::*;

pub(crate) struct TranscriptDisplayDocument<'a> {
    document: &'a mut TranscriptDocument,
    lua: &'a LuaRuntime,
    width: u16,
    theme: &'a Theme,
}

impl<'a> TranscriptDisplayDocument<'a> {
    pub(crate) fn new(
        document: &'a mut TranscriptDocument,
        lua: &'a LuaRuntime,
        width: u16,
        theme: &'a Theme,
    ) -> Self {
        Self {
            document,
            lua,
            width,
            theme,
        }
    }
}

impl DisplayDocument for TranscriptDisplayDocument<'_> {
    fn snapshot(&mut self) -> DisplaySnapshot {
        DisplaySnapshot {
            generation: self.document.content.projection.projection_generation(),
            total_rows: self
                .document
                .approximate_scrollbar_total_rows(self.lua, self.width),
        }
    }

    fn materialize(
        &mut self,
        range: std::ops::Range<crate::smelt_edit::RowIndex>,
    ) -> crate::smelt_edit::DisplayRows {
        self.document.exact_or_gap_display_rows_for_range(
            self.lua,
            self.width,
            self.theme,
            range.start,
            range.end.saturating_sub(range.start),
        )
    }

    fn copy_range(&mut self, range: TextRange) -> Option<crate::smelt_edit::CopyOutput> {
        range.rows().map(|range| {
            self.document
                .copy_exact_loaded_range(self.lua, self.width, self.theme, range)
        })
    }
}

pub(crate) struct ResumePreviewCache {
    views: HashMap<String, TranscriptDocument>,
    order: VecDeque<String>,
    limit: usize,
}

impl ResumePreviewCache {
    pub(crate) fn new(limit: usize) -> Self {
        Self {
            views: HashMap::new(),
            order: VecDeque::new(),
            limit,
        }
    }

    pub(crate) fn take(&mut self, key: &str) -> Option<TranscriptDocument> {
        self.views.remove(key)
    }

    pub(crate) fn store(&mut self, key: String, view: TranscriptDocument) {
        self.order.retain(|existing| existing != &key);
        self.order.push_back(key.clone());
        self.views.insert(key.clone(), view);

        while self.order.len() > self.limit {
            let Some(old_key) = self.order.pop_front() else {
                break;
            };
            if old_key != key {
                self.views.remove(&old_key);
            }
        }
    }

    pub(crate) fn set_inline_options(&mut self, options: InlineOptions) {
        for view in self.views.values_mut() {
            view.set_inline_options(options.clone());
        }
    }

    pub(crate) fn invalidate_theme(&mut self) {
        for view in self.views.values_mut() {
            view.invalidate_theme();
        }
    }

    pub(crate) fn invalidate_renderer_if_changed(
        &mut self,
        generation: u64,
        cache_key: Option<u64>,
    ) {
        for view in self.views.values_mut() {
            view.invalidate_renderer_if_changed(generation, cache_key);
        }
    }
}
