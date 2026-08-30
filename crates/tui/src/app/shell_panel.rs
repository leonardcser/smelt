use crate::app::{ShellPanel, TuiApp};
use crate::commands::ShellSink;

const SHELL_PANEL_MAX_LINES: usize = 10_000;

impl TuiApp {
    pub(crate) fn shell_panel_is_focused(&self) -> bool {
        let Some(panel) = self.overlays.shell_panel() else {
            return false;
        };
        self.ui.focused_overlay() == Some(panel.overlay)
    }

    pub(crate) fn close_shell_panel(&mut self) -> bool {
        let Some(panel) = self.overlays.take_shell_panel() else {
            return false;
        };
        self.close_overlay(panel.overlay);
        true
    }

    pub(crate) fn close_shell_panel_and_stop_job(&mut self) -> bool {
        self.overlays.cancel_execution_for_sink(ShellSink::Overlay);
        self.close_shell_panel()
    }

    pub(crate) fn open_shell_panel(&mut self, command: &str) {
        self.close_shell_panel();

        let buf = self
            .ui
            .buf_create(crate::smelt_edit::BufCreateOpts::default());
        if let Some(b) = self.ui.buf_mut(buf) {
            b.readonly = true;
            b.set_all_lines(vec![String::new()]);
        }

        let Some(win) = self.ui.win_open_split(
            buf,
            crate::smelt_edit::SplitConfig {
                region: "shell_output".into(),
                gutters: crate::smelt_edit::Gutters {
                    pad_left: 1,
                    pad_right: 1,
                    scrollbar: true,
                },
            },
        ) else {
            return;
        };
        if let Some(w) = self.ui.win_mut(win) {
            w.set_surface(crate::smelt_edit::WindowSurface::readonly_text());
            w.set_vim_enabled(self.core.config.settings.vim);
            w.pin_scroll(0);
        }

        let border =
            crate::smelt_edit::Border::single().top(smelt_buffer::theme::intern("SmeltSeparator"));
        let title = format!(" :!{command} ");
        let layout = crate::smelt_edit::LayoutTree::vbox(vec![(
            crate::smelt_edit::Constraint::Percentage(100),
            crate::smelt_edit::LayoutTree::leaf(win),
        )])
        .with_border(border)
        .with_title(title);
        let overlay = self.ui.overlay_open(
            crate::smelt_edit::Overlay::new(
                layout,
                crate::smelt_edit::layout::Anchor::ScreenBottom { above_rows: 1 },
            )
            .modal(true)
            .with_z(45)
            .resize_config(crate::smelt_edit::ResizeConfig {
                top: true,
                right: false,
                bottom: false,
                left: false,
                corners: false,
            })
            .with_height(crate::smelt_edit::Constraint::Percentage(30))
            .with_min_height(Some(crate::smelt_edit::Constraint::Length(4)))
            .with_width(crate::smelt_edit::Constraint::Percentage(100)),
        );
        self.ui.set_focus(win);
        self.overlays
            .install_shell_panel(ShellPanel { overlay, win, buf });
    }

    pub(crate) fn append_shell_output(&mut self, line: &str, sink: ShellSink) {
        match sink {
            ShellSink::Transcript => self.append_exec_output(line.to_owned()),
            ShellSink::Overlay => self.append_shell_panel_line(line),
        }
    }

    pub(crate) fn finish_shell_output(
        &mut self,
        output: String,
        exit_code: Option<i32>,
        termination: Option<protocol::JobTermination>,
        sink: ShellSink,
    ) {
        match sink {
            ShellSink::Transcript => self.finish_exec(Some(output)),
            ShellSink::Overlay => {
                self.replace_shell_panel_output(&output);
                let status = match termination {
                    Some(protocol::JobTermination::OutOfMemory) => "[out of memory]".to_string(),
                    Some(protocol::JobTermination::Signaled) => {
                        "[terminated by signal]".to_string()
                    }
                    Some(protocol::JobTermination::Stopped) => "[stopped]".to_string(),
                    Some(protocol::JobTermination::Exited) | None => match exit_code {
                        Some(0) => "[exit 0]".to_string(),
                        Some(code) => format!("[exit {code}]"),
                        None => "[process exited]".to_string(),
                    },
                };
                self.append_shell_panel_line("");
                self.append_shell_panel_line(&status);
            }
        }
    }

