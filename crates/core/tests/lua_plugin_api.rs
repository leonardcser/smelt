//! Integration tests for the Lua plugin API surface.
//!
//! Boots a [`LuaRuntime`] with the host-tier API registered and the
//! bundled `_bootstrap.lua` evaluated, then exercises the public
//! primitives (`smelt.task.timeout/race/all`, `smelt.reg.compose`,
//! `smelt.fs.watch`, `smelt.fs.read_async`, `smelt.process.run`,
//! cancellation behavior, …) end-to-end.
//!
//! Tests that need tokio (anything that goes through `tokio::spawn` -
//! `process.run`) use `#[tokio::test]`; everything else uses the
//! plain `#[test]` form.

use smelt_core::lua::LuaRuntime;
use std::time::{Duration, Instant};

/// Inlined copy of `runtime/lua/smelt/_bootstrap.lua` so the host-only
/// runtime tests can evaluate it directly. Loading the full bundled
/// bootstrap chain (`dialog.lua`, `widgets/picker.lua`, …) would pull
/// in UiHost-tier namespaces the core tests don't register.
const BOOTSTRAP_LUA: &str = include_str!("../../../runtime/lua/smelt/_bootstrap.lua");
const READ_PROCESS_OUTPUT_LUA: &str =
    include_str!("../../../runtime/lua/smelt/tools/read_process_output.lua");

/// Build a runtime with bootstrap evaluated. Tests that don't need
/// host I/O can drive coroutines through this directly.
fn fresh() -> LuaRuntime {
    let rt = LuaRuntime::new();
    rt.lua
        .load(BOOTSTRAP_LUA)
        .set_name("smelt/_bootstrap.lua")
        .exec()
        .expect("bootstrap");
    rt
}

