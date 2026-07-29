use super::*;
use protocol::{
    AssistantStep, Content, EngineEvent, HistoryAppend, HistoryAppendResult, HistoryItem, Role,
    TokenUsage, ToolInvocation, ToolOutcome,
};
use smelt_core::transcript_model::Block;
use std::collections::HashMap;

fn loaded_session(app: &TestApp, id: &str) -> smelt_core::session::Session {
    crate::app::history::materialize_full_session(
        &app.core_probe().sessions,
        id,
        crate::app::history::FullSessionMaterializationReason::TestSavedSessionAssertion,
    )
    .expect("session saved")
}

fn session_revision(app: &TestApp, id: &str) -> u64 {
    smelt_store::SessionReader::open_existing(app.core_probe().sessions.dir_for_id(id))
        .unwrap()
        .store_head()
        .unwrap()
        .revision
        .get()
}

fn has_sticky_session_save_failure(app: &TestApp, session_id: &str) -> bool {
    app.overlays_probe()
        .notification()
        .is_some_and(|notification| {
            notification.lifetime.is_sticky()
                && matches!(
                    notification.owner.as_ref(),
                    Some(crate::app::NotificationOwner::SessionPersistence(owner_session_id))
                        if owner_session_id == session_id
                )
        })
}

fn legacy_preserve_order_hash<T: serde::Serialize>(value: &T) -> u64 {
    let value = serde_json::to_value(value).unwrap();
    let bytes = serde_json::to_vec(&value).unwrap();
    seahash::hash(&bytes)
}

fn rewrite_tool_record_hashes_as_legacy(session_id: &str) -> usize {
    let db_path = smelt_core::session::dir_for_id(session_id).join("session.db");
    let mut db = smelt_store::SessionDb::open(db_path).unwrap();
    let mut records = db.read_all_transcript_records().unwrap();
    let mut rewritten = 0;

    for record in &mut records {
        if record.kind != "tool" {
            continue;
        }
        let persisted_json: serde_json::Value = serde_json::from_str(&record.block_json).unwrap();
        let block: Block = serde_json::from_value(persisted_json.clone()).unwrap();
        let legacy_hash = legacy_preserve_order_hash(&persisted_json);
        assert_eq!(legacy_hash, seahash::hash(record.block_json.as_bytes()));
        assert_ne!(legacy_hash, block.content_hash());
        record.content_hash = legacy_hash.to_string();
        rewritten += 1;
    }

    db.apply_transcript_record_fixture(&records).unwrap();
    rewritten
}

fn retry_persistence_via_lua(app: &mut TestApp) -> bool {
    app.eval_lua("return smelt.session.retry_persistence()")
        .unwrap()
}

fn request_audit_entry(request_id: u64) -> protocol::request_log::RequestLogEntry {
    protocol::request_log::RequestLogEntry {
        request_id,
        kind: "turn".into(),
        turn_id: Some(request_id),
        ask_id: None,
        history_len: Some(1),
        timestamp_ms: 1,
        provider_kind: "openai".into(),
        api_base: "https://api.example.test".into(),
        model: "model-a".into(),
        url: "https://api.example.test/v1/chat/completions".into(),
        http_status: Some(200),
        body: serde_json::json!({"model": "model-a"}),
        prompt_cache_key: None,
        stream: true,
        system_prompt: None,
        messages: None,
        tools: None,
        response: None,
        usage: None,
        cost_usd: None,
        tokens_per_sec: None,
        elapsed_ms: Some(10),
        attempt: 1,
        error: None,
        background: false,
    }
}

fn tool_history() -> Vec<HistoryItem> {
    vec![
        HistoryItem::user(Content::text("run the tool")),
        HistoryItem::Assistant(AssistantStep::with_invocations(
            Some(Content::text("tool completed")),
            None,
            Vec::new(),
            vec![ToolInvocation {
                call_id: "call-1".into(),
                name: "bash".into(),
                arguments: r#"{"command":"echo persisted"}"#.into(),
                result: ToolOutcome {
                    content: "persisted\n".into(),
                    is_error: false,
                    metadata: None,
                },
                elapsed_ms: Some(12),
            }],
        )),
    ]
}

fn assert_committed_tool_invocation(history: &[HistoryItem]) {
    let invocation = history
        .iter()
        .find_map(|item| match item {
            HistoryItem::Assistant(step) => step.invocations.first(),
            _ => None,
        })
        .expect("committed tool invocation persisted");

    assert_eq!(invocation.call_id, "call-1");
    assert_eq!(invocation.name, "bash");
    assert_eq!(invocation.arguments, r#"{"command":"echo persisted"}"#);
    assert_eq!(invocation.result.content, "persisted\n");
    assert!(!invocation.result.is_error);
    assert_eq!(invocation.elapsed_ms, Some(12));
}

fn assert_model_history_tool_messages(messages: &[protocol::Message]) {
    let assistant = messages
        .iter()
        .find(|message| message.role == Role::Assistant && message.tool_calls.is_some())
        .expect("assistant tool call message restored");
    let tool_calls = assistant.tool_calls.as_ref().unwrap();
    assert_eq!(tool_calls.len(), 1);
    assert_eq!(tool_calls[0].id, "call-1");
    assert_eq!(tool_calls[0].function.name, "bash");
    assert_eq!(
        tool_calls[0].function.arguments,
        r#"{"command":"echo persisted"}"#
    );

    let tool = messages
        .iter()
        .find(|message| {
            message.role == Role::Tool && message.tool_call_id.as_deref() == Some("call-1")
        })
        .expect("matching tool result message restored");
    assert_eq!(tool.content.as_ref().unwrap().text_content(), "persisted\n");
    assert!(!tool.is_error);
}

fn saved_one_row_session(guard: &smelt_test_support::ProcessEnvironmentGuard) -> String {
    let mut app = TestApp::builder().build_with_test_home_guard(guard);
    app.session_append_history(HistoryItem::user(Content::text("persisted before resume")));
    app.save_session_and_flush();
    app.session_snapshot().id.clone()
}

async fn next_app_event(app: &mut TestApp) -> crate::app::AppEvent {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        if let Some(event) = app.try_recv_app_event() {
            return event;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for app event"
        );
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    }
}

#[test]
fn explicit_resume_migrates_old_supported_schema_before_bounded_load() {
    let guard = test_home_guard();
    let session_id = saved_one_row_session(&guard);
    let session_dir = smelt_core::session::dir_for_id(&session_id);
    let db_path = session_dir.join("session.db");
    let db = smelt_store::SessionDb::open(&db_path).unwrap();
    db.connection()
        .execute_batch("PRAGMA user_version = 9")
        .unwrap();
    drop(db);

    let mut resumed = TestApp::builder().build_without_test_home_reset(&guard);
    let sessions_root = session_dir.parent().unwrap();
    assert_eq!(
        smelt_store::session_schema_status(sessions_root, &session_id).unwrap(),
        smelt_store::SessionSchemaStatus::Upgradeable {
            found: 9,
            target: smelt_store::SCHEMA_VERSION,
        }
    );

    resumed.load_session_by_id(&session_id);

    assert_eq!(resumed.session_snapshot().id, session_id);
    assert_eq!(resumed.app.session_history_len(), 1);
    assert_eq!(
        smelt_store::session_schema_status(sessions_root, &session_id).unwrap(),
        smelt_store::SessionSchemaStatus::Current {
            version: smelt_store::SCHEMA_VERSION,
        }
    );
    assert_eq!(
        loaded_session(&resumed, &session_id).history,
        vec![HistoryItem::user(Content::text("persisted before resume"))]
    );
}

#[test]
fn explicit_resume_rejects_future_schema_without_modifying_or_loading_it() {
    let guard = test_home_guard();
    let session_id = saved_one_row_session(&guard);
    let session_dir = smelt_core::session::dir_for_id(&session_id);
    let future_version = smelt_store::SCHEMA_VERSION + 1;
    let db = smelt_store::SessionDb::open(session_dir.join("session.db")).unwrap();
    db.connection()
        .pragma_update(None, "user_version", future_version)
        .unwrap();
    drop(db);

    let mut resumed = TestApp::builder().build_without_test_home_reset(&guard);
    let initial_session_id = resumed.session_snapshot().id.clone();
    resumed.load_session_by_id(&session_id);

    assert_eq!(resumed.session_snapshot().id, initial_session_id);
    assert_eq!(
        smelt_store::session_schema_status(session_dir.parent().unwrap(), &session_id).unwrap(),
        smelt_store::SessionSchemaStatus::Future {
            found: future_version,
            supported: smelt_store::SCHEMA_VERSION,
        }
    );
    let notification = resumed
        .overlays_probe()
        .notification()
        .expect("future schema error notification");
    assert!(notification.lifetime.is_sticky());
    assert!(notification.summary.contains("unsupported session schema"));
}

