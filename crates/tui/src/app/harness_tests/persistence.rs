use super::*;
use crate::persist::{PersistFailure, PersistSaveKind};
use protocol::{
    AssistantStep, Content, EngineEvent, HistoryAppend, HistoryAppendResult, HistoryItem, Role,
    TokenUsage, ToolInvocation, ToolOutcome,
};
use smelt_core::transcript_model::Block;

fn loaded_session(id: &str) -> smelt_core::session::Session {
    crate::app::history::materialize_full_session(
        id,
        crate::app::history::FullSessionMaterializationReason::TestSavedSessionAssertion,
    )
    .expect("session saved")
}

fn save_receipt(
    save_id: u64,
    session_id: &str,
    history_len: usize,
    descriptor_len: usize,
    revision: u64,
) -> smelt_store::SaveReceipt {
    smelt_store::SaveReceipt {
        session_id: session_id.to_string(),
        save_id: smelt_store::SaveId::new(save_id),
        previous_revision: smelt_store::Revision::new(revision.saturating_sub(1)),
        revision: smelt_store::Revision::new(revision),
        history_len: smelt_store::HistoryLen::new(history_len as u64),
        descriptor_len: smelt_store::DescriptorLen::new(descriptor_len as u64),
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

fn saved_one_row_session(guard: &std::sync::MutexGuard<'static, ()>) -> String {
    let mut app = TestApp::builder().build_with_test_home_guard(guard);
    app.app
        .session_append_history(HistoryItem::user(Content::text("persisted before resume")));
    app.app.save_session_and_flush();
    app.app.core.session.id.clone()
}

#[test]
fn metadata_only_title_update_persists_after_clean_history_save() {
    let guard = test_home_guard();
    let mut app = TestApp::builder().build_with_test_home_guard(&guard);
    let session_id = app.app.core.session.id.clone();

    app.app
        .session_append_history(HistoryItem::user(Content::text("persisted history")));
    app.app.save_session_and_flush();
    app.app
        .set_session_title("Renamed session".into(), "renamed-session".into(), None);
    app.app.save_session_and_flush();

    let loaded = loaded_session(&session_id);
    assert_eq!(loaded.title.as_deref(), Some("Renamed session"));
    assert_eq!(loaded.slug.as_deref(), Some("renamed-session"));
    assert_eq!(loaded.history.len(), 1);
}

#[test]
fn shutdown_flushes_latest_generation_after_in_flight_save() {
    let guard = test_home_guard();
    let mut app = TestApp::builder().build_with_test_home_guard(&guard);

    app.app
        .session_append_history(HistoryItem::user(Content::text("first generation")));
    app.app.save_session();
    assert!(app.app.session_document.has_pending_save());

    app.app
        .session_append_history(HistoryItem::user(Content::text("final generation")));
    app.app.save_session();
    assert!(app.app.session_document.is_save_queued());

    app.app.save_session_and_flush();

    let loaded = loaded_session(&app.app.core.session.id);
    assert_eq!(loaded.history.len(), 2);
    assert!(matches!(
        loaded.history.last(),
        Some(HistoryItem::User { content, .. }) if content.text_content() == "final generation"
    ));
}

#[test]
fn shutdown_retries_after_transient_session_directory_failure() {
    let guard = test_home_guard();
    let mut app = TestApp::builder().build_with_test_home_guard(&guard);
    let session_id = app.app.core.session.id.clone();
    let session_dir = smelt_core::session::dir_for_id(&session_id);
    std::fs::create_dir_all(session_dir.parent().unwrap()).unwrap();
    std::fs::write(&session_dir, "temporarily blocks directory creation").unwrap();
    app.app
        .session_append_history(HistoryItem::user(Content::text("retry me")));

    app.app.save_session_and_flush();
    assert!(app.app.session_document_has_unflushed_work());
    assert!(app.app.notification.is_some());

    std::fs::remove_file(&session_dir).unwrap();
    app.app.save_session_and_flush();
    let loaded = loaded_session(&session_id);
    assert_eq!(loaded.history.len(), 1);
}

#[test]
fn lua_delete_returns_actionable_error_for_malicious_id() {
    let guard = test_home_guard();
    let mut app = TestApp::builder().build_with_test_home_guard(&guard);
    let target = engine::state_dir().join("must-not-delete");
    std::fs::create_dir_all(&target).unwrap();
    app.app
        .lua
        .lua
        .globals()
        .set("DELETE_TARGET", target.to_string_lossy().as_ref())
        .unwrap();

    let (ok, message) = {
        let _app_guard = crate::lua::install_app_ptr(&mut app.app);
        app.app
            .lua
            .lua
            .load(
                "local ok, err = pcall(function() smelt.session.delete(DELETE_TARGET) end); return ok, tostring(err)",
            )
            .eval::<(bool, String)>()
            .unwrap()
    };

    assert!(!ok);
    assert!(message.contains("invalid session id"), "{message}");
    assert!(target.exists());
}

#[test]
fn shutdown_keeps_permanent_storage_failure_visible_and_dirty() {
    let guard = test_home_guard();
    let mut app = TestApp::builder().build_with_test_home_guard(&guard);
    let session_id = app.app.core.session.id.clone();
    let session_dir = smelt_core::session::dir_for_id(&session_id);
    std::fs::create_dir_all(session_dir.parent().unwrap()).unwrap();
    std::fs::write(&session_dir, "permanently blocks directory creation").unwrap();
    app.app
        .session_append_history(HistoryItem::user(Content::text("cannot save")));

    app.app.save_session_and_flush();

    assert!(app.app.session_document_has_unflushed_work());
    assert!(app.app.notification.is_some());
    assert!(!session_dir.join("session.db").exists());
}

#[cfg(unix)]
#[test]
fn failed_sparse_fork_currently_leaves_a_published_destination_database() {
    use std::os::unix::fs::PermissionsExt;

    // Privileged processes can read mode-000 files, so this failure injection
    // is meaningful only for the unprivileged environment smelt supports.
    if unsafe { libc::geteuid() } == 0 {
        return;
    }

    let guard = test_home_guard();
    let session_id = {
        let mut app = TestApp::builder().build_with_test_home_guard(&guard);
        app.app
            .session_append_history(HistoryItem::user(Content::text("fork source")));
        app.app.save_session_and_flush();
        app.app.core.session.id.clone()
    };
    let mut resumed = TestApp::builder().build_without_test_home_reset(&guard);
    resumed.app.load_session_by_id(&session_id);
    let source_dir = smelt_core::session::dir_for_id(&session_id);
    let blob_dir = source_dir.join("blobs");
    std::fs::create_dir_all(&blob_dir).unwrap();
    let unreadable = blob_dir.join("unreadable.png");
    std::fs::write(&unreadable, "private attachment").unwrap();
    std::fs::set_permissions(&unreadable, std::fs::Permissions::from_mode(0o0)).unwrap();
    let sessions_dir = source_dir.parent().unwrap().to_path_buf();
    let before = std::fs::read_dir(&sessions_dir)
        .unwrap()
        .filter_map(|entry| entry.ok().map(|entry| entry.file_name()))
        .collect::<std::collections::HashSet<_>>();

    resumed.app.fork_session();

    std::fs::set_permissions(&unreadable, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(resumed.app.core.session.id, session_id);
    assert!(resumed.app.notification.is_some());
    let destinations = std::fs::read_dir(&sessions_dir)
        .unwrap()
        .filter_map(|entry| entry.ok())
        .filter(|entry| !before.contains(&entry.file_name()))
        .collect::<Vec<_>>();
    assert_eq!(
        destinations.len(),
        1,
        "failed fork should leave one partial destination"
    );
    assert!(destinations[0].path().join("session.db").is_file());
}

#[test]
fn shutdown_flushes_descriptor_only_transcript_blocks() {
    let guard = test_home_guard();
    let mut app = TestApp::builder().build_with_test_home_guard(&guard);
    let session_id = app.app.core.session.id.clone();

    app.app.push_block(Block::Thinking {
        title: None,
        summary_titles: Vec::new(),
        kind: protocol::ReasoningKind::Raw,
        content: "descriptor-only interrupted thinking".into(),
    });
    app.app.save_session_and_flush();

    let db = smelt_store::SessionReader::open_database(
        smelt_core::session::dir_for_id(&session_id).join("session.db"),
    )
    .unwrap();
    let rows = db.read_all_transcript_descriptor_records().unwrap();
    assert!(
        rows.iter().any(|row| row
            .preview_text
            .contains("descriptor-only interrupted thinking")),
        "descriptor-only transcript block should be durable: {rows:#?}"
    );
}

#[test]
fn sparse_descriptor_resume_interrupt_save_compacts_and_appends_again() {
    let guard = test_home_guard();
    let session_id = {
        let mut app = TestApp::builder().build_with_test_home_guard(&guard);
        app.app.commit_request_history_item(
            HistoryItem::user(Content::text("sparse descriptor prompt")),
            Some(Block::User {
                text: "sparse descriptor prompt".into(),
                image_labels: Vec::new(),
            }),
        );
        app.app.push_block(Block::Text {
            content: "persisted before sparse rewrite".into(),
        });
        app.app.save_session_and_flush();
        app.app.core.session.id.clone()
    };

    let db_path = smelt_core::session::dir_for_id(&session_id).join("session.db");
    let db = smelt_store::SessionDb::open(&db_path).unwrap();
    assert_eq!(db.transcript_descriptor_count().unwrap(), 2);
    db.connection()
        .execute(
            "UPDATE transcript_blocks SET descriptor_idx = 302 WHERE descriptor_idx = 1",
            [],
        )
        .unwrap();
    drop(db);

    let mut resumed = TestApp::builder().build_without_test_home_reset(&guard);
    resumed.app.load_session_by_id(&session_id);
    assert_eq!(
        resumed
            .app
            .session_document
            .transcript
            .descriptor_total_count(),
        Some(2)
    );
    resumed.start_turn(3030);
    resumed.feed_one(SourceEvent::engine(EngineEvent::TextDelta {
        delta: "interrupted after sparse descriptor load".into(),
    }));
    resumed.cancel();
    resumed.app.save_session_and_flush();
    assert!(
        resumed.app.notification.is_none(),
        "sparse descriptor save should not surface an integrity failure: {:?}",
        resumed.app.notification
    );
    drop(resumed);

    let db = smelt_store::SessionDb::open_read_only(&db_path).unwrap();
    let (count, min, max): (i64, Option<i64>, Option<i64>) = db
        .connection()
        .query_row(
            "SELECT COUNT(*), MIN(descriptor_idx), MAX(descriptor_idx)
             FROM transcript_blocks WHERE descriptor_json IS NOT NULL",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!((count, min, max), (3, Some(0), Some(2)));
    drop(db);

    let mut resumed = TestApp::builder().build_without_test_home_reset(&guard);
    resumed.app.load_session_by_id(&session_id);
    resumed.app.push_block(Block::Thinking {
        title: None,
        summary_titles: Vec::new(),
        content: "append after dense reconciliation".into(),
        kind: protocol::ReasoningKind::Raw,
    });
    resumed.app.save_session_and_flush();
    assert!(resumed.app.notification.is_none());

    let db = smelt_store::SessionReader::open_database(&db_path).unwrap();
    let rows = db.read_all_transcript_descriptor_records().unwrap();
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
        app.app
            .session_append_history(HistoryItem::user(Content::text("token identity prompt")));
        app.app.record_visible_token_usage(TokenUsage {
            context_tokens: Some(1234),
            ..Default::default()
        });
        app.app.save_session_and_flush();
        app.app.core.session.id.clone()
    };

    let mut resumed = TestApp::builder().build_without_test_home_reset(&guard);
    resumed.app.load_session_by_id(&session_id);
    let identity = resumed.app.active_context_token_identity();

    assert_eq!(
        resumed.app.core.session.display_context_tokens(),
        Some(1234)
    );
    assert!(!resumed
        .app
        .core
        .session
        .display_context_tokens_stale(&identity));
}

#[test]
fn interrupted_turn_rewind_save_resume_restores_prior_context_tokens() {
    let guard = test_home_guard();
    let session_id = {
        let mut app = TestApp::builder().build_with_test_home_guard(&guard);
        app.app.commit_request_history_item(
            HistoryItem::user(Content::text("first prompt")),
            Some(Block::User {
                text: "first prompt".into(),
                image_labels: Vec::new(),
            }),
        );
        app.app.commit_request_history_item(
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
        app.app.save_session_and_flush();

        let second_prompt_block_idx = app.app.session_document.transcript.history().len();
        app.app.commit_request_history_item(
            HistoryItem::user(Content::text("second prompt")),
            Some(Block::User {
                text: "second prompt".into(),
                image_labels: Vec::new(),
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

        app.app
            .rewind_to_block(Some(second_prompt_block_idx), false);
        app.app.save_session_and_flush();
        assert_eq!(app.app.session_history_len(), 2);
        assert_eq!(app.app.core.session.display_context_tokens(), Some(100));
        app.app.core.session.id.clone()
    };

    let loaded = loaded_session(&session_id);
    assert_eq!(loaded.history.len(), 2);
    assert_eq!(loaded.current_context_tokens(), Some(100));
    assert_eq!(loaded.display_context_tokens(), Some(100));

    let mut resumed = TestApp::builder().build_without_test_home_reset(&guard);
    resumed.app.load_session_by_id(&session_id);
    assert_eq!(resumed.app.session_history_len(), 2);
    assert_eq!(resumed.app.core.session.display_context_tokens(), Some(100));
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
                if app.app.agent.is_some() {
                    app.app.finish_turn(crate::app::TurnEnd::Cancelled);
                }
            }
        }
        app.app.save_session_and_flush();

        let loaded = loaded_session(&app.app.core.session.id);
        assert_committed_tool_invocation(&loaded.history);
    }
}

#[test]
fn store_backed_resume_restores_tool_calls_for_model_history() {
    let guard = test_home_guard();
    let session_id = {
        let mut app = TestApp::builder().build_with_test_home_guard(&guard);
        for item in tool_history() {
            app.app.session_append_history(item);
        }
        app.app.save_session_and_flush();
        app.app.core.session.id.clone()
    };

    let mut resumed = TestApp::builder().build_without_test_home_reset(&guard);
    resumed.app.load_session_by_id(&session_id);

    assert_eq!(resumed.app.core.session.id, session_id);
    assert!(
        resumed.app.core.session.history.is_empty(),
        "resume should use the production sparse SQLite session path"
    );
    assert_eq!(resumed.app.session_history_len(), 2);

    let stored_history = resumed
        .app
        .session_history_range(0..resumed.app.session_history_len());
    assert_committed_tool_invocation(&stored_history);
    assert_model_history_tool_messages(&resumed.app.model_history_messages());
}

#[test]
fn store_backed_resume_tolerates_bad_checkpoint_without_repairing_database() {
    let guard = test_home_guard();
    let session_id = {
        let mut app = TestApp::builder().build_with_test_home_guard(&guard);
        app.app
            .session_append_history(HistoryItem::user(Content::text("old prompt")));
        app.app
            .session_append_history(HistoryItem::assistant(AssistantStep::terminal(
                Some(Content::text("recent reply")),
                None,
                Vec::new(),
            )));
        app.app.save_session_and_flush();
        app.app.core.session.id.clone()
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
    resumed.app.load_session_by_id(&session_id);

    assert_eq!(resumed.app.core.session.id, session_id);
    assert!(resumed.app.core.session.history.is_empty());
    let checkpoint = resumed
        .app
        .core
        .session
        .checkpoint
        .as_ref()
        .expect("checkpoint tolerated on sparse resume");
    assert_eq!(checkpoint.first_live_index, 0);

    let history = resumed.app.model_history();
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
        .session_state()
        .unwrap()
        .unwrap()
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
            app.app.session_append_history(item);
        }
        app.app.save_session_and_flush();
        app.app.core.session.id.clone()
    };

    let mut resumed = TestApp::builder().build_without_test_home_reset(&guard);
    resumed.app.load_session_by_id(&session_id);
    resumed
        .app
        .session_append_history(HistoryItem::user(Content::text("continue after resume")));
    resumed.app.save_session_and_flush();

    let loaded = loaded_session(&session_id);
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
            app.app.session_append_history(item);
        }
        app.app.save_session_and_flush();
        app.app.core.session.id.clone()
    };

    for cycle in 0..4 {
        let mut resumed = TestApp::builder().build_without_test_home_reset(&guard);
        resumed.app.load_session_by_id(&session_id);
        assert_eq!(resumed.app.core.session.id, session_id);
        assert!(resumed.app.session_document.live_session.is_some());
        resumed
            .app
            .session_append_history(HistoryItem::user(Content::text(format!("cycle {cycle}"))));
        resumed.app.save_session_and_flush();
    }

    let loaded = loaded_session(&session_id);
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
    writer
        .app
        .session_append_history(HistoryItem::user(Content::text("owned history")));
    writer.app.save_session_and_flush();

    let session_id = writer.app.core.session.id.clone();
    let session_dir = smelt_core::session::dir_for_id(&session_id);
    let db_path = session_dir.join("session.db");
    let before = smelt_store::SessionReader::open_database(&db_path)
        .unwrap()
        .session_state()
        .unwrap()
        .expect("session state before read-only resume");
    let mut reader = TestApp::builder().build_without_test_home_reset(&guard);
    reader.app.load_session_by_id(&session_id);

    assert_eq!(reader.app.core.session.id, session_id);
    assert!(reader.app.session_is_read_only());
    assert_eq!(reader.app.session_history_len(), 1);

    let result = reader
        .app
        .apply_history_append_to_history(&HistoryAppend::append(HistoryItem::user(Content::text(
            "read-only mutation",
        ))));
    assert_eq!(result, HistoryAppendResult::Unchanged);
    reader.app.save_session_and_flush();

    let after = smelt_store::SessionReader::open_database(&db_path)
        .unwrap()
        .session_state()
        .unwrap()
        .expect("session state after read-only resume");
    assert_eq!(after.revision, before.revision);
    assert_eq!(after.history_len, before.history_len);
    assert_eq!(
        smelt_store::SessionReader::open_database(&db_path)
            .unwrap()
            .history_item_count()
            .unwrap(),
        1
    );
}

fn fake_pending_history_save(app: &mut TestApp, save_id: u64, history_len: usize) {
    let generation = app.app.session_document.current_generation_for_test();
    app.app.session_document.set_pending_save_for_test(
        save_id,
        app.app.core.session.id.clone(),
        PersistSaveKind::History,
        generation,
        history_len,
    );
}

#[test]
fn stale_live_save_ack_does_not_drop_later_live_history() {
    let guard = test_home_guard();
    let session_id = saved_one_row_session(&guard);

    let mut resumed = TestApp::builder().build_without_test_home_reset(&guard);
    resumed.app.load_session_by_id(&session_id);
    assert!(resumed.app.session_document.live_session.is_some());
    let before_len = resumed.app.session_history_len();
    fake_pending_history_save(&mut resumed, 700, before_len);

    resumed
        .app
        .session_append_history(HistoryItem::user(Content::text("appended after stale ack")));
    assert_eq!(resumed.app.session_history_len(), before_len + 1);

    resumed
        .app
        .ack_persist_save(save_receipt(700, &session_id, before_len, before_len, 7));

    assert_eq!(
        resumed.app.session_history_len(),
        before_len + 1,
        "stale ack must not drop live history appended after the save began"
    );
    resumed.app.save_session_and_flush();

    let loaded = loaded_session(&session_id);
    assert_eq!(loaded.history.len(), before_len + 1);
    assert!(matches!(
        loaded.history.last(),
        Some(HistoryItem::User { content, .. }) if content.text_content() == "appended after stale ack"
    ));
}

#[test]
fn stale_live_save_ack_does_not_drop_later_transcript_blocks() {
    let guard = test_home_guard();
    let session_id = saved_one_row_session(&guard);

    let mut resumed = TestApp::builder().build_without_test_home_reset(&guard);
    resumed.app.load_session_by_id(&session_id);
    assert!(resumed.app.session_document.live_session.is_some());
    let before_len = resumed.app.session_history_len();
    fake_pending_history_save(&mut resumed, 701, before_len);

    resumed.app.push_block(Block::Thinking {
        title: None,
        summary_titles: Vec::new(),
        kind: protocol::ReasoningKind::Raw,
        content: "late thinking block".into(),
    });
    assert!(resumed
        .app
        .session_document
        .transcript
        .history()
        .descriptor_dirty_from()
        .is_some());

    resumed
        .app
        .ack_persist_save(save_receipt(701, &session_id, before_len, before_len, 7));

    assert!(
        resumed
            .app
            .session_document
            .transcript
            .history()
            .descriptor_dirty_from()
            .is_some(),
        "stale ack must not clear transcript blocks appended after the save began"
    );
    resumed.app.save_session_and_flush();

    let db = smelt_store::SessionReader::open_database(
        smelt_core::session::dir_for_id(&session_id).join("session.db"),
    )
    .unwrap();
    let rows = db.read_all_transcript_descriptor_records().unwrap();
    assert!(
        rows.iter()
            .any(|row| row.preview_text.contains("late thinking block")),
        "persisted transcript descriptors should contain the interrupted thinking block: {rows:#?}"
    );
}

#[test]
fn stale_live_save_ack_does_not_drop_later_streaming_text() {
    let guard = test_home_guard();
    let session_id = saved_one_row_session(&guard);

    let mut resumed = TestApp::builder().build_without_test_home_reset(&guard);
    resumed.app.load_session_by_id(&session_id);
    assert!(resumed.app.session_document.live_session.is_some());
    let before_len = resumed.app.session_history_len();
    fake_pending_history_save(&mut resumed, 703, before_len);

    resumed.start_turn(7030);
    resumed.feed_one(SourceEvent::engine(EngineEvent::TextDelta {
        delta: "late streaming text before interrupt".into(),
    }));
    assert!(resumed
        .app
        .session_document
        .transcript
        .history()
        .descriptor_dirty_from()
        .is_some());

    resumed
        .app
        .ack_persist_save(save_receipt(703, &session_id, before_len, before_len, 7));

    assert!(
        resumed
            .app
            .session_document
            .transcript
            .history()
            .descriptor_dirty_from()
            .is_some(),
        "stale ack must not clear streaming text appended after the save began"
    );
    resumed.cancel();
    resumed.app.save_session_and_flush();

    let db = smelt_store::SessionReader::open_database(
        smelt_core::session::dir_for_id(&session_id).join("session.db"),
    )
    .unwrap();
    let rows = db.read_all_transcript_descriptor_records().unwrap();
    assert!(
        rows.iter().any(|row| row
            .preview_text
            .contains("late streaming text before interrupt")),
        "persisted transcript descriptors should contain interrupted streaming text: {rows:#?}"
    );
}

#[test]
fn stale_live_save_ack_does_not_drop_later_tool_blocks() {
    let guard = test_home_guard();
    let session_id = saved_one_row_session(&guard);

    let mut resumed = TestApp::builder().build_without_test_home_reset(&guard);
    resumed.app.load_session_by_id(&session_id);
    let before_len = resumed.app.session_history_len();
    fake_pending_history_save(&mut resumed, 702, before_len);

    resumed.start_turn(7020);
    resumed.feed_one(SourceEvent::engine(EngineEvent::ToolStarted {
        call_id: "call-late-tool".into(),
        tool_name: "bash".into(),
        args: std::collections::HashMap::new(),
    }));
    resumed.feed_one(SourceEvent::engine(EngineEvent::ToolOutput {
        call_id: "call-late-tool".into(),
        chunk: "tool output after stale ack\n".into(),
    }));
    resumed.feed_one(SourceEvent::engine(EngineEvent::ToolFinished {
        call_id: "call-late-tool".into(),
        result: ToolOutcome {
            content: "tool output after stale ack\n".into(),
            is_error: false,
            metadata: None,
        },
        elapsed_ms: Some(5),
    }));

    resumed
        .app
        .ack_persist_save(save_receipt(702, &session_id, before_len, before_len, 7));
    assert!(
        resumed
            .app
            .session_document
            .transcript
            .history()
            .descriptor_dirty_from()
            .is_some(),
        "stale ack must not clear tool blocks appended after the save began"
    );

    resumed.cancel();
    resumed.app.save_session_and_flush();

    let db = smelt_store::SessionReader::open_database(
        smelt_core::session::dir_for_id(&session_id).join("session.db"),
    )
    .unwrap();
    let rows = db.read_all_transcript_descriptor_records().unwrap();
    assert!(
        rows.iter()
            .any(|row| row.tool_name.as_deref() == Some("bash"))
            || rows
                .iter()
                .any(|row| row.preview_text.contains("tool output after stale ack")),
        "persisted transcript descriptors should contain the interrupted tool block: {rows:#?}"
    );
}

#[test]
fn live_save_failure_forces_full_retry_instead_of_repeating_bad_suffix() {
    let guard = test_home_guard();
    let session_id = saved_one_row_session(&guard);

    let mut resumed = TestApp::builder().build_without_test_home_reset(&guard);
    resumed.app.load_session_by_id(&session_id);
    let before_len = resumed.app.session_history_len();
    resumed
        .app
        .session_append_history(HistoryItem::user(Content::text("recover after failure")));

    resumed.app.fail_persist_save(PersistFailure {
        save_id: 900,
        session_id: session_id.clone(),
        message: "save session database: integrity error: history unchanged prefix exceeds stored rows: prefix 2, stored 1".into(),
        commit_failure: None,
    });
    assert_eq!(
        resumed.app.session_document.dirty_history_from_for_test(),
        Some(0),
        "a live-session save failure should force the retry to start from the beginning"
    );
    assert_eq!(
        resumed.app.session_document.durable_history_len_for_test(),
        before_len,
        "save failure must not make the durable cursor forget stored rows"
    );

    resumed.app.save_session_and_flush();
    let loaded = loaded_session(&session_id);
    assert_eq!(loaded.history.len(), before_len + 1);
    assert!(matches!(
        loaded.history.last(),
        Some(HistoryItem::User { content, .. }) if content.text_content() == "recover after failure"
    ));
}

#[test]
fn recoverable_stale_persist_failure_retries_without_sticky_notification() {
    let guard = test_home_guard();
    let session_id = saved_one_row_session(&guard);

    let mut resumed = TestApp::builder().build_without_test_home_reset(&guard);
    resumed.app.load_session_by_id(&session_id);
    let before_len = resumed.app.session_history_len();
    resumed
        .app
        .session_append_history(HistoryItem::user(Content::text(
            "recover after stale descriptor",
        )));
    fake_pending_history_save(&mut resumed, 901, before_len);

    resumed.app.fail_persist_save(PersistFailure {
        save_id: 901,
        session_id: session_id.clone(),
        message: "save session database: stale descriptor base: base 303, current 1".into(),
        commit_failure: Some(smelt_store::SessionCommitFailure::StaleDescriptorBase {
            base: smelt_store::DescriptorLen::new(303),
            current: smelt_store::DescriptorLen::new(before_len as u64),
        }),
    });

    assert!(
        resumed.app.notification.is_none(),
        "recoverable stale commit failures should retry without sticky user-facing errors"
    );
    resumed.app.save_session_and_flush();

    let loaded = loaded_session(&session_id);
    assert_eq!(loaded.history.len(), before_len + 1);
    assert!(matches!(
        loaded.history.last(),
        Some(HistoryItem::User { content, .. }) if content.text_content() == "recover after stale descriptor"
    ));
}

#[test]
fn ownership_loss_moves_session_to_read_only_and_releases_lock() {
    let guard = test_home_guard();
    let session_id = saved_one_row_session(&guard);
    let mut resumed = TestApp::builder().build_without_test_home_reset(&guard);
    resumed.app.load_session_by_id(&session_id);
    assert!(!resumed.app.session_is_read_only());

    resumed.app.fail_persist_save(PersistFailure {
        save_id: 902,
        session_id: session_id.clone(),
        message: "session writer ownership was lost".into(),
        commit_failure: Some(smelt_store::SessionCommitFailure::OwnershipLost),
    });

    assert!(resumed.app.session_is_read_only());
    let session_dir = smelt_core::session::dir_for_id(&session_id);
    let replacement = smelt_store::OwnedSessionWriter::open(&session_dir, &session_id)
        .expect("ownership loss releases the lifetime lock");
    drop(replacement);
}

#[test]
fn live_save_restarts_at_stored_prefix_when_dirty_marker_skips_missing_row() {
    let guard = test_home_guard();
    let session_id = saved_one_row_session(&guard);

    let mut resumed = TestApp::builder().build_without_test_home_reset(&guard);
    resumed.app.load_session_by_id(&session_id);
    let stored_len = resumed.app.session_history_len();
    assert_eq!(stored_len, 1);

    resumed
        .app
        .session_append_history(HistoryItem::user(Content::text("kept live row")));
    assert!(resumed.app.session_document.live_session.is_some());
    resumed
        .app
        .session_document
        .set_history_resave_from_for_test(stored_len + 1);

    resumed.app.save_session();
    resumed.app.flush_persist();

    assert!(
        resumed.app.notification.is_none(),
        "save should not surface a prefix-exceeds-stored integrity error"
    );
    let live = resumed
        .app
        .session_document
        .live_session
        .as_ref()
        .expect("store-backed session");
    assert_eq!(live.live_suffix_len(), 0);

    let loaded = loaded_session(&session_id);
    assert_eq!(loaded.history.len(), stored_len + 1);
    assert!(matches!(
        loaded.history.last(),
        Some(HistoryItem::User { content, .. }) if content.text_content() == "kept live row"
    ));
}

#[test]
fn pre_request_compaction_append_save_resume_keeps_canonical_history() {
    let guard = test_home_guard();
    let mut app = TestApp::builder().build_with_test_home_guard(&guard);
    for idx in 0..24 {
        app.app
            .session_append_history(HistoryItem::user(Content::text(format!("row {idx}"))));
    }
    let compacted_prefix_len = app.app.session_history_len();
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
    app.app.save_session_and_flush();

    let mut settings = app.app.core.config.settings.clone();
    settings.auto_compact = true;
    settings.compact_threshold = 0.8;
    settings.compact_keep_recent_groups = 1.0;
    app.app.set_settings(settings);
    app.app.core.config.context_window = Some(100);
    app.app.commit_request_history_item(
        HistoryItem::user(Content::text("request after compaction")),
        Some(Block::User {
            text: "request after compaction".into(),
            image_labels: Vec::new(),
        }),
    );
    app.start_turn(42);

    let messages = protocol::history_to_messages(&app.app.model_history());
    let (tx, mut rx) = tokio::sync::oneshot::channel();
    {
        let _guard = crate::lua::install_app_ptr(&mut app.app);
        app.app
            .dispatch_host_call(engine::HostCall::PrepareRequest {
                messages,
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
        let _guard = crate::lua::install_app_ptr(&mut app.app);
        app.app
            .dispatch_engine_event(EngineEvent::EngineAskResponse {
                id: ask_id,
                message: Some(protocol::Message::assistant(
                    Some(Content::text("# Goal\nretained summary")),
                    None,
                    None,
                )),
                error: None,
            });
        app.app.drive_lua_tasks();
    }

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
    app.app.save_session_and_flush();

    assert!(
        app.app.notification.is_none(),
        "compacted append should save without a side-table bounds error: {:?}",
        app.app.notification
    );
    assert_eq!(app.app.session_history_len(), compacted_prefix_len + 2);
    let session_id = app.app.core.session.id.clone();
    let loaded = loaded_session(&session_id);
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
    resumed.app.load_session_by_id(&session_id);
    assert_eq!(resumed.app.session_history_len(), compacted_prefix_len + 2);
    let resumed_history = resumed
        .app
        .session_history_range(0..resumed.app.session_history_len());
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
            app.app
                .session_append_history(HistoryItem::user(Content::text(format!("row {idx}"))));
        }
        app.app
            .session_set_checkpoint(Some(smelt_core::ContextCheckpoint {
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
        app.app.save_session_and_flush();
        app.app.core.session.id.clone()
    };

    let mut resumed = TestApp::builder().build_without_test_home_reset(&guard);
    resumed.app.load_session_by_id(&session_id);
    assert_eq!(
        resumed
            .app
            .core
            .session
            .checkpoint
            .as_ref()
            .map(|checkpoint| checkpoint.first_live_index),
        Some(3)
    );
    let identity = resumed.app.active_context_token_identity();
    resumed.app.apply_session_document_mutation(
        crate::app::session_document::SessionMutation::RewindHistoryTo {
            index: 1,
            keep_checkpoint_at_boundary: false,
            identity,
        },
    );
    assert!(resumed.app.core.session.checkpoint.is_none());

    resumed
        .app
        .session_append_history(HistoryItem::user(Content::text("new after rewind")));
    resumed.app.save_session_and_flush();

    assert!(
        resumed.app.notification.is_none(),
        "save after rewind should not surface a checkpoint/history integrity error: {:?}",
        resumed.app.notification
    );
}

#[test]
fn in_flight_live_save_then_rewind_flushes_without_bad_prefix() {
    let guard = test_home_guard();
    let session_id = saved_one_row_session(&guard);

    let mut resumed = TestApp::builder().build_without_test_home_reset(&guard);
    resumed.app.load_session_by_id(&session_id);
    resumed
        .app
        .session_append_history(HistoryItem::user(Content::text("save before rewind")));
    resumed.app.save_session();
    assert!(resumed.app.session_document.has_pending_save());

    resumed.app.rewind_to_start();
    resumed.app.save_session_and_flush();

    assert!(
        resumed.app.notification.is_none(),
        "rewind after an in-flight live save should not surface a save error"
    );
    let loaded = loaded_session(&session_id);
    assert!(loaded.history.is_empty());
}

#[test]
fn repeated_read_only_resumes_do_not_modify_writer_session() {
    let guard = test_home_guard();
    let mut writer = TestApp::builder().build_with_test_home_guard(&guard);
    writer
        .app
        .session_append_history(HistoryItem::user(Content::text("writer row")));
    writer.app.save_session_and_flush();

    let session_id = writer.app.core.session.id.clone();
    let db_path = smelt_core::session::dir_for_id(&session_id).join("session.db");
    let before = smelt_store::SessionReader::open_database(&db_path)
        .unwrap()
        .session_state()
        .unwrap()
        .expect("session state before readonly loops");
    for idx in 0..5 {
        let mut reader = TestApp::builder().build_without_test_home_reset(&guard);
        reader.app.load_session_by_id(&session_id);
        assert!(
            reader.app.session_is_read_only(),
            "reader {idx} should be read-only"
        );
        assert_eq!(reader.app.session_history_len(), 1);
        let result = reader
            .app
            .apply_history_append_to_history(&HistoryAppend::append(HistoryItem::user(
                Content::text(format!("ignored reader row {idx}")),
            )));
        assert_eq!(result, HistoryAppendResult::Unchanged);
        reader.app.save_session_and_flush();
    }

    let after = smelt_store::SessionReader::open_database(&db_path)
        .unwrap()
        .session_state()
        .unwrap()
        .expect("session state after readonly loops");
    assert_eq!(after.revision, before.revision);
    assert_eq!(after.history_len, before.history_len);
    assert_eq!(
        smelt_store::SessionReader::open_database(&db_path)
            .unwrap()
            .history_item_count()
            .unwrap(),
        1
    );
}
