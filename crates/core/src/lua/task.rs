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
    External(u64),
}

pub enum TaskCompletion {
    FireAndForget,
    ToolResult { request_id: u64, call_id: String },
}

struct LuaTask {
    id: u64,
    thread: mlua::Thread,
    wait: TaskWait,
    completion: TaskCompletion,
    cancel: CancellationToken,
}

pub enum TaskDriveOutput {
    ToolComplete {
        request_id: u64,
        call_id: String,
        content: String,
        is_error: bool,
    },
    Error(String),
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
    pub session_dir: &'a std::path::Path,
}

/// Single-threaded task runtime; all methods must run on the Lua owner thread.
pub struct LuaTaskRuntime {
    tasks: Vec<LuaTask>,
    next_task_id: AtomicU64,
}

impl LuaTaskRuntime {
    pub fn new() -> Self {
        Self {
            tasks: Vec::new(),
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
        let thread = lua.create_thread(func)?;
        let id = self.next_task_id.fetch_add(1, Ordering::Relaxed);
        self.tasks.push(LuaTask {
            id,
            thread,
            wait: TaskWait::Ready(initial_args),
            completion,
            cancel: CancellationToken::new(),
        });
        Ok(id)
    }

    /// Resolve a `TaskWait::External(id)` wait; returns `true` if found.
    pub fn resolve_external(&mut self, external_id: u64, value: LuaValue) -> bool {
        for task in &mut self.tasks {
            if matches!(&task.wait, TaskWait::External(id) if *id == external_id) {
                let mut mv = LuaMultiValue::new();
                mv.push_back(value);
                task.wait = TaskWait::Ready(mv);
                return true;
            }
        }
        false
    }

    pub fn cancel_all(&mut self, lua: &Lua) {
        let marker = cancelled_marker(lua);
        for task in &mut self.tasks {
            task.cancel.cancel();
            match &task.wait {
                TaskWait::Sleep(_) | TaskWait::External(_) => {
                    let mut mv = LuaMultiValue::new();
                    mv.push_back(marker.clone());
                    task.wait = TaskWait::Ready(mv);
                }
                TaskWait::Ready(_) => {}
            }
        }
    }

    /// Cancel every task and drop the entries immediately. Used by
    /// `/reload`: the surviving Lua handles are about to be wiped, so
    /// letting tasks resume on the next `drive()` would invoke stale
    /// closures.
    pub fn cancel_and_clear(&mut self) {
        for task in &mut self.tasks {
            task.cancel.cancel();
        }
        self.tasks.clear();
    }

    pub fn drive(&mut self, lua: &Lua, now: Instant) -> Vec<TaskDriveOutput> {
        let mut outputs = Vec::new();
        let mut i = 0;
        while i < self.tasks.len() {
            let ready = match &self.tasks[i].wait {
                TaskWait::Ready(_) => true,
                TaskWait::Sleep(deadline) => {
                    self.tasks[i].cancel.is_cancelled() || *deadline <= now
                }
                TaskWait::External(_) => false,
            };
            if !ready {
                i += 1;
                continue;
            }
            let drop_task = self.step_task(lua, i, &mut outputs);
            if drop_task {
                self.tasks.swap_remove(i);
            } else {
                i += 1;
            }
        }
        outputs
    }

    fn step_task(&mut self, lua: &Lua, idx: usize, outputs: &mut Vec<TaskDriveOutput>) -> bool {
        let task = &mut self.tasks[idx];
        let resume_args =
            match std::mem::replace(&mut task.wait, TaskWait::Ready(LuaMultiValue::new())) {
                TaskWait::Ready(mv) => mv,
                TaskWait::Sleep(_) => LuaMultiValue::new(),
                TaskWait::External(_) => LuaMultiValue::new(), // unreachable per ready check
            };
        let cancel = task.cancel.clone();
        let result: LuaResult<LuaValue> =
            with_task_cancel(cancel, || task.thread.resume(resume_args));

        match result {
            Ok(v) => {
                if task.thread.status() == mlua::ThreadStatus::Finished {
                    match &task.completion {
                        TaskCompletion::FireAndForget => {}
                        TaskCompletion::ToolResult {
                            request_id,
                            call_id,
                        } => {
                            let (content, is_error) = coerce_tool_result(&v);
                            outputs.push(TaskDriveOutput::ToolComplete {
                                request_id: *request_id,
                                call_id: call_id.clone(),
                                content,
                                is_error,
                            });
                        }
                    }
                    return true;
                }
                match decode_yield(lua, v) {
                    Ok(Yield::Sleep(d)) => {
                        if task.cancel.is_cancelled() {
                            let mut mv = LuaMultiValue::new();
                            mv.push_back(cancelled_marker(lua));
                            task.wait = TaskWait::Ready(mv);
                        } else {
                            task.wait = TaskWait::Sleep(Instant::now() + d);
                        }
                        false
                    }
                    Ok(Yield::External(id)) => {
                        if task.cancel.is_cancelled() {
                            let mut mv = LuaMultiValue::new();
                            mv.push_back(cancelled_marker(lua));
                            task.wait = TaskWait::Ready(mv);
                        } else {
                            task.wait = TaskWait::External(id);
                        }
                        false
                    }
                    Err(msg) => {
                        outputs.push(TaskDriveOutput::Error(format!("task {}: {msg}", task.id)));
                        fail_completion(&task.completion, &msg, outputs);
                        true
                    }
                }
            }
            Err(e) => {
                let msg = e.to_string();
                outputs.push(TaskDriveOutput::Error(format!("task {}: {msg}", task.id)));
                fail_completion(&task.completion, &msg, outputs);
                true
            }
        }
    }
}

// Thread-local cancellation token for the executing coroutine; read by async Lua bindings.
thread_local! {
    static CURRENT_TASK_CANCEL: RefCell<Option<CancellationToken>> = const { RefCell::new(None) };
}

/// Install the task's cancellation token for the closure's duration.
pub fn with_task_cancel<R>(cancel: CancellationToken, f: impl FnOnce() -> R) -> R {
    CURRENT_TASK_CANCEL.with(|c| *c.borrow_mut() = Some(cancel));
    let r = f();
    CURRENT_TASK_CANCEL.with(|c| *c.borrow_mut() = None);
    r
}

/// Current task's cancellation token; `None` when called outside `step_task`.
pub fn current_task_cancel() -> Option<CancellationToken> {
    CURRENT_TASK_CANCEL.with(|c| c.borrow().clone())
}

fn cancelled_marker(lua: &Lua) -> LuaValue {
    lua.create_table()
        .and_then(|t| {
            t.set("__cancelled", true)?;
            Ok(LuaValue::Table(t))
        })
        .unwrap_or(LuaValue::Nil)
}

fn fail_completion(completion: &TaskCompletion, msg: &str, outputs: &mut Vec<TaskDriveOutput>) {
    if let TaskCompletion::ToolResult {
        request_id,
        call_id,
    } = completion
    {
        outputs.push(TaskDriveOutput::ToolComplete {
            request_id: *request_id,
            call_id: call_id.clone(),
            content: format!("tool error: {msg}"),
            is_error: true,
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
    External(u64),
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
            Ok(Yield::External(id))
        }
        other => Err(format!("unknown yield kind: {other}")),
    }
}

/// Coerce a task return value to `(content, is_error)`: string or `{ content, is_error }` table.
fn coerce_tool_result(v: &LuaValue) -> (String, bool) {
    match v {
        LuaValue::String(s) => (s.to_string_lossy().to_string(), false),
        LuaValue::Table(t) => {
            let content: String = t.get("content").unwrap_or_default();
            let is_error: bool = t.get("is_error").unwrap_or(false);
            (content, is_error)
        }
        LuaValue::Nil => (String::new(), false),
        other => (
            format!("tool returned non-string value: {}", other.type_name()),
            true,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(matches!(rt.tasks[0].wait, TaskWait::Sleep(_)));

        // Second drive before deadline — still parked.
        let out = rt.drive(&lua, t0 + Duration::from_millis(50));
        assert!(out.is_empty());
        assert_eq!(rt.tasks.len(), 1);

        // Third drive past deadline — resumes and completes.
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
                request_id: 7,
                call_id: "c1".into(),
            },
        )
        .unwrap();
        let out = rt.drive(&lua, Instant::now());
        assert_eq!(out.len(), 1);
        match &out[0] {
            TaskDriveOutput::ToolComplete {
                request_id,
                call_id,
                content,
                is_error,
            } => {
                assert_eq!(*request_id, 7);
                assert_eq!(call_id, "c1");
                assert_eq!(content, "hello");
                assert!(!*is_error);
            }
            _ => panic!("expected ToolComplete"),
        }
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
                request_id: 1,
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
    fn handler_error_reports_task_error_and_tool_error() {
        let lua = lua_with_sleep();
        let mut rt = LuaTaskRuntime::new();
        let func: mlua::Function = lua.load(r#"function() error("bang") end"#).eval().unwrap();
        rt.spawn(
            &lua,
            func,
            LuaMultiValue::new(),
            TaskCompletion::ToolResult {
                request_id: 2,
                call_id: "y".into(),
            },
        )
        .unwrap();
        let out = rt.drive(&lua, Instant::now());
        // Error notification + failing tool completion.
        let has_error = out
            .iter()
            .any(|o| matches!(o, TaskDriveOutput::Error(m) if m.contains("bang")));
        let has_tool_err = out
            .iter()
            .any(|o| matches!(o, TaskDriveOutput::ToolComplete { is_error: true, .. }));
        assert!(has_error);
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
        assert!(matches!(rt.tasks[0].wait, TaskWait::Sleep(_)));

        // Cancel all tasks.
        rt.cancel_all(&lua);
        assert!(matches!(rt.tasks[0].wait, TaskWait::Ready(_)));

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
        assert!(matches!(rt.tasks[0].wait, TaskWait::External(42)));

        // Cancel all tasks.
        rt.cancel_all(&lua);
        assert!(matches!(rt.tasks[0].wait, TaskWait::Ready(_)));

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

        // Drive: task runs, does sync work, yields sleep.
        // step_task sees cancelled token and replaces the sleep
        // with a Ready(cancel_marker).
        let out = rt.drive(&lua, Instant::now());
        assert!(out.is_empty());
        assert_eq!(rt.tasks.len(), 1);
        assert!(matches!(rt.tasks[0].wait, TaskWait::Ready(_)));

        // Next drive resumes with cancel marker and finishes.
        let out = rt.drive(&lua, Instant::now());
        assert!(out.is_empty());
        assert_eq!(rt.tasks.len(), 0);
    }
}
