//! Pending tool-approval dialog requests.
//! [`Confirms::is_clear`] gates engine event draining while a confirm dialog is open.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::transcript_model::ConfirmRequest;

pub struct ConfirmEntry {
    pub req: ConfirmRequest,
}

#[derive(Default)]
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
