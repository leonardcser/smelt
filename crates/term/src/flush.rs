use super::grid::{char_width, to_crossterm_color, CellUpdate, Style};
use crossterm::style::{
    Attribute, Color, ResetColor, SetAttribute, SetBackgroundColor, SetForegroundColor,
};
use crossterm::{cursor, QueueableCommand};
use std::io::Write;

pub fn flush_diff<'a, W: Write>(
    w: &mut W,
    updates: impl Iterator<Item = CellUpdate<'a>>,
) -> std::io::Result<()> {
    let mut current = Style::default();
    let mut cursor_x: u16 = u16::MAX;
    let mut cursor_y: u16 = u16::MAX;

    for update in updates {
        if update.y != cursor_y || update.x != cursor_x {
            w.queue(cursor::MoveTo(update.x, update.y))?;
        }
        if update.cell.style != current {
            emit_style_diff(w, &current, &update.cell.style)?;
            current = update.cell.style;
        }
        let printable = printable_symbol(update.cell.symbol);
        let overflow = update.x.saturating_add(char_width(printable)) > update.row_width;
        let symbol = if overflow { ' ' } else { printable };
        let mut buf = [0u8; 4];
        let s = symbol.encode_utf8(&mut buf);
        w.write_all(s.as_bytes())?;
        cursor_x = update.x.saturating_add(char_width(symbol));
        cursor_y = update.y;
    }

    if cursor_x != u16::MAX {
        w.queue(SetAttribute(Attribute::Reset))?;
        w.queue(ResetColor)?;
    }

    Ok(())
}

fn printable_symbol(symbol: char) -> char {
    if symbol.is_control() {
        ' '
    } else {
        symbol
    }
}

