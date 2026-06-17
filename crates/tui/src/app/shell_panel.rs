use crate::app::{ShellPanel, TuiApp};
use crate::commands::ShellSink;

const SHELL_PANEL_MAX_LINES: usize = 10_000;

impl TuiApp {
    pub(crate) fn shell_panel_is_focused(&self) -> bool {
        let Some(panel) = self.shell_panel else {
            return false;
        };
        self.ui.focused_overlay() == Some(panel.overlay)
    }

    pub(crate) fn close_shell_panel(&mut self) -> bool {
        let Some(panel) = self.shell_panel.take() else {
            return false;
        };
        self.close_overlay(panel.overlay);
        true
    }

    pub(crate) fn close_shell_panel_and_stop_job(&mut self) -> bool {
        if self
            .exec
            .as_ref()
            .is_some_and(|handle| handle.sink == ShellSink::Overlay)
        {
            if let Some(handle) = self.exec.take() {
                handle.kill.notify_one();
            }
        }
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
            crate::smelt_edit::Border::single().top(smelt_buffer::theme::intern("SmeltBar"));
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
        self.shell_panel = Some(ShellPanel { overlay, win, buf });
    }

    pub(crate) fn append_shell_output(&mut self, line: &str, sink: ShellSink) {
        match sink {
            ShellSink::Transcript => self.append_exec_output(line),
            ShellSink::Overlay => self.append_shell_panel_line(line),
        }
    }

    pub(crate) fn finish_shell_output(&mut self, code: Option<i32>, sink: ShellSink) {
        match sink {
            ShellSink::Transcript => {
                self.finish_exec(code);
                self.finalize_exec();
            }
            ShellSink::Overlay => {
                let status = match code {
                    Some(0) => "[exit 0]".to_string(),
                    Some(code) => format!("[exit {code}]"),
                    None => "[process exited]".to_string(),
                };
                self.append_shell_panel_line("");
                self.append_shell_panel_line(&status);
            }
        }
    }

    fn append_shell_panel_line(&mut self, line: &str) {
        let Some(panel) = self.shell_panel else {
            return;
        };
        let Some(buf) = self.ui.buf_mut(panel.buf) else {
            self.shell_panel = None;
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
