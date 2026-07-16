use crate::app::TuiApp;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SessionCwdRestore {
    Current,
    Missing,
    Restored,
    Fallback {
        requested: String,
        fallback: String,
        error: String,
    },
}

pub(crate) struct PendingCwdChange {
    path: std::path::PathBuf,
    mark_session_dirty: bool,
}

pub(crate) struct StagedProcessCwd {
    previous_cwd: std::path::PathBuf,
    previous_pwd: Option<std::ffi::OsString>,
    cwd: std::path::PathBuf,
    committed: bool,
}

impl StagedProcessCwd {
    pub(crate) fn cwd(&self) -> &std::path::Path {
        &self.cwd
    }

    pub(crate) fn commit(mut self) {
        self.committed = true;
    }
}

impl Drop for StagedProcessCwd {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        let _ = std::env::set_current_dir(&self.previous_cwd);
        match &self.previous_pwd {
            Some(pwd) => std::env::set_var("PWD", pwd),
            None => std::env::remove_var("PWD"),
        }
    }
}

impl TuiApp {
    /// Request a coherent project-context transition. Only the latest target
    /// is retained, and it commits after the Lua callback returns and the app
    /// reaches a safe point. Active turns retain their cwd and static policy.
    pub(crate) fn change_cwd(
        &mut self,
        path: std::path::PathBuf,
    ) -> Result<(String, bool), String> {
        let path = Self::resolve_cwd_target(path)?;
        let target = path.to_string_lossy().into_owned();
        self.pending_cwd_change = Some(PendingCwdChange {
            path,
            mark_session_dirty: true,
        });
        Ok((target, true))
    }

    fn resolve_cwd_target(path: std::path::PathBuf) -> Result<std::path::PathBuf, String> {
        let path = std::fs::canonicalize(&path)
            .map_err(|error| format!("resolve cwd {}: {error}", path.display()))?;
        if !path.is_dir() {
            return Err(format!("cwd is not a directory: {}", path.display()));
        }
        Ok(path)
    }

    pub(crate) fn try_perform_scheduled_cwd_change(&mut self) -> bool {
        if self.pending_cwd_change.is_none()
            || self.prompt_input_is_busy()
            || self.ui.active_modal().is_some()
        {
            return false;
        }
        let pending = self
            .pending_cwd_change
            .take()
            .expect("pending cwd checked above");
        let requested = pending.path.to_string_lossy().into_owned();
        if let Some(error) = self.bring_up_lua_for_cwd(pending.path, pending.mark_session_dirty) {
            if pending.mark_session_dirty {
                self.notify_error_sticky(format!("cwd change: {error}"));
            } else {
                let fallback = self.cwd.clone();
                if !self.session_is_read_only() {
                    self.apply_session_cwd(fallback.clone(), false);
                }
                self.notify_error_sticky(format!(
                    "session cwd unavailable: {requested}: {error}; using {fallback}"
                ));
            }
        } else if self.notification.as_ref().is_some_and(|notification| {
            notification.summary.starts_with("cwd change:")
                || notification.summary.starts_with("session cwd unavailable:")
        }) {
            self.dismiss_notification();
        }
        true
    }

    /// Restore the working directory stored on a loaded session. Unlike
    /// `change_cwd`, this is not a user-visible directory switch inside the
    /// conversation, so it updates runtime state without appending a new context
    /// note or marking the restored session dirty.
    pub(crate) fn restore_session_cwd(&mut self, cwd: Option<&str>) -> SessionCwdRestore {
        let Some(cwd) = cwd.map(str::trim).filter(|cwd| !cwd.is_empty()) else {
            if !self.session_is_read_only() {
                self.apply_session_cwd(self.cwd.clone(), false);
            }
            return SessionCwdRestore::Missing;
        };
        if cwd == self.cwd {
            if !self.session_is_read_only() {
                self.apply_session_cwd(self.cwd.clone(), false);
            }
            return SessionCwdRestore::Current;
        }

        match Self::resolve_cwd_target(std::path::PathBuf::from(cwd)) {
            Ok(path) => {
                self.pending_cwd_change = Some(PendingCwdChange {
                    path,
                    mark_session_dirty: false,
                });
                SessionCwdRestore::Restored
            }
            Err(error) => {
                let fallback = self.cwd.clone();
                if !self.session_is_read_only() {
                    self.apply_session_cwd(fallback.clone(), false);
                }
                SessionCwdRestore::Fallback {
                    requested: cwd.to_string(),
                    fallback,
                    error,
                }
            }
        }
    }

    pub(crate) fn stage_process_cwd(path: std::path::PathBuf) -> Result<StagedProcessCwd, String> {
        let path = std::fs::canonicalize(&path).unwrap_or(path);
        let previous_cwd = std::env::current_dir().map_err(|error| error.to_string())?;
        let previous_pwd = std::env::var_os("PWD");
        std::env::set_current_dir(&path)
            .map_err(|error| format!("set cwd {}: {error}", path.display()))?;
        let cwd = std::env::current_dir().unwrap_or(path);
        std::env::set_var("PWD", &cwd);
        Ok(StagedProcessCwd {
            previous_cwd,
            previous_pwd,
            cwd,
            committed: false,
        })
    }

    pub(crate) fn install_runtime_cwd(
        &mut self,
        cwd: std::path::PathBuf,
        mark_session_dirty: bool,
    ) {
        self.cwd = cwd.to_string_lossy().into_owned();
        self.core.env.set_cwd(cwd.clone());
        if !self.session_is_read_only() {
            self.apply_session_cwd(self.cwd.clone(), mark_session_dirty);
        }
        self.refresh_cwd_status();
        self.core
            .signals
            .publish_if_changed("cwd", self.cwd.clone());
        self.core
            .signals
            .publish_if_changed("cwd_project", self.cwd_project.clone());
        self.core
            .signals
            .publish_if_changed("cwd_branch", self.cwd_branch.clone());
        self.core
            .signals
            .publish_if_changed("cwd_worktree", self.cwd_worktree.clone());
        self.core
            .signals
            .publish_if_changed("cwd_worktree_path", self.cwd_worktree_path.clone());
        self.core
            .signals
            .publish_if_changed("cwd_managed_worktree", self.cwd_managed_worktree);
        let branch = engine::paths::git_branch(&cwd).unwrap_or_default();
        self.core.signals.publish_if_changed("branch", branch);
    }

    fn apply_session_cwd(&mut self, cwd: String, mark_dirty: bool) {
        let mutation = if mark_dirty {
            crate::app::session_document::SessionMutation::SetCwd { cwd }
        } else {
            crate::app::session_document::SessionMutation::RestoreCwd { cwd }
        };
        self.apply_session_document_mutation(mutation);
    }

    pub(crate) fn publish_cwd_change(&mut self, user_visible: bool) {
        self.core.engine.send(protocol::UiCommand::SetCwd {
            cwd: self.cwd.clone(),
        });
        if user_visible && !self.session_is_read_only() {
            self.ensure_current_context_note();
            self.save_session();
        }
    }
}
