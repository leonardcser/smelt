//! Emacs-style kill ring with yank-pop support.
//!
//! The kill ring carries plain text only. Attachment markers have no
//! representation here — an `AttachmentId` is bound to a specific buffer's
//! attachment list, not to text — so every entry point strips
//! `ATTACHMENT_MARKER` before storing. Yanking back can then go through
//! `AttachedTextMut::insert_str` without risk of producing orphan markers
//! that would desync source from the id list.

use crate::attachment::ATTACHMENT_MARKER;
use std::time::{Duration, Instant};

fn strip_markers(text: String) -> String {
    if text.contains(ATTACHMENT_MARKER) {
        text.replace(ATTACHMENT_MARKER, "")
    } else {
        text
    }
}

const KILL_RING_MAX: usize = 32;

/// Duration of the post-yank highlight flash.
pub(crate) const YANK_FLASH_DURATION: Duration = Duration::from_millis(200);

/// Kill ring shared by emacs-style edits and vim yank/paste operations.
pub struct KillRing {
    current: String,
    /// Older kills, newest first.
    history: Vec<String>,
    /// Byte range of the last yank insertion, for yank-pop replacement.
    last_yank: Option<(usize, usize)>,
    pop_idx: usize,
    /// True for line-wise kills (`Y`, `yy`, `dd`); vim `p`/`P` insert on a new line.
    linewise: bool,
    /// Byte range in the source buffer the last kill was captured from.
    source_range: Option<(usize, usize)>,
    /// Monotonic counter, incremented on every `set_with_source`. Hosts use it
    /// to detect "vim just yanked something" without depending on the flash
    /// timer (which is a UI animation concern, not an event-tick).
    yank_tick: u64,
    /// Set by `mark_yanked`; drives the post-yank highlight flash.
    last_yank_at: Option<Instant>,
    /// Last text pushed to the system clipboard. Paste sites compare against
    /// `clipboard.read()` to detect external clipboard updates.
    last_clipboard_write: Option<String>,
}

impl Default for KillRing {
    fn default() -> Self {
        Self::new()
    }
}

impl KillRing {
    pub fn new() -> Self {
        Self {
            current: String::new(),
            history: Vec::new(),
            last_yank: None,
            pop_idx: 0,
            linewise: false,
            source_range: None,
            yank_tick: 0,
            last_yank_at: None,
            last_clipboard_write: None,
        }
    }

    /// Push a new kill, rotating the previous current into history.
    pub fn kill(&mut self, text: String) {
        let text = strip_markers(text);
        if text.is_empty() {
            return;
        }
        if !self.current.is_empty() {
            self.history.insert(0, std::mem::take(&mut self.current));
            if self.history.len() > KILL_RING_MAX {
                self.history.pop();
            }
        }
        self.current = text;
        self.last_yank = None;
        self.linewise = false;
        self.source_range = None;
    }

    /// Yank the current kill into `buf` at `cpos`. Returns new cpos.
    /// `cpos` is snapped to a char boundary.
    pub fn yank(
        &mut self,
        buf: &mut crate::attached::AttachedTextMut<'_>,
        cpos: usize,
    ) -> Option<usize> {
        if self.current.is_empty() {
            return None;
        }
        let cpos = buf.insert_str(cpos, &self.current);
        let end = cpos + self.current.len();
        self.last_yank = Some((cpos, end));
        self.pop_idx = 0;
        Some(end)
    }

    /// Replace the last yank with the next history entry. Returns new cpos.
    pub fn yank_pop(&mut self, buf: &mut crate::attached::AttachedTextMut<'_>) -> Option<usize> {
        let (start, end) = self.last_yank?;
        if self.history.is_empty() {
            return None;
        }
        let start = crate::text::snap(buf.as_str(), start);
        let end = crate::text::snap(buf.as_str(), end).max(start);
        let text = self.history[self.pop_idx % self.history.len()].clone();
        let new_end = start + text.len();
        buf.replace_range(start..end, &text);
        self.last_yank = Some((start, new_end));
        self.pop_idx = (self.pop_idx + 1) % self.history.len();
        Some(new_end)
    }

    /// Clear last-yank tracking.
    pub fn clear_yank(&mut self) {
        self.last_yank = None;
    }

    pub fn take(&mut self) -> String {
        self.linewise = false;
        self.source_range = None;
        std::mem::take(&mut self.current)
    }

    /// Set the current kill text, clearing the linewise flag.
    pub fn set(&mut self, text: String) {
        self.current = strip_markers(text);
        self.linewise = false;
        self.source_range = None;
    }

    /// Set the current kill text with an explicit linewise flag.
    pub fn set_with_linewise(&mut self, text: String, linewise: bool) {
        self.current = strip_markers(text);
        self.linewise = linewise;
    }

