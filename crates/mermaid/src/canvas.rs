//! Character grid with mergeable box-drawing lines.

use crate::text::{char_width, width};

pub(crate) const UP: u8 = 1;
pub(crate) const RIGHT: u8 = 2;
pub(crate) const DOWN: u8 = 4;
pub(crate) const LEFT: u8 = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Stroke {
    Solid,
    Dotted,
    Thick,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Cell {
    Empty,
    Char(char),
    /// Right half of a double-width character.
    Wide,
    /// Line segments leaving the cell center. `dotted` and `thick` mark
    /// which of those directions use that stroke.
    Line {
        mask: u8,
        dotted: u8,
        thick: u8,
    },
}

pub(crate) struct Canvas {
    width: usize,
    cells: Vec<Vec<Cell>>,
}

impl Canvas {
    pub(crate) fn new(width: usize, height: usize) -> Self {
        Self {
            width,
            cells: vec![vec![Cell::Empty; width]; height],
        }
    }

    fn cell_mut(&mut self, x: usize, y: usize) -> Option<&mut Cell> {
        self.cells.get_mut(y).and_then(|row| row.get_mut(x))
    }

    /// Writes one character, clearing any wide character it overlaps.
    pub(crate) fn put(&mut self, x: usize, y: usize, c: char) {
        let w = char_width(c);
        if w == 0 || x + w > self.width || y >= self.cells.len() {
            return;
        }
        self.clear_wide(x, y);
        if w == 2 {
            self.clear_wide(x + 1, y);
        }
        self.cells[y][x] = Cell::Char(c);
        if w == 2 {
            self.cells[y][x + 1] = Cell::Wide;
        }
    }

    fn clear_wide(&mut self, x: usize, y: usize) {
        match self.cells[y][x] {
            Cell::Wide if x > 0 => self.cells[y][x - 1] = Cell::Empty,
            Cell::Char(c) if char_width(c) == 2 && x + 1 < self.width => {
                self.cells[y][x + 1] = Cell::Empty;
            }
            _ => {}
        }
    }

    /// Writes text left to right starting at `x`. Returns the end column.
    pub(crate) fn text(&mut self, x: usize, y: usize, s: &str) -> usize {
        let mut col = x;
        for c in s.chars() {
            self.put(col, y, c);
            col += char_width(c);
        }
        col
    }

    pub(crate) fn text_centered(&mut self, x: usize, w: usize, y: usize, s: &str) {
        let pad = w.saturating_sub(width(s)) / 2;
        self.text(x + pad, y, s);
    }

    /// Adds line directions to a cell. A box border character in the cell
    /// grows a junction toward `dirs` when one exists; other characters are
    /// left alone.
    pub(crate) fn line(&mut self, x: usize, y: usize, dirs: u8, stroke: Stroke) {
        let Some(cell) = self.cell_mut(x, y) else {
            return;
        };
        let (dotted, thick) = match stroke {
            Stroke::Solid => (0, 0),
            Stroke::Dotted => (dirs, 0),
            Stroke::Thick => (0, dirs),
        };
        match cell {
            Cell::Empty => {
                *cell = Cell::Line {
                    mask: dirs,
                    dotted,
                    thick,
                }
            }
            Cell::Line {
                mask,
                dotted: d,
                thick: t,
            } => {
                *d = (*d & !dirs) | dotted;
                *t = (*t & !dirs) | thick;
                *mask |= dirs;
            }
            Cell::Char(c) => {
                if let Some(j) = join(*c, dirs) {
                    *c = j;
                }
            }
            Cell::Wide => {}
        }
    }

    /// Draws an axis-aligned run from `a` to `b`, both inclusive, with the
    /// end cells only receiving the half that points inward.
    pub(crate) fn run(&mut self, a: (usize, usize), b: (usize, usize), stroke: Stroke) {
        let (ax, ay) = a;
        let (bx, by) = b;
        if ay == by {
            let (lo, hi) = (ax.min(bx), ax.max(bx));
            for x in lo..=hi {
                let mut dirs = 0;
                if x > lo {
                    dirs |= LEFT;
                }
                if x < hi {
                    dirs |= RIGHT;
                }
                if dirs != 0 {
                    self.line(x, ay, dirs, stroke);
                }
            }
        } else if ax == bx {
            let (lo, hi) = (ay.min(by), ay.max(by));
            for y in lo..=hi {
                let mut dirs = 0;
                if y > lo {
                    dirs |= UP;
                }
                if y < hi {
                    dirs |= DOWN;
                }
                if dirs != 0 {
                    self.line(ax, y, dirs, stroke);
                }
            }
        }
    }

    /// Draws a rectangle border with the given corner and edge glyphs.
    pub(crate) fn frame(&mut self, x: usize, y: usize, w: usize, h: usize, glyphs: &Frame) {
        if w < 2 || h < 2 {
            return;
        }
        let (r, b) = (x + w - 1, y + h - 1);
        for cx in x + 1..r {
            self.put(cx, y, glyphs.top);
            self.put(cx, b, glyphs.bottom);
        }
        for cy in y + 1..b {
            self.put(x, cy, glyphs.left);
            self.put(r, cy, glyphs.right);
        }
        self.put(x, y, glyphs.corners[0]);
        self.put(r, y, glyphs.corners[1]);
        self.put(x, b, glyphs.corners[2]);
        self.put(r, b, glyphs.corners[3]);
    }

    pub(crate) fn clear_rect(&mut self, x: usize, y: usize, w: usize, h: usize) {
        for row in self.cells.iter_mut().skip(y).take(h) {
            for cell in row.iter_mut().skip(x).take(w) {
                *cell = Cell::Empty;
            }
        }
    }

    fn glyph(&self, x: usize, y: usize) -> char {
        let Cell::Line {
            mask,
            dotted,
            thick,
        } = self.cells[y][x]
        else {
            return ' ';
        };
        line_glyph(mask, dotted, thick)
    }

    pub(crate) fn render(&self) -> String {
        let mut rows: Vec<String> = (0..self.cells.len())
            .map(|y| {
                let mut row = String::new();
                for (x, cell) in self.cells[y].iter().enumerate() {
                    match cell {
                        Cell::Empty => row.push(' '),
                        Cell::Char(c) => row.push(*c),
                        Cell::Wide => {}
                        Cell::Line { .. } => row.push(self.glyph(x, y)),
                    }
                }
                row.truncate(row.trim_end().len());
                row
            })
            .collect();
        while rows.last().is_some_and(String::is_empty) {
            rows.pop();
        }
        let lead = rows.iter().take_while(|r| r.is_empty()).count();
        rows.drain(..lead);
        rows.join("\n")
    }
}

pub(crate) struct Frame {
    pub top: char,
    pub bottom: char,
    pub left: char,
    pub right: char,
    /// Top-left, top-right, bottom-left, bottom-right.
    pub corners: [char; 4],
}

impl Frame {
    pub(crate) const fn new(h: char, v: char, corners: [char; 4]) -> Self {
        Self {
            top: h,
            bottom: h,
            left: v,
            right: v,
            corners,
        }
    }
}

pub(crate) const SQUARE: Frame = Frame::new('─', '│', ['┌', '┐', '└', '┘']);
pub(crate) const ROUNDED: Frame = Frame::new('─', '│', ['╭', '╮', '╰', '╯']);

fn line_glyph(mask: u8, dotted: u8, thick: u8) -> char {
    let vertical = mask & !(UP | DOWN) == 0;
    let horizontal = mask & !(LEFT | RIGHT) == 0;
    if vertical || horizontal {
        let all = |m: u8| m & mask == mask;
        return match (vertical, all(thick), all(dotted)) {
            (true, true, _) => '┃',
            (true, _, true) => '┆',
            (true, _, _) => '│',
            (false, true, _) => '━',
            (false, _, true) => '┄',
            (false, _, _) => '─',
        };
    }
    let heavy = thick & mask == mask;
    let solid = [
        ' ', ' ', ' ', '└', ' ', '│', '┌', '├', ' ', '┘', '─', '┴', '┐', '┤', '┬', '┼',
    ];
    let bold = [
        ' ', ' ', ' ', '┗', ' ', '┃', '┏', '┣', ' ', '┛', '━', '┻', '┓', '┫', '┳', '╋',
    ];
    if heavy {
        bold[mask as usize]
    } else {
        solid[mask as usize]
    }
}

/// Border character grown toward `dirs` by an attached line.
fn join(c: char, dirs: u8) -> Option<char> {
    let j = match (c, dirs) {
        ('─', DOWN) => '┬',
        ('─', UP) => '┴',
        ('│', RIGHT) => '├',
        ('│', LEFT) => '┤',
        ('═', DOWN) => '╤',
        ('═', UP) => '╧',
        ('║', RIGHT) => '╟',
        ('║', LEFT) => '╢',
        ('┬', UP) | ('┴', DOWN) | ('├', LEFT) | ('┤', RIGHT) => '┼',
        _ => return None,
    };
    Some(j)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_merge_into_junctions() {
        let mut c = Canvas::new(5, 3);
        c.run((0, 1), (4, 1), Stroke::Solid);
        c.run((2, 0), (2, 2), Stroke::Solid);
        c.run((4, 1), (4, 2), Stroke::Dotted);
        assert_eq!(c.render(), "  │\n──┼─┐\n  │ ┆");
    }

    #[test]
    fn border_grows_port() {
        let mut c = Canvas::new(3, 4);
        c.frame(0, 0, 3, 3, &SQUARE);
        c.run((1, 2), (1, 3), Stroke::Solid);
        assert_eq!(c.render(), "┌─┐\n│ │\n└┬┘\n │");
    }

    #[test]
    fn wide_chars_take_two_cells() {
        let mut c = Canvas::new(6, 1);
        c.text(0, 0, "请求x");
        c.put(1, 0, 'a');
        assert_eq!(c.render(), " a求x");
    }
}
