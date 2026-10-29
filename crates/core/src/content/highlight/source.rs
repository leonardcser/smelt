//! Random-access syntax colours for one source, shared by every renderer.
//!
//! A request parses from a nearby sync point instead of the source start: an
//! unindented line up to `LOOKBACK_LINES` above it, where a fresh parse almost
//! always matches a full one. The parsed window then grows forward, so
//! scrolling continues the same parse. Caches hold scopes, not colours, so a
//! theme change recolours without parsing again.

use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::HashMap;
use std::ops::Range;

use syntect::highlighting::Highlighter;
use syntect::parsing::{ParseState, Scope, ScopeStack, SyntaxReference};

use super::{syntax_theme, InlineSyntaxSpan, SYNTAX_SET};
use crate::style::Color;

const CHECKPOINT_LINES: usize = 128;
const CANCEL_CHECK_LINES: usize = 32;
const LOOKBACK_LINES: usize = 2048;
// Without an unindented line, a window starts this far past the lookback.
const SYNC_LINES: usize = 1024;
// Windows keep at most this many lines; older ones are dropped from the front.
const MAX_WINDOW_LINES: usize = 16 * 1024;

/// Lines of one source. `None` marks a line that is not highlighted and
/// restarts parsing, such as an elided gap or an oversized line.
pub(crate) trait LineSource {
    fn line_count(&self) -> usize;
    fn line(&self, index: usize) -> Option<Cow<'_, str>>;
}

impl LineSource for [&str] {
    fn line_count(&self) -> usize {
        self.len()
    }

    fn line(&self, index: usize) -> Option<Cow<'_, str>> {
        self.get(index).map(|line| Cow::Borrowed(*line))
    }
}

