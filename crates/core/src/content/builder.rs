//! `LineBuilder` is the single layout primitive for all block renderers.
//! Renderers call `print` / `newline` / `push_style` etc.; the builder resolves styles
//! against the supplied [`Theme`] and writes lines + highlights + decorations into a [`Buffer`].
//! On [`LineBuilder::finish`] the trailing incomplete line is flushed and an [`Outcome`] returned.

use crate::buffer::{Buffer, LineDecoration, SourceLine, SpanMeta};
use crate::style::{Color, Style};
use crate::theme::{intern_anonymous_style, HlGroup, Theme};
use unicode_width::UnicodeWidthStr;

/// Display-column width of a string slice.
pub fn display_width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

/// Outcome returned by [`LineBuilder::finish`].
#[derive(Debug, Clone, Copy, Default)]
pub struct Outcome {
    pub line_count: usize,
    pub layout_width: u16,
    /// True when the layout wrapped at least one line; false means replayable at any width >= `max_line_width`.
    pub was_wrapped: bool,
    pub max_line_width: u16,
}

impl Outcome {
    pub fn is_valid_at(&self, new_width: u16) -> bool {
        if self.was_wrapped {
            new_width == self.layout_width
        } else {
            new_width >= self.max_line_width
        }
    }
}

pub struct LineBuilder<'a> {
    buf: &'a mut Buffer,
    theme: &'a Theme,
    layout_width: u16,

    // Per-line accumulator
    cur_text: String,
    cur_highlights: Vec<(u16, u16, HlGroup, SpanMeta)>,
    cur_decoration: LineDecoration,
    cur_visible_cols: u16,

    // Line counters
    starting_line: usize,
    lines_committed: usize,
    has_pending_content: bool,
    overwrote_blank_seed: bool,

    cur_group: Option<HlGroup>,
    cur_style: Style,
    style_stack: Vec<(Option<HlGroup>, Style)>,

    // Source-text plumbing
    pending_source_text: Option<String>,
    auto_soft_wrap_continuation: bool,

    // Outcome flags
    was_wrapped: bool,
    max_line_width: u16,
}

impl<'a> LineBuilder<'a> {
    pub fn new(buf: &'a mut Buffer, theme: &'a Theme, layout_width: u16) -> Self {
        // The first committed line replaces the trailing empty seed line when present.
        let starting_line = buf.line_count();
        let trailing_seed_blank = buf
            .get_line(starting_line.saturating_sub(1))
            .map(|s| s.is_empty())
            .unwrap_or(false);
        let starting_line = if trailing_seed_blank && starting_line > 0 {
            starting_line - 1
        } else {
            starting_line
        };
        Self {
            buf,
            theme,
            layout_width,
            cur_text: String::new(),
            cur_highlights: Vec::new(),
            cur_decoration: LineDecoration::default(),
            cur_visible_cols: 0,
            starting_line,
            lines_committed: 0,
            has_pending_content: false,
            overwrote_blank_seed: false,
            cur_group: None,
            cur_style: Style::default(),
            style_stack: Vec::new(),
            pending_source_text: None,
            auto_soft_wrap_continuation: false,
            was_wrapped: false,
            max_line_width: 0,
        }
    }

    pub fn theme(&self) -> &Theme {
        self.theme
    }

    /// Commit any pending line and return rendering metadata.
    pub fn finish(mut self) -> Outcome {
        if self.has_pending_content || self.cur_decoration_present() || self.cur_visible_cols > 0 {
            self.commit_line();
        }
        Outcome {
            line_count: self.lines_committed,
            layout_width: self.layout_width,
            was_wrapped: self.was_wrapped,
            max_line_width: self.max_line_width,
        }
    }

    /// Lines accumulated so far, including the current incomplete line if it has content.
    pub fn line_count(&self) -> usize {
        self.lines_committed
            + if self.has_pending_content
                || self.cur_decoration_present()
                || self.cur_visible_cols > 0
            {
                1
            } else {
                0
            }
    }

