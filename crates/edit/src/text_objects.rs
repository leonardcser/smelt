//! Vim text-object selection (iw/aw, i"/a", i(/a(, ip/ap, etc.) over `&str` buffers.

use super::scan::{Scan, Visual};
use super::text::{line_end, line_start, next_grapheme_boundary, prev_grapheme_boundary};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SurroundDelimiters {
    pub open_start: usize,
    pub open_end: usize,
    pub close_start: usize,
    pub close_end: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TextObjectSpec {
    pub inner: bool,
    pub kind: TextObjectKind,
}

impl TextObjectSpec {
    pub fn new(inner: bool, kind: char) -> Option<Self> {
        Some(Self {
            inner,
            kind: TextObjectKind::from_char(kind)?,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TextObjectKind {
    Word,
    BigWord,
    Sentence,
    Quote(char),
    AnyQuote,
    Pair {
        open: char,
        close: char,
    },
    Tag,
    Paragraph,
    Line,
    /// A fenced code block, which only a row document knows.
    CodeBlock,
}

impl TextObjectKind {
    fn from_char(kind: char) -> Option<Self> {
        match kind {
            'w' => Some(Self::Word),
            'W' => Some(Self::BigWord),
            '"' | '\'' | '`' => Some(Self::Quote(kind)),
            'q' => Some(Self::AnyQuote),
            's' => Some(Self::Sentence),
            'l' => Some(Self::Line),
            'c' => Some(Self::CodeBlock),
            '(' | ')' | 'b' => Some(Self::Pair {
                open: '(',
                close: ')',
            }),
            '[' | ']' | 'r' => Some(Self::Pair {
                open: '[',
                close: ']',
            }),
            '{' | '}' | 'B' => Some(Self::Pair {
                open: '{',
                close: '}',
            }),
            '<' | '>' | 'a' => Some(Self::Pair {
                open: '<',
                close: '>',
            }),
            't' => Some(Self::Tag),
            'p' | 'P' => Some(Self::Paragraph),
            _ => None,
        }
    }
}

/// The Visual area a text object extends, when there is one.
#[derive(Clone, Copy, Debug)]
pub(crate) struct VisualArea {
    pub anchor: usize,
    pub linewise: bool,
}

/// What a text object selects: from `start` to `end`, the cursor after the
/// object. `inclusive` says whether the character at `end` is part of it. In
/// Visual mode `start` is the new anchor and `end` the new cursor, both
/// inclusive, and `linewise` the mode to continue in. `ahead` says the object
/// was found after the cursor, as none was around it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ObjectSelection {
    pub start: usize,
    pub end: usize,
    pub inclusive: bool,
    pub linewise: bool,
    pub ahead: bool,
}

impl ObjectSelection {
    /// The bytes it covers, with the line break after a linewise object.
    pub fn range(self, buf: &str) -> (usize, usize) {
        let (lo, hi) = (self.start.min(self.end), self.start.max(self.end));
        if self.linewise {
            return (line_start(buf, lo), (line_end(buf, hi) + 1).min(buf.len()));
        }
        let end = match buf[hi.min(buf.len())..].chars().next() {
            Some(c) if self.inclusive && c != '\n' => {
                smelt_buffer::text::ceil_grapheme(buf, hi + c.len_utf8())
            }
            _ => hi,
        };
        (smelt_buffer::text::snap_grapheme(buf, lo), end)
    }
}

/// The bytes the `kind` object covers from `cpos`.
pub(crate) fn text_object(
    buf: &str,
    cpos: usize,
    inner: bool,
    kind: char,
) -> Option<(usize, usize)> {
    let spec = TextObjectSpec::new(inner, kind)?;
    Some(select_object(buf, cpos, None, spec, 1).ok()?.range(buf))
}

/// Where a failed object left the cursor and, in Visual mode, the anchor. A
/// count that runs out of text fails after the object has already moved them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Stranded {
    pub cursor: usize,
    pub anchor: Option<usize>,
}

/// Select `count` objects of `spec` around `cursor`, or extend `visual`.
pub(crate) fn select_object(
    buf: &str,
    cursor: usize,
    visual: Option<VisualArea>,
    spec: TextObjectSpec,
    count: usize,
) -> Result<ObjectSelection, Stranded> {
    let mut scan = Scan::new(buf, cursor);
    scan.vis = visual.map(|v| Visual {
        start: scan.pos_at(v.anchor),
        linewise: v.linewise,
    });
    let include = !spec.inner;
    let count = count.max(1);
    let found = match spec.kind {
        TextObjectKind::Word => scan.current_word(count, include, false),
        TextObjectKind::BigWord => scan.current_word(count, include, true),
        TextObjectKind::Sentence => scan.current_sent(count, include),
        TextObjectKind::Paragraph => scan.current_par(count, include),
        TextObjectKind::Line => scan.current_line(include),
        TextObjectKind::CodeBlock => {
            return code_block(buf, cursor, spec.inner).ok_or(Stranded {
                cursor,
                anchor: visual.map(|v| v.anchor),
            })
        }
        TextObjectKind::Quote(quote) => scan.current_quote(count, include, quote),
        TextObjectKind::Pair { open, close } => scan.current_block(count, include, open, close),
        TextObjectKind::Tag => scan.current_tagblock(count, include),
        TextObjectKind::AnyQuote => {
            let Some(quote) = nearest_quote(buf, cursor) else {
                return Err(Stranded {
                    cursor,
                    anchor: visual.map(|v| v.anchor),
                });
            };
            let kind = TextObjectKind::Quote(quote);
            return select_object(buf, cursor, visual, TextObjectSpec { kind, ..spec }, count);
        }
    };
    let end = scan.offset(scan.cur);
    if !found {
        return Err(Stranded {
            cursor: end,
            anchor: scan.vis.map(|vis| scan.offset(vis.start)),
        });
    }
    // A bracket or quote followed by a combining mark ends inside a grapheme:
    // an inner object starts after it, an outer one before it.
    let whole = |pos: usize, linewise: bool| match (linewise, spec.inner) {
        (true, _) => pos,
        (false, true) => smelt_buffer::text::ceil_grapheme(buf, pos),
        (false, false) => smelt_buffer::text::snap_grapheme(buf, pos),
    };
    Ok(match scan.vis {
        Some(vis) => ObjectSelection {
            start: whole(scan.offset(vis.start), vis.linewise),
            end,
            inclusive: true,
            linewise: vis.linewise,
            ahead: scan.ahead,
        },
        None => ObjectSelection {
            start: whole(scan.offset(scan.oap.start), scan.oap.linewise),
            end,
            inclusive: scan.oap.inclusive,
            linewise: scan.oap.linewise,
            ahead: scan.ahead,
        },
    })
}

/// `ic` and `ac`: the lines of the fenced code block around `cursor`, or else
/// of the next one after it, without or with its fences. `ic` fails on an
/// empty block.
fn code_block(buf: &str, cursor: usize, inner: bool) -> Option<ObjectSelection> {
    let (open, close) = code_fence_at(buf, cursor)?;
    let (start, end) = if inner {
        let first = line_end(buf, open) + 1;
        if first >= close {
            return None;
        }
        (first, line_start(buf, close - 1))
    } else {
        (open, close)
    };
    Some(ObjectSelection {
        start,
        end,
        inclusive: true,
        linewise: true,
        ahead: open > cursor,
    })
}

/// The starts of the opening and closing fence lines of the first Markdown
/// code block that does not end before the line `cursor` is on: the one that
/// holds it, or else the next one. A fence is a run of at least three
/// backticks or tildes; the closing one is at least as long as the opening one
/// and has nothing after it.
fn code_fence_at(buf: &str, cursor: usize) -> Option<(usize, usize)> {
    let mut open: Option<(usize, char, usize)> = None;
    let mut line = 0;
    loop {
        let eol = line_end(buf, line);
        let text = buf[line..eol].trim_start_matches(' ');
        let mark = text.chars().next().filter(|c| matches!(c, '`' | '~'));
        let run = mark.map_or(0, |mark| text.len() - text.trim_start_matches(mark).len());
        match open {
            None if run >= 3 => open = mark.map(|mark| (line, mark, run)),
            Some((start, held, len))
                if mark == Some(held) && run >= len && text[run..].trim().is_empty() =>
            {
                if cursor <= eol {
                    return Some((start, line));
                }
                open = None;
            }
            _ => {}
        }
        if eol >= buf.len() {
            return None;
        }
        line = eol + 1;
    }
}

/// The quote character of the string nearest `cursor` on its line: the
/// smallest one around it, or else the first one after it.
fn nearest_quote(buf: &str, cursor: usize) -> Option<char> {
    ['"', '\'', '`']
        .into_iter()
        .filter_map(|quote| {
            // `2i"` takes the quotes and nothing around them.
            let spec = TextObjectSpec {
                inner: true,
                kind: TextObjectKind::Quote(quote),
            };
            let (start, end) = select_object(buf, cursor, None, spec, 2).ok()?.range(buf);
            let around = start <= cursor && cursor < end;
            Some((!around, if around { end - start } else { start }, quote))
        })
        .min()
        .map(|(_, _, quote)| quote)
}

/// The delimiters of the `kind` object around `cpos`, which `ds` and `cs`
/// edit: its first and last characters, or a tag block's tags.
pub(crate) fn surrounding_delimiters(
    buf: &str,
    cpos: usize,
    kind: char,
) -> Option<SurroundDelimiters> {
    let kind = if kind == 's' { 'q' } else { kind };
    let select = |inner: bool, count: usize| {
        let spec = TextObjectSpec::new(inner, kind)?;
        Some(select_object(buf, cpos, None, spec, count).ok()?.range(buf))
    };
    let (start, end, inner) = match TextObjectKind::from_char(kind)? {
        TextObjectKind::Tag => {
            let (start, end) = select(false, 1)?;
            (start, end, select(true, 1)?)
        }
        TextObjectKind::Quote(_) | TextObjectKind::AnyQuote => {
            let (start, end) = select(true, 2)?;
            (
                start,
                end,
                (
                    next_grapheme_boundary(buf, start),
                    prev_grapheme_boundary(buf, end),
                ),
            )
        }
        TextObjectKind::Pair { .. } => {
            let (start, end) = select(false, 1)?;
            (
                start,
                end,
                (
                    next_grapheme_boundary(buf, start),
                    prev_grapheme_boundary(buf, end),
                ),
            )
        }
        _ => return None,
    };
    (inner.0 <= inner.1).then_some(SurroundDelimiters {
        open_start: start,
        open_end: inner.0,
        close_start: inner.1,
        close_end: end,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slice(buf: &str, range: (usize, usize)) -> &str {
        &buf[range.0..range.1]
    }

    fn assert_grapheme_range(buf: &str, range: (usize, usize)) {
        let boundaries: Vec<usize> = smelt_buffer::cell_width::grapheme_indices(buf)
            .map(|(start, _)| start)
            .chain(std::iter::once(buf.len()))
            .collect();
        assert!(boundaries.contains(&range.0), "invalid start: {range:?}");
        assert!(boundaries.contains(&range.1), "invalid end: {range:?}");
    }

    #[test]
    fn capital_p_alias_selects_paragraph() {
        let s = "before\n\npara a\npara b\n\nafter";
        let r = text_object(s, s.find("para a").unwrap(), true, 'P').unwrap();
        assert_eq!(slice(s, r), "para a\npara b\n");
    }

    // ── dispatcher ───────────────────────────────────────────────────────

    #[test]
    fn unknown_text_object_kind_returns_none() {
        assert_eq!(text_object("hello", 0, true, 'z'), None);
        assert_eq!(text_object("hello", 0, false, '?'), None);
    }

    // ── word: iw / aw ────────────────────────────────────────────────────

    #[test]
    fn iw_selects_just_the_word_under_cursor() {
        let s = "the quick brown";
        let r = text_object(s, 5, true, 'w').unwrap();
        assert_eq!(slice(s, r), "quick");
    }

    #[test]
    fn word_objects_never_split_grapheme_clusters() {
        let decomposed = "one e\u{301}cho two";
        let start = decomposed.find("e\u{301}").unwrap();
        let range = text_object(decomposed, start + 1, true, 'w').unwrap();
        assert_eq!(slice(decomposed, range), "e\u{301}cho");

        for grapheme in ["👩\u{200d}💻", "9\u{fe0f}", "🇨🇦"] {
            let text = format!("a {grapheme} z");
            let start = text.find(grapheme).unwrap();
            let range = text_object(&text, start, true, 'w').unwrap();
            assert_eq!(slice(&text, range), grapheme);
        }
    }

    #[test]
    fn aw_extends_to_trailing_whitespace_when_present() {
        let s = "the quick brown";
        let r = text_object(s, 5, false, 'w').unwrap();
        assert_eq!(slice(s, r), "quick ");
    }

    #[test]
    fn aw_falls_back_to_leading_whitespace_at_eol() {
        let s = "the quick";
        let r = text_object(s, 5, false, 'w').unwrap();
        assert_eq!(slice(s, r), " quick");
    }

    #[test]
    fn aw_whitespace_extension_keeps_graphemes_atomic() {
        for text in ["\u{600} word", "x \u{301}word"] {
            let range = text_object(text, text.find("word").unwrap(), false, 'w').unwrap();
            assert_grapheme_range(text, range);
        }
        let combining_space = "x \u{301}word";
        let range = text_object(
            combining_space,
            combining_space.find("word").unwrap(),
            false,
            'w',
        )
        .unwrap();
        assert_eq!(slice(combining_space, range), " \u{301}word");
    }

    #[test]
    fn iw_in_punctuation_class_selects_the_contiguous_punctuation_run() {
        // `iw` groups same-class chars; `(`, `.`, `)` are all punctuation.
        let s = "foo(...)bar";
        let r = text_object(s, 4, true, 'w').unwrap();
        assert_eq!(slice(s, r), "(...)");
    }

    #[test]
    fn capital_iw_treats_word_plus_punctuation_as_a_single_word() {
        let s = "foo(bar) baz";
        let r = text_object(s, 0, true, 'W').unwrap();
        assert_eq!(slice(s, r), "foo(bar)");
    }

    #[test]
    fn iw_on_an_empty_buffer_selects_nothing() {
        assert_eq!(text_object("", 0, true, 'w'), Some((0, 0)));
    }

    #[test]
    fn aw_leaves_the_indent_alone() {
        let s = "  word";
        let r = text_object(s, 2, false, 'w').unwrap();
        assert_eq!(slice(s, r), "word");
    }

    // ── quote: i" / a" ───────────────────────────────────────────────────

    #[test]
    fn i_quote_selects_inside_quotes_only() {
        let s = r#"x = "hello" end"#;
        let r = text_object(s, 6, true, '"').unwrap();
        assert_eq!(slice(s, r), "hello");
    }

    #[test]
    fn a_quote_includes_trailing_whitespace_when_present() {
        // vim: `a"` includes trailing whitespace if any, else leading.
        let s = r#"x = "hello" end"#;
        let r = text_object(s, 6, false, '"').unwrap();
        assert_eq!(slice(s, r), "\"hello\" ");
    }

    #[test]
    fn a_quote_falls_back_to_leading_whitespace_at_eol() {
        let s = r#"x = "hello""#;
        let r = text_object(s, 6, false, '"').unwrap();
        assert_eq!(slice(s, r), " \"hello\"");
    }

    #[test]
    fn quote_text_object_finds_inner_string_under_cursor() {
        let s = r#""a" "b" "c""#;
        let r = text_object(s, 5, true, '"').unwrap();
        assert_eq!(slice(s, r), "b");
    }

    #[test]
    fn quote_text_object_on_whitespace_between_strings_selects_the_gap() {
        // Quirk of vim's prev-then-next algorithm: cursor in the space
        // between two quoted strings treats the closing+opening quote pair
        // as a "string". `i"` yields the lone space character.
        let s = r#""a" "b" "c""#;
        let r = text_object(s, 3, true, '"').unwrap();
        assert_eq!(slice(s, r), " ");
    }

    #[test]
    fn quote_text_object_respects_backslash_escape() {
        // `\"` is escaped, not a closing quote (vim's default 'quoteescape').
        let s = r#"x = "a\"b" y"#;
        let r = text_object(s, 6, true, '"').unwrap();
        assert_eq!(slice(s, r), r#"a\"b"#);
    }

    #[test]
    fn quote_text_object_returns_none_when_no_quotes_on_line() {
        let s = "no quotes here";
        assert_eq!(text_object(s, 3, true, '"'), None);
    }

    #[test]
    fn quote_text_object_does_not_cross_line_boundaries() {
        let s = "\"a\nb\"";
        // cursor on `a` - only the open quote is on this line.
        assert_eq!(text_object(s, 1, true, '"'), None);
    }

    #[test]
    fn q_alias_selects_the_nearest_quoted_string() {
        let s = r#""a" `b` 'c'"#;
        let r = text_object(s, 5, true, 'q').unwrap();
        assert_eq!(slice(s, r), "b");
    }

    #[test]
    fn is_and_as_select_sentences() {
        let s = "One two.  Three four.  Five.";
        let r = text_object(s, 12, true, 's').unwrap();
        assert_eq!(slice(s, r), "Three four.");
        let r = text_object(s, 12, false, 's').unwrap();
        assert_eq!(slice(s, r), "Three four.  ");
    }

    #[test]
    fn tag_text_objects_select_inner_and_outer_html_tags() {
        let s = r#"x <div class="a"><span>hello</span></div> y"#;
        let inner = text_object(s, 25, true, 't').unwrap();
        assert_eq!(slice(s, inner), "hello");
        let outer = text_object(s, 25, false, 't').unwrap();
        assert_eq!(slice(s, outer), "<span>hello</span>");
    }

    #[test]
    fn tag_text_object_handles_nested_same_name_tags() {
        let s = "<div>outer <div>inner</div> tail</div>";
        let inner = text_object(s, 17, false, 't').unwrap();
        assert_eq!(slice(s, inner), "<div>inner</div>");
        let outer = text_object(s, 30, false, 't').unwrap();
        assert_eq!(slice(s, outer), s);
    }

    #[test]
    fn delimiter_text_objects_include_complete_graphemes() {
        for (text, kind) in [
            ("\"\u{301}x\"\u{301}", '"'),
            ("(\u{301}x)\u{301}", '('),
            ("<b>\u{301}x</b>\u{301}", 't'),
            ("\u{600}(x)", '('),
        ] {
            let cursor = text.find('x').unwrap();
            let inner = text_object(text, cursor, true, kind).unwrap();
            let outer = text_object(text, cursor, false, kind).unwrap();
            assert_grapheme_range(text, inner);
            assert_grapheme_range(text, outer);
            assert_eq!(slice(text, inner), "x");
            assert_eq!(slice(text, outer), text);
        }
    }

    // ── pair: i( / a( / i{ / a{ / i[ / i< ───────────────────────────────

    #[test]
    fn i_paren_selects_inside_the_innermost_pair() {
        let s = "f(a, g(b), c)";
        let r = text_object(s, 7, true, '(').unwrap();
        assert_eq!(slice(s, r), "b");
    }

    #[test]
    fn a_paren_includes_the_delimiters() {
        let s = "f(a, g(b), c)";
        let r = text_object(s, 7, false, '(').unwrap();
        assert_eq!(slice(s, r), "(b)");
    }

    #[test]
    fn b_alias_is_equivalent_to_paren() {
        let s = "(hello)";
        let r1 = text_object(s, 3, true, '(').unwrap();
        let r2 = text_object(s, 3, true, 'b').unwrap();
        assert_eq!(r1, r2);
    }

    #[test]
    fn capital_b_alias_is_equivalent_to_brace() {
        let s = "{hello}";
        let r1 = text_object(s, 3, true, '{').unwrap();
        let r2 = text_object(s, 3, true, 'B').unwrap();
        assert_eq!(r1, r2);
    }

    #[test]
    fn brace_pair_selection_respects_nesting() {
        let s = "{a {b} c}";
        let r = text_object(s, 4, true, '{').unwrap();
        assert_eq!(slice(s, r), "b");
        let r = text_object(s, 1, true, '{').unwrap();
        assert_eq!(slice(s, r), "a {b} c");
    }

    #[test]
    fn bracket_pair_spans_multiple_lines() {
        let s = "[a\nb\nc]";
        let r = text_object(s, 3, true, '[').unwrap();
        assert_eq!(slice(s, r), "a\nb\nc");
    }

    #[test]
    fn angle_pair_works_for_lt_and_gt_keys() {
        let s = "x<a>y";
        let r = text_object(s, 2, true, '<').unwrap();
        assert_eq!(slice(s, r), "a");
        let r = text_object(s, 2, false, '>').unwrap();
        assert_eq!(slice(s, r), "<a>");
    }

    #[test]
    fn pair_with_no_enclosing_open_returns_none() {
        let s = "no parens here";
        assert_eq!(text_object(s, 4, true, '('), None);
    }

    #[test]
    fn pair_with_open_but_missing_close_returns_none() {
        let s = "f(a, b, c";
        assert_eq!(text_object(s, 4, true, '('), None);
    }

    // ── paragraph: ip / ap ───────────────────────────────────────────────

    #[test]
    fn ip_selects_run_of_nonblank_lines_around_cursor() {
        let s = "a\nb\nc\n\nd\n";
        let r = text_object(s, 2, true, 'p').unwrap();
        assert_eq!(slice(s, r), "a\nb\nc\n");
    }

    #[test]
    fn ap_extends_to_trailing_blank_lines() {
        let s = "a\nb\nc\n\n\nd\n";
        let r = text_object(s, 0, false, 'p').unwrap();
        assert_eq!(slice(s, r), "a\nb\nc\n\n\n");
    }

    #[test]
    fn ip_on_a_blank_line_selects_the_blank_run() {
        let s = "a\n\n\n\nb\n";
        let r = text_object(s, 3, true, 'p').unwrap();
        assert_eq!(slice(s, r), "\n\n\n");
    }

    #[test]
    fn ap_on_blank_run_extends_to_trailing_nonblank_run() {
        // The two `\n` bytes at positions 2 and 3 each terminate one empty
        // (blank) line. Cursor sits in that blank run; `ap` glues it to the
        // following non-blank paragraph.
        let s = "a\n\n\nb\nc\n";
        let r = text_object(s, 2, false, 'p').unwrap();
        assert_eq!(slice(s, r), "\n\nb\nc\n");
    }

    #[test]
    fn ap_in_trailing_blank_run_at_eof_returns_none() {
        // vim's `current_par` returns FAIL when cursor is in a trailing
        // blank run with no following non-blank paragraph.
        let s = "a\nb\n\n\n";
        assert_eq!(text_object(s, 4, false, 'p'), None);
    }

    #[test]
    fn ap_on_nonblank_with_only_leading_blanks_extends_backward() {
        // vim: when there are no trailing blanks but leading ones exist,
        // `ap` includes the leading blank run.
        let s = "\n\na\nb";
        let r = text_object(s, 2, false, 'p').unwrap();
        assert_eq!(slice(s, r), "\n\na\nb");
    }

    #[test]
    fn ip_in_single_paragraph_buffer_selects_whole_buffer() {
        let s = "only line\nsecond line";
        let r = text_object(s, 0, true, 'p').unwrap();
        assert_eq!(slice(s, r), "only line\nsecond line");
    }

    #[test]
    fn paragraph_on_empty_buffer_selects_its_only_line() {
        assert_eq!(text_object("", 0, true, 'p'), Some((0, 0)));
    }

    #[test]
    fn ip_treats_whitespace_only_line_as_blank() {
        let s = "a\n   \n\t\nb\n";
        let r = text_object(s, 2, true, 'p').unwrap();
        assert_eq!(slice(s, r), "   \n\t\n");
    }
}
