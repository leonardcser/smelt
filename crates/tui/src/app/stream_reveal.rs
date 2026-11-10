//! Paced reveal of streamed text, applied when a frame is presented.
//!
//! Streamed text enters the document as it arrives. Each composed frame
//! measures how many glyphs the streamed blocks gained, and presenting a frame
//! hides that many of each stream's newest glyphs, uncovering them over the gap
//! that preceded their arrival. Bursty providers still read as continuous
//! typing, and the typing costs a present per step rather than a document edit,
//! a layout and a compose.

use crate::smelt_edit::{
    row_to_usize, CellSymbol, ComposedFrame, Grid, PaintedWindow, RowIndex, Style,
};
use std::collections::HashMap;
use std::hash::Hash;
use std::time::{Duration, Instant};

/// Shortest reveal, so back-to-back chunks still type out over a few frames.
const MIN_SPREAD: Duration = Duration::from_millis(50);
/// Longest reveal, so a stalled provider does not make the next chunk crawl.
pub(crate) const MAX_SPREAD: Duration = Duration::from_millis(400);

/// How many of a stream's newest glyphs stay hidden, and how fast they show.
#[derive(Default)]
struct Pace {
    hidden: usize,
    last_arrival: Option<Instant>,
    /// When every hidden glyph should be visible.
    deadline: Option<Instant>,
    last_tick: Option<Instant>,
    credit: f64,
}

impl Pace {
    /// `glyphs` new glyphs arrived at `now`. They type out over the gap that
    /// preceded them, expecting the next arrival after the same gap.
    fn grow(&mut self, glyphs: usize, now: Instant) {
        if glyphs == 0 {
            return;
        }
        let spread = self
            .last_arrival
            .map_or(MAX_SPREAD, |last| now.saturating_duration_since(last))
            .clamp(MIN_SPREAD, MAX_SPREAD);
        self.last_arrival = Some(now);
        let was_idle = self.hidden == 0;
        self.hidden += glyphs;
        // A frame may already have spent time past `now`; do not spend it twice.
        let start = self.last_tick.map_or(now, |last| last.max(now));
        self.deadline = Some(
            self.deadline
                .filter(|_| !was_idle)
                .map_or(start + spread, |deadline| deadline.max(start + spread)),
        );
        if was_idle {
            let show_first = self
                .last_tick
                .is_none_or(|last| now.saturating_duration_since(last) >= MAX_SPREAD);
            self.last_tick = Some(start);
            self.credit = 0.0;
            // Show the first glyph at once, without banking credit while idle.
            if show_first {
                self.hidden -= 1;
            }
        }
    }

    /// Show at once what is past the newest `visible` glyphs, which cannot
    /// type in where anyone sees it. The rest keeps typing at the same rate.
    fn cap(&mut self, visible: usize) {
        if self.hidden <= visible {
            return;
        }
        if let (Some(deadline), Some(last)) = (self.deadline, self.last_tick) {
            let left = deadline.saturating_duration_since(last);
            self.deadline = Some(last + left.mul_f64(visible as f64 / self.hidden as f64));
        }
        self.hidden = visible;
        if visible == 0 {
            self.credit = 0.0;
        }
    }

    /// Uncover glyphs linearly so the last one shows at the deadline.
    fn advance(&mut self, now: Instant) {
        let Some(deadline) = self.deadline.filter(|_| self.hidden > 0) else {
            return;
        };
        let last = self.last_tick.replace(now).unwrap_or(now);
        if now >= deadline {
            self.hidden = 0;
            self.credit = 0.0;
            return;
        }
        let elapsed = now.saturating_duration_since(last);
        if elapsed.is_zero() {
            return;
        }
        let budget = deadline.saturating_duration_since(last);
        self.credit += self.hidden as f64 * elapsed.as_secs_f64() / budget.as_secs_f64();
        let shown = (self.credit.floor() as usize).min(self.hidden);
        self.credit -= shown as f64;
        self.hidden -= shown;
        if self.hidden == 0 {
            self.credit = 0.0;
        }
    }
}

/// One block of a stream and the glyphs its rows showed when last composed.
struct Segment {
    id: u64,
    /// Rows the segment had when last measured, or `None` until then, when
    /// what it already holds counts as shown.
    rows: Option<RowIndex>,
    /// Glyphs per row on screen, by row within the segment.
    glyphs: HashMap<RowIndex, u32>,
}