    /// Set kill text, linewise flag, and source byte range. Bumps `yank_tick`
    /// so hosts can detect that a yank happened. Clears `last_yank_at` so
    /// deletes don't inherit a prior yank's flash; call `mark_yanked` after for yanks.
    pub fn set_with_source(&mut self, text: String, linewise: bool, start: usize, end: usize) {
        self.current = strip_markers(text);
        self.linewise = linewise;
        self.source_range = Some((start, end));
        self.yank_tick = self.yank_tick.wrapping_add(1);
        self.last_yank_at = None;
    }

    /// Monotonic counter, bumped on every `set_with_source`. Hosts snapshot
    /// this before and after a dispatch; a difference means a yank landed.
    pub fn yank_tick(&self) -> u64 {
        self.yank_tick
    }

    pub fn current(&self) -> &str {
        &self.current
    }

    pub fn is_linewise(&self) -> bool {
        self.linewise
    }

    pub fn source_range(&self) -> Option<(usize, usize)> {
        self.source_range
    }

    /// Mark the most recent kill as a yank, enabling the post-yank highlight flash.
    /// `now` is taken from the host clock so virtual-clock tests see the same
    /// flash window the production renderer reads.
    pub fn mark_yanked(&mut self, now: Instant) {
        self.last_yank_at = Some(now);
    }

    /// Source range of the most recent yank if its flash window is still active.
    pub fn yank_flash_range(&self, now: Instant) -> Option<(usize, usize)> {
        let started = self.last_yank_at?;
        let range = self.source_range?;
        if now.duration_since(started) < YANK_FLASH_DURATION {
            Some(range)
        } else {
            None
        }
    }

    /// When the current flash window expires, if one is active.
    pub fn yank_flash_until(&self) -> Option<Instant> {
        self.last_yank_at.map(|t| t + YANK_FLASH_DURATION)
    }

    /// Record the last text pushed to the system clipboard for external-update detection.
    pub fn record_clipboard_write(&mut self, text: String) {
        self.last_clipboard_write = Some(text);
    }

