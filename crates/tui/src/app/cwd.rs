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

pub(super) struct PreparedLuaInputs {
    pub(super) cwd: std::path::PathBuf,
    pub(super) project: smelt_core::worktree::ProjectContext,
    pub(super) prompt_inputs: crate::prompt_inputs::PromptInputs,
    pub(super) skills: std::sync::Arc<engine::SkillLoader>,
    pub(super) system_prompt_read_error: Option<String>,
    pub(super) permissions: smelt_core::permissions::PermissionContext,
}

pub(super) type LuaPreparation = std::sync::mpsc::Receiver<Result<PreparedLuaInputs, String>>;
pub(super) type ProjectPreparation =
    std::sync::mpsc::Receiver<Result<PreparedProjectContext, String>>;

pub(super) struct PreparedProjectContext {
    pub(super) cwd: std::path::PathBuf,
    pub(super) project: smelt_core::worktree::ProjectContext,
    pub(super) permissions: smelt_core::permissions::PermissionContext,
}

impl PreparedProjectContext {
    fn load(
        target: std::path::PathBuf,
        root: std::path::PathBuf,
        store: smelt_core::permissions::store::PermissionStore,
    ) -> Result<Self, String> {
        let cwd = std::fs::canonicalize(&target)
            .map_err(|error| format!("resolve cwd {}: {error}", target.display()))?;
        if !cwd.is_dir() {
            return Err(format!("cwd is not a directory: {}", cwd.display()));
        }
        let project = smelt_core::worktree::project_context(&cwd, Some(&root));
        let permissions =
            smelt_core::permissions::PermissionContext::load(&cwd, project.clone(), &store)
                .map_err(|error| format!("load persisted permissions: {error}"))?;
        Ok(Self {
            cwd,
            project,
            permissions,
        })
    }
}

fn spawn_workspace_preparation<T: Send + 'static>(
    wakeup: tokio::sync::mpsc::UnboundedSender<()>,
    work: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<std::sync::mpsc::Receiver<Result<T, String>>, String> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("smelt-workspace-prepare".into())
        .spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(work))
                .unwrap_or_else(|_| Err("workspace preparation worker panicked".into()));
            let _ = tx.send(result);
            let _ = wakeup.send(());
        })
        .map_err(|error| format!("prepare workspace: {error}"))?;
    Ok(rx)
}

struct PendingCwdChange {
    path: std::path::PathBuf,
    mark_session_dirty: bool,
    tool_invocation: Option<smelt_core::lua::ToolInvocationContext>,
    preparation: Option<LuaPreparation>,
    tool_completion: Option<(String, super::agent::LuaToolCompletion)>,
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