#[derive(Default)]
struct Stream {
    pace: Pace,
    /// Segments in the order the stream produced them.
    segments: Vec<Segment>,
    /// When the earliest delta not yet measured arrived. Its glyphs type out
    /// from then, however late the frame that measures them composes.
    arrived: Option<Instant>,
    /// Screen rows the stream showed on the last composed frame, bottom up,
    /// when its last row was on screen. Hidden glyphs come off these rows.
    tail_rows: Vec<u16>,
}

/// What a stream's segments already hold when it starts tracking them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Baseline {
    /// The segment was just created: everything it holds is new.
    Empty,
    /// The segment was just created with a header: its first row shows at
    /// once, and the rows below it are new.
    Header,
    /// The segment existed: what it holds by the next compose shows at once.
    Current,
}

/// The paced reveal of every stream drawn into one window, keyed by `K`.
pub(crate) struct StreamReveal<K> {
    streams: HashMap<K, Stream>,
    /// Text width of the last measure. Rewrapping moves every glyph, so a
    /// new width measures again without counting growth.
    width: Option<u16>,
    painted: Option<PaintedWindow>,
}

impl<K> Default for StreamReveal<K> {
    fn default() -> Self {
        Self {
            streams: HashMap::new(),
            width: None,
            painted: None,
        }
    }
}

impl<K: Hash + Eq> StreamReveal<K> {
    /// Reveal what segment `id` gains as part of stream `key`. Tracking a
    /// segment again keeps its measures.
    pub(crate) fn track(&mut self, key: K, id: u64, baseline: Baseline) {
        let stream = self.streams.entry(key).or_default();
        if stream.segments.iter().any(|segment| segment.id == id) {
            return;
        }
        stream.segments.push(Segment {
            id,
            rows: match baseline {
                Baseline::Empty => Some(0),
                Baseline::Header => Some(1),
                Baseline::Current => None,
            },
            glyphs: HashMap::new(),
        });
    }

    /// Note that stream `key` received a delta at `at`.
    pub(crate) fn arrive(&mut self, key: &K, at: Instant) {
        if let Some(stream) = self.streams.get_mut(key) {
            stream.arrived = Some(stream.arrived.map_or(at, |earlier| earlier.min(at)));
        }
    }

    /// Whether stream `key` is tracked.
    pub(crate) fn tracks(&self, key: &K) -> bool {
        self.streams.contains_key(key)
    }

    /// Show everything stream `key` holds and stop tracking it.
    pub(crate) fn finish(&mut self, key: &K) {
        self.streams.remove(key);
    }

    /// Show every stream and stop tracking them.
    pub(crate) fn finish_all(&mut self) {
        self.streams.clear();
    }

    /// Show the streams `done` picks and stop tracking them.
    pub(crate) fn finish_where(&mut self, mut done: impl FnMut(&K) -> bool) {
        self.streams.retain(|key, _| !done(key));
    }

    /// Whether any stream is tracked, so composed frames need measuring.
    pub(crate) fn is_tracking(&self) -> bool {
        !self.streams.is_empty()
    }

