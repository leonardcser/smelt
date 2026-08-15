use crate::app::{NotificationOperation, TuiApp};

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

struct PendingCwdChange {
    path: std::path::PathBuf,
    mark_session_dirty: bool,
    tool_invocation: Option<smelt_core::lua::ToolInvocationContext>,
}

pub(crate) struct StagedCwdTransition {
    previous_cwd: std::path::PathBuf,
    previous_pwd: Option<std::ffi::OsString>,
    cwd: std::path::PathBuf,
    mark_session_dirty: bool,
    committed: bool,
}

impl StagedCwdTransition {
    pub(crate) fn stage(
        path: std::path::PathBuf,
        mark_session_dirty: bool,
    ) -> Result<Self, String> {
        let path = std::fs::canonicalize(&path).unwrap_or(path);
        let previous_cwd = std::env::current_dir().map_err(|error| error.to_string())?;
        let previous_pwd = std::env::var_os("PWD");
        std::env::set_current_dir(&path)
            .map_err(|error| format!("set cwd {}: {error}", path.display()))?;
        let cwd = std::env::current_dir().unwrap_or(path);
        std::env::set_var("PWD", &cwd);
        Ok(Self {
            previous_cwd,
            previous_pwd,
            cwd,
            mark_session_dirty,
            committed: false,
        })
    }

    pub(crate) fn commit(mut self, app: &mut TuiApp) -> bool {
        app.install_runtime_cwd(self.cwd.clone(), self.mark_session_dirty);
        self.committed = true;
        self.mark_session_dirty
    }
}

impl Drop for StagedCwdTransition {
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

pub(crate) struct WorkspaceState {
    cwd: String,
    home: std::path::PathBuf,
    context: smelt_core::worktree::ProjectContext,
    worktree_path: String,
    pending_change: Option<PendingCwdChange>,
}

impl WorkspaceState {
    pub(crate) fn new(
        cwd: String,
        home: std::path::PathBuf,
        worktree_root: &std::path::Path,
    ) -> Self {
        let context =
            smelt_core::worktree::project_context(std::path::Path::new(&cwd), Some(worktree_root));
        let worktree_path = worktree_display_path(&context, &home);
        Self {
            cwd,
            home,
            context,
            worktree_path,
            pending_change: None,
        }
    }

    pub(crate) fn cwd(&self) -> &str {
        &self.cwd
    }

    pub(crate) fn cwd_path(&self) -> &std::path::Path {
        std::path::Path::new(&self.cwd)
    }

    pub(crate) fn project(&self) -> &str {
        &self.context.project_name
    }

    pub(crate) fn branch(&self) -> &str {
        &self.context.branch
    }

    pub(crate) fn worktree(&self) -> &str {
        self.context.worktree_name.as_deref().unwrap_or_default()
    }

    pub(crate) fn worktree_path(&self) -> &str {
        &self.worktree_path
    }

    pub(crate) fn is_managed_worktree(&self) -> bool {
        self.context.managed_worktree
    }

    pub(crate) fn install_cwd(&mut self, cwd: std::path::PathBuf, worktree_root: &std::path::Path) {
        self.cwd = cwd.to_string_lossy().into_owned();
        self.refresh(worktree_root);
    }

    pub(crate) fn refresh(&mut self, worktree_root: &std::path::Path) {
        let context = smelt_core::worktree::project_context(self.cwd_path(), Some(worktree_root));
        self.install_context(context);
    }

    pub(crate) fn context_note(&self, worktree_root: &std::path::Path) -> String {
        smelt_core::context_notes::cwd_note(self.cwd_path(), worktree_root)
    }

    fn install_context(&mut self, context: smelt_core::worktree::ProjectContext) {
        self.worktree_path = worktree_display_path(&context, &self.home);
        self.context = context;
    }

    fn schedule(
        &mut self,
        path: std::path::PathBuf,
        mark_session_dirty: bool,
        tool_invocation: Option<smelt_core::lua::ToolInvocationContext>,
    ) {
        self.pending_change = Some(PendingCwdChange {
            path,
            mark_session_dirty,
            tool_invocation,
        });
    }