#[tokio::test(flavor = "current_thread")]
async fn explicit_resume_migration_completes_off_thread_before_loading() {
    let guard = test_home_guard();
    let session_id = saved_one_row_session(&guard);
    let session_dir = smelt_core::session::dir_for_id(&session_id);
    let db = smelt_store::SessionDb::open(session_dir.join("session.db")).unwrap();
    db.connection()
        .execute_batch("PRAGMA user_version = 9")
        .unwrap();
    drop(db);

    let mut resumed = TestApp::builder().build_without_test_home_reset(&guard);
    let initial_session_id = resumed.session_snapshot().id.clone();
    resumed.load_session_by_id(&session_id);
    assert_eq!(resumed.session_snapshot().id, initial_session_id);
    assert!(resumed.working_state().busy);

    let event = next_app_event(&mut resumed).await;
    assert_eq!(resumed.session_snapshot().id, initial_session_id);

    resumed.handle_app_event(event);

    assert!(!resumed.working_state().busy);
    assert_eq!(resumed.session_snapshot().id, session_id);
    assert_eq!(resumed.app.session_history_len(), 1);
    assert_eq!(
        smelt_store::session_schema_status(session_dir.parent().unwrap(), &session_id).unwrap(),
        smelt_store::SessionSchemaStatus::Current {
            version: smelt_store::SCHEMA_VERSION,
        }
    );
}

#[tokio::test(flavor = "current_thread")]
async fn stale_schema_migration_completion_does_not_replace_newer_load_request() {
    let guard = test_home_guard();
    let session_id = saved_one_row_session(&guard);
    let session_dir = smelt_core::session::dir_for_id(&session_id);
    let db = smelt_store::SessionDb::open(session_dir.join("session.db")).unwrap();
    db.connection()
        .execute_batch("PRAGMA user_version = 9")
        .unwrap();
    drop(db);

    let mut resumed = TestApp::builder().build_without_test_home_reset(&guard);
    let initial_session_id = resumed.session_snapshot().id.clone();
    resumed.load_session_by_id(&session_id);
    assert!(resumed.working_state().busy);
    resumed.load_session_by_id("missing-session");

    let event = next_app_event(&mut resumed).await;
    resumed.handle_app_event(event);

    assert!(!resumed.working_state().busy);
    assert_eq!(resumed.session_snapshot().id, initial_session_id);
    assert_eq!(
        smelt_store::session_schema_status(session_dir.parent().unwrap(), &session_id).unwrap(),
        smelt_store::SessionSchemaStatus::Current {
            version: smelt_store::SCHEMA_VERSION,
        }
    );
}

#[test]
fn session_save_notification_dismissal_uses_typed_ownership() {
    let mut app = TestApp::builder().build();
    let session_id = app.session_snapshot().id.clone();

    app.notify_error_sticky(format!(
        "failed to save session {session_id}: unrelated diagnostic"
    ));
    app.dismiss_session_save_failure_notification(&session_id);
    assert!(
        app.overlays_probe().notification().is_some(),
        "matching copy without persistence ownership must remain visible"
    );

    app.notify_session_save_failure(&session_id, "database busy");
    app.dismiss_session_save_failure_notification("another-session");
    assert!(has_sticky_session_save_failure(&app, &session_id));

    app.dismiss_session_save_failure_notification(&session_id);
    assert!(app.overlays_probe().notification().is_none());
}

#[test]
fn metadata_only_title_update_persists_after_clean_history_save() {
    let guard = test_home_guard();
    let mut app = TestApp::builder().build_with_test_home_guard(&guard);
    let session_id = app.session_snapshot().id.clone();

    app.session_append_history(HistoryItem::user(Content::text("persisted history")));
    app.save_session_and_flush();
    app.set_session_title("Renamed session".into(), "renamed-session".into(), None);
    app.save_session_and_flush();

    let loaded = loaded_session(&app, &session_id);
    assert_eq!(loaded.title.as_deref(), Some("Renamed session"));
    assert_eq!(loaded.slug.as_deref(), Some("renamed-session"));
    assert_eq!(loaded.history.len(), 1);
}

#[test]
fn fast_mode_only_save_advances_canonical_revision_once() {
    let guard = test_home_guard();
    let mut app = TestApp::builder().build_with_test_home_guard(&guard);
    let session_id = app.session_snapshot().id.clone();
    app.session_append_history(HistoryItem::user(Content::text("persisted history")));
    app.save_session_and_flush();
    let before = session_revision(&app, &session_id);

    app.set_fast_mode(true);
    app.save_session_and_flush();

    assert_eq!(session_revision(&app, &session_id), before + 1);
    assert_eq!(loaded_session(&app, &session_id).fast_mode, Some(true));
}

#[test]
fn record_only_save_advances_canonical_revision_once() {
    let guard = test_home_guard();
    let mut app = TestApp::builder().build_with_test_home_guard(&guard);
    let session_id = app.session_snapshot().id.clone();
    app.session_append_history(HistoryItem::user(Content::text("persisted history")));
    app.save_session_and_flush();
    let before = session_revision(&app, &session_id);

    app.push_transcript_block(Block::Thinking {
        title: None,
        summary_titles: Vec::new(),
        kind: protocol::ReasoningKind::Raw,
        content: "record-only change".into(),
    });
    app.save_session_and_flush();

    assert_eq!(session_revision(&app, &session_id), before + 1);
}

#[test]
fn reasoning_summary_event_merges_durably_compacted_tail() {
    let guard = test_home_guard();
    let mut app = TestApp::builder().build_with_test_home_guard(&guard);
    app.start_turn(42);
    app.feed_one(SourceEvent::engine(EngineEvent::ReasoningPartFinished {
        id: "summary-1".into(),
        kind: protocol::ReasoningKind::Summary,
        title: Some("Inspecting the report".into()),
        content: String::new(),
    }));
    app.save_session_and_flush();

    app.drain_transcript_compaction_for_harness();
    let (len, original_id, materialized) = app
        .transcript_tail_state_for_harness()
        .expect("compacted reasoning summary");
    assert_eq!(len, 1);
    assert!(!materialized);
    app.set_transcript_memory_budget_for_harness(crate::app::transcript::TranscriptMemoryBudget {
        hydrated_blocks: 1,
        ..Default::default()
    });

    app.feed_one(SourceEvent::engine(EngineEvent::ReasoningPartFinished {
        id: "summary-2".into(),
        kind: protocol::ReasoningKind::Summary,
        title: Some("Planning the fix".into()),
        content: "The stored tail remains mergeable.".into(),
    }));

    let (len, id, block) = app
        .with_pinned_transcript_blocks(&[original_id], |history| {
            (
                history.len(),
                history.last_block_id(),
                history.block(original_id).cloned(),
            )
        })
        .expect("hydrate merged summary");
    assert_eq!(len, 1);
    assert_eq!(id, Some(original_id));
    assert!(matches!(
        block,
        Some(Block::Thinking {
            title: Some(title),
            summary_titles,
            content,
            kind: protocol::ReasoningKind::Summary,
        }) if title == "Planning the fix"
            && summary_titles == ["Inspecting the report", "Planning the fix"]
            && content == "The stored tail remains mergeable."
    ));
}

#[test]
fn resumed_turn_hydrates_legacy_multi_arg_tool_record_suffix_for_save() {
    const RECORD_COUNT: usize = 600;

    let guard = test_home_guard();
    let session_id = {
        let mut app = TestApp::builder().build_with_test_home_guard(&guard);
        for index in 0..RECORD_COUNT {
            let block = if index % 2 == 0 {
                Block::Text {
                    content: format!("record {index}"),
                }
            } else {
                Block::ToolCall {
                    call_id: format!("call-{index}"),
                    name: "bash".into(),
                    summary: protocol::StyledLines::from_plain(format!("tool {index}")),
                    args: HashMap::from([
                        ("command".into(), serde_json::json!(format!("echo {index}"))),
                        ("description".into(), serde_json::json!("regression")),
                        ("timeout_ms".into(), serde_json::json!(30_000)),
                        ("background".into(), serde_json::json!(false)),
                        ("alpha".into(), serde_json::json!({"nested": true})),
                        ("bravo".into(), serde_json::json!([1, 2, 3])),
                        ("charlie".into(), serde_json::json!(null)),
                        ("delta".into(), serde_json::json!(4)),
                    ]),
                }
            };
            app.push_transcript_block(block);
        }
        app.save_session_and_flush();
        app.session_snapshot().id.clone()
    };
    let legacy_tool_records = rewrite_tool_record_hashes_as_legacy(&session_id);
    assert_eq!(legacy_tool_records, RECORD_COUNT / 2);

    let mut app = TestApp::builder().build_without_test_home_reset(&guard);
    app.load_session_by_id(&session_id);
    let history = app.conversation_probe().transcript().history();
    let legacy_tool_id = history
        .order
        .iter()
        .copied()
        .find(|id| history.block_kind(*id) == Some("tool") && !history.is_materialized(*id))
        .expect("test requires a compacted legacy tool record");
    let loaded_content_hash = history.content_hash(legacy_tool_id);
    let legacy_record = smelt_store::SessionReader::open_existing(app.session_dir())
        .unwrap()
        .read_all_transcript_records()
        .unwrap()
        .into_iter()
        .find(|record| record.block_idx == legacy_tool_id.get())
        .unwrap();
    let block: Block = serde_json::from_str(&legacy_record.block_json).unwrap();
    assert_ne!(
        legacy_record.content_hash.parse::<u64>().unwrap(),
        block.content_hash()
    );
    assert_eq!(loaded_content_hash, block.content_hash());

    app.require_transcript_record_resave_from_for_harness(0);
    app.use_model(app.core_probe().config.available_models[0].clone());
    app.type_text("continue the session");
    app.press(KeyCode::Enter);

    assert!(
        app.agent_running(),
        "failed to start resumed turn: {:?}",
        app.overlays_probe().notification()
    );
    assert!(
        !has_sticky_session_save_failure(&app, &session_id),
        "resumed turn failed to save: {:?}",
        app.overlays_probe().notification()
    );
}