    pub(super) fn project_context(&self) -> smelt_core::worktree::ProjectContext {
        self.context.clone()
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

    pub(crate) fn repository_permission_context(
        &self,
    ) -> Option<(&std::path::Path, &std::path::Path)> {
        self.context.repository_key.as_deref().map(|key| {
            let display_root = self
                .context
                .base_path
                .as_deref()
                .unwrap_or(&self.context.active_root);
            (key, display_root)
        })
    }

    pub(crate) fn install_cwd(&mut self, cwd: std::path::PathBuf) {
        self.cwd = cwd.to_string_lossy().into_owned();
    }

    pub(crate) fn refresh(&mut self, worktree_root: &std::path::Path) {
        let context = smelt_core::worktree::project_context(self.cwd_path(), Some(worktree_root));
        self.install_context(context);
    }

    pub(crate) fn context_note(&self) -> String {
        smelt_core::context_notes::cwd_note_for_project(self.cwd_path(), &self.context)
    }

    pub(super) fn install_context(&mut self, context: smelt_core::worktree::ProjectContext) {
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
            preparation: None,
            tool_completion: None,
        });
    }

    fn pending(&self) -> Option<&PendingCwdChange> {
        self.pending_change.as_ref()
    }

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
        let invocation = smelt_core::lua::current_tool_invocation();
        if self.workspace.pending().is_some_and(|pending| {
            pending.tool_invocation.is_some() && pending.tool_invocation != invocation
        }) {
            return Err("a model tool owns the pending cwd transition".into());
        }
        let path = self.resolve_cwd_target(path)?;
        let target = path.to_string_lossy().into_owned();
        self.workspace.schedule(path, true, invocation);
        Ok((target, true))
    }

    fn resolve_cwd_target(&self, path: std::path::PathBuf) -> Result<std::path::PathBuf, String> {
        let path = smelt_core::path::resolve_from(path, self.core.env.cwd(), self.core.env.home());
        let path = std::fs::canonicalize(&path)
            .map_err(|error| format!("resolve cwd {}: {error}", path.display()))?;
        if !path.is_dir() {
            return Err(format!("cwd is not a directory: {}", path.display()));
        }
        Ok(path)
    }

    pub(super) fn prepare_lua_inputs(
        &self,
        target: std::path::PathBuf,
        refresh_agent_inputs: bool,
    ) -> Result<LuaPreparation, String> {
        let mut prompt_inputs = self.prompt_inputs.clone();
        let root = std::path::PathBuf::from(&self.core.config.settings.worktree_root);
        let permission_store = self.core.permission_store.clone();
        spawn_workspace_preparation(self.lua.wakeup_sender(), move || {
            let PreparedProjectContext {
                cwd,
                project,
                permissions,
            } = PreparedProjectContext::load(target, root, permission_store)?;
            let (skills, system_prompt_read_error) = if refresh_agent_inputs {
                let outcome = prompt_inputs.refresh(&cwd);
                (outcome.loader, outcome.system_prompt_read_error)
            } else {
                (prompt_inputs.skill_loader_for_cwd(&cwd), None)
            };
            Ok(PreparedLuaInputs {
                cwd,
                project,
                prompt_inputs,
                skills,
                system_prompt_read_error,
                permissions,
            })
        })
    }

    pub(super) fn prepare_project_context(&self) -> Result<ProjectPreparation, String> {
        let cwd = self.core.env.cwd();
        let root = std::path::PathBuf::from(&self.core.config.settings.worktree_root);
        let store = self.core.permission_store.clone();
        spawn_workspace_preparation(self.lua.wakeup_sender(), move || {
            PreparedProjectContext::load(cwd, root, store)
        })
    }

    pub(super) fn install_prepared_project_context(
        &mut self,
        prepared: PreparedProjectContext,
    ) -> bool {
        let previous_context = self.current_context_note_text();
        let desired = self.lua.desired();
        let permissions = prepared.permissions.resolve(
            &desired.permissions.rules,
            &desired.permissions.tool_defaults,
            desired.modes.behaviors.clone(),
            &self.core.config.settings,
            self.core.env.home(),
            self.core.permissions.paths_fn(),
        );
        self.core.permissions.apply_resolution(permissions);
        self.workspace.install_context(prepared.project);
        self.refresh_active_turn_permissions();
        self.publish_workspace_signals();
        let context_changed = previous_context != self.current_context_note_text();
        if context_changed {
            self.ensure_current_context_note();
            self.apply_pending_history_appends_for_request();
        }
        context_changed
    }

    pub(crate) fn try_perform_scheduled_cwd_change(&mut self) -> bool {
        let Some(pending) = self.workspace.pending() else {
            return false;
        };
        if pending.tool_invocation.is_some() {
            if pending.tool_completion.is_none() {
                return false;
            }
        } else if self.prompt_input_is_busy() || self.ui.active_modal().is_some() {
            return false;
        }
        if pending.preparation.is_none() {
            let preparation = self.prepare_lua_inputs(pending.path.clone(), true);
            match preparation {
                Ok(preparation) => {
                    self.workspace.pending_change.as_mut().unwrap().preparation = Some(preparation);
                    return true;
                }
                Err(error) => return self.finish_pending_cwd_change(Err(error)),
            }
        }
        let result = match pending.preparation.as_ref().unwrap().try_recv() {
            Ok(result) => result,
            Err(std::sync::mpsc::TryRecvError::Empty) => return false,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                Err("workspace preparation worker stopped".into())
            }
        };
        if result
            .as_ref()
            .is_ok_and(|prepared| !prepared.permissions.is_current())
        {
            self.workspace.pending_change.as_mut().unwrap().preparation = None;
            return true;
        }
        self.finish_pending_cwd_change(result)
    }

    fn finish_pending_cwd_change(&mut self, prepared: Result<PreparedLuaInputs, String>) -> bool {
        let pending = self.workspace.take_pending().unwrap();
        let requested = pending.path.to_string_lossy().into_owned();
        let result = prepared
            .and_then(|prepared| {
                match self.bring_up_lua_for_cwd(prepared, pending.mark_session_dirty) {
                    Some(error) => Err(error.to_string()),
                    None => Ok(true),
                }
            })
            .map_err(|error| {
                let message = if pending.mark_session_dirty {
                    format!("cwd change: {error}")
                } else {
                    format!(
                        "session cwd unavailable: {requested}: {error}; using {}",
                        self.workspace.cwd()
                    )
                };
                self.notify_operation_error_sticky(
                    NotificationOperation::CwdChange,
                    message.clone(),
                );
                message
            });
        if result.is_ok() {
            self.dismiss_operation_notification(&NotificationOperation::CwdChange);
        }
        if let (Some(invocation), Some((call_id, completion))) =
            (pending.tool_invocation, pending.tool_completion)
        {
            self.finish_lua_tool(invocation, call_id, completion, result);
        }
        true
    }

    pub(super) fn park_cwd_tool_completion(
        &mut self,
        call_id: String,
        completion: super::agent::LuaToolCompletion,
    ) {
        self.workspace
            .pending_change
            .as_mut()
            .unwrap()
            .tool_completion = Some((call_id, completion));
    }

    /// Check whether this result must wait for its cwd transaction. A direct Lua
    /// request cannot be pulled forward by an unrelated tool completion.
    pub(super) fn defer_tool_cwd_result(
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
        Ok(true)
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

    /// Restore the working directory stored on a loaded session. Unlike
    /// `change_cwd`, this is not a user-visible directory switch inside the
    /// conversation, so it updates runtime state without appending a new context
    /// note or marking the restored session dirty.
    pub(crate) fn restore_session_cwd(&mut self, cwd: Option<&str>) -> SessionCwdRestore {
        self.workspace.discard_pending();
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
        self.workspace.install_cwd(cwd.clone());
        if mark_session_dirty && !self.session_is_read_only() {
            self.conversation.set_cwd(self.workspace.cwd().to_owned());
        }
        self.publish_workspace_signals();
    }

    fn publish_workspace_signals(&mut self) {
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
        self.core
            .signals
            .publish_if_changed("branch", self.workspace.branch().to_owned());
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
    fn prepared_tool_cwd_retains_input_and_discards_cancelled_results() {
        use crate::app::test_harness::{test_environment_guard, Action, SourceEvent, TestApp};

        for cancel in [false, true] {
            let environment = test_environment_guard();
            let target = tempfile::TempDir::new().unwrap();
            let target = std::fs::canonicalize(target.path()).unwrap();
            let mut app = TestApp::builder().build_with_test_environment_guard(&environment);
            let original_cwd = app.core_probe().env.cwd();
            let original_generation = app.lua_probe().id;
            let prepared = app
                .app
                .prepare_lua_inputs(target.clone(), true)
                .unwrap()
                .recv_timeout(std::time::Duration::from_secs(10))
                .unwrap()
                .unwrap();
            let invocation = smelt_core::lua::ToolInvocationContext {
                invocation_id: protocol::InvocationId::new(91),
                request_id: 91,
                execution_mode: protocol::ToolExecutionMode::Sequential,
            };
            app.start_turn(1);
            app.app
                .workspace
                .schedule(target.clone(), true, Some(invocation));
            app.complete_lua_tool(invocation, "prepared-cwd".into(), "ok".into(), false, None);
            let (tx, rx) = std::sync::mpsc::channel();
            app.app
                .workspace
                .pending_change
                .as_mut()
                .unwrap()
                .preparation = Some(rx);
            app.clear_actions();

            assert!(app.run_lua(
                "local ok, err = pcall(smelt.session.switch_cwd, smelt.session.cwd()); assert(not ok and tostring(err):find('owns the pending cwd transition'))"
            ));
            app.type_text("input while preparing");
            assert_eq!(app.state().prompt_text, "input while preparing");
            assert_eq!(app.core_probe().env.cwd(), original_cwd);
            assert!(!app.actions().iter().any(|action| matches!(action,
                Action::EngineSend(command) if matches!(command.as_ref(), protocol::UiCommand::ToolResult { .. }))));

            if cancel {
                app.discard_turn(crate::app::TurnEnd::Cancelled);
                assert!(tx.send(Ok(prepared)).is_err());
                app.feed_one(SourceEvent::LuaWakeup);
                assert_eq!(app.core_probe().env.cwd(), original_cwd);
                assert_eq!(app.lua_probe().id, original_generation);
                assert!(!app.app.workspace.has_pending_change());
                assert!(!app.actions().iter().any(|action| matches!(action,
                    Action::EngineSend(command) if matches!(command.as_ref(), protocol::UiCommand::ToolResult { .. }))));
            } else {
                app.app
                    .core
                    .permission_store
                    .add_tool(
                        &target.to_string_lossy(),
                        smelt_core::permissions::store::PersistenceScope::Workspace,
                        "bash",
                        vec![],
                    )
                    .unwrap();
                assert!(tx.send(Ok(prepared)).is_ok());
                app.feed_one(SourceEvent::LuaWakeup);
                assert_eq!(app.core_probe().env.cwd(), original_cwd);
                assert_eq!(app.lua_probe().id, original_generation);
                app.wait_for_tool_result("prepared-cwd");
                assert_eq!(app.core_probe().env.cwd(), target);
                assert_eq!(app.lua_probe().id, original_generation.wrapping_add(1));
            }
            assert_eq!(app.state().prompt_text, "input while preparing");
        }
    }

    #[test]
    fn restoring_current_session_cwd_discards_another_pending_transition() {
        use crate::app::test_harness::{test_environment_guard, TestApp};

        let environment = test_environment_guard();
        let target = tempfile::tempdir().unwrap();
        let mut app = TestApp::builder().build_with_test_environment_guard(&environment);
        let original_cwd = app.core_probe().env.cwd();
        app.app
            .workspace
            .schedule(target.path().to_owned(), true, None);
        assert_eq!(
            app.app.restore_session_cwd(original_cwd.to_str()),
            super::SessionCwdRestore::Current
        );
        app.drain_idle_work();
        assert!(!app.app.workspace.has_pending_change());
        assert_eq!(app.core_probe().env.cwd(), original_cwd);
    }

    #[test]
    fn worktree_display_path_is_relative_to_project_root() {
        let context = smelt_core::worktree::ProjectContext {
            project_name: "smelt".into(),
            active_root: std::path::PathBuf::from("/home/dev/dev/smelt/.worktrees/test"),
            branch: "test".into(),
            default_base: "main".into(),
            default_base_path: Some(std::path::PathBuf::from("/home/dev/dev/smelt")),
            managed_worktree: true,
            worktree_name: Some("test".into()),
            base_path: Some(std::path::PathBuf::from("/home/dev/dev/smelt")),
            repository_key: Some(std::path::PathBuf::from("/home/dev/dev/smelt/.git")),
            allowed_roots: Vec::new(),
        };

        assert_eq!(
            worktree_display_path(&context, std::path::Path::new("/home/dev")),
            ".worktrees/test"
        );
    }
}
