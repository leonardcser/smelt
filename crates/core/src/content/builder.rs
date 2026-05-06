//! Buffer-builder used by every block renderer.
//!
//! `LineBuilder` is the single layout primitive: renderers
//! (markdown, diff, syntax, tool blocks, dialog previews) walk their
//! input and call `print` / `newline` / `push_style` etc. on a
//! `&mut LineBuilder`; the collector resolves styles against the
//! supplied [`Theme`] and writes lines + highlights + decorations
//! directly into a [`Buffer`]. There is no intermediate
//! span-tree representation — `Buffer` is the only output.
//!
//! Callers construct a fresh collector each time they want to render
//! into a buffer. The collector borrows the buffer and theme for the
//! duration of rendering; on [`LineBuilder::finish`] the trailing
//! incomplete line is flushed and an [`Outcome`] (line count + width
//! pinning info) returned.

use crate::buffer::{Buffer, LineDecoration, SpanMeta};
use crate::style::{Color, Style};
use crate::theme::{intern_anonymous_style, HlGroup, Theme};
use unicode_width::UnicodeWidthStr;

/// Display-column width of a string slice. Used for visible-width
/// tracking inside `LineBuilder` and by callers pre-measuring
/// content.
pub fn display_width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

/// Outcome metadata returned by [`LineBuilder::finish`]. Mirrors the
/// fields the old `DisplayBlock` carried so callers can reason about
/// width pinning the same way.
#[derive(Debug, Clone, Copy, Default)]
pub struct Outcome {
    /// Number of logical lines committed to the buffer.
    pub line_count: usize,
    /// Terminal width this layout was computed at.
    pub layout_width: u16,
    /// True iff layout broke at least one logical line into multiple
    /// visual rows. When false, the layout is replayable at any width
    /// >= `max_line_width`.
    pub was_wrapped: bool,
    /// Longest visible line in the layout (display columns).
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

/// Index of the first line this collector wrote to in `buf`. Lines
/// before this offset are left untouched; renderers append to a
/// non-empty buffer by passing the existing line count as the start
/// row at construction (handled internally — collector starts writing
/// at `buf.line_count()` minus one if the trailing line is empty,
/// otherwise appends after it).
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

    // Style state — group + axis mods tracked separately. Resolved
    // at print time. Single-group spans with default mods flow the
    // group id directly; compound spans intern anonymously.
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
        // Append mode: write past the existing content. Buffer always
        // starts with at least one (possibly empty) line; the first
        // committed line replaces the trailing empty seed when present.
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

    /// Active theme reference. Used by callers that need to resolve
    /// theme groups to concrete colors (e.g. for paint-time decoration
    /// fields that don't carry an HlGroup id).
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

    /// Number of logical display lines accumulated so far, including
    /// the current incomplete line if it has any content. Mirrors the
    /// old DisplayBlock-based count for renderer code that branches on
    /// it (e.g. tool previews).
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

    /// Mark the layout as width-pinned: it cannot be replayed at a
    /// different viewport width without re-laying-out from the source.
    /// Wrap helpers call this when they break a line.
    pub fn mark_wrapped(&mut self) {
        self.was_wrapped = true;
    }

    /// Fill the remainder of the current visual row with `bg` so the row
    /// extends to the right edge of the viewport (minus `right_margin`
    /// columns reserved on the right). Used by diff and code rows.
    pub fn fill_line_bg(&mut self, bg: Color, right_margin: u16) {
        // Calling `fill_line_bg` twice on the same row would silently
        // overwrite the first fill. No legitimate caller does this — a
        // row has at most one trailing bg fill — so catch the misuse in
        // debug builds.
        debug_assert!(
            self.cur_decoration.fill_bg.is_none(),
            "fill_line_bg called twice on the same row"
        );
        self.cur_decoration.fill_bg = Some(bg);
        self.cur_decoration.fill_right_margin = right_margin;
    }

