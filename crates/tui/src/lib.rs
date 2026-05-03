pub mod alloc;
pub(crate) mod api;
pub mod app;
pub(crate) mod commands;
pub(crate) mod completer;
pub(crate) mod content;
pub(crate) mod format;
pub use smelt_core::fuzzy;
pub mod instructions;
pub(crate) mod keymap;
pub mod lua;
pub use smelt_core::mcp;
pub(crate) mod metrics;
pub use smelt_core::perf;
pub(crate) mod persist;
pub(crate) mod picker;
pub(crate) mod prompt_sections;
pub(crate) mod sleep_inhibit;
pub use content::highlight::warm_up_syntect;
pub use smelt_core::state;
pub(crate) mod input;
pub mod theme;
pub(crate) mod ui;
pub(crate) mod window;

pub use smelt_core::attachment;
pub use smelt_core::session;

pub fn print_resume_hint(session_id: &str) {
    use crossterm::style::{Attribute, Print, SetAttribute};
    use crossterm::QueueableCommand;
    use std::io::Write;

    let mut out = std::io::stdout();
    let _ = out.queue(SetAttribute(Attribute::Dim));
    let _ = out.queue(Print(format!(
        "\nresume with:\nagent --resume {session_id}\n\n"
    )));
    let _ = out.queue(SetAttribute(Attribute::Reset));
    let _ = out.flush();
}