#[test]
fn compacted_record_suffix_stays_saveable_after_inserting_a_prefix() {
    const RECORD_COUNT: usize = 600;

    let guard = test_home_guard();
    let session_id = {
        let mut app = TestApp::builder().build_with_test_home_guard(&guard);
        for index in 0..RECORD_COUNT {
            app.push_transcript_block(Block::Text {
                content: format!("record {index}"),
            });
        }
        app.save_session_and_flush();
        app.session_snapshot().id.clone()
    };
    let mut app = TestApp::builder().build_without_test_home_reset(&guard);
    app.load_session_by_id(&session_id);
    let initial_record_start = {
        let history = app.conversation_probe().transcript().history();
        let first = history.order.first().copied().expect("loaded tail");
        history.stored_ref(first).unwrap().record_index
    };
    assert!(
        initial_record_start > 0,
        "test requires a sparse tail with a non-zero record base"
    );
    assert!(app
        .conversation_probe()
        .transcript()
        .history()
        .order
        .iter()
        .all(|id| !app
            .conversation_probe()
            .transcript()
            .history()
            .is_materialized(*id)));
    app.set_transcript_memory_budget_for_harness(crate::app::transcript::TranscriptMemoryBudget {
        hydrated_blocks: 1,
        ..Default::default()
    });

    app.insert_transcript_checkpoint_for_harness(
        0,
        Block::Text {
            content: "inserted prefix".into(),
        },
    );
    app.save_session();
    assert!(app
        .conversation_probe()
        .transcript()
        .history()
        .order
        .iter()
        .any(|id| !app
            .conversation_probe()
            .transcript()
            .history()
            .is_materialized(*id)));
    app.flush_persist();
    assert!(
        !app.session_document_has_unflushed_work(),
        "shifted record save was not acknowledged"
    );
    assert_eq!(
        app.conversation_probe().transcript().record_total_count(),
        Some(RECORD_COUNT + 1)
    );
    app.drain_transcript_compaction_for_harness();
    let inserted_id = app.conversation_probe().transcript().history().order[0];
    assert_eq!(
        app.conversation_probe()
            .transcript()
            .history()
            .stored_ref(inserted_id)
            .expect("inserted record compacted")
            .record_index,
        initial_record_start,
        "compaction lost the sparse tail's global record base"
    );
    app.require_transcript_record_resave_from_for_harness(0);
    app.set_fast_mode(true);
    app.save_session_and_flush();

    assert_eq!(
        app.conversation_probe()
            .transcript()
            .history()
            .record_dirty_from(),
        None
    );
    assert!(
        !app.session_document_has_unflushed_work(),
        "rewritten compacted record suffix remained dirty: {:?}",
        app.overlays_probe().notification()
    );
    assert!(
        !has_sticky_session_save_failure(&app, &session_id),
        "rewritten compacted record suffix failed to save: {:?}",
        app.overlays_probe().notification()
    );
    assert!(
        app.overlays_probe()
            .notification()
            .is_none_or(|notification| !notification
                .summary
                .contains("hydrate canonical transcript record suffix")),
        "record retry used stale persisted indices: {:?}",
        app.overlays_probe().notification()
    );

    let reader =
        smelt_store::SessionReader::open_existing(smelt_core::session::dir_for_id(&session_id))
            .unwrap();
    let durable_previews = reader
        .read_all_transcript_records()
        .unwrap()
        .into_iter()
        .map(|record| record.preview_text)
        .collect::<Vec<_>>();
    let mut expected_previews = (0..RECORD_COUNT)
        .map(|index| format!("record {index}"))
        .collect::<Vec<_>>();
    expected_previews.insert(initial_record_start, "inserted prefix".into());
    assert_eq!(durable_previews, expected_previews);
    drop(app);

    let mut reloaded = TestApp::builder().build_without_test_home_reset(&guard);
    reloaded.load_session_by_id(&session_id);
    let ids = reloaded
        .conversation_probe()
        .transcript()
        .history()
        .order
        .clone();
    for id in ids.iter().copied() {
        let record_index = reloaded
            .conversation_probe()
            .transcript()
            .history()
            .stored_ref(id)
            .unwrap()
            .record_index;
        assert!(record_index < durable_previews.len());
    }
    let reloaded_rows = reloaded
        .with_pinned_transcript_blocks(&ids, |history| {
            ids.iter()
                .map(|id| {
                    let stored = history.stored_ref(*id).unwrap();
                    (history.raw_text(*id).unwrap(), stored.record_index)
                })
                .collect::<Vec<_>>()
        })
        .expect("hydrate reloaded record tail");
    let reloaded_text = reloaded_rows
        .iter()
        .map(|(text, _)| text.clone())
        .collect::<Vec<_>>();
    let reloaded_expected = reloaded_rows
        .iter()
        .map(|(_, record_index)| durable_previews[*record_index].clone())
        .collect::<Vec<_>>();
    assert_eq!(reloaded_text, reloaded_expected);
}

#[test]
fn record_resave_preserves_semantic_history_links() {
    let guard = test_home_guard();
    let mut app = TestApp::builder().build_with_test_home_guard(&guard);
    let session_id = app.session_snapshot().id.clone();
    app.commit_request_history_item(
        HistoryItem::user(protocol::compaction_summary_content("retained summary")),
        Some(Block::Compacted {
            summary: "retained summary".into(),
        }),
    );
    let note = protocol::HistoryNote::process_status_event(
        protocol::ProcessStatusEvent::background_process_completed("4242", Some(0)),
    );
    app.commit_request_history_item(
        HistoryItem::note(note.clone()),
        app.history_note_to_block(&note),
    );
    assert!(!has_sticky_session_save_failure(&app, &session_id));

    app.require_transcript_record_resave_from_for_harness(0);
    app.save_session_and_flush();

    assert!(
        !has_sticky_session_save_failure(&app, &session_id),
        "record resave must preserve semantic origins: {:?}",
        app.overlays_probe().notification()
    );
}

#[test]
fn history_only_process_status_session_accepts_persisted_follow_up() {
    let guard = test_home_guard();
    let session_id = {
        let mut app = TestApp::builder().build_with_test_home_guard(&guard);
        app.handle_process_completed("4242".into(), Some(0));
        let turn_id = app.current_turn_id().expect("process-status turn started");
        app.feed_one(SourceEvent::engine(EngineEvent::TurnComplete {
            turn_id,
            history: None,
            meta: None,
        }));
        app.save_session_and_flush();
        assert!(!app.session_is_read_only());
        app.session_snapshot().id.clone()
    };

    let db_path = smelt_core::session::dir_for_id(&session_id).join("session.db");
    let db = smelt_store::SessionDb::open(db_path).unwrap();
    db.connection()
        .execute("DELETE FROM transcript_search", [])
        .unwrap();
    db.connection()
        .execute("DELETE FROM transcript_blocks", [])
        .unwrap();
    drop(db);

    let mut resumed = TestApp::builder().build_without_test_home_reset(&guard);
    resumed.load_session_by_id(&session_id);
    assert!(!resumed.session_is_read_only());

    resumed.start_submitted_turn("follow up after the agent finished");
    resumed.save_session_and_flush();

    assert!(
        !has_sticky_session_save_failure(&resumed, &session_id),
        "follow-up save failed: {:?}",
        resumed.overlays_probe().notification()
    );
    assert!(!resumed.session_is_read_only());
    assert!(loaded_session(&resumed, &session_id)
        .history
        .iter()
        .any(|item| {
            matches!(
                item,
                HistoryItem::User { content, .. }
                    if content.text_content() == "follow up after the agent finished"
            )
        }));
}

