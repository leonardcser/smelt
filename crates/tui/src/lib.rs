/// Route the tui test binary through the counting global allocator so the
/// per-event allocation snapshots in tests see real numbers. Production
/// builds install the allocator in `src/main.rs`.
#[cfg(test)]
#[global_allocator]
static ALLOCATOR: smelt_perf::alloc::Counting = smelt_perf::alloc::Counting;

pub mod app;
pub mod auto_reload;
pub(crate) mod commands;
pub(crate) mod content;
pub mod event_source;
pub(crate) mod format;
pub use smelt_core::fuzzy;
pub mod instructions;
pub(crate) mod keymap;
pub mod lua;
pub use lua::VERSION_STRING;
pub use smelt_core::mcp;
pub use smelt_core::permissions;
pub(crate) mod metrics;
pub(crate) mod persist;
pub(crate) mod picker;
pub mod prompt_inputs;
pub(crate) mod prompt_sections;
pub(crate) mod sleep_inhibit;
pub use content::highlight::warm_up_syntect;
pub use smelt_core::state;
pub(crate) mod input;
pub(crate) mod term_setup;
pub mod theme;
pub use ::smelt_edit as smelt_term;

pub use smelt_core::attachment;
pub use smelt_core::lua::{CliFlagKind, CliFlagSpec, CliFlagValue};
pub use smelt_core::session;