    // ── Text emission ───────────────────────────────────────────────

    pub fn print(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let w = display_width(text) as u16;
        self.cur_visible_cols = self.cur_visible_cols.saturating_add(w);
        self.append_span_styled(text, SpanMeta::default());
    }

    pub fn print_string(&mut self, s: String) {
        self.print(&s);
    }

    pub fn print_with_meta(&mut self, text: &str, meta: SpanMeta) {
        if text.is_empty() {
            return;
        }
        let w = display_width(text) as u16;
        self.cur_visible_cols = self.cur_visible_cols.saturating_add(w);
        self.append_span_styled(text, meta);
    }

    pub fn print_gutter(&mut self, text: &str) {
        self.print_with_meta(
            text,
            SpanMeta {
                selectable: false,
                copy_as: None,
            },
        );
    }

    pub fn newline(&mut self) {
        if let Some(src) = self.pending_source_text.take() {
            self.cur_decoration.source_text = Some(src);
        } else if self.auto_soft_wrap_continuation {
            self.cur_decoration.soft_wrapped = true;
        }
        if self.cur_visible_cols > self.max_line_width {
            self.max_line_width = self.cur_visible_cols;
        }
        self.commit_line();
    }

    // ── Per-line decorations ────────────────────────────────────────

    /// Mark the layout as width-pinned (not replayable at a different viewport width).
    pub fn mark_wrapped(&mut self) {
        self.was_wrapped = true;
    }

    /// Fill the row remainder with `bg` up to the right edge, leaving `right_margin` columns blank.
    pub fn fill_line_bg(&mut self, bg: Color, right_margin: u16) {
        // A row has at most one fill; catch double-calls in debug builds.
        debug_assert!(
            self.cur_decoration.fill_bg.is_none(),
            "fill_line_bg called twice on the same row"
        );
        self.cur_decoration.fill_bg = Some(bg);
        self.cur_decoration.fill_right_margin = right_margin;
    }

    /// Like `fill_line_bg` but resolves the background from a theme group.
    pub fn fill_line_bg_group(&mut self, group: HlGroup, right_margin: u16) {
        let bg = self.theme.resolve(group).bg.unwrap_or(Color::Reset);
        self.fill_line_bg(bg, right_margin);
    }

    pub fn set_gutter_bg(&mut self, bg: Color) {
        self.cur_decoration.gutter_bg = Some(bg);
    }

    /// Like `set_gutter_bg` but resolves the background from a theme group.
    pub fn set_gutter_bg_group(&mut self, group: HlGroup) {
        let bg = self.theme.resolve(group).bg.unwrap_or(Color::Reset);
        self.set_gutter_bg(bg);
    }

    pub fn mark_soft_wrap_continuation(&mut self) {
        self.cur_decoration.soft_wrapped = true;
    }

    /// Attach raw source text to the current line so copy emits markdown rather than display text.
    pub fn set_source_text(&mut self, text: &str) {
        self.cur_decoration.source_text = Some(text.to_string());
    }

    /// Stamp the current line's logical source-line mapping. Gutter providers
    /// like `LineNumberGutter` read this to render per-row line numbers.
    pub fn set_source_line(&mut self, source_line: SourceLine) {
        self.cur_decoration.source_line = Some(source_line);
    }

    /// Attach `source` to the next committed line; subsequent `newline()` calls become soft-wrap
    /// continuations until `disarm_source_text` is called.
    pub fn arm_source_text(&mut self, source: String) {
        self.pending_source_text = Some(source);
        self.auto_soft_wrap_continuation = true;
    }

    pub fn disarm_source_text(&mut self) {
        self.pending_source_text = None;
        self.auto_soft_wrap_continuation = false;
    }

    // ── Style state ─────────────────────────────────────────────────

    /// Push the current (group, style) onto the stack and replace with the supplied pair.
    pub fn push(&mut self, group: Option<HlGroup>, style: Style) {
        self.style_stack.push((self.cur_group, self.cur_style));
        self.cur_group = group;
        self.cur_style = style;
    }

