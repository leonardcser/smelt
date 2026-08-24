use crate::input::PromptState;
use crate::smelt_edit::{Buffer, ExtmarkOpts, ExtmarkPayload};

/// Extmark namespace for `Win:placeholder` text rendered as a dim
/// suggestion when the buffer is empty.
pub(crate) const PLACEHOLDER_NS: &str = "placeholder";

pub(crate) fn set_placeholder_extmark(buf: &mut Buffer, text: Option<String>) {
    let ns = buf.create_namespace(PLACEHOLDER_NS);
    buf.clear_namespace(ns, 0, usize::MAX);
    if let Some(text) = text.filter(|s| !s.is_empty()) {
        buf.set_extmark(
            ns,
            0,
            0,
            ExtmarkOpts::virt_text(text, Some("GhostText".into())),
        );
    }
}

pub(crate) fn placeholder_text(buf: &mut Buffer) -> Option<String> {
    let ns = buf.create_namespace(PLACEHOLDER_NS);
    buf.extmarks(ns).into_iter().find_map(|(_, mark)| {
        if let ExtmarkPayload::VirtText { text, .. } = &mark.payload {
            Some(text.clone())
        } else {
            None
        }
    })
}

pub(crate) struct InputLeafInput<'a> {
    pub(crate) input: &'a PromptState,
    pub(crate) win: &'a crate::smelt_edit::Window,
    pub(crate) clipboard: &'a crate::smelt_edit::Clipboard,
    /// Host clock at render time; used to compute the yank-flash window.
    pub(crate) now: std::time::Instant,
}

/// Write parser-derived prompt selection and yank-flash overlays into `buf`.
/// Prompt text projection lives in `PromptBufferParser`; `Window::render` applies gutters and wrapping.
pub(crate) fn sync_prompt_overlays(inp: &InputLeafInput<'_>, buf: &mut Buffer) {
    assert!(
        buf.has_parser(),
        "prompt input buffer should be projected by PromptBufferParser"
    );

    if let Some((start, end)) = inp
        .input
        .selection_range(crate::input::PromptCtxRef { buf, win: inp.win })
    {
        let ranges = smelt_buffer::coords::byte_range_to_row_ranges(buf, start, end);
        buf.set_range_layer(crate::smelt_edit::RangeLayer::Selection, ranges);
    } else {
        buf.clear_range_layer(crate::smelt_edit::RangeLayer::Selection);
    }
    if let Some((start, end)) = inp.input.yank_flash_range(
        crate::input::PromptCtxRef { buf, win: inp.win },
        inp.clipboard,
        inp.now,
    ) {
        let ranges = smelt_buffer::coords::byte_range_to_row_ranges(buf, start, end);
        buf.set_range_layer(crate::smelt_edit::RangeLayer::YankFlash, ranges);
    } else {
        buf.clear_range_layer(crate::smelt_edit::RangeLayer::YankFlash);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sync_prompt_overlays_writes_overlays_only() {
        let input_state = PromptState::default();
        let test_clipboard = crate::smelt_edit::Clipboard::null();
        let test_win = crate::smelt_edit::Window::new(
            crate::app::PROMPT_WIN,
            crate::app::PROMPT_EDIT_BUF,
            crate::smelt_edit::SplitConfig {
                region: "prompt".into(),
                gutters: crate::smelt_edit::Gutters::default(),
            },
        );
        let inp = InputLeafInput {
            input: &input_state,
            win: &test_win,
            clipboard: &test_clipboard,
            now: std::time::Instant::now(),
        };
        let mut input_buf = Buffer::new(
            crate::app::PROMPT_EDIT_BUF,
            crate::smelt_edit::BufCreateOpts::default(),
        );
        input_buf.set_parser(std::sync::Arc::new(
            crate::content::prompt_parser::PromptBufferParser::new(input_state.store.clone()),
        ));
        input_buf.ensure_rendered_at(78);
        sync_prompt_overlays(&inp, &mut input_buf);
        assert_eq!(input_buf.line_count(), 1);
    }

    #[test]
    fn window_render_honors_prompt_gutters() {
        use crate::app::PROMPT_WIN;
        use crate::smelt_edit::{
            grid::Grid, BufCreateOpts, CursorShape, DrawContext, Gutters, SplitConfig, Theme,
            Window,
        };

        let theme: std::sync::Arc<Theme> = crate::theme::default_baked().clone();

        let mut buf = Buffer::new(crate::app::PROMPT_EDIT_BUF, BufCreateOpts::default());
        buf.set_all_lines(vec!["hello".into()]);

        let win = Window::new(
            PROMPT_WIN,
            buf.id(),
            SplitConfig {
                region: "prompt".into(),
                gutters: Gutters {
                    pad_left: 1,
                    pad_right: 1,
                    ..Default::default()
                },
            },
        );

        let mut grid = Grid::new(10, 1);
        let mut slice = grid.slice_mut(crate::smelt_edit::Rect::new(0, 0, 10, 1));
        let ctx = DrawContext {
            terminal_width: 10,
            terminal_height: 1,
            focused: true,
            cursor_shape: CursorShape::Block {
                glyph: '█',
                style: crate::smelt_edit::grid::Style::default(),
                pos: None,
            },
            theme,
            vim_mode: crate::smelt_edit::VimMode::Insert,
        };
        win.render(&buf, &mut slice, &ctx);

        let cells: Vec<&str> = (0..10).map(|c| grid.cell(c, 0).symbol.as_str()).collect();
        assert_eq!(cells[0], " ", "left gutter blank");
        assert_eq!(cells[9], " ", "right gutter blank");
        // Block cursor inverts the underlying glyph rather than stamping its own,
        // so the buffer grapheme at the cursor stays visible and only its style flips.
        assert_eq!(cells[1], "h", "block cursor preserves underlying glyph");
        assert_eq!(
            &cells[2..6],
            ["e", "l", "l", "o"],
            "content paints inside inner zone"
        );
    }
}
