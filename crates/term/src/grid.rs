use super::geometry::Rect;
pub use smelt_style::style::{Color, Style};

/// Convert the frontend-neutral `Color` to crossterm's terminal
/// `Color` at the SGR-emit boundary. 1:1 enum mapping; the trait
/// orphan rule prevents `From<smelt_style::Color> for crossterm::Color`,
/// so this is a free function. Internal — callers should write `Style`
/// values into the grid and let the compositor handle the SGR
/// translation.
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

    /// Direct mutable cell access for hot paint paths. Returns `None`
    /// when `(x, y)` is out of bounds. Bypasses the wide-char
    /// continuation marker bookkeeping that `set` does — only use this
    /// for ASCII / width-1 painting where you control the entire grid
    /// region (e.g. half-block colour fills).
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
            // Wide-char invariant: the cell immediately after a width-2
            // glyph holds a `\0` continuation marker so the flush and
            // diff paths can skip it (the terminal renders the wide
            // glyph across both visual cells on its own). Marker
            // inherits the wide char's style so inclusion in a styled
            // pill stays visually consistent.
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
            let idx = self.idx(col, y);
            self.cells[idx] = Cell { symbol: ch, style };
            col += cw;
        }
    }

    /// Partial cell update — overwrites `symbol` and `style.fg`, but
    /// preserves the existing cell's `bg` and text attributes (bold,
    /// italic, etc.). Use when painting fg-only content over a row that
    /// already carries a background fill (treemap tile labels, cursor-
    /// line text, inline diff markers).
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

    /// String form of [`Grid::put_char`]. Each character preserves the
    /// underlying bg / attrs at its target cell; only `symbol` and
    /// `style.fg` are overwritten.
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

    /// Paint a [`Line`] of styled spans starting at `(x, y)`. Each
    /// span paints with its own style; spans run left-to-right with
    /// no implicit gap. Clips at the right edge.
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
            // Wide-char continuation cells (`\0` sentinel) are never
            // flushed — the preceding wide glyph paints both visual
            // columns. Emitting them would either overwrite the
            // continuation (clobbering the glyph) or desync the
            // terminal cursor.
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

    /// Read a cell from the underlying grid at slice-local coords.
    /// Returns the default `Cell` when out of bounds.
    pub fn cell(&self, x: u16, y: u16) -> Cell {
        if x < self.area.width && y < self.area.height {
            *self.grid.cell(self.area.left + x, self.area.top + y)
        } else {
            Cell::default()
        }
    }

    /// Mutable cell access at slice-local coords. Mirrors
    /// [`Grid::cell_mut`] — bypasses wide-char continuation
    /// bookkeeping, intended for hot ASCII / half-block paint paths.
    pub fn cell_mut(&mut self, x: u16, y: u16) -> Option<&mut Cell> {
        if x < self.area.width && y < self.area.height {
            self.grid.cell_mut(self.area.left + x, self.area.top + y)
        } else {
            None
        }
    }

    /// Absolute rect this slice covers in the underlying grid. Useful
    /// for hosts that want to record screen-coordinate hit regions
    /// from inside a paint callback.
    pub fn screen_rect(&self) -> Rect {
        self.area
    }

    pub fn put_str(&mut self, x: u16, y: u16, text: &str, style: Style) {
        if y >= self.area.height {
            return;
        }
        let abs_y = self.area.top + y;
        for (col, ch) in (x..).zip(text.chars()) {
            if col >= self.area.width {
                break;
            }
            self.grid.set(self.area.left + col, abs_y, ch, style);
        }
    }

    /// Partial cell update at slice-local coords. See [`Grid::put_char`]
    /// for semantics: overwrites symbol + fg, preserves existing bg
    /// and attrs.
    pub fn put_char(&mut self, x: u16, y: u16, symbol: char, fg: Color) {
        if x < self.area.width && y < self.area.height {
            self.grid
                .put_char(self.area.left + x, self.area.top + y, symbol, fg);
        }
    }

    /// Paint a styled [`Line`] at slice-local coords. See
    /// [`Grid::put_line`] for semantics — spans paint left-to-right
    /// with their own styles, clipping at the slice's right edge.
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

    /// String form of [`GridSlice::put_char`].
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
}
