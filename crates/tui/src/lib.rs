/// Route the tui test binary through the counting global allocator so the
/// per-event allocation snapshots in tests see real numbers. Production
/// builds install the allocator in `src/main.rs`.
#[cfg(test)]
#[global_allocator]
static ALLOCATOR: smelt_perf::alloc::Counting = smelt_perf::alloc::Counting;

#[cfg(test)]
pub(crate) static COMMAND_RESOLVER_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
pub(crate) mod test_support {
    pub(crate) struct ProcessCwdGuard {
        cwd: std::path::PathBuf,
        pwd: Option<std::ffi::OsString>,
    }

    impl ProcessCwdGuard {
        pub(crate) fn capture() -> Self {
            Self {
                cwd: std::env::current_dir().expect("capture process cwd"),
                pwd: std::env::var_os("PWD"),
            }
        }
    }

    impl Drop for ProcessCwdGuard {
        fn drop(&mut self) {
            let _ = std::env::set_current_dir(&self.cwd);
            match &self.pwd {
                Some(pwd) => std::env::set_var("PWD", pwd),
                None => std::env::remove_var("PWD"),
            }
        }
    }
}

pub mod app;
pub mod auto_reload;
pub(crate) mod commands;
pub(crate) mod content;
pub mod event_source;
pub(crate) mod format;
pub mod inspect_server;
pub use smelt_core::fuzzy;
pub mod instructions;
pub(crate) mod keymap;
pub(crate) mod line_input;
pub mod lua;
pub use lua::DISPLAY;
pub use smelt_core::mcp;
pub use smelt_core::permissions;
pub(crate) mod metrics;
pub(crate) mod persist;
pub(crate) mod picker;
pub mod prompt_inputs;
pub(crate) mod sleep_inhibit;
pub use content::highlight::warm_up_syntect;
pub use smelt_core::state;
pub(crate) mod input;
pub(crate) mod term_input;
pub(crate) mod term_setup;
pub mod theme;
pub use ::smelt_edit;

pub use smelt_core::attachment;
pub use smelt_core::lua::{CliFlagKind, CliFlagSpec, CliFlagValue};
pub use smelt_core::session;
