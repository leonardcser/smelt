use super::block_buffers::BlockBufferCache;
use crate::ui::Buffer;
use crate::ui::Theme;
use smelt_core::buffer::{LineDecoration, Span, SpanMeta};
use smelt_core::transcript_model::{BlockHistory, LayoutKey, ViewState};
use smelt_core::transcript_present::ToolBodyRenderer;

/// Namespace name for transcript selection extmarks. Created on the
/// transcript display buffer at startup; populated each frame from the
/// active vim Visual / mouse drag / yank-flash range and read by
/// `Window::render` (which walks all namespaces in NsId order, so
/// selection paints over projection highlights).
pub(crate) const NS_SELECTION: &str = "transcript.selection";

/// Projection cache for the transcript buffer. Tracks the last
/// (generation, width, show_thinking) it projected at so repeated
/// renders short-circuit when nothing changed. The buffer itself
/// lives in `Ui::bufs`; the projection borrows it through `project`.
pub(crate) struct TranscriptProjection {
    generation: u64,
    width: u16,
    show_thinking: bool,
    cache: BlockBufferCache,
}

impl TranscriptProjection {
    pub(crate) fn new() -> Self {
        Self {
            generation: u64::MAX,
            width: 0,
            show_thinking: false,
            cache: BlockBufferCache::new(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn project(
        &mut self,
        buf: &mut Buffer,
        history: &mut BlockHistory,
        width: u16,
        show_thinking: bool,
        theme: &Theme,
        ephemeral: &Buffer,
        renderer: Option<&dyn ToolBodyRenderer>,
    ) {
        let gen = history.generation();
        if gen == self.generation && width == self.width && show_thinking == self.show_thinking {
            return;
        }

        // Generation changed — some block content mutated. Coarse
        // full-clear; incremental per-block invalidation is a perf
        // optimisation that can attach a hash-per-block when needed.
        if gen != self.generation || width != self.width {
            self.cache.clear();
        }

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
            let (block_buf, _) = self.cache.ensure(history, id, bkey, theme, renderer);
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

        self.generation = gen;
        self.width = width;
        self.show_thinking = show_thinking;
    }
}

fn apply_row_highlights(buf: &mut Buffer, row: usize, highlights: Vec<Span>) {
    for span in highlights {
        let meta: SpanMeta = span.meta;
        buf.add_highlight_group_with_meta(row, span.col_start, span.col_end, span.hl, meta);
    }
}
