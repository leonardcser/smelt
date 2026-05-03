//! Bottom-row painter. Pulls items from Lua sources via
//! `LuaRuntime::tick_statusline` (the shipped `smelt/status.lua`
//! registers a `core` source that builds every segment), runs the
//! responsive layout (priority dropping + truncation + alignment) on
//! the resulting list, and writes the styled line into the
//! `well_known.statusline` buffer.

use crate::app::TuiApp;

impl TuiApp {
    pub(crate) fn refresh_status_bar(&mut self) {
        use crate::content::status::{spans_to_buffer_line, StatusSpan};
        use crate::ui::buffer::SpanStyle;
        use crossterm::style::Color;

        let (term_w, _) = self.ui.terminal_size();
        let width = term_w as usize;
        let status_bg = Color::AnsiValue(233);
        let theme_muted_fg = self.ui.theme().get("Comment").fg;

        let mut spans: Vec<StatusSpan> = self
            .custom_status_items
            .iter()
            .map(|item| item.to_span(status_bg))
            .collect();

        let line = spans_to_buffer_line(&mut spans, width, status_bg, theme_muted_fg);
        if let Some(buf) = self.ui.win_buf_mut(self.well_known.statusline) {
            buf.set_all_lines(vec![line.text]);
            buf.clear_highlights(0, 1);
            for span in line.spans {
                let style = SpanStyle {
                    fg: span.style.fg,
                    bg: span.style.bg,
                    bold: span.style.bold,
                    dim: span.style.dim,
                    italic: span.style.italic,
                };
                buf.add_highlight(0, span.col_start, span.col_end, style);
            }
        }
    }
}
