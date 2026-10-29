//! Demand-driven syntax and inline highlighting. Parser state stays on the
//! worker thread. Inline ranges publish before potentially distant syntax seeks;
//! neither loading nor rendering waits for offscreen source processing.

use smelt_buffer::document::ViewId;
use std::borrow::Cow;
use std::collections::{BTreeMap, HashSet};
use std::ops::Range;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use smelt_buffer::theme::{intern, intern_anonymous_style, HlGroup};
use syntect::parsing::Scope;
use tokio::sync::mpsc::UnboundedSender;

use super::{inline, Diff, Kind};
use crate::content::highlight::sides::{DiffSides, RowSide};
use crate::content::highlight::source::{stack_colors, Syntax};

const CHUNK_ROWS: usize = 128;
const CACHE_CHUNKS: usize = 64;
const CACHE_BYTES: usize = 8 * 1024 * 1024;
// Files whose parses stay warm, so switching back to one does not seek again.
const PARSED_FILES: usize = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Request {
    file: usize,
    start: usize,
    end: usize,
}

/// Consecutive equal scopes coalesce, including across patch prefixes.
#[derive(Debug)]
pub(super) struct SyntaxRange {
    pub end: usize,
    pub scope: usize,
}

#[derive(Debug)]
pub(super) struct Chunk {
    pub raw: Range<usize>,
    pub spans: Vec<SyntaxRange>,
    pub inline: Arc<[inline::InlineRow]>,
    syntax_ready: bool,
    scopes: Vec<Vec<Scope>>,
    colors: Mutex<(u64, Arc<[[HlGroup; 3]]>)>,
}

impl Chunk {
    fn bytes(&self) -> usize {
        self.spans.capacity() * std::mem::size_of::<SyntaxRange>()
            + self.inline.len() * std::mem::size_of::<inline::InlineRow>()
            + self
                .inline
                .iter()
                .map(|row| {
                    row.ranges.capacity()
                        * std::mem::size_of::<crate::content::highlight::diff::DiffByteRange>()
                })
                .sum::<usize>()
            + self.scopes.capacity() * std::mem::size_of::<Vec<Scope>>()
            + self
                .scopes
                .iter()
                .map(|s| s.capacity() * std::mem::size_of::<Scope>())
                .sum::<usize>()
    }

    pub fn colors(&self, theme: &crate::theme::Theme) -> Arc<[[HlGroup; 3]]> {
        let mut cached = self.colors.lock().unwrap();
        if cached.0 != theme.revision() {
            let backgrounds = ["Normal", "SmeltDiffAddBg", "SmeltDiffDeleteBg"]
                .map(|name| theme.resolve(intern(name)).bg);
            let colors: Vec<_> = stack_colors(&self.scopes, theme)
                .into_iter()
                .map(|fg| {
                    backgrounds.map(|bg| {
                        intern_anonymous_style(crate::style::Style {
                            fg: Some(fg),
                            bg,
                            ..Default::default()
                        })
                    })
                })
                .collect();
            *cached = (theme.revision(), colors.into());
        }
        Arc::clone(&cached.1)
    }