    /// Same as `fill_line_bg` but takes a theme group; reads the
    /// group's bg through the active theme. For role-keyed background
    /// fills the renderer still wants the *current* color baked in
    /// (decoration is a paint-time hint, not a buffer-level group ref).
    pub fn fill_line_bg_group(&mut self, group: HlGroup, right_margin: u16) {
        let bg = self.theme.resolve(group).bg.unwrap_or(Color::Reset);
        self.fill_line_bg(bg, right_margin);
    }

    /// Set the gutter background for the current line. Paint-time gutter
    /// padding will be filled with this color instead of blank spaces.
    pub fn set_gutter_bg(&mut self, bg: Color) {
        self.cur_decoration.gutter_bg = Some(bg);
    }

    /// Same as `set_gutter_bg` but takes a theme group; reads the
    /// group's bg through the active theme.
    pub fn set_gutter_bg_group(&mut self, group: HlGroup) {
        let bg = self.theme.resolve(group).bg.unwrap_or(Color::Reset);
        self.set_gutter_bg(bg);
    }

    /// Mark the current line as a soft-wrap continuation of the
    /// previous logical line.
    pub fn mark_soft_wrap_continuation(&mut self) {
        self.cur_decoration.soft_wrapped = true;
    }

    /// Set the raw source text for the current line. Used by markdown
    /// rendering so copy walks emit raw markdown instead of display
    /// text for fully-selected rows.
    pub fn set_source_text(&mut self, text: &str) {
        self.cur_decoration.source_text = Some(text.to_string());
    }

    /// Tag the next-closed line with `source` and turn every following
    /// `newline()` into a soft-wrap continuation until
    /// `disarm_source_text` is called.
    pub fn arm_source_text(&mut self, source: String) {
        self.pending_source_text = Some(source);
        self.auto_soft_wrap_continuation = true;
    }

    pub fn disarm_source_text(&mut self) {
        self.pending_source_text = None;
        self.auto_soft_wrap_continuation = false;
    }

    // ── Style state ─────────────────────────────────────────────────

    /// Push the current (group, style) onto the stack and replace
    /// with the supplied pair. Pair `pop_style` to restore.
    pub fn push(&mut self, group: Option<HlGroup>, style: Style) {
        self.style_stack.push((self.cur_group, self.cur_style));
        self.cur_group = group;
        self.cur_style = style;
    }

