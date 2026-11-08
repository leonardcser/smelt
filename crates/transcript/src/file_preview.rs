//! Random-access file previews. A file is read once, up to a size bound, and
//! indexed by line; rows are produced only for the lines a window shows. A
//! worker parses syntax for just those lines, so a preview costs the same at
//! the top of a small file and deep inside a large one.

use std::collections::BTreeMap;
use std::ops::Range;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use smelt_buffer::buffer::{LineDecoration, SourceLine, Span, SpanMeta};
use smelt_buffer::document::{DocumentRow, DocumentSnapshot, DocumentViewport, RowSource, ViewId};
use smelt_buffer::theme::{intern, intern_anonymous_style, HlGroup, Theme};
use syntect::parsing::Scope;
use tokio::sync::mpsc::UnboundedSender;

use crate::code_search::Pattern;
use crate::content::highlight::source::{stack_colors, LineSource, SourceHighlighter, Syntax};
use smelt_buffer::text::{display_text, DisplayOffsets};

const CHUNK_LINES: usize = 128;
const CACHE_CHUNKS: usize = 64;
/// Longer lines show as plain text, bounding parse cost on minified files.
const MAX_HIGHLIGHT_LINE: usize = 16 * 1024;
/// How long a frame that moved the viewport waits for its colours before
/// painting plain text. Parsing a screen of a fresh file fits well inside.
const PREPARE_BUDGET: Duration = Duration::from_millis(8);
/// Bytes probed for a NUL to tell binary files apart.
const BINARY_PROBE: usize = 8 * 1024;

#[derive(Debug, PartialEq, Eq)]
pub enum OpenError {
    Binary,
    Io(String),
}

/// Where to draw attention in a preview.
#[derive(Clone, Debug, Default)]
pub struct Marks {
    /// Matches of this pattern are highlighted in every shown line.
    pub pattern: Option<Pattern>,
    /// 1-based line drawn as the current line.
    pub focus: Option<u32>,
}

struct Source {
    text: String,
    lines: Vec<Range<usize>>,
}

impl Source {
    fn line_text(&self, index: usize) -> &str {
        smelt_buffer::text::slice(&self.text, self.lines[index].clone())
    }
}

impl LineSource for Source {
    fn line_count(&self) -> usize {
        self.lines.len()
    }

    fn line(&self, index: usize) -> Option<std::borrow::Cow<'_, str>> {
        let range = self.lines.get(index)?;
        (range.len() <= MAX_HIGHLIGHT_LINE).then(|| self.line_text(index).into())
    }
}

/// Theme-independent syntax of `CHUNK_LINES` lines.
struct Chunk {
    /// Per line, `(end byte, stack id)` runs.
    lines: Vec<Vec<(usize, usize)>>,
    stacks: Vec<Vec<Scope>>,
    colors: Mutex<(u64, Arc<[HlGroup]>)>,
}

impl Chunk {
    fn colors(&self, theme: &Theme) -> Arc<[HlGroup]> {
        let mut cached = self.colors.lock().unwrap();
        if cached.1.is_empty() || cached.0 != theme.revision() {
            let colors: Vec<_> = stack_colors(&self.stacks, theme)
                .into_iter()
                .map(|fg| {
                    intern_anonymous_style(crate::style::Style {
                        fg: Some(fg),
                        ..Default::default()
                    })
                })
                .collect();
            *cached = (theme.revision(), colors.into());
        }
        Arc::clone(&cached.1)
    }
}

#[derive(Default)]
struct State {
    /// Visible line range of each window.
    views: BTreeMap<ViewId, Range<usize>>,
    /// Parsed chunks by first line, with a last-used stamp.
    chunks: BTreeMap<usize, (u64, Arc<Chunk>)>,
    clock: u64,
    worker: bool,
    wakeup: Option<UnboundedSender<()>>,
}

