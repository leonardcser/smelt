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

use smelt_core::lua::{LuaRuntime, TaskDriveOutput, ToolEnv, ToolExecResult, ToolVisibility};
use smelt_core::permissions::ToolEffectKind;
use std::collections::HashMap;
use std::path::Path;
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Inlined copy of `runtime/lua/smelt/_bootstrap.lua` so the host-only
/// runtime tests can evaluate it directly. Loading the full bundled
/// bootstrap chain (`dialog.lua`, `widgets/picker.lua`, …) would pull
/// in UiHost-tier namespaces the core tests don't register.
const BOOTSTRAP_LUA: &str = include_str!("../../../runtime/lua/smelt/_bootstrap.lua");
const READ_PROCESS_OUTPUT_LUA: &str =
    include_str!("../../../runtime/lua/smelt/tools/read_process_output.lua");
const LSP_PLUGIN_LUA: &str = include_str!("../../../runtime/lua/smelt/plugins/lsp.lua");

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

// -- time ---------------------------------------------------------------

#[test]
fn time_api_parses_and_formats_iso8601_timestamps() {
    let rt = fresh();
    rt.lua
        .load(
            r#"
            assert(smelt.clock == nil)
            assert(type(smelt.time.now()) == "number")
            assert(type(smelt.time.now_ms()) == "number")
            local stamp = assert(smelt.time.parse_iso8601("2026-07-31T20:08:39.887867Z"))
            assert(smelt.time.format_utc(stamp, "%Y-%m-%d %H:%M") == "2026-07-31 20:08")
            assert(smelt.time.parse_iso8601("not a timestamp") == nil)
            "#,
        )
        .exec()
        .expect("time api");
}

// -- json ---------------------------------------------------------------

