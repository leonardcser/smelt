//! Live code search over a workspace file list. Files are searched in
//! parallel, results publish in file order as soon as every earlier file is
//! done, and dropping a search cancels it.

use std::collections::BTreeMap;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use smelt_buffer::buffer::{LineDecoration, Span, SpanMeta};
use smelt_buffer::document::{DocumentRow, DocumentSnapshot, RowSource};
use smelt_buffer::theme::{intern, intern_anonymous_style, HlGroup, Theme};
use tokio::sync::mpsc::UnboundedSender;

use crate::content::file_icons::{lookup_path_with_color, FileIconOptions};
use crate::content::highlight::source::{line_spans, Syntax};
use smelt_buffer::text::{display_text, DisplayOffsets};

/// Display text kept around a match on very long lines, in bytes.
const MAX_LINE_BYTES: usize = 512;
/// How often a running search publishes progress.
const PUBLISH_INTERVAL: Duration = Duration::from_millis(16);

/// What a search looks for. Literal by default; case-insensitive unless the
/// query has an uppercase letter.
#[derive(Clone, Debug)]
pub struct Pattern(regex::bytes::Regex);

impl Pattern {
    pub fn new(query: &str, regex: bool) -> Result<Self, String> {
        if query.is_empty() {
            return Err("empty search".to_string());
        }
        let source = if regex {
            query.to_string()
        } else {
            regex::escape(query)
        };
        regex::bytes::RegexBuilder::new(&source)
            .case_insensitive(!query.chars().any(char::is_uppercase))
            .multi_line(true)
            .build()
            .map(Self)
            .map_err(|err| err.to_string())
    }

    /// Non-empty byte ranges of matches in one line. Unicode patterns only
    /// match whole characters, so the ranges fall on character boundaries.
    pub fn find_in_line(&self, line: &str) -> Vec<Range<usize>> {
        self.0
            .find_iter(line.as_bytes())
            .map(|found| found.range())
            .filter(|range| !range.is_empty())
            .collect()
    }
}

/// One line with at least one match.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Match {
    /// Index into the searched file list.
    pub file: usize,
    /// 1-based line number.
    pub line: u32,
    /// 1-based character column of the first match.
    pub column: u32,
    /// The line, trimmed of indentation and clipped around the first match
    /// when very long.
    pub text: String,
    /// Byte ranges of the matches within `text`.
    pub ranges: Vec<Range<usize>>,
}

#[derive(Clone, Debug, Default)]
pub struct Progress {
    /// Matching lines in file order, at most the search limit.
    pub matches: Vec<Match>,
    /// Matching lines found so far, including those past the limit.
    pub total: usize,
    pub files_searched: usize,
    pub files_total: usize,
    pub done: bool,
    /// Changes whenever anything above does.
    pub version: u64,
}

struct Shared {
    progress: Mutex<Progress>,
    cancel: AtomicBool,
    wakeup: Option<UnboundedSender<()>>,
}

/// How a search shows its matches as document rows.
#[derive(Clone, Debug, Default)]
pub struct Presentation {
    /// The first match is the last row, as in a picker whose input sits
    /// below its results. Short lists are filled with blank rows above.
    pub reverse: bool,
    /// File icons before each path, or none.
    pub icons: Option<FileIconOptions>,
}

/// What the document shows: matches as of the caller's last `sync`, so rows
/// only move when it can keep its selection in step.
#[derive(Default)]
struct View {
    rows: usize,
    fill: usize,
    generation: u64,
}

/// A running search. Dropping it stops the search.
pub struct Search {
    root: PathBuf,
    files: Arc<Vec<String>>,
    presentation: Presentation,
    view: Mutex<View>,
    shared: Arc<Shared>,
}

impl Drop for Search {
    fn drop(&mut self) {
        self.shared.cancel.store(true, Ordering::Release);
    }
}