/// Pump task events + drive the runtime in a tight loop until `done`
/// returns true or the deadline expires. Sync version (no tokio yield);
/// safe for tests that don't depend on tokio-spawned work.
fn pump_until_sync(rt: &LuaRuntime, ms: u64, done: impl Fn(&LuaRuntime) -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_millis(ms);
    while Instant::now() < deadline {
        rt.pump_task_events();
        let _ = rt.drive_tasks(Instant::now());
        if done(rt) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    false
}

/// Same shape as `pump_until_sync` but yields to tokio between ticks so
/// `tokio::spawn` tasks (e.g. `process.run`) can run.
async fn pump_until_async(rt: &LuaRuntime, ms: u64, done: impl Fn(&LuaRuntime) -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_millis(ms);
    while Instant::now() < deadline {
        rt.pump_task_events();
        let _ = rt.drive_tasks(Instant::now());
        if done(rt) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    false
}

/// Read a Lua global by name. Panics if the global is absent or the
/// wrong type - keeps test bodies free of unwrap chains.
fn get_global<T: mlua::FromLua>(rt: &LuaRuntime, name: &str) -> T {
    rt.lua
        .globals()
        .get::<T>(name)
        .unwrap_or_else(|e| panic!("global `{name}`: {e}"))
}

// -- reg.compose / reg.new ----------------------------------------------

#[test]
fn reg_compose_fires_every_inner_undoer_once() {
    let rt = fresh();
    rt.lua
        .load(
            r#"
            CALLS = {}
            local a = smelt.reg.new(function() table.insert(CALLS, "a") end)
            local b = smelt.reg.new(function() table.insert(CALLS, "b") end)
            REG = smelt.reg.compose(a, b)
            assert(REG:remove() == true)
            assert(REG:remove() == false, "second remove must be a no-op")
            "#,
        )
        .exec()
        .expect("compose");
    let calls: Vec<String> = get_global(&rt, "CALLS");
    assert_eq!(calls, vec!["a", "b"]);
}

#[test]
fn reg_compose_skips_nil_inputs() {
    let rt = fresh();
    rt.lua
        .load(
            r#"
            FIRED = false
            local r = smelt.reg.new(function() FIRED = true end)
            local composed = smelt.reg.compose(nil, r, nil)
            composed:remove()
            "#,
        )
        .exec()
        .expect("compose nil");
    let fired: bool = get_global(&rt, "FIRED");
    assert!(fired, "non-nil inner must still fire");
}

// -- spawn cancellation -------------------------------------------------

#[test]
fn spawn_remove_raises_cancelled_inside_sleep() {
    let rt = fresh();
    rt.lua
        .load(
            r#"
            UNWOUND = false
            REG = smelt.spawn(function()
                local ok, err = pcall(function() smelt.sleep(60000) end)
                UNWOUND = (not ok) and tostring(err):find("cancelled") ~= nil
            end)
            "#,
        )
        .exec()
        .expect("spawn");
    // First drive parks the coroutine on the long sleep.
    rt.pump_task_events();
    let _ = rt.drive_tasks(Instant::now());
    let unwound_before: bool = get_global(&rt, "UNWOUND");
    assert!(!unwound_before, "must still be parked");

    // Cancel via the returned Reg, then drive to let the coroutine unwind.
    rt.lua.load("REG:remove()").exec().unwrap();
    assert!(pump_until_sync(&rt, 500, |rt| {
        rt.lua.globals().get::<bool>("UNWOUND").unwrap_or(false)
    }));
}

// -- task.timeout -------------------------------------------------------

#[test]
fn task_timeout_returns_result_when_fast() {
    let rt = fresh();
    rt.lua
        .load(
            r#"
            smelt.spawn(function()
                local out, err = smelt.task.timeout(500, function()
                    smelt.sleep(5)
                    return 42
                end)
                RESULT = out
                ERR = err
                DONE = true
            end)
            "#,
        )
        .exec()
        .expect("spawn timeout");
    assert!(pump_until_sync(&rt, 1000, |rt| rt
        .lua
        .globals()
        .get::<bool>("DONE")
        .unwrap_or(false)));
    let result: Option<i64> = get_global(&rt, "RESULT");
    let err: Option<String> = get_global(&rt, "ERR");
    assert_eq!(result, Some(42));
    assert_eq!(err, None);
}

// NOTE: `task_timeout_returns_nil_err_on_deadline` would belong here
// but timer firings depend on `Core::timers` which the host-only test
// runtime doesn't install. The deadline path is covered by the
// TUI-integration test suite where a full `Core` drives `drain_due`.

// -- task.race ----------------------------------------------------------

#[test]
fn task_race_returns_first_to_finish() {
    let rt = fresh();
    rt.lua
        .load(
            r#"
            smelt.spawn(function()
                local idx, val = smelt.task.race(
                    function() smelt.sleep(100); return "slow" end,
                    function() smelt.sleep(5);   return "fast" end
                )
                INDEX = idx
                RESULT = val
                DONE = true
            end)
            "#,
        )
        .exec()
        .expect("spawn race");
    assert!(pump_until_sync(&rt, 1000, |rt| rt
        .lua
        .globals()
        .get::<bool>("DONE")
        .unwrap_or(false)));
    let index: i64 = get_global(&rt, "INDEX");
    let result: String = get_global(&rt, "RESULT");
    assert_eq!(index, 2);
    assert_eq!(result, "fast");
}

// -- task.all -----------------------------------------------------------

#[test]
fn task_all_preserves_input_order() {
    let rt = fresh();
    rt.lua
        .load(
            r#"
            smelt.spawn(function()
                local results = smelt.task.all(
                    function() smelt.sleep(30); return "a" end,
                    function() smelt.sleep(5);  return "b" end,
                    function() smelt.sleep(15); return "c" end
                )
                R1, R2, R3 = results[1], results[2], results[3]
                DONE = true
            end)
            "#,
        )
        .exec()
        .expect("spawn all");
    assert!(pump_until_sync(&rt, 1000, |rt| rt
        .lua
        .globals()
        .get::<bool>("DONE")
        .unwrap_or(false)));
    let r1: String = get_global(&rt, "R1");
    let r2: String = get_global(&rt, "R2");
    let r3: String = get_global(&rt, "R3");
    assert_eq!((r1.as_str(), r2.as_str(), r3.as_str()), ("a", "b", "c"));
}

// -- fs.read_async / fs.write_async -------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn fs_async_round_trip() {
    let tmp = tempfile::NamedTempFile::new().expect("tmp");
    let path = tmp.path().to_string_lossy().into_owned();
    let rt = fresh();
    rt.lua.globals().set("PATH", path.clone()).unwrap();
    rt.lua
        .load(
            r#"
            smelt.spawn(function()
                local ok = smelt.fs.write_async(PATH, "hello from async")
                WROTE = ok
                local content = smelt.fs.read_async(PATH)
                CONTENT = content
                DONE = true
            end)
            "#,
        )
        .exec()
        .expect("spawn fs round-trip");
    assert!(
        pump_until_async(&rt, 1000, |rt| rt
            .lua
            .globals()
            .get::<bool>("DONE")
            .unwrap_or(false))
        .await
    );
    let wrote: bool = get_global(&rt, "WROTE");
    let content: String = get_global(&rt, "CONTENT");
    assert!(wrote);
    assert_eq!(content, "hello from async");
}

// -- process.run --------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn process_run_happy_path() {
    let rt = fresh();
    rt.lua
        .load(
            r#"
            smelt.spawn(function()
                local out, err = smelt.process.run("echo", { "hello async" })
                STDOUT = out and out.stdout or ""
                EXIT = out and out.exit_code or -99
                ERR = err
                DONE = true
            end)
            "#,
        )
        .exec()
        .expect("spawn run");
    assert!(
        pump_until_async(&rt, 2000, |rt| rt
            .lua
            .globals()
            .get::<bool>("DONE")
            .unwrap_or(false))
        .await
    );
    let stdout: String = get_global(&rt, "STDOUT");
    let exit: i64 = get_global(&rt, "EXIT");
    let err: Option<String> = get_global(&rt, "ERR");
    assert_eq!(stdout.trim_end(), "hello async");
    assert_eq!(exit, 0);
    assert!(err.is_none());
}