    fn pending(&self) -> Option<&PendingCwdChange> {
        self.pending_change.as_ref()
    }

    #[cfg(test)]
    pub(crate) fn has_pending_change(&self) -> bool {
        self.pending_change.is_some()
    }

    fn take_pending(&mut self) -> Option<PendingCwdChange> {
        self.pending_change.take()
    }

    fn discard_pending(&mut self) {
        self.pending_change = None;
    }
}

fn worktree_display_path(
    context: &smelt_core::worktree::ProjectContext,
    home: &std::path::Path,
) -> String {
    if !context.managed_worktree {
        return String::new();
    }
    if let Some(base_path) = context.base_path.as_deref() {
        if let Ok(suffix) = context.active_root.strip_prefix(base_path) {
            return suffix.display().to_string();
        }
    }
    engine::paths::collapse_tilde_from(&context.active_root, home)
        .display()
        .to_string()
}

impl TuiApp {
    /// Request a coherent project-context transition. Only the latest target
    /// is retained. Ordinary callers commit at the next idle safe point; model
    /// tool calls use their completion boundary as an explicit safe point.
    pub(crate) fn change_cwd(
        &mut self,
        path: std::path::PathBuf,
    ) -> Result<(String, bool), String> {
        let path = self.resolve_cwd_target(path)?;
        let target = path.to_string_lossy().into_owned();
        self.workspace
            .schedule(path, true, smelt_core::lua::current_tool_invocation());
        Ok((target, true))
    }

    fn resolve_cwd_target(&self, path: std::path::PathBuf) -> Result<std::path::PathBuf, String> {
        let path = if path.is_absolute() {
            path
        } else {
            self.core.env.cwd().join(path)
        };
        let path = std::fs::canonicalize(&path)
            .map_err(|error| format!("resolve cwd {}: {error}", path.display()))?;
        if !path.is_dir() {
            return Err(format!("cwd is not a directory: {}", path.display()));
        }
        Ok(path)
    }

    pub(crate) fn try_perform_scheduled_cwd_change(&mut self) -> bool {
        if self.workspace.pending().is_none()
            || self
                .workspace
                .pending()
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
            .workspace
            .pending()
            .and_then(|pending| pending.tool_invocation)
            != Some(invocation)
        {
            return Ok(false);
        }
        if !tool_succeeded {
            self.workspace.discard_pending();
            return Ok(false);
        }
        if invocation.execution_mode != protocol::ToolExecutionMode::Sequential {
            self.workspace.discard_pending();
            return Err("cwd-changing model tools must use sequential execution".into());
        }
        self.commit_pending_cwd_change()
    }

    pub(crate) fn discard_model_tool_cwd_change(&mut self) {
        if self
            .workspace
            .pending()
            .is_some_and(|pending| pending.tool_invocation.is_some())
        {
            self.workspace.discard_pending();
        }
    }

    /// Commit the pending transaction without applying the idle gate. Callers
    /// must own a safe boundary, such as a completed model tool callback.
    fn commit_pending_cwd_change(&mut self) -> Result<bool, String> {
        let Some(pending) = self.workspace.take_pending() else {
            return Ok(false);
        };
        let requested = pending.path.to_string_lossy().into_owned();
        if let Some(error) = self.bring_up_lua_for_cwd(pending.path, pending.mark_session_dirty) {
            let message = if pending.mark_session_dirty {
                format!("cwd change: {error}")
            } else {
                format!(
                    "session cwd unavailable: {requested}: {error}; using {}",
                    self.workspace.cwd()
                )
            };
            self.notify_operation_error_sticky(NotificationOperation::CwdChange, message.clone());
            return Err(message);
        }
        self.dismiss_operation_notification(&NotificationOperation::CwdChange);
        Ok(true)
    }

