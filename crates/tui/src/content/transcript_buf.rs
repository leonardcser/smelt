use super::block_buffers::BlockBufferCache;
use crate::content::transcript_snapshot::TranscriptSnapshot;
use crate::smelt_term::Buffer;
use crate::smelt_term::Theme;
use smelt_core::buffer::{LineDecoration, Span, SpanMeta};
use smelt_core::transcript_model::{BlockHistory, LayoutKey, ViewState};
use std::sync::{Arc, Mutex};

/// Shared latest snapshot. The transcript's `BufferCopy` impl reads from this
/// holder after every rebuild so `Buffer::copy_range` returns rendered text
/// without the host needing to plumb the snapshot through every call site.
pub(crate) type SharedSnapshot = Arc<Mutex<Option<Arc<TranscriptSnapshot>>>>;

pub(crate) struct TranscriptProjection {
    cache: BlockBufferCache,
    cache_generation: u64,
    cache_width: u16,
    project_key: Option<ProjectKey>,
    snapshot: Option<Arc<TranscriptSnapshot>>,
    shared: SharedSnapshot,
}

#[derive(PartialEq, Eq, Clone, Copy)]
struct ProjectKey {
    generation: u64,
    width: u16,
    show_thinking: bool,
}

impl TranscriptProjection {
    pub(crate) fn new() -> Self {
        Self {
            cache: BlockBufferCache::new(),
            cache_generation: u64::MAX,
            cache_width: 0,
            project_key: None,
            snapshot: None,
            shared: Arc::new(Mutex::new(None)),
        }
    }

    /// Handle to the latest snapshot. The transcript buffer's `BufferCopy`
    /// reads from this Arc to compute rendered yanks.
    pub(crate) fn shared_snapshot(&self) -> SharedSnapshot {
        self.shared.clone()
    }

    /// Clear cache on generation or width change so stale entries don't accumulate.
    fn gc_if_stale(&mut self, gen: u64, width: u16) {
        if gen != self.cache_generation || width != self.cache_width {
            self.cache.clear();
            self.cache_generation = gen;
            self.cache_width = width;
            self.snapshot = None;
        }
    }

    pub(crate) fn project(
        &mut self,
        buf: &mut Buffer,
        history: &mut BlockHistory,
        width: u16,
        show_thinking: bool,
        theme: &Theme,
        ephemeral: Option<&Buffer>,
    ) {
        let gen = history.generation();
        let key = ProjectKey {
            generation: gen,
            width,
            show_thinking,
        };
        if self.project_key == Some(key) {
            return;
        }

        self.gc_if_stale(gen, width);

        let base_key = LayoutKey {
            view_state: ViewState::Expanded,
            width,
            show_thinking,
            content_hash: 0,
        };

        let _walk = smelt_perf::perf::begin("project:walk_blocks");
        let mut texts: Vec<String> = Vec::new();
        let mut highlights: Vec<Vec<Span>> = Vec::new();
        let mut decorations: Vec<LineDecoration> = Vec::new();

        let emit = |row_text: String,
                    row_highlights: Vec<Span>,
                    row_decoration: LineDecoration,
                    texts: &mut Vec<String>,
                    highlights: &mut Vec<Vec<Span>>,
                    decorations: &mut Vec<LineDecoration>| {
            texts.push(row_text);
            highlights.push(row_highlights);
            decorations.push(row_decoration);
        };

        for i in 0..history.len() {
            let gap = history.block_gap(i);
            for _ in 0..gap {
                emit(
                    String::new(),
                    Vec::new(),
                    LineDecoration::default(),
                    &mut texts,
                    &mut highlights,
                    &mut decorations,
                );
            }

            let id = history.order[i];
            let bkey = history.resolve_key(id, base_key);
            let (block_buf, _) = self.cache.ensure(history, id, bkey, theme);
            for r in 0..block_buf.line_count() {
                let text = block_buf.get_line(r).unwrap_or("").to_string();
                let row_h = block_buf.highlights_at(r);
                let dec = block_buf.decoration_at(r).clone();
                emit(
                    text,
                    row_h,
                    dec,
                    &mut texts,
                    &mut highlights,
                    &mut decorations,
                );
            }
        }

        if let Some(ephemeral) = ephemeral {
            for r in 0..ephemeral.line_count() {
                let text = ephemeral.get_line(r).unwrap_or("").to_string();
                let row_h = ephemeral.highlights_at(r);
                let dec = ephemeral.decoration_at(r).clone();
                emit(
                    text,
                    row_h,
                    dec,
                    &mut texts,
                    &mut highlights,
                    &mut decorations,
                );
            }
        }
        drop(_walk);

        buf.set_all_lines(texts);
        for (row, row_highlights) in highlights.into_iter().enumerate() {
            apply_row_highlights(buf, row, row_highlights);
        }
        for (row, dec) in decorations.into_iter().enumerate() {
            if dec != LineDecoration::default() {
                buf.set_decoration(row, dec);
            }
        }

        self.project_key = Some(key);
    }