#[test]
fn exact_no_op_save_does_not_advance_canonical_revision() {
    let guard = test_home_guard();
    let mut app = TestApp::builder().build_with_test_home_guard(&guard);
    let session_id = app.session_snapshot().id.clone();
    app.session_append_history(HistoryItem::user(Content::text("persisted history")));
    app.save_session_and_flush();
    let before = session_revision(&app, &session_id);

    app.save_session_and_flush();

    assert_eq!(session_revision(&app, &session_id), before);
}

#[test]
fn shutdown_flushes_latest_generation_after_in_flight_save() {
    let guard = test_home_guard();
    let mut app = TestApp::builder().build_with_test_home_guard(&guard);

    app.session_append_history(HistoryItem::user(Content::text("first generation")));
    app.save_session();
    assert!(app.session_document_has_unflushed_work());

    app.session_append_history(HistoryItem::user(Content::text("final generation")));
    app.save_session();
    assert!(app.session_document_has_unflushed_work());

    app.save_session_and_flush();

    let loaded = loaded_session(&app, &app.session_snapshot().id);
    assert_eq!(loaded.history.len(), 2);
    assert!(matches!(
        loaded.history.last(),
        Some(HistoryItem::User { content, .. }) if content.text_content() == "final generation"
    ));
}

#[test]
fn blocked_save_requires_explicit_retry() {
    let guard = test_home_guard();
    let mut app = TestApp::builder().build_with_test_home_guard(&guard);
    let session_id = app.session_snapshot().id.clone();
    app.session_append_history(HistoryItem::user(Content::text("baseline")));
    app.save_session_and_flush();
    app.inject_commit_failure(smelt_store::SessionCommitFailure::UnsupportedSchema {
        found: i32::MAX,
        expected: 0,
    });
    app.session_append_history(HistoryItem::user(Content::text("retry me")));

    app.save_session_and_flush();
    assert!(app.session_document_has_unflushed_work());
    assert!(has_sticky_session_save_failure(&app, &session_id));

    assert!(retry_persistence_via_lua(&mut app));
    let outcome = app.flush_persist();
    assert!(
        !app.session_document_has_unflushed_work(),
        "retry flush left work pending: {outcome:?}; status: {:?}",
        app.conversation_probe().persistence_status()
    );
    let loaded = loaded_session(&app, &session_id);
    assert_eq!(loaded.history.len(), 2);
}

#[test]
fn environmental_failures_remain_dirty_until_explicit_retry() {
    let guard = test_home_guard();
    let mut app = TestApp::builder().build_with_test_home_guard(&guard);
    let session_id = app.session_snapshot().id.clone();
    app.session_append_history(HistoryItem::user(Content::text("baseline")));
    app.save_session_and_flush();

    for (index, failure) in [
        smelt_store::SessionCommitFailure::Sqlite {
            message: "disk full while extending session database".into(),
        },
        smelt_store::SessionCommitFailure::Io {
            message: "permission denied while writing session database".into(),
        },
        smelt_store::SessionCommitFailure::Io {
            message: "session storage root is missing".into(),
        },
    ]
    .into_iter()
    .enumerate()
    {
        let message = match &failure {
            smelt_store::SessionCommitFailure::Sqlite { message }
            | smelt_store::SessionCommitFailure::Io { message } => message,
            _ => unreachable!("environmental fault fixture"),
        };
        for _ in 0..2 {
            app.inject_commit_failure(failure.clone());
        }
        app.session_append_history(HistoryItem::user(Content::text(message.as_str())));

        app.save_session_and_flush();

        assert!(app.session_document_has_unflushed_work());
        assert!(has_sticky_session_save_failure(&app, &session_id));
        assert!(app
            .overlays_probe()
            .notification()
            .is_some_and(|notification| notification.summary.contains(message.as_str())));
        assert_eq!(loaded_session(&app, &session_id).history.len(), index + 1);

        assert!(retry_persistence_via_lua(&mut app));
        app.flush_persist();
        assert!(!app.session_document_has_unflushed_work());
        assert_eq!(loaded_session(&app, &session_id).history.len(), index + 2);
    }
}

#[test]
fn lua_delete_returns_actionable_error_for_malicious_id() {
    let guard = test_home_guard();
    let mut app = TestApp::builder().build_with_test_home_guard(&guard);
    let target = engine::state_dir().join("must-not-delete");
    std::fs::create_dir_all(&target).unwrap();
    app.set_lua_string_global("DELETE_TARGET", target.to_string_lossy())
        .unwrap();

    let (ok, message): (bool, String) = app
        .eval_lua(
            "local ok, err = pcall(function() smelt.session.delete(DELETE_TARGET) end); return ok, tostring(err)",
        )
        .unwrap();

    assert!(!ok);
    assert!(message.contains("invalid session id"), "{message}");
    assert!(target.exists());
}

#[test]
fn shutdown_keeps_permanent_storage_failure_visible_and_dirty() {
    let guard = test_home_guard();
    let mut app = TestApp::builder().build_with_test_home_guard(&guard);
    let session_id = app.session_snapshot().id.clone();
    let session_dir = smelt_core::session::dir_for_id(&session_id);
    std::fs::create_dir_all(session_dir.parent().unwrap()).unwrap();
    std::fs::write(&session_dir, "permanently blocks directory creation").unwrap();
    app.session_append_history(HistoryItem::user(Content::text("cannot save")));

    app.save_session_and_flush();

    assert!(app.session_document_has_unflushed_work());
    assert!(app.overlays_probe().notification().is_some());
    assert!(!session_dir.join("session.db").exists());
}

#[test]
fn new_empty_session_does_not_create_a_directory() {
    let guard = test_home_guard();
    let app = TestApp::builder().build_with_test_home_guard(&guard);
    let session_dir = smelt_core::session::dir_for_id(&app.session_snapshot().id);

    assert!(!session_dir.exists());
    drop(app);
    assert!(!session_dir.exists());
}

