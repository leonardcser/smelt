//! Typed effect ops applied by the app reducer.
//!
//! `AppOp` is the single funnel for deferred state mutations. Lua
//! handlers queue ops via `smelt.api.*` bindings; the app drains them
//! each tick and dispatches through `App::apply_ops`. Future phases
//! route Rust-side UI callbacks (dialogs, keymaps) through the same
//! enum so there is one reducer, one mutation log.

/// Deferred mutation queued by a handler. Applied by the app loop
/// after the handler returns, avoiding nested borrows on `App`.
pub enum AppOp {
    Notify(String),
    NotifyError(String),
    RunCommand(String),
    SetMode(String),
    SetModel(String),
    SetReasoningEffort(String),
    Cancel,
    Compact(Option<String>),
    Submit(String),
    SetPromptSection(String, String),
    RemovePromptSection(String),
    SetPermissionOverrides(protocol::PermissionOverrides),
    /// Background LLM call from a Lua plugin.
    BackgroundAsk {
        id: u64,
        system: String,
        messages: Vec<protocol::Message>,
        task: Option<String>,
    },
    /// Set ghost text (predicted input) on the prompt.
    SetGhostText(String),
    /// Clear ghost text from the prompt.
    ClearGhostText,
    BufCreate {
        id: u64,
    },
    BufSetLines {
        id: u64,
        lines: Vec<String>,
    },
    WinOpenFloat {
        buf_id: u64,
        title: String,
        footer_items: Vec<String>,
        accent: Option<crossterm::style::Color>,
    },
    WinUpdate {
        id: u64,
        title: Option<String>,
    },
    WinClose {
        id: u64,
    },
    /// Send a deferred plugin tool result.
    ResolveToolResult {
        request_id: u64,
        call_id: String,
        content: String,
        is_error: bool,
    },
}