    pub fn highlight_inline(
        &self,
        raw: usize,
        row: &mut smelt_buffer::document::DocumentRow,
        group: &str,
        theme: &crate::theme::Theme,
    ) {
        let Ok(index) = self.inline.binary_search_by_key(&raw, |row| row.raw) else {
            return;
        };
        let bg = theme.resolve(intern(group)).bg.or(row.decoration.window_bg);
        let fallback = smelt_buffer::buffer::Span {
            col_start: 2,
            col_end: smelt_buffer::text::byte_to_cell(&row.text, row.text.len())
                .min(u16::MAX as usize) as u16,
            hl: intern("Normal"),
            meta: Default::default(),
            hl_eol: false,
            on_cursor_row: false,
        };
        let tokens = if row.spans.len() > 2 {
            &row.spans[2..]
        } else {
            std::slice::from_ref(&fallback)
        };
        let mut token = 0;
        let mut overlays = Vec::new();
        for range in &self.inline[index].ranges {
            let start = smelt_buffer::text::byte_to_cell(&row.text, 2 + range.start)
                .min(u16::MAX as usize) as u16;
            let end = smelt_buffer::text::byte_to_cell(&row.text, 2 + range.end)
                .min(u16::MAX as usize) as u16;
            while token < tokens.len() && tokens[token].col_end <= start {
                token += 1;
            }
            for span in tokens[token..]
                .iter()
                .take_while(|span| span.col_start < end)
            {
                let mut overlay = span.clone();
                overlay.col_start = start.max(span.col_start);
                overlay.col_end = end.min(span.col_end);
                overlay.hl = intern_anonymous_style(crate::style::Style {
                    bg,
                    ..theme.resolve(span.hl)
                });
                overlays.push(overlay);
            }
        }
        row.spans.extend(overlays);
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Demand {
    visible: Vec<Request>,
    prefetch: Vec<Request>,
}

#[derive(Debug, Default)]
struct State {
    views: BTreeMap<ViewId, Demand>,
    wanted: Vec<Request>,
    prefetch: Vec<Request>,
    attempted: HashSet<usize>,
    chunks: BTreeMap<usize, (u64, Arc<Chunk>)>,
    clock: u64,
    bytes: usize,
    wakeup: Option<UnboundedSender<()>>,
    /// The worker is waiting with no visible or speculative work left.
    idle: bool,
}

impl State {
    fn reconcile(&mut self) {
        self.wanted.clear();
        self.prefetch.clear();
        // Round-robin admission keeps the pinned cache bounded without letting
        // one large viewport consume every other live window's budget.
        for rank in 0..CACHE_CHUNKS {
            for demand in self.views.values() {
                if let Some(request) = demand.visible.get(rank) {
                    if self.wanted.len() < CACHE_CHUNKS && !self.wanted.contains(request) {
                        self.wanted.push(*request);
                    }
                }
            }
        }
        for demand in self.views.values() {
            for request in &demand.prefetch {
                if self.prefetch.len() + self.wanted.len() < CACHE_CHUNKS
                    && !self.wanted.contains(request)
                    && !self.prefetch.contains(request)
                {
                    self.prefetch.push(*request);
                }
            }
        }
        self.attempted
            .retain(|start| self.prefetch.iter().any(|r| r.start == *start));
        self.evict();
    }

    fn priority(&self, diff: &Diff, request: Request) -> (bool, bool, usize) {
        (
            !self.wanted.contains(&request),
            self.chunks.contains_key(&request.start),
            request.start - diff.files[request.file].start,
        )
    }

    fn next(&self, diff: &Diff) -> Option<Request> {
        self.pending()
            .chain(
                self.prefetch
                    .iter()
                    .filter(|r| {
                        !self.attempted.contains(&r.start)
                            && self
                                .chunks
                                .get(&r.start)
                                .is_none_or(|(_, chunk)| !chunk.syntax_ready)
                    })
                    .copied(),
            )
            .min_by_key(|r| self.priority(diff, *r))
    }

    fn pending(&self) -> impl Iterator<Item = Request> + '_ {
        self.wanted
            .iter()
            .filter(|r| {
                self.chunks
                    .get(&r.start)
                    .is_none_or(|(_, chunk)| !chunk.syntax_ready)
            })
            .copied()
    }

    fn publish(&mut self, chunk: Chunk) {
        self.clock += 1;
        self.bytes += chunk.bytes();
        if let Some((_, old)) = self
            .chunks
            .insert(chunk.raw.start, (self.clock, Arc::new(chunk)))
        {
            self.bytes -= old.bytes();
        }
        self.evict();
    }

    fn evict(&mut self) {
        while self.chunks.len() > CACHE_CHUNKS || self.bytes > CACHE_BYTES {
            // The active viewport is pinned, even if one unusually long source
            // row exceeds the cache target on its own.
            let victim = self
                .chunks
                .iter()
                .filter(|(start, _)| !self.wanted.iter().any(|r| r.start == **start))
                .min_by_key(|(_, (stamp, _))| stamp)
                .map(|(start, _)| *start);
            let Some(victim) = victim else { break };
            self.bytes -= self.chunks.remove(&victim).unwrap().1.bytes();
            if self.prefetch.iter().any(|r| r.start == victim) {
                // A speculative chunk that exceeds the remaining byte budget
                // must not be regenerated and evicted in a busy loop.
                self.attempted.insert(victim);
            }
        }
    }
}

#[derive(Debug, Default)]
struct Shared {
    state: Mutex<State>,
    changed: Condvar,
    stopped: AtomicBool,
    request: AtomicU64,
    generation: AtomicU64,
    #[cfg(test)]
    parsed_rows: AtomicU64,
}

#[derive(Debug)]
pub(super) struct SyntaxCache {
    shared: Arc<Shared>,
}

impl SyntaxCache {
    pub fn new(diff: Arc<Diff>) -> Self {
        let shared = Arc::new(Shared::default());
        let worker = Arc::clone(&shared);
        if diff.files.is_empty()
            || std::thread::Builder::new()
                .name("smelt-diff-syntax".into())
                .spawn(move || run(diff, worker))
                .is_err()
        {
            // Text and navigation remain available if no worker can be started.
            shared.stopped.store(true, Ordering::Release);
        }
        Self { shared }
    }

