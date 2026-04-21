use crossterm::event::{KeyCode, KeyModifiers};
use ui::component::{Component, CursorInfo, DrawContext, KeyResult};
use ui::grid::{GridSlice, Style};
use ui::layout::Rect;

pub(crate) struct StyledSegment {
    pub text: String,
    pub style: Style,
}

pub(crate) struct PromptRow {
    pub segments: Vec<StyledSegment>,
    pub fill: Option<Style>,
    /// Optional scrollbar indicator at a specific column.
    pub scrollbar: Option<(u16, Style)>,
}

impl PromptRow {
    pub fn styled(segments: Vec<StyledSegment>) -> Self {
        Self {
            segments,
            fill: None,
            scrollbar: None,
        }
    }

    pub fn styled_with_scrollbar(segments: Vec<StyledSegment>, col: u16, style: Style) -> Self {
        Self {
            segments,
            fill: None,
            scrollbar: Some((col, style)),
        }
    }
}

pub(crate) struct PromptView {
    rows: Vec<PromptRow>,
    cursor_info: Option<CursorInfo>,
}

impl PromptView {
    pub fn new() -> Self {
        Self {
            rows: Vec::new(),
            cursor_info: None,
        }
    }

    pub fn set_rows(&mut self, rows: Vec<PromptRow>) {
        self.rows = rows;
    }

    pub fn set_cursor(&mut self, pos: Option<(u16, u16)>, style: Option<(Style, char)>) {
        self.cursor_info = pos.map(|(cx, cy)| {
            if let Some((s, glyph)) = style {
                CursorInfo::block(cx, cy, glyph, s)
            } else {
                CursorInfo::hardware(cx, cy)
            }
        });
    }
}

impl Component for PromptView {
    fn draw(&self, _area: Rect, grid: &mut GridSlice<'_>, _ctx: &DrawContext) {
        let h = grid.height();
        let w = grid.width();
        if h == 0 || w == 0 {
            return;
        }

        for (row_idx, row) in self.rows.iter().enumerate() {
            if row_idx as u16 >= h {
                break;
            }
            let y = row_idx as u16;

            if let Some(fill) = row.fill {
                grid.fill(Rect::new(0, y, w, 1), ' ', fill);
            }

            let mut col: u16 = 0;
            for seg in &row.segments {
                for ch in seg.text.chars() {
                    if col >= w {
                        break;
                    }
                    let style = if let Some(fill) = row.fill {
                        Style {
                            fg: seg.style.fg.or(fill.fg),
                            bg: seg.style.bg.or(fill.bg),
                            bold: seg.style.bold,
                            dim: seg.style.dim,
                            italic: seg.style.italic,
                            underline: seg.style.underline,
                            crossedout: seg.style.crossedout,
                        }
                    } else {
                        seg.style
                    };
                    grid.set(col, y, ch, style);
                    col += 1;
                }
            }

            if let Some((sb_col, sb_style)) = row.scrollbar {
                if sb_col < w {
                    grid.set(sb_col, y, ' ', sb_style);
                }
            }
        }
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
    use ui::grid::Grid;

    fn plain(text: &str) -> PromptRow {
        PromptRow::styled(vec![StyledSegment {
            text: text.to_string(),
            style: Style::default(),
        }])
    }

    #[test]
    fn renders_plain_rows() {
        let mut view = PromptView::new();
        view.set_rows(vec![plain("hello"), plain("world")]);

        let mut grid = Grid::new(20, 5);
        let ctx = DrawContext {
            terminal_width: 20,
            terminal_height: 5,
            focused: true,
        };
        let mut slice = grid.slice_mut(Rect::new(0, 0, 20, 5));
        view.draw(Rect::new(0, 0, 20, 5), &mut slice, &ctx);

        assert_eq!(grid.cell(0, 0).symbol, 'h');
        assert_eq!(grid.cell(0, 1).symbol, 'w');
    }

    #[test]
    fn cursor_position_reported() {
        let mut view = PromptView::new();
        view.set_cursor(Some((5, 2)), None);
        let ci = view.cursor().unwrap();
        assert_eq!((ci.col, ci.row), (5, 2));
        assert!(ci.style.is_none());
    }

    #[test]
    fn cursor_block_style_reported() {
        let cursor_style = Style {
            fg: Some(Color::Black),
            bg: Some(Color::White),
            ..Style::default()
        };
        let mut view = PromptView::new();
        view.set_cursor(Some((1, 0)), Some((cursor_style, 'b')));
        let ci = view.cursor().unwrap();
        assert_eq!((ci.col, ci.row), (1, 0));
        let cs = ci.style.unwrap();
        assert_eq!(cs.glyph, 'b');
        assert_eq!(cs.style.fg, Some(Color::Black));
        assert_eq!(cs.style.bg, Some(Color::White));
    }
}