/// The syntect syntax of one language.
#[derive(Clone, Copy)]
pub(crate) struct Syntax(&'static SyntaxReference);

impl Syntax {
    /// A file extension such as `rs`; unknown extensions are plain text.
    pub(crate) fn for_ext(ext: &str) -> Self {
        Self(
            SYNTAX_SET
                .find_syntax_by_extension(ext)
                .unwrap_or_else(|| SYNTAX_SET.find_syntax_plain_text()),
        )
    }

    /// A markdown fence or language token such as `rust` or `bash`.
    pub(crate) fn for_lang(lang: &str) -> Self {
        Self(
            SYNTAX_SET
                .find_syntax_by_extension(super::lang_to_ext(lang))
                .or_else(|| SYNTAX_SET.find_syntax_by_name(lang))
                .unwrap_or_else(|| SYNTAX_SET.find_syntax_plain_text()),
        )
    }

    /// A file path, by its full name (`Makefile`) or its extension.
    pub(crate) fn for_path(path: &str) -> Self {
        let path = std::path::Path::new(path);
        Self(
            [path.file_name(), path.extension()]
                .into_iter()
                .flatten()
                .filter_map(|name| SYNTAX_SET.find_syntax_by_extension(name.to_str()?))
                .next()
                .unwrap_or_else(|| SYNTAX_SET.find_syntax_plain_text()),
        )
    }

    fn is_plain(self) -> bool {
        std::ptr::eq(self.0, SYNTAX_SET.find_syntax_plain_text())
    }
}

/// Stateful, theme-independent tokenization of one source, line by line.
#[derive(Clone, Debug)]
pub(crate) struct SyntaxParser {
    parse: ParseState,
    scopes: ScopeStack,
}

impl SyntaxParser {
    pub(crate) fn new(syntax: Syntax) -> Self {
        Self {
            parse: ParseState::new(syntax.0),
            scopes: ScopeStack::new(),
        }
    }

    /// `line` includes its newline. Emitted ranges are byte offsets in it.
    pub(crate) fn line(&mut self, line: &str, mut emit: impl FnMut(Range<usize>, &[Scope])) {
        let mut start = 0;
        if let Ok(ops) = self.parse.parse_line(line, &SYNTAX_SET) {
            for (end, op) in ops {
                if start < end {
                    emit(start..end, self.scopes.as_slice());
                }
                let _ = self.scopes.apply(&op);
                start = end;
            }
        }
        if start < line.len() {
            emit(start..line.len(), self.scopes.as_slice());
        }
    }
}

/// Runs of one line: `(end byte, scope stack id)`, covering the line.
pub(crate) type LineRuns = Vec<(usize, usize)>;

/// A contiguous parsed range of lines and the parser state after it.
struct Window {
    start: usize,
    lines: Vec<LineRuns>,
    parser: SyntaxParser,
    /// Parser states before every `CHECKPOINT_LINES`th line in the window.
    checkpoints: Vec<(usize, SyntaxParser)>,
}

impl Window {
    fn new(syntax: Syntax, start: usize) -> Self {
        Self {
            start,
            lines: Vec::new(),
            parser: SyntaxParser::new(syntax),
            checkpoints: Vec::new(),
        }
    }

    fn end(&self) -> usize {
        self.start + self.lines.len()
    }

    /// Forgets lines from `line` on, resuming from the last checkpoint.
    fn truncate(&mut self, syntax: Syntax, line: usize) {
        if line >= self.end() {
            return;
        }
        let kept = self.checkpoints.partition_point(|(at, _)| *at <= line);
        self.checkpoints.truncate(kept);
        match self.checkpoints.pop() {
            Some((at, parser)) => {
                self.lines.truncate(at - self.start);
                self.parser = parser;
            }
            None => *self = Self::new(syntax, self.start),
        }
    }

    /// Drops lines before `line` to bound memory.
    fn trim_front(&mut self, line: usize) {
        let drop = line.saturating_sub(self.start).min(self.lines.len());
        self.lines.drain(..drop);
        self.start += drop;
        self.checkpoints.retain(|(at, _)| *at >= self.start);
    }
}

#[derive(Default)]
struct Cache {
    /// Line count and last line length the window was parsed from.
    key: (usize, usize),
    window: Option<Window>,
    stacks: Vec<Vec<Scope>>,
    ids: HashMap<Vec<Scope>, usize>,
}

/// Syntax colours for random line ranges of one source. Keep one per source
/// so later requests reuse its parse; content may only grow at its end. The
/// syntect parser state is not `Send`, so a highlighter stays on its thread.
pub(crate) struct SourceHighlighter {
    syntax: Syntax,
    cache: RefCell<Cache>,
}

impl std::fmt::Debug for SourceHighlighter {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SourceHighlighter")
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
thread_local! {
    /// Source lines parsed on this thread, so tests can observe cache reuse.
    pub(crate) static PARSED_LINES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

impl SourceHighlighter {
    pub(crate) fn new(syntax: Syntax) -> Self {
        Self {
            syntax,
            cache: RefCell::default(),
        }
    }

    /// Theme-independent bytes retained by the parse cache.
    pub(crate) fn retained_bytes(&self) -> usize {
        let cache = self.cache.borrow();
        let stacks: usize = cache
            .stacks
            .iter()
            .map(|stack| 2 * stack.capacity() * std::mem::size_of::<Scope>())
            .sum();
        stacks
            + cache.window.as_ref().map_or(0, |window| {
                window.checkpoints.capacity() * std::mem::size_of::<(usize, SyntaxParser)>()
                    + window
                        .lines
                        .iter()
                        .map(|runs| runs.capacity() * std::mem::size_of::<(usize, usize)>())
                        .sum::<usize>()
            })
    }

    /// Coloured byte spans of each line in `range`, relative to the line.
    /// Lines that `source` does not highlight come back empty.
    pub(crate) fn spans(
        &self,
        source: &(impl LineSource + ?Sized),
        range: Range<usize>,
        theme: &crate::theme::Theme,
    ) -> Vec<Vec<InlineSyntaxSpan>> {
        self.scopes(source, range, &mut || false)
            .unwrap_or_default()
            .spans(theme)
    }

    /// Scopes of each line in `range`, or `None` if `cancel` stopped the
    /// parse. Progress made before a cancel is kept for the next request.
    pub(crate) fn scopes(
        &self,
        source: &(impl LineSource + ?Sized),
        range: Range<usize>,
        cancel: &mut dyn FnMut() -> bool,
    ) -> Option<ScopedLines> {
        let range = range.start.min(source.line_count())..range.end.min(source.line_count());
        if range.is_empty() {
            return Some(ScopedLines::default());
        }
        if self.syntax.is_plain() {
            return Some(ScopedLines {
                lines: range
                    .map(|index| {
                        source
                            .line(index)
                            .filter(|line| !line.is_empty())
                            .map_or_else(Vec::new, |line| vec![(line.len(), 0)])
                    })
                    .collect(),
                stacks: vec![vec![plain_scope()]],
            });
        }
        let mut cache = self.cache.borrow_mut();
        if !self.parse(&mut cache, source, range.clone(), cancel) {
            return None;
        }
        // Renumber the cached stacks so the result holds only those it uses.
        let mut ids = vec![None; cache.stacks.len()];
        let mut stacks = Vec::new();
        let window = cache.window.as_ref().unwrap();
        let lines = window.lines[range.start - window.start..range.end - window.start]
            .iter()
            .map(|runs| {
                runs.iter()
                    .map(|&(end, id)| {
                        let id = *ids[id].get_or_insert_with(|| {
                            stacks.push(cache.stacks[id].clone());
                            stacks.len() - 1
                        });
                        (end, id)
                    })
                    .collect()
            })
            .collect();
        Some(ScopedLines { lines, stacks })
    }

    /// Makes the window cover `range`, continuing the current parse when the
    /// request is within reach of it. Returns `false` if `cancel` stopped it.
    fn parse(
        &self,
        cache: &mut Cache,
        source: &(impl LineSource + ?Sized),
        range: Range<usize>,
        cancel: &mut dyn FnMut() -> bool,
    ) -> bool {
        let count = source.line_count();
        let key = (count, source.line(count - 1).map_or(0, |line| line.len()));
        if cache.key != key {
            // Only the previous last line may have changed.
            if let Some(window) = &mut cache.window {
                if count < cache.key.0 {
                    cache.window = None;
                } else {
                    window.truncate(self.syntax, cache.key.0.saturating_sub(1));
                }
            }
            cache.key = key;
        }
        let start = sync_start(source, range.start);
        let reusable = cache
            .window
            .as_ref()
            .is_some_and(|window| window.start <= range.start && start <= window.end());
        if !reusable {
            cache.window = Some(Window::new(self.syntax, start));
        }
        let Cache {
            window,
            stacks,
            ids,
            ..
        } = cache;
        let window = window.as_mut().unwrap();
        let mut text = String::new();
        while window.end() < range.end {
            let index = window.end();
            if index.is_multiple_of(CANCEL_CHECK_LINES) && cancel() {
                return false;
            }
            if index.is_multiple_of(CHECKPOINT_LINES) {
                window.checkpoints.push((index, window.parser.clone()));
            }
            let Some(line) = source.line(index) else {
                window.parser = SyntaxParser::new(self.syntax);
                window.lines.push(LineRuns::new());
                continue;
            };
            text.clear();
            text.push_str(&line);
            text.push('\n');
            #[cfg(test)]
            PARSED_LINES.with(|count| count.set(count.get() + 1));
            let mut runs = LineRuns::new();
            window.parser.line(&text, |bytes, stack| {
                let end = bytes.end.min(line.len());
                if bytes.start >= end {
                    return;
                }
                let id = match ids.get(stack) {
                    Some(id) => *id,
                    None => {
                        stacks.push(stack.to_vec());
                        ids.insert(stack.to_vec(), stacks.len() - 1);
                        stacks.len() - 1
                    }
                };
                match runs.last_mut() {
                    Some(last) if last.1 == id => last.0 = end,
                    _ => runs.push((end, id)),
                }
            });
            window.lines.push(runs);
        }
        if window.lines.len() > MAX_WINDOW_LINES {
            window.trim_front(range.start.min(window.end() - MAX_WINDOW_LINES));
        }
        true
    }
}

/// Where a parse for `line` starts: back to an unindented line at least
/// `LOOKBACK_LINES` above, never across a line that restarts parsing.
fn sync_start(source: &(impl LineSource + ?Sized), line: usize) -> usize {
    let mut start = line;
    while start > 0 && source.line(start - 1).is_some() {
        let distance = line - start;
        if distance >= LOOKBACK_LINES + SYNC_LINES
            || (distance >= LOOKBACK_LINES
                && source
                    .line(start)
                    .is_some_and(|text| text.starts_with(|c: char| !c.is_whitespace())))
        {
            break;
        }
        start -= 1;
    }
    start
}

/// Theme-independent syntax of some lines: per line, `(end byte, stack id)`
/// runs covering it, where ids index `stacks`. Lines that are not highlighted
/// have no runs.
#[derive(Debug, Default)]
pub(crate) struct ScopedLines {
    pub(crate) lines: Vec<LineRuns>,
    pub(crate) stacks: Vec<Vec<Scope>>,
}

impl ScopedLines {
    /// Coloured byte spans of each line in the active syntax theme.
    pub(crate) fn spans(&self, theme: &crate::theme::Theme) -> Vec<Vec<InlineSyntaxSpan>> {
        let colors = stack_colors(&self.stacks, theme);
        self.lines
            .iter()
            .map(|runs| {
                let mut start = 0;
                span_list(runs.iter().map(|&(end, id)| {
                    let span = (start..end, colors[id]);
                    start = end;
                    span
                }))
            })
            .collect()
    }
}

/// The foreground of each scope stack in the active syntax theme.
pub(crate) fn stack_colors(stacks: &[Vec<Scope>], theme: &crate::theme::Theme) -> Vec<Color> {
    let highlighter = Highlighter::new(syntax_theme(theme));
    stacks
        .iter()
        .map(|stack| {
            let fg = highlighter.style_for_stack(stack).foreground;
            Color::Rgb {
                r: fg.r,
                g: fg.g,
                b: fg.b,
            }
        })
        .collect()
}

/// Colours a single line with no surrounding source.
pub(crate) fn line_spans(
    syntax: Syntax,
    line: &str,
    theme: &crate::theme::Theme,
) -> Vec<InlineSyntaxSpan> {
    SourceHighlighter::new(syntax)
        .spans(&[line][..], 0..1, theme)
        .pop()
        .unwrap_or_default()
}

fn plain_scope() -> Scope {
    Scope::new("text.plain").unwrap()
}

/// The colour of unhighlighted text in the active syntax theme.
pub(crate) fn plain_foreground(theme: &crate::theme::Theme) -> Color {
    stack_colors(&[vec![plain_scope()]], theme)[0]
}

/// Coalesces adjacent spans of one colour and drops empty ones.
fn span_list(spans: impl Iterator<Item = (Range<usize>, Color)>) -> Vec<InlineSyntaxSpan> {
    let mut out: Vec<InlineSyntaxSpan> = Vec::new();
    for (bytes, fg) in spans {
        if bytes.is_empty() {
            continue;
        }
        let Color::Rgb { r, g, b } = fg else {
            continue;
        };
        match out.last_mut() {
            Some(last) if last.foreground == [r, g, b] && last.byte_end == bytes.start => {
                last.byte_end = bytes.end;
            }
            _ => out.push(InlineSyntaxSpan {
                byte_start: bytes.start,
                byte_end: bytes.end,
                foreground: [r, g, b],
            }),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn theme() -> crate::theme::Theme {
        crate::theme::Theme::default()
    }

    /// Colours of every byte, for comparing span lists that split differently.
    fn byte_colors(line: &str, spans: &[InlineSyntaxSpan]) -> Vec<Option<[u8; 3]>> {
        let mut colors = vec![None; line.len()];
        for span in spans {
            colors[span.byte_start..span.byte_end].fill(Some(span.foreground));
        }
        colors
    }

    fn full(syntax: Syntax, lines: &[&str]) -> Vec<Vec<InlineSyntaxSpan>> {
        SourceHighlighter::new(syntax).spans(lines, 0..lines.len(), &theme())
    }

    #[test]
    fn random_access_matches_a_full_parse() {
        let mut lines = Vec::new();
        for i in 0..3000 {
            lines.push(format!("fn f{i}() -> u32 {{"));
            if i == 1500 {
                lines.push("    /* a comment".into());
                lines.extend((0..2400).map(|_| "       spanning many lines".to_string()));
                lines.push("    */".into());
            }
            lines.push(format!("    let x = \"{i}\"; // note"));
            lines.push("    x.len() as u32".into());
            lines.push("}".into());
        }
        let lines: Vec<&str> = lines.iter().map(String::as_str).collect();
        {
            let syntax = Syntax::for_ext("rs");
            let expected = full(syntax, &lines);
            let highlighter = SourceHighlighter::new(syntax);
            for start in [13_000, 8_350, 8_420, 6_050, 200, 14_390, 8_100] {
                let got = highlighter.spans(&lines[..], start..start + 10, &theme());
                for (offset, spans) in got.iter().enumerate() {
                    let line = start + offset;
                    assert_eq!(
                        byte_colors(lines[line], spans),
                        byte_colors(lines[line], &expected[line]),
                        "line {line}: {}",
                        lines[line]
                    );
                }
            }
        }
    }

    #[test]
    fn gaps_restart_parsing_and_render_empty() {
        struct Gapped<'a>(&'a [&'a str]);
        impl LineSource for Gapped<'_> {
            fn line_count(&self) -> usize {
                self.0.len()
            }
            fn line(&self, index: usize) -> Option<Cow<'_, str>> {
                (self.0[index] != "...").then(|| Cow::Borrowed(self.0[index]))
            }
        }
        let lines = ["/* open", "...", "let x = 1;"];
        {
            let syntax = Syntax::for_ext("rs");
            let spans = SourceHighlighter::new(syntax).spans(&Gapped(&lines), 0..3, &theme());
            assert!(spans[1].is_empty());
            let expected = SourceHighlighter::new(syntax)
                .spans(&["let x = 1;"][..], 0..1, &theme())
                .pop()
                .unwrap();
            assert!(expected.len() > 1);
            assert_eq!(spans[2], expected);
        }
    }

    #[test]
    fn scrolling_and_growing_continue_the_parse() {
        let lines: Vec<String> = (0..10_000).map(|i| format!("let x{i} = {i};")).collect();
        let mut lines: Vec<&str> = lines.iter().map(String::as_str).collect();
        let highlighter = SourceHighlighter::new(Syntax::for_ext("rs"));
        let parsed = |range: Range<usize>, lines: &[&str]| {
            PARSED_LINES.with(|count| count.set(0));
            highlighter.spans(lines, range, &theme());
            PARSED_LINES.with(std::cell::Cell::get)
        };
        assert_eq!(parsed(8_000..8_040, &lines), 8_040 - 5_952);
        assert_eq!(parsed(8_020..8_060, &lines), 20);
        assert_eq!(parsed(7_000..7_040, &lines), 0);
        assert_eq!(parsed(3_000..3_040, &lines), 2_088);
        lines.push("let tail = 1;");
        assert_eq!(parsed(9_990..10_001, &lines), 10_001 - 7_942);
        lines.push("let more = 2;");
        assert!(parsed(9_990..10_002, &lines) <= 2 + CHECKPOINT_LINES);
    }

    #[test]
    fn growing_last_line_invalidates_the_window() {
        let highlighter = SourceHighlighter::new(Syntax::for_ext("rs"));
        let before = highlighter.spans(&["let x = \"open"][..], 0..1, &theme());
        let after = highlighter.spans(&["let x = \"open\"; let y = 2;"][..], 0..1, &theme());
        assert_ne!(before, after);
        assert_eq!(
            after,
            full(Syntax::for_ext("rs"), &["let x = \"open\"; let y = 2;"])
        );
    }

    #[test]
    fn plain_text_and_unknown_languages_use_one_colour() {
        for syntax in [Syntax::for_ext("txt"), Syntax::for_lang("no-such-language")] {
            let spans = line_spans(syntax, "plain é 界", &theme());
            assert_eq!(spans.len(), 1);
            assert_eq!(
                (spans[0].byte_start, spans[0].byte_end),
                (0, "plain é 界".len())
            );
        }
        assert!(line_spans(Syntax::for_ext("txt"), "", &theme()).is_empty());
    }
}