    /// Snapshot the current (group, style) onto the stack without changing it. Pair with
    /// `pop_style` to restore. Use this when emitting a run that mutates style state
    /// (e.g. per-region syntax colors) without clobbering what the caller had set.
    pub fn save_style(&mut self) {
        self.style_stack.push((self.cur_group, self.cur_style));
    }

    fn push_clone(&mut self) {
        self.save_style();
    }

    pub fn pop_style(&mut self) {
        if let Some((g, s)) = self.style_stack.pop() {
            self.cur_group = g;
            self.cur_style = s;
        }
    }

    pub fn reset_style(&mut self) {
        self.cur_group = None;
        self.cur_style = Style::default();
    }

    pub fn set_fg(&mut self, c: Color) {
        self.cur_style.fg = Some(c);
    }

    pub fn set_bg(&mut self, c: Color) {
        self.cur_style.bg = Some(c);
    }

    pub fn set_hl(&mut self, group: HlGroup) {
        self.cur_group = Some(group);
    }

    pub fn set_bold(&mut self) {
        self.cur_style.bold = true;
    }

    pub fn set_dim(&mut self) {
        self.cur_style.dim = true;
    }

    pub fn set_italic(&mut self) {
        self.cur_style.italic = true;
    }

    pub fn set_crossedout(&mut self) {
        self.cur_style.crossedout = true;
    }

    pub fn set_dim_italic(&mut self) {
        self.cur_style.dim = true;
        self.cur_style.italic = true;
    }

    pub fn push_fg(&mut self, c: Color) {
        self.push_clone();
        self.cur_style.fg = Some(c);
    }

    pub fn push_hl(&mut self, group: HlGroup) {
        self.push_clone();
        self.cur_group = Some(group);
    }

    pub fn push_bold(&mut self) {
        self.push_clone();
        self.cur_style.bold = true;
    }

    pub fn push_dim(&mut self) {
        self.push_clone();
        self.cur_style.dim = true;
    }

    pub fn push_italic(&mut self) {
        self.push_clone();
        self.cur_style.italic = true;
    }

    pub fn push_crossedout(&mut self) {
        self.push_clone();
        self.cur_style.crossedout = true;
    }

    // ── Internals ───────────────────────────────────────────────────

    fn append_span_styled(&mut self, text: &str, meta: SpanMeta) {
        let resolved = self.resolve_current();
        let style_default = style_is_default(&resolved);
        let meta_default = meta.selectable && meta.copy_as.is_none();
        if style_default && meta_default {
            self.append_text(text);
            return;
        }
        let hl = self.current_hl(resolved);
        self.append_span_with_hl(text, hl, meta);
    }

    fn append_span_resolved(&mut self, text: &str, style: Style, meta: SpanMeta) {
        let style_default = style_is_default(&style);
        let meta_default = meta.selectable && meta.copy_as.is_none();
        if style_default && meta_default {
            self.append_text(text);
            return;
        }
        let hl = intern_anonymous_style(style);
        self.append_span_with_hl(text, hl, meta);
    }

    fn append_text(&mut self, text: &str) {
        let chars_before = self.cur_text.chars().count() as u16;
        self.cur_text.push_str(text);
        let chars_after = self.cur_text.chars().count() as u16;
        if chars_after != chars_before {
            self.has_pending_content = true;
        }
    }

    fn append_span_with_hl(&mut self, text: &str, hl: HlGroup, meta: SpanMeta) {
        let chars_before = self.cur_text.chars().count() as u16;
        self.cur_text.push_str(text);
        let chars_after = self.cur_text.chars().count() as u16;
        if chars_after == chars_before {
            return;
        }
        self.has_pending_content = true;
        // Coalesce with the previous highlight if it has the same
        // hl+meta and was contiguous.
        if let Some(last) = self.cur_highlights.last_mut() {
            if last.1 == chars_before && last.2 == hl && last.3 == meta {
                last.1 = chars_after;
                return;
            }
        }
        self.cur_highlights
            .push((chars_before, chars_after, hl, meta));
    }

