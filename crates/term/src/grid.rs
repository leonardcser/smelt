use super::geometry::Rect;
pub use smelt_style::style::{Color, Style};

/// Convert `Color` to crossterm's `Color` at the SGR-emit boundary.
/// A free function because the orphan rule prevents a `From` impl.
pub(crate) fn to_crossterm_color(c: Color) -> crossterm::style::Color {
    use crossterm::style::Color as X;
    match c {
        Color::Reset => X::Reset,
        Color::Black => X::Black,
        Color::DarkGrey => X::DarkGrey,
        Color::Red => X::Red,
        Color::DarkRed => X::DarkRed,
        Color::Green => X::Green,
        Color::DarkGreen => X::DarkGreen,
        Color::Yellow => X::Yellow,
        Color::DarkYellow => X::DarkYellow,
        Color::Blue => X::Blue,
        Color::DarkBlue => X::DarkBlue,
        Color::Magenta => X::Magenta,
        Color::DarkMagenta => X::DarkMagenta,
        Color::Cyan => X::Cyan,
        Color::DarkCyan => X::DarkCyan,
        Color::White => X::White,
        Color::Grey => X::Grey,
        Color::Rgb { r, g, b } => X::Rgb { r, g, b },
        Color::AnsiValue(v) => X::AnsiValue(v),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cell {
    pub symbol: char,
    pub style: Style,
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            symbol: ' ',
            style: Style::default(),
        }
    }
}

pub struct Grid {
    cells: Vec<Cell>,
    width: u16,
    height: u16,
}

impl Grid {
    pub fn new(width: u16, height: u16) -> Self {
        let len = width as usize * height as usize;
        Self {
            cells: vec![Cell::default(); len],
            width,
            height,
        }
    }

    pub fn width(&self) -> u16 {
        self.width
    }

    pub fn height(&self) -> u16 {
        self.height
    }

    pub fn resize(&mut self, width: u16, height: u16) {
        self.width = width;
        self.height = height;
        self.cells
            .resize(width as usize * height as usize, Cell::default());
        self.clear_all();
    }

    pub fn cell(&self, x: u16, y: u16) -> &Cell {
        &self.cells[self.idx(x, y)]
    }

    /// Mutable cell access; returns `None` when out of bounds.
    /// Bypasses wide-char continuation bookkeeping — use only for ASCII /
    /// width-1 painting where you own the entire region.
    pub fn cell_mut(&mut self, x: u16, y: u16) -> Option<&mut Cell> {
        if x < self.width && y < self.height {
            let idx = self.idx(x, y);
            Some(&mut self.cells[idx])
        } else {
            None
        }
    }

    pub fn set(&mut self, x: u16, y: u16, symbol: char, style: Style) {
        use unicode_width::UnicodeWidthChar;
        if x < self.width && y < self.height {
            let idx = self.idx(x, y);
            self.cells[idx] = Cell { symbol, style };
            // Mark the next cell as a wide-char continuation (`\0`) so flush
            // and diff paths skip it — the terminal covers both visual columns.
            // Inherits the same style so styled backgrounds stay consistent.
            if UnicodeWidthChar::width(symbol).unwrap_or(1) == 2 && x + 1 < self.width {
                let cont = self.idx(x + 1, y);
                self.cells[cont] = Cell {
                    symbol: '\0',
                    style,
                };
            }
        }
    }

    pub fn put_str(&mut self, x: u16, y: u16, text: &str, style: Style) {
        use unicode_width::UnicodeWidthChar;

        if y >= self.height {
            return;
        }
        let mut col = x;
        for ch in text.chars() {
            let cw = UnicodeWidthChar::width(ch).unwrap_or(1).max(1) as u16;
            if col + cw > self.width {
                break;
            }
            // Delegate to `set` so wide-char continuation marking stays
            // consistent across all paths that write into the grid.
            self.set(col, y, ch, style);
            col += cw;
        }
    }