#[test]
fn identical_object_bytes_can_serve_distinct_request_roles() {
    let guard = test_home_guard();
    let mut app = TestApp::builder().build_with_test_home_guard(&guard);
    let session_id = app.session_snapshot().id.clone();
    app.session_append_history(HistoryItem::user(Content::text("same bytes in two roles")));
    app.save_session_and_flush();
    let mut audit = request_audit_entry(43);
    audit.body = serde_json::json!({});
    audit.response = Some(protocol::request_log::RequestResponse {
        content: None,
        reasoning: None,
        tool_calls: None,
        raw: None,
    });

    app.dispatch_host_call(engine::HostCall::RequestAudit {
        session_dir: smelt_core::session::dir_for_id(&session_id),
        persistence: app.conversation_probe().persistence_scope(),
        entry: Box::new(audit),
        payload_mode: smelt_store::RequestAuditPayloadMode::Full,
    });
    app.flush_persist();

    let db_path = smelt_core::session::dir_for_id(&session_id).join("session.db");
    let db = smelt_store::SessionDb::open_read_only(db_path).unwrap();
    let shared_roles: i64 = db
        .connection()
        .query_row(
            "SELECT COUNT(*)
             FROM request_object_refs body
             JOIN request_object_refs response
               ON response.request_attempt_id = body.request_attempt_id
              AND response.object_hash = body.object_hash
             WHERE body.role = 'body_json' AND response.role = 'response'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(shared_roles, 1);
    let attempts = db
        .query_request_attempts(&smelt_store::RequestAuditQuery::default())
        .unwrap();
    assert_eq!(attempts.len(), 1);
    let payloads = db.request_payloads(attempts[0].id).unwrap().unwrap();
    assert_eq!(payloads.body, Some(serde_json::json!({})));
    assert_eq!(payloads.response, Some(serde_json::json!({})));
}

#[test]
fn stale_request_audit_after_session_switch_is_rejected() {
    let guard = test_home_guard();
    let mut app = TestApp::builder().build_with_test_home_guard(&guard);
    app.session_append_history(HistoryItem::user(Content::text("old session")));
    app.save_session_and_flush();
    let old_id = app.session_snapshot().id.clone();
    let old_dir = smelt_core::session::dir_for_id(&old_id);
    let old_scope = app.conversation_probe().persistence_scope();

    app.reset_session();
    app.session_append_history(HistoryItem::user(Content::text("new session")));
    app.save_session_and_flush();
    let new_id = app.session_snapshot().id.clone();

    app.dispatch_host_call(engine::HostCall::RequestAudit {
        session_dir: old_dir.clone(),
        persistence: old_scope,
        entry: Box::new(request_audit_entry(42)),
        payload_mode: smelt_store::RequestAuditPayloadMode::SUMMARY,
    });
    app.flush_persist();

    for id in [&old_id, &new_id] {
        let reader =
            smelt_store::SessionReader::open_existing(smelt_core::session::dir_for_id(id)).unwrap();
        assert!(reader
            .query_request_attempts(&smelt_store::RequestAuditQuery::default())
            .unwrap()
            .is_empty());
    }
}

#[test]
fn sparse_fork_publishes_a_complete_destination() {
    let guard = test_home_guard();
    let session_id = {
        let mut app = TestApp::builder().build_with_test_home_guard(&guard);
        app.session_append_history(HistoryItem::user(Content::text("fork source")));
        app.save_session_and_flush();
        app.session_snapshot().id.clone()
    };
    let mut resumed = TestApp::builder().build_without_test_home_reset(&guard);
    resumed.load_session_by_id(&session_id);

    resumed.fork_session();

    let fork_id = resumed.session_snapshot().id.clone();
    assert_ne!(fork_id, session_id);
    let fork_dir = smelt_core::session::dir_for_id(&fork_id);
    let reader = smelt_store::SessionReader::open_existing(&fork_dir).unwrap();
    let stored = reader.stored_session().unwrap().unwrap();
    assert_eq!(stored.identity.id, fork_id);
    assert_eq!(
        stored.identity.parent_id.as_deref(),
        Some(session_id.as_str())
    );
    assert_eq!(reader.read_history_items_range(0..1).unwrap().len(), 1);
}

#[test]
fn large_sparse_fork_preserves_every_canonical_history_and_record_row() {
    let guard = test_home_guard();
    let session_id = {
        let mut app = TestApp::builder().build_with_test_home_guard(&guard);
        for index in 0..700 {
            app.session_append_history(HistoryItem::user(Content::text(format!(
                "fork source row {index}\nexact suffix {index}"
            ))));
        }
        app.restore_screen();
        app.save_session_and_flush();
        app.session_snapshot().id.clone()
    };
    let source_dir = smelt_core::session::dir_for_id(&session_id);
    let source = smelt_store::SessionReader::open_existing(&source_dir).unwrap();
    let source_history = source.read_history_items_range(0..700).unwrap();
    let source_records = source.read_all_transcript_records().unwrap();
    assert_eq!(source_history.len(), 700);
    assert_eq!(source_records.len(), 700);

    let mut resumed = TestApp::builder().build_without_test_home_reset(&guard);
    resumed.load_session_by_id(&session_id);
    resumed.set_transcript_memory_budget_for_harness(
        crate::app::transcript::TranscriptMemoryBudget {
            hydrated_blocks: 1,
            ..Default::default()
        },
    );
    resumed.render_silent();
    assert!(
        resumed
            .conversation_probe()
            .transcript()
            .memory_snapshot()
            .hydrated_blocks
            < source_records.len()
    );

    resumed.fork_session();

    let fork_id = resumed.session_snapshot().id.clone();
    assert_ne!(fork_id, session_id);
    let fork = smelt_store::SessionReader::open_existing(smelt_core::session::dir_for_id(&fork_id))
        .unwrap();
    assert_eq!(
        fork.read_history_items_range(0..700).unwrap(),
        source_history
    );
    assert_eq!(fork.read_all_transcript_records().unwrap(), source_records);
}

#[test]
fn sparse_fork_does_not_copy_unreferenced_legacy_blobs() {
    let guard = test_home_guard();
    let session_id = {
        let mut app = TestApp::builder().build_with_test_home_guard(&guard);
        app.session_append_history(HistoryItem::user(Content::text("fork source")));
        app.save_session_and_flush();
        app.session_snapshot().id.clone()
    };
    let mut resumed = TestApp::builder().build_without_test_home_reset(&guard);
    resumed.load_session_by_id(&session_id);
    let source_dir = smelt_core::session::dir_for_id(&session_id);
    let blob_dir = source_dir.join("blobs");
    std::fs::create_dir_all(&blob_dir).unwrap();
    std::fs::write(blob_dir.join("unreferenced.png"), "private attachment").unwrap();

    resumed.fork_session();

    let fork_id = resumed.session_snapshot().id.clone();
    assert_ne!(fork_id, session_id);
    assert!(!smelt_core::session::dir_for_id(&fork_id)
        .join("blobs")
        .exists());
}

#[cfg(unix)]
#[test]
fn sparse_fork_rejects_symlinked_legacy_attachment() {
    use std::os::unix::fs::symlink;

    const DATA_URL: &str = "data:image/png;base64,OUTSIDE";
    const HASH: &str = "94fc06df2866baea99e3b3ea05bc2cd733de4ed7a085dda436280f7f69ffd426";

    let guard = test_home_guard();
    let session_id = {
        let mut app = TestApp::builder().build_with_test_home_guard(&guard);
        app.session_append_history(HistoryItem::user(Content::text("fork source")));
        app.save_session_and_flush();
        app.session_snapshot().id.clone()
    };
    let source_dir = smelt_core::session::dir_for_id(&session_id);
    let reader = smelt_store::SessionReader::open_existing(&source_dir).unwrap();
    let stored = reader.stored_session().unwrap().unwrap();
    drop(reader);
    let root = source_dir.parent().expect("sessions root");
    let mut writer = smelt_store::OwnedSessionWriter::open(root, &session_id).unwrap();
    writer
        .commit_session(&smelt_store::SessionCommit {
            session_id: session_id.clone(),
            expected: stored.head,
            identity: stored.identity,
            metadata: stored.metadata,
            history: smelt_store::HistorySuffix {
                start: smelt_store::HistoryIndex::ZERO,
                final_len: smelt_store::HistoryLen::new(1),
                items: vec![HistoryItem::user(Content::with_images(
                    "legacy".into(),
                    vec![("attachment.png".into(), format!("blob:{HASH}.png"))],
                ))],
            },
            side_tables: smelt_store::SideTableSuffixes::default(),
            transcript_records: None,
        })
        .unwrap();
    writer.release().unwrap();

    let blob_dir = source_dir.join("blobs");
    std::fs::create_dir(&blob_dir).unwrap();
    let blob_path = blob_dir.join(format!("{HASH}.png"));
    std::fs::write(&blob_path, DATA_URL).unwrap();
    let mut resumed = TestApp::builder().build_without_test_home_reset(&guard);
    resumed.load_session_by_id(&session_id);
    assert!(resumed.conversation_probe().has_live_session());

    let external = tempfile::tempdir().unwrap();
    let target = external.path().join("private-image");
    std::fs::write(&target, DATA_URL).unwrap();
    std::fs::remove_file(&blob_path).unwrap();
    symlink(&target, &blob_path).unwrap();
    resumed.fork_session();

    assert_eq!(resumed.session_snapshot().id, session_id);
    assert!(resumed.overlays_probe().notification().is_some());
    let sessions = smelt_core::session::list_sessions();
    assert_eq!(sessions.len(), 1, "unexpected sessions: {sessions:#?}");
}

#[test]
fn shutdown_flushes_record_only_transcript_blocks() {
    let guard = test_home_guard();
    let mut app = TestApp::builder().build_with_test_home_guard(&guard);
    let session_id = app.session_snapshot().id.clone();

    app.push_transcript_block(Block::Thinking {
        title: None,
        summary_titles: Vec::new(),
        kind: protocol::ReasoningKind::Raw,
        content: "record-only interrupted thinking".into(),
    });
    app.save_session_and_flush();

    let db = smelt_store::SessionReader::open_database(
        smelt_core::session::dir_for_id(&session_id).join("session.db"),
    )
    .unwrap();
    let rows = db.read_all_transcript_records().unwrap();
    assert!(
        rows.iter().any(|row| row
            .preview_text
            .contains("record-only interrupted thinking")),
        "record-only transcript block should be durable: {rows:#?}"
    );
}

#[test]
fn sparse_record_resume_interrupt_save_compacts_and_appends_again() {
    let guard = test_home_guard();
    let session_id = {
        let mut app = TestApp::builder().build_with_test_home_guard(&guard);
        app.commit_request_history_item(
            HistoryItem::user(Content::text("sparse record prompt")),
            Some(Block::User {
                text: "sparse record prompt".into(),
                image_labels: Vec::new(),
                command: false,
            }),
        );
        app.push_transcript_block(Block::Text {
            content: "persisted before sparse rewrite".into(),
        });
        app.save_session_and_flush();
        app.session_snapshot().id.clone()
    };

    let db_path = smelt_core::session::dir_for_id(&session_id).join("session.db");
    let db = smelt_store::SessionDb::open(&db_path).unwrap();
    assert_eq!(db.transcript_record_count().unwrap(), 2);
    db.connection()
        .execute(
            "UPDATE transcript_blocks SET record_idx = 302 WHERE record_idx = 1",
            [],
        )
        .unwrap();
    drop(db);

    let mut resumed = TestApp::builder().build_without_test_home_reset(&guard);
    resumed.load_session_by_id(&session_id);
    assert_eq!(
        resumed
            .conversation_probe()
            .transcript()
            .record_total_count(),
        Some(2)
    );
    resumed.start_turn(3030);
    resumed.feed_one(SourceEvent::engine(EngineEvent::TextDelta {
        delta: "interrupted after sparse record load".into(),
    }));
    resumed.cancel();
    resumed.save_session_and_flush();
    assert!(
        resumed.overlays_probe().notification().is_none(),
        "sparse record save should not surface an integrity failure: {:?}",
        resumed.overlays_probe().notification()
    );
    drop(resumed);

    let db = smelt_store::SessionDb::open_read_only(&db_path).unwrap();
    let (count, min, max): (i64, Option<i64>, Option<i64>) = db
        .connection()
        .query_row(
            "SELECT COUNT(*), MIN(record_idx), MAX(record_idx)
             FROM transcript_blocks WHERE block_json IS NOT NULL",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!((count, min, max), (3, Some(0), Some(2)));
    drop(db);

    let mut resumed = TestApp::builder().build_without_test_home_reset(&guard);
    resumed.load_session_by_id(&session_id);
    resumed.push_transcript_block(Block::Thinking {
        title: None,
        summary_titles: Vec::new(),
        content: "append after dense reconciliation".into(),
        kind: protocol::ReasoningKind::Raw,
    });
    resumed.save_session_and_flush();
    assert!(resumed.overlays_probe().notification().is_none());

    let db = smelt_store::SessionReader::open_database(&db_path).unwrap();
    let rows = db.read_all_transcript_records().unwrap();
    assert_eq!(rows.len(), 4);
    assert!(rows.iter().any(|row| row
        .preview_text
        .contains("append after dense reconciliation")));
}

#[test]
fn store_backed_resume_preserves_context_token_identity() {
    let guard = test_home_guard();
    let session_id = {
        let mut app = TestApp::builder().build_with_test_home_guard(&guard);
        app.session_append_history(HistoryItem::user(Content::text("token identity prompt")));
        app.record_visible_token_usage(TokenUsage {
            context_tokens: Some(1234),
            ..Default::default()
        });
        app.save_session_and_flush();
        app.session_snapshot().id.clone()
    };

    let mut resumed = TestApp::builder().build_without_test_home_reset(&guard);
    resumed.load_session_by_id(&session_id);
    let identity = resumed.active_context_token_identity();

    assert_eq!(
        resumed.session_snapshot().display_context_tokens(),
        Some(1234)
    );
    assert!(!resumed
        .conversation_probe()
        .session()
        .display_context_tokens_stale(&identity));
}

#[test]
fn interrupted_turn_rewind_save_resume_restores_prior_context_tokens() {
    let guard = test_home_guard();
    let session_id = {
        let mut app = TestApp::builder().build_with_test_home_guard(&guard);
        app.commit_request_history_item(
            HistoryItem::user(Content::text("first prompt")),
            Some(Block::User {
                text: "first prompt".into(),
                image_labels: Vec::new(),
                command: false,
            }),
        );
        app.commit_request_history_item(
            HistoryItem::assistant(AssistantStep::terminal(
                Some(Content::text("first reply")),
                None,
                Vec::new(),
            )),
            Some(Block::Text {
                content: "first reply".into(),
            }),
        );
        app.start_turn(1);
        app.feed_one(SourceEvent::engine(EngineEvent::TokenUsage {
            usage: TokenUsage {
                context_tokens: Some(100),
                ..Default::default()
            },
            tokens_per_sec: None,
            cost_usd: None,
            background: false,
        }));
        app.feed_one(SourceEvent::engine(EngineEvent::TurnComplete {
            turn_id: 1,
            history: None,
            meta: None,
        }));
        app.save_session_and_flush();

        let second_prompt_block_idx = app.conversation_probe().transcript().history().len();
        app.commit_request_history_item(
            HistoryItem::user(Content::text("second prompt")),
            Some(Block::User {
                text: "second prompt".into(),
                image_labels: Vec::new(),
                command: false,
            }),
        );
        app.start_turn(2);
        app.feed_one(SourceEvent::engine(EngineEvent::TokenUsage {
            usage: TokenUsage {
                context_tokens: Some(250),
                ..Default::default()
            },
            tokens_per_sec: None,
            cost_usd: None,
            background: false,
        }));

        app.rewind_to_block(Some(second_prompt_block_idx), false);
        app.save_session_and_flush();
        assert_eq!(app.session_message_count(), 2);
        assert_eq!(app.session_snapshot().display_context_tokens(), Some(100));
        app.session_snapshot().id.clone()
    };

    let mut resumed = TestApp::builder().build_without_test_home_reset(&guard);
    let loaded = loaded_session(&resumed, &session_id);
    assert_eq!(loaded.history.len(), 2);
    assert_eq!(loaded.current_context_tokens(), Some(100));
    assert_eq!(loaded.display_context_tokens(), Some(100));

    resumed.load_session_by_id(&session_id);
    assert_eq!(resumed.session_message_count(), 2);
    assert_eq!(
        resumed.session_snapshot().display_context_tokens(),
        Some(100)
    );
}

#[test]
fn cancel_or_shutdown_preserves_committed_tool_invocations() {
    enum Finalizer {
        Cancel,
        Shutdown,
    }

    let guard = test_home_guard();
    for finalizer in [Finalizer::Cancel, Finalizer::Shutdown] {
        let mut app = TestApp::builder().build_with_test_home_guard(&guard);
        app.start_turn(42);
        app.feed_one(SourceEvent::engine(
            protocol::EngineEvent::HistoryAppended {
                turn_id: 42,
                delta: protocol::CanonicalHistoryDelta::new(0, tool_history()),
            },
        ));

        match finalizer {
            Finalizer::Cancel => app.cancel(),
            Finalizer::Shutdown => {
                if app.agent_running() {
                    app.discard_turn(crate::app::TurnEnd::Cancelled);
                }
            }
        }
        app.save_session_and_flush();

        let loaded = loaded_session(&app, &app.session_snapshot().id);
        assert_committed_tool_invocation(&loaded.history);
    }
}

#[test]
fn store_backed_resume_restores_tool_calls_for_model_history() {
    let guard = test_home_guard();
    let session_id = {
        let mut app = TestApp::builder().build_with_test_home_guard(&guard);
        for item in tool_history() {
            app.session_append_history(item);
        }
        app.save_session_and_flush();
        app.session_snapshot().id.clone()
    };

    let mut resumed = TestApp::builder().build_without_test_home_reset(&guard);
    resumed.load_session_by_id(&session_id);

    assert_eq!(resumed.session_snapshot().id, session_id);
    assert!(
        resumed.session_snapshot().history.is_empty(),
        "resume should use the production sparse SQLite session path"
    );
    assert_eq!(resumed.session_message_count(), 2);

    let stored_history = resumed.session_history_range(0..resumed.session_message_count());
    assert_committed_tool_invocation(&stored_history);
    assert_model_history_tool_messages(&resumed.model_history_messages());
}

#[test]
fn store_backed_resume_tolerates_bad_checkpoint_without_repairing_database() {
    let guard = test_home_guard();
    let session_id = {
        let mut app = TestApp::builder().build_with_test_home_guard(&guard);
        app.session_append_history(HistoryItem::user(Content::text("old prompt")));
        app.session_append_history(HistoryItem::assistant(AssistantStep::terminal(
            Some(Content::text("recent reply")),
            None,
            Vec::new(),
        )));
        app.save_session_and_flush();
        app.session_snapshot().id.clone()
    };

    let db_path = smelt_core::session::dir_for_id(&session_id).join("session.db");
    let db = smelt_store::SessionDb::open(&db_path).unwrap();
    let checkpoint = serde_json::json!({
        "kind": "compaction",
        "summary": "retained summary",
        "first_live_index": 177,
        "created_at_ms": 1,
    });
    db.connection()
        .execute(
            "UPDATE session_state SET checkpoint_json = ?1 WHERE singleton = 1",
            [checkpoint.to_string()],
        )
        .unwrap();
    drop(db);

    let mut resumed = TestApp::builder().build_without_test_home_reset(&guard);
    resumed.load_session_by_id(&session_id);

    assert_eq!(resumed.session_snapshot().id, session_id);
    assert!(resumed.session_snapshot().history.is_empty());
    let checkpoint = resumed
        .conversation_probe()
        .session()
        .checkpoint
        .as_ref()
        .expect("checkpoint tolerated on sparse resume");
    assert_eq!(checkpoint.first_live_index, 0);

    let history = resumed.model_history();
    assert_eq!(history.len(), 3);
    assert!(matches!(
        &history[0],
        HistoryItem::User { content, .. } if content.text_content().contains("retained summary")
    ));
    assert!(matches!(
        &history[1],
        HistoryItem::User { content, .. } if content.text_content() == "old prompt"
    ));
    assert!(matches!(
        &history[2],
        HistoryItem::Assistant(step)
            if step.content.as_ref().is_some_and(|content| content.text_content() == "recent reply")
    ));

    let persisted = smelt_store::SessionReader::open_database(&db_path)
        .unwrap()
        .stored_session()
        .unwrap()
        .unwrap()
        .metadata
        .checkpoint_json
        .unwrap();
    assert_eq!(persisted["first_live_index"].as_u64(), Some(177));
}

#[test]
fn store_backed_resume_then_continue_preserves_prior_tool_invocations() {
    let guard = test_home_guard();
    let session_id = {
        let mut app = TestApp::builder().build_with_test_home_guard(&guard);
        for item in tool_history() {
            app.session_append_history(item);
        }
        app.save_session_and_flush();
        app.session_snapshot().id.clone()
    };

    let mut resumed = TestApp::builder().build_without_test_home_reset(&guard);
    resumed.load_session_by_id(&session_id);
    resumed.session_append_history(HistoryItem::user(Content::text("continue after resume")));
    resumed.save_session_and_flush();

    let loaded = loaded_session(&resumed, &session_id);
    assert_eq!(loaded.history.len(), 3);
    assert_committed_tool_invocation(&loaded.history);
    assert!(matches!(
        loaded.history.last(),
        Some(HistoryItem::User { content, .. }) if content.text_content() == "continue after resume"
    ));
}

#[test]
fn repeated_store_backed_resume_cycles_preserve_all_history() {
    let guard = test_home_guard();
    let session_id = {
        let mut app = TestApp::builder().build_with_test_home_guard(&guard);
        for item in tool_history() {
            app.session_append_history(item);
        }
        app.save_session_and_flush();
        app.session_snapshot().id.clone()
    };

    for cycle in 0..4 {
        let mut resumed = TestApp::builder().build_without_test_home_reset(&guard);
        resumed.load_session_by_id(&session_id);
        assert_eq!(resumed.session_snapshot().id, session_id);
        assert!(resumed.conversation_probe().has_live_session());
        resumed.session_append_history(HistoryItem::user(Content::text(format!("cycle {cycle}"))));
        resumed.save_session_and_flush();
    }

    let reader = TestApp::builder().build_without_test_home_reset(&guard);
    let loaded = loaded_session(&reader, &session_id);
    assert_eq!(loaded.history.len(), 6);
    assert_committed_tool_invocation(&loaded.history);
    for cycle in 0..4 {
        assert!(loaded
            .history
            .iter()
            .any(|item| { matches!(item, HistoryItem::User { content, .. } if content.text_content() == format!("cycle {cycle}")) }));
    }
}

#[test]
fn resuming_session_with_active_writer_is_read_only() {
    let guard = test_home_guard();
    let mut writer = TestApp::builder().build_with_test_home_guard(&guard);
    writer.session_append_history(HistoryItem::user(Content::text("owned history")));
    writer.save_session_and_flush();

    let session_id = writer.session_snapshot().id.clone();
    let session_dir = smelt_core::session::dir_for_id(&session_id);
    let db_path = session_dir.join("session.db");
    let before = smelt_store::SessionReader::open_database(&db_path)
        .unwrap()
        .stored_session()
        .unwrap()
        .expect("session state before read-only resume");
    let mut reader = TestApp::builder().build_without_test_home_reset(&guard);
    reader.load_session_by_id(&session_id);

    assert_eq!(reader.session_snapshot().id, session_id);
    assert!(reader.session_is_read_only());
    assert_eq!(reader.session_message_count(), 1);

    let result = reader.apply_history_append_to_history(&HistoryAppend::append(HistoryItem::user(
        Content::text("read-only mutation"),
    )));
    assert_eq!(result, HistoryAppendResult::Unchanged);
    reader.save_session_and_flush();

    let after = smelt_store::SessionReader::open_database(&db_path)
        .unwrap()
        .stored_session()
        .unwrap()
        .expect("session state after read-only resume");
    assert_eq!(after.head.revision, before.head.revision);
    assert_eq!(after.head.history_len, before.head.history_len);
    assert_eq!(
        smelt_store::SessionReader::open_database(&db_path)
            .unwrap()
            .history_item_count()
            .unwrap(),
        1
    );
}

#[test]
fn ownership_loss_moves_session_to_read_only_and_keeps_document_dirty() {
    let guard = test_home_guard();
    let session_id = saved_one_row_session(&guard);
    let mut resumed = TestApp::builder().build_without_test_home_reset(&guard);
    resumed.load_session_by_id(&session_id);
    assert!(!resumed.session_is_read_only());
    resumed.inject_commit_failure(smelt_store::SessionCommitFailure::OwnershipLost);
    resumed.session_append_history(HistoryItem::user(Content::text(
        "unsaved after ownership loss",
    )));

    resumed.save_session_and_flush();

    assert!(resumed.session_is_read_only());
    assert!(resumed.session_document_has_unflushed_work());
    assert!(has_sticky_session_save_failure(&resumed, &session_id));
}

#[test]
fn ownership_loss_with_dirty_state_can_fork_to_a_writable_session() {
    let guard = test_home_guard();
    let session_id = saved_one_row_session(&guard);
    let mut resumed = TestApp::builder().build_without_test_home_reset(&guard);
    resumed.load_session_by_id(&session_id);
    resumed.start_turn(42);
    resumed.feed_one(SourceEvent::engine(EngineEvent::TextDelta {
        delta: "stream finalized by fork cancellation".into(),
    }));
    assert!(resumed.streaming_state().text);
    resumed.inject_commit_failure(smelt_store::SessionCommitFailure::OwnershipLost);
    resumed.session_append_history(HistoryItem::user(Content::text(
        "preserved in ownership-loss fork",
    )));
    resumed.push_transcript_block(Block::Text {
        content: "unsaved ownership-loss record".into(),
    });
    resumed.save_session_and_flush();
    assert!(resumed.session_is_read_only());
    assert!(resumed.session_document_has_unflushed_work());

    resumed.fork_session();

    let fork_id = resumed.session_snapshot().id.clone();
    assert_ne!(fork_id, session_id);
    assert!(!resumed.session_is_read_only());
    assert!(!resumed.session_document_has_unflushed_work());
    assert!(resumed.session_snapshot().history.is_empty());
    assert!(resumed.conversation_probe().has_live_session());
    let reader = smelt_store::SessionReader::open_existing(
        resumed.core_probe().sessions.dir_for_id(&fork_id),
    )
    .unwrap();
    let record_rows = reader.read_all_transcript_records().unwrap();
    assert!(record_rows
        .iter()
        .any(|row| row.preview_text.contains("unsaved ownership-loss record")));
    let full = reader.load_full_session().unwrap().unwrap();
    assert!(full.turn_metas.iter().any(|(history_len, meta)| {
        *history_len == 2 && meta["interrupted"] == serde_json::Value::Bool(true)
    }));
    let forked = loaded_session(&resumed, &fork_id);
    assert_eq!(forked.parent_id.as_deref(), Some(session_id.as_str()));
    assert_eq!(forked.history.len(), 2);
    assert!(matches!(
        forked.history.last(),
        Some(HistoryItem::User { content, .. })
            if content.text_content() == "preserved in ownership-loss fork"
    ));
    assert_eq!(loaded_session(&resumed, &session_id).history.len(), 1);
}

#[test]
fn live_save_restarts_at_stored_prefix_when_dirty_marker_skips_missing_row() {
    let guard = test_home_guard();
    let session_id = saved_one_row_session(&guard);

    let mut resumed = TestApp::builder().build_without_test_home_reset(&guard);
    resumed.load_session_by_id(&session_id);
    let stored_len = resumed.session_message_count();
    assert_eq!(stored_len, 1);

    resumed.session_append_history(HistoryItem::user(Content::text("kept live row")));
    assert!(resumed.conversation_probe().has_live_session());
    resumed.set_history_resave_from_for_harness(stored_len + 1);

    resumed.save_session();
    resumed.flush_persist();

    assert!(
        resumed.overlays_probe().notification().is_none(),
        "save should not surface a prefix-exceeds-stored integrity error"
    );
    let live = resumed
        .conversation_probe()
        .live_session()
        .expect("store-backed session");
    assert_eq!(live.live_suffix_len(), 0);

    let loaded = loaded_session(&resumed, &session_id);
    assert_eq!(loaded.history.len(), stored_len + 1);
    assert!(matches!(
        loaded.history.last(),
        Some(HistoryItem::User { content, .. }) if content.text_content() == "kept live row"
    ));
}

#[test]
fn pre_request_compaction_append_save_resume_keeps_canonical_history() {
    fn compacted_marker_count(app: &TestApp) -> usize {
        let history = app.conversation_probe().transcript().history();
        (0..history.len())
            .filter(|index| {
                history
                    .block_id_at(*index)
                    .and_then(|id| history.block_kind(id))
                    == Some("compacted")
            })
            .count()
    }

    let guard = test_home_guard();
    let mut app = TestApp::builder().build_with_test_home_guard(&guard);
    for idx in 0..24 {
        app.session_append_history(HistoryItem::user(Content::text(format!("row {idx}"))));
    }
    let compacted_prefix_len = app.session_message_count();
    app.start_turn(1);
    app.feed_one(SourceEvent::engine(EngineEvent::TokenUsage {
        usage: TokenUsage {
            context_tokens: Some(100),
            ..Default::default()
        },
        tokens_per_sec: None,
        cost_usd: None,
        background: false,
    }));
    app.feed_one(SourceEvent::engine(EngineEvent::TurnComplete {
        turn_id: 1,
        history: None,
        meta: None,
    }));
    app.save_session_and_flush();

    let mut settings = app.core_probe().config.settings.clone();
    settings.auto_compact = true;
    settings.compact_threshold = 0.8;
    settings.compact_keep_recent_groups = 1.0;
    app.set_settings_for_harness(settings);
    app.set_context_window(Some(100));
    app.commit_request_history_item(
        HistoryItem::user(Content::text("request after compaction")),
        Some(Block::User {
            text: "request after compaction".into(),
            image_labels: Vec::new(),
            command: false,
        }),
    );
    app.start_turn(42);

    let messages = protocol::history_to_messages(&app.model_history());
    let (tx, mut rx) = tokio::sync::oneshot::channel();
    {
        app.dispatch_host_call(engine::HostCall::PrepareRequest {
            messages: engine::PreparedRequestMessages::model_only(messages),
            estimated_tokens: 200,
            reply: tx,
        });
    }
    let ask_id = app
        .drain_engine_sends()
        .into_iter()
        .filter_map(|command| match command {
            protocol::UiCommand::EngineAsk { id, .. } => Some(id),
            _ => None,
        })
        .next_back()
        .expect("pre-request compaction should issue EngineAsk");
    {
        app.dispatch_engine_event(EngineEvent::EngineAskResponse {
            id: ask_id,
            message: Some(protocol::Message::assistant(
                Some(Content::text("# Goal\nretained summary")),
                None,
                None,
            )),
            error: None,
        });
        app.drive_lua_tasks();
    }
    assert_eq!(compacted_marker_count(&app), 1);

    let (replacement, coordinates) = match rx
        .try_recv()
        .expect("compaction prepare reply should be ready")
    {
        engine::HostRequestDecision::Replace {
            messages,
            coordinates,
        } => (messages, coordinates),
        decision => panic!("expected model-history replacement, got {decision:?}"),
    };
    assert_eq!(coordinates.model_prefix_len(), 1);
    assert_eq!(coordinates.canonical_start().get(), compacted_prefix_len);
    assert_eq!(replacement.len(), 2);

    let replacement_history = protocol::history_from_messages(replacement);
    app.feed_one(SourceEvent::engine(EngineEvent::HistoryUpdated {
        turn_id: 42,
        update: coordinates.canonical_delta(protocol::ModelHistoryIndex::ZERO, replacement_history),
    }));
    assert_eq!(compacted_marker_count(&app), 1);
    app.feed_one(SourceEvent::engine(EngineEvent::HistoryAppended {
        turn_id: 42,
        delta: protocol::CanonicalHistoryDelta {
            first_index: coordinates.canonical_index(protocol::ModelHistoryIndex::new(2)),
            items: vec![HistoryItem::Assistant(AssistantStep::terminal(
                Some(Content::text("reply after compaction")),
                None,
                Vec::new(),
            ))],
        },
    }));
    app.save_session_and_flush();

    assert!(
        app.overlays_probe().notification().is_none(),
        "compacted append should save without a side-table bounds error: {:?}",
        app.overlays_probe().notification()
    );
    assert_eq!(app.session_message_count(), compacted_prefix_len + 2);
    let session_id = app.session_snapshot().id.clone();
    let loaded = loaded_session(&app, &session_id);
    assert_eq!(loaded.history.len(), compacted_prefix_len + 2);
    assert!(loaded
        .context_snapshots
        .iter()
        .any(|(index, _)| *index == compacted_prefix_len));
    assert!(loaded
        .context_snapshots
        .iter()
        .all(|(index, _)| *index <= loaded.history.len()));
    assert!(matches!(
        loaded.history.first(),
        Some(HistoryItem::User { content, .. }) if content.text_content() == "row 0"
    ));
    assert!(matches!(
        loaded.history.get(compacted_prefix_len),
        Some(HistoryItem::User { content, .. }) if content.text_content() == "request after compaction"
    ));
    assert!(matches!(
        loaded.history.last(),
        Some(HistoryItem::Assistant(step))
            if step.content.as_ref().is_some_and(|content| content.text_content() == "reply after compaction")
    ));

    drop(app);
    let mut resumed = TestApp::builder().build_without_test_home_reset(&guard);
    resumed.load_session_by_id(&session_id);
    assert_eq!(compacted_marker_count(&resumed), 1);
    assert_eq!(resumed.session_message_count(), compacted_prefix_len + 2);
    let resumed_history = resumed.session_history_range(0..resumed.session_message_count());
    assert!(matches!(
        resumed_history.first(),
        Some(HistoryItem::User { content, .. }) if content.text_content() == "row 0"
    ));
    assert!(matches!(
        resumed_history.last(),
        Some(HistoryItem::Assistant(step))
            if step.content.as_ref().is_some_and(|content| content.text_content() == "reply after compaction")
    ));
}

#[test]
fn live_rewind_below_checkpoint_then_next_append_saves_without_bad_checkpoint() {
    let guard = test_home_guard();
    let session_id = {
        let mut app = TestApp::builder().build_with_test_home_guard(&guard);
        for idx in 0..4 {
            app.session_append_history(HistoryItem::user(Content::text(format!("row {idx}"))));
        }
        app.session_set_checkpoint(Some(smelt_core::ContextCheckpoint {
            kind: "compaction".into(),
            summary: "retained summary".into(),
            first_live_index: 3,
            created_at_ms: 1,
            tokens_before: None,
            tokens_after_estimate: None,
            tokens_after_estimate_history_len: None,
            pre_checkpoint_context_tokens: None,
            pre_checkpoint_context_history_len: None,
        }));
        app.save_session_and_flush();
        app.session_snapshot().id.clone()
    };

    let mut resumed = TestApp::builder().build_without_test_home_reset(&guard);
    resumed.load_session_by_id(&session_id);
    assert_eq!(
        resumed
            .conversation_probe()
            .session()
            .checkpoint
            .as_ref()
            .map(|checkpoint| checkpoint.first_live_index),
        Some(3)
    );
    let identity = resumed.active_context_token_identity();
    resumed.rewind_history_for_harness(1, false, identity);
    assert!(resumed.session_snapshot().checkpoint.is_none());

    resumed.session_append_history(HistoryItem::user(Content::text("new after rewind")));
    resumed.save_session_and_flush();

    assert!(
        resumed.overlays_probe().notification().is_none(),
        "save after rewind should not surface a checkpoint/history integrity error: {:?}",
        resumed.overlays_probe().notification()
    );
}

#[test]
fn in_flight_live_save_then_rewind_flushes_without_bad_prefix() {
    let guard = test_home_guard();
    let session_id = saved_one_row_session(&guard);

    let mut resumed = TestApp::builder().build_without_test_home_reset(&guard);
    resumed.load_session_by_id(&session_id);
    resumed.session_append_history(HistoryItem::user(Content::text("save before rewind")));
    resumed.save_session();
    assert!(resumed.session_document_has_unflushed_work());

    resumed.rewind_to_start();
    resumed.save_session_and_flush();

    assert!(
        resumed.overlays_probe().notification().is_none(),
        "rewind after an in-flight live save should not surface a save error"
    );
    let loaded = loaded_session(&resumed, &session_id);
    assert!(loaded.history.is_empty());
}

#[test]
fn repeated_read_only_resumes_do_not_modify_writer_session() {
    let guard = test_home_guard();
    let mut writer = TestApp::builder().build_with_test_home_guard(&guard);
    writer.session_append_history(HistoryItem::user(Content::text("writer row")));
    writer.save_session_and_flush();

    let session_id = writer.session_snapshot().id.clone();
    let db_path = smelt_core::session::dir_for_id(&session_id).join("session.db");
    let before = smelt_store::SessionReader::open_database(&db_path)
        .unwrap()
        .stored_session()
        .unwrap()
        .expect("session state before readonly loops");
    for idx in 0..5 {
        let mut reader = TestApp::builder().build_without_test_home_reset(&guard);
        reader.load_session_by_id(&session_id);
        assert!(
            reader.session_is_read_only(),
            "reader {idx} should be read-only"
        );
        assert_eq!(reader.session_message_count(), 1);
        let result = reader.apply_history_append_to_history(&HistoryAppend::append(
            HistoryItem::user(Content::text(format!("ignored reader row {idx}"))),
        ));
        assert_eq!(result, HistoryAppendResult::Unchanged);
        reader.save_session_and_flush();
    }

    let after = smelt_store::SessionReader::open_database(&db_path)
        .unwrap()
        .stored_session()
        .unwrap()
        .expect("session state after readonly loops");
    assert_eq!(after.head.revision, before.head.revision);
    assert_eq!(after.head.history_len, before.head.history_len);
    assert_eq!(
        smelt_store::SessionReader::open_database(&db_path)
            .unwrap()
            .history_item_count()
            .unwrap(),
        1
    );
}