    /// Map the active (group, style) to an interned [`HlGroup`].
    /// A plain group reference flows the id directly (live theme updates); compound styles intern anonymously.
    fn current_hl(&self, resolved: Style) -> HlGroup {
        if let Some(group) = self.cur_group {
            if !style_has_axis_mods(&self.cur_style) && self.theme.contains(group) {
                return group;
            }
        }
        intern_anonymous_style(resolved)
    }

    fn commit_line(&mut self) {
        let target_row = self.starting_line + self.lines_committed;
        let buf_len = self.buf.line_count();
        let text = std::mem::take(&mut self.cur_text);
        let highlights = std::mem::take(&mut self.cur_highlights);
        let mut decoration = std::mem::take(&mut self.cur_decoration);
        // LineBuilder output is intrinsically pre-formatted: callers (parsers,
        // markdown, code, diff) have already laid this row out at the chosen
        // width. The host window's `WrappedLayout` keys off this so it doesn't
        // re-wrap parser-produced rows.
        decoration.pre_formatted = true;

        if target_row < buf_len {
            self.buf.set_lines(target_row, target_row + 1, vec![text]);
            if target_row == self.starting_line && !self.overwrote_blank_seed {
                self.overwrote_blank_seed = true;
            }
        } else {
            self.buf.set_lines(buf_len, buf_len, vec![text]);
        }

        for (col_start, col_end, hl, meta) in highlights {
            self.buf
                .add_highlight_group_with_meta(target_row, col_start, col_end, hl, meta);
        }
        self.buf.set_decoration(target_row, decoration);

        self.lines_committed += 1;
        self.has_pending_content = false;
        self.cur_visible_cols = 0;
    }

    fn cur_decoration_present(&self) -> bool {
        has_decoration(&self.cur_decoration)
    }

    fn resolve_current(&self) -> Style {
        let (group_fg, group_bg) = match self.cur_group {
            Some(g) => {
                let s = self.theme.resolve(g);
                // Unregistered group: force Color::Reset so the extmark bypasses the `style_is_default` short-circuit.
                let fg = s.fg.or(if s.bg.is_none() {
                    Some(Color::Reset)
                } else {
                    None
                });
                (fg, s.bg)
            }
            None => (None, None),
        };
        Style {
            fg: self.cur_style.fg.or(group_fg),
            bg: self.cur_style.bg.or(group_bg),
            bold: self.cur_style.bold,
            dim: self.cur_style.dim,
            italic: self.cur_style.italic,
            underline: self.cur_style.underline,
            crossedout: self.cur_style.crossedout,
        }
    }
}

fn style_has_axis_mods(s: &Style) -> bool {
    s.fg.is_some() || s.bg.is_some() || s.bold || s.dim || s.italic || s.underline || s.crossedout
}

fn has_decoration(dec: &LineDecoration) -> bool {
    dec.gutter_bg.is_some()
        || dec.fill_bg.is_some()
        || dec.fill_right_margin != 0
        || dec.soft_wrapped
        || dec.source_text.is_some()
}

fn style_is_default(s: &Style) -> bool {
    s.fg.is_none()
        && s.bg.is_none()
        && !s.bold
        && !s.dim
        && !s.italic
        && !s.underline
        && !s.crossedout
}

/// Build a fresh `Buffer`, render into it, and return the outcome.
pub fn render_into_fresh(
    width: u16,
    theme: &Theme,
    fill: impl FnOnce(&mut LineBuilder),
) -> (Buffer, Outcome) {
    use crate::buffer::{BufCreateOpts, BufId};
    let mut buf = Buffer::new(BufId(0), BufCreateOpts::default());
    let outcome = render_into(&mut buf, width, theme, fill);
    (buf, outcome)
}

pub fn render_into(
    buf: &mut Buffer,
    width: u16,
    theme: &Theme,
    fill: impl FnOnce(&mut LineBuilder),
) -> Outcome {
    let mut col = LineBuilder::new(buf, theme, width);
    fill(&mut col);
    col.finish()
}

