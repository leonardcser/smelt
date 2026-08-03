//! `cargo xtask synth` - generate a synthetic session for perf testing.
//!
//! Writes a session with `--turns` (user, assistant) message pairs to
//! `<state>/sessions/<id>/`, then prints the new session id. Resume it via
//! `smelt -r <id>` to inspect scrolling, layout, and theme performance against
//! a long transcript without a live LLM.

use std::collections::HashMap;
use std::error::Error;
use std::time::Duration;

use protocol::{Content, HistoryItem, HistoryNote};
use smelt_core::session::{self, Session};
use smelt_core::{Block, BlockOrigin, ToolOutput, ToolState, ToolStatus, TranscriptBlockRecord};

pub fn run() {
    let mut turns: usize = 5000;
    let mut words: usize = 60;
    let mut title: Option<String> = None;

    let mut args = std::env::args().skip(2);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--turns" => {
                turns = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .filter(|value| *value > 0)
                    .unwrap_or_else(|| usage_exit("--turns expects a positive integer"));
            }
            "--words" => {
                words = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .filter(|value| *value > 0)
                    .unwrap_or_else(|| usage_exit("--words expects a positive integer"));
            }
            "--title" => {
                title = Some(
                    args.next()
                        .unwrap_or_else(|| usage_exit("--title expects a value")),
                );
            }
            "--help" | "-h" => {
                print_usage();
                std::process::exit(0);
            }
            other => usage_exit(&format!("unknown argument `{other}`")),
        }
    }

    if let Err(error) = generate(turns, words, title) {
        eprintln!("xtask synth: {error}");
        std::process::exit(1);
    }
}

fn usage_exit(msg: &str) -> ! {
    eprintln!("xtask synth: {msg}");
    print_usage();
    std::process::exit(2);
}

fn print_usage() {
    eprintln!("usage: cargo xtask synth [--turns N] [--words N] [--title STR]");
    eprintln!();
    eprintln!("  --turns N    user/assistant turn pairs to generate (default 5000)");
    eprintln!("  --words N    words per assistant message body (default 60)");
    eprintln!("  --title STR  optional session title");
}

fn generate(turns: usize, words: usize, title: Option<String>) -> Result<(), Box<dyn Error>> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let mut session = Session::new(std::process::id(), cwd);
    let stamp = session.id.clone();
    session.title =
        Some(title.unwrap_or_else(|| format!("synth fixture · {turns} turns × {words} words")));
    session.first_user_message = Some("synth turn 1 - describe topic 1".to_string());
    session.slug = Some("synth".into());
    session.model = Some("synth/local".into());

    for i in 1..=turns {
        let user_text = format!("synth turn {i} - describe topic {i}");
        session
            .history
            .push(protocol::HistoryItem::user(Content::text(user_text)));

        let body = assistant_body(i, words);
        session.history.push(protocol::HistoryItem::Assistant(
            protocol::AssistantStep::terminal(Some(Content::text(body)), None, Vec::new()),
        ));
    }

    session.updated_at_ms = session::now_ms();

    let transcript_rows = save_session_with_transcript_records(&session)?;
    println!("{}", stamp);
    eprintln!(
        "synth: wrote {turns} turns ({} history items, {transcript_rows} transcript records) → {}",
        session.history.len(),
        session::dir_for(&session).display()
    );
    eprintln!("resume with: smelt -r {stamp}");
    Ok(())
}

fn save_session_with_transcript_records(session: &Session) -> Result<usize, Box<dyn Error>> {
    let receipt = session::save_result(session)?;
    let session_dir = session::dir_for(session);
    let root = session_dir
        .parent()
        .ok_or("synthetic session directory has no storage root")?;
    let reader = smelt_store::LineageSessionReader::open_existing(root, &session.id)?;
    let state = reader.snapshot()?;
    if state.head != receipt.current {
        return Err(format!(
            "synthetic session head changed while saving: expected {:?}, found {:?}",
            receipt.current, state.head
        )
        .into());
    }
    drop(reader);

    let records = transcript_records_for_history(&session.history)?;
    let record_count = records.len();
    if records.is_empty() {
        return Ok(0);
    }

    let mut writer = smelt_store::OwnedLineageWriter::open_existing(root, &session.id)?;
    let command = smelt_store::SessionCommit {
        session_id: session.id.clone(),
        expected: state.head,
        identity: state.identity,
        metadata: state.metadata,
        history: smelt_store::HistorySuffix {
            start: smelt_store::HistoryIndex::new(state.head.history_len.get()),
            final_len: state.head.history_len,
            items: Vec::new(),
        },
        side_tables: smelt_store::SideTableSuffixes {
            start: smelt_store::HistoryIndex::new(state.head.history_len.get()),
            ..Default::default()
        },
        transcript_records: Some(smelt_store::TranscriptRecordSuffix {
            start: smelt_store::TranscriptRecordIndex::ZERO,
            records,
        }),
    };
    let receipt = writer
        .commit_session(&command)
        .map_err(|failure| format!("synthetic transcript record commit failed: {failure:?}"))?;
    writer.release()?;
    session::publish_session_catalog_commit(&command, &receipt, true);
    Ok(record_count)
}