fn emit_style_diff<W: Write>(w: &mut W, from: &Style, to: &Style) -> std::io::Result<()> {
    let need_unbold = from.bold && !to.bold;
    let need_undim = from.dim && !to.dim;
    let need_unitalic = from.italic && !to.italic;
    let need_uncrossed = from.crossedout && !to.crossedout;
    let need_unreverse = from.reverse && !to.reverse;
    let need_ununderline = from.underline && !to.underline;

    let unsets = need_unbold as u8
        + need_undim as u8
        + need_unitalic as u8
        + need_uncrossed as u8
        + need_unreverse as u8
        + need_ununderline as u8;
    let intensity_conflict = (need_unbold && to.dim) || (need_undim && to.bold);

    if unsets >= 2 || intensity_conflict {
        w.queue(SetAttribute(Attribute::Reset))?;
        w.queue(ResetColor)?;

        if let Some(fg) = to.fg {
            w.queue(SetForegroundColor(to_crossterm_color(fg)))?;
        }
        if let Some(bg) = to.bg {
            w.queue(SetBackgroundColor(to_crossterm_color(bg)))?;
        }
        if to.bold {
            w.queue(SetAttribute(Attribute::Bold))?;
        }
        if to.dim {
            w.queue(SetAttribute(Attribute::Dim))?;
        }
        if to.italic {
            w.queue(SetAttribute(Attribute::Italic))?;
        }
        if to.crossedout {
            w.queue(SetAttribute(Attribute::CrossedOut))?;
        }
        if to.reverse {
            w.queue(SetAttribute(Attribute::Reverse))?;
        }
        if to.underline {
            w.queue(SetAttribute(Attribute::Underlined))?;
        }
        return Ok(());
    }

    if need_unbold || need_undim {
        w.queue(SetAttribute(Attribute::NormalIntensity))?;
        if need_unbold && to.dim {
            w.queue(SetAttribute(Attribute::Dim))?;
        }
        if need_undim && to.bold {
            w.queue(SetAttribute(Attribute::Bold))?;
        }
    }
    if need_unitalic {
        w.queue(SetAttribute(Attribute::NoItalic))?;
    }
    if need_uncrossed {
        w.queue(SetAttribute(Attribute::NotCrossedOut))?;
    }
    if need_unreverse {
        w.queue(SetAttribute(Attribute::NoReverse))?;
    }
    if need_ununderline {
        w.queue(SetAttribute(Attribute::NoUnderline))?;
    }

    if !from.bold && to.bold {
        w.queue(SetAttribute(Attribute::Bold))?;
    }
    if !from.dim && to.dim {
        w.queue(SetAttribute(Attribute::Dim))?;
    }
    if !from.italic && to.italic {
        w.queue(SetAttribute(Attribute::Italic))?;
    }
    if !from.crossedout && to.crossedout {
        w.queue(SetAttribute(Attribute::CrossedOut))?;
    }
    if !from.reverse && to.reverse {
        w.queue(SetAttribute(Attribute::Reverse))?;
    }
    if !from.underline && to.underline {
        w.queue(SetAttribute(Attribute::Underlined))?;
    }

    if from.fg != to.fg {
        if let Some(fg) = to.fg {
            w.queue(SetForegroundColor(to_crossterm_color(fg)))?;
        } else {
            w.queue(SetForegroundColor(Color::Reset))?;
        }
    }
    if from.bg != to.bg {
        if let Some(bg) = to.bg {
            w.queue(SetBackgroundColor(to_crossterm_color(bg)))?;
        } else {
            w.queue(SetBackgroundColor(Color::Reset))?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grid::Grid;
    use smelt_style::style::Color;

    #[test]
    fn flush_empty_diff_produces_no_output() {
        let a = Grid::new(5, 3);
        let b = Grid::new(5, 3);
        let mut out = Vec::new();
        flush_diff(&mut out, a.diff(&b)).unwrap();
        assert!(out.is_empty());
    }

    /// Run `flush_diff` over the diff between `curr` and `prev` and return the bytes
    /// crossterm wrote. Tests assert on the exact byte sequence to pin SGR + cursor codes.
    fn flush_to_string(curr: &Grid, prev: &Grid) -> String {
        crossterm::style::force_color_output(true);
        let mut out = Vec::new();
        flush_diff(&mut out, curr.diff(prev)).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn flush_single_cell_moves_cursor_and_writes_char() {
        let prev = Grid::new(5, 3);
        let mut curr = Grid::new(5, 3);
        curr.set(2, 1, 'X', Style::default());
        let s = flush_to_string(&curr, &prev);
        // MoveTo(2, 1) emits "\x1b[2;3H" (1-based, row-first).
        assert!(
            s.starts_with("\x1b[2;3H"),
            "expected MoveTo before char, got {s:?}"
        );
        // Char follows; final reset closes the run.
        assert!(s.contains("\x1b[2;3HX"));
        assert!(s.ends_with("\x1b[0m"));
    }

    #[test]
    fn flush_styled_cell_emits_an_sgr_before_the_char() {
        let prev = Grid::new(5, 1);
        let mut curr = Grid::new(5, 1);
        curr.set(0, 0, 'A', Style::new().fg(Color::Red));
        let s = flush_to_string(&curr, &prev);
        // We don't pin the exact bytes (crossterm picks the encoding for
        // named colors). The contract: a styled cell emits *some* SGR
        // sequence before the char, distinct from the default empty.
        let a_pos = s.find('A').expect("A in output");
        let before_a = &s[..a_pos];
        assert!(
            before_a.contains("\x1b[") && before_a.matches("\x1b[").count() >= 2,
            "expected at least one SGR escape before A (after the MoveTo), got {before_a:?}"
        );
    }

    #[test]
    fn flush_emits_different_bytes_for_different_fg_colors() {
        let render = |c: Color| {
            let prev = Grid::new(3, 1);
            let mut curr = Grid::new(3, 1);
            curr.set(0, 0, 'A', Style::new().fg(c));
            flush_to_string(&curr, &prev)
        };
        // Distinct named colors must not collapse to the same SGR.
        assert_ne!(render(Color::Red), render(Color::Blue));
        assert_ne!(render(Color::Green), render(Color::Yellow));
    }

    #[test]
    fn flush_resets_style_at_end() {
        let prev = Grid::new(3, 1);
        let mut curr = Grid::new(3, 1);
        curr.set(0, 0, 'A', Style::new().bold());
        let mut out = Vec::new();
        flush_diff(&mut out, curr.diff(&prev)).unwrap();
        let s = String::from_utf8(out).unwrap();
        assert!(s.ends_with("\x1b[0m"));
    }

    #[test]
    fn flush_never_emits_cell_control_characters() {
        let prev = Grid::new(4, 1);
        let mut curr = Grid::new(4, 1);
        curr.set(0, 0, 'A', Style::default());
        curr.set(1, 0, '\r', Style::default());
        curr.set(2, 0, '\x1b', Style::default());
        curr.set(3, 0, 'B', Style::default());

        let s = flush_to_string(&curr, &prev);
        assert!(
            !s.contains('\r'),
            "carriage return leaked into flush bytes: {s:?}"
        );
        assert!(
            s.contains("A  B"),
            "control cells should render as spaces: {s:?}"
        );
    }

    // ── Color encodings ───────────────────────────────────────────────────

    #[test]
    fn flush_emits_ansi_palette_color_code() {
        let prev = Grid::new(3, 1);
        let mut curr = Grid::new(3, 1);
        curr.set(0, 0, 'A', Style::new().fg(Color::AnsiValue(208)));
        let s = flush_to_string(&curr, &prev);
        // Crossterm encodes ANSI palette fg as "\x1b[38;5;<n>m".
        assert!(s.contains("\x1b[38;5;208m"), "got {s:?}");
    }

    #[test]
    fn flush_emits_rgb_color_code() {
        let prev = Grid::new(3, 1);
        let mut curr = Grid::new(3, 1);
        curr.set(
            0,
            0,
            'A',
            Style::new().fg(Color::Rgb {
                r: 10,
                g: 20,
                b: 30,
            }),
        );
        let s = flush_to_string(&curr, &prev);
        // Crossterm encodes truecolor fg as "\x1b[38;2;r;g;bm".
        assert!(s.contains("\x1b[38;2;10;20;30m"), "got {s:?}");
    }

    #[test]
    fn flush_emits_distinct_sgr_for_fg_vs_bg() {
        // A cell that only sets bg must produce different bytes than one
        // that only sets fg with the same color - fg/bg can't collapse.
        let render = |s: Style| {
            let prev = Grid::new(3, 1);
            let mut curr = Grid::new(3, 1);
            curr.set(0, 0, 'A', s);
            flush_to_string(&curr, &prev)
        };
        let fg_only = render(Style::new().fg(Color::Blue));
        let bg_only = render(Style::new().bg(Color::Blue));
        assert_ne!(fg_only, bg_only);
    }

    // ── Text attributes ──────────────────────────────────────────────────

    #[test]
    fn flush_emits_bold_attribute() {
        let prev = Grid::new(3, 1);
        let mut curr = Grid::new(3, 1);
        curr.set(0, 0, 'A', Style::new().bold());
        let s = flush_to_string(&curr, &prev);
        assert!(s.contains("\x1b[1m"), "got {s:?}");
    }

    #[test]
    fn flush_emits_dim_attribute() {
        let prev = Grid::new(3, 1);
        let mut curr = Grid::new(3, 1);
        curr.set(0, 0, 'A', Style::new().dim());
        let s = flush_to_string(&curr, &prev);
        assert!(s.contains("\x1b[2m"), "got {s:?}");
    }

    #[test]
    fn flush_emits_italic_attribute() {
        let prev = Grid::new(3, 1);
        let mut curr = Grid::new(3, 1);
        curr.set(0, 0, 'A', Style::new().italic());
        let s = flush_to_string(&curr, &prev);
        assert!(s.contains("\x1b[3m"), "got {s:?}");
    }

    #[test]
    fn flush_emits_underline_attribute() {
        let prev = Grid::new(3, 1);
        let mut curr = Grid::new(3, 1);
        curr.set(0, 0, 'A', Style::new().underline());
        let s = flush_to_string(&curr, &prev);
        assert!(s.contains("\x1b[4m"), "got {s:?}");
    }

    #[test]
    fn flush_emits_crossedout_attribute() {
        let prev = Grid::new(3, 1);
        let mut curr = Grid::new(3, 1);
        curr.set(0, 0, 'A', Style::new().crossedout());
        let s = flush_to_string(&curr, &prev);
        assert!(s.contains("\x1b[9m"), "got {s:?}");
    }

    #[test]
    fn flush_emits_reverse_attribute() {
        let prev = Grid::new(3, 1);
        let mut curr = Grid::new(3, 1);
        curr.set(0, 0, 'A', Style::new().reverse());
        let s = flush_to_string(&curr, &prev);
        assert!(s.contains("\x1b[7m"), "got {s:?}");
    }

    // ── Transitions & cursor ─────────────────────────────────────────────

    #[test]
    fn flush_re_emits_sgr_when_style_changes_mid_run() {
        // Two adjacent cells with different styles must not share one SGR
        // run - there must be an escape between them.
        let prev = Grid::new(3, 1);
        let mut curr = Grid::new(3, 1);
        curr.set(0, 0, 'A', Style::default());
        curr.set(1, 0, 'B', Style::new().fg(Color::Red));
        let s = flush_to_string(&curr, &prev);
        let a_pos = s.find('A').expect("A in output");
        let b_pos = s.find('B').expect("B in output");
        let between = &s[a_pos + 1..b_pos];
        assert!(
            between.contains("\x1b["),
            "expected an SGR escape between unstyled A and styled B, got {between:?}"
        );
    }

    #[test]
    fn flush_noncontiguous_cells_emit_moveto_between() {
        let prev = Grid::new(10, 2);
        let mut curr = Grid::new(10, 2);
        curr.set(0, 0, 'A', Style::default());
        curr.set(5, 1, 'B', Style::default());
        let s = flush_to_string(&curr, &prev);
        // 'A' at (0,0) → MoveTo(0,0) = "\x1b[1;1H". Then 'B' at (5,1) →
        // MoveTo(5,1) = "\x1b[2;6H".
        assert!(s.contains("\x1b[1;1HA"), "got {s:?}");
        assert!(s.contains("\x1b[2;6HB"), "got {s:?}");
    }

    #[test]
    fn flush_contiguous_cells_share_one_moveto() {
        let prev = Grid::new(10, 1);
        let mut curr = Grid::new(10, 1);
        curr.set(0, 0, 'A', Style::default());
        curr.set(1, 0, 'B', Style::default());
        curr.set(2, 0, 'C', Style::default());
        let s = flush_to_string(&curr, &prev);
        // Only one MoveTo at the start; the rest stream after.
        assert!(s.contains("\x1b[1;1HABC"), "got {s:?}");
    }

    #[test]
    fn flush_tracks_cursor_after_wide_glyphs() {
        let prev = Grid::new(10, 1);
        let mut curr = Grid::new(10, 1);
        curr.set(0, 0, '漢', Style::default());
        curr.set(2, 0, 'A', Style::default());
        let s = flush_to_string(&curr, &prev);

        assert!(
            s.contains("\x1b[1;1H漢A"),
            "wide glyph and following cell should stream in one run, got {s:?}"
        );
    }

    #[test]
    fn flush_streams_private_use_icons_as_narrow_glyphs() {
        let prev = Grid::new(10, 1);
        let mut curr = Grid::new(10, 1);
        curr.set(0, 0, '\u{e6b2}', Style::default());
        curr.set(1, 0, 'A', Style::default());
        let s = flush_to_string(&curr, &prev);

        assert!(
            s.contains("\x1b[1;1H\u{e6b2}A"),
            "private-use icon and following cell should stream as narrow glyphs, got {s:?}"
        );
    }

    #[test]
    fn flush_does_not_emit_wide_glyph_at_right_edge() {
        let prev = Grid::new(2, 1);
        let mut curr = Grid::new(2, 1);
        let cell = curr.cell_mut(1, 0).expect("right edge cell");
        cell.symbol = '漢';
        cell.style = Style::default();

        let s = flush_to_string(&curr, &prev);
        assert!(
            !s.contains('漢'),
            "right-edge wide glyph would force terminal wrap: {s:?}"
        );
        assert!(
            s.contains("\x1b[1;2H "),
            "expected a clearing space, got {s:?}"
        );
    }

    #[test]
    fn flush_clears_wide_glyph_continuation() {
        let mut prev = Grid::new(10, 1);
        prev.set(0, 0, '漢', Style::default());
        prev.set(2, 0, 'A', Style::default());
        let curr = Grid::new(10, 1);
        let s = flush_to_string(&curr, &prev);

        assert!(
            s.contains("\x1b[1;1H   "),
            "clearing a wide glyph must rewrite its continuation cell, got {s:?}"
        );
    }
}
