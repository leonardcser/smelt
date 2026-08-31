#![no_main]

//! Focused transcript/render shell target. It drives transcript-producing engine
//! events plus resize/scroll/render without prompt/Lua noise, so parser and
//! projection bugs get denser coverage than in the broad `smelt_loop` target.

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use protocol::{CanonicalHistoryDelta, Content, EngineEvent, HistoryItem, ReasoningKind};
use smelt_fuzz::{runtime::with_current_thread_runtime, TestApp};
use tui::app::test_harness::SourceEvent;

#[derive(Arbitrary, Debug)]
struct Input {
    ops: Vec<Op>,
}

#[derive(Arbitrary, Debug)]
enum Op {
    StartTurn(u8),
    Text(String),
    TextDelta(String),
    Thinking(String),
    ThinkingDelta(String),
    ToolStart {
        id: u8,
        name: String,
    },
    ToolOutput {
        id: u8,
        text: String,
    },
    ToolFinish {
        id: u8,
        is_error: bool,
        text: String,
    },
    KnownToolRoundTrip {
        id: u8,
        kind: u8,
        is_error: bool,
    },
    Complete {
        count: u8,
    },
    Error(String),
    Resize {
        w: u8,
        h: u8,
    },
    Tick(u16),
    TranscriptGroups(u8),
    TranscriptProbe {
        kind: u8,
        row: u16,
        count: u8,
    },
    Render,
}

fn run(input: Input) {
    with_current_thread_runtime("transcript_render", || run_with_app(input));
}

fn run_with_app(input: Input) {
    let mut app = TestApp::builder().build();

    for op in input.ops.into_iter().take(96) {
        match op {
            Op::StartTurn(id) => app.start_turn(u64::from(id)),
            Op::Text(content) => app.feed_one(SourceEvent::engine(EngineEvent::Text { content })),
            Op::TextDelta(delta) => {
                app.feed_one(SourceEvent::engine(EngineEvent::TextDelta { delta }))
            }
            Op::Thinking(content) => app.feed_one(SourceEvent::engine(EngineEvent::Reasoning {
                kind: ReasoningKind::Raw,
                title: None,
                content,
            })),
            Op::ThinkingDelta(delta) => {
                app.feed_one(SourceEvent::engine(EngineEvent::ReasoningPartDelta {
                    id: "fuzz:raw:0".into(),
                    kind: ReasoningKind::Raw,
                    delta,
                    title: None,
                }))
            }
            Op::ToolStart { id, name } => {
                app.feed_one(SourceEvent::engine(EngineEvent::ToolStarted {
                    invocation_id: protocol::InvocationId::new(u64::from(id)),
                    call_id: call_id(id),
                    tool_name: name,
                    args: std::collections::HashMap::new(),
                    called_at_ms: u64::from(id),
                }))
            }
            Op::ToolOutput { id, text } => {
                app.feed_one(SourceEvent::engine(EngineEvent::ToolOutput {
                    invocation_id: protocol::InvocationId::new(u64::from(id)),
                    call_id: call_id(id),
                    line: text,
                }))
            }
            Op::ToolFinish { id, is_error, text } => {
                app.feed_one(SourceEvent::engine(EngineEvent::ToolFinished {
                    invocation_id: protocol::InvocationId::new(u64::from(id)),
                    call_id: call_id(id),
                    result: protocol::ToolOutcome::new(text, is_error, None),
                    elapsed_ms: None,
                }))
            }
            Op::KnownToolRoundTrip { id, kind, is_error } => {
                known_tool_round_trip(&mut app, id, kind, is_error)
            }
            Op::Complete { count } => {
                let turn_id = app.current_turn_id().unwrap_or(0);
                app.feed_one(SourceEvent::engine(EngineEvent::TurnComplete {
                    turn_id,
                    history: Some(CanonicalHistoryDelta::new(0, history(count))),
                    meta: None,
                }));
            }
            Op::Error(message) => {
                app.feed_one(SourceEvent::engine(EngineEvent::TurnError {
                    message,
                    kind: None,
                    retry_at_ms: None,
                }));
            }
            Op::Resize { w, h } => app.feed_one(SourceEvent::Resize {
                width: u16::from(w % 120).max(1),
                height: u16::from(h % 40).max(1),
            }),
            Op::Tick(ms) => app.feed_one(SourceEvent::Tick(u64::from(ms))),
            Op::TranscriptGroups(kind) => register_transcript_group(&mut app, kind),
            Op::TranscriptProbe { kind, row, count } => {
                transcript_probe(&mut app, kind, row, count)
            }
            Op::Render => {}
        }
        app.render_silent();
        app.assert_invariants();
    }
}