#[test]
fn json_api_round_trips_tables_and_reports_decode_errors() {
    let rt = fresh();
    rt.lua
        .load(
            r#"
            local encoded = smelt.json.encode({
                kind = "smelt.plan",
                title = "quote \" ok",
                items = { "a", "b" },
            }, { pretty = true })
            assert(encoded:match('"kind"'))
            assert(encoded:match('"items"'))
            local decoded, err = smelt.json.decode(encoded)
            assert(err == nil, err)
            assert(decoded.kind == "smelt.plan")
            assert(decoded.title == 'quote " ok')
            assert(decoded.items[2] == "b")
            local bad, bad_err = smelt.json.decode("{")
            assert(bad == nil)
            assert(type(bad_err) == "string" and #bad_err > 0)
            "#,
        )
        .exec()
        .expect("json api round trip");
}

// -- tools.register -------------------------------------------------------

#[test]
fn tools_register_records_effect_metadata() {
    let rt = fresh();
    rt.lua
        .load(
            r#"
            smelt.tools.register({
                name = "test_write_tool",
                effect = "write",
                execute = function() return "ok" end,
            })
            smelt.tools.register({
                name = "test_process_tool",
                effect = "process",
                execute = function() return "ok" end,
            })
            "#,
        )
        .exec()
        .expect("register tools with effects");

    let defaults = rt.tool_defaults();
    assert_eq!(
        defaults.tool_effects.get("test_write_tool"),
        Some(&ToolEffectKind::Write)
    );
    assert_eq!(
        defaults.tool_effects.get("test_process_tool"),
        Some(&ToolEffectKind::Process)
    );
}

#[test]
fn tools_patch_updates_and_restores_metadata() {
    let rt = fresh();
    rt.lua
        .load(
            r#"
            smelt.tools.register({
                name = "patchable_tool",
                description = "before",
                parameters = { type = "object", properties = {} },
                execute = function() return "ok" end,
            })
            reg = smelt.tools.patch("patchable_tool", {
                description = "after",
                parameters = {
                    type = "object",
                    properties = { query = { type = "string" } },
                    required = { "query" },
                },
            })
            "#,
        )
        .exec()
        .expect("patch tool metadata");

    let defs = rt.tool_defs(protocol::AgentMode::normal(), ToolVisibility::Interactive);
    let def = defs
        .iter()
        .find(|def| def.name == "patchable_tool")
        .expect("patched tool def");
    assert_eq!(def.description, "after");
    assert!(def.parameters["properties"].get("query").is_some());

    rt.lua
        .load("reg:remove()")
        .exec()
        .expect("restore tool metadata");
    let defs = rt.tool_defs(protocol::AgentMode::normal(), ToolVisibility::Interactive);
    let def = defs
        .iter()
        .find(|def| def.name == "patchable_tool")
        .expect("restored tool def");
    assert_eq!(def.description, "before");
    assert!(def.parameters["properties"].get("query").is_none());
}

#[test]
fn agent_system_prompt_fragments_are_registered_and_removed() {
    let rt = fresh();
    rt.lua
        .load(
            r#"
            reg = smelt.agent.add_system_prompt("extra guidance")
            "#,
        )
        .exec()
        .expect("add system prompt fragment");
    assert_eq!(
        rt.system_prompt_fragments(),
        vec!["extra guidance".to_string()]
    );

    rt.lua
        .load("reg:remove()")
        .exec()
        .expect("remove system prompt fragment");
    assert!(rt.system_prompt_fragments().is_empty());
}

#[test]
fn execute_tool_does_not_hold_task_mutex_while_stepping_handler() {
    #[derive(Debug)]
    enum Msg {
        ReturnedPending,
        ReturnedImmediate,
        Completed(String, bool),
    }

    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let rt = fresh();
        rt.lua
            .load(
                r#"
                smelt.tools.register({
                    name = "task_all_tool",
                    execute = function()
                        local results = smelt.task.all(
                            function() return "a" end,
                            function() return "b" end
                        )
                        return results[1] .. "," .. results[2]
                    end,
                })
                "#,
            )
            .exec()
            .expect("register task_all_tool");

        let result = rt.execute_tool(
            "task_all_tool",
            &HashMap::new(),
            42,
            "call-1",
            ToolEnv {
                mode: protocol::AgentMode::normal(),
                session_id: "sess",
                session_dir: Path::new("/tmp"),
            },
            Instant::now(),
        );
        match result {
            ToolExecResult::Pending => tx.send(Msg::ReturnedPending).unwrap(),
            ToolExecResult::Immediate { .. } => {
                tx.send(Msg::ReturnedImmediate).unwrap();
                return;
            }
        }

        let deadline = Instant::now() + Duration::from_secs(1);
        while Instant::now() < deadline {
            rt.pump_task_events();
            for out in rt.drive_tasks(Instant::now()) {
                if let TaskDriveOutput::ToolComplete {
                    request_id,
                    call_id,
                    content,
                    is_error,
                    ..
                } = out
                {
                    assert_eq!(request_id, 42);
                    assert_eq!(call_id, "call-1");
                    tx.send(Msg::Completed(content, is_error)).unwrap();
                    return;
                }
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    });

    match rx.recv_timeout(Duration::from_millis(500)) {
        Ok(Msg::ReturnedPending) => {}
        Ok(Msg::ReturnedImmediate) => panic!("unexpected immediate result from execute_tool"),
        Ok(Msg::Completed(content, is_error)) => {
            panic!("tool completed before execute_tool returned pending: is_error={is_error}, content={content}")
        }
        Err(_) => panic!("execute_tool did not return; likely held task mutex across Lua resume"),
    }

    match rx.recv_timeout(Duration::from_secs(1)) {
        Ok(Msg::Completed(content, is_error)) => {
            assert!(!is_error, "tool completed with error: {content}");
            assert_eq!(content, "a,b");
        }
        Ok(Msg::ReturnedPending) => panic!("execute_tool returned pending twice"),
        Ok(Msg::ReturnedImmediate) => panic!("unexpected late immediate result"),
        Err(_) => panic!("task_all_tool did not complete after execute_tool returned"),
    }
}

#[test]
fn parallel_tool_execute_steps_the_new_task_not_an_older_ready_task() {
    let rt = fresh();
    rt.lua
        .load(
            r#"
            smelt.tools.register({
                name = "yield_once",
                execute = function(args)
                    smelt.sleep(0)
                    return args.value
                end,
            })
            "#,
        )
        .exec()
        .expect("register yield_once");

    let now = Instant::now();
    let mut first_args = HashMap::new();
    first_args.insert("value".into(), serde_json::json!("first"));
    let first = rt.execute_tool(
        "yield_once",
        &first_args,
        1,
        "call-1",
        ToolEnv {
            mode: protocol::AgentMode::normal(),
            session_id: "sess",
            session_dir: Path::new("/tmp"),
        },
        now,
    );
    assert!(matches!(first, ToolExecResult::Pending));

    let mut second_args = HashMap::new();
    second_args.insert("value".into(), serde_json::json!("second"));
    let second = rt.execute_tool(
        "yield_once",
        &second_args,
        2,
        "call-2",
        ToolEnv {
            mode: protocol::AgentMode::normal(),
            session_id: "sess",
            session_dir: Path::new("/tmp"),
        },
        now,
    );
    assert!(matches!(second, ToolExecResult::Pending));

    let outs = rt.drive_tasks(now);
    let completions: Vec<_> = outs
        .into_iter()
        .filter_map(|out| match out {
            TaskDriveOutput::ToolComplete {
                request_id,
                call_id,
                content,
                is_error,
                ..
            } => Some((request_id, call_id, content, is_error)),
            _ => None,
        })
        .collect();

    assert_eq!(
        completions,
        vec![
            (1, "call-1".to_string(), "first".to_string(), false),
            (2, "call-2".to_string(), "second".to_string(), false),
        ]
    );
}

#[test]
fn tool_execute_preserves_result_metadata() {
    let rt = fresh();
    rt.lua
        .load(
            r#"
            smelt.tools.register({
                name = "metadata_tool",
                execute = function()
                    return {
                        content = "{\"ok\":true}",
                        metadata = { syntax = "json" },
                    }
                end,
            })
            "#,
        )
        .exec()
        .expect("register metadata_tool");

    let result = rt.execute_tool(
        "metadata_tool",
        &HashMap::new(),
        3,
        "call-metadata",
        ToolEnv {
            mode: protocol::AgentMode::normal(),
            session_id: "sess",
            session_dir: Path::new("/tmp"),
        },
        Instant::now(),
    );

    match result {
        ToolExecResult::Immediate {
            content,
            is_error,
            metadata,
        } => {
            assert_eq!(content, "{\"ok\":true}");
            assert!(!is_error);
            assert_eq!(metadata, Some(serde_json::json!({ "syntax": "json" })));
        }
        ToolExecResult::Pending => panic!("metadata_tool should complete immediately"),
    }
}

#[test]
fn lsp_plugin_formats_semantic_results_as_text() {
    let rt = fresh();
    rt.lua
        .load(
            r#"
            smelt.lsp = {
                configure = function(_) end,
                __call = function(id, operation, _)
                    assert(operation == "outline")
                    smelt.task.resume(id, { result = {
                        file_path = "/tmp/example.rs",
                        total = 2,
                        shown = 2,
                        omitted = 0,
                        filters = { max_depth = 1 },
                        symbols = {
                            { kind = "function", name = "outer", line = 3, column = 1, children = {
                                { kind = "method", name = "inner", line = 5, column = 3 },
                            } },
                        },
                    } })
                end,
            }
            for _, name in ipairs({ "glob", "grep", "edit_file" }) do
                smelt.tools.register({ name = name, execute = function() return "" end })
            end
            "#,
        )
        .exec()
        .expect("stub lsp backend");
    rt.lua
        .load(LSP_PLUGIN_LUA)
        .set_name("smelt/plugins/lsp.lua")
        .exec()
        .expect("load lsp plugin");

    let mut args = HashMap::new();
    args.insert("file_path".into(), serde_json::json!("/tmp/example.rs"));
    let result = rt.execute_tool(
        "outline",
        &args,
        4,
        "call-get-file-outline",
        ToolEnv {
            mode: protocol::AgentMode::normal(),
            session_id: "sess",
            session_dir: Path::new("/tmp"),
        },
        Instant::now(),
    );

    let (content, is_error, metadata) = match result {
        ToolExecResult::Immediate {
            content,
            is_error,
            metadata,
        } => (content, is_error, metadata),
        ToolExecResult::Pending => {
            rt.pump_task_events();
            rt.drive_tasks(Instant::now())
                .into_iter()
                .find_map(|out| match out {
                    TaskDriveOutput::ToolComplete {
                        request_id: 4,
                        call_id,
                        content,
                        is_error,
                        metadata,
                    } if call_id == "call-get-file-outline" => Some((content, is_error, metadata)),
                    _ => None,
                })
                .expect("outline completion")
        }
    };

    assert!(!is_error);
    assert!(content.contains("Outline: /tmp/example.rs"));
    assert!(content.contains("Filters: max_depth=1"));
    assert!(content.contains("- fn outer @ 3:1"));
    assert!(content.contains("  - method inner @ 5:3"));
    assert!(!content.contains("\"symbols\""));
    assert!(metadata.is_none() || metadata == Some(serde_json::Value::Null));
}

#[test]
fn lsp_plugin_formats_null_outline_result_as_empty_outline() {
    let rt = fresh();
    rt.lua
        .load(
            r#"
            smelt.lsp = {
                configure = function(_) end,
                __call = function(id, operation, _)
                    assert(operation == "outline")
                    smelt.task.resume(id, {})
                end,
            }
            for _, name in ipairs({ "glob", "grep", "edit_file" }) do
                smelt.tools.register({ name = name, execute = function() return "" end })
            end
            "#,
        )
        .exec()
        .expect("stub lsp backend");
    rt.lua
        .load(LSP_PLUGIN_LUA)
        .set_name("smelt/plugins/lsp.lua")
        .exec()
        .expect("load lsp plugin");

    let mut args = HashMap::new();
    args.insert("file_path".into(), serde_json::json!("/tmp/empty.rs"));
    let result = rt.execute_tool(
        "outline",
        &args,
        7,
        "call-null-outline",
        ToolEnv {
            mode: protocol::AgentMode::normal(),
            session_id: "sess",
            session_dir: Path::new("/tmp"),
        },
        Instant::now(),
    );

    let (content, is_error) = match result {
        ToolExecResult::Immediate {
            content, is_error, ..
        } => (content, is_error),
        ToolExecResult::Pending => {
            rt.pump_task_events();
            rt.drive_tasks(Instant::now())
                .into_iter()
                .find_map(|out| match out {
                    TaskDriveOutput::ToolComplete {
                        request_id: 7,
                        call_id,
                        content,
                        is_error,
                        ..
                    } if call_id == "call-null-outline" => Some((content, is_error)),
                    _ => None,
                })
                .expect("outline completion")
        }
    };

    assert!(!is_error);
    assert!(content.contains("Outline: /tmp/empty.rs"));
    assert!(content.contains("Symbols: 0 shown of 0"));
}

#[test]
fn lsp_plugin_registers_agent_friendly_semantic_tools() {
    let rt = fresh();
    rt.lua
        .load(
            r#"
            smelt.lsp = { configure = function(_) end }
            for _, name in ipairs({ "glob", "grep", "edit_file" }) do
                smelt.tools.register({ name = name, execute = function() return "" end })
            end
            "#,
        )
        .exec()
        .expect("stub lsp backend");
    rt.lua
        .load(LSP_PLUGIN_LUA)
        .set_name("smelt/plugins/lsp.lua")
        .exec()
        .expect("load lsp plugin");

    let defs = rt.tool_defs(protocol::AgentMode::normal(), ToolVisibility::Interactive);
    let outline = defs
        .iter()
        .find(|def| def.name == "outline")
        .expect("outline tool");
    assert_eq!(
        outline.parameters["required"],
        serde_json::json!(["file_path"])
    );
    assert!(outline.parameters["properties"].get("symbol").is_some());
    assert!(outline.parameters["properties"].get("max_depth").is_some());
    assert!(outline.parameters["properties"]
        .get("around_line")
        .is_none());
    assert!(outline.parameters["properties"]
        .get("name_contains")
        .is_some());

    let find = defs
        .iter()
        .find(|def| def.name == "find_symbol")
        .expect("find_symbol tool");
    assert!(find.description.contains("semantic name search"));
    assert!(find.parameters["properties"].get("path_glob").is_some());
    assert!(find.parameters["properties"].get("exact").is_some());
    assert_eq!(find.parameters["required"], serde_json::json!(["query"]));

    let inspect_at = defs
        .iter()
        .find(|def| def.name == "inspect_symbol_at")
        .expect("inspect_symbol_at tool");
    assert!(inspect_at.parameters["properties"].get("depth").is_some());
    assert_eq!(
        inspect_at.parameters["required"],
        serde_json::json!(["file_path", "line", "column"])
    );

    let inspect = defs
        .iter()
        .find(|def| def.name == "inspect_symbol")
        .expect("inspect_symbol tool");
    assert!(inspect.parameters["properties"].get("depth").is_some());
    assert!(inspect.parameters["properties"].get("query").is_some());
    assert_eq!(inspect.parameters["required"], serde_json::json!(["query"]));

    let refs = defs
        .iter()
        .find(|def| def.name == "find_references")
        .expect("find_references tool");
    assert!(refs.description.contains("normalized locations"));
    assert!(refs.parameters["properties"].get("raw").is_some());
    assert!(refs.parameters["properties"].get("group_by").is_none());

    for (name, required) in [
        ("language_server_status", serde_json::Value::Null),
        (
            "find_definition",
            serde_json::json!(["file_path", "line", "column"]),
        ),
        ("diagnostics", serde_json::Value::Null),
        (
            "preview_rename",
            serde_json::json!(["file_path", "line", "column", "new_name"]),
        ),
        (
            "rename_symbol",
            serde_json::json!(["file_path", "line", "column", "new_name"]),
        ),
    ] {
        let tool = defs
            .iter()
            .find(|definition| definition.name == name)
            .unwrap_or_else(|| panic!("{name} tool"));
        assert_eq!(tool.parameters["required"], required, "{name}");
    }
}

#[test]
fn lsp_plugin_formats_reference_summaries_as_text() {
    let rt = fresh();
    rt.lua
        .load(
            r#"
            smelt.lsp = {
                configure = function(_) end,
                __call = function(id, operation, _)
                    assert(operation == "references")
                    smelt.task.resume(id, { result = {
                        symbol = { file_path = "/tmp/example.rs", line = 10, column = 4 },
                        total = 2,
                        shown = 2,
                        omitted = 0,
                        locations = {
                            { file_path = "/tmp/example.rs", line = 12, column = 8, preview = "call_site()" },
                            { file_path = "/tmp/example.rs", line = 18, column = 2 },
                        },
                    } })
                end,
            }
            for _, name in ipairs({ "glob", "grep", "edit_file" }) do
                smelt.tools.register({ name = name, execute = function() return "" end })
            end
            "#,
        )
        .exec()
        .expect("stub lsp backend");
    rt.lua
        .load(LSP_PLUGIN_LUA)
        .set_name("smelt/plugins/lsp.lua")
        .exec()
        .expect("load lsp plugin");

    let mut args = HashMap::new();
    args.insert("file_path".into(), serde_json::json!("/tmp/example.rs"));
    args.insert("line".into(), serde_json::json!(10));
    args.insert("column".into(), serde_json::json!(4));
    let result = rt.execute_tool(
        "find_references",
        &args,
        6,
        "call-lsp-references-text",
        ToolEnv {
            mode: protocol::AgentMode::normal(),
            session_id: "sess",
            session_dir: Path::new("/tmp"),
        },
        Instant::now(),
    );

    let (content, is_error, metadata) = match result {
        ToolExecResult::Immediate {
            content,
            is_error,
            metadata,
        } => (content, is_error, metadata),
        ToolExecResult::Pending => {
            rt.pump_task_events();
            rt.drive_tasks(Instant::now())
                .into_iter()
                .find_map(|out| match out {
                    TaskDriveOutput::ToolComplete {
                        request_id: 6,
                        call_id,
                        content,
                        is_error,
                        metadata,
                    } if call_id == "call-lsp-references-text" => {
                        Some((content, is_error, metadata))
                    }
                    _ => None,
                })
                .expect("find_references completion")
        }
    };

    assert!(!is_error);
    assert!(content.contains("References for /tmp/example.rs:10:4: 2 total, 2 shown"));
    assert!(content.contains("/tmp/example.rs:12:8 - call_site()"));
    assert!(!content.contains("\"locations\""));
    assert!(metadata.is_none() || metadata == Some(serde_json::Value::Null));
}

#[test]
fn lsp_plugin_truncates_large_structured_results() {
    let rt = fresh();
    rt.lua
        .load(
            r#"
            smelt.lsp = {
                configure = function(_) end,
                __call = function(id, operation, _)
                    assert(operation == "references")
                    local refs = {}
                    for i = 1, 205 do
                        refs[i] = {
                            uri = "file:///tmp/example" .. i .. ".rs",
                            range = {
                                start = { line = i, character = 1 },
                                ["end"] = { line = i, character = 2 },
                            },
                        }
                    end
                    smelt.task.resume(id, { result = refs })
                end,
            }
            for _, name in ipairs({ "glob", "grep", "edit_file" }) do
                smelt.tools.register({ name = name, execute = function() return "" end })
            end
            "#,
        )
        .exec()
        .expect("stub lsp backend");
    rt.lua
        .load(LSP_PLUGIN_LUA)
        .set_name("smelt/plugins/lsp.lua")
        .exec()
        .expect("load lsp plugin");

    let mut args = HashMap::new();
    args.insert("file_path".into(), serde_json::json!("/tmp/example.rs"));
    args.insert("line".into(), serde_json::json!(1));
    args.insert("column".into(), serde_json::json!(1));
    let result = rt.execute_tool(
        "find_references",
        &args,
        5,
        "call-lsp-references",
        ToolEnv {
            mode: protocol::AgentMode::normal(),
            session_id: "sess",
            session_dir: Path::new("/tmp"),
        },
        Instant::now(),
    );

    let (content, is_error, metadata) = match result {
        ToolExecResult::Immediate {
            content,
            is_error,
            metadata,
        } => (content, is_error, metadata),
        ToolExecResult::Pending => {
            rt.pump_task_events();
            rt.drive_tasks(Instant::now())
                .into_iter()
                .find_map(|out| match out {
                    TaskDriveOutput::ToolComplete {
                        request_id: 5,
                        call_id,
                        content,
                        is_error,
                        metadata,
                    } if call_id == "call-lsp-references" => Some((content, is_error, metadata)),
                    _ => None,
                })
                .expect("find_references completion")
        }
    };

    assert!(!is_error);
    assert!(content.contains("\"truncated\": true"));
    assert!(content.contains("\"total\": 205"));
    assert!(content.contains("\"shown\": 200"));
    assert!(!content.contains("example205.rs"));
    assert_eq!(metadata, Some(serde_json::json!({ "syntax": "json" })));
}
#[test]
fn tool_timeout_completes_a_parked_tool_with_error() {
    let rt = fresh();
    rt.lua
        .load(
            r#"
            smelt.tools.register({
                name = "slow_tool",
                execute = function()
                    smelt.sleep(1000)
                    return "too late"
                end,
            })
            "#,
        )
        .exec()
        .expect("register slow_tool");

    let now = Instant::now();
    let mut args = HashMap::new();
    args.insert("timeout_ms".into(), serde_json::json!(5));
    let result = rt.execute_tool(
        "slow_tool",
        &args,
        9,
        "call-timeout",
        ToolEnv {
            mode: protocol::AgentMode::normal(),
            session_id: "sess",
            session_dir: Path::new("/tmp"),
        },
        now,
    );
    assert!(matches!(result, ToolExecResult::Pending));

    let outs = rt.drive_tasks(now + Duration::from_millis(6));
    assert!(outs.iter().any(|out| matches!(
        out,
        TaskDriveOutput::ToolComplete {
            request_id: 9,
            call_id,
            content,
            is_error: true,
            ..
        } if call_id == "call-timeout" && content.contains("timed out")
    )));
}

#[test]
fn tool_watchdog_uses_explicit_timeout_arg_metadata() {
    let rt = fresh();
    rt.lua
        .load(
            r#"
            smelt.tools.register({
                name = "seconds_timeout_tool",
                watchdog_timeout_arg = "deadline",
                watchdog_timeout_arg_scale_ms = 1000,
                watchdog_grace_ms = 5,
                watchdog_max_timeout_ms = 2000,
                execute = function()
                    smelt.sleep(5000)
                    return "too late"
                end,
            })
            "#,
        )
        .exec()
        .expect("register seconds_timeout_tool");

    let now = Instant::now();
    let mut args = HashMap::new();
    args.insert("deadline".into(), serde_json::json!(1));
    let result = rt.execute_tool(
        "seconds_timeout_tool",
        &args,
        10,
        "call-deadline",
        ToolEnv {
            mode: protocol::AgentMode::normal(),
            session_id: "sess",
            session_dir: Path::new("/tmp"),
        },
        now,
    );
    assert!(matches!(result, ToolExecResult::Pending));

    let outs = rt.drive_tasks(now + Duration::from_millis(1005));
    assert!(outs.iter().any(|out| matches!(
        out,
        TaskDriveOutput::ToolComplete {
            request_id: 10,
            call_id,
            content,
            is_error: true,
            ..
        } if call_id == "call-deadline" && content.contains("timed out after 1.0s")
    )));
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

#[test]
fn turn_cancellation_preserves_top_level_spawned_app_tasks() {
    let rt = fresh();
    rt.lua
        .load(
            r#"
            APP_DONE = false
            smelt.spawn(function()
                smelt.sleep(1000)
                APP_DONE = true
            end)
            smelt.tools.register({
                name = "slow_turn_tool",
                execute = function()
                    smelt.sleep(1000)
                    return "too late"
                end,
            })
            "#,
        )
        .exec()
        .expect("setup scoped tasks");

    let now = Instant::now();
    assert!(rt.drive_tasks(now).is_empty());
    let result = rt.execute_tool(
        "slow_turn_tool",
        &HashMap::new(),
        77,
        "call-turn-cancel",
        ToolEnv {
            mode: protocol::AgentMode::normal(),
            session_id: "sess",
            session_dir: Path::new("/tmp"),
        },
        now,
    );
    assert!(matches!(result, ToolExecResult::Pending));

    rt.cancel_turn_tasks();
    let outs = rt.drive_tasks(now);
    assert!(
        outs.is_empty(),
        "turn cancellation should not surface Lua task output: {outs:?}"
    );
    let app_done: bool = get_global(&rt, "APP_DONE");
    assert!(
        !app_done,
        "app-scoped task should survive turn cancellation"
    );

    let _ = rt.drive_tasks(now + Duration::from_millis(1001));
    let app_done: bool = get_global(&rt, "APP_DONE");
    assert!(
        app_done,
        "surviving app-scoped task should still resume later"
    );
}

#[test]
fn turn_cancellation_cancels_spawned_child_tasks() {
    let rt = fresh();
    rt.lua
        .load(
            r#"
            CHILD_DONE = false
            smelt.tools.register({
                name = "spawns_child_turn_task",
                execute = function()
                    smelt.spawn(function()
                        smelt.sleep(1000)
                        CHILD_DONE = true
                    end)
                    smelt.sleep(1000)
                    return "too late"
                end,
            })
            "#,
        )
        .exec()
        .expect("setup child task tool");

    let now = Instant::now();
    let result = rt.execute_tool(
        "spawns_child_turn_task",
        &HashMap::new(),
        78,
        "call-child-turn-cancel",
        ToolEnv {
            mode: protocol::AgentMode::normal(),
            session_id: "sess",
            session_dir: Path::new("/tmp"),
        },
        now,
    );
    assert!(matches!(result, ToolExecResult::Pending));

    rt.cancel_turn_tasks();
    let outs = rt.drive_tasks(now);
    assert!(
        outs.is_empty(),
        "turn cancellation should not surface Lua task output: {outs:?}"
    );

    let _ = rt.drive_tasks(now + Duration::from_millis(1001));
    let child_done: bool = get_global(&rt, "CHILD_DONE");
    assert!(!child_done, "turn-scoped child task should be cancelled");
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
    assert_eq!(content.to_string_lossy(), "partial");
    let output_id: String = get_global(&rt, "OUTPUT_ID");
    assert_eq!(output_id, "proc_1");
}

// -- fs.complete_path -----------------------------------------------------

#[test]
fn fs_complete_path_filters_sorts_and_limits_entries() {
    let dir = tempfile::tempdir().expect("tmp");
    std::fs::create_dir(dir.path().join("zeta-dir")).unwrap();
    std::fs::create_dir(dir.path().join("alpha-dir")).unwrap();
    std::fs::write(dir.path().join("beta.txt"), "beta").unwrap();
    std::fs::write(dir.path().join("aardvark.txt"), "aardvark").unwrap();
    std::fs::write(dir.path().join(".hidden"), "hidden").unwrap();

    let rt = fresh();
    rt.lua
        .globals()
        .set("DIR", dir.path().to_string_lossy().into_owned())
        .unwrap();
    rt.lua
        .load(
            r#"
            RESULT, ERR = smelt.fs.complete_path(DIR, "", {
                limit = 3,
                insert_prefix = "~/tmp/",
            })
            HIDDEN, HIDDEN_ERR = smelt.fs.complete_path(DIR, ".", { limit = 10 })
            PREFIXED, PREFIXED_ERR = smelt.fs.complete_path(DIR, "aa", { limit = 10 })
            EMPTY_LIMIT, EMPTY_LIMIT_ERR = smelt.fs.complete_path(DIR, "", { limit = 0 })
            DEFAULT_PREFIX, DEFAULT_PREFIX_ERR = smelt.fs.complete_path(DIR, "aa", { limit = 10 })
            "#,
        )
        .exec()
        .expect("complete_path");

    assert!(matches!(
        rt.lua.globals().get::<mlua::Value>("ERR").unwrap(),
        mlua::Value::Nil
    ));

    let labels = lua_item_labels(get_global(&rt, "RESULT"));
    assert_eq!(labels, vec!["alpha-dir/", "zeta-dir/", "aardvark.txt"]);

    let result: mlua::Table = get_global(&rt, "RESULT");
    let items: mlua::Table = result.get("items").unwrap();
    let first: mlua::Table = items.raw_get(1).unwrap();
    assert_eq!(first.get::<String>("kind").unwrap(), "dir");
    assert_eq!(
        first.get::<String>("insert_text").unwrap(),
        "~/tmp/alpha-dir/"
    );

    assert!(matches!(
        rt.lua.globals().get::<mlua::Value>("HIDDEN_ERR").unwrap(),
        mlua::Value::Nil
    ));
    assert_eq!(lua_item_labels(get_global(&rt, "HIDDEN")), vec![".hidden"]);
    assert_eq!(
        lua_item_labels(get_global(&rt, "PREFIXED")),
        vec!["aardvark.txt"]
    );
    assert_eq!(
        lua_item_labels(get_global(&rt, "EMPTY_LIMIT")),
        Vec::<String>::new()
    );

    assert!(matches!(
        rt.lua
            .globals()
            .get::<mlua::Value>("DEFAULT_PREFIX_ERR")
            .unwrap(),
        mlua::Value::Nil
    ));
    let default_result: mlua::Table = get_global(&rt, "DEFAULT_PREFIX");
    let default_items: mlua::Table = default_result.get("items").unwrap();
    let default_item: mlua::Table = default_items.raw_get(1).unwrap();
    assert_eq!(
        default_item.get::<String>("insert_text").unwrap(),
        dir.path().join("aardvark.txt").to_string_lossy()
    );
}

fn lua_item_labels(result: mlua::Table) -> Vec<String> {
    let items: mlua::Table = result.get("items").unwrap();
    items
        .sequence_values::<mlua::Table>()
        .map(|item| item.unwrap().get::<String>("label").unwrap())
        .collect()
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
            local a = smelt.state.get("plugin_x")
            a.counter = 1
            local b = smelt.state.get("plugin_x")
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
