#![no_main]

//! Focused transcript/render shell target. It drives transcript-producing engine
//! events plus resize/scroll/render without prompt/Lua noise, so parser and
//! projection bugs get denser coverage than in the broad `smelt_loop` target.

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use protocol::{Content, EngineEvent, HistoryItem};
use smelt_fuzz::TestApp;
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
    ToolStart { id: u8, name: String },
    ToolOutput { id: u8, text: String },
    ToolFinish { id: u8, is_error: bool, text: String },
    ProcessCompleted { id: String, code: Option<i32> },
    Complete { count: u8 },
    Error(String),
    Resize { w: u8, h: u8 },
    Tick(u16),
    Render,
}

fn run(input: Input) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build runtime");
    let _guard = runtime.enter();
    let mut app = TestApp::builder().build();

    for op in input.ops.into_iter().take(96) {
        match op {
            Op::StartTurn(id) => app.start_turn(u64::from(id)),
            Op::Text(content) => app.feed_one(SourceEvent::Engine(EngineEvent::Text { content })),
            Op::TextDelta(delta) => app.feed_one(SourceEvent::Engine(EngineEvent::TextDelta { delta })),
            Op::Thinking(content) => app.feed_one(SourceEvent::Engine(EngineEvent::Thinking { content })),
            Op::ThinkingDelta(delta) => app.feed_one(SourceEvent::Engine(EngineEvent::ThinkingDelta { delta })),
            Op::ToolStart { id, name } => app.feed_one(SourceEvent::Engine(EngineEvent::ToolStarted {
                call_id: call_id(id),
                tool_name: name,
                args: std::collections::HashMap::new(),
            })),
            Op::ToolOutput { id, text } => app.feed_one(SourceEvent::Engine(EngineEvent::ToolOutput {
                call_id: call_id(id),
                chunk: text,
            })),
            Op::ToolFinish { id, is_error, text } => app.feed_one(SourceEvent::Engine(EngineEvent::ToolFinished {
                call_id: call_id(id),
                result: protocol::ToolOutcome { content: text, is_error, metadata: None },
                elapsed_ms: None,
            })),
            Op::ProcessCompleted { id, code } => app.feed_one(SourceEvent::Engine(EngineEvent::ProcessCompleted { id, exit_code: code })),
            Op::Complete { count } => {
                let turn_id = app.current_turn_id().unwrap_or(0);
                app.feed_one(SourceEvent::Engine(EngineEvent::TurnComplete {
                    turn_id,
                    history: history(count),
                    meta: None,
                }));
            }
            Op::Error(message) => {
                app.feed_one(SourceEvent::Engine(EngineEvent::TurnError { message }));
            }
            Op::Resize { w, h } => app.feed_one(SourceEvent::Resize {
                width: u16::from(w % 120).max(1),
                height: u16::from(h % 40).max(1),
            }),
            Op::Tick(ms) => app.feed_one(SourceEvent::Tick(u64::from(ms))),
            Op::Render => {}
        }
        app.render_silent();
        app.assert_invariants();
    }
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
