//! The shell commands smelt lists: jobs of its own supervisor, and commands a
//! native agent such as Claude Code runs inside its own process.

use crate::process::{JobInfo, JobOutput};
use crate::runtime::Core;
use protocol::{NativeProcess, UiCommand};

impl Core {
    /// Running jobs, then the native session's shell commands.
    pub fn processes(&self) -> Vec<JobInfo> {
        let mut processes = self.jobs.list();
        processes.extend(self.native_processes.iter().map(|process| JobInfo {
            id: process.id.clone(),
            pid: None,
            command: process.command.clone(),
            elapsed_secs: self.native_elapsed_secs(process),
        }));
        processes
    }

    /// How many processes `processes` lists.
    pub fn running_process_count(&self) -> usize {
        self.jobs.running_count() + self.native_processes.len()
    }

    /// The output of process `id` without draining it. A native shell command
    /// shows what it printed by its last read and is read again.
    pub fn process_output(&self, id: &str) -> Result<JobOutput, String> {
        let Some(process) = self.native_process(id) else {
            return self.jobs.snapshot_output(id);
        };
        self.send_native(|session_id| UiCommand::ReadNativeProcess {
            session_id,
            id: id.into(),
        });
        Ok(JobOutput {
            text: process.output.clone(),
            running: true,
            exit_code: None,
            termination: None,
            elapsed_secs: self.native_elapsed_secs(process),
            pid: None,
        })
    }

    /// Stops process `id` asynchronously.
    pub fn stop_process(&self, id: String) {
        if self.native_process(&id).is_some() {
            self.send_native(|session_id| UiCommand::StopNativeProcess { session_id, id });
            return;
        }
        let supervisor = self.jobs.clone();
        tokio::spawn(async move {
            let _ = supervisor.stop(&id).await;
        });
    }

    /// Leaves the latest foreground job, or else the native session's
    /// foreground shell commands, running in the background. Returns whether
    /// it asked for that.
    pub fn detach_foreground_process(&self) -> bool {
        self.jobs.detach_latest_foreground().requested()
            || (self
                .native_processes
                .iter()
                .any(|process| !process.background)
                && self
                    .send_native(|session_id| UiCommand::BackgroundNativeProcesses { session_id }))
    }

    fn native_process(&self, id: &str) -> Option<&NativeProcess> {
        self.native_processes
            .iter()
            .find(|process| process.id == id)
    }

    fn native_elapsed_secs(&self, process: &NativeProcess) -> u64 {
        engine::clock::unix_time_ms(self.clock.as_ref()).saturating_sub(process.started_at_ms)
            / 1000
    }

    fn send_native(&self, command: impl FnOnce(String) -> UiCommand) -> bool {
        let Some(session_id) = self.native_session.clone() else {
            return false;
        };
        self.engine.send(command(session_id));
        true
    }
}