#[tokio::test(flavor = "current_thread")]
async fn process_run_kills_child_on_cancel() {
    let rt = fresh();
    // Spawn a coroutine that runs `sleep 5`, then cancel it after a short
    // delay. The coroutine must unwind with `cancelled` (raised by the
    // task runtime when the cancellation marker reaches the wait).
    rt.lua
        .load(
            r#"
            CANCELLED = false
            REG = smelt.spawn(function()
                local ok, err = pcall(function()
                    smelt.process.run("sleep", { "5" })
                end)
                CANCELLED = (not ok) and tostring(err):find("cancelled") ~= nil
            end)
            "#,
        )
        .exec()
        .expect("spawn run");
    // Let smelt.process.run actually launch its tokio task + spawn the child.
    let _ = pump_until_async(&rt, 200, |_| false).await;

    let started = Instant::now();
    rt.lua.load("REG:remove()").exec().unwrap();
    assert!(
        pump_until_async(&rt, 2000, |rt| rt
            .lua
            .globals()
            .get::<bool>("CANCELLED")
            .unwrap_or(false))
        .await,
        "coroutine never observed cancelled"
    );
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(3),
        "child should die fast after cancel; took {elapsed:?}"
    );
}

// -- read_process_output tool ---------------------------------------------

#[test]
fn read_process_output_tool_is_snapshot_only() {
    let rt = fresh();
    rt.lua
        .load(
            r#"
            local raw_register = smelt.tools.register
            CAPTURED_TOOL = nil
            OUTPUT_ID = nil
            smelt.tools.register = function(def)
                CAPTURED_TOOL = def
                return raw_register(def)
            end
            smelt.process.output = function(id)
                OUTPUT_ID = id
                return { text = "partial", running = true, elapsed_secs = 12 }
            end
            "#,
        )
        .exec()
        .expect("install capture");
    rt.lua
        .load(READ_PROCESS_OUTPUT_LUA)
        .set_name("smelt/tools/read_process_output.lua")
        .exec()
        .expect("load read_process_output");

    let tool: mlua::Table = get_global(&rt, "CAPTURED_TOOL");
    let parameters: mlua::Table = tool.get("parameters").expect("parameters");
    let properties: mlua::Table = parameters.get("properties").expect("properties");
    assert!(matches!(
        properties.get::<mlua::Value>("wait").unwrap(),
        mlua::Value::Nil
    ));
    assert!(matches!(
        properties.get::<mlua::Value>("timeout_ms").unwrap(),
        mlua::Value::Nil
    ));

    let args = rt.lua.create_table().unwrap();
    args.set("id", "proc_1").unwrap();
    // Older callers may still pass these fields; they must not make the tool wait.
    args.set("wait", true).unwrap();
    args.set("timeout_ms", 60_000).unwrap();

    let execute: mlua::Function = tool.get("execute").expect("execute");
    let started = Instant::now();
    let result: mlua::Value = execute.call(args).expect("execute read_process_output");
    assert!(
        started.elapsed() < Duration::from_millis(100),
        "read_process_output should return a snapshot immediately"
    );

    let mlua::Value::String(content) = result else {
        panic!("expected string result, got {result:?}");
    };
    assert_eq!(content.to_string_lossy(), "partial\n[running]");
    let output_id: String = get_global(&rt, "OUTPUT_ID");
    assert_eq!(output_id, "proc_1");
}

