//! Cooperative Lua task runtime. A task wraps `mlua::Thread`; it runs until it yields
//! a discriminated `{ __yield = "...", ... }` table and parks on a typed `TaskWait`.

use mlua::prelude::*;
use std::cell::RefCell;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

enum TaskWait {
    Ready(LuaMultiValue),
    Sleep(Instant),
    External { id: u64 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskScope {
    App,
    Turn,
    NativeSession,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandQueueTarget {
    Turn,
    Request,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ToolInvocationContext {
    pub invocation_id: protocol::InvocationId,
    pub request_id: u64,
    pub execution_mode: protocol::ToolExecutionMode,
}

#[derive(Debug, Clone)]
pub struct ToolEvaluationRequest {
    pub scope: TaskScope,
    pub request_id: u64,
    pub invocation_id: protocol::InvocationId,
    pub tool_name: String,
    pub args: std::collections::HashMap<String, serde_json::Value>,
    pub mode: protocol::AgentMode,
}

pub enum TaskCompletion {
    ToolEvaluation(Box<ToolEvaluationRequest>),
    FireAndForget,
    Child {
        invocation_id: protocol::InvocationId,
        invocation: Option<ToolInvocationContext>,
    },
    /// Slash-command dispatch. `name` carries the cmd name so an error
    /// surfaces as `cmd `<name>`: …` instead of the opaque `task <id>: …`.
    Command {
        name: String,
        queue_target: CommandQueueTarget,
        sent_at_ms: Option<u64>,
    },
    ToolResult {
        invocation: ToolInvocationContext,
        call_id: String,
    },
}

impl TaskCompletion {
    fn invocation_id(&self) -> Option<protocol::InvocationId> {
        match self {
            Self::ToolEvaluation(request) => Some(request.invocation_id),
            Self::ToolResult { invocation, .. } => Some(invocation.invocation_id),
            Self::Child { invocation_id, .. } => Some(*invocation_id),
            _ => None,
        }
    }

    fn notification_label(&self, task_id: u64) -> String {
        match self {
            TaskCompletion::Command { name, .. } => format!("cmd `{name}`"),
            _ => format!("task {task_id}"),
        }
    }

    fn command_queue_target(&self) -> Option<CommandQueueTarget> {
        match self {
            TaskCompletion::Command { queue_target, .. } => Some(*queue_target),
            _ => None,
        }
    }

    fn command_sent_at_ms(&self) -> Option<u64> {
        match self {
            TaskCompletion::Command { sent_at_ms, .. } => *sent_at_ms,
            _ => None,
        }
    }

    fn tool_invocation(&self) -> Option<ToolInvocationContext> {
        match self {
            TaskCompletion::ToolResult { invocation, .. }
            | TaskCompletion::Child {
                invocation: Some(invocation),
                ..
            } => Some(*invocation),
            _ => None,
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct TaskDeadline {
    pub at: Instant,
    pub label_ms: u64,
    pub(crate) paused_at: Option<Instant>,
}

pub(crate) struct LuaTask {
    id: u64,
    thread: mlua::Thread,
    wait: TaskWait,
    completion: TaskCompletion,
    scope: TaskScope,
    cancel: CancellationToken,
    deadline: Option<TaskDeadline>,
}

#[derive(Debug)]
pub enum TaskDriveOutput {
    ToolEvaluated {
        request: Box<ToolEvaluationRequest>,
        result: Result<mlua::Table, String>,
    },
    ToolComplete {
        invocation: ToolInvocationContext,
        call_id: String,
        content: String,
        is_error: bool,
        metadata: Option<serde_json::Value>,
        display_content: Vec<protocol::ToolDisplayContent>,
        attachment: Option<Box<protocol::ToolAttachment>>,
    },
    NotifyError(String),
}

pub enum TaskEvent {
    /// In-thread resume via `smelt.task.resume(id, value)`.
    ExternalResolved {
        external_id: u64,
        value: mlua::RegistryKey,
    },
    /// Cross-thread resume from a tokio task; JSON is `Send` and converted on the main thread.
    ExternalResolvedJson {
        external_id: u64,
        value: serde_json::Value,
    },
}

/// Second argument of `execute(args, ctx)`.
pub struct ToolEnv<'a> {
    pub mode: protocol::AgentMode,
    pub session_id: &'a str,
    pub artifact_dir: &'a std::path::Path,
}

struct ScheduledTask {
    task: LuaTask,
    ready_key: Option<u64>,
}

/// Single-threaded task runtime; all methods must run on the Lua owner thread.
///
/// **Invariant**: the `LuaShared::tasks` mutex guarding this struct must never
/// be held across a call into Lua code. Lua callbacks can re-enter the runtime
/// (e.g. `smelt.spawn` from inside a coroutine, `Reg:remove()` cancelling a
/// sibling task), and `std::sync::Mutex` is non-reentrant - holding the lock
/// across a resume would deadlock. Task drivers enforce this by popping a
/// task out via `take_next_ready`, dropping the lock for the duration of
/// `step_task_owned`, then reacquiring to `put_back` the parked task.
pub struct LuaTaskRuntime {
    tasks: std::collections::BTreeMap<u64, ScheduledTask>,
    ready: std::collections::BTreeMap<u64, u64>,
    wakeups: std::collections::BTreeSet<(Instant, u64)>,
    external: std::collections::BTreeSet<(u64, u64)>,
    next_ready_key: u64,
    next_task_id: AtomicU64,
}

impl LuaTaskRuntime {
    pub fn new() -> Self {
        Self {
            tasks: Default::default(),
            ready: Default::default(),
            wakeups: Default::default(),
            external: Default::default(),
            next_ready_key: 0,
            next_task_id: AtomicU64::new(1),
        }
    }

    pub fn spawn(
        &mut self,
        lua: &Lua,
        func: mlua::Function,
        initial_args: LuaMultiValue,
        completion: TaskCompletion,
    ) -> LuaResult<u64> {
        self.spawn_scoped(
            lua,
            func,
            initial_args,
            completion,
            current_task_scope().unwrap_or(TaskScope::App),
            None,
        )
    }

    pub(crate) fn spawn_scoped(
        &mut self,
        lua: &Lua,
        func: mlua::Function,
        initial_args: LuaMultiValue,
        completion: TaskCompletion,
        scope: TaskScope,
        deadline: Option<TaskDeadline>,
    ) -> LuaResult<u64> {
        let thread = lua.create_thread(func)?;
        let id = self.next_task_id.fetch_add(1, Ordering::Relaxed);
        self.put_back(LuaTask {
            id,
            thread,
            wait: TaskWait::Ready(initial_args),
            completion,
            scope,
            cancel: CancellationToken::new(),
            deadline,
        });
        Ok(id)
    }

    /// Resolve a `TaskWait::External(id)` wait; returns `true` if found.
    pub fn resolve_external(&mut self, external_id: u64, value: LuaValue) -> bool {
        let Some((_, id)) = self
            .external
            .range((external_id, 0)..=(external_id, u64::MAX))
            .next()
            .copied()
        else {
            return false;
        };
        let mut task = self.take_task(id).expect("indexed external task");
        let mut args = LuaMultiValue::new();
        args.push_back(value);
        task.wait = TaskWait::Ready(args);
        self.put_back(task);
        true
    }

    /// Cancel a single task by id. Returns `true` if the task was found.
    /// Sleeping/external tasks are unparked with the cancelled marker; the
    /// next `drive()` step delivers the marker to the coroutine so user
    /// code in `smelt.sleep` / `task.wait` raises `cancelled` and unwinds.
    pub fn cancel_task(&mut self, lua: &Lua, id: u64) -> bool {
        let Some(mut task) = self.take_task(id) else {
            return false;
        };
        task.cancel.cancel();
        if matches!(&task.wait, TaskWait::Sleep(_) | TaskWait::External { .. }) {
            let mut args = LuaMultiValue::new();
            args.push_back(cancelled_marker(lua));
            task.wait = TaskWait::Ready(args);
        }
        self.put_back(task);
        true
    }

    pub fn cancel_invocation(&mut self, lua: &Lua, invocation_id: protocol::InvocationId) {
        self.cancel_matching(lua, |task| {
            task.completion.invocation_id() == Some(invocation_id)
        });
    }

    pub fn cancel_scope(&mut self, lua: &Lua, scope: TaskScope) {
        self.cancel_matching(lua, |task| task.scope == scope);
    }

    pub fn cancel_all(&mut self, lua: &Lua) {
        self.cancel_matching(lua, |_| true);
    }

    fn cancel_matching(&mut self, lua: &Lua, mut matches: impl FnMut(&LuaTask) -> bool) {
        let ids: Vec<_> = self
            .tasks
            .values()
            .filter(|entry| matches(&entry.task))
            .map(|entry| entry.task.id)
            .collect();
        for id in ids {
            self.cancel_task(lua, id);
        }
    }

    /// Cancel every task and drop the entries immediately. Used by
    /// `/reload`: the surviving Lua handles are about to be wiped, so
    /// letting tasks resume on the next `drive()` would invoke stale
    /// closures.
    pub fn cancel_and_clear(&mut self) {
        for entry in self.tasks.values() {
            entry.task.cancel.cancel();
        }
        self.tasks.clear();
        self.ready.clear();
        self.wakeups.clear();
        self.external.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }

    pub fn len(&self) -> usize {
        self.tasks.len()
    }

    pub(crate) fn next_wakeup(&self, now: Instant) -> Option<Instant> {
        if self.ready.is_empty() {
            self.wakeups.first().map(|(at, _)| (*at).max(now))
        } else {
            Some(now)
        }
    }

    /// Pop a ready task out of the runtime, returning it by value. `drive_tasks`
    /// uses this to step a task without holding the `tasks` mutex - so Lua code
    /// that re-enters the runtime (e.g. `smelt.spawn` from inside a coroutine)
    /// can acquire the lock synchronously instead of deadlocking.
    pub(crate) fn take_next_ready(&mut self, now: Instant) -> Option<LuaTask> {
        // Promote at most one timer per work unit. A timer burst must not bypass
        // the pump budget or starve tasks already waiting in the ready FIFO.
        if let Some((at, id)) = self.wakeups.first().copied().filter(|(at, _)| *at <= now) {
            self.wakeups.remove(&(at, id));
            let key = self.next_ready_key;
            let entry = self.tasks.get_mut(&id).expect("indexed timer task");
            if entry.ready_key.is_none() {
                entry.ready_key = Some(key);
                self.ready.insert(key, id);
                self.next_ready_key = key.checked_add(1).expect("ready queue sequence overflow");
            }
        }
        let (_, id) = self.ready.first_key_value()?;
        let mut task = self.take_task(*id).expect("indexed ready task");
        task.resume_deadline_if_paused(now);
        Some(task)
    }

    pub(crate) fn take_task(&mut self, id: u64) -> Option<LuaTask> {
        let entry = self.tasks.remove(&id)?;
        if let Some(key) = entry.ready_key {
            self.ready.remove(&key);
        }
        if let Some(at) = entry.task.next_timer() {
            self.wakeups.remove(&(at, id));
        }
        if let TaskWait::External { id: external_id } = entry.task.wait {
            self.external.remove(&(external_id, id));
        }
        Some(entry.task)
    }

    pub(crate) fn put_back(&mut self, task: LuaTask) {
        let id = task.id;
        let ready_key = if matches!(task.wait, TaskWait::Ready(_)) {
            let key = self.next_ready_key;
            self.next_ready_key = key.checked_add(1).expect("ready queue sequence overflow");
            self.ready.insert(key, id);
            Some(key)
        } else {
            None
        };
        if let Some(at) = task.next_timer() {
            self.wakeups.insert((at, id));
        }
        if let TaskWait::External { id: external_id } = task.wait {
            self.external.insert((external_id, id));
        }
        assert!(
            self.tasks
                .insert(id, ScheduledTask { task, ready_key })
                .is_none(),
            "task scheduled twice"
        );
    }

    /// Drive a bounded batch of ready tasks. Used by tests; production callers use
    /// `take_next_ready` + `step_task_owned` + `put_back` so the lock can be
    /// dropped during the resume.
    pub fn drive(&mut self, lua: &Lua, now: Instant) -> Vec<TaskDriveOutput> {
        let mut outputs = Vec::new();
        for _ in 0..super::MAX_LUA_WORK_PER_PUMP {
            let Some(task) = self.take_next_ready(now) else {
                break;
            };
            if let Some(parked) = step_task_owned(lua, task, now, &mut outputs) {
                self.put_back(parked);
            }
        }
        outputs
    }
}

impl LuaTask {
    fn timed_out(&self, now: Instant) -> bool {
        self.deadline
            .is_some_and(|deadline| deadline.paused_at.is_none() && deadline.at <= now)
    }

    fn next_timer(&self) -> Option<Instant> {
        let wait_at = match &self.wait {
            TaskWait::Sleep(deadline) => Some(*deadline),
            TaskWait::Ready(_) | TaskWait::External { .. } => None,
        };
        match (wait_at, self.deadline) {
            (Some(a), Some(deadline)) if deadline.paused_at.is_none() => Some(a.min(deadline.at)),
            (Some(a), _) => Some(a),
            (None, Some(deadline)) if deadline.paused_at.is_none() => Some(deadline.at),
            (None, _) => None,
        }
    }

    fn pause_deadline(&mut self, now: Instant) {
        if let Some(deadline) = &mut self.deadline {
            if deadline.paused_at.is_none() {
                deadline.paused_at = Some(now);
            }
        }
    }

    fn resume_deadline_if_paused(&mut self, now: Instant) {
        if let Some(deadline) = &mut self.deadline {
            if let Some(paused_at) = deadline.paused_at.take() {
                deadline.at += now.saturating_duration_since(paused_at);
            }
        }
    }

    fn timeout_message(&self) -> String {
        match self.deadline {
            Some(deadline) => format!("timed out after {:.1}s", deadline.label_ms as f64 / 1000.0),
            None => "timed out".to_string(),
        }
    }
}

/// Resume one task without holding the runtime. Returns the task if it parked
/// again (sleep/external), `None` if it finished or errored.
pub(crate) fn step_task_owned(
    lua: &Lua,
    mut task: LuaTask,
    now: Instant,
    outputs: &mut Vec<TaskDriveOutput>,
) -> Option<LuaTask> {
    if task.timed_out(now) {
        task.cancel.cancel();
        emit_task_timeout(&task, outputs);
        return None;
    }

    let resume_args = match std::mem::replace(&mut task.wait, TaskWait::Ready(LuaMultiValue::new()))
    {
        TaskWait::Ready(mv) => mv,
        TaskWait::Sleep(_) => LuaMultiValue::new(),
        TaskWait::External { .. } => LuaMultiValue::new(),
    };
    let cancel = task.cancel.clone();
    let queue_target = task.completion.command_queue_target();
    let sent_at_ms = task.completion.command_sent_at_ms();
    let tool_invocation = task.completion.tool_invocation();
    let scope = task.scope;
    let previous_owner =
        CURRENT_INVOCATION_OWNER.with(|owner| owner.replace(task.completion.invocation_id()));
    let result: LuaResult<LuaValue> = with_task_context(
        cancel,
        queue_target,
        sent_at_ms,
        tool_invocation,
        scope,
        || task.thread.resume(resume_args),
    );

    CURRENT_INVOCATION_OWNER.with(|owner| owner.replace(previous_owner));
    match result {
        Ok(v) => {
            if task.thread.is_finished() {
                match &task.completion {
                    TaskCompletion::FireAndForget
                    | TaskCompletion::Child { .. }
                    | TaskCompletion::Command { .. } => {}
                    TaskCompletion::ToolEvaluation(request) => {
                        let result = match v {
                            LuaValue::Table(table) => Ok(table),
                            _ => Err("tool evaluation must return a table".into()),
                        };
                        outputs.push(TaskDriveOutput::ToolEvaluated {
                            request: request.clone(),
                            result,
                        });
                    }
                    TaskCompletion::ToolResult {
                        invocation,
                        call_id,
                    } => match coerce_tool_result(lua, &v) {
                        Ok(result) => outputs.push(TaskDriveOutput::ToolComplete {
                            invocation: *invocation,
                            call_id: call_id.clone(),
                            content: result.content,
                            is_error: result.is_error,
                            metadata: result.metadata,
                            display_content: result.display_content,
                            attachment: result.attachment.map(Box::new),
                        }),
                        Err(error) => emit_task_failure(
                            &task,
                            &format!("invalid tool result: {error}"),
                            outputs,
                        ),
                    },
                }
                return None;
            }
            match decode_yield(lua, v) {
                Ok(Yield::Sleep(d)) => {
                    if task.cancel.is_cancelled() {
                        let mut mv = LuaMultiValue::new();
                        mv.push_back(cancelled_marker(lua));
                        task.wait = TaskWait::Ready(mv);
                    } else {
                        task.wait = TaskWait::Sleep(now + d);
                    }
                    Some(task)
                }
                Ok(Yield::External {
                    id,
                    pauses_deadline,
                }) => {
                    if task.cancel.is_cancelled() {
                        let mut mv = LuaMultiValue::new();
                        mv.push_back(cancelled_marker(lua));
                        task.wait = TaskWait::Ready(mv);
                    } else {
                        if pauses_deadline {
                            task.pause_deadline(now);
                        }
                        task.wait = TaskWait::External { id };
                    }
                    Some(task)
                }
                Err(msg) => {
                    emit_task_failure(&task, &msg, outputs);
                    None
                }
            }
        }
        Err(e) => {
            if task.cancel.is_cancelled() && is_cancelled_lua_error(&e) {
                return None;
            }
            let msg = e.to_string();
            emit_task_failure(&task, &msg, outputs);
            None
        }
    }
}

// Thread-local context for the executing coroutine; read by async Lua bindings.
thread_local! {
    static CURRENT_INVOCATION_OWNER: RefCell<Option<protocol::InvocationId>> = const { RefCell::new(None) };
    static CURRENT_TASK_CANCEL: RefCell<Option<CancellationToken>> = const { RefCell::new(None) };
    static CURRENT_COMMAND_QUEUE_TARGET: RefCell<Option<CommandQueueTarget>> = const { RefCell::new(None) };
    static CURRENT_COMMAND_SENT_AT_MS: RefCell<Option<u64>> = const { RefCell::new(None) };
    static CURRENT_TOOL_INVOCATION: RefCell<Option<ToolInvocationContext>> = const { RefCell::new(None) };
    static CURRENT_TASK_SCOPE: RefCell<Option<TaskScope>> = const { RefCell::new(None) };
}

/// Install task context for the closure's duration.
fn with_task_context<R>(
    cancel: CancellationToken,
    queue_target: Option<CommandQueueTarget>,
    sent_at_ms: Option<u64>,
    tool_invocation: Option<ToolInvocationContext>,
    scope: TaskScope,
    f: impl FnOnce() -> R,
) -> R {
    let previous_cancel = CURRENT_TASK_CANCEL.with(|c| c.replace(Some(cancel)));
    let previous_target = CURRENT_COMMAND_QUEUE_TARGET.with(|c| c.replace(queue_target));
    let previous_sent_at_ms = CURRENT_COMMAND_SENT_AT_MS.with(|c| c.replace(sent_at_ms));
    let previous_tool_invocation = CURRENT_TOOL_INVOCATION.with(|c| c.replace(tool_invocation));
    let previous_scope = CURRENT_TASK_SCOPE.with(|c| c.replace(Some(scope)));
    let r = f();
    CURRENT_TASK_SCOPE.with(|c| c.replace(previous_scope));
    CURRENT_TOOL_INVOCATION.with(|c| c.replace(previous_tool_invocation));
    CURRENT_COMMAND_SENT_AT_MS.with(|c| c.replace(previous_sent_at_ms));
    CURRENT_COMMAND_QUEUE_TARGET.with(|c| c.replace(previous_target));
    CURRENT_TASK_CANCEL.with(|c| c.replace(previous_cancel));
    r
}

/// Install the task's cancellation token for the closure's duration.
pub fn with_task_cancel<R>(cancel: CancellationToken, f: impl FnOnce() -> R) -> R {
    with_task_context(cancel, None, None, None, TaskScope::App, f)
}

/// Current task's cancellation token; `None` when called outside `step_task`.
pub fn current_task_cancel() -> Option<CancellationToken> {
    CURRENT_TASK_CANCEL.with(|c| c.borrow().clone())
}

/// Current slash-command queue target; `None` outside slash-command tasks.
pub fn current_command_queue_target() -> Option<CommandQueueTarget> {
    CURRENT_COMMAND_QUEUE_TARGET.with(|c| *c.borrow())
}

/// Submission time of the current slash command, preserved across coroutine yields.
pub fn current_command_sent_at_ms() -> Option<u64> {
    CURRENT_COMMAND_SENT_AT_MS.with(|c| *c.borrow())
}

pub(crate) fn with_tool_invocation_context<R>(
    invocation: ToolInvocationContext,
    f: impl FnOnce() -> R,
) -> R {
    let previous = CURRENT_TOOL_INVOCATION.with(|current| current.replace(Some(invocation)));
    let result = f();
    CURRENT_TOOL_INVOCATION.with(|current| current.replace(previous));
    result
}

pub(crate) fn current_invocation_owner() -> Option<protocol::InvocationId> {
    CURRENT_INVOCATION_OWNER
        .with(|owner| *owner.borrow())
        .or_else(|| current_tool_invocation().map(|invocation| invocation.invocation_id))
}

/// Current model tool invocation; `None` outside a tool callback or middleware.
pub fn current_tool_invocation() -> Option<ToolInvocationContext> {
    CURRENT_TOOL_INVOCATION.with(|current| *current.borrow())
}

pub fn with_task_scope<R>(scope: TaskScope, f: impl FnOnce() -> R) -> R {
    let previous = CURRENT_TASK_SCOPE.with(|current| current.replace(Some(scope)));
    let result = f();
    CURRENT_TASK_SCOPE.with(|current| current.replace(previous));
    result
}

/// Current task scope; `None` outside the Lua task runtime.
pub fn current_task_scope() -> Option<TaskScope> {
    CURRENT_TASK_SCOPE.with(|c| *c.borrow())
}

fn cancelled_marker(lua: &Lua) -> LuaValue {
    lua.create_table()
        .and_then(|t| {
            t.set("__cancelled", true)?;
            Ok(LuaValue::Table(t))
        })
        .unwrap_or(LuaValue::Nil)
}

fn emit_task_timeout(task: &LuaTask, outputs: &mut Vec<TaskDriveOutput>) {
    let message = task.timeout_message();
    if matches!(&task.completion, TaskCompletion::ToolResult { .. }) {
        fail_completion(&task.completion, &message, outputs);
    } else {
        emit_task_failure(task, &message, outputs);
    }
}

fn emit_task_failure(task: &LuaTask, msg: &str, outputs: &mut Vec<TaskDriveOutput>) {
    outputs.push(TaskDriveOutput::NotifyError(format!(
        "{}: {msg}",
        task.completion.notification_label(task.id)
    )));
    fail_completion(&task.completion, msg, outputs);
}

fn is_cancelled_lua_error(err: &mlua::Error) -> bool {
    match err {
        // mlua appends a Lua traceback after the error string, so the
        // RuntimeError payload is "cancelled\nstack traceback:\n...".
        mlua::Error::RuntimeError(msg) => msg.lines().next() == Some("cancelled"),
        mlua::Error::CallbackError { cause, .. } => is_cancelled_lua_error(cause.as_ref()),
        _ => false,
    }
}

fn fail_completion(completion: &TaskCompletion, msg: &str, outputs: &mut Vec<TaskDriveOutput>) {
    if let TaskCompletion::ToolEvaluation(request) = completion {
        outputs.push(TaskDriveOutput::ToolEvaluated {
            request: request.clone(),
            result: Err(msg.into()),
        });
    }
    if let TaskCompletion::ToolResult {
        invocation,
        call_id,
    } = completion
    {
        outputs.push(TaskDriveOutput::ToolComplete {
            invocation: *invocation,
            call_id: call_id.clone(),
            content: format!("tool error: {msg}"),
            is_error: true,
            metadata: None,
            display_content: Vec::new(),
            attachment: None,
        });
    }
}

impl Default for LuaTaskRuntime {
    fn default() -> Self {
        Self::new()
    }
}

enum Yield {
    Sleep(Duration),
    External { id: u64, pauses_deadline: bool },
}

fn decode_yield(_lua: &Lua, v: LuaValue) -> Result<Yield, String> {
    let table = match v {
        LuaValue::Table(t) => t,
        other => {
            return Err(format!("expected yield table, got {}", other.type_name()));
        }
    };
    let kind: String = table
        .get("__yield")
        .map_err(|e| format!("yield missing __yield discriminator: {e}"))?;
    match kind.as_str() {
        "sleep" => {
            let ms: u64 = table.get("ms").map_err(|e| format!("sleep: {e}"))?;
            Ok(Yield::Sleep(Duration::from_millis(ms)))
        }
        "external" => {
            let id: u64 = table.get("id").map_err(|e| format!("external: {e}"))?;
            let pauses_deadline = table
                .get::<Option<bool>>("pauses_deadline")
                .ok()
                .flatten()
                .or_else(|| table.get::<Option<bool>>("interactive").ok().flatten())
                .unwrap_or(false);
            Ok(Yield::External {
                id,
                pauses_deadline,
            })
        }
        other => Err(format!("unknown yield kind: {other}")),
    }
}

/// Coerce a task return value to content, status, bounded JSON metadata, and retained display
/// content. Large presentation payloads and attachments are extracted before metadata limits.
fn coerce_tool_result(lua: &Lua, value: &LuaValue) -> LuaResult<crate::lua::LuaToolResultParts> {
    match value {
        LuaValue::String(content) => Ok(crate::lua::LuaToolResultParts {
            content: content.to_string_lossy(),
            is_error: false,
            metadata: None,
            display_content: Vec::new(),
            attachment: None,
        }),
        LuaValue::Table(result) => crate::lua::tool_result_from_lua_table(lua, result),
        LuaValue::Nil => Ok(crate::lua::LuaToolResultParts {
            content: String::new(),
            is_error: false,
            metadata: None,
            display_content: Vec::new(),
            attachment: None,
        }),
        other => Ok(crate::lua::LuaToolResultParts {
            content: format!("tool returned non-string value: {}", other.type_name()),
            is_error: true,
            metadata: None,
            display_content: Vec::new(),
            attachment: None,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool_invocation(request_id: u64) -> ToolInvocationContext {
        ToolInvocationContext {
            invocation_id: protocol::InvocationId::new(request_id),
            request_id,
            execution_mode: protocol::ToolExecutionMode::Concurrent,
        }
    }

    fn lua_with_sleep() -> Lua {
        let lua = Lua::new();
        lua.load(
            r#"
            smelt = {}
            function smelt.sleep(ms)
              if not coroutine.isyieldable() then
                error("smelt.sleep: not inside a task", 2)
              end
              return coroutine.yield({__yield = "sleep", ms = ms})
            end
            "#,
        )
        .exec()
        .unwrap();
        lua
    }

    #[test]
    fn parked_tasks_have_no_ready_scan_and_external_resolution_uses_its_index() {
        let lua = lua_with_sleep();
        let mut rt = LuaTaskRuntime::new();
        let func: mlua::Function = lua
            .load("function(id) coroutine.yield({__yield = 'external', id = id}) end")
            .eval()
            .unwrap();
        let now = Instant::now();
        let mut last = 0;
        for id in 1..=8_192 {
            last = rt
                .spawn(
                    &lua,
                    func.clone(),
                    LuaMultiValue::from_vec(vec![LuaValue::Integer(id)]),
                    TaskCompletion::FireAndForget,
                )
                .unwrap();
        }
        while !rt.ready.is_empty() {
            assert!(rt.drive(&lua, now).is_empty());
        }
        assert_eq!(rt.external.len(), 8_192);
        assert!(rt.wakeups.is_empty());
        assert_eq!(rt.next_wakeup(now), None);
        assert!(rt.resolve_external(8_192, LuaValue::Nil));
        assert_eq!(rt.ready.len(), 1);
        let task = rt.take_next_ready(now).unwrap();
        assert_eq!(task.id, last);
        assert_eq!(rt.external.len(), 8_191);
        assert_eq!(rt.next_wakeup(now), None);
        assert!(step_task_owned(&lua, task, now, &mut Vec::new()).is_none());
        rt.cancel_and_clear();
        assert!(rt.ready.is_empty() && rt.external.is_empty() && rt.wakeups.is_empty());
    }

    #[test]
    fn timer_bursts_promote_one_task_per_step_and_preserve_fifo_fairness() {
        let lua = lua_with_sleep();
        let mut rt = LuaTaskRuntime::new();
        let func: mlua::Function = lua.load("function() smelt.sleep(10) end").eval().unwrap();
        let now = Instant::now();
        for _ in 0..512 {
            rt.spawn(
                &lua,
                func.clone(),
                LuaMultiValue::new(),
                TaskCompletion::FireAndForget,
            )
            .unwrap();
        }
        while !rt.ready.is_empty() {
            rt.drive(&lua, now);
        }
        assert_eq!(rt.wakeups.len(), 512);
        let ready = rt
            .spawn(
                &lua,
                lua.load("function() end").eval().unwrap(),
                LuaMultiValue::new(),
                TaskCompletion::FireAndForget,
            )
            .unwrap();
        let later = now + Duration::from_millis(20);
        let task = rt.take_next_ready(later).unwrap();
        assert_eq!(task.id, ready);
        assert_eq!(rt.wakeups.len(), 511);
        assert_eq!(rt.ready.len(), 1);
        rt.drive(&lua, later);
        assert_eq!(rt.len(), 256);
        assert_eq!(rt.wakeups.len(), 255);
        assert_eq!(rt.ready.len(), 1);
        rt.drive(&lua, later);
        assert!(rt.is_empty());
        assert!(rt.wakeups.is_empty() && rt.ready.is_empty() && rt.external.is_empty());
    }

    #[test]
    fn fire_and_forget_returns_immediately() {
        let lua = lua_with_sleep();
        let mut rt = LuaTaskRuntime::new();
        let func: mlua::Function = lua.load("function() end").eval().unwrap();
        rt.spawn(
            &lua,
            func,
            LuaMultiValue::new(),
            TaskCompletion::FireAndForget,
        )
        .unwrap();
        let out = rt.drive(&lua, Instant::now());
        assert!(out.is_empty());
        assert_eq!(rt.tasks.len(), 0);
    }

    #[test]
    fn noninteractive_external_wait_counts_against_deadline() {
        let lua = lua_with_sleep();
        let mut rt = LuaTaskRuntime::new();
        let func: mlua::Function = lua
            .load(
                r#"function()
                coroutine.yield({__yield = "external", id = 7})
                return "done"
              end"#,
            )
            .eval()
            .unwrap();
        let t0 = Instant::now();
        rt.spawn_scoped(
            &lua,
            func,
            LuaMultiValue::new(),
            TaskCompletion::FireAndForget,
            TaskScope::Turn,
            Some(TaskDeadline {
                at: t0 + Duration::from_millis(100),
                label_ms: 100,
                paused_at: None,
            }),
        )
        .unwrap();

        assert!(rt.drive(&lua, t0).is_empty());
        assert_eq!(rt.tasks.len(), 1);
        assert!(matches!(
            rt.tasks.first_key_value().unwrap().1.task.wait,
            TaskWait::External { id: 7, .. }
        ));

        let out = rt.drive(&lua, t0 + Duration::from_millis(101));
        assert!(
            matches!(out.as_slice(), [TaskDriveOutput::NotifyError(msg)] if msg.contains("timed out"))
        );
        assert!(rt.tasks.is_empty());
    }

    #[test]
    fn tool_timeout_reports_only_failing_completion() {
        let lua = lua_with_sleep();
        let mut rt = LuaTaskRuntime::new();
        let func: mlua::Function = lua.load("function() smelt.sleep(1000) end").eval().unwrap();
        let t0 = Instant::now();
        rt.spawn_scoped(
            &lua,
            func,
            LuaMultiValue::new(),
            TaskCompletion::ToolResult {
                invocation: tool_invocation(7),
                call_id: "slow-tool".into(),
            },
            TaskScope::Turn,
            Some(TaskDeadline {
                at: t0 + Duration::from_millis(100),
                label_ms: 100,
                paused_at: None,
            }),
        )
        .unwrap();

        assert!(rt.drive(&lua, t0).is_empty());
        let out = rt.drive(&lua, t0 + Duration::from_millis(101));

        assert!(matches!(
            out.as_slice(),
            [TaskDriveOutput::ToolComplete {
                call_id,
                content,
                is_error: true,
                ..
            }] if call_id == "slow-tool" && content == "tool error: timed out after 0.1s"
        ));
        assert!(rt.tasks.is_empty());
    }

    #[test]
    fn interactive_external_wait_pauses_deadline() {
        let lua = lua_with_sleep();
        let mut rt = LuaTaskRuntime::new();
        let func: mlua::Function = lua
            .load(
                r#"function()
                coroutine.yield({__yield = "external", id = 7, interactive = true})
                return "done"
              end"#,
            )
            .eval()
            .unwrap();
        let t0 = Instant::now();
        rt.spawn_scoped(
            &lua,
            func,
            LuaMultiValue::new(),
            TaskCompletion::FireAndForget,
            TaskScope::Turn,
            Some(TaskDeadline {
                at: t0 + Duration::from_millis(100),
                label_ms: 100,
                paused_at: None,
            }),
        )
        .unwrap();

        assert!(rt.drive(&lua, t0).is_empty());
        assert_eq!(rt.tasks.len(), 1);
        assert!(rt
            .tasks
            .first_key_value()
            .unwrap()
            .1
            .task
            .deadline
            .is_some_and(|deadline| deadline.paused_at == Some(t0)));

        assert!(rt.drive(&lua, t0 + Duration::from_millis(1_000)).is_empty());
        assert_eq!(rt.tasks.len(), 1);

        assert!(rt.resolve_external(7, LuaValue::Nil));
        let out = rt.drive(&lua, t0 + Duration::from_millis(1_000));
        assert!(out.is_empty());
        assert!(rt.tasks.is_empty());
    }

    #[test]
    fn sleep_yields_and_parks() {
        let lua = lua_with_sleep();
        let mut rt = LuaTaskRuntime::new();
        let func: mlua::Function = lua
            .load(
                r#"function()
                smelt.sleep(100)
                return "done"
              end"#,
            )
            .eval()
            .unwrap();
        rt.spawn(
            &lua,
            func,
            LuaMultiValue::new(),
            TaskCompletion::FireAndForget,
        )
        .unwrap();

        // First drive: task runs, yields sleep, parks.
        let t0 = Instant::now();
        let out = rt.drive(&lua, t0);
        assert!(out.is_empty());
        assert_eq!(rt.tasks.len(), 1);
        assert!(matches!(
            rt.tasks.first_key_value().unwrap().1.task.wait,
            TaskWait::Sleep(_)
        ));

        // Second drive before deadline - still parked.
        let out = rt.drive(&lua, t0 + Duration::from_millis(50));
        assert!(out.is_empty());
        assert_eq!(rt.tasks.len(), 1);

        // Third drive past deadline - resumes and completes.
        let out = rt.drive(&lua, t0 + Duration::from_millis(200));
        assert!(out.is_empty());
        assert_eq!(rt.tasks.len(), 0);
    }

    #[test]
    fn tool_result_string_return() {
        let lua = lua_with_sleep();
        let mut rt = LuaTaskRuntime::new();
        let func: mlua::Function = lua.load(r#"function() return "hello" end"#).eval().unwrap();
        rt.spawn(
            &lua,
            func,
            LuaMultiValue::new(),
            TaskCompletion::ToolResult {
                invocation: tool_invocation(7),
                call_id: "c1".into(),
            },
        )
        .unwrap();
        let out = rt.drive(&lua, Instant::now());
        assert_eq!(out.len(), 1);
        match &out[0] {
            TaskDriveOutput::ToolComplete {
                invocation,
                call_id,
                content,
                is_error,
                metadata,
                display_content,
                attachment,
            } => {
                assert_eq!(invocation.request_id, 7);
                assert_eq!(call_id, "c1");
                assert_eq!(content, "hello");
                assert!(!*is_error);
                assert!(metadata.is_none());
                assert!(display_content.is_empty());
                assert!(attachment.is_none());
            }
            _ => panic!("expected ToolComplete"),
        }
    }

    #[test]
    fn tool_result_extracts_retained_display_content_outside_metadata() {
        let lua = lua_with_sleep();
        let mut rt = LuaTaskRuntime::new();
        let func: mlua::Function = lua
            .load(
                r#"function()
                    return {
                      content = "edited",
                      metadata = { path = "src/lib.rs" },
                      display_content = {
                        old_content = "before",
                        new_content = "after",
                      },
                    }
                end"#,
            )
            .eval()
            .unwrap();
        rt.spawn(
            &lua,
            func,
            LuaMultiValue::new(),
            TaskCompletion::ToolResult {
                invocation: tool_invocation(8),
                call_id: "edit-1".into(),
            },
        )
        .unwrap();

        let out = rt.drive(&lua, Instant::now());
        let TaskDriveOutput::ToolComplete {
            metadata,
            display_content,
            ..
        } = &out[0]
        else {
            panic!("expected tool completion");
        };
        assert_eq!(metadata, &Some(serde_json::json!({ "path": "src/lib.rs" })));
        assert_eq!(display_content.len(), 2);
        assert!(display_content
            .iter()
            .any(|field| field.name == "old_content" && field.content.as_str() == "before"));
        assert!(display_content
            .iter()
            .any(|field| field.name == "new_content" && field.content.as_str() == "after"));
        assert!(metadata
            .as_ref()
            .is_none_or(|metadata| metadata.get("old_content").is_none()));
    }

    #[test]
    fn cyclic_tool_metadata_fails_without_recursing() {
        let lua = lua_with_sleep();
        let mut runtime = LuaTaskRuntime::new();
        let function: mlua::Function = lua
            .load(
                r#"function()
                    local metadata = {}
                    metadata.self = metadata
                    return { content = "bad", metadata = metadata }
                end"#,
            )
            .eval()
            .unwrap();
        runtime
            .spawn(
                &lua,
                function,
                LuaMultiValue::new(),
                TaskCompletion::ToolResult {
                    invocation: tool_invocation(9),
                    call_id: "cycle".into(),
                },
            )
            .unwrap();

        let output = runtime.drive(&lua, Instant::now());
        assert!(output.iter().any(
            |output| matches!(output, TaskDriveOutput::NotifyError(message) if message.contains("table cycle"))
        ));
        assert!(output.iter().any(|output| matches!(
            output,
            TaskDriveOutput::ToolComplete { content, is_error: true, metadata: None, .. }
                if content.contains("table cycle")
        )));
    }

    #[test]
    fn oversized_generic_tool_metadata_is_rejected_atomically() {
        let lua = lua_with_sleep();
        let mut runtime = LuaTaskRuntime::new();
        let function: mlua::Function = lua
            .load(format!(
                "function() return {{ content = 'bad', metadata = {{ payload = string.rep('x', {}) }} }} end",
                protocol::TOOL_METADATA_MAX_BYTES + 1
            ))
            .eval()
            .unwrap();
        runtime
            .spawn(
                &lua,
                function,
                LuaMultiValue::new(),
                TaskCompletion::ToolResult {
                    invocation: tool_invocation(10),
                    call_id: "oversized".into(),
                },
            )
            .unwrap();

        let output = runtime.drive(&lua, Instant::now());
        let completion = output
            .iter()
            .find_map(|output| match output {
                TaskDriveOutput::ToolComplete {
                    content,
                    is_error,
                    metadata,
                    display_content,
                    ..
                } => Some((content, is_error, metadata, display_content)),
                TaskDriveOutput::NotifyError(_) | TaskDriveOutput::ToolEvaluated { .. } => None,
            })
            .unwrap();
        assert!(*completion.1);
        assert!(completion.0.contains("tool metadata exceeds"));
        assert!(completion.2.is_none());
        assert!(completion.3.is_empty());
    }

    #[test]
    fn historical_large_edit_metadata_is_promoted_before_bounding() {
        let lua = lua_with_sleep();
        let mut runtime = LuaTaskRuntime::new();
        let function: mlua::Function = lua
            .load(format!(
                "function() return {{ content = 'edited', metadata = {{ path = 'a.rs', old_content = string.rep('x', {}), new_content = 'after' }} }} end",
                protocol::TOOL_METADATA_MAX_BYTES * 2
            ))
            .eval()
            .unwrap();
        runtime
            .spawn(
                &lua,
                function,
                LuaMultiValue::new(),
                TaskCompletion::ToolResult {
                    invocation: tool_invocation(11),
                    call_id: "historical-edit".into(),
                },
            )
            .unwrap();

        let output = runtime.drive(&lua, Instant::now());
        let TaskDriveOutput::ToolComplete {
            metadata,
            display_content,
            is_error,
            ..
        } = &output[0]
        else {
            panic!("expected tool completion");
        };
        assert!(!is_error);
        assert_eq!(metadata, &Some(serde_json::json!({ "path": "a.rs" })));
        assert_eq!(
            display_content
                .iter()
                .find(|field| field.name == "old_content")
                .unwrap()
                .content
                .len(),
            protocol::TOOL_METADATA_MAX_BYTES * 2
        );
    }

    #[test]
    fn attachment_payload_bypasses_generic_metadata_budget() {
        let lua = lua_with_sleep();
        let mut runtime = LuaTaskRuntime::new();
        let function: mlua::Function = lua
            .load(format!(
                "function() return {{ content = 'attached', metadata = {{ kind = 'file_attachment', modality = 'image', mime = 'image/png', data_url = 'data:image/png;base64,' .. string.rep('a', {}) }} }} end",
                protocol::TOOL_METADATA_MAX_BYTES * 2
            ))
            .eval()
            .unwrap();
        runtime
            .spawn(
                &lua,
                function,
                LuaMultiValue::new(),
                TaskCompletion::ToolResult {
                    invocation: tool_invocation(12),
                    call_id: "attachment".into(),
                },
            )
            .unwrap();

        let output = runtime.drive(&lua, Instant::now());
        let TaskDriveOutput::ToolComplete {
            metadata,
            attachment,
            is_error,
            ..
        } = &output[0]
        else {
            panic!("expected tool completion");
        };
        assert!(!is_error);
        assert!(metadata.as_ref().unwrap().get("data_url").is_none());
        assert_eq!(
            attachment.as_ref().unwrap().data_url.len(),
            "data:image/png;base64,".len() + protocol::TOOL_METADATA_MAX_BYTES * 2
        );
    }

    #[test]
    fn tool_result_error_table() {
        let lua = lua_with_sleep();
        let mut rt = LuaTaskRuntime::new();
        let func: mlua::Function = lua
            .load(r#"function() return {content = "boom", is_error = true} end"#)
            .eval()
            .unwrap();
        rt.spawn(
            &lua,
            func,
            LuaMultiValue::new(),
            TaskCompletion::ToolResult {
                invocation: tool_invocation(1),
                call_id: "x".into(),
            },
        )
        .unwrap();
        let out = rt.drive(&lua, Instant::now());
        assert!(matches!(
            &out[0],
            TaskDriveOutput::ToolComplete { is_error: true, content, .. } if content == "boom"
        ));
    }

    #[test]
    fn handler_error_reports_notification_and_tool_error() {
        let lua = lua_with_sleep();
        let mut rt = LuaTaskRuntime::new();
        let func: mlua::Function = lua.load(r#"function() error("bang") end"#).eval().unwrap();
        rt.spawn(
            &lua,
            func,
            LuaMultiValue::new(),
            TaskCompletion::ToolResult {
                invocation: tool_invocation(2),
                call_id: "y".into(),
            },
        )
        .unwrap();
        let out = rt.drive(&lua, Instant::now());
        let has_notification = out
            .iter()
            .any(|o| matches!(o, TaskDriveOutput::NotifyError(m) if m.contains("bang")));
        let has_tool_err = out
            .iter()
            .any(|o| matches!(o, TaskDriveOutput::ToolComplete { is_error: true, .. }));
        assert!(has_notification);
        assert!(has_tool_err);
        assert_eq!(rt.tasks.len(), 0);
    }

    #[test]
    fn sleep_outside_task_errors() {
        let lua = lua_with_sleep();
        let res: LuaResult<()> = lua.load("smelt.sleep(10)").exec();
        assert!(res.is_err());
        let msg = format!("{}", res.unwrap_err());
        assert!(msg.contains("not inside a task"));
    }

    #[test]
    fn cancel_all_resolves_sleep_with_cancel_marker() {
        let lua = lua_with_sleep();
        let mut rt = LuaTaskRuntime::new();
        let func: mlua::Function = lua
            .load(
                r#"function()
                local r = smelt.sleep(100)
                return r
              end"#,
            )
            .eval()
            .unwrap();
        rt.spawn(
            &lua,
            func,
            LuaMultiValue::new(),
            TaskCompletion::FireAndForget,
        )
        .unwrap();

        // First drive parks on sleep.
        let out = rt.drive(&lua, Instant::now());
        assert!(out.is_empty());
        assert_eq!(rt.tasks.len(), 1);
        assert!(matches!(
            rt.tasks.first_key_value().unwrap().1.task.wait,
            TaskWait::Sleep(_)
        ));

        // Cancel all tasks.
        rt.cancel_all(&lua);
        assert!(matches!(
            rt.tasks.first_key_value().unwrap().1.task.wait,
            TaskWait::Ready(_)
        ));

        // Next drive resumes with cancel marker and finishes.
        let out = rt.drive(&lua, Instant::now());
        assert!(out.is_empty());
        assert_eq!(rt.tasks.len(), 0);
    }

    #[test]
    fn cancel_all_resolves_external_with_cancel_marker() {
        let lua = lua_with_sleep();
        let mut rt = LuaTaskRuntime::new();
        let func: mlua::Function = lua
            .load(
                r#"function()
                local r = coroutine.yield({__yield = "external", id = 42})
                return r
              end"#,
            )
            .eval()
            .unwrap();
        rt.spawn(
            &lua,
            func,
            LuaMultiValue::new(),
            TaskCompletion::FireAndForget,
        )
        .unwrap();

        // First drive parks on external.
        let out = rt.drive(&lua, Instant::now());
        assert!(out.is_empty());
        assert_eq!(rt.tasks.len(), 1);
        assert!(matches!(
            rt.tasks.first_key_value().unwrap().1.task.wait,
            TaskWait::External { id: 42, .. }
        ));

        // Cancel all tasks.
        rt.cancel_all(&lua);
        assert!(matches!(
            rt.tasks.first_key_value().unwrap().1.task.wait,
            TaskWait::Ready(_)
        ));

        // Next drive resumes with cancel marker and finishes.
        let out = rt.drive(&lua, Instant::now());
        assert!(out.is_empty());
        assert_eq!(rt.tasks.len(), 0);
    }

    #[test]
    fn cancelled_task_that_yields_again_gets_marker() {
        let lua = lua_with_sleep();
        let mut rt = LuaTaskRuntime::new();
        // Task does some sync work, then sleeps.
        let func: mlua::Function = lua
            .load(
                r#"function()
                local x = 1
                local r = smelt.sleep(100)
                return r
              end"#,
            )
            .eval()
            .unwrap();
        rt.spawn(
            &lua,
            func,
            LuaMultiValue::new(),
            TaskCompletion::FireAndForget,
        )
        .unwrap();

        // Cancel before first drive.
        rt.cancel_all(&lua);

        // Drive: task runs, yields sleep (rewritten to Ready(cancel_marker)
        // by step_task_owned), then is picked up again in the same `drive`
        // loop, resumed with the cancel marker, and finishes.
        let out = rt.drive(&lua, Instant::now());
        assert!(out.is_empty());
        assert_eq!(rt.tasks.len(), 0);
    }

    #[test]
    fn cancel_scope_only_cancels_matching_tasks() {
        let lua = lua_with_sleep();
        let mut rt = LuaTaskRuntime::new();
        let app_func: mlua::Function = lua
            .load(r#"function() smelt.sleep(1000) end"#)
            .eval()
            .unwrap();
        let turn_func: mlua::Function = lua
            .load(r#"function() smelt.sleep(1000) end"#)
            .eval()
            .unwrap();
        rt.spawn_scoped(
            &lua,
            app_func,
            LuaMultiValue::new(),
            TaskCompletion::FireAndForget,
            TaskScope::App,
            None,
        )
        .unwrap();
        rt.spawn_scoped(
            &lua,
            turn_func,
            LuaMultiValue::new(),
            TaskCompletion::FireAndForget,
            TaskScope::Turn,
            None,
        )
        .unwrap();

        let now = Instant::now();
        assert!(rt.drive(&lua, now).is_empty());
        assert_eq!(rt.tasks.len(), 2);

        rt.cancel_scope(&lua, TaskScope::Turn);
        assert!(rt.drive(&lua, now).is_empty());
        assert_eq!(rt.tasks.len(), 1);
        assert_eq!(
            rt.tasks.first_key_value().unwrap().1.task.scope,
            TaskScope::App
        );
    }

    #[test]
    fn cancelled_tool_completion_is_not_task_error() {
        let lua = Lua::new();
        lua.load(
            r#"
            smelt = {}
            local function yield_with_cancel(payload)
              local result = coroutine.yield(payload)
              if type(result) == "table" and result.__cancelled then
                error("cancelled", 0)
              end
              return result
            end
            function smelt.sleep(ms)
              return yield_with_cancel({__yield = "sleep", ms = ms})
            end
            "#,
        )
        .exec()
        .unwrap();
        let mut rt = LuaTaskRuntime::new();
        let func: mlua::Function = lua
            .load(r#"function() smelt.sleep(1000); return "done" end"#)
            .eval()
            .unwrap();
        rt.spawn_scoped(
            &lua,
            func,
            LuaMultiValue::new(),
            TaskCompletion::ToolResult {
                invocation: tool_invocation(42),
                call_id: "call-cancel".into(),
            },
            TaskScope::Turn,
            None,
        )
        .unwrap();

        let now = Instant::now();
        assert!(rt.drive(&lua, now).is_empty());
        rt.cancel_scope(&lua, TaskScope::Turn);
        let out = rt.drive(&lua, now);
        assert!(
            out.is_empty(),
            "cancelled tool task should not emit Lua task output: {out:?}"
        );
    }

    #[test]
    fn uncancelled_cancelled_error_is_reported() {
        let lua = lua_with_sleep();
        let mut rt = LuaTaskRuntime::new();
        let func: mlua::Function = lua
            .load(r#"function() error("cancelled") end"#)
            .eval()
            .unwrap();
        rt.spawn(
            &lua,
            func,
            LuaMultiValue::new(),
            TaskCompletion::FireAndForget,
        )
        .unwrap();

        let out = rt.drive(&lua, Instant::now());
        assert!(
            out.iter().any(
                |o| matches!(o, TaskDriveOutput::NotifyError(msg) if msg.contains("cancelled"))
            ),
            "plain user error named cancelled should be reported: {out:?}"
        );
    }
}