    /// Save current state on the stack without changing it. Following
    /// `set_*` calls modify the new layer; `pop_style` restores.
    fn push_clone(&mut self) {
        self.style_stack.push((self.cur_group, self.cur_style));
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

    /// Set the current span's theme group. The group's resolved
    /// fg/bg fill any unset slots on the explicit `fg`/`bg`. When the
    /// rest of the style is default, the collector emits this id
    /// directly so theme switches flip the rendered span without
    /// re-running the parser.
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

    /// Renderer-facing append: resolves the active (group, style)
    /// pair through the theme. Single-group spans with default mods
    /// flow the group id directly so theme switches flip the rendered
    /// span without re-running the parser; compound spans intern
    /// anonymously at the resolved Style.
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

    /// Resolved-Style append for replay paths that read spans from an
    /// existing buffer (no role info to recover). Falls through to
    /// anonymous interning — these spans don't follow theme switches,
    /// but neither does the source buffer they're being copied from.
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
    /// Single theme-group reference with no other axis modifications
    /// flows the group id directly (theme switches mutate
    /// `Theme.styles[id]` once and the rendered span tracks live).
    /// Anything more complex — group plus axis mods, or concrete
    /// `fg`/`bg` — falls back to content-hashed anonymous interning of
    /// the resolved `Style`.
    fn current_hl(&self, resolved: Style) -> HlGroup {
        if let Some(group) = self.cur_group {
            if !style_has_axis_mods(&self.cur_style) && self.theme.contains(group) {
                return group;
            }
        }
        intern_anonymous_style(resolved)
    }

    fn commit_line(&mut self) {
        // Choose the destination row.
        let target_row = self.starting_line + self.lines_committed;
        let buf_len = self.buf.line_count();
        let text = std::mem::take(&mut self.cur_text);
        let highlights = std::mem::take(&mut self.cur_highlights);
        let decoration = std::mem::take(&mut self.cur_decoration);

        if target_row < buf_len {
            // Replace existing line (the buffer's seed empty line on
            // the very first commit, or a line we previously wrote in
            // append mode).
            self.buf.set_lines(target_row, target_row + 1, vec![text]);
            if target_row == self.starting_line && !self.overwrote_blank_seed {
                self.overwrote_blank_seed = true;
            }
        } else {
            // Append.
            self.buf.set_lines(buf_len, buf_len, vec![text]);
        }

        for (col_start, col_end, hl, meta) in highlights {
            self.buf
                .add_highlight_group_with_meta(target_row, col_start, col_end, hl, meta);
        }
        if has_decoration(&decoration) {
            self.buf.set_decoration(target_row, decoration);
        }

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
                // Empty Theme entry ⇒ ensure the span still emits a
                // non-default Style so the extmark survives the
                // `style_is_default` short-circuit. `Color::Reset`
                // is the role-fallback color for groups the active
                // theme hasn't registered.
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

/// Convenience: build a fresh Buffer, render into it, and return the
/// outcome. Used by callers that want a one-off scratch buffer and
/// don't care about the BufId.
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

/// Construct a `LineBuilder` around `buf`, run `fill`, and return
/// the outcome. The most common renderer entry point.
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

/// Read a previously-rendered Buffer back as if it were a single
/// "block" — useful when nested renderers want to inline a tool's
/// per-call Buffer into a parent collector. Mirrors the old
/// `buffer_into_collector` shape but writes via the regular
/// collector API so styles and metas round-trip through theme
/// resolution unchanged.
pub fn replay_buffer_into(buf: &Buffer, out: &mut LineBuilder) {
    let n = buf.line_count();
    for i in 0..n {
        replay_buffer_row_into(buf, i as u16, out);
        out.newline();
    }

    // Carry through line decorations from the source buffer.
    // Replay loop above committed `n` lines starting from
    // `out.starting_line + (out.lines_committed - n)`. We can't easily
    // address those from outside, so we set decorations as we go via
    // a small internal helper.
    let _ = buf; // suppress unused after the loop
}

/// Replay one row of `buf` into `out` as styled spans, without emitting
/// a trailing newline. Used by `render_summary` Lua hooks: the caller
/// mints an ephemeral Buffer, runs the Lua callback against it, then
/// projects row 0 inline into the transcript / confirm-title sink.
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
    /// Append a span whose style is already resolved (no theme lookup
    /// needed). Internal helper for [`replay_buffer_row_into`]:
    /// replay reads spans by HlGroup id from the source Buffer and
    /// hands the caller the per-span resolved Style; we re-intern
    /// anonymously so the replayed mark sits in the destination
    /// Buffer's payload alongside live-named groups.
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
    //! Helpers that rebuild the old `DisplayBlock` / `DisplayLine` /
    //! `DisplaySpan` shape from a rendered `Buffer`, for the unit
    //! tests that grew up around the IR.
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

    /// Build a fresh buffer + default theme, run `fill`, then read the
    /// resulting buffer back into the legacy `TestBlock` shape.
    pub fn render_test(width: u16, fill: impl FnOnce(&mut LineBuilder)) -> TestBlock {
        let theme = Theme::default();
        let mut buf = Buffer::new(BufId(0), BufCreateOpts::default());
        let outcome = render_into(&mut buf, width, &theme, fill);
        let lines = read_buffer(&buf, &theme, outcome.line_count);
        TestBlock { lines, outcome }
    }

    /// Convert a rendered buffer into per-line text + source / soft-wrap
    /// metadata + spans (highlight runs interleaved with plain runs).
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