// -- fs.watch -----------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn fs_watch_delivers_events_until_removed() {
    let dir = tempfile::tempdir().expect("tmp");
    let dir_path = dir.path().to_string_lossy().into_owned();
    let rt = fresh();
    rt.lua.globals().set("WATCH_DIR", dir_path.clone()).unwrap();
    rt.lua
        .load(
            r#"
            EVENTS = 0
            REG = smelt.fs.watch(WATCH_DIR, function(_ev)
                EVENTS = EVENTS + 1
            end)
            "#,
        )
        .exec()
        .expect("watch setup");

    // Let the polling coroutine reach its first __watch_arm before we
    // mutate the directory.
    let _ = pump_until_async(&rt, 50, |_| false).await;

    std::fs::write(dir.path().join("a.txt"), "one").unwrap();
    // notify backends batch events; give them up to 2s on macOS (FSEvents
    // delivers via a thread with a small flush interval).
    assert!(
        pump_until_async(&rt, 2000, |rt| {
            rt.lua.globals().get::<i64>("EVENTS").unwrap_or(0) > 0
        })
        .await,
        "no events observed"
    );

    // Stop the watcher; further mutations must not increment.
    let events_before: i64 = get_global(&rt, "EVENTS");
    rt.lua.load("REG:remove()").exec().unwrap();
    let _ = pump_until_async(&rt, 100, |_| false).await;
    std::fs::write(dir.path().join("b.txt"), "two").unwrap();
    let _ = pump_until_async(&rt, 400, |_| false).await;
    let events_after: i64 = get_global(&rt, "EVENTS");
    assert_eq!(events_after, events_before, "no events after remove");
}

// -- smelt.grep.run -----------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn grep_run_files_with_matches_happy_path() {
    let dir = tempfile::tempdir().expect("tmp");
    std::fs::write(dir.path().join("a.rs"), "needle\n").unwrap();
    std::fs::write(dir.path().join("b.txt"), "needle\n").unwrap();
    let dir_path = dir.path().to_string_lossy().into_owned();
    let rt = fresh();
    rt.lua.globals().set("GREP_DIR", dir_path.clone()).unwrap();
    rt.lua
        .load(
            r#"
            smelt.spawn(function()
                local out, err = smelt.grep.run("needle", GREP_DIR, {
                    mode = "files_with_matches",
                    glob = "*.rs",
                    timeout_secs = 5
                })
                RESULT = out
                ERR = err
                DONE = true
            end)
            "#,
        )
        .exec()
        .expect("spawn grep");
    assert!(
        pump_until_async(&rt, 10000, |rt| rt
            .lua
            .globals()
            .get::<bool>("DONE")
            .unwrap_or(false))
        .await,
        "grep never completed"
    );
    let done: bool = get_global(&rt, "DONE");
    assert!(done);
    let err: Option<String> = get_global(&rt, "ERR");
    assert!(err.is_none(), "unexpected err: {:?}", err);
    let result: mlua::Table = rt.lua.globals().get("RESULT").expect("RESULT table");
    let stdout: String = result.get("stdout").expect("stdout field");
    let stderr: String = result.get("stderr").expect("stderr field");
    let exit_code: i32 = result.get("exit_code").expect("exit_code field");
    let timed_out: bool = result.get("timed_out").expect("timed_out field");
    assert!(!timed_out, "grep should not time out");
    assert!(stdout.contains("a.rs"), "should find a.rs");
    assert!(!stdout.contains("b.txt"), "glob should exclude b.txt");
    assert_eq!(exit_code, 0, "grep should succeed");
    assert!(stderr.is_empty(), "stderr should be empty");
}

// -- smelt.state --------------------------------------------------------

#[test]
fn smelt_state_returns_same_table_for_same_name() {
    let rt = fresh();
    rt.lua
        .load(
            r#"
            local a = smelt.state("plugin_x")
            a.counter = 1
            local b = smelt.state("plugin_x")
            SAME = rawequal(a, b)
            B_COUNTER = b.counter
            "#,
        )
        .exec()
        .expect("state");
    let same: bool = get_global(&rt, "SAME");
    let counter: i64 = get_global(&rt, "B_COUNTER");
    assert!(same, "second call must return the same table");
    assert_eq!(counter, 1);
}
