pub mod alloc;
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
pub mod perf;
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

// Frontend-neutral document model lives in `smelt-buffer` so headless
// runtimes and non-smelt frontends (e.g. tcloc) can depend on it
// without pulling in engine / lua / http. Re-exported here so existing
// call sites (`smelt_core::buffer::Buffer`, etc.) keep resolving.
pub use smelt_buffer::{attachment, buffer, clipboard, kill_ring, undo};

mod theme_roles;

/// Style primitives — re-exported from the leaf `smelt-style` crate
/// (via `smelt-buffer`'s own re-export).
pub mod style {
    pub use smelt_buffer::style::*;
}

/// Theme registry — generic interner + Theme machinery from
/// `smelt-style`, plus the smelt-host-specific [`role_hl`] role
/// mapping table.
pub mod theme {
    pub use crate::theme_roles::role_hl;
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
