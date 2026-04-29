use super::scrollbar::Scrollbar;
use crossterm::event::{KeyCode, KeyModifiers};
use ui::buffer::Buffer;
use ui::buffer_view::BufferView;
use ui::component::{Component, CursorInfo, DrawContext, KeyResult};
use ui::grid::{GridSlice, Style};
use ui::layout::Rect;

pub(crate) struct SoftCursor {
    pub col: u16,
    pub row: u16,
    pub glyph: char,
}

pub(crate) struct WindowView {
    buffer_view: BufferView,
    viewport: Option<ui::WindowViewport>,
    cursor_info: Option<CursorInfo>,
}

impl WindowView {
    pub fn new() -> Self {
        Self {
            buffer_view: BufferView::new(),
            viewport: None,
            cursor_info: None,
        }
    }

    pub fn sync_from_buffer(&mut self, buf: &mut Buffer, scroll_offset: usize) {
        self.buffer_view.sync_from_buffer(buf);
        self.buffer_view.set_scroll(scroll_offset);
    }

    /// Layer a transient highlight on top of the synced buffer. Cleared
    /// on the next `sync_from_buffer`, so callers reapply each frame.
    /// Used for selection/search overlays.
    pub fn add_highlight(&mut self, line: usize, col_start: u16, col_end: u16, style: Style) {
        self.buffer_view
            .add_highlight(line, col_start, col_end, style);
    }

    pub fn set_viewport(&mut self, viewport: Option<ui::WindowViewport>) {
        self.viewport = viewport;
    }

    pub fn set_soft_cursor(&mut self, cursor: Option<SoftCursor>, theme: &ui::Theme) {
        self.cursor_info = cursor.map(|c| {
            let (fg, bg) = if theme.is_light() {
                (
                    crossterm::style::Color::White,
                    crossterm::style::Color::Black,
                )
            } else {
                (
                    crossterm::style::Color::Black,
                    crossterm::style::Color::White,
                )
            };
            CursorInfo::block(
                c.col,
                c.row,
                c.glyph,
                Style {
                    fg: Some(fg),
                    bg: Some(bg),
                    ..Style::default()
                },
            )
        });
    }

    fn draw_scrollbar(&self, area: Rect, grid: &mut GridSlice<'_>, theme: &ui::Theme) {
        let h = grid.height();
        let w = grid.width();
        let Some(viewport) = self.viewport else {
            return;
        };
        let Some(bar) = viewport.scrollbar else {
            return;
        };
        let local_col = bar.col.saturating_sub(area.left);
        let local_top = viewport.rect.top.saturating_sub(area.top);
        if local_col >= w || local_top >= h {
            return;
        }
        let scrollbar = Scrollbar::new(
            bar.total_rows as usize,
            bar.viewport_rows as usize,
            viewport.scroll_top as usize,
        );
        let thumb = theme.get("SmeltScrollbarThumb");
        let track = theme.get("SmeltScrollbarTrack");
        let thumb_style = Style::bg(
            thumb
                .bg
                .or(thumb.fg)
                .unwrap_or(crossterm::style::Color::Reset),
        );
        let track_style = Style::bg(
            track
                .bg
                .or(track.fg)
                .unwrap_or(crossterm::style::Color::Reset),
        );
        for row in 0..h.saturating_sub(local_top).min(bar.viewport_rows) {
            let style = if scrollbar.is_thumb(row as usize) {
                thumb_style
            } else {
                track_style
            };
            grid.set(local_col, local_top + row, ' ', style);
        }
    }
}

impl Component for WindowView {
    fn draw(&self, area: Rect, grid: &mut GridSlice<'_>, ctx: &DrawContext) {
        let h = grid.height();
        let w = grid.width();
        if h == 0 || w == 0 {
            return;
        }

        self.buffer_view.draw(area, grid, ctx);
        self.draw_scrollbar(area, grid, &ctx.theme);
    }

    fn handle_key(&mut self, _code: KeyCode, _mods: KeyModifiers) -> KeyResult {
        KeyResult::Ignored
    }

    fn cursor(&self) -> Option<CursorInfo> {
        self.cursor_info.clone()
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::style::Color;
    use ui::buffer::{BufCreateOpts, SpanStyle};
    use ui::grid::Grid;
    use ui::BufId;

    fn make_buf(lines: &[&str]) -> Buffer {
        let mut buf = Buffer::new(BufId(1), BufCreateOpts::default());
        buf.set_all_lines(lines.iter().map(|s| String::from(*s)).collect());
        buf
    }

    fn ctx(w: u16, h: u16) -> DrawContext {
        DrawContext {
            terminal_width: w,
            terminal_height: h,
            focused: true,
            theme: Default::default(),
        }
    }

    #[test]
    fn renders_buffer_lines() {
        let mut buf = make_buf(&["hello", "world"]);
        let mut view = WindowView::new();
        view.sync_from_buffer(&mut buf, 0);

        let mut grid = Grid::new(20, 5);
        let mut slice = grid.slice_mut(Rect::new(0, 0, 20, 5));
        view.draw(Rect::new(0, 0, 20, 5), &mut slice, &ctx(20, 5));

        assert_eq!(grid.cell(0, 0).symbol, 'h');
        assert_eq!(grid.cell(0, 1).symbol, 'w');
        assert_eq!(grid.cell(0, 2).symbol, ' ');
    }

    #[test]
    fn renders_with_scroll_offset() {
        let mut buf = make_buf(&["line0", "line1", "line2", "line3"]);
        let mut view = WindowView::new();
        view.sync_from_buffer(&mut buf, 2);

        let mut grid = Grid::new(20, 2);
        let mut slice = grid.slice_mut(Rect::new(0, 0, 20, 2));
        view.draw(Rect::new(0, 0, 20, 2), &mut slice, &ctx(20, 2));

        assert_eq!(grid.cell(4, 0).symbol, '2');
        assert_eq!(grid.cell(4, 1).symbol, '3');
    }

    #[test]
    fn renders_highlighted_buffer() {
        let mut buf = Buffer::new(BufId(1), BufCreateOpts::default());
        buf.set_all_lines(vec!["colored text".into()]);
        buf.add_highlight(0, 0, 7, SpanStyle::fg(Color::Red));

        let mut view = WindowView::new();
        view.sync_from_buffer(&mut buf, 0);

        let mut grid = Grid::new(20, 1);
        let mut slice = grid.slice_mut(Rect::new(0, 0, 20, 1));
        view.draw(Rect::new(0, 0, 20, 1), &mut slice, &ctx(20, 1));

        assert_eq!(grid.cell(0, 0).style.fg, Some(Color::Red));
        assert_eq!(grid.cell(7, 0).style.fg, None);
    }

    #[test]
    fn cursor_info_from_soft_cursor() {
        let mut view = WindowView::new();
        view.set_soft_cursor(
            Some(SoftCursor {
                col: 1,
                row: 0,
                glyph: 'b',
            }),
            &ui::Theme::new(),
        );

        let ci = view.cursor().unwrap();
        assert_eq!((ci.col, ci.row), (1, 0));
        let cs = ci.style.unwrap();
        assert_eq!(cs.glyph, 'b');
        assert!(cs.style.fg.is_some());
        assert!(cs.style.bg.is_some());
    }
}