    /// Measure what each stream gained on `composed`, where `painted` is the
    /// window the streams show in, or `None` when it is not on screen, and
    /// `layout` gives a segment's `(first row, rows)` in the window's buffer
    /// rows while it is laid out.
    ///
    /// Glyphs are counted on screen, the same way presenting hides them, so
    /// only what shows can type in. Text that lands off screen shows at once,
    /// and of a burst taller than the window, only what shows types in, at
    /// the burst's rate.
    pub(crate) fn measure(
        &mut self,
        composed: &ComposedFrame,
        painted: Option<PaintedWindow>,
        layout: impl Fn(u64) -> Option<(RowIndex, RowIndex)>,
        now: Instant,
    ) {
        self.painted = painted;
        let rewrapped = painted.is_some_and(|painted| {
            self.width
                .replace(painted.text.width)
                .is_some_and(|width| width != painted.text.width)
        });
        let on_screen = painted.map_or(0..0, |painted| {
            painted.scroll_top..painted.scroll_top + RowIndex::from(painted.text.height)
        });
        for stream in self.streams.values_mut() {
            // Only text that arrived grows a stream. A fold, a rewrap or a
            // renderer change also moves glyphs, and shows at once.
            let arrived = stream.arrived.take();
            let mut gained = 0i64;
            let mut shown = 0usize;
            let mut last_row = None;
            // Rows past what each segment had, in all and on screen.
            let (mut new_rows, mut new_rows_shown) = (0, 0);
            stream.tail_rows.clear();
            for segment in &mut stream.segments {
                if rewrapped || arrived.is_none() {
                    segment.rows = None;
                    segment.glyphs.clear();
                }
                let Some((start, rows)) = layout(segment.id) else {
                    continue;
                };
                if rows > 0 {
                    last_row = last_row.max(Some(start + rows - 1));
                }
                let known = segment.rows;
                new_rows += known.map_or(0, |known| rows.saturating_sub(known));
                let mut glyphs = HashMap::new();
                let first = start.max(on_screen.start);
                let end = (start + rows).min(on_screen.end);
                for row in first..end {
                    let Some(painted) = painted else {
                        break;
                    };
                    let y = painted.text.y + (row - painted.scroll_top) as u16;
                    let count = row_glyphs(composed.grid(), composed, painted, y);
                    let row = row - start;
                    let is_new = known.is_some_and(|known| row >= known);
                    new_rows_shown += RowIndex::from(is_new);
                    let before = segment.glyphs.get(&row).copied().or(is_new.then_some(0));
                    if let Some(before) = before {
                        gained += i64::from(count) - i64::from(before);
                    }
                    glyphs.insert(row, count);
                    shown += count as usize;
                    stream.tail_rows.push(y);
                }
                segment.rows = Some(rows);
                segment.glyphs = glyphs;
            }
            if let Some(arrived) = arrived {
                let mut gained = usize::try_from(gained).unwrap_or(0);
                // New rows off screen grew too: estimate them from those that
                // show, so the burst keeps its rate.
                if new_rows_shown > 0 {
                    gained = gained * row_to_usize(new_rows) / row_to_usize(new_rows_shown);
                }
                stream.pace.grow(gained, arrived.min(now));
            }
            // Glyphs can only be hidden off the stream's end, so they type in
            // only while its last row shows.
            if last_row.is_some_and(|row| on_screen.contains(&row)) {
                stream.tail_rows.sort_unstable_by(|a, b| b.cmp(a));
            } else {
                stream.tail_rows.clear();
                shown = 0;
            }
            stream.pace.cap(shown);
        }
    }

    /// Hide each stream's newest glyphs on `grid`, a copy of `composed` being
    /// presented at `now`. Returns whether any glyph is still hidden.
    pub(crate) fn present(
        &mut self,
        grid: &mut Grid,
        composed: &ComposedFrame,
        now: Instant,
    ) -> bool {
        let mut pending = false;
        for stream in self.streams.values_mut() {
            stream.pace.advance(now);
            pending |= stream.pace.hidden > 0;
            if let Some(painted) = self.painted {
                hide_tail(
                    grid,
                    composed,
                    painted,
                    &stream.tail_rows,
                    stream.pace.hidden,
                );
            }
        }
        pending
    }
}

/// Whether screen cell `(x, y)` of `grid` holds a glyph of `painted`'s text.
/// Blanks, the right halves of wide glyphs and cells the window does not
/// show, such as those under an overlay, do not count.
fn is_glyph(grid: &Grid, composed: &ComposedFrame, painted: PaintedWindow, x: u16, y: u16) -> bool {
    if composed
        .owner(x, y)
        .is_none_or(|owner| owner.win != painted.win)
    {
        return false;
    }
    let symbol = &grid.cell(x, y).symbol;
    !symbol.is_continuation() && !symbol.as_str().chars().all(char::is_whitespace)
}

/// Glyphs on screen row `y` of `painted`'s text.
fn row_glyphs(grid: &Grid, composed: &ComposedFrame, painted: PaintedWindow, y: u16) -> u32 {
    let text = painted.text;
    (text.x..text.right())
        .filter(|&x| is_glyph(grid, composed, painted, x, y))
        .count() as u32
}