impl State {
    fn visible(&self) -> impl Iterator<Item = usize> + '_ {
        self.views
            .values()
            .flat_map(|lines| chunk_starts(lines.clone()))
    }

    /// Chunks worth parsing ahead: one before and after each view.
    fn prefetch(&self, count: usize) -> impl Iterator<Item = usize> + '_ {
        self.views.values().flat_map(move |lines| {
            let first = lines.start / CHUNK_LINES * CHUNK_LINES;
            let after = lines.end.div_ceil(CHUNK_LINES) * CHUNK_LINES;
            [
                first.checked_sub(CHUNK_LINES),
                (after < count).then_some(after),
            ]
            .into_iter()
            .flatten()
        })
    }

    fn next(&self, count: usize) -> Option<usize> {
        self.visible()
            .chain(self.prefetch(count))
            .find(|start| !self.chunks.contains_key(start))
    }

    fn wants(&self, start: usize, count: usize) -> bool {
        self.visible()
            .chain(self.prefetch(count))
            .any(|s| s == start)
    }

    fn ready(&self) -> bool {
        self.visible().all(|start| self.chunks.contains_key(&start))
    }

    fn publish(&mut self, start: usize, chunk: Chunk, count: usize) {
        self.clock += 1;
        self.chunks.insert(start, (self.clock, Arc::new(chunk)));
        while self.chunks.len() > CACHE_CHUNKS {
            let victim = self
                .chunks
                .iter()
                .filter(|(start, _)| !self.wants(**start, count))
                .min_by_key(|(_, (stamp, _))| *stamp)
                .map(|(start, _)| *start);
            let Some(victim) = victim else { break };
            self.chunks.remove(&victim);
        }
    }
}

fn chunk_starts(lines: Range<usize>) -> impl Iterator<Item = usize> {
    let first = lines.start / CHUNK_LINES * CHUNK_LINES;
    (first..lines.end).step_by(CHUNK_LINES)
}

#[derive(Default)]
struct Shared {
    state: Mutex<State>,
    /// Wakes the worker when demand changes or the preview drops.
    changed: Condvar,
    /// Wakes a waiting frame when a chunk lands.
    published: Condvar,
    stopped: AtomicBool,
    generation: AtomicU64,
}

pub struct FilePreview {
    source: Arc<Source>,
    syntax: Syntax,
    truncated: bool,
    marks: Marks,
    shared: Arc<Shared>,
}

impl Drop for FilePreview {
    fn drop(&mut self) {
        self.shared.stopped.store(true, Ordering::Release);
        self.shared.changed.notify_all();
    }
}

impl FilePreview {
    /// Read up to `max_bytes` of `path`. Text past the bound is dropped at a
    /// line boundary and a final row says so.
    pub fn open(path: &Path, max_bytes: usize, marks: Marks) -> Result<Self, OpenError> {
        let _perf = smelt_perf::perf::begin("file_preview:open");
        let file = std::fs::File::open(path).map_err(|err| OpenError::Io(err.to_string()))?;
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(
            &mut std::io::Read::take(file, (max_bytes as u64).saturating_add(1)),
            &mut bytes,
        )
        .map_err(|err| OpenError::Io(err.to_string()))?;
        if memchr::memchr(0, &bytes[..bytes.len().min(BINARY_PROBE)]).is_some() {
            return Err(OpenError::Binary);
        }
        let truncated = bytes.len() > max_bytes;
        if truncated {
            bytes.truncate(
                memchr::memrchr(b'\n', &bytes[..max_bytes]).map_or(max_bytes, |at| at + 1),
            );
        }
        let text = match String::from_utf8(bytes) {
            Ok(text) => text,
            Err(err) => String::from_utf8_lossy(err.as_bytes()).into_owned(),
        };
        let mut lines = Vec::new();
        let mut start = 0;
        for end in memchr::memchr_iter(b'\n', text.as_bytes()) {
            let line_end = if end > start && text.as_bytes()[end - 1] == b'\r' {
                end - 1
            } else {
                end
            };
            lines.push(start..line_end);
            start = end + 1;
        }
        if start < text.len() || lines.is_empty() {
            lines.push(start..text.len());
        }
        Ok(Self {
            source: Arc::new(Source { text, lines }),
            syntax: Syntax::for_path(&path.to_string_lossy()),
            truncated,
            marks,
            shared: Arc::default(),
        })
    }

    pub fn set_wakeup(&self, wakeup: Option<UnboundedSender<()>>) {
        self.shared.state.lock().unwrap().wakeup = wakeup;
    }

    pub fn line_count(&self) -> usize {
        self.source.lines.len()
    }

    fn spawn_worker(&self, state: &mut State) {
        if state.worker || self.syntax.is_plain() {
            return;
        }
        state.worker = true;
        let (source, syntax, shared) = (
            Arc::clone(&self.source),
            self.syntax,
            Arc::clone(&self.shared),
        );
        let _ = std::thread::Builder::new()
            .name("smelt-preview-syntax".into())
            .spawn(move || run(&source, syntax, &shared));
    }