    pub fn set_wakeup(&self, wakeup: Option<UnboundedSender<()>>) {
        self.shared.state.lock().unwrap().wakeup = wakeup;
    }

    pub fn generation(&self) -> u64 {
        self.shared.generation.load(Ordering::Acquire)
    }

    pub fn pending(&self) -> bool {
        !self.shared.stopped.load(Ordering::Acquire)
            && self.shared.state.lock().unwrap().pending().next().is_some()
    }

    pub fn prepare(
        &self,
        view: ViewId,
        diff: &Diff,
        rows: impl Iterator<Item = usize>,
        selected: Option<usize>,
        height: usize,
    ) {
        let visible = requests(diff, rows);
        // Warm the chunks just above and below the viewport, so scrolling
        // reveals rows that are already highlighted.
        let neighbors = visible
            .first()
            .map(|first| first.start.saturating_sub(CHUNK_ROWS)..first.start)
            .into_iter()
            .chain(
                visible
                    .last()
                    .map(|last| last.end..(last.end + CHUNK_ROWS).min(diff.lines.len())),
            )
            .flatten();
        // Warm only the initial one or two chunks of adjacent files. Seeking to
        // the tail of a huge folded neighbor is not speculative viewport work.
        let adjacent_files = selected
            .into_iter()
            .flat_map(|file| {
                [file.checked_sub(1), file.checked_add(1)]
                    .into_iter()
                    .flatten()
            })
            .filter_map(|file| {
                let start = diff.files.get(file)?.start;
                let end = diff
                    .files
                    .get(file + 1)
                    .map_or(diff.lines.len(), |f| f.start);
                Some(start..end.min(start + height.clamp(CHUNK_ROWS, CHUNK_ROWS * 2)))
            })
            .flatten();
        let demand = Demand {
            visible,
            prefetch: requests(diff, neighbors.chain(adjacent_files)),
        };
        let mut state = self.shared.state.lock().unwrap();
        if state.views.get(&view) != Some(&demand) {
            state.views.insert(view, demand);
            state.reconcile();
            self.shared.request.fetch_add(1, Ordering::Release);
            self.shared.changed.notify_one();
        }
    }

    pub fn release(&self, view: ViewId) {
        let mut state = self.shared.state.lock().unwrap();
        if state.views.remove(&view).is_some() {
            state.reconcile();
            self.shared.request.fetch_add(1, Ordering::Release);
            self.shared.changed.notify_one();
        }
    }

    /// Cache-only access for rendering, copying and searching. Never changes
    /// subscriptions, cancels another view's work, or initiates a syntax seek.
    pub fn cached(
        &self,
        diff: &Diff,
        rows: impl Iterator<Item = usize>,
    ) -> BTreeMap<usize, Arc<Chunk>> {
        let requests = requests(diff, rows);
        let mut state = self.shared.state.lock().unwrap();
        state.clock += 1;
        let stamp = state.clock;
        requests
            .into_iter()
            .filter_map(|request| {
                let (used, chunk) = state.chunks.get_mut(&request.start)?;
                *used = stamp;
                Some((request.start, Arc::clone(chunk)))
            })
            .collect()
    }

    #[cfg(test)]
    pub fn demands(&self) -> (usize, Vec<Range<usize>>) {
        let state = self.shared.state.lock().unwrap();
        (
            state.views.len(),
            state.wanted.iter().map(|r| r.start..r.end).collect(),
        )
    }

