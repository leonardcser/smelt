//! Prevent the system from sleeping while the agent is working.
//!
//! - macOS: runs `caffeinate -i` (prevent idle sleep)
//! - Linux: runs `systemd-inhibit` (prevent idle and suspend)
//! - Windows: calls `SetThreadExecutionState`
//!
//! On Unix the inhibitor wraps `cat` reading a pipe smelt holds open. Closing
//! the pipe ends `cat` and with it the inhibition, so it is released even when
//! smelt exits without cleaning up. A waiter thread spawns and reaps each
//! inhibitor, which keeps process creation off the UI thread.

#[cfg(not(target_os = "windows"))]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(not(target_os = "windows"))]
use std::sync::Arc;

pub(crate) struct SleepInhibitor {
    #[cfg(not(target_os = "windows"))]
    hold: Option<Hold>,
    /// Set once an inhibitor fails on its own, for example when the session
    /// may not inhibit sleep. Later turns then skip spawning one.
    #[cfg(not(target_os = "windows"))]
    unavailable: Arc<AtomicBool>,
    #[cfg(target_os = "windows")]
    active: bool,
}

impl SleepInhibitor {
    pub(crate) fn new() -> Self {
        Self {
            #[cfg(not(target_os = "windows"))]
            hold: None,
            #[cfg(not(target_os = "windows"))]
            unavailable: Arc::default(),
            #[cfg(target_os = "windows")]
            active: false,
        }
    }

    /// Prevent the system from idle-sleeping. Idempotent.
    pub(crate) fn acquire(&mut self) {
        #[cfg(not(target_os = "windows"))]
        {
            if self.hold.is_some() || self.unavailable.load(Ordering::Acquire) {
                return;
            }
            self.hold = hold_inhibitor(inhibitor_command(), Arc::clone(&self.unavailable));
        }
        #[cfg(target_os = "windows")]
        {
            if self.active {
                return;
            }
            set_execution_state(true);
            self.active = true;
        }
    }

    /// Allow the system to sleep again. Idempotent.
    pub(crate) fn release(&mut self) {
        #[cfg(not(target_os = "windows"))]
        {
            self.hold = None;
        }
        #[cfg(target_os = "windows")]
        {
            if self.active {
                set_execution_state(false);
                self.active = false;
            }
        }
    }
}

impl Drop for SleepInhibitor {
    fn drop(&mut self) {
        self.release();
    }
}

// ── Platform: Unix ───────────────────────────────────────────────────────────

/// Keeps an inhibitor running. Dropping it closes the inhibitor's input.
#[cfg(not(target_os = "windows"))]
struct Hold {
    _input: std::io::PipeWriter,
    released: Arc<AtomicBool>,
}

#[cfg(not(target_os = "windows"))]
impl Drop for Hold {
    fn drop(&mut self) {
        // Runs before `_input` closes, so the waiter sees a deliberate release.
        self.released.store(true, Ordering::Release);
    }
}

#[cfg(not(target_os = "windows"))]
fn hold_inhibitor(
    mut command: std::process::Command,
    unavailable: Arc<AtomicBool>,
) -> Option<Hold> {
    // Both ends are close-on-exec, so only the inhibitor's stdin keeps the
    // read end open in the child.
    let (output, input) = std::io::pipe().ok()?;
    let released = Arc::new(AtomicBool::new(false));
    command
        .stdin(output)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .current_dir("/");
    // Without a controlling terminal, helpers such as the polkit agent cannot
    // prompt on smelt's tty, and job-control signals stay away.
    smelt_core::process::without_controlling_terminal(&mut command);
    std::thread::Builder::new()
        .name("smelt-sleep-inhibit".into())
        .spawn({
            let released = Arc::clone(&released);
            move || {
                let status = command.spawn().and_then(|mut child| child.wait());
                if !status.is_ok_and(|status| status.success()) && !released.load(Ordering::Acquire)
                {
                    unavailable.store(true, Ordering::Release);
                }
            }
        })
        .ok()?;
    Some(Hold {
        _input: input,
        released,
    })
}

#[cfg(target_os = "macos")]
fn inhibitor_command() -> std::process::Command {
    let mut command = std::process::Command::new("caffeinate");
    command.args(["-i", "cat"]);
    command
}

#[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
fn inhibitor_command() -> std::process::Command {
    let mut command = std::process::Command::new("systemd-inhibit");
    // Without a graphical polkit agent, authorization would prompt for a
    // password on smelt's terminal. Fail instead; inhibition is best-effort.
    command.args([
        "--no-ask-password",
        "--what=idle:sleep",
        "--who=agent",
        "--why=Agent working",
        "cat",
    ]);
    command
}

// ── Platform: Windows ────────────────────────────────────────────────────────

#[cfg(target_os = "windows")]
fn set_execution_state(prevent_sleep: bool) {
    const ES_CONTINUOUS: u32 = 0x80000000;
    const ES_SYSTEM_REQUIRED: u32 = 0x00000001;

    extern "system" {
        fn SetThreadExecutionState(flags: u32) -> u32;
    }

    let flags = if prevent_sleep {
        ES_CONTINUOUS | ES_SYSTEM_REQUIRED
    } else {
        ES_CONTINUOUS
    };
    unsafe {
        SetThreadExecutionState(flags);
    }
}

#[cfg(all(test, not(target_os = "windows")))]
mod tests {
    use super::*;

    fn eventually(condition: impl Fn() -> bool) -> bool {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            if condition() {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        false
    }

    fn sh(script: &str) -> std::process::Command {
        let mut command = std::process::Command::new("sh");
        command.args(["-c", script]);
        command
    }

    #[test]
    fn inhibitor_that_fails_on_its_own_marks_inhibition_unavailable() {
        let unavailable = Arc::new(AtomicBool::new(false));
        let _hold = hold_inhibitor(sh("exit 1"), Arc::clone(&unavailable)).unwrap();
        assert!(eventually(|| unavailable.load(Ordering::Acquire)));
    }

    #[test]
    fn inhibitor_runs_in_its_own_session_without_a_terminal() {
        let dir = tempfile::tempdir().unwrap();
        let leader = dir.path().join("leader");
        let script = format!(
            r#"[ "$(ps -o sid= -p $$ | tr -d ' ')" = "$$" ] && touch '{}'"#,
            leader.display()
        );
        let _hold = hold_inhibitor(sh(&script), Arc::default()).unwrap();
        assert!(eventually(|| leader.exists()));
    }

    #[test]
    fn release_ends_the_inhibitor_without_marking_it_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let ended = dir.path().join("ended");
        let unavailable = Arc::new(AtomicBool::new(false));
        let script = format!("cat; touch '{}'; exit 3", ended.display());
        let hold = hold_inhibitor(sh(&script), Arc::clone(&unavailable)).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(!ended.exists(), "the inhibitor runs until released");

        drop(hold);
        assert!(eventually(|| ended.exists()));
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(!unavailable.load(Ordering::Acquire));
    }
}