    /// Restore the working directory stored on a loaded session. Unlike
    /// `change_cwd`, this is not a user-visible directory switch inside the
    /// conversation, so it updates runtime state without appending a new context
    /// note or marking the restored session dirty.
    pub(crate) fn restore_session_cwd(&mut self, cwd: Option<&str>) -> SessionCwdRestore {
        let Some(cwd) = cwd.map(str::trim).filter(|cwd| !cwd.is_empty()) else {
            return SessionCwdRestore::Missing;
        };
        if cwd == self.workspace.cwd() {
            return SessionCwdRestore::Current;
        }

        match self.resolve_cwd_target(std::path::PathBuf::from(cwd)) {
            Ok(path) => {
                self.workspace.schedule(path, false, None);
                SessionCwdRestore::Restored
            }
            Err(error) => {
                let fallback = self.workspace.cwd().to_owned();
                SessionCwdRestore::Fallback {
                    requested: cwd.to_string(),
                    fallback,
                    error,
                }
            }
        }
    }

    fn install_runtime_cwd(&mut self, cwd: std::path::PathBuf, mark_session_dirty: bool) {
        self.core.env.set_cwd(cwd.clone());
        self.platform.install_cwd(cwd.clone());
        self.prompt.set_cwd(cwd.clone());
        self.workspace.install_cwd(
            cwd.clone(),
            std::path::Path::new(&self.core.config.settings.worktree_root),
        );
        if mark_session_dirty && !self.session_is_read_only() {
            self.conversation.set_cwd(self.workspace.cwd().to_owned());
        }
        self.core
            .signals
            .publish_if_changed("cwd", self.workspace.cwd().to_owned());
        self.core
            .signals
            .publish_if_changed("cwd_project", self.workspace.project().to_owned());
        self.core
            .signals
            .publish_if_changed("cwd_branch", self.workspace.branch().to_owned());
        self.core
            .signals
            .publish_if_changed("cwd_worktree", self.workspace.worktree().to_owned());
        self.core.signals.publish_if_changed(
            "cwd_worktree_path",
            self.workspace.worktree_path().to_owned(),
        );
        self.core
            .signals
            .publish_if_changed("cwd_managed_worktree", self.workspace.is_managed_worktree());
        let branch = engine::paths::git_branch(&cwd).unwrap_or_default();
        self.core.signals.publish_if_changed("branch", branch);
    }

    pub(crate) fn publish_cwd_change(&mut self, user_visible: bool) {
        self.publish_agent_project_context();
        if user_visible && !self.session_is_read_only() {
            self.ensure_current_context_note();
            self.save_session();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{worktree_display_path, StagedCwdTransition};

    #[test]
    fn staged_cwd_transition_rolls_back_when_not_committed() {
        let target = tempfile::TempDir::new().unwrap();
        let _environment = smelt_test_support::ProcessEnvironmentGuard::capture();
        let original_cwd = std::env::current_dir().unwrap();
        let original_pwd = std::env::var_os("PWD");
        let target = std::fs::canonicalize(target.path()).unwrap();

        {
            let _staged = StagedCwdTransition::stage(target.clone(), true).unwrap();
            assert_eq!(std::env::current_dir().unwrap(), target);
            assert_eq!(std::env::var_os("PWD").as_deref(), Some(target.as_os_str()));
        }

        assert_eq!(std::env::current_dir().unwrap(), original_cwd);
        assert_eq!(std::env::var_os("PWD"), original_pwd);
    }

    #[test]
    fn worktree_display_path_is_relative_to_project_root() {
        let context = smelt_core::worktree::ProjectContext {
            project_name: "smelt".into(),
            active_root: std::path::PathBuf::from("/home/dev/dev/smelt/.worktrees/test"),
            branch: "test".into(),
            managed_worktree: true,
            worktree_name: Some("test".into()),
            base_path: Some(std::path::PathBuf::from("/home/dev/dev/smelt")),
            allowed_roots: Vec::new(),
        };

        assert_eq!(
            worktree_display_path(&context, std::path::Path::new("/home/dev")),
            ".worktrees/test"
        );
    }
}