fn known_tool_round_trip(app: &mut TestApp, id: u8, kind: u8, is_error: bool) {
    let invocation_id = protocol::InvocationId::new(u64::from(id));
    let call_id = call_id(id);
    let (tool_name, args, content, metadata) = known_tool_payload(kind);
    if app.current_turn_id().is_none() {
        app.start_turn(u64::from(id));
    }
    app.feed_one(SourceEvent::engine(EngineEvent::ToolStarted {
        invocation_id,
        call_id: call_id.clone(),
        tool_name: tool_name.to_string(),
        args,
        called_at_ms: u64::from(id),
    }));
    for line in content.lines() {
        app.feed_one(SourceEvent::engine(EngineEvent::ToolOutput {
            invocation_id,
            call_id: call_id.clone(),
            line: line.to_string(),
        }));
    }
    app.feed_one(SourceEvent::engine(EngineEvent::ToolFinished {
        invocation_id,
        call_id,
        result: protocol::ToolOutcome::new(content, is_error, metadata),
        elapsed_ms: Some(123),
    }));
}

fn known_tool_payload(
    kind: u8,
) -> (
    &'static str,
    std::collections::HashMap<String, serde_json::Value>,
    String,
    Option<serde_json::Value>,
) {
    let mut args = std::collections::HashMap::new();
    match kind % 6 {
        0 => {
            args.insert(
                "command".into(),
                serde_json::json!("printf 'one\\ntwo' && echo done"),
            );
            ("bash", args, "one\ntwo\ndone\n".into(), None)
        }
        1 => {
            args.insert("pattern".into(), serde_json::json!("needle"));
            args.insert("path".into(), serde_json::json!("src"));
            args.insert("output_mode".into(), serde_json::json!("content"));
            (
                "grep",
                args,
                "src/lib.rs:1:needle\nsrc/main.rs:2:needle\n".into(),
                Some(serde_json::json!({ "display_count": { "value": 2, "unit": "line" } })),
            )
        }
        2 => {
            args.insert("pattern".into(), serde_json::json!("**/*.missing"));
            (
                "glob",
                args,
                "no matches found".into(),
                Some(serde_json::json!({ "display_count": { "value": 0, "unit": "file" } })),
            )
        }
        3 => {
            args.insert("file_path".into(), serde_json::json!("/tmp/fuzz.rs"));
            args.insert("offset".into(), serde_json::json!(3));
            args.insert("limit".into(), serde_json::json!(2));
            (
                "read_file",
                args,
                "   3\tfn main() {\n   4\t    println!(\"/tmp/fuzz.rs\");\n".into(),
                None,
            )
        }
        4 => {
            args.insert("file_path".into(), serde_json::json!("/tmp/fuzz.rs"));
            args.insert(
                "old_string".into(),
                serde_json::json!("fn old() {\n    1\n}\n"),
            );
            args.insert(
                "new_string".into(),
                serde_json::json!("fn new() {\n    2\n}\n"),
            );
            args.insert("replace_all".into(), serde_json::json!(false));
            (
                "edit_file",
                args,
                "edited /tmp/fuzz.rs".into(),
                Some(serde_json::json!({
                    "path": "/tmp/fuzz.rs",
                    "old_content": "fn old() {\n    1\n}\n",
                    "new_content": "fn new() {\n    2\n}\n"
                })),
            )
        }
        _ => {
            args.insert("file_path".into(), serde_json::json!("/tmp/fuzz.txt"));
            args.insert("content".into(), serde_json::json!("alpha\nbeta\n"));
            ("write_file", args, "wrote /tmp/fuzz.txt".into(), None)
        }
    }
}

fn register_transcript_group(app: &mut TestApp, kind: u8) {
    const SNIPPETS: &[&str] = &[
        r#"
          smelt.transcript.groups.register({
            name = "fuzz_tool_batch",
            cache_key = "v1",
            selector = { kind = "tool" },
            bucket = { "name" },
            min = 2,
            default_view = "collapsed",
          })
          smelt.transcript.extend_renderer("fuzz_tool_batch_renderer", function(next, node, ctx)
            if node.kind ~= "group" or node.name ~= "fuzz_tool_batch" then
              return next(node, ctx)
            end
            if node.view_state == "expanded" then
              return smelt.layout.group_children()
            end
            return smelt.layout.text("fuzz tools " .. tostring(node.child_count))
          end, { cache_key = "v1" })
        "#,
        r#"
          smelt.transcript.groups.register({
            name = "fuzz_process_batch",
            cache_key = "v1",
            selector = { kind = "process_status", fields = { event = "background_process_completed" } },
            bucket = { "process_id", "exit_code" },
            min = 2,
            default_view = "collapsed",
          })
          smelt.transcript.extend_renderer("fuzz_process_batch_renderer", function(next, node, ctx)
            if node.kind == "group" and node.name == "fuzz_process_batch" then
              return smelt.layout.text("fuzz process " .. tostring(node.bucket) .. " x" .. tostring(node.child_count))
            end
            return next(node, ctx)
          end, { cache_key = "v1" })
        "#,
        r#"
          smelt.transcript.extend_renderer("fuzz_wrapper", function(next, block, ctx)
            local layout = next(block, ctx)
            if block.kind == "thinking" and ctx.view_state == "collapsed" then
              return smelt.layout.text("fuzz thinking")
            end
            return layout
          end, { cache_key = "v1" })
        "#,
        r#"
          smelt.settings.transcript = {
            view = {
              blocks = { thinking = "collapsed" },
              tools = { read_file = "collapsed", grep = "collapsed", bash = "expanded" },
              groups = { fuzz_tool_batch = "expanded", fuzz_process_batch = "collapsed" },
            },
            limits = { tool_rows = 12, collapsed_error_rows = 3 },
          }
          smelt.transcript.invalidate_renderer()
        "#,
    ];
    let snippet = SNIPPETS[(kind as usize) % SNIPPETS.len()];
    app.run_lua_result(snippet)
        .unwrap_or_else(|error| panic!("transcript registration failed: {error}"));
}

