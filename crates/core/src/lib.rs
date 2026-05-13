// Self-alias so `#[derive(LuaOpts)]` / `#[derive(LuaAlias)]` emit
// `::smelt_core::...` paths that resolve both inside this crate and
// from downstream consumers.
extern crate self as smelt_core;

pub mod app_config;
pub mod cells;
pub mod config;
pub mod confirms;
pub mod content;
pub mod custom_commands;
pub mod engine_client;
pub mod fs;
pub mod fuzzy;
pub mod grep;
pub mod headless;
pub mod headless_app;
pub mod history;
pub mod host;
pub mod html;
pub mod http;
pub mod lua;
pub mod mcp;
pub mod messages;
pub mod notebook;
pub mod path;
pub mod permissions;
pub mod process;
pub mod runtime;
pub mod session;
pub mod state;
pub mod timers;
pub mod tools;
pub mod transcript_model;
pub mod trust;
pub mod utils;
pub mod working;

#[cfg(test)]
mod test_util;

// Re-exported from `smelt-buffer` so call sites (`smelt_core::buffer::Buffer`, etc.) keep resolving.
pub use smelt_buffer::{attachment, buffer, clipboard, kill_ring, undo};

/// Style primitives re-exported from `smelt-buffer`.
pub mod style {
    pub use smelt_buffer::style::*;
}

/// Theme registry re-export. Highlight groups follow nvim's PascalCase
/// convention (`Comment`, `SmeltAccent`, …); call `intern(name)` to get a
/// stable `HlGroup` id, then `theme.resolve(id)` for the current style.
pub mod theme {
    pub use smelt_buffer::theme::*;
}

pub use app_config::AppConfig;
pub use cells::Cells;
pub use clipboard::{Clipboard, NullSink, Sink};
pub(crate) use clipboard::{Osc52Sink, SystemSink};
pub use engine_client::EngineClient;
pub use headless::{ColorMode, HeadlessSink, OutputFormat};
pub use headless_app::HeadlessApp;
pub use runtime::{Core, FrontendKind};
pub use session::Session;
pub use timers::Timers;

pub use crate::transcript_model::{
    ApprovalScope, Block, BlockId, ConfirmChoice, ConfirmRequest, PermissionEntry, ToolOutput,
    ToolState, ToolStatus, ViewState,
};