fn transcript_records_for_history(
    history: &[HistoryItem],
) -> smelt_store::Result<Vec<smelt_store::StoredTranscriptBlock>> {
    let mut records = Vec::new();
    for (history_idx, item) in history.iter().enumerate() {
        match item {
            HistoryItem::System { .. } | HistoryItem::Note(HistoryNote::Context { .. }) => {}
            HistoryItem::User {
                content,
                display,
                command,
            } => {
                let text = display
                    .clone()
                    .unwrap_or_else(|| content.text_content().into_owned());
                if !text.trim().is_empty() || !content.image_labels().is_empty() {
                    push_transcript_record(
                        &mut records,
                        history_idx,
                        Block::User {
                            text,
                            image_labels: content.image_labels(),
                            command: *command,
                        },
                        None,
                    )?;
                }
            }
            HistoryItem::Assistant(step) => {
                if let Some(reasoning) = step
                    .reasoning
                    .as_ref()
                    .filter(|text| !text.trim().is_empty())
                {
                    push_transcript_record(
                        &mut records,
                        history_idx,
                        Block::Thinking {
                            title: None,
                            summary_titles: Vec::new(),
                            content: reasoning.clone(),
                            kind: protocol::ReasoningKind::default(),
                        },
                        None,
                    )?;
                }
                if let Some(content) = &step.content {
                    let text = content.text_content();
                    if !text.trim().is_empty() {
                        push_transcript_record(
                            &mut records,
                            history_idx,
                            Block::Text {
                                content: text.into_owned(),
                            },
                            None,
                        )?;
                    }
                }
                for invocation in &step.invocations {
                    let is_error = invocation.result.is_error;
                    push_transcript_record(
                        &mut records,
                        history_idx,
                        Block::ToolCall {
                            call_id: invocation.call_id.clone(),
                            name: invocation.name.clone(),
                            summary: protocol::StyledLines::from_plain(invocation.name.clone()),
                            args: tool_arguments(&invocation.arguments),
                        },
                        Some(ToolState {
                            status: if is_error {
                                ToolStatus::Err
                            } else {
                                ToolStatus::Ok
                            },
                            elapsed: invocation.elapsed_ms.map(Duration::from_millis),
                            called_at_ms: invocation.called_at_ms,
                            elapsed_active: false,
                            output: Some(Box::new(ToolOutput {
                                content: invocation.result.content.clone(),
                                is_error,
                                metadata: invocation.result.metadata.clone(),
                            })),
                            user_message: None,
                            preview_output: None,
                        }),
                    )?;
                }
            }
            HistoryItem::Note(HistoryNote::ModeChange { text, mode, .. }) => {
                push_transcript_record(
                    &mut records,
                    history_idx,
                    Block::Mode {
                        text: text.clone(),
                        icon: mode.as_deref().unwrap_or("mode").to_string(),
                        hl_group: "SmeltAccent".into(),
                    },
                    None,
                )?;
            }
            HistoryItem::Note(HistoryNote::ProcessStatus { text, event }) => {
                push_transcript_record(
                    &mut records,
                    history_idx,
                    Block::ProcessStatus {
                        text: text.clone(),
                        event: event.clone(),
                    },
                    None,
                )?;
            }
        }
    }
    Ok(records)
}