    fn row(&self, index: usize, chunk: Option<&Chunk>, theme: &Theme) -> DocumentRow {
        let source = self.source.line_text(index);
        let text = display_text(source).into_owned();
        let mut spans = Vec::new();
        let mut cells = smelt_buffer::text::CellCursor::new(&text);
        let column =
            |cells: &mut smelt_buffer::text::CellCursor, map: &mut DisplayOffsets, byte| {
                cells.cell(map.display(source, byte)).min(u16::MAX as usize) as u16
            };
        if let Some(chunk) = chunk {
            let colors = chunk.colors(theme);
            let mut map = DisplayOffsets::default();
            let mut start = 0;
            for &(end, id) in &chunk.lines[index % CHUNK_LINES] {
                let end = end.min(source.len());
                let (col_start, col_end) = (
                    column(&mut cells, &mut map, start),
                    column(&mut cells, &mut map, end),
                );
                if col_start < col_end {
                    spans.push(plain_span(col_start, col_end, colors[id]));
                }
                start = end;
            }
        }
        if let Some(pattern) = &self.marks.pattern {
            let mut map = DisplayOffsets::default();
            for range in pattern.find_in_line(source) {
                let (col_start, col_end) = (
                    column(&mut cells, &mut map, range.start),
                    column(&mut cells, &mut map, range.end),
                );
                spans.push(plain_span(col_start, col_end, intern("Search")));
            }
        }
        let lineno = index as u32 + 1;
        DocumentRow {
            text,
            spans,
            decoration: LineDecoration {
                source_line: Some(SourceLine::Linear { lineno }),
                window_bg: (self.marks.focus == Some(lineno))
                    .then(|| theme.resolve(intern("CursorLine")).bg)
                    .flatten(),
                ..Default::default()
            },
        }
    }
}

fn plain_span(col_start: u16, col_end: u16, hl: HlGroup) -> Span {
    Span {
        col_start,
        col_end,
        hl,
        meta: SpanMeta::default(),
        hl_eol: false,
        on_cursor_row: false,
    }
}

fn run(source: &Source, syntax: Syntax, shared: &Shared) {
    let highlighter = SourceHighlighter::new(syntax);
    let count = source.lines.len();
    loop {
        let start = {
            let mut state = shared.state.lock().unwrap();
            loop {
                if shared.stopped.load(Ordering::Acquire) {
                    return;
                }
                if let Some(start) = state.next(count) {
                    break start;
                }
                state = shared.changed.wait(state).unwrap();
            }
        };
        let mut cancel = || {
            shared.stopped.load(Ordering::Acquire)
                || !shared.state.lock().unwrap().wants(start, count)
        };
        let end = (start + CHUNK_LINES).min(count);
        let Some(scoped) = highlighter.scopes(source, start..end, &mut cancel) else {
            continue;
        };
        let chunk = Chunk {
            lines: scoped.lines,
            stacks: scoped.stacks,
            colors: Mutex::new((0, Arc::from([]))),
        };
        let mut state = shared.state.lock().unwrap();
        state.publish(start, chunk, count);
        shared.generation.fetch_add(1, Ordering::Release);
        shared.published.notify_all();
        if let Some(wakeup) = &state.wakeup {
            let _ = wakeup.send(());
        }
    }
}

impl RowSource for FilePreview {
    fn snapshot(&self) -> DocumentSnapshot {
        DocumentSnapshot {
            generation: self.shared.generation.load(Ordering::Acquire),
            total_rows: (self.source.lines.len() + usize::from(self.truncated)) as u64,
        }
    }

    fn prepare(&self, view: ViewId, viewport: &DocumentViewport) {
        let count = self.source.lines.len();
        let lines = viewport.rows.start.min(count as u64) as usize
            ..viewport.rows.end.min(count as u64) as usize;
        let mut state = self.shared.state.lock().unwrap();
        if state.views.get(&view) == Some(&lines) {
            return;
        }
        state.views.insert(view, lines);
        self.spawn_worker(&mut state);
        if !state.worker {
            return;
        }
        self.shared.changed.notify_all();
        let deadline = Instant::now() + PREPARE_BUDGET;
        while !state.ready() && !self.shared.stopped.load(Ordering::Acquire) {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            state = self
                .shared
                .published
                .wait_timeout(state, deadline - now)
                .unwrap()
                .0;
        }
    }

    fn release(&self, view: ViewId) {
        let mut state = self.shared.state.lock().unwrap();
        if state.views.remove(&view).is_some() {
            self.shared.changed.notify_all();
        }
    }

    fn is_pending(&self) -> bool {
        let state = self.shared.state.lock().unwrap();
        state.worker && !self.shared.stopped.load(Ordering::Acquire) && !state.ready()
    }

    fn line_number_bounds(&self) -> Option<SourceLine> {
        Some(SourceLine::Linear {
            lineno: self.source.lines.len() as u32,
        })
    }

