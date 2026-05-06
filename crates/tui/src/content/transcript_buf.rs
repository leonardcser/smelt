use super::block_buffers::BlockBufferCache;
use crate::content::transcript_snapshot::TranscriptSnapshot;
use crate::ui::Buffer;
use crate::ui::Theme;
use smelt_core::buffer::{LineDecoration, Span, SpanMeta};
use smelt_core::transcript_model::{BlockHistory, LayoutKey, ViewState};

/// Namespace name for transcript selection extmarks. Created on the
/// transcript display buffer at startup; populated each frame from the
/// active vim Visual / mouse drag / yank-flash range and read by
/// `Window::render` (which walks all namespaces in NsId order, so
/// selection paints over projection highlights).
pub(crate) const NS_SELECTION: &str = "transcript.selection";

/// Single per-block cache shared between the display-buffer projection
/// and the snapshot consumers (copy / yank / line-break / cell-snap /
/// pane-focus). Both reads ride the same `BlockBufferCache`; both
/// invalidate on `BlockHistory::generation()` change.
pub(crate) struct TranscriptProjection {
    cache: BlockBufferCache,
    cache_generation: u64,
    cache_width: u16,
    /// Last `(generation, width, show_thinking)` we wrote into the
    /// display buffer. Same-key reprojection is a no-op.
    project_key: Option<ProjectKey>,
    /// Cached snapshot. Rebuilt lazily when its embedded
    /// `(generation, width, show_thinking)` no longer matches the
    /// caller's request.
    snapshot: Option<TranscriptSnapshot>,
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
        }
    }

    /// Drop cached per-block buffers when generation or width drifts.
    /// Width changes are key-discriminating in `BlockBufferCache`, but
    /// we still clear so dead entries don't accumulate after rewinds
    /// or terminal resizes. The snapshot also drops on generation
    /// change (its rows reflect the laid-out blocks).
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
        ephemeral: &Buffer,
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

        // Collect everything before committing to the buffer in one
        // shot. Appending block-by-block via Buffer's set_lines/
        // add_highlight is awkward when the destination starts with a
        // seed empty line; collecting first keeps row indices simple.
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

        // Apply.
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

    /// Lazily rebuilt snapshot of the transcript at `(width,
    /// show_thinking)`. Reuses the per-block cache `project()`
    /// populated. Returned reference is valid until the next call
    /// to `snapshot` or `project`.
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
            self.snapshot = Some(snap);
        }
        self.snapshot.as_ref().expect("just rebuilt")
    }
}

fn apply_row_highlights(buf: &mut Buffer, row: usize, highlights: Vec<Span>) {
    for span in highlights {
        let meta: SpanMeta = span.meta;
        buf.add_highlight_group_with_meta(row, span.col_start, span.col_end, span.hl, meta);
    }
}
