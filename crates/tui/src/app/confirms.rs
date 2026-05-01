//! `Confirms` — pending tool-approval dialog requests.
//!
//! Each entry pairs the live `ConfirmRequest` (tool name, args, desc,
//! optional outside-cwd dir + approval patterns) with the resolved
//! choice array the dialog presents. The dialog reads the request
//! payload from the `confirm_requested` cell (a typed
//! `ConfirmRequested` snapshot covering tool / desc / args / options
//! / cwd label / outside dir) and resolves it through a single
//! `smelt.confirm._resolve(handle_id, choice_idx, message)` primitive.
//! Two ancillary primitives (`_render_title`, `_back_tab`) are still
//! handle-keyed because they reach into Rust-only state (span-level
//! bash highlight, mode toggle + auto-allow check) — once they grow
//! Lua surfaces those will collapse onto the cell payload too.
//!
//! Engine generation pauses while confirms are pending; today's gate
//! runs through the focused overlay's `blocks_agent` flag.
//! [`Confirms::is_clear`] is the canonical predicate the
//! `EngineBridge` carve-out (P2.a.11) consumes to drain
//! `engine.event_rx` only when no dialog is open.

use std::collections::HashMap;

use crate::app::transcript_model::{ConfirmChoice, ConfirmRequest};

/// Live Confirm request held in `Confirms::pending` while the Lua
/// dialog is open. The choices array is populated by
/// `dialogs::confirm::build_options` so resolve can look up the
/// user's pick by index.
pub(crate) struct ConfirmEntry {
    pub(crate) req: ConfirmRequest,
    pub(crate) choices: Vec<ConfirmChoice>,
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
    /// plugin / statusline subscribers fan out from one signal, and
    /// the `EngineBridge` carve-out (P2.a.11) gates engine drain on
    /// it so streaming pauses while a confirm is open.
    pub(crate) fn is_clear(&self) -> bool {
        self.pending.is_empty()
    }
}
