//! Dialog data types shared between the render layer and the
//! compositor-driven dialogs in `app/dialogs/`. The legacy
//! `trait Dialog`, `DialogResult`, `TextArea`, `ConfirmDialog`, and
//! `QuestionDialog` were all removed with the panel-framework
//! migration (Step 9.5b items 9–12).

pub(crate) mod confirm;
mod question;

pub use question::{parse_questions, Question, QuestionOption};

use crate::app::AgentToolEntry;
use std::sync::{Arc, Mutex};

/// Snapshot of a tracked agent's state, published by the main loop
/// and consumed by the agents dialog.
#[derive(Clone)]
pub struct AgentSnapshot {
    pub agent_id: String,
    pub prompt: Arc<String>,
    pub tool_calls: Vec<AgentToolEntry>,
    pub context_tokens: Option<u32>,
    pub cost_usd: f64,
}

/// Shared, live-updating list of agent snapshots.
pub type SharedSnapshots = Arc<Mutex<Vec<AgentSnapshot>>>;