    fn replace_shell_panel_output(&mut self, output: &str) {
        let Some(panel) = self.overlays.shell_panel() else {
            return;
        };
        let Some(buf) = self.ui.buf_mut(panel.buf) else {
            self.overlays.clear_shell_panel();
            return;
        };
        buf.set_all_lines(output.split('\n').map(str::to_string).collect());
    }

    fn append_shell_panel_line(&mut self, line: &str) {
        let Some(panel) = self.overlays.shell_panel() else {
            return;
        };
        let Some(buf) = self.ui.buf_mut(panel.buf) else {
            self.overlays.clear_shell_panel();
            return;
        };
        let line_count = buf.line_count();
        let mut replacement: Vec<String> = line.split('\n').map(str::to_string).collect();
        if replacement.is_empty() {
            replacement.push(String::new());
        }
        let start = if line_count == 1 && buf.get_line(0) == Some("") {
            0
        } else {
            line_count
        };
        buf.set_lines(start, line_count, replacement);
        let overflow = buf.line_count().saturating_sub(SHELL_PANEL_MAX_LINES);
        if overflow > 0 {
            buf.set_lines(0, overflow, Vec::new());
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn shell_panel_open_output_finish_and_close_lifecycle() {
        let mut app = crate::app::test_harness::TestApp::builder().build().app;

        app.open_shell_panel("printf hello");
        let panel = app.overlays.shell_panel().expect("shell panel opens");
        assert!(app.shell_panel_is_focused());

        app.append_shell_output("hello\nworld", ShellSink::Overlay);
        app.finish_shell_output(
            "hello\nworld".into(),
            Some(3),
            Some(protocol::JobTermination::Exited),
            ShellSink::Overlay,
        );

        let lines = app.ui.buf(panel.buf).expect("shell panel buffer").lines();
        assert_eq!(lines, ["hello", "world", "", "[exit 3]"]);
        assert!(app.close_shell_panel());
        assert!(app.overlays.shell_panel().is_none());
        assert!(!app.shell_panel_is_focused());
        assert!(!app.close_shell_panel());
    }

    #[test]
    fn shell_panel_displays_typed_termination_status() {
        let mut app = crate::app::test_harness::TestApp::builder().build().app;
        app.open_shell_panel("oom");
        let panel = app.overlays.shell_panel().expect("shell panel opens");

        app.finish_shell_output(
            "partial output".into(),
            None,
            Some(protocol::JobTermination::OutOfMemory),
            ShellSink::Overlay,
        );

        let lines = app.ui.buf(panel.buf).expect("shell panel buffer").lines();
        assert_eq!(lines, ["partial output", "", "[out of memory]"]);
    }

    #[tokio::test]
    async fn closing_shell_panel_cancels_overlay_job_once() {
        let mut app = crate::app::test_harness::TestApp::builder().build().app;
        app.open_shell_panel("sleep 10");
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        drop(tx);
        let kill = tokio_util::sync::CancellationToken::new();
        app.overlays.install_execution(crate::commands::ExecHandle {
            rx,
            kill: kill.clone(),
            sink: ShellSink::Overlay,
        });

        assert!(app.close_shell_panel_and_stop_job());
        tokio::time::timeout(Duration::from_millis(100), kill.cancelled())
            .await
            .expect("closing the panel cancels the supervised job");
        assert!(!app.overlays.execution_is_running());
        assert!(!app.close_shell_panel_and_stop_job());
    }

    #[tokio::test]
    async fn real_shell_panel_streams_stdout_stderr_and_exit_status() {
        let mut test_app = crate::app::test_harness::TestApp::builder().build();
        let app = &mut test_app.app;
        let handle = app
            .start_shell_escape_with_sink(
                "printf 'stdout-line\\n'; printf 'stderr-line\\n' >&2; exit 7",
                ShellSink::Overlay,
            )
            .expect("shell command starts");
        app.overlays.install_execution(handle);

        loop {
            let event =
                tokio::time::timeout(Duration::from_secs(5), app.overlays.next_execution_event())
                    .await
                    .expect("shell command completes")
                    .expect("shell command sends terminal event");
            match event {
                crate::commands::ExecEvent::Output(line) => {
                    app.append_shell_output(&line, ShellSink::Overlay);
                }
                crate::commands::ExecEvent::Done {
                    output,
                    exit_code,
                    termination,
                } => {
                    app.finish_shell_output(output, exit_code, termination, ShellSink::Overlay);
                    app.overlays.finish_execution();
                    break;
                }
            }
        }

        let panel = app
            .overlays
            .shell_panel()
            .expect("shell panel remains open");
        let lines = app.ui.buf(panel.buf).expect("shell output buffer").lines();
        assert!(lines.iter().any(|line| line == "stdout-line"));
        assert!(lines.iter().any(|line| line == "stderr-line"));
        assert_eq!(lines.last().map(String::as_str), Some("[exit 7]"));
    }

    #[tokio::test]
    async fn real_shell_transcript_replaces_live_preview_with_final_output() {
        let mut test_app = crate::app::test_harness::TestApp::builder().build();
        let app = &mut test_app.app;
        let handle = app
            .start_shell_escape(
                "i=1; while [ $i -le 300 ]; do printf 'line-%03d\\n' $i; i=$((i + 1)); done",
            )
            .expect("shell command starts");
        app.overlays.install_execution(handle);

        loop {
            let event =
                tokio::time::timeout(Duration::from_secs(5), app.overlays.next_execution_event())
                    .await
                    .expect("shell command completes")
                    .expect("shell command sends terminal event");
            match event {
                crate::commands::ExecEvent::Output(line) => {
                    app.append_shell_output(&line, ShellSink::Transcript);
                }
                crate::commands::ExecEvent::Done {
                    output,
                    exit_code,
                    termination,
                } => {
                    app.finish_shell_output(output, exit_code, termination, ShellSink::Transcript);
                    app.overlays.finish_execution();
                    break;
                }
            }
        }

        assert!(app.render_pending_transient_frame_to(&mut std::io::sink()));
        let history = app.conversation.transcript().history();
        let block = history
            .last_block_id()
            .and_then(|id| history.block(id))
            .expect("completed exec block");
        let smelt_core::transcript_model::Block::Exec { output, .. } = block else {
            panic!("last block is exec output");
        };
        let output = output.snapshot();
        assert!(output.contains("line-001"));
        assert!(output.contains("line-300"));
        assert!(!output.contains("live process output truncated"));
    }

    #[test]
    fn shell_panel_enforces_the_retained_line_cap() {
        let mut app = crate::app::test_harness::TestApp::builder().build().app;
        app.open_shell_panel("many lines");
        for index in 0..SHELL_PANEL_MAX_LINES + 17 {
            app.append_shell_panel_line(&format!("line-{index}"));
        }

        let panel = app
            .overlays
            .shell_panel()
            .expect("shell panel remains open");
        let lines = app.ui.buf(panel.buf).expect("shell output buffer").lines();
        assert_eq!(lines.len(), SHELL_PANEL_MAX_LINES);
        assert_eq!(lines.first().map(String::as_str), Some("line-17"));
        assert_eq!(
            lines.last().map(String::as_str),
            Some(format!("line-{}", SHELL_PANEL_MAX_LINES + 16).as_str())
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancelling_real_shell_command_kills_and_reaps_its_process_group() {
        let mut test_app = crate::app::test_harness::TestApp::builder().build();
        let app = &mut test_app.app;
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("shell.pid");
        let command = format!("echo $$ > '{}'; sleep 30 & wait", pid_file.display());
        let mut handle = app
            .start_shell_escape_with_sink(&command, ShellSink::Overlay)
            .expect("shell command starts");

        let pid = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(text) = std::fs::read_to_string(&pid_file) {
                    if let Ok(pid) = text.trim().parse::<i32>() {
                        break pid;
                    }
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "shell did not write its pid: {:?}",
                std::fs::read_to_string(&pid_file)
            )
        });
        handle.kill.cancel();

        let done = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match handle.rx.recv().await {
                    Some(crate::commands::ExecEvent::Done {
                        exit_code,
                        termination,
                        ..
                    }) => break (exit_code, termination),
                    Some(crate::commands::ExecEvent::Output(_)) => {}
                    None => panic!("shell event channel closed before completion"),
                }
            }
        })
        .await
        .expect("cancelled shell is reaped");
        assert_eq!(done, (None, Some(protocol::JobTermination::Stopped)));
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
    }
}