    fn rows(&self, range: Range<u64>, theme: &Theme) -> Vec<DocumentRow> {
        let count = self.source.lines.len();
        let total = count + usize::from(self.truncated);
        let range = range.start.min(total as u64) as usize..range.end.min(total as u64) as usize;
        let lines = range.start.min(count)..range.end.min(count);
        let chunks: BTreeMap<usize, Arc<Chunk>> = {
            let mut state = self.shared.state.lock().unwrap();
            state.clock += 1;
            let stamp = state.clock;
            chunk_starts(lines.clone())
                .filter_map(|start| {
                    let (used, chunk) = state.chunks.get_mut(&start)?;
                    *used = stamp;
                    Some((start, Arc::clone(chunk)))
                })
                .collect()
        };
        let mut rows: Vec<DocumentRow> = lines
            .map(|index| {
                let chunk = chunks.get(&(index / CHUNK_LINES * CHUNK_LINES));
                self.row(index, chunk.map(Arc::as_ref), theme)
            })
            .collect();
        if range.end > count {
            rows.push(DocumentRow {
                text: "[preview truncated]".to_string(),
                spans: vec![plain_span(0, u16::MAX, intern("Comment"))],
                decoration: LineDecoration {
                    source_line: Some(SourceLine::Synthetic),
                    ..Default::default()
                },
            });
        }
        rows
    }

    fn search_text(&self, rows: Range<u64>, visit: &mut dyn FnMut(u64, Option<&str>)) -> bool {
        let count = self.source.lines.len() as u64;
        for row in rows.start..rows.end.min(count) {
            visit(
                row,
                Some(&display_text(self.source.line_text(row as usize))),
            );
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static THEME: std::sync::LazyLock<Theme> = std::sync::LazyLock::new(Theme::default);

    fn write(text: &[u8], name: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(name);
        std::fs::write(&path, text).unwrap();
        (dir, path)
    }

    fn viewport(rows: Range<u64>) -> DocumentViewport {
        DocumentViewport {
            rows,
            width: 80,
            cursor: None,
        }
    }

    #[test]
    fn deep_viewports_colour_without_parsing_the_whole_file() {
        let text: String = (0..20_000)
            .map(|i| format!("fn f{i}() {{ let x = \"{i}\"; }}\n"))
            .collect();
        let (_dir, path) = write(text.as_bytes(), "big.rs");
        let preview = Arc::new(FilePreview::open(&path, usize::MAX, Marks::default()).unwrap());
        assert_eq!(preview.snapshot().total_rows, 20_000);
        let view = smelt_buffer::document::RowView::new(preview.clone());
        let deadline = Instant::now() + Duration::from_secs(10);
        view.prepare(&viewport(15_000..15_040));
        while preview.is_pending() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
        let rows = preview.rows(15_000..15_040, &THEME);
        assert_eq!(rows.len(), 40);
        assert_eq!(rows[0].text, "fn f15000() { let x = \"15000\"; }");
        assert!(rows.iter().all(|row| row.spans.len() > 3));
        assert_eq!(
            rows[0].decoration.source_line,
            Some(SourceLine::Linear { lineno: 15_001 })
        );
        let chunks = preview.shared.state.lock().unwrap().chunks.len();
        assert!(chunks <= 3, "{chunks}");
    }

    #[test]
    fn marks_tabs_truncation_and_binary() {
        let (_dir, path) = write(b"\tlet task = 1;\nother\nlast task\n", "a.txt");
        let marks = Marks {
            pattern: Some(Pattern::new("TASK", false).unwrap()),
            focus: Some(3),
        };
        let preview = FilePreview::open(&path, 21, marks).unwrap();
        let rows = preview.rows(0..10, &THEME);
        assert_eq!(
            rows.iter().map(|row| row.text.as_str()).collect::<Vec<_>>(),
            ["    let task = 1;", "other", "[preview truncated]"]
        );
        // Case-sensitive: the pattern has an uppercase letter.
        assert!(rows[0].spans.is_empty());
        let marks = Marks {
            pattern: Some(Pattern::new("task", false).unwrap()),
            focus: Some(1),
        };
        let preview = FilePreview::open(&path, 1 << 20, marks).unwrap();
        let rows = preview.rows(0..3, &THEME);
        let search = &rows[0].spans[0];
        assert_eq!((search.col_start, search.col_end), (8, 12));
        assert_eq!(rows[2].spans.len(), 1);
        let (_dir, path) = write(b"abc\0def", "bin");
        assert_eq!(
            FilePreview::open(&path, 1 << 20, Marks::default()).err(),
            Some(OpenError::Binary)
        );
        assert!(matches!(
            FilePreview::open(&path.with_file_name("missing"), 1, Marks::default()),
            Err(OpenError::Io(_))
        ));
    }
}
