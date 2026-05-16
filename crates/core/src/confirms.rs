//! Pending tool-approval dialog requests.
//! [`Confirms::is_clear`] gates engine event draining while a confirm dialog is open.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::transcript_model::ConfirmRequest;

pub struct ConfirmEntry {
    pub req: ConfirmRequest,
}

pub struct Confirms {
    pending: HashMap<u64, ConfirmEntry>,
    next_handle: u64,
    is_clear_flag: Arc<AtomicBool>,
}

impl Confirms {
    pub(crate) fn new() -> Self {
        Self {
            pending: HashMap::new(),
            next_handle: 1,
            is_clear_flag: Arc::new(AtomicBool::new(true)),
        }
    }

    pub fn register(&mut self, req: ConfirmRequest) -> u64 {
        let id = self.next_handle;
        self.next_handle = self.next_handle.wrapping_add(1);
        self.pending.insert(id, ConfirmEntry { req });
        self.is_clear_flag
            .store(self.pending.is_empty(), Ordering::Relaxed);
        id
    }

    pub fn get(&self, id: u64) -> Option<&ConfirmEntry> {
        self.pending.get(&id)
    }

    pub fn take(&mut self, id: u64) -> Option<ConfirmEntry> {
        let result = self.pending.remove(&id);
        self.is_clear_flag
            .store(self.pending.is_empty(), Ordering::Relaxed);
        result
    }

    /// `true` when no confirm dialog is open.
    pub fn is_clear(&self) -> bool {
        self.is_clear_flag.load(Ordering::Relaxed)
    }

    pub fn is_clear_flag(&self) -> Arc<AtomicBool> {
        self.is_clear_flag.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(id: u64) -> ConfirmRequest {
        ConfirmRequest {
            call_id: format!("call-{id}"),
            tool_name: "test".into(),
            args: Default::default(),
            approval_patterns: vec![],
            outside_dir: None,
            summary: protocol::StyledLines::from_plain("test"),
            request_id: id,
        }
    }

    #[test]
    fn new_starts_clear_with_handle_one() {
        let c = Confirms::new();
        assert!(c.is_clear());
    }

    #[test]
    fn register_returns_monotonic_handles_and_marks_not_clear() {
        let mut c = Confirms::new();
        let h1 = c.register(req(1));
        let h2 = c.register(req(2));
        assert_ne!(h1, h2);
        assert!(!c.is_clear());
        assert!(c.get(h1).is_some());
        assert!(c.get(h2).is_some());
    }

    #[test]
    fn take_removes_entry_and_clears_when_pending_empties() {
        let mut c = Confirms::new();
        let h = c.register(req(1));
        let entry = c.take(h);
        assert!(entry.is_some());
        assert!(c.is_clear());
        assert!(c.get(h).is_none());
    }

    #[test]
    fn take_remains_not_clear_while_others_pending() {
        let mut c = Confirms::new();
        let h1 = c.register(req(1));
        let _h2 = c.register(req(2));
        c.take(h1);
        assert!(!c.is_clear());
    }

    #[test]
    fn take_unknown_handle_returns_none_and_does_not_alter_state() {
        let mut c = Confirms::new();
        let h = c.register(req(1));
        assert!(c.take(999).is_none());
        assert!(!c.is_clear());
        assert!(c.get(h).is_some());
    }

    #[test]
    fn is_clear_flag_reflects_register_and_take_lifecycle() {
        let mut c = Confirms::new();
        let flag = c.is_clear_flag();
        assert!(flag.load(Ordering::Relaxed));
        let h = c.register(req(1));
        assert!(!flag.load(Ordering::Relaxed));
        c.take(h);
        assert!(flag.load(Ordering::Relaxed));
    }

    #[test]
    fn register_handle_increments_even_after_take() {
        let mut c = Confirms::new();
        let h1 = c.register(req(1));
        c.take(h1);
        let h2 = c.register(req(2));
        assert_ne!(h1, h2);
        assert!(h2 > h1);
    }
}
