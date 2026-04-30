//! `Confirms` — pending tool-approval dialog requests.
//!
//! Each entry pairs the live `ConfirmRequest` (tool name, args, desc,
//! optional outside-cwd dir + approval patterns) with the resolved
//! choice array the dialog presents. The Lua dialog reads the entry
//! back through `smelt.confirm._*` primitives and resolves it on
//! submit / dismiss.
//!
//! Engine generation pauses while confirms are pending; today's
//! gate runs through the focused overlay's `blocks_agent` flag.
//! `Confirms::is_clear()` lands alongside the `EngineBridge` carve-out
//! in P2.a.11, where it replaces that overlay-shaped gate.

use std::collections::HashMap;

use crate::app::transcript_model::{ConfirmChoice, ConfirmRequest};

/// Live Confirm request held in `Confirms::pending` while the Lua
/// dialog is open. The choices array is populated by
/// `dialogs::confirm::build_options` so resolve can look up the
/// user's pick by index.
pub(crate) struct ConfirmEntry {
    pub req: ConfirmRequest,
    pub choices: Vec<ConfirmChoice>,
}

#[derive(Default)]
pub(crate) struct Confirms {
    pending: HashMap<u64, ConfirmEntry>,
    next_handle: u64,
}

impl Confirms {
    pub(crate) fn new() -> Self {
        Self {
            pending: HashMap::new(),
            next_handle: 1,
        }
    }

    pub(crate) fn register(&mut self, req: ConfirmRequest, choices: Vec<ConfirmChoice>) -> u64 {
        let id = self.next_handle;
        self.next_handle = self.next_handle.wrapping_add(1);
        self.pending.insert(id, ConfirmEntry { req, choices });
        id
    }

    pub(crate) fn get(&self, id: u64) -> Option<&ConfirmEntry> {
        self.pending.get(&id)
    }

    pub(crate) fn take(&mut self, id: u64) -> Option<ConfirmEntry> {
        self.pending.remove(&id)
    }

    /// `true` when no dialog request is registered. The main-loop
    /// tick reads this to publish the `confirms_pending` cell so
    /// plugin / statusline subscribers fan out from one signal.
    pub(crate) fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }
}
