//! Emacs-style kill ring with yank-pop support.

use std::time::{Duration, Instant};

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
    pub fn yank(&mut self, buf: &mut String, cpos: usize) -> Option<usize> {
        if self.current.is_empty() {
            return None;
        }
        let cpos = crate::text::safe_insert_str(buf, cpos, &self.current);
        let end = cpos + self.current.len();
        self.last_yank = Some((cpos, end));
        self.pop_idx = 0;
        Some(end)
    }

    /// Replace the last yank with the next history entry. Returns new cpos.
    pub fn yank_pop(&mut self, buf: &mut String) -> Option<usize> {
        let (start, end) = self.last_yank?;
        if self.history.is_empty() {
            return None;
        }
        let start = crate::text::snap(buf, start);
        let end = crate::text::snap(buf, end).max(start);
        let text = self.history[self.pop_idx % self.history.len()].clone();
        let new_end = start + text.len();
        crate::text::safe_replace_range(buf, start..end, &text);
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
        self.current = text;
        self.linewise = false;
        self.source_range = None;
    }

    /// Set the current kill text with an explicit linewise flag.
    pub fn set_with_linewise(&mut self, text: String, linewise: bool) {
        self.current = text;
        self.linewise = linewise;
    }

    /// Set kill text, linewise flag, and source byte range. Bumps `yank_tick`
    /// so hosts can detect that a yank happened. Clears `last_yank_at` so
    /// deletes don't inherit a prior yank's flash; call `mark_yanked` after for yanks.
    pub fn set_with_source(&mut self, text: String, linewise: bool, start: usize, end: usize) {
        self.current = text;
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
    pub fn mark_yanked(&mut self) {
        self.last_yank_at = Some(Instant::now());
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

    #[test]
    fn flash_range_active_only_after_mark_yanked() {
        let mut kr = KillRing::new();
        kr.set_with_source("hello".into(), false, 3, 8);
        // set_with_source alone (delete / change) must not flash.
        assert!(kr.yank_flash_range(Instant::now()).is_none());
        // Yank-only sites mark explicitly.
        kr.mark_yanked();
        assert_eq!(kr.yank_flash_range(Instant::now()), Some((3, 8)));
    }

    #[test]
    fn flash_range_expires_after_window() {
        let mut kr = KillRing::new();
        kr.set_with_source("x".into(), false, 0, 1);
        kr.mark_yanked();
        let later = Instant::now() + YANK_FLASH_DURATION + Duration::from_millis(50);
        assert!(kr.yank_flash_range(later).is_none());
    }

    #[test]
    fn delete_after_yank_clears_flash() {
        let mut kr = KillRing::new();
        kr.set_with_source("first".into(), false, 0, 5);
        kr.mark_yanked();
        assert!(kr.yank_flash_range(Instant::now()).is_some());
        // Subsequent delete-style update — no mark_yanked.
        kr.set_with_source("second".into(), false, 10, 16);
        assert!(kr.yank_flash_range(Instant::now()).is_none());
    }
}