/// Blank the last `count` glyphs on `rows`, scanning each from the right.
/// A blanked cell takes the style of the row's trailing blank, so underlines,
/// inline backgrounds and selections do not show ahead of the text.
fn hide_tail(
    grid: &mut Grid,
    composed: &ComposedFrame,
    painted: PaintedWindow,
    rows: &[u16],
    count: usize,
) {
    let mut left = count;
    let text = painted.text;
    for &y in rows {
        if left == 0 {
            return;
        }
        let last = text.right().saturating_sub(1);
        let fill = composed
            .owner(last, y)
            .is_some_and(|owner| owner.win == painted.win)
            .then(|| grid.cell(last, y))
            .filter(|cell| cell.symbol.as_str().chars().all(char::is_whitespace))
            .map(|cell| cell.style);
        for x in (text.x..text.right()).rev() {
            if left == 0 {
                return;
            }
            if !is_glyph(grid, composed, painted, x, y) {
                continue;
            }
            let wide = grid.cell(x, y).symbol.width() > 1;
            for x in x..x + if wide { 2 } else { 1 } {
                if let Some(cell) = grid.cell_mut(x, y) {
                    cell.symbol = CellSymbol::space();
                    cell.style = fill.unwrap_or(Style {
                        bg: cell.style.bg,
                        ..Default::default()
                    });
                }
            }
            left -= 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Glyphs shown at each frame while chunks of `sizes` arrive at `at` ms.
    fn shown_over(arrivals: &[(u64, usize)], until_ms: u64) -> Vec<(u64, usize)> {
        let start = Instant::now();
        let mut pace = Pace::default();
        let mut received = 0;
        let mut frames = Vec::new();
        for ms in (0..=until_ms).step_by(16) {
            let now = start + Duration::from_millis(ms);
            for &(_, glyphs) in arrivals.iter().filter(|(at, _)| (ms..ms + 16).contains(at)) {
                pace.grow(glyphs, now);
                received += glyphs;
            }
            pace.advance(now);
            frames.push((ms, received - pace.hidden));
        }
        frames
    }

    #[test]
    fn bursty_chunks_type_continuously_between_arrivals() {
        let frames = shown_over(&[(0, 20), (352, 24), (704, 23)], 704 + 400);
        let mut last_change = 0;
        let mut last_shown = 0;
        for &(ms, shown) in &frames {
            assert!(shown >= last_shown, "glyphs never hide again");
            if shown > last_shown {
                last_change = ms;
            } else if ms <= 704 {
                assert!(ms - last_change <= 32, "typing stalled at {ms}ms");
            }
            last_shown = shown;
        }
        assert_eq!(last_shown, 67, "every glyph shows by the last deadline");
    }

    #[test]
    fn first_glyph_shows_at_once_and_idle_time_is_not_banked() {
        let start = Instant::now();
        let mut pace = Pace::default();
        pace.grow(10, start);
        assert_eq!(pace.hidden, 9);
        pace.advance(start);
        assert_eq!(pace.hidden, 9, "no time, no progress");

        let later = start + Duration::from_secs(5);
        pace.advance(later);
        assert_eq!(pace.hidden, 0, "a stalled stream drains by its deadline");
        pace.grow(10, later + Duration::from_secs(5));
        assert_eq!(
            pace.hidden, 9,
            "idle time does not reveal the next chunk at once"
        );
    }

    #[test]
    fn a_burst_taller_than_the_window_types_in_only_where_it_shows() {
        let start = Instant::now();
        let mut pace = Pace::default();
        pace.grow(1000, start);
        pace.cap(100);
        assert_eq!(pace.hidden, 100, "what scrolled past shows at once");
        pace.advance(start + MAX_SPREAD / 20);
        assert!(
            (45..=55).contains(&pace.hidden),
            "the rest keeps the burst's rate: {}",
            pace.hidden
        );
        pace.advance(start + MAX_SPREAD / 10);
        assert_eq!(pace.hidden, 0);
    }

    #[test]
    fn steady_streams_stay_close_behind() {
        let arrivals: Vec<_> = (0..10).map(|i| (i * 100, 10)).collect();
        let frames = shown_over(&arrivals, 1000);
        let (_, shown) = frames.iter().find(|(ms, _)| *ms == 912).copied().unwrap();
        assert!(
            shown >= 90,
            "only the latest chunk is still typing: {shown}"
        );
    }

    fn painted(height: u16, scroll_top: RowIndex) -> PaintedWindow {
        PaintedWindow {
            win: crate::smelt_edit::WinId(7),
            buf: crate::smelt_edit::BufId(7),
            text: crate::smelt_edit::Rect::new(0, 0, 8, height),
            scroll_top,
        }
    }

    /// A 4-row window showing `rows` from buffer row `scroll_top`.
    fn screen(rows: &[&str], scroll_top: RowIndex) -> (ComposedFrame, PaintedWindow) {
        let window = painted(4, scroll_top);
        let mut grid = Grid::new(8, 4);
        for (y, row) in rows.iter().enumerate() {
            grid.as_mut_slice()
                .put_str(0, y as u16, row, Style::default());
        }
        (ComposedFrame::new(grid, vec![window]), window)
    }

    #[test]
    fn growth_counts_new_glyphs_once_across_rewraps_of_its_rows() {
        let mut reveal = StreamReveal::<()>::default();
        reveal.track((), 1, Baseline::Empty);
        let start = Instant::now();
        let measure = |reveal: &mut StreamReveal<()>, rows: &[&str]| {
            let (composed, window) = screen(rows, 0);
            let count = rows.len() as RowIndex;
            reveal.arrive(&(), start);
            reveal.measure(
                &composed,
                Some(window),
                |_| Some((0, count)),
                start + MAX_SPREAD * 4,
            );
        };
        measure(&mut reveal, &["abcde"]);
        assert_eq!(
            reveal.streams[&()].pace.hidden,
            4,
            "a new segment types out what it holds"
        );
        reveal.finish(&());

        reveal.track((), 1, Baseline::Current);
        measure(&mut reveal, &["abcde"]);
        assert_eq!(
            reveal.streams[&()].pace.hidden,
            0,
            "a current baseline shows at once"
        );
        // A word wraps onto a new row: 3 glyphs leave row 0, 3 + 2 land on row 1.
        measure(&mut reveal, &["ab", "cdefg"]);
        assert_eq!(
            reveal.streams[&()].pace.hidden,
            1,
            "only the 2 new glyphs grow; the first shows"
        );
    }

    #[test]
    fn a_burst_taller_than_the_view_types_in_what_shows_at_its_rate() {
        let mut reveal = StreamReveal::<()>::default();
        reveal.track((), 1, Baseline::Empty);
        let start = Instant::now();
        // 8 new rows of 4 glyphs; the view shows the last 4.
        let (composed, window) = screen(&["abcd"; 4], 4);
        reveal.arrive(&(), start);
        reveal.measure(&composed, Some(window), |_| Some((0, 8)), start);
        let pace = &reveal.streams[&()].pace;
        assert_eq!(pace.hidden, 16, "only what shows types in");
        let spent = pace.deadline.unwrap() - start;
        assert!(
            spent < MAX_SPREAD * 6 / 10,
            "half the burst shows, so it types in about half the time: {spent:?}"
        );
    }

    #[test]
    fn text_typed_below_the_view_shows_at_once() {
        let mut reveal = StreamReveal::<()>::default();
        reveal.track((), 1, Baseline::Empty);
        let start = Instant::now();
        // Scrolled to the top of a 6-row segment: its last 2 rows are below.
        let (composed, window) = screen(&["ab", "cd", "ef", "gh"], 0);
        reveal.arrive(&(), start);
        reveal.measure(&composed, Some(window), |_| Some((0, 6)), start);
        assert_eq!(reveal.streams[&()].pace.hidden, 0);
        let mut grid = composed.grid().clone();
        assert!(
            !reveal.present(&mut grid, &composed, start),
            "no frames to wait for"
        );
    }

    #[test]
    fn hidden_glyphs_come_off_the_tail_without_their_style() {
        let mut grid = Grid::new(8, 2);
        let link = Style {
            underline: true,
            ..Style::default()
        };
        grid.as_mut_slice().put_str(0, 0, "ab cd", link);
        grid.as_mut_slice().put_str(0, 1, "e漢", Style::default());
        let composed = ComposedFrame::new(grid.clone(), vec![painted(2, 0)]);
        let mut out = grid.clone();
        hide_tail(&mut out, &composed, painted(2, 0), &[1, 0], 3);
        let row = |grid: &Grid, y| {
            (0..grid.width())
                .map(|x| grid.cell(x, y).symbol.as_str().replace('\0', ""))
                .collect::<String>()
        };
        assert_eq!(
            row(&out, 1).trim_end(),
            "",
            "the wide glyph and its continuation blank"
        );
        assert_eq!(row(&out, 0).trim_end(), "ab c");
        assert_eq!(
            out.cell(4, 0).style,
            Style::default(),
            "a hidden glyph takes the trailing blank's style, not its underline"
        );
    }
}
