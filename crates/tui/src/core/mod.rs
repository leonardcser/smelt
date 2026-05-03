#![allow(clippy::module_inception)]

pub(crate) mod app_config;
pub(crate) mod cells;
pub(crate) mod clipboard;
pub(crate) mod commands;
pub(crate) mod confirms;
pub(crate) mod content;
pub(crate) mod core;
pub(crate) mod engine_client;
pub(crate) mod fs;
pub(crate) mod fuzzy;
pub(crate) mod grep;
pub(crate) mod headless;
pub(crate) mod headless_app;
pub(crate) mod history;
pub(crate) mod host;
pub(crate) mod html;
pub(crate) mod http;
pub(crate) mod kill_ring;
pub(crate) mod notebook;
pub(crate) mod path;
pub mod permissions;
pub(crate) mod process;
pub(crate) mod timers;
pub(crate) mod tools;
pub(crate) mod transcript_cache;
pub(crate) mod transcript_model;
pub(crate) mod transcript_present;
pub(crate) mod working;

pub use app_config::AppConfig;
pub use clipboard::{Clipboard, NullSink, Sink};
pub(crate) use clipboard::{Osc52Sink, SystemSink};
pub use core::{Core, FrontendKind};
pub use headless::{ColorMode, HeadlessSink, OutputFormat};
pub use headless_app::HeadlessApp;
pub(crate) use host::Host;

pub(crate) use crate::core::transcript_model::{
    ApprovalScope, Block, BlockId, ConfirmChoice, ConfirmRequest, PermissionEntry, ToolOutput,
    ToolState, ToolStatus, ViewState,
};