    /// Overwrites `symbol` and `style.fg`; preserves the existing cell's
    /// `bg` and text attributes. Use for fg-only painting over a filled background.
    pub fn put_char(&mut self, x: u16, y: u16, symbol: char, fg: Color) {
        use unicode_width::UnicodeWidthChar;
        if x >= self.width || y >= self.height {
            return;
        }
        let idx = self.idx(x, y);
        let mut style = self.cells[idx].style;
        style.fg = Some(fg);
        self.cells[idx] = Cell { symbol, style };
        if UnicodeWidthChar::width(symbol).unwrap_or(1) == 2 && x + 1 < self.width {
            let cont = self.idx(x + 1, y);
            self.cells[cont] = Cell {
                symbol: '\0',
                style,
            };
        }
    }

    /// String form of [`Grid::put_char`]: overwrites symbol + fg, preserves bg and attrs.
    pub fn put_str_fg(&mut self, x: u16, y: u16, text: &str, fg: Color) {
        use unicode_width::UnicodeWidthChar;
        if y >= self.height {
            return;
        }
        let mut col = x;
        for ch in text.chars() {
            let cw = UnicodeWidthChar::width(ch).unwrap_or(1).max(1) as u16;
            if col + cw > self.width {
                break;
            }
            self.put_char(col, y, ch, fg);
            col += cw;
        }
    }

    /// Paint a [`Line`] of styled spans at `(x, y)`, clipping at the right edge.
    pub fn put_line(&mut self, x: u16, y: u16, line: &crate::line::Line<'_>) {
        let mut col = x;
        for span in &line.spans {
            if col >= self.width {
                break;
            }
            let before = col;
            self.put_str(col, y, span.text.as_ref(), span.style);
            col = col.saturating_add(span.width());
            if col == before {
                break;
            }
        }
    }

    pub fn fill(&mut self, area: Rect, symbol: char, style: Style) {
        for row in area.top..area.bottom().min(self.height) {
            for col in area.left..area.right().min(self.width) {
                let idx = self.idx(col, row);
                self.cells[idx] = Cell { symbol, style };
            }
        }
    }

    pub fn clear(&mut self, area: Rect) {
        self.fill(area, ' ', Style::default());
    }

    pub fn clear_all(&mut self) {
        for cell in &mut self.cells {
            *cell = Cell::default();
        }
    }

    pub fn slice_mut(&mut self, area: Rect) -> GridSlice<'_> {
        let area = Rect::new(
            area.top.min(self.height),
            area.left.min(self.width),
            area.width.min(self.width.saturating_sub(area.left)),
            area.height.min(self.height.saturating_sub(area.top)),
        );
        GridSlice { grid: self, area }
    }

    pub fn diff<'a>(&'a self, prev: &'a Grid) -> impl Iterator<Item = CellUpdate<'a>> {
        self.cells.iter().enumerate().filter_map(move |(i, cell)| {
            // Skip wide-char continuation cells (`\0`); the preceding wide
            // glyph already covers both visual columns on the terminal.
            if cell.symbol == '\0' {
                return None;
            }
            let prev_cell = prev.cells.get(i)?;
            if cell != prev_cell {
                let x = (i % self.width as usize) as u16;
                let y = (i / self.width as usize) as u16;
                Some(CellUpdate { x, y, cell })
            } else {
                None
            }
        })
    }

    pub fn swap_with(&mut self, other: &mut Grid) {
        std::mem::swap(&mut self.cells, &mut other.cells);
        std::mem::swap(&mut self.width, &mut other.width);
        std::mem::swap(&mut self.height, &mut other.height);
    }

    fn idx(&self, x: u16, y: u16) -> usize {
        y as usize * self.width as usize + x as usize
    }
}

pub struct CellUpdate<'a> {
    pub x: u16,
    pub y: u16,
    pub cell: &'a Cell,
}

pub struct GridSlice<'a> {
    grid: &'a mut Grid,
    area: Rect,
}

impl<'a> GridSlice<'a> {
    pub fn width(&self) -> u16 {
        self.area.width
    }

    pub fn height(&self) -> u16 {
        self.area.height
    }

    pub fn area(&self) -> Rect {
        self.area
    }