    pub fn last_clipboard_write(&self) -> Option<&str> {
        self.last_clipboard_write.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attached::AttachedTextMut;
    use crate::attachment::AttachmentId;

    /// Mutate `buf` (a `String`) as if it were an attached text — empty ids.
    /// Returns the closure result.
    fn with_attached<F, R>(buf: &mut String, f: F) -> R
    where
        F: FnOnce(&mut AttachedTextMut<'_>) -> R,
    {
        let mut ids: Vec<AttachmentId> = Vec::new();
        let mut a = AttachedTextMut::new(buf, &mut ids);
        f(&mut a)
    }

    #[test]
    fn flash_range_active_only_after_mark_yanked() {
        let mut kr = KillRing::new();
        let t0 = Instant::now();
        kr.set_with_source("hello".into(), false, 3, 8);
        // set_with_source alone (delete / change) must not flash.
        assert!(kr.yank_flash_range(t0).is_none());
        // Yank-only sites mark explicitly.
        kr.mark_yanked(t0);
        assert_eq!(kr.yank_flash_range(t0), Some((3, 8)));
    }

    #[test]
    fn flash_range_expires_after_window() {
        let mut kr = KillRing::new();
        let t0 = Instant::now();
        kr.set_with_source("x".into(), false, 0, 1);
        kr.mark_yanked(t0);
        let later = t0 + YANK_FLASH_DURATION + Duration::from_millis(50);
        assert!(kr.yank_flash_range(later).is_none());
    }

    #[test]
    fn delete_after_yank_clears_flash() {
        let mut kr = KillRing::new();
        let t0 = Instant::now();
        kr.set_with_source("first".into(), false, 0, 5);
        kr.mark_yanked(t0);
        assert!(kr.yank_flash_range(t0).is_some());
        // Subsequent delete-style update — no mark_yanked.
        kr.set_with_source("second".into(), false, 10, 16);
        assert!(kr.yank_flash_range(t0).is_none());
    }

    // ── kill (push) ───────────────────────────────────────────────────────

    #[test]
    fn kill_empty_text_is_noop() {
        let mut kr = KillRing::new();
        kr.kill("first".into());
        kr.kill("".into()); // empty must not displace current
        assert_eq!(kr.current(), "first");
    }

    #[test]
    fn second_kill_rotates_first_into_history() {
        let mut kr = KillRing::new();
        kr.kill("A".into());
        kr.kill("B".into());
        assert_eq!(kr.current(), "B");
        // First kill is now the most-recent history entry.
        // We can observe history indirectly through yank_pop after a yank.
        let mut buf = String::from("|");
        with_attached(&mut buf, |a| kr.yank(a, 1));
        assert_eq!(buf, "|B");
        with_attached(&mut buf, |a| kr.yank_pop(a));
        assert_eq!(buf, "|A");
    }

    #[test]
    fn kill_history_cap_evicts_oldest_beyond_thirty_two() {
        let mut kr = KillRing::new();
        // Push 34 distinct kills: current = "k33", history holds the previous
        // 32 ("k32" … "k1"); "k0" got dropped past the cap.
        for i in 0..34 {
            kr.kill(format!("k{i}"));
        }
        // Yank installs k33, then yank_pop cycles through history newest-first.
        let mut buf = String::new();
        with_attached(&mut buf, |a| kr.yank(a, 0));
        assert_eq!(buf, "k33");
        // Yank-pop 32 times — every entry in history should be reachable
        // exactly once before it wraps. The oldest reachable should be k1.
        let mut seen = vec![buf.clone()];
        for _ in 0..32 {
            with_attached(&mut buf, |a| kr.yank_pop(a));
            seen.push(buf.clone());
        }
        assert!(seen.contains(&"k1".to_string()));
        assert!(!seen.contains(&"k0".to_string()), "k0 evicted past cap");
    }

    // ── yank / yank_pop ───────────────────────────────────────────────────

    #[test]
    fn yank_on_empty_kill_ring_returns_none() {
        let mut kr = KillRing::new();
        let mut buf = String::from("hi");
        assert!(with_attached(&mut buf, |a| kr.yank(a, 1)).is_none());
        assert_eq!(buf, "hi", "buffer untouched");
    }

    #[test]
    fn yank_inserts_current_and_returns_cursor_at_end_of_inserted_text() {
        let mut kr = KillRing::new();
        kr.kill("xyz".into());
        let mut buf = String::from("ab|cd");
        let end = with_attached(&mut buf, |a| kr.yank(a, 3)).expect("yank into 'ab|cd' at byte 3");
        assert_eq!(buf, "ab|xyzcd");
        assert_eq!(end, 6, "cursor lands at byte after 'xyz'");
    }

    #[test]
    fn yank_pop_replaces_last_yank_with_next_history_entry() {
        let mut kr = KillRing::new();
        kr.kill("A".into());
        kr.kill("B".into());
        kr.kill("C".into());
        // After 3 kills: current=C, history=[B, A] (newest first).
        let mut buf = String::new();
        with_attached(&mut buf, |a| kr.yank(a, 0));
        assert_eq!(buf, "C");
        with_attached(&mut buf, |a| kr.yank_pop(a));
        assert_eq!(buf, "B", "first yank_pop: history[0]");
        with_attached(&mut buf, |a| kr.yank_pop(a));
        assert_eq!(buf, "A", "second yank_pop: history[1]");
        with_attached(&mut buf, |a| kr.yank_pop(a));
        assert_eq!(buf, "B", "third yank_pop wraps back to history[0]");
    }

    #[test]
    fn yank_pop_returns_none_when_no_prior_yank() {
        let mut kr = KillRing::new();
        kr.kill("A".into());
        kr.kill("B".into());
        let mut buf = String::from("x");
        assert!(
            with_attached(&mut buf, |a| kr.yank_pop(a)).is_none(),
            "no last_yank to replace yet"
        );
        assert_eq!(buf, "x");
    }

    #[test]
    fn clear_yank_disables_subsequent_yank_pop() {
        let mut kr = KillRing::new();
        kr.kill("A".into());
        kr.kill("B".into());
        let mut buf = String::new();
        with_attached(&mut buf, |a| kr.yank(a, 0));
        kr.clear_yank();
        // last_yank is gone — yank_pop has no range to replace.
        assert!(with_attached(&mut buf, |a| kr.yank_pop(a)).is_none());
    }

    // ── linewise / source / accessors ─────────────────────────────────────

    #[test]
    fn take_returns_current_and_clears_it() {
        let mut kr = KillRing::new();
        kr.kill("payload".into());
        assert_eq!(kr.take(), "payload");
        assert_eq!(kr.current(), "");
    }

    #[test]
    fn set_with_linewise_records_the_flag() {
        let mut kr = KillRing::new();
        kr.set_with_linewise("line\n".into(), true);
        assert_eq!(kr.current(), "line\n");
        assert!(kr.is_linewise());
    }

    #[test]
    fn set_with_source_records_range_and_bumps_yank_tick() {
        let mut kr = KillRing::new();
        let t0 = kr.yank_tick();
        kr.set_with_source("payload".into(), true, 5, 12);
        assert_eq!(kr.current(), "payload");
        assert!(kr.is_linewise());
        assert_eq!(kr.source_range(), Some((5, 12)));
        assert_eq!(kr.yank_tick(), t0 + 1, "tick monotonic on set_with_source");
    }

    #[test]
    fn record_clipboard_write_round_trips() {
        let mut kr = KillRing::new();
        assert_eq!(kr.last_clipboard_write(), None);
        kr.record_clipboard_write("from-system".into());
        assert_eq!(kr.last_clipboard_write(), Some("from-system"));
    }
}