fn transcript_probe(app: &mut TestApp, kind: u8, row: u16, count: u8) {
    let row = row % 256;
    let count = count % 32;
    let snippet = match kind % 10 {
        0 => format!(
            r#"
            local rows = smelt.transcript.rows({row}, {count})
            assert(type(rows) == "table", "rows() did not return a table")
            assert(#rows <= {count}, "rows() returned more rows than requested")
            for _, line in ipairs(rows) do
              assert(type(line) == "string", "rows() returned a non-string row")
            end
            "#
        ),
        1 => format!(
            r#"
            local node = smelt.transcript.node_at_row({row})
            if node ~= nil then
              assert(type(node) == "table", "node_at_row() returned a non-table node")
              assert(type(node.first_row) == "number", "node is missing first_row")
              assert(type(node.rows) == "number", "node is missing rows")
              assert(node.rows >= 0, "node has negative rows")
              assert(node.first_row <= {row} and {row} < node.first_row + math.max(node.rows, 1), "node does not contain requested row")
            end
            "#
        ),
        2 => format!(
            r#"
            local block = smelt.transcript.loaded_block_at_row({row})
            if block ~= nil then
              assert(type(block) == "table", "loaded_block_at_row() returned a non-table block")
              assert(type(block.first_row) == "number", "block is missing first_row")
              assert(type(block.rows) == "number", "block is missing rows")
              assert(block.rows >= 0, "block has negative rows")
              assert(block.first_row <= {row} and {row} < block.first_row + math.max(block.rows, 1), "block does not contain requested row")
            end
            "#
        ),
        3 => block_snapshot_probe("loaded_blocks_expensive"),
        4 => block_snapshot_probe("visible_blocks"),
        5 => "assert(type(smelt.transcript.loaded_text_expensive()) == \"string\")".to_string(),
        6 => format!("assert(type(smelt.transcript.fold_at_row({row}, 'toggle')) == \"boolean\")"),
        7 => "assert(type(smelt.transcript.fold_kind('thinking', 'toggle')) == \"boolean\")"
            .to_string(),
        8 => "smelt.transcript.fold_all('open')".to_string(),
        _ => format!("smelt.win.transcript():reveal({row}, {{ top_padding = 1 }})"),
    };
    app.run_lua_result(&snippet)
        .unwrap_or_else(|error| panic!("transcript probe failed: {error}\n{snippet}"));
}

fn block_snapshot_probe(name: &str) -> String {
    format!(
        r#"
        local blocks = smelt.transcript.{name}()
        assert(type(blocks) == "table", "{name}() did not return a table")
        local prev_first_row = -1
        for _, block in ipairs(blocks) do
          assert(type(block) == "table", "{name}() returned a non-table block")
          assert(type(block.first_row) == "number", "block is missing first_row")
          assert(type(block.rows) == "number", "block is missing rows")
          assert(block.first_row >= prev_first_row, "blocks are not ordered by first_row")
          assert(block.rows >= 0, "block has negative rows")
          prev_first_row = block.first_row
        end
        "#
    )
}

fn call_id(id: u8) -> String {
    format!("call-{}", id % 8)
}

fn history(count: u8) -> Vec<HistoryItem> {
    (0..usize::from(count.min(8)))
        .map(|i| {
            if i % 2 == 0 {
                HistoryItem::user(Content::text(format!("user {i}")))
            } else {
                HistoryItem::Assistant(protocol::AssistantStep::terminal(
                    Some(Content::text(format!("assistant {i}"))),
                    None,
                    Vec::new(),
                ))
            }
        })
        .collect()
}

fuzz_target!(|input: Input| run(input));