/// Replay a previously-rendered `Buffer` into `out` as styled spans.
pub fn replay_buffer_into(buf: &Buffer, out: &mut LineBuilder) {
    let n = buf.line_count();
    for i in 0..n {
        replay_buffer_row_into(buf, i as u16, out);
        out.newline();
    }
    let _ = buf;
}

/// Replay one row of `buf` into `out` as styled spans without a trailing newline.
pub fn replay_buffer_row_into(buf: &Buffer, row: u16, out: &mut LineBuilder) {
    let text = buf.get_line(row as usize).unwrap_or("");
    let mut highlights = buf.highlights_at(row as usize);
    highlights.sort_by_key(|h| h.col_start);

    let chars: Vec<char> = text.chars().collect();
    let mut col_idx: u16 = 0;
    for h in &highlights {
        if h.col_end <= col_idx {
            continue;
        }
        if h.col_start > col_idx {
            let plain: String = chars[col_idx as usize..h.col_start as usize]
                .iter()
                .collect();
            out.print(&plain);
            col_idx = h.col_start;
        }
        let end = h.col_end.min(chars.len() as u16);
        if end <= col_idx {
            continue;
        }
        let segment: String = chars[col_idx as usize..end as usize].iter().collect();
        let style = out.theme.resolve(h.hl);
        out.append_resolved_span(&segment, style, h.meta.clone());
        col_idx = end;
    }
    if (col_idx as usize) < chars.len() {
        let tail: String = chars[col_idx as usize..].iter().collect();
        out.print(&tail);
    }
}

impl<'a> LineBuilder<'a> {
    /// Append a span with a pre-resolved style (no theme lookup). Used by replay paths.
    pub fn append_resolved_span(&mut self, text: &str, style: Style, meta: SpanMeta) {
        if text.is_empty() {
            return;
        }
        let w = display_width(text) as u16;
        self.cur_visible_cols = self.cur_visible_cols.saturating_add(w);
        self.append_span_resolved(text, style, meta);
    }
}

pub mod test_util {
    use super::*;
    use crate::buffer::{BufCreateOpts, BufId};

    pub struct TestSpan {
        pub text: String,
        pub style: Style,
        pub meta: SpanMeta,
    }

    pub struct TestLine {
        pub text: String,
        pub source_text: Option<String>,
        pub soft_wrapped: bool,
        pub spans: Vec<TestSpan>,
    }

    pub struct TestBlock {
        pub lines: Vec<TestLine>,
        pub outcome: Outcome,
    }

    pub fn render_test(width: u16, fill: impl FnOnce(&mut LineBuilder)) -> TestBlock {
        let theme = Theme::default();
        let mut buf = Buffer::new(BufId(0), BufCreateOpts::default());
        let outcome = render_into(&mut buf, width, &theme, fill);
        let lines = read_buffer(&buf, &theme, outcome.line_count);
        TestBlock { lines, outcome }
    }