    /// No visible or prefetch work is queued or running.
    #[cfg(test)]
    pub fn settled(&self, diff: &Diff) -> bool {
        let state = self.shared.state.lock().unwrap();
        self.shared.stopped.load(Ordering::Acquire) || (state.idle && state.next(diff).is_none())
    }

    #[cfg(test)]
    pub fn stats(&self) -> (usize, usize, u64) {
        let state = self.shared.state.lock().unwrap();
        (
            state.chunks.len(),
            state.bytes,
            self.shared.parsed_rows.load(Ordering::Relaxed),
        )
    }
}

impl Drop for SyntaxCache {
    fn drop(&mut self) {
        // Pair the condition-variable notification with its mutex so a worker
        // cannot miss shutdown between testing the flag and going to sleep.
        let _state = self.shared.state.lock().unwrap();
        self.shared.stopped.store(true, Ordering::Release);
        self.shared.changed.notify_one();
    }
}

fn requests(diff: &Diff, rows: impl Iterator<Item = usize>) -> Vec<Request> {
    let mut requests = Vec::new();
    for raw in rows {
        if !matches!(
            diff.lines[raw].kind,
            Kind::Add | Kind::Delete | Kind::Context
        ) {
            continue;
        }
        let file = diff.files.partition_point(|file| file.start <= raw) - 1;
        let start = diff.files[file].start;
        let end = diff
            .files
            .get(file + 1)
            .map_or(diff.lines.len(), |f| f.start);
        let start = start + (raw - start) / CHUNK_ROWS * CHUNK_ROWS;
        let request = Request {
            file,
            start,
            end: (start + CHUNK_ROWS).min(end),
        };
        if requests.last() != Some(&request) {
            requests.push(request);
            if requests.len() == CACHE_CHUNKS {
                break;
            }
        }
    }
    requests
}

fn interrupted(
    shared: &Shared,
    diff: &Diff,
    request: Request,
    version: &std::cell::Cell<u64>,
) -> bool {
    if shared.stopped.load(Ordering::Acquire) {
        return true;
    }
    let current = shared.request.load(Ordering::Acquire);
    if current == version.get() {
        return false;
    }
    version.set(current);
    let state = shared.state.lock().unwrap();
    // A second viewport does not cancel a still-requested seek. Preempt only
    // for obsolete demand or higher-priority visible/inline work.
    (!state.wanted.contains(&request) && !state.prefetch.contains(&request))
        || state
            .next(diff)
            .is_some_and(|next| state.priority(diff, next) < state.priority(diff, request))
}

/// Both sides of `file`, highlighted over the rows of the patch.
fn file_sides(diff: &Diff, file: usize) -> DiffSides {
    let meta = &diff.files[file];
    let end = diff
        .files
        .get(file + 1)
        .map_or(diff.lines.len(), |f| f.start);
    let rows = (meta.start..end).filter_map(|raw| {
        let side = match diff.lines[raw].kind {
            Kind::Hunk => RowSide::Gap,
            Kind::Context => RowSide::Both,
            Kind::Add => RowSide::New,
            Kind::Delete => RowSide::Old,
            _ => return None,
        };
        Some((raw, side))
    });
    DiffSides::new(
        [
            Syntax::for_path(&meta.old_path),
            Syntax::for_path(&meta.path),
        ],
        rows,
    )
}

/// Source text of a patch row; hunk headers and other rows are gaps.
fn row_text(diff: &Diff, raw: usize) -> Option<Cow<'_, str>> {
    matches!(
        diff.lines[raw].kind,
        Kind::Add | Kind::Delete | Kind::Context
    )
    .then(|| Cow::Borrowed(diff.source(raw, false)))
}

