use super::to_buffer::{apply_to_buffer, project_display_line, ProjectedLine};
use crate::ui::buffer::Buffer;
use crate::ui::Theme;
use smelt_core::content::display::DisplayLine;
use smelt_core::transcript_model::{BlockHistory, LayoutKey, ViewState};

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
}

impl TranscriptProjection {
    pub(crate) fn new() -> Self {
        Self {
            generation: u64::MAX,
            width: 0,
            show_thinking: false,
        }
    }

    pub(crate) fn project(
        &mut self,
        buf: &mut Buffer,
        history: &mut BlockHistory,
        width: u16,
        show_thinking: bool,
        theme: &Theme,
        ephemeral_lines: &[DisplayLine],
    ) {
        let gen = history.generation();
        if gen == self.generation && width == self.width && show_thinking == self.show_thinking {
            return;
        }

        if width as usize != history.cache_width {
            history.invalidate_for_width(width as usize);
        }

        let key = LayoutKey {
            view_state: ViewState::Expanded,
            width,
            show_thinking,
            content_hash: 0,
        };

        let mut lines: Vec<ProjectedLine> = Vec::new();

        for i in 0..history.len() {
            let rows = history.ensure_rows(i, key);
            let gap = if rows == 0 { 0 } else { history.block_gap(i) };

            for _ in 0..gap {
                lines.push(ProjectedLine::default());
            }

            let id = history.order[i];
            let bkey = history.resolve_key(id, key);
            if let Some(display) = history.artifacts.get(&id).and_then(|a| a.get(bkey)) {
                for dline in &display.lines {
                    lines.push(project_display_line(dline, theme));
                }
            }
        }

        for dline in ephemeral_lines {
            lines.push(project_display_line(dline, theme));
        }

        apply_to_buffer(buf, &lines);

        self.generation = gen;
        self.width = width;
        self.show_thinking = show_thinking;
    }
}