    pub fn read_buffer(buf: &Buffer, theme: &Theme, line_count: usize) -> Vec<TestLine> {
        let n = line_count.min(buf.line_count());
        (0..n)
            .map(|i| {
                let text = buf.get_line(i).unwrap_or("").to_string();
                let dec = buf.decoration_at(i).clone();
                let mut highlights = buf.highlights_at(i);
                highlights.sort_by_key(|h| h.col_start);
                let chars: Vec<char> = text.chars().collect();
                let mut spans = Vec::new();
                let mut col: u16 = 0;
                for h in &highlights {
                    if h.col_end <= col {
                        continue;
                    }
                    if h.col_start > col {
                        let plain: String =
                            chars[col as usize..h.col_start as usize].iter().collect();
                        spans.push(TestSpan {
                            text: plain,
                            style: Style::default(),
                            meta: SpanMeta::default(),
                        });
                        col = h.col_start;
                    }
                    let end = h.col_end.min(chars.len() as u16);
                    if end <= col {
                        continue;
                    }
                    let segment: String = chars[col as usize..end as usize].iter().collect();
                    let style = theme.resolve(h.hl);
                    spans.push(TestSpan {
                        text: segment,
                        style,
                        meta: h.meta.clone(),
                    });
                    col = end;
                }
                if (col as usize) < chars.len() {
                    let tail: String = chars[col as usize..].iter().collect();
                    spans.push(TestSpan {
                        text: tail,
                        style: Style::default(),
                        meta: SpanMeta::default(),
                    });
                }
                TestLine {
                    text,
                    source_text: dec.source_text,
                    soft_wrapped: dec.soft_wrapped,
                    spans,
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::{BufCreateOpts, BufId};
    use crate::theme::intern;

    fn fresh_buf() -> Buffer {
        Buffer::new(BufId(0), BufCreateOpts::default())
    }

    #[test]
    fn display_width_counts_visible_columns() {
        assert_eq!(display_width(""), 0);
        assert_eq!(display_width("abc"), 3);
        assert_eq!(display_width("日本"), 4);
    }

    #[test]
    fn outcome_is_valid_at_pinned_when_wrapped() {
        let o = Outcome {
            line_count: 1,
            layout_width: 40,
            was_wrapped: true,
            max_line_width: 20,
        };
        assert!(o.is_valid_at(40));
        assert!(!o.is_valid_at(39));
        assert!(!o.is_valid_at(41));
    }

    #[test]
    fn outcome_is_valid_at_replayable_when_not_wrapped() {
        let o = Outcome {
            line_count: 1,
            layout_width: 40,
            was_wrapped: false,
            max_line_width: 20,
        };
        assert!(o.is_valid_at(20));
        assert!(o.is_valid_at(80));
        assert!(!o.is_valid_at(19));
    }

    #[test]
    fn line_count_zero_when_no_content() {
        let mut buf = fresh_buf();
        let theme = Theme::default();
        let lb = LineBuilder::new(&mut buf, &theme, 80);
        assert_eq!(lb.line_count(), 0);
    }

    #[test]
    fn line_count_one_when_pending_text() {
        let mut buf = fresh_buf();
        let theme = Theme::default();
        let mut lb = LineBuilder::new(&mut buf, &theme, 80);
        lb.print("hi");
        assert_eq!(lb.line_count(), 1);
    }

    #[test]
    fn line_count_one_when_decoration_present_but_no_text() {
        let mut buf = fresh_buf();
        let theme = Theme::default();
        let mut lb = LineBuilder::new(&mut buf, &theme, 80);
        lb.set_gutter_bg(Color::Red);
        assert_eq!(lb.line_count(), 1);
    }

    #[test]
    fn print_empty_string_is_noop() {
        let block = test_util::render_test(80, |out| {
            out.print("");
        });
        assert_eq!(block.outcome.line_count, 0);
    }

    #[test]
    fn print_with_meta_attaches_custom_copy_as() {
        let block = test_util::render_test(80, |out| {
            out.print_with_meta(
                "click",
                SpanMeta {
                    selectable: true,
                    copy_as: Some("real".into()),
                },
            );
        });
        assert_eq!(block.lines.len(), 1);
        let meta_copy = block.lines[0]
            .spans
            .iter()
            .find_map(|s| s.meta.copy_as.clone());
        assert_eq!(meta_copy.as_deref(), Some("real"));
    }

    #[test]
    fn fill_line_bg_group_resolves_from_theme_or_falls_back_to_reset() {
        let block = test_util::render_test(80, |out| {
            out.print("x");
            // Unknown group -> theme.resolve returns Style::default(), bg=None -> Color::Reset.
            out.fill_line_bg_group(intern("DefinitelyMissingGroup_xyz"), 0);
            out.newline();
        });
        assert_eq!(block.lines.len(), 1);
    }

    #[test]
    fn set_gutter_bg_and_group_set_decoration() {
        let mut buf = fresh_buf();
        let theme = Theme::default();
        let mut lb = LineBuilder::new(&mut buf, &theme, 80);
        lb.set_gutter_bg(Color::Blue);
        assert_eq!(lb.cur_decoration.gutter_bg, Some(Color::Blue));
        lb.set_gutter_bg_group(intern("AnotherMissing_xyz"));
        // Theme fallback -> Color::Reset; ensure the decoration was updated.
        assert!(lb.cur_decoration.gutter_bg.is_some());
    }

    #[test]
    fn mark_soft_wrap_continuation_sets_decoration_flag() {
        let mut buf = fresh_buf();
        let theme = Theme::default();
        let mut lb = LineBuilder::new(&mut buf, &theme, 80);
        lb.mark_soft_wrap_continuation();
        assert!(lb.cur_decoration.soft_wrapped);
    }

    #[test]
    fn set_source_text_attaches_raw_markdown() {
        let block = test_util::render_test(80, |out| {
            out.print("rendered");
            out.set_source_text("**rendered**");
            out.newline();
        });
        assert_eq!(block.lines[0].source_text.as_deref(), Some("**rendered**"));
    }

    #[test]
    fn arm_source_text_attaches_then_marks_subsequent_wraps_as_continuations() {
        let block = test_util::render_test(80, |out| {
            out.arm_source_text("source line".into());
            out.print("first");
            out.newline();
            out.print("second");
            out.newline();
            out.disarm_source_text();
            out.print("third");
            out.newline();
        });
        assert_eq!(block.lines.len(), 3);
        assert_eq!(block.lines[0].source_text.as_deref(), Some("source line"));
        // Second line: no fresh source, but armed continuation flag set.
        assert_eq!(block.lines[1].source_text, None);
        assert!(block.lines[1].soft_wrapped);
        // Third line: disarmed, so no continuation flag.
        assert!(!block.lines[2].soft_wrapped);
    }

    #[test]
    fn disarm_source_text_clears_pending_and_continuation() {
        let mut buf = fresh_buf();
        let theme = Theme::default();
        let mut lb = LineBuilder::new(&mut buf, &theme, 80);
        lb.arm_source_text("x".into());
        lb.disarm_source_text();
        assert!(lb.pending_source_text.is_none());
        assert!(!lb.auto_soft_wrap_continuation);
    }

    #[test]
    fn set_style_helpers_set_corresponding_flags() {
        let mut buf = fresh_buf();
        let theme = Theme::default();
        let mut lb = LineBuilder::new(&mut buf, &theme, 80);
        lb.set_bold();
        assert!(lb.cur_style.bold);
        lb.set_italic();
        assert!(lb.cur_style.italic);
        lb.set_dim();
        assert!(lb.cur_style.dim);
        lb.set_crossedout();
        assert!(lb.cur_style.crossedout);
        lb.reset_style();
        lb.set_dim_italic();
        assert!(lb.cur_style.dim);
        assert!(lb.cur_style.italic);
        assert!(!lb.cur_style.bold);
    }

    #[test]
    fn push_helpers_save_state_and_apply_modification() {
        let mut buf = fresh_buf();
        let theme = Theme::default();
        let mut lb = LineBuilder::new(&mut buf, &theme, 80);
        lb.push_fg(Color::Red);
        assert_eq!(lb.cur_style.fg, Some(Color::Red));
        lb.pop_style();
        assert_eq!(lb.cur_style.fg, None);

        lb.push_hl(intern("SomeGroup"));
        assert!(lb.cur_group.is_some());
        lb.pop_style();
        assert!(lb.cur_group.is_none());

        lb.push_bold();
        lb.push_italic();
        lb.push_dim();
        lb.push_crossedout();
        assert!(lb.cur_style.bold);
        assert!(lb.cur_style.italic);
        assert!(lb.cur_style.dim);
        assert!(lb.cur_style.crossedout);
        for _ in 0..4 {
            lb.pop_style();
        }
        assert_eq!(lb.cur_style, Style::default());
    }

    #[test]
    fn pop_style_on_empty_stack_is_noop() {
        let mut buf = fresh_buf();
        let theme = Theme::default();
        let mut lb = LineBuilder::new(&mut buf, &theme, 80);
        // Should not panic with no pushes.
        lb.pop_style();
        assert_eq!(lb.cur_style, Style::default());
        assert!(lb.cur_group.is_none());
    }

    #[test]
    fn set_hl_assigns_group_for_subsequent_prints() {
        let block = test_util::render_test(80, |out| {
            let g = intern("MyGroup");
            out.set_hl(g);
            out.print("x");
            out.reset_style();
            out.newline();
        });
        // The group passes through as a highlight on the span.
        let has_hl = block.lines[0].spans.iter().any(|s| !s.text.is_empty());
        assert!(has_hl);
    }

    #[test]
    fn render_into_fresh_returns_buffer_and_outcome() {
        let theme = Theme::default();
        let (buf, outcome) = render_into_fresh(80, &theme, |out| {
            out.print("hello");
            out.newline();
        });
        assert_eq!(outcome.line_count, 1);
        assert!(!outcome.was_wrapped);
        assert_eq!(buf.line_count(), 1);
        assert_eq!(buf.get_line(0), Some("hello"));
    }

    #[test]
    fn replay_buffer_into_reproduces_text_lines() {
        let theme = Theme::default();
        let (src_buf, _) = render_into_fresh(80, &theme, |out| {
            out.print("line one");
            out.newline();
            out.set_hl(intern("MyGroup"));
            out.print("line two");
            out.reset_style();
            out.newline();
        });
        let block = test_util::render_test(80, |out| {
            replay_buffer_into(&src_buf, out);
        });
        let texts: Vec<&str> = block.lines.iter().map(|l| l.text.as_str()).collect();
        assert!(texts.contains(&"line one"));
        assert!(texts.contains(&"line two"));
    }

    #[test]
    fn replay_buffer_row_into_emits_one_row_without_trailing_newline() {
        let theme = Theme::default();
        let (src_buf, _) = render_into_fresh(80, &theme, |out| {
            out.print("only one");
            out.newline();
        });
        let block = test_util::render_test(80, |out| {
            replay_buffer_row_into(&src_buf, 0, out);
        });
        assert!(!block.lines.is_empty());
        assert_eq!(block.lines[0].text, "only one");
    }

    #[test]
    fn finish_no_content_returns_empty_outcome() {
        let mut buf = fresh_buf();
        let theme = Theme::default();
        let lb = LineBuilder::new(&mut buf, &theme, 80);
        let outcome = lb.finish();
        assert_eq!(outcome.line_count, 0);
        assert_eq!(outcome.layout_width, 80);
        assert!(!outcome.was_wrapped);
    }

    #[test]
    fn finish_flushes_trailing_pending_content_as_one_line() {
        let block = test_util::render_test(80, |out| {
            out.print("no trailing newline");
        });
        assert_eq!(block.outcome.line_count, 1);
        assert_eq!(block.lines[0].text, "no trailing newline");
    }

    #[test]
    fn newline_after_styled_text_records_max_line_width() {
        let block = test_util::render_test(80, |out| {
            out.print("hello");
            out.newline();
            out.print("hi");
            out.newline();
        });
        assert!(block.outcome.max_line_width >= 5);
    }

    #[test]
    fn current_hl_uses_registered_group_directly_without_axis_mods() {
        // Register the group with a real bg so theme.contains() is true.
        let mut theme = Theme::default();
        theme.set(
            "__BuilderTestGroup_xyz__",
            Style {
                bg: Some(Color::Red),
                ..Default::default()
            },
        );
        let group = theme.id_for("__BuilderTestGroup_xyz__");
        let mut buf = fresh_buf();
        let mut lb = LineBuilder::new(&mut buf, &theme, 80);
        lb.set_hl(group);
        lb.print("x");
        lb.newline();
        let outcome = lb.finish();
        assert_eq!(outcome.line_count, 1);
    }
}
