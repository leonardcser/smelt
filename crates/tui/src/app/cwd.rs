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

impl TuiApp {
    /// Change the process and app working directory together. This is used by
    /// managed worktree entry: relative shell commands, session metadata,
    /// runtime cwd, and workspace permissions all move as one visible
    /// transition. Cacheable prompt inputs stay stable; the model learns the
    /// new cwd through a context note instead.
    pub(crate) fn change_cwd(&mut self, path: std::path::PathBuf) -> Result<(), String> {
        let cwd = self.set_process_and_runtime_cwd(path)?;
        self.refresh_workspace_permissions(&cwd);
        self.sync_inline_options();
        self.publish_cwd_change();
        Ok(())
    }

    /// Restore the working directory stored on a loaded session. Unlike
    /// `change_cwd`, this is not a user-visible directory switch inside the
    /// conversation, so it updates runtime state without appending a new context
    /// note or marking the restored session dirty.
    pub(crate) fn restore_session_cwd(&mut self, cwd: Option<&str>) -> SessionCwdRestore {
        let Some(cwd) = cwd.map(str::trim).filter(|cwd| !cwd.is_empty()) else {
            if !self.session_is_read_only() {
                self.core.session.cwd = Some(self.cwd.clone());
            }
            return SessionCwdRestore::Missing;
        };
        if cwd == self.cwd {
            if !self.session_is_read_only() {
                self.core.session.cwd = Some(self.cwd.clone());
            }
            return SessionCwdRestore::Current;
        }

        match self.set_process_and_runtime_cwd(std::path::PathBuf::from(cwd)) {
            Ok(path) => {
                self.refresh_workspace_permissions(&path);
                self.sync_inline_options();
                self.core.engine.send(protocol::UiCommand::SetCwd {
                    cwd: self.cwd.clone(),
                });
                SessionCwdRestore::Restored
            }
            Err(err) => {
                let fallback = self.cwd.clone();
                if !self.session_is_read_only() {
                    self.core.session.cwd = Some(fallback.clone());
                }
                SessionCwdRestore::Fallback {
                    requested: cwd.to_string(),
                    fallback,
                    error: err,
                }
            }
        }
    }

    fn set_process_and_runtime_cwd(
        &mut self,
        path: std::path::PathBuf,
    ) -> Result<std::path::PathBuf, String> {
        let path = std::fs::canonicalize(&path).unwrap_or(path);
        std::env::set_current_dir(&path).map_err(|e| format!("set cwd {}: {e}", path.display()))?;
        let cwd = std::env::current_dir().unwrap_or(path);
        std::env::set_var("PWD", &cwd);
        self.cwd = cwd.to_string_lossy().into_owned();
        self.core.env.set_cwd(cwd.clone());
        if !self.session_is_read_only() {
            self.core.session.cwd = Some(self.cwd.clone());
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
        Ok(cwd)
    }

    fn refresh_workspace_permissions(&mut self, cwd: &std::path::Path) {
        let worktree_root = std::path::Path::new(&self.core.config.settings.worktree_root);
        let ctx = smelt_core::worktree::project_context(cwd, Some(worktree_root));
        let mut permissions = self.core.permissions.as_ref().clone();
        let roots = ctx.allowed_roots.clone();
        permissions.set_allowed_roots(ctx.active_root, ctx.allowed_roots);
        let rules = smelt_core::permissions::store::load_for_roots(&self.cwd, &roots);
        let (ws_tools, ws_dirs) = smelt_core::permissions::store::into_approvals(&rules);
        permissions
            .approvals
            .write()
            .unwrap()
            .load_workspace(ws_tools, ws_dirs);
        self.core.permissions = std::sync::Arc::new(permissions);
        if let Some(turn) = self.agent.as_mut() {
            turn.permissions = self.core.permissions.clone();
        }
    }

    fn publish_cwd_change(&mut self) {
        self.core.engine.send(protocol::UiCommand::SetCwd {
            cwd: self.cwd.clone(),
        });
        if !self.session_is_read_only() {
            self.ensure_current_context_note();
            self.mark_session_dirty();
            self.save_session();
        }
    }
}