impl Search {
    /// Start searching `files`, relative to `root`, keeping at most `limit`
    /// matching lines.
    pub fn start(
        root: PathBuf,
        files: Arc<Vec<String>>,
        pattern: Pattern,
        limit: usize,
        presentation: Presentation,
        wakeup: Option<UnboundedSender<()>>,
    ) -> Self {
        let shared = Arc::new(Shared {
            progress: Mutex::new(Progress {
                files_total: files.len(),
                ..Default::default()
            }),
            cancel: AtomicBool::new(false),
            wakeup,
        });
        let job = Job {
            root: root.clone(),
            files: Arc::clone(&files),
            pattern,
            limit,
            shared: Arc::clone(&shared),
        };
        if std::thread::Builder::new()
            .name("smelt-code-search".into())
            .spawn(move || job.run())
            .is_err()
        {
            let mut progress = shared.progress.lock().unwrap();
            progress.done = true;
            progress.version += 1;
        }
        Self {
            root,
            files,
            presentation,
            view: Mutex::default(),
            shared,
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn path(&self, file: usize) -> Option<&str> {
        self.files.get(file).map(String::as_str)
    }

    pub fn cancel(&self) {
        self.shared.cancel.store(true, Ordering::Release);
    }

    /// Reads the current progress without copying it.
    pub fn with_progress<T>(&self, read: impl FnOnce(&Progress) -> T) -> T {
        read(&self.shared.progress.lock().unwrap())
    }

    /// Show every match found so far in the document. Returns how many
    /// rows that added above the existing ones, which a reversed document
    /// grows by, so a caller can keep its cursor and scroll on the same rows.
    pub fn sync(&self) -> usize {
        let rows = self.with_progress(|progress| progress.matches.len());
        let mut view = self.view.lock().unwrap();
        if view.rows == rows {
            return 0;
        }
        let before = Self::total(&view, self.presentation.reverse);
        view.rows = rows;
        view.generation += 1;
        if self.presentation.reverse {
            Self::total(&view, true) - before
        } else {
            0
        }
    }

    /// Keep at least `rows` rows, so a reversed list sits at the bottom.
    /// Returns how many rows that added above the existing ones; negative
    /// when it removed some.
    pub fn fill(&self, rows: usize) -> isize {
        let mut view = self.view.lock().unwrap();
        if view.fill == rows {
            return 0;
        }
        let before = Self::total(&view, self.presentation.reverse);
        view.fill = rows;
        view.generation += 1;
        Self::total(&view, self.presentation.reverse) as isize - before as isize
    }

    fn total(view: &View, reverse: bool) -> usize {
        if reverse {
            view.rows.max(view.fill)
        } else {
            view.rows
        }
    }

    /// Index of the match on document `row`, or `None` for fill rows.
    pub fn match_at(&self, row: usize) -> Option<usize> {
        let view = self.view.lock().unwrap();
        self.match_in(&view, row)
    }

    fn match_in(&self, view: &View, row: usize) -> Option<usize> {
        let total = Self::total(view, self.presentation.reverse);
        if row >= total {
            return None;
        }
        if !self.presentation.reverse {
            return Some(row);
        }
        (total - 1 - row < view.rows).then(|| total - 1 - row)
    }

    /// Document row of match `index`.
    pub fn row_of(&self, index: usize) -> Option<usize> {
        let view = self.view.lock().unwrap();
        (index < view.rows).then(|| {
            if self.presentation.reverse {
                Self::total(&view, true) - 1 - index
            } else {
                index
            }
        })
    }
}

/// One row per matching line: `path:line:column: text`, with the text in
/// its file's syntax colours and the matches highlighted.
impl RowSource for Search {
    fn snapshot(&self) -> DocumentSnapshot {
        let view = self.view.lock().unwrap();
        DocumentSnapshot {
            generation: view.generation,
            total_rows: Self::total(&view, self.presentation.reverse) as u64,
        }
    }

    fn rows(&self, range: Range<u64>, theme: &Theme) -> Vec<DocumentRow> {
        let indices: Vec<Option<usize>> = {
            let view = self.view.lock().unwrap();
            let total = Self::total(&view, self.presentation.reverse) as u64;
            (range.start.min(total)..range.end.min(total))
                .map(|row| self.match_in(&view, row as usize))
                .collect()
        };
        let matches = self.with_progress(|progress| {
            indices
                .iter()
                .map(|index| index.and_then(|index| progress.matches.get(index).cloned()))
                .collect::<Vec<_>>()
        });
        matches
            .iter()
            .map(|found| {
                found
                    .as_ref()
                    .map_or_else(DocumentRow::default, |found| self.row(found, theme))
            })
            .collect()
    }
}

impl Search {
    fn row(&self, found: &Match, theme: &Theme) -> DocumentRow {
        let path = self.path(found.file).unwrap_or_default();
        let mut text = String::new();
        let mut spans = Vec::new();
        if let Some(options) = &self.presentation.icons {
            if let Some((icon, color)) = lookup_path_with_color(Path::new(path), options) {
                text.push_str(icon);
                let hl = color.map_or(intern("Normal"), |fg| {
                    intern_anonymous_style(crate::style::Style {
                        fg: Some(fg),
                        ..Default::default()
                    })
                });
                spans.push(span(&text, 0, text.len(), hl));
                text.push(' ');
            }
        }
        let path_start = text.len();
        text.push_str(&display_text(path));
        let path_end = text.len();
        text.push_str(&format!(":{}:{}: ", found.line, found.column));
        let body = text.len();
        text.push_str(&display_text(&found.text));
        spans.push(span(&text, path_start, path_end, intern("Directory")));
        spans.push(span(&text, path_end, body, intern("Comment")));
        let mut offsets = DisplayOffsets::default();
        let mut at = |byte| body + offsets.display(&found.text, byte);
        for syntax in line_spans(Syntax::for_path(path), &found.text, theme) {
            let (start, end) = (at(syntax.byte_start), at(syntax.byte_end));
            spans.push(span(
                &text,
                start,
                end,
                intern_anonymous_style(crate::style::Style {
                    fg: Some(syntax.foreground),
                    ..Default::default()
                }),
            ));
        }
        for range in &found.ranges {
            let (start, end) = (at(range.start), at(range.end));
            spans.push(span(&text, start, end, intern("Search")));
        }
        DocumentRow {
            text,
            spans,
            decoration: LineDecoration::default(),
        }
    }
}

/// A span over bytes `start..end` of `text`, in cells.
fn span(text: &str, start: usize, end: usize, hl: HlGroup) -> Span {
    let cell = |byte| smelt_buffer::text::byte_to_cell(text, byte).min(u16::MAX as usize) as u16;
    Span {
        col_start: cell(start),
        col_end: cell(end),
        hl,
        meta: SpanMeta::default(),
        hl_eol: false,
        on_cursor_row: false,
    }
}

struct Job {
    root: PathBuf,
    files: Arc<Vec<String>>,
    pattern: Pattern,
    limit: usize,
    shared: Arc<Shared>,
}

struct FileResult {
    matches: Vec<Match>,
    total: usize,
}

impl Job {
    fn run(self) {
        let next = AtomicUsize::new(0);
        // Set once `limit` ordered matches exist; later files only count.
        let full = AtomicBool::new(false);
        let (tx, rx) = mpsc::channel::<(usize, FileResult)>();
        let threads = std::thread::available_parallelism()
            .map_or(4, usize::from)
            .clamp(1, 16);
        std::thread::scope(|scope| {
            let job = &self;
            for _ in 0..threads {
                let tx = tx.clone();
                let (next, full) = (&next, &full);
                scope.spawn(move || {
                    let mut buf = Vec::new();
                    loop {
                        if job.shared.cancel.load(Ordering::Acquire) {
                            return;
                        }
                        let index = next.fetch_add(1, Ordering::Relaxed);
                        let Some(path) = job.files.get(index) else {
                            return;
                        };
                        let result = job.search_file(
                            index,
                            &job.root.join(path),
                            &mut buf,
                            full.load(Ordering::Relaxed),
                        );
                        if tx.send((index, result)).is_err() {
                            return;
                        }
                    }
                });
            }
            drop(tx);
            self.collect(rx, &full);
        });
    }

    /// Orders per-file results and publishes the completed prefix.
    fn collect(&self, rx: mpsc::Receiver<(usize, FileResult)>, full: &AtomicBool) {
        let mut pending = BTreeMap::new();
        let mut ordered = Vec::new();
        let (mut next, mut total) = (0, 0);
        let mut published = Instant::now();
        loop {
            let received = rx.recv_timeout(PUBLISH_INTERVAL);
            if self.shared.cancel.load(Ordering::Acquire) {
                return;
            }
            let finished = match received {
                Ok((index, result)) => {
                    pending.insert(index, result);
                    false
                }
                Err(mpsc::RecvTimeoutError::Timeout) => false,
                Err(mpsc::RecvTimeoutError::Disconnected) => true,
            };
            while let Some(result) = pending.remove(&next) {
                next += 1;
                total += result.total;
                let room = self.limit.saturating_sub(ordered.len());
                ordered.extend(result.matches.into_iter().take(room));
                if ordered.len() >= self.limit {
                    full.store(true, Ordering::Relaxed);
                }
            }
            if finished || published.elapsed() >= PUBLISH_INTERVAL {
                published = Instant::now();
                {
                    let mut progress = self.shared.progress.lock().unwrap();
                    progress.matches.append(&mut ordered);
                    progress.total = total;
                    progress.files_searched = next;
                    progress.done = finished;
                    progress.version += 1;
                }
                if let Some(wakeup) = &self.shared.wakeup {
                    let _ = wakeup.send(());
                }
            }
            if finished {
                return;
            }
        }
    }

    fn search_file(
        &self,
        file: usize,
        path: &Path,
        buf: &mut Vec<u8>,
        count_only: bool,
    ) -> FileResult {
        let mut result = FileResult {
            matches: Vec::new(),
            total: 0,
        };
        buf.clear();
        let Ok(mut handle) = std::fs::File::open(path) else {
            return result;
        };
        if std::io::Read::read_to_end(&mut handle, buf).is_err() || memchr::memchr(0, buf).is_some()
        {
            return result;
        }
        let haystack = buf.as_slice();
        // The line holding the latest match, and the matches found on it.
        let (mut line, mut number) = (0..0, 0u32);
        let mut ranges = Vec::new();
        for found in self.pattern.0.find_iter(haystack) {
            if found.is_empty() {
                continue;
            }
            if ranges.is_empty() || found.start() > line.end {
                self.push_line(
                    &mut result,
                    file,
                    &haystack[line.clone()],
                    number,
                    &mut ranges,
                    count_only,
                );
                let start = memchr::memrchr(b'\n', &haystack[line.end..found.start()])
                    .map_or(line.end, |at| line.end + at + 1);
                number += memchr::memchr_iter(b'\n', &haystack[line.end..start]).count() as u32
                    + u32::from(number == 0);
                let end = memchr::memchr(b'\n', &haystack[start..])
                    .map_or(haystack.len(), |at| start + at);
                line = start..end;
            }
            ranges.push(found.start() - line.start..found.end().min(line.end) - line.start);
        }
        self.push_line(
            &mut result,
            file,
            &haystack[line],
            number,
            &mut ranges,
            count_only,
        );
        result
    }

    /// Records the matching line `number`, if `ranges` holds any matches.
    fn push_line(
        &self,
        result: &mut FileResult,
        file: usize,
        line: &[u8],
        number: u32,
        ranges: &mut Vec<Range<usize>>,
        count_only: bool,
    ) {
        if ranges.is_empty() {
            return;
        }
        result.total += 1;
        if !count_only && result.matches.len() < self.limit {
            let line = String::from_utf8_lossy(line);
            let line = line.strip_suffix('\r').unwrap_or(&line);
            result.matches.push(clip_line(file, number, line, ranges));
        }
        ranges.clear();
    }
}

/// A match row for `line`: indentation trimmed and, on very long lines, a
/// window around the first match. Ranges move with the kept text.
fn clip_line(file: usize, line_number: u32, line: &str, ranges: &[Range<usize>]) -> Match {
    let first = ranges.first().map_or(0, |range| range.start);
    let column = smelt_buffer::text::slice(line, 0..first).chars().count() as u32 + 1;
    let indent = line.len() - line.trim_start().len();
    let mut start = indent.min(first);
    let mut end = line.len();
    if end - start > MAX_LINE_BYTES {
        start = smelt_buffer::text::snap(line, first.saturating_sub(MAX_LINE_BYTES / 4).max(start));
        end = smelt_buffer::text::snap(line, (start + MAX_LINE_BYTES).min(line.len()));
    }
    let text = smelt_buffer::text::slice(line, start..end)
        .trim_end()
        .to_string();
    let ranges = ranges
        .iter()
        .filter_map(|range| {
            let clipped = range.start.max(start).min(start + text.len()) - start
                ..range.end.min(start + text.len()).saturating_sub(start);
            (clipped.start < clipped.end).then_some(clipped)
        })
        .collect();
    Match {
        file,
        line: line_number,
        column,
        text,
        ranges,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn search(files: &[(&str, &str)], query: &str, limit: usize) -> (Vec<Match>, Progress) {
        let dir = tempfile::tempdir().unwrap();
        let mut names = Vec::new();
        for (name, text) in files {
            std::fs::write(dir.path().join(name), text).unwrap();
            names.push(name.to_string());
        }
        let search = Search::start(
            dir.path().to_path_buf(),
            Arc::new(names),
            Pattern::new(query, false).unwrap(),
            limit,
            Presentation::default(),
            None,
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        while !search.with_progress(|progress| progress.done) {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
        let progress = search.with_progress(Clone::clone);
        (progress.matches.clone(), progress)
    }

    #[test]
    fn matches_report_lines_columns_and_ranges_in_file_order() {
        let files: Vec<(String, String)> = (0..40)
            .map(|i| {
                (
                    format!("f{i:02}.rs"),
                    format!("x\n  let task = async_task();\n{}", "y\n".repeat(i)),
                )
            })
            .collect();
        let files: Vec<(&str, &str)> = files
            .iter()
            .map(|(a, b)| (a.as_str(), b.as_str()))
            .collect();
        let (matches, progress) = search(&files, "task", 1000);
        assert_eq!(progress.total, 40);
        assert_eq!(progress.files_searched, 40);
        assert_eq!(
            matches.iter().map(|m| m.file).collect::<Vec<_>>(),
            (0..40).collect::<Vec<_>>()
        );
        let first = &matches[0];
        assert_eq!((first.line, first.column), (2, 7));
        assert_eq!(first.text, "let task = async_task();");
        assert_eq!(first.ranges, vec![4..8, 17..21]);
    }

    #[test]
    fn smart_case_and_limit_keep_counting() {
        let text = "Task\ntask\nTASK\n";
        let (matches, progress) = search(&[("a.txt", text), ("b.txt", text)], "task", 2);
        assert_eq!(progress.total, 6);
        assert_eq!(matches.len(), 2);
        let (matches, _) = search(&[("a.txt", text)], "Task", 10);
        assert_eq!(matches.iter().map(|m| m.line).collect::<Vec<_>>(), [1]);
    }

    #[test]
    fn binary_files_and_long_lines() {
        let long = format!("{}needle{}", "é".repeat(2000), "z".repeat(2000));
        let (matches, _) = search(&[("bin", "needle\0"), ("long.txt", &long)], "needle", 10);
        assert_eq!(matches.len(), 1);
        let row = &matches[0];
        assert_eq!(row.column, 2001);
        assert!(row.text.len() <= MAX_LINE_BYTES);
        assert_eq!(&row.text[row.ranges[0].clone()], "needle");
    }

    #[test]
    fn regex_patterns_and_errors() {
        assert!(Pattern::new("(", true).is_err());
        assert!(Pattern::new("", false).is_err());
        let pattern = Pattern::new("fn \\w+", true).unwrap();
        assert_eq!(
            pattern.find_in_line("pub fn alpha() { fn beta() }"),
            vec![4..12, 17..24]
        );
        assert_eq!(
            Pattern::new("a.b", false).unwrap().find_in_line("axb a.b"),
            vec![4..7]
        );
    }
}