    pub fn set(&mut self, x: u16, y: u16, symbol: char, style: Style) {
        if x < self.area.width && y < self.area.height {
            self.grid
                .set(self.area.left + x, self.area.top + y, symbol, style);
        }
    }

    /// Read a cell at slice-local coords; returns default `Cell` when out of bounds.
    pub fn cell(&self, x: u16, y: u16) -> Cell {
        if x < self.area.width && y < self.area.height {
            *self.grid.cell(self.area.left + x, self.area.top + y)
        } else {
            Cell::default()
        }
    }

    /// Mutable cell access at slice-local coords. See [`Grid::cell_mut`] for caveats.
    pub fn cell_mut(&mut self, x: u16, y: u16) -> Option<&mut Cell> {
        if x < self.area.width && y < self.area.height {
            self.grid.cell_mut(self.area.left + x, self.area.top + y)
        } else {
            None
        }
    }

    /// Absolute rect this slice covers in the underlying grid.
    pub fn screen_rect(&self) -> Rect {
        self.area
    }

    pub fn put_str(&mut self, x: u16, y: u16, text: &str, style: Style) {
        use unicode_width::UnicodeWidthChar;
        if y >= self.area.height {
            return;
        }
        let abs_y = self.area.top + y;
        let mut col = x;
        for ch in text.chars() {
            let cw = UnicodeWidthChar::width(ch).unwrap_or(1).max(1) as u16;
            if col + cw > self.area.width {
                break;
            }
            self.grid.set(self.area.left + col, abs_y, ch, style);
            col += cw;
        }
    }

    /// Slice-local [`Grid::put_char`]: overwrites symbol + fg, preserves bg and attrs.
    pub fn put_char(&mut self, x: u16, y: u16, symbol: char, fg: Color) {
        if x < self.area.width && y < self.area.height {
            self.grid
                .put_char(self.area.left + x, self.area.top + y, symbol, fg);
        }
    }

    /// Slice-local [`Grid::put_line`]: paint spans left-to-right, clipping at the slice edge.
    pub fn put_line(&mut self, x: u16, y: u16, line: &crate::line::Line<'_>) {
        if y >= self.area.height {
            return;
        }
        let mut col = x;
        for span in &line.spans {
            if col >= self.area.width {
                break;
            }
            let before = col;
            self.put_str(col, y, span.text.as_ref(), span.style);
            col = col.saturating_add(span.width());
            if col == before {
                break;
            }
        }
    }

    /// Slice-local [`Grid::put_str_fg`]: overwrites symbol + fg per char, preserves bg and attrs.
    pub fn put_str_fg(&mut self, x: u16, y: u16, text: &str, fg: Color) {
        use unicode_width::UnicodeWidthChar;
        if y >= self.area.height {
            return;
        }
        let abs_y = self.area.top + y;
        let mut col = x;
        for ch in text.chars() {
            let cw = UnicodeWidthChar::width(ch).unwrap_or(1).max(1) as u16;
            if col + cw > self.area.width {
                break;
            }
            self.grid.put_char(self.area.left + col, abs_y, ch, fg);
            col += cw;
        }
    }

    pub fn fill(&mut self, area: Rect, symbol: char, style: Style) {
        let abs = Rect::new(
            self.area.top + area.top,
            self.area.left + area.left,
            area.width.min(self.area.width.saturating_sub(area.left)),
            area.height.min(self.area.height.saturating_sub(area.top)),
        );
        self.grid.fill(abs, symbol, style);
    }