fn push_transcript_record(
    records: &mut Vec<smelt_store::StoredTranscriptBlock>,
    history_idx: usize,
    block: Block,
    tool_state: Option<ToolState>,
) -> smelt_store::Result<()> {
    let content_hash = block.content_hash();
    let record = TranscriptBlockRecord {
        block,
        content_hash,
        origin: Some(BlockOrigin::History(history_idx)),
        tool_state,
    };
    let record_idx = records.len();
    records.push(
        smelt_core::transcript_model::transcript_block_row_with_block_idx(
            record_idx,
            record_idx as u64,
            &record,
        )?,
    );
    Ok(())
}

fn tool_arguments(arguments: &str) -> HashMap<String, serde_json::Value> {
    serde_json::from_str::<serde_json::Value>(arguments)
        .ok()
        .and_then(|value| match value {
            serde_json::Value::Object(fields) => Some(fields.into_iter().collect()),
            _ => None,
        })
        .unwrap_or_default()
}

fn assistant_body(turn: usize, words: usize) -> String {
    const LOREM: &[&str] = &[
        "the",
        "buffer",
        "extmark",
        "namespace",
        "renders",
        "incrementally",
        "across",
        "wrapped",
        "rows",
        "while",
        "the",
        "compositor",
        "diffs",
        "every",
        "frame",
        "into",
        "a",
        "minimal",
        "SGR",
        "stream",
        "that",
        "the",
        "terminal",
        "consumes",
        "without",
        "flicker",
        "or",
        "tearing",
        "regardless",
        "of",
        "throughput",
    ];
    let pick = |i: usize| LOREM[i % LOREM.len()];

    let prose: String = (0..words)
        .map(|i| pick(turn.wrapping_mul(7).wrapping_add(i)))
        .collect::<Vec<_>>()
        .join(" ");

    match turn % 4 {
        0 => format!(
            "## Reply {turn}\n\n{prose}.\n\n```rust\nfn synth_{turn}() -> usize {{\n    {turn} * 2 + {turn}\n}}\n```\n"
        ),
        1 => {
            let bullets: Vec<String> = (1..=5)
                .map(|j| format!("- point {j} for turn {turn}: {}", pick(turn + j)))
                .collect();
            format!("Reply {turn}.\n\n{prose}\n\n{}\n", bullets.join("\n"))
        }
        2 => format!("Reply {turn}.\n\n{prose}.\n\n{prose}.\n"),
        _ => format!("Reply {turn}.\n\n> {prose}\n\nSee `synth_{turn}()` for details.\n"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transcript_projection_skips_invisible_items_and_preserves_origins() {
        let history = vec![
            HistoryItem::system("hidden system"),
            HistoryItem::user(Content::text("visible user")),
            HistoryItem::note(HistoryNote::context("hidden context")),
            HistoryItem::Assistant(protocol::AssistantStep::terminal(
                Some(Content::text("visible assistant")),
                Some("visible reasoning".into()),
                Vec::new(),
            )),
            HistoryItem::note(HistoryNote::process_status("visible process")),
        ];

        let records = transcript_records_for_history(&history).unwrap();
        assert_eq!(records.len(), 4);
        assert_eq!(
            records
                .iter()
                .map(|record| record.history_idx)
                .collect::<Vec<_>>(),
            vec![Some(1), Some(3), Some(3), Some(4)]
        );
        assert!(records[0].indexed_text.contains("visible user"));
        assert!(records[1].indexed_text.contains("visible reasoning"));
        assert!(records[2].indexed_text.contains("visible assistant"));
        assert!(records[3].indexed_text.contains("visible process"));
    }

    #[test]
    fn transcript_projection_indexes_tool_invocation_outputs() {
        let history = vec![HistoryItem::Assistant(
            protocol::AssistantStep::with_invocations(
                Some(Content::text("tool preface")),
                None,
                Vec::new(),
                vec![protocol::ToolInvocation {
                    call_id: "call-1".into(),
                    name: "demo_tool".into(),
                    arguments: r#"{"path":"src/main.rs"}"#.into(),
                    result: protocol::ToolOutcome {
                        content: "searchable tool output".into(),
                        is_error: false,
                        metadata: Some(serde_json::json!({"note": "metadata"})),
                    },
                    elapsed_ms: Some(42),
                    called_at_ms: Some(1234),
                }],
            ),
        )];

        let records = transcript_records_for_history(&history).unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[1].kind, "tool");
        assert!(records[1].indexed_text.contains("searchable tool output"));
        assert!(records[1]
            .tool_state_json
            .as_deref()
            .unwrap()
            .contains("1234"));
    }
}
