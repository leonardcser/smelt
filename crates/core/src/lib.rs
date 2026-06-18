// Self-alias so `#[derive(LuaOpts)]` / `#[derive(LuaAlias)]` emit
// `::smelt_core::...` paths that resolve both inside this crate and
// from downstream consumers.
extern crate self as smelt_core;

#[cfg(test)]
#[global_allocator]
static ALLOCATOR: smelt_perf::alloc::Counting = smelt_perf::alloc::Counting;

pub mod app_config;
pub mod commands;
pub mod config;
pub mod confirms;
pub mod content;
pub mod context_notes;
pub mod custom_commands;
pub mod engine_client;
pub mod file_ref;
pub mod fs;
pub mod fuzzy;
pub mod grep;
pub mod headless;
pub mod headless_app;
pub mod history;
pub mod host;
pub mod html;
pub mod http;
pub mod keymap;
pub mod lsp;
pub mod lua;
pub mod mcp;
pub mod messages;
pub mod notebook;
pub mod output_limit;
pub mod path;
pub mod path_display;
pub(crate) mod paused_timer;
pub mod permissions;
pub mod process;
pub mod public_status;
pub mod runtime;
pub mod session;
pub mod session_migration;
pub mod session_runtime;
pub mod signals;
pub mod state;
pub mod timers;
pub mod tools;
pub mod transcript_model;
pub mod trust;
pub mod utils;
pub mod working;
pub mod workspace_files;
pub mod worktree;

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
pub use clipboard::{Clipboard, NullSink, Sink};
pub(crate) use clipboard::{Osc52Sink, SystemSink};
pub use engine_client::EngineClient;
pub use headless::{ColorMode, HeadlessSink, OutputFormat};
pub use headless_app::HeadlessApp;
pub use runtime::{Core, FrontendKind};
pub use session::{ContextCheckpoint, Session};
pub use signals::Signals;
pub use timers::Timers;

pub use crate::transcript_model::{
    ApprovalScope, Block, BlockId, BlockOrigin, ConfirmChoice, ConfirmRequest, PermissionEntry,
    ToolOutput, ToolState, ToolStatus, TranscriptBlockDescriptor, TranscriptBlockRecord,
    TranscriptBlockRecordWithId, ViewState,
};