fn run(diff: Arc<Diff>, shared: Arc<Shared>) {
    // Most recently used last.
    let mut files: Vec<(usize, DiffSides)> = Vec::new();
    loop {
        let (request, version, inline) = {
            let mut state = shared.state.lock().unwrap();
            loop {
                if shared.stopped.load(Ordering::Acquire) {
                    return;
                }
                if let Some(request) = state.next(&diff) {
                    state.idle = false;
                    let inline = state
                        .chunks
                        .get(&request.start)
                        .map(|(_, chunk)| Arc::clone(&chunk.inline));
                    break (request, shared.request.load(Ordering::Acquire), inline);
                }
                state.idle = true;
                state = shared.changed.wait(state).unwrap();
            }
        };
        let version = std::cell::Cell::new(version);
        let Some(inline) = inline else {
            let Some(rows) = inline::rows(&diff, request.start..request.end, || {
                interrupted(&shared, &diff, request, &version)
            }) else {
                continue;
            };
            let changed = !rows.is_empty();
            let mut state = shared.state.lock().unwrap();
            state.publish(Chunk {
                raw: request.start..request.end,
                spans: Vec::new(),
                inline: rows.into(),
                syntax_ready: false,
                scopes: Vec::new(),
                colors: Mutex::default(),
            });
            if changed {
                shared.generation.fetch_add(1, Ordering::Release);
                if state.wanted.contains(&request) {
                    if let Some(wakeup) = &state.wakeup {
                        let _ = wakeup.send(());
                    }
                }
            }
            continue;
        };
        let at = match files.iter().position(|(file, _)| *file == request.file) {
            Some(at) => at,
            None => {
                if files.len() == PARSED_FILES {
                    files.remove(0);
                }
                files.push((request.file, file_sides(&diff, request.file)));
                files.len() - 1
            }
        };
        let entry = files.remove(at);
        files.push(entry);
        let sides = &files.last().unwrap().1;
        #[cfg(test)]
        let parsed = crate::content::highlight::source::PARSED_LINES.with(std::cell::Cell::get);
        let scoped = sides.scopes(
            |raw| row_text(&diff, raw),
            request.start..request.end,
            &mut || interrupted(&shared, &diff, request, &version),
        );
        #[cfg(test)]
        shared.parsed_rows.fetch_add(
            (crate::content::highlight::source::PARSED_LINES.with(std::cell::Cell::get) - parsed)
                as u64,
            Ordering::Relaxed,
        );
        let Some(scoped) = scoped else {
            continue;
        };
        let mut spans: Vec<SyntaxRange> = Vec::new();
        for (raw, runs) in (request.start..).zip(&scoped.lines) {
            let start = diff.lines[raw].start;
            for &(end, scope) in runs {
                match spans.last_mut() {
                    Some(last) if last.scope == scope => last.end = start + end,
                    _ => spans.push(SyntaxRange {
                        end: start + end,
                        scope,
                    }),
                }
            }
        }
        let scopes = scoped.stacks;
        publish_syntax(&shared, request, spans, scopes, inline);
    }
}

fn publish_syntax(
    shared: &Shared,
    request: Request,
    spans: Vec<SyntaxRange>,
    scopes: Vec<Vec<Scope>>,
    inline: Arc<[inline::InlineRow]>,
) {
    let mut state = shared.state.lock().unwrap();
    state.publish(Chunk {
        raw: request.start..request.end,
        spans,
        inline,
        syntax_ready: true,
        scopes,
        colors: Mutex::new((0, Arc::default())),
    });
    shared.generation.fetch_add(1, Ordering::Release);
    if state.wanted.contains(&request) {
        if let Some(wakeup) = &state.wakeup {
            let _ = wakeup.send(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oversized_viewport_chunk_is_pinned_only_while_requested() {
        let mut state = State::default();
        state.wanted.push(Request {
            file: 0,
            start: 0,
            end: 128,
        });
        state.publish(Chunk {
            raw: 0..128,
            spans: Vec::with_capacity(CACHE_BYTES / std::mem::size_of::<SyntaxRange>() + 1),
            scopes: Vec::new(),
            inline: Arc::default(),
            syntax_ready: true,
            colors: Mutex::default(),
        });
        assert!(state.bytes > CACHE_BYTES);
        assert_eq!(state.chunks.len(), 1);
        state.wanted = vec![Request {
            file: 0,
            start: 128,
            end: 256,
        }];
        state.publish(Chunk {
            raw: 128..256,
            spans: Vec::new(),
            scopes: Vec::new(),
            inline: Arc::default(),
            syntax_ready: true,
            colors: Mutex::default(),
        });
        assert_eq!(state.bytes, 0);
        assert_eq!(state.chunks.len(), 1);
        assert!(state.chunks.contains_key(&128));
    }
}
