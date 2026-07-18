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
    tool_invocation: Option<smelt_core::lua::ToolInvocationContext>,
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
    /// is retained. Ordinary callers commit at the next idle safe point; model
    /// tool calls use their completion boundary as an explicit safe point.
    pub(crate) fn change_cwd(
        &mut self,
        path: std::path::PathBuf,
    ) -> Result<(String, bool), String> {
        let path = Self::resolve_cwd_target(path)?;
        let target = path.to_string_lossy().into_owned();
        self.pending_cwd_change = Some(PendingCwdChange {
            path,
            mark_session_dirty: true,
            tool_invocation: smelt_core::lua::current_tool_invocation(),
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
            || self
                .pending_cwd_change
                .as_ref()
                .is_some_and(|pending| pending.tool_invocation.is_some())
            || self.prompt_input_is_busy()
            || self.ui.active_modal().is_some()
        {
            return false;
        }
        let _ = self.commit_pending_cwd_change();
        true
    }

    /// Commit a transaction requested by this model tool. A direct Lua request
    /// cannot be pulled forward accidentally by an unrelated tool completion.
    pub(crate) fn commit_tool_cwd_change(
        &mut self,
        invocation: smelt_core::lua::ToolInvocationContext,
        tool_succeeded: bool,
    ) -> Result<bool, String> {
        if self
            .pending_cwd_change
            .as_ref()
            .and_then(|pending| pending.tool_invocation)
            != Some(invocation)
        {
            return Ok(false);
        }
        if !tool_succeeded {
            self.pending_cwd_change = None;
            return Ok(false);
        }
        if invocation.execution_mode != protocol::ToolExecutionMode::Sequential {
            self.pending_cwd_change = None;
            return Err("cwd-changing model tools must use sequential execution".into());
        }
        self.commit_pending_cwd_change()
    }

    pub(crate) fn discard_model_tool_cwd_change(&mut self) {
        if self
            .pending_cwd_change
            .as_ref()
            .is_some_and(|pending| pending.tool_invocation.is_some())
        {
            self.pending_cwd_change = None;
        }
    }

    /// Commit the pending transaction without applying the idle gate. Callers
    /// must own a safe boundary, such as a completed model tool callback.
    fn commit_pending_cwd_change(&mut self) -> Result<bool, String> {
        let Some(pending) = self.pending_cwd_change.take() else {
            return Ok(false);
        };
        let requested = pending.path.to_string_lossy().into_owned();
        if let Some(error) = self.bring_up_lua_for_cwd(pending.path, pending.mark_session_dirty) {
            let message = if pending.mark_session_dirty {
                format!("cwd change: {error}")
            } else {
                let fallback = self.cwd.clone();
                if !self.session_is_read_only() {
                    self.apply_session_cwd(fallback.clone(), false);
                }
                format!("session cwd unavailable: {requested}: {error}; using {fallback}")
            };
            self.notify_error_sticky(message.clone());
            return Err(message);
        }
        if self.notification.as_ref().is_some_and(|notification| {
            notification.summary.starts_with("cwd change:")
                || notification.summary.starts_with("session cwd unavailable:")
        }) {
            self.dismiss_notification();
        }
        Ok(true)
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
                    tool_invocation: None,
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
        self.publish_agent_project_context();
        if user_visible && !self.session_is_read_only() {
            self.ensure_current_context_note();
            self.save_session();
        }
    }
}