    pub(crate) fn snapshot(
        &mut self,
        history: &mut BlockHistory,
        width: u16,
        show_thinking: bool,
        theme: &Theme,
    ) -> &TranscriptSnapshot {
        let gen = history.generation();
        let needs_rebuild = match &self.snapshot {
            None => true,
            Some(s) => s.generation != gen || s.width != width || s.show_thinking != show_thinking,
        };
        if needs_rebuild {
            self.gc_if_stale(gen, width);
            let snap = crate::content::transcript_snapshot::build_snapshot(
                &mut self.cache,
                history,
                width,
                show_thinking,
                theme,
            );
            let snap = Arc::new(snap);
            *self.shared.lock().unwrap() = Some(snap.clone());
            self.snapshot = Some(snap);
        }
        self.snapshot.as_deref().expect("just rebuilt")
    }
}

/// Yank transform for the transcript. `kill_ring` keeps the raw source bytes
/// (paste-back fidelity), `clipboard` walks the latest snapshot's cells so
/// `copy_as` substitutions, soft-wrap merging, and `source_text` row overrides
/// are honored on external paste.
pub(crate) struct TranscriptCopier {
    shared: SharedSnapshot,
}

impl TranscriptCopier {
    pub(crate) fn new(shared: SharedSnapshot) -> Self {
        Self { shared }
    }
}

impl smelt_core::buffer::BufferCopy for TranscriptCopier {
    fn copy(&self, buf: &Buffer, range: std::ops::Range<usize>) -> smelt_core::buffer::CopyOutput {
        // Transcript has no `source` — its editable-byte space is
        // `lines.join("\n")`, which `buf.text()` returns.
        let text = buf.text();
        let raw = text
            .get(range.start..range.end)
            .map(str::to_string)
            .unwrap_or_default();
        let snap = self.shared.lock().unwrap().clone();
        let clipboard = match snap {
            Some(s) => s.copy_byte_range(range.start, range.end),
            None => raw.clone(),
        };
        smelt_core::buffer::CopyOutput {
            kill_ring: raw,
            clipboard,
        }
    }
}

fn apply_row_highlights(buf: &mut Buffer, row: usize, highlights: Vec<Span>) {
    for span in highlights {
        let meta: SpanMeta = span.meta;
        buf.add_highlight_group_with_meta(row, span.col_start, span.col_end, span.hl, meta);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smelt_core::content::transcript::Transcript;
    use smelt_core::transcript_model::Block;

    #[test]
    fn project_without_ephemeral_does_not_append_default_blank_row() {
        let mut transcript = Transcript::new();
        transcript.push(Block::Text {
            content: "hello".into(),
        });
        let theme = Theme::default();
        let mut projection = TranscriptProjection::new();
        let mut buf = Buffer::new(crate::smelt_term::BufId(1), Default::default());

        projection.project(&mut buf, &mut transcript.history, 80, false, &theme, None);
        let snap_rows = projection
            .snapshot(&mut transcript.history, 80, false, &theme)
            .rows
            .len();

        assert_eq!(buf.line_count(), snap_rows);
        assert_eq!(buf.get_line(buf.line_count() - 1), Some("hello"));
    }
}