    pub fn clear(&mut self) {
        self.grid.clear(self.area);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_grid_filled_with_spaces() {
        let grid = Grid::new(10, 5);
        assert_eq!(grid.width(), 10);
        assert_eq!(grid.height(), 5);
        assert_eq!(grid.cell(0, 0).symbol, ' ');
        assert_eq!(grid.cell(9, 4).symbol, ' ');
    }

    #[test]
    fn set_and_read_cell() {
        let mut grid = Grid::new(10, 5);
        let style = Style::new().fg(Color::Red);
        grid.set(3, 2, 'X', style);
        assert_eq!(grid.cell(3, 2).symbol, 'X');
        assert_eq!(grid.cell(3, 2).style.fg, Some(Color::Red));
    }

    #[test]
    fn put_str_writes_chars() {
        let mut grid = Grid::new(10, 5);
        grid.put_str(2, 1, "hello", Style::default());
        assert_eq!(grid.cell(2, 1).symbol, 'h');
        assert_eq!(grid.cell(3, 1).symbol, 'e');
        assert_eq!(grid.cell(6, 1).symbol, 'o');
        assert_eq!(grid.cell(7, 1).symbol, ' ');
    }

    #[test]
    fn put_str_clips_at_width() {
        let mut grid = Grid::new(5, 1);
        grid.put_str(3, 0, "hello", Style::default());
        assert_eq!(grid.cell(3, 0).symbol, 'h');
        assert_eq!(grid.cell(4, 0).symbol, 'e');
    }

    #[test]
    fn fill_region() {
        let mut grid = Grid::new(10, 5);
        let style = Style::new().bg(Color::Blue);
        grid.fill(Rect::new(1, 2, 3, 2), '#', style);
        assert_eq!(grid.cell(2, 1).symbol, '#');
        assert_eq!(grid.cell(4, 2).symbol, '#');
        assert_eq!(grid.cell(5, 1).symbol, ' ');
    }

    #[test]
    fn diff_yields_changed_cells() {
        let prev = Grid::new(5, 3);
        let mut curr = Grid::new(5, 3);
        curr.set(1, 0, 'A', Style::default());
        curr.set(3, 2, 'B', Style::default());

        let updates: Vec<_> = curr.diff(&prev).collect();
        assert_eq!(updates.len(), 2);
        assert_eq!((updates[0].x, updates[0].y), (1, 0));
        assert_eq!((updates[1].x, updates[1].y), (3, 2));
    }

    #[test]
    fn diff_empty_for_identical_grids() {
        let a = Grid::new(5, 3);
        let b = Grid::new(5, 3);
        assert_eq!(a.diff(&b).count(), 0);
    }

    #[test]
    fn slice_writes_offset_correctly() {
        let mut grid = Grid::new(20, 10);
        let area = Rect::new(2, 5, 10, 4);
        {
            let mut slice = grid.slice_mut(area);
            assert_eq!(slice.width(), 10);
            assert_eq!(slice.height(), 4);
            slice.set(0, 0, 'A', Style::default());
            slice.put_str(1, 1, "hi", Style::default());
        }
        assert_eq!(grid.cell(5, 2).symbol, 'A');
        assert_eq!(grid.cell(6, 3).symbol, 'h');
        assert_eq!(grid.cell(7, 3).symbol, 'i');
    }

    #[test]
    fn slice_clips_to_bounds() {
        let mut grid = Grid::new(10, 5);
        let mut slice = grid.slice_mut(Rect::new(0, 0, 3, 2));
        slice.put_str(0, 0, "hello world", Style::default());
        assert_eq!(grid.cell(2, 0).symbol, 'l');
        assert_eq!(grid.cell(3, 0).symbol, ' ');
    }

    #[test]
    fn resize_clears_grid() {
        let mut grid = Grid::new(5, 3);
        grid.set(2, 1, 'A', Style::default());
        grid.resize(10, 5);
        assert_eq!(grid.width(), 10);
        assert_eq!(grid.height(), 5);
        assert_eq!(grid.cell(2, 1).symbol, ' ');
    }

    #[test]
    fn put_char_preserves_bg_and_attrs() {
        let mut grid = Grid::new(10, 3);
        let base = Style::new().fg(Color::Yellow).bg(Color::Blue).bold();
        grid.set(2, 1, '#', base);
        grid.put_char(2, 1, 'X', Color::Red);
        let cell = grid.cell(2, 1);
        assert_eq!(cell.symbol, 'X');
        assert_eq!(cell.style.fg, Some(Color::Red));
        assert_eq!(cell.style.bg, Some(Color::Blue));
        assert!(cell.style.bold);
    }

    #[test]
    fn put_char_on_empty_cell_leaves_bg_none() {
        let mut grid = Grid::new(5, 2);
        grid.put_char(0, 0, 'A', Color::Green);
        let cell = grid.cell(0, 0);
        assert_eq!(cell.symbol, 'A');
        assert_eq!(cell.style.fg, Some(Color::Green));
        assert_eq!(cell.style.bg, None);
    }

    #[test]
    fn put_line_paints_spans_with_their_styles() {
        use crate::line::{Line, Span};
        let mut grid = Grid::new(15, 1);
        let red = Style::new().fg(Color::Red);
        let line = Line::from_spans([Span::raw("ab"), Span::styled("CD", red), Span::raw("ef")]);
        grid.put_line(1, 0, &line);
        assert_eq!(grid.cell(1, 0).symbol, 'a');
        assert_eq!(grid.cell(1, 0).style.fg, None);
        assert_eq!(grid.cell(3, 0).symbol, 'C');
        assert_eq!(grid.cell(3, 0).style.fg, Some(Color::Red));
        assert_eq!(grid.cell(5, 0).symbol, 'e');
        assert_eq!(grid.cell(5, 0).style.fg, None);
    }

    #[test]
    fn slice_put_line_clips_at_right_edge() {
        use crate::line::{Line, Span};
        let mut grid = Grid::new(10, 1);
        {
            let mut slice = grid.slice_mut(Rect::new(0, 2, 5, 1));
            slice.put_line(0, 0, &Line::from_spans([Span::raw("abcdefgh")]));
        }
        assert_eq!(grid.cell(2, 0).symbol, 'a');
        assert_eq!(grid.cell(6, 0).symbol, 'e');
        assert_eq!(grid.cell(7, 0).symbol, ' ');
    }

    #[test]
    fn slice_put_str_fg_preserves_bg() {
        let mut grid = Grid::new(10, 2);
        grid.fill(Rect::new(0, 0, 6, 1), ' ', Style::new().bg(Color::Cyan));
        {
            let mut slice = grid.slice_mut(Rect::new(0, 0, 10, 1));
            slice.put_str_fg(1, 0, "hi", Color::Red);
        }
        assert_eq!(grid.cell(1, 0).symbol, 'h');
        assert_eq!(grid.cell(1, 0).style.fg, Some(Color::Red));
        assert_eq!(grid.cell(1, 0).style.bg, Some(Color::Cyan));
        assert_eq!(grid.cell(2, 0).style.bg, Some(Color::Cyan));
    }

    #[test]
    fn swap_grids() {
        let mut a = Grid::new(5, 3);
        let mut b = Grid::new(5, 3);
        a.set(0, 0, 'A', Style::default());
        b.set(0, 0, 'B', Style::default());
        a.swap_with(&mut b);
        assert_eq!(a.cell(0, 0).symbol, 'B');
        assert_eq!(b.cell(0, 0).symbol, 'A');
    }

    // ── Wide chars ───────────────────────────────────────────────────────

    #[test]
    fn set_wide_char_marks_next_cell_as_continuation() {
        // CJK chars have display width 2. The continuation cell carries
        // '\0' so flush/diff can skip it.
        let mut grid = Grid::new(5, 1);
        grid.set(1, 0, '漢', Style::default());
        assert_eq!(grid.cell(1, 0).symbol, '漢');
        assert_eq!(grid.cell(2, 0).symbol, '\0');
    }

    #[test]
    fn put_str_lays_wide_chars_two_columns_apart() {
        let mut grid = Grid::new(10, 1);
        grid.put_str(0, 0, "a漢b", Style::default());
        assert_eq!(grid.cell(0, 0).symbol, 'a');
        assert_eq!(grid.cell(1, 0).symbol, '漢');
        assert_eq!(grid.cell(3, 0).symbol, 'b');
    }

    #[test]
    fn wide_char_continuation_is_marked_consistently_across_paths() {
        // Every path that writes a wide char must mark the next cell so
        // downstream diff/flush can skip it. Otherwise a diff against a
        // prev frame with non-empty content at the continuation slot
        // emits a spurious update that overwrites the wide char's right
        // half on the terminal.
        let via_set = {
            let mut g = Grid::new(5, 1);
            g.set(0, 0, '漢', Style::default());
            g
        };
        let via_put_str = {
            let mut g = Grid::new(5, 1);
            g.put_str(0, 0, "漢", Style::default());
            g
        };
        let via_put_char = {
            let mut g = Grid::new(5, 1);
            g.put_char(0, 0, '漢', Color::Reset);
            g
        };
        assert_eq!(via_set.cell(1, 0).symbol, via_put_str.cell(1, 0).symbol);
        assert_eq!(via_set.cell(1, 0).symbol, via_put_char.cell(1, 0).symbol);
    }

    #[test]
    fn diff_does_not_emit_update_for_cell_under_a_wide_char() {
        // Regression for the wide-char bug: if prev had a real char at
        // the continuation column and curr paints a wide char that covers
        // it, diff must not yield an update for the continuation column —
        // otherwise flush_diff overwrites the right half of the wide char.
        let mut prev = Grid::new(5, 1);
        prev.set(1, 0, 'X', Style::default());
        let mut curr = Grid::new(5, 1);
        curr.put_str(0, 0, "漢", Style::default());
        let updates: Vec<_> = curr.diff(&prev).collect();
        let cols: Vec<u16> = updates.iter().map(|u| u.x).collect();
        assert_eq!(
            cols,
            vec![0],
            "expected one update at the wide char's column only; got {cols:?}"
        );
    }

    #[test]
    fn slice_put_str_lays_wide_chars_two_columns_apart() {
        let mut grid = Grid::new(10, 1);
        {
            let mut slice = grid.slice_mut(Rect::new(0, 0, 10, 1));
            slice.put_str(0, 0, "a漢b", Style::default());
        }
        assert_eq!(grid.cell(0, 0).symbol, 'a');
        assert_eq!(grid.cell(1, 0).symbol, '漢');
        assert_eq!(grid.cell(3, 0).symbol, 'b');
    }

    #[test]
    fn put_str_breaks_when_wide_char_would_overflow() {
        // 4-wide grid, write "ab漢": the wide char would land at col 2 and
        // would need col 3 too — fits. Try "abc漢" in width 4: 'c' at 2,
        // wide char needs 3+4 → overflows, breaks before writing.
        let mut grid = Grid::new(4, 1);
        grid.put_str(0, 0, "abc漢", Style::default());
        assert_eq!(grid.cell(0, 0).symbol, 'a');
        assert_eq!(grid.cell(1, 0).symbol, 'b');
        assert_eq!(grid.cell(2, 0).symbol, 'c');
        // Position 3 was never written.
        assert_eq!(grid.cell(3, 0).symbol, ' ');
    }

    // ── Diff over styles ─────────────────────────────────────────────────

    #[test]
    fn diff_picks_up_style_only_change() {
        let mut prev = Grid::new(5, 1);
        prev.set(0, 0, 'X', Style::default());
        let mut curr = Grid::new(5, 1);
        curr.set(0, 0, 'X', Style::new().fg(Color::Red));
        let updates: Vec<_> = curr.diff(&prev).collect();
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].cell.style.fg, Some(Color::Red));
    }

    // ── Bounds ───────────────────────────────────────────────────────────

    #[test]
    fn set_out_of_bounds_is_silent_noop() {
        let mut grid = Grid::new(3, 2);
        grid.set(99, 99, 'X', Style::default());
        // No panic; nothing changed.
        assert_eq!(grid.cell(0, 0).symbol, ' ');
        assert_eq!(grid.cell(2, 1).symbol, ' ');
    }

    #[test]
    fn put_str_skips_when_y_out_of_bounds() {
        let mut grid = Grid::new(5, 2);
        grid.put_str(0, 99, "hello", Style::default());
        // First row unchanged.
        assert_eq!(grid.cell(0, 0).symbol, ' ');
    }
}
