use super::*;
use crate::{EngineConfig, EngineOutput, HostCall, HostCallbacks};
use protocol::{ModelTarget, ReasoningEffort};
use serial_test::serial;
use std::path::Path;
use std::sync::Arc;

struct Fixture {
    root: tempfile::TempDir,
    previous_executable: Option<std::ffi::OsString>,
    previous_scenario: Option<std::ffi::OsString>,
    previous_log: Option<std::ffi::OsString>,
    previous_cache: Option<std::ffi::OsString>,
}

impl Fixture {
    fn new(scenario: &str) -> Self {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let executable = root.path().join("claude");
        std::fs::write(
            &executable,
            include_str!("../../../../tests/fixtures/claude_code.py"),
        )
        .unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let fixture = Self {
            root,
            previous_executable: std::env::var_os("SMELT_CLAUDE_CODE_EXECUTABLE"),
            previous_scenario: std::env::var_os("SMELT_CLAUDE_FIXTURE"),
            previous_log: std::env::var_os("SMELT_CLAUDE_FIXTURE_LOG"),
            previous_cache: std::env::var_os("XDG_CACHE_HOME"),
        };
        std::env::set_var("SMELT_CLAUDE_CODE_EXECUTABLE", executable);
        std::env::set_var("SMELT_CLAUDE_FIXTURE", scenario);
        std::env::set_var(
            "SMELT_CLAUDE_FIXTURE_LOG",
            fixture.root.path().join("calls.jsonl"),
        );
        std::env::set_var("XDG_CACHE_HOME", fixture.root.path().join("cache"));
        fixture
    }

    fn handle(&self) -> crate::EngineHandle {
        let mut config =
            EngineConfig::new(self.root.path().into(), Arc::new(crate::clock::RealClock));
        config.host_callbacks = HostCallbacks::Enabled;
        crate::start(config, Box::new(crate::tools::EmptyDispatcher))
    }

    fn calls(&self) -> Vec<Value> {
        std::fs::read_to_string(self.root.path().join("calls.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    /// Waits for Claude Code to log a call that matches, which it does on its own time.
    async fn wait_for(&self, expected: &str, matches: impl Fn(&Value) -> bool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !self.calls().iter().any(&matches) {
            assert!(std::time::Instant::now() < deadline, "{expected}");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for (key, value) in [
            (
                "SMELT_CLAUDE_CODE_EXECUTABLE",
                self.previous_executable.take(),
            ),
            ("SMELT_CLAUDE_FIXTURE", self.previous_scenario.take()),
            ("SMELT_CLAUDE_FIXTURE_LOG", self.previous_log.take()),
            ("XDG_CACHE_HOME", self.previous_cache.take()),
        ] {
            if let Some(value) = value {
                std::env::set_var(key, value);
            } else {
                std::env::remove_var(key);
            }
        }
    }
}

fn payload(root: &Path, turn: u64, history: Vec<HistoryItem>) -> UiCommand {
    UiCommand::StartTurn(Box::new(StartTurnPayload {
        turn_id: turn,
        input: StartTurnInput::user(Content::text(format!("turn {turn}"))),
        mode: protocol::AgentMode::normal(),
        model_target: ModelTarget {
            model: "sonnet".into(),
            provider_type: "claude-code".into(),
            provider: "claude-code".into(),
            api_base: String::new(),
            api_key: String::new(),
            config: Default::default(),
        },
        request_config: Default::default(),
        reasoning_effort: ReasoningEffort::Off,
        fast_mode: false,
        history: protocol::ModelHistorySource::items(history),
        session_id: "test-session".into(),
        sessions_root: root.join("sessions"),
        ephemeral: false,
        persistence: Default::default(),
        permission_overrides: None,
        system_prompt: None,
        native_prompt: None,
        tool_visibility: protocol::ToolVisibility::Interactive,
        tools: Vec::new(),
    }))
}

async fn output(handle: &mut crate::EngineHandle) -> EngineOutput {
    let output = tokio::time::timeout(Duration::from_secs(8), handle.recv_output())
        .await
        .expect("engine output timed out")
        .expect("engine disconnected");
    match output {
        EngineOutput::Event(EngineEvent::NativeSessionEvent { event, .. }) => {
            EngineOutput::Event(*event)
        }
        output => output,
    }
}

async fn finish(
    handle: &mut crate::EngineHandle,
    history: &mut Vec<HistoryItem>,
) -> protocol::TurnMeta {
    loop {
        match output(handle).await {
            EngineOutput::Event(EngineEvent::HistoryAppended { delta, .. })
            | EngineOutput::Event(EngineEvent::Steered { delta, .. }) => {
                assert_eq!(delta.first_index.get(), history.len());
                history.extend(delta.items);
            }
            EngineOutput::Event(EngineEvent::TurnComplete { meta, .. }) => return meta.unwrap(),
            EngineOutput::Event(EngineEvent::TurnError { message, .. }) => panic!("{message}"),
            EngineOutput::HostCall(crate::HostCall::NativeToolEvaluation { reply, .. }) => {
                let _ = reply.send(ToolEvaluation {
                    decision: Decision::Allow,
                    metadata: Default::default(),
                });
            }
            _ => {}
        }
    }
}

#[tokio::test]
#[serial(claude_code)]
async fn stopping_one_native_child_preserves_sibling_and_parent() {
    let fixture = Fixture::new("subagent-stop");
    let mut handle = fixture.handle();
    handle.send(payload(fixture.root.path(), 1, Vec::new()));
    let mut stopped = false;
    let mut sibling_completed = false;
    let mut sibling_output = false;
    loop {
        match output(&mut handle).await {
            EngineOutput::Event(EngineEvent::AgentUpdated { agent, .. }) => {
                if agent.id == "claude-code:two" && agent.status == protocol::AgentStatus::Running {
                    handle.send(UiCommand::StopAgent {
                        session_id: "other-session".into(),
                        agent_id: "claude-code:one".into(),
                    });
                    handle.send(UiCommand::StopAgent {
                        session_id: "test-session".into(),
                        agent_id: "claude-code:one".into(),
                    });
                }
                stopped |= agent.id == "claude-code:one"
                    && agent.status == protocol::AgentStatus::Cancelled;
                sibling_completed |= agent.id == "claude-code:two"
                    && agent.status == protocol::AgentStatus::Completed;
            }
            EngineOutput::Event(EngineEvent::AgentOutput {
                event, agent_id, ..
            }) if agent_id == "claude-code:two" => {
                sibling_output |= matches!(event.as_ref(), EngineEvent::TextDelta { delta } if delta == "CHILD_SIBLING_REPORT");
            }
            EngineOutput::Event(EngineEvent::HistoryAppended { delta, .. }) => {
                assert!(!serde_json::to_string(&delta.items)
                    .unwrap()
                    .contains("CHILD_SIBLING_REPORT"));
            }
            EngineOutput::HostCall(HostCall::NativeToolEvaluation { reply, .. }) => {
                let _ = reply.send(ToolEvaluation {
                    decision: Decision::Allow,
                    metadata: Default::default(),
                });
            }
            EngineOutput::Event(EngineEvent::TurnComplete { meta, .. }) => {
                assert!(!meta.unwrap().interrupted);
                break;
            }
            EngineOutput::Event(EngineEvent::TurnError { message, .. }) => panic!("{message}"),
            _ => {}
        }
    }
    assert!(stopped && sibling_completed && sibling_output);
    let calls = fixture.calls();
    assert_eq!(
        calls
            .iter()
            .filter(|call| call["request"]["subtype"] == "stop_task")
            .count(),
        1
    );
    assert!(!calls
        .iter()
        .any(|call| call["request"]["subtype"] == "interrupt"));
}

#[tokio::test]
#[serial(claude_code)]
async fn foreground_child_isolated_transcript_and_policy_callbacks() {
    let fixture = Fixture::new("subagent-foreground");
    let mut handle = fixture.handle();
    handle.send(payload(fixture.root.path(), 1, Vec::new()));
    let mut child_history = Vec::new();
    let mut child_read = false;
    let mut evaluated = false;
    let mut completed = false;
    loop {
        match output(&mut handle).await {
            EngineOutput::Event(EngineEvent::HistoryAppended { delta, .. }) => {
                let text = serde_json::to_string(&delta.items).unwrap();
                assert!(!text.contains("CHILD_"));
                assert!(!text.contains("child-read"));
            }
            EngineOutput::Event(EngineEvent::AgentOutput {
                agent_id, event, ..
            }) => {
                assert_eq!(agent_id, "claude-code:child-task");
                match *event {
                    EngineEvent::HistoryAppended { delta, .. } => {
                        assert_eq!(delta.first_index.get(), child_history.len());
                        child_history.extend(delta.items);
                    }
                    EngineEvent::ToolFinished {
                        result, call_id, ..
                    } => {
                        assert_eq!(call_id, "child-read");
                        assert!(result.is_error);
                        assert_eq!(result.content, "CHILD_READ_DENIED");
                        child_read = true;
                    }
                    _ => {}
                }
            }
            EngineOutput::Event(EngineEvent::AgentUpdated { agent, .. }) => {
                assert_eq!(agent.owner, protocol::AgentOwner::ClaudeCode);
                completed |= agent.status == protocol::AgentStatus::Completed;
            }
            EngineOutput::HostCall(HostCall::NativeToolEvaluation {
                tool_name, reply, ..
            }) => {
                evaluated |= tool_name == "Read";
                let _ = reply.send(ToolEvaluation {
                    decision: if tool_name == "Read" {
                        Decision::Deny
                    } else {
                        Decision::Allow
                    },
                    metadata: Default::default(),
                });
            }
            EngineOutput::Event(EngineEvent::TurnComplete { .. }) => break,
            EngineOutput::Event(EngineEvent::TurnError { message, .. }) => panic!("{message}"),
            _ => {}
        }
    }
    assert!(completed && evaluated && child_read);
    let text = serde_json::to_string(&child_history).unwrap();
    assert!(text.contains("CHILD_BEFORE_START"));
    assert!(text.contains("CHILD_READ_DENIED"));
}

#[tokio::test]
#[serial(claude_code)]
async fn background_child_outlives_parent_result_without_polluting_parent_history() {
    let fixture = Fixture::new("subagent-background");
    let mut handle = fixture.handle();
    handle.send(payload(fixture.root.path(), 1, Vec::new()));
    let mut completions = 0;
    let mut child_completed = false;
    let mut child_history = String::new();
    let mut late_evaluation = false;
    let mut idle_beside_child = false;
    let mut history = Vec::new();
    let (mut prompt_tokens, mut completion_tokens) = (0, 0);
    loop {
        match output(&mut handle).await {
            EngineOutput::Event(EngineEvent::HistoryAppended { delta, .. }) => {
                assert_eq!(delta.first_index.get(), history.len());
                assert!(!serde_json::to_string(&delta.items)
                    .unwrap()
                    .contains("CHILD_REPORT_ONLY"));
                history.extend(delta.items);
            }
            EngineOutput::Event(EngineEvent::TurnComplete { .. }) => completions += 1,
            EngineOutput::Event(EngineEvent::TokenUsage { usage, .. }) => {
                prompt_tokens += usage.prompt_tokens.unwrap_or(0);
                completion_tokens += usage.completion_tokens.unwrap_or(0);
            }
            EngineOutput::Event(EngineEvent::AgentOutput { event, .. }) => {
                if let EngineEvent::HistoryAppended { delta, .. } = *event {
                    child_history.push_str(&serde_json::to_string(&delta.items).unwrap());
                }
            }
            EngineOutput::Event(EngineEvent::AgentUpdated { agent, .. })
                if agent.status.is_terminal() =>
            {
                assert_eq!(agent.status, protocol::AgentStatus::Completed);
                assert_eq!(
                    completions, 1,
                    "background child must outlive root completion"
                );
                child_completed = true;
            }
            EngineOutput::HostCall(HostCall::NativeToolEvaluation { reply, .. }) => {
                late_evaluation |= completions == 1;
                let _ = reply.send(ToolEvaluation {
                    decision: if completions == 1 {
                        Decision::Deny
                    } else {
                        Decision::Allow
                    },
                    metadata: Default::default(),
                });
            }
            // A running child does not hold the next prompt back; its wake does.
            EngineOutput::Event(EngineEvent::NativeSessionState { working: false, .. }) => {
                if child_completed {
                    break;
                }
                idle_beside_child |= completions == 1;
            }
            EngineOutput::Event(EngineEvent::TurnError { message, .. }) => panic!("{message}"),
            _ => {}
        }
    }
    assert_eq!(completions, 1, "parent completes exactly once");
    assert!(child_completed && late_evaluation && idle_beside_child);
    assert!(child_history.contains("CHILD_READ_DENIED"));
    assert!(child_history.contains("CHILD_REPORT_ONLY"));
    let parent = serde_json::to_string(&history).unwrap();
    assert_eq!(parent.matches("BACKGROUND_WAKE_REPLY").count(), 1);
    assert!(!parent.contains("CHILD_READ_DENIED"));
    assert_eq!((prompt_tokens, completion_tokens), (7, 11));

    assert!(
        session::load(
            &fixture.root.path().join("sessions"),
            "test-session",
            fixture.root.path(),
            &history
        )
        .is_some(),
        "the settled native session must be resumable with its wake history"
    );
    handle.send(payload(fixture.root.path(), 2, history.clone()));
    finish(&mut handle, &mut history).await;
    let calls = fixture.calls();
    assert_eq!(
        calls
            .iter()
            .filter(|call| call.get("argv").is_some())
            .count(),
        1,
        "the next turn continues on the same native process"
    );
    let prompt = calls.iter().rfind(|call| call["type"] == "user").unwrap();
    assert_eq!(prompt["message"]["content"][0]["text"], "turn 2");
}

#[tokio::test]
#[serial(claude_code)]
async fn resumes_native_session_and_rewinds_without_replaying_tools() {
    let fixture = Fixture::new("text");
    let mut history = Vec::new();
    let mut handle = fixture.handle();
    handle.send(payload(fixture.root.path(), 1, history.clone()));
    assert!(!finish(&mut handle, &mut history).await.interrupted);
    drop(handle);
    let mut handle = fixture.handle();
    handle.send(payload(fixture.root.path(), 2, history.clone()));
    finish(&mut handle, &mut history).await;
    let launches: Vec<_> = fixture
        .calls()
        .into_iter()
        .filter(|call| call.get("argv").is_some())
        .collect();
    assert!(launches[1]["argv"]
        .as_array()
        .unwrap()
        .iter()
        .any(|arg| arg.as_str().is_some_and(|s| s.starts_with("--resume="))));
    history.truncate(1);
    handle.send(payload(fixture.root.path(), 3, history.clone()));
    finish(&mut handle, &mut history).await;
    let calls = fixture.calls();
    let launch = calls
        .iter()
        .rfind(|call| call.get("argv").is_some())
        .unwrap();
    assert!(!launch["argv"]
        .as_array()
        .unwrap()
        .iter()
        .any(|arg| arg.as_str().is_some_and(|s| s.starts_with("--resume="))));
    let prompt = calls.iter().rfind(|call| call["type"] == "user").unwrap();
    assert!(prompt["message"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("historical context"));
}

#[tokio::test]
#[serial(claude_code)]
async fn resumed_native_plan_uses_the_saved_acknowledged_write() {
    let fixture = Fixture::new("plan-save");
    let mut history = Vec::new();
    let mut handle = fixture.handle();
    handle.send(payload(fixture.root.path(), 1, history.clone()));
    finish(&mut handle, &mut history).await;
    // A process stopped while Claude Code still works unlinks its session.
    while !matches!(
        output(&mut handle).await,
        EngineOutput::Event(EngineEvent::NativeSessionState { working: false, .. })
    ) {}
    drop(handle);
    let directory = plan_directory(&fixture.root.path().join("sessions"), "test-session").unwrap();
    std::fs::write(directory.join("unrelated.md"), "# Unrelated plan").unwrap();
    std::env::set_var("SMELT_CLAUDE_FIXTURE", "plan-resume");
    let mut handle = fixture.handle();
    let mut request = payload(fixture.root.path(), 2, history.clone());
    if let UiCommand::StartTurn(payload) = &mut request {
        payload.tools.push(
            serde_json::from_value(json!({
                "name":"present_plan", "description":"test adapter", "parameters":{}
            }))
            .unwrap(),
        );
    }
    handle.send(request);
    let mut presented = false;
    loop {
        match output(&mut handle).await {
            EngineOutput::Event(EngineEvent::ToolEvaluationRequest {
                request_id, args, ..
            }) => {
                assert_eq!(
                    args["plan"],
                    "# Fixture plan\n\nImplement the selected fixture features."
                );
                handle.send(UiCommand::ToolEvaluationResponse {
                    request_id,
                    evaluation: ToolEvaluation {
                        decision: Decision::Allow,
                        metadata: Default::default(),
                    },
                });
            }
            EngineOutput::Event(EngineEvent::ToolDispatch {
                request_id,
                invocation_id,
                call_id,
                args,
                ..
            }) => {
                assert_eq!(
                    args["plan"],
                    "# Fixture plan\n\nImplement the selected fixture features."
                );
                presented = true;
                handle.send(UiCommand::ToolResult {
                    request_id,
                    invocation_id,
                    call_id,
                    content: "Saved draft".into(),
                    is_error: false,
                    metadata: Some(json!({"action":"draft"})),
                    display_content: Vec::new(),
                    attachment: None,
                });
            }
            EngineOutput::Event(EngineEvent::TurnComplete { .. }) => break,
            EngineOutput::Event(EngineEvent::TurnError { message, .. }) => panic!("{message}"),
            _ => {}
        }
    }
    assert!(presented, "resumed plan never reached host UI adapter");
    let launches: Vec<_> = fixture
        .calls()
        .into_iter()
        .filter(|call| call.get("argv").is_some())
        .collect();
    assert!(launches[1]["argv"]
        .as_array()
        .unwrap()
        .iter()
        .any(|arg| arg.as_str().is_some_and(|arg| arg.starts_with("--resume="))));
}

#[tokio::test]
#[serial(claude_code)]
async fn queued_input_is_acknowledged_and_committed_exactly_once() {
    let fixture = Fixture::new("text");
    let mut handle = fixture.handle();
    handle.send(payload(fixture.root.path(), 1, Vec::new()));
    handle.send(UiCommand::Steer {
        input: StartTurnInput::user(Content::text("queued")),
    });
    let mut history = Vec::new();
    finish(&mut handle, &mut history).await;
    assert_eq!(
        history
            .iter()
            .filter(|item| matches!(item, HistoryItem::User { .. }))
            .count(),
        2
    );
    assert_eq!(
        fixture
            .calls()
            .iter()
            .filter(|call| call["type"] == "user")
            .count(),
        2
    );
}

#[tokio::test]
#[serial(claude_code)]
async fn steers_reach_claude_code_mid_turn_and_follow_the_step_they_join() {
    let fixture = Fixture::new("steer");
    let mut handle = fixture.handle();
    handle.send(payload(fixture.root.path(), 1, Vec::new()));
    handle.send(UiCommand::Steer {
        input: StartTurnInput::user(Content::text("steered")),
    });
    let mut history = Vec::new();
    finish(&mut handle, &mut history).await;
    let kinds: Vec<_> = history
        .iter()
        .map(|item| match item {
            HistoryItem::User { .. } => "user",
            HistoryItem::Assistant(step) if !step.invocations.is_empty() => "tool",
            HistoryItem::Assistant(_) => "answer",
            _ => "other",
        })
        .collect();
    assert_eq!(kinds, ["user", "tool", "user", "answer"]);
    let steer = fixture
        .calls()
        .into_iter()
        .find(|call| call["type"] == "user" && call["priority"] == "next")
        .expect("the steer is sent while Claude Code works");
    assert!(steer["uuid"].is_string());
}

#[tokio::test]
#[serial(claude_code)]
async fn unsteered_input_is_withdrawn_from_claude_code() {
    let fixture = Fixture::new("steer-cancel");
    let mut handle = fixture.handle();
    let mut history = Vec::new();
    handle.send(payload(fixture.root.path(), 1, history.clone()));
    loop {
        match output(&mut handle).await {
            EngineOutput::Event(EngineEvent::HistoryAppended { delta, .. }) => {
                history.extend(delta.items);
            }
            EngineOutput::Event(EngineEvent::ToolStarted { .. }) => break,
            EngineOutput::Event(EngineEvent::TurnError { message, .. }) => panic!("{message}"),
            _ => {}
        }
    }
    handle.send(UiCommand::Steer {
        input: StartTurnInput::user(Content::text("withdrawn")),
    });
    handle.send(UiCommand::Unsteer { count: 1 });
    finish(&mut handle, &mut history).await;
    assert_eq!(
        history
            .iter()
            .filter(|item| matches!(item, HistoryItem::User { .. }))
            .count(),
        1,
        "a withdrawn steer never joins the conversation"
    );
}

#[tokio::test]
#[serial(claude_code)]
async fn idle_native_processes_are_released_and_resumed_on_the_next_turn() {
    let fixture = Fixture::new("text");
    let mut config = EngineConfig::new(
        fixture.root.path().into(),
        Arc::new(crate::clock::RealClock),
    );
    config.native_idle_timeout = Duration::from_millis(200);
    let mut handle = crate::start(config, Box::new(crate::tools::EmptyDispatcher));
    let mut history = Vec::new();
    handle.send(payload(fixture.root.path(), 1, history.clone()));
    finish(&mut handle, &mut history).await;
    while !matches!(
        output(&mut handle).await,
        EngineOutput::Event(EngineEvent::NativeSessionState { active: false, .. })
    ) {}
    let pid = fixture.calls()[0]["pid"].as_u64().unwrap() as i32;
    assert_eq!(
        unsafe { libc::kill(pid, 0) },
        -1,
        "idle process kept running"
    );

    handle.send(payload(fixture.root.path(), 2, history.clone()));
    finish(&mut handle, &mut history).await;
    let launches: Vec<_> = fixture
        .calls()
        .into_iter()
        .filter(|call| call.get("argv").is_some())
        .collect();
    assert_eq!(launches.len(), 2);
    assert!(launches[1]["argv"]
        .as_array()
        .unwrap()
        .iter()
        .any(|arg| arg.as_str().is_some_and(|s| s.starts_with("--resume="))));
}

#[tokio::test]
#[serial(claude_code)]
async fn cancellation_ends_the_turn_and_the_session_end_reaps_claude_code() {
    let fixture = Fixture::new("hang");
    let mut handle = fixture.handle();
    handle.send(payload(fixture.root.path(), 1, Vec::new()));
    let mut history = Vec::new();
    loop {
        if let EngineOutput::Event(EngineEvent::HistoryAppended { delta, .. }) =
            output(&mut handle).await
        {
            history.extend(delta.items);
            break;
        }
    }
    handle.send(UiCommand::Cancel);
    assert!(finish(&mut handle, &mut history).await.interrupted);
    let pid = fixture.calls()[0]["pid"].as_u64().unwrap() as i32;
    assert_eq!(
        unsafe { libc::kill(pid, 0) },
        0,
        "a cancelled turn ended Claude Code"
    );
    // Claude Code reads the interrupt on its own time.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !fixture
        .calls()
        .iter()
        .any(|call| call["request"]["subtype"] == "interrupt")
    {
        assert!(
            std::time::Instant::now() < deadline,
            "no interrupt reached Claude Code"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    handle.send(UiCommand::EndNativeSession);
    while !matches!(
        output(&mut handle).await,
        EngineOutput::Event(EngineEvent::NativeSessionState { active: false, .. })
    ) {}
    assert_eq!(
        unsafe { libc::kill(pid, 0) },
        -1,
        "native process remained alive after the session ended"
    );
}

#[tokio::test]
#[serial(claude_code)]
async fn a_turn_after_cancellation_continues_on_the_same_process() {
    let fixture = Fixture::new("cancel-continue");
    let mut handle = fixture.handle();
    handle.send(payload(fixture.root.path(), 1, Vec::new()));
    let mut history = Vec::new();
    let mut shells = Vec::new();
    while !serde_json::to_string(&history)
        .unwrap()
        .contains("KEPT_STEP")
    {
        match output(&mut handle).await {
            EngineOutput::Event(EngineEvent::HistoryAppended { delta, .. }) => {
                history.extend(delta.items)
            }
            EngineOutput::Event(EngineEvent::NativeProcesses { processes }) => shells = processes,
            _ => {}
        }
    }
    handle.send(UiCommand::Cancel);
    loop {
        match output(&mut handle).await {
            EngineOutput::Event(EngineEvent::NativeProcesses { processes }) => shells = processes,
            EngineOutput::Event(EngineEvent::TurnComplete { meta, .. }) => {
                assert!(meta.unwrap().interrupted);
                break;
            }
            _ => {}
        }
    }

    handle.send(payload(fixture.root.path(), 2, history.clone()));
    finish(&mut handle, &mut history).await;
    assert_eq!(shells.len(), 1, "the background shell outlives the cancel");
    assert!(!fixture
        .calls()
        .iter()
        .any(|call| call["request"]["subtype"] == "stop_task"));
    let history = serde_json::to_string(&history).unwrap();
    assert!(history.contains("KEPT_STEP"));
    assert!(history.contains("fixture answer"));
    assert!(!history.contains("LATE_FRAME") && !history.contains("LATE_RESULT"));
    let launches = fixture
        .calls()
        .iter()
        .filter(|call| call.get("argv").is_some())
        .count();
    assert_eq!(launches, 1, "the next turn relaunched Claude Code");
}

#[tokio::test]
#[serial(claude_code)]
async fn a_prompt_sent_while_claude_code_works_on_its_own_starts_once_taken() {
    let fixture = Fixture::new("monitor-wake");
    let mut handle = fixture.handle();
    handle.send(payload(fixture.root.path(), 1, Vec::new()));
    let mut history = Vec::new();
    finish(&mut handle, &mut history).await;
    fixture
        .wait_for("Claude Code never woke", |call| call["wake"] == "started")
        .await;

    handle.send(payload(fixture.root.path(), 2, history.clone()));
    let meta = finish(&mut handle, &mut history).await;
    assert!(!meta.interrupted);
    assert!(
        fixture
            .calls()
            .iter()
            .any(|call| call["wake"] == "prompted"),
        "the prompt waited for Claude Code instead of its queue"
    );
    let history = serde_json::to_string(&history).unwrap();
    let prompt = history.find("turn 2").expect("second prompt");
    let wake = prompt
        + history[prompt..]
            .find("MONITOR_WAKE_REPLY")
            .expect("wake reply");
    assert!(
        history[wake..].contains("fixture answer"),
        "the wake reply ended the turn"
    );
}

#[tokio::test]
#[serial(claude_code)]
async fn cancelling_interrupts_claude_code_thinking_on_its_own() {
    let fixture = Fixture::new("monitor-wake");
    let mut handle = fixture.handle();
    handle.send(payload(fixture.root.path(), 1, Vec::new()));
    finish(&mut handle, &mut Vec::new()).await;
    // The engine sees the wake before the cancel.
    while !matches!(
        output(&mut handle).await,
        EngineOutput::Event(EngineEvent::NativeSessionState { working: true, .. })
    ) {}

    handle.send(UiCommand::Cancel);
    fixture
        .wait_for("no interrupt reached Claude Code", |call| {
            call["request"]["subtype"] == "interrupt"
        })
        .await;
}

#[tokio::test]
#[serial(claude_code)]
async fn cancelling_a_prompt_claude_code_has_not_taken_withdraws_it() {
    let fixture = Fixture::new("monitor-wake-held");
    let mut handle = fixture.handle();
    handle.send(payload(fixture.root.path(), 1, Vec::new()));
    let mut history = Vec::new();
    finish(&mut handle, &mut history).await;
    fixture
        .wait_for("Claude Code never woke", |call| call["wake"] == "started")
        .await;
    handle.send(payload(fixture.root.path(), 2, history));
    fixture
        .wait_for("the prompt never reached Claude Code", |call| {
            call["wake"] == "prompted"
        })
        .await;

    handle.send(UiCommand::Cancel);
    let meta = loop {
        if let EngineOutput::Event(EngineEvent::TurnComplete { meta, .. }) =
            output(&mut handle).await
        {
            break meta.unwrap();
        }
    };
    assert!(meta.interrupted);
    let prompt = fixture
        .calls()
        .into_iter()
        .find(|call| call["type"] == "user" && call["message"].to_string().contains("turn 2"))
        .unwrap();
    fixture
        .wait_for("the prompt was not withdrawn", |call| {
            call["withdrawn"] == prompt["uuid"]
        })
        .await;
}

#[tokio::test]
#[serial(claude_code)]
async fn models_are_listed_by_claude_code_and_cached_until_it_changes() {
    let fixture = Fixture::new("text");
    assert!(super::cached_models().is_empty());
    assert!(super::models_stale());

    let listed = super::refresh_models().await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, "opus");
    assert_eq!(listed[0].display_name.as_deref(), Some("Opus 5.5"));
    assert_eq!(super::cached_models(), listed);
    assert!(!super::models_stale());
    assert!(fixture.calls().iter().all(|call| call["type"] != "user"));

    let executable = fixture.root.path().join("claude");
    let mut script = std::fs::read_to_string(&executable).unwrap();
    script.push_str("\n# updated\n");
    std::fs::write(&executable, script).unwrap();
    assert!(super::models_stale());
}

#[tokio::test]
#[serial(claude_code)]
async fn usage_is_reported_by_claude_code_without_a_model_request() {
    let fixture = Fixture::new("text");
    let usage = super::usage().await.unwrap();
    let rows: Vec<_> = usage
        .limits
        .iter()
        .map(|row| (row.label.as_str(), row.used, row.limit))
        .collect();
    assert_eq!(rows, [("5h limit", 42, 100)]);
    assert!(fixture.calls().iter().all(|call| call["type"] != "user"));
}

#[tokio::test]
#[serial(claude_code)]
async fn sessions_record_the_models_claude_code_lists() {
    let fixture = Fixture::new("text");
    let mut handle = fixture.handle();
    handle.send(payload(fixture.root.path(), 1, Vec::new()));
    finish(&mut handle, &mut Vec::new()).await;

    let cached: Vec<_> = super::cached_models()
        .into_iter()
        .map(|model| model.id)
        .collect();
    assert_eq!(cached, ["opus"]);
    assert!(!super::models_stale());
}

#[tokio::test]
#[serial(claude_code)]
async fn missing_executable_and_invalid_transport_fail_without_http_fallback() {
    let fixture = Fixture::new("text");
    assert!(super::installed());
    std::env::set_var(
        "SMELT_CLAUDE_CODE_EXECUTABLE",
        fixture.root.path().join("missing"),
    );
    assert!(!super::installed());
    let mut handle = fixture.handle();
    handle.send(payload(fixture.root.path(), 1, Vec::new()));
    loop {
        if let EngineOutput::Event(EngineEvent::TurnError { message, .. }) =
            output(&mut handle).await
        {
            assert!(message.contains("Install Claude Code"));
            break;
        }
    }
    let mut request = payload(fixture.root.path(), 2, Vec::new());
    if let UiCommand::StartTurn(payload) = &mut request {
        payload.model_target.api_base = "http://127.0.0.1:1".into();
    }
    handle.send(request);
    loop {
        if let EngineOutput::Event(EngineEvent::TurnError { message, .. }) =
            output(&mut handle).await
        {
            assert!(message.contains("remove api_base"));
            break;
        }
    }
}

#[tokio::test]
#[serial(claude_code)]
async fn auxiliary_request_uses_schema_and_disables_tools() {
    let fixture = Fixture::new("text");
    let mut handle = fixture.handle();
    let UiCommand::StartTurn(payload) = payload(fixture.root.path(), 1, Vec::new()) else {
        unreachable!()
    };
    handle.send(UiCommand::EngineAsk {
        id: 42,
        system: "title".into(),
        messages: vec![protocol::Message::user(Content::text("test"))],
        target: Box::new(payload.model_target),
        request_config: Default::default(),
        response_format: Some(protocol::AskResponseFormat {
            name: "title".into(),
            schema: json!({"type":"object"}),
        }),
        reasoning_effort: ReasoningEffort::Off,
        fast_mode: false,
        tools: Vec::new(),
        session_id: "test-session".into(),
        persistence: Default::default(),
        stream: true,
        visible_retries: false,
    });
    loop {
        if let EngineOutput::Event(EngineEvent::EngineAskResponse { id, message, error }) =
            output(&mut handle).await
        {
            assert_eq!(id, 42);
            assert!(error.is_none(), "{error:?}");
            assert_eq!(
                message.unwrap().content.unwrap().as_text(),
                "{\"title\":\"fixture title\"}"
            );
            break;
        }
    }
    let calls = fixture.calls();
    let args = calls[0]["argv"].as_array().unwrap();
    let tool_flag = args.iter().position(|arg| arg == "--tools").unwrap();
    assert_eq!(args[tool_flag + 1], "");
    assert!(args.iter().any(|arg| arg == "--strict-mcp-config"));
    assert!(args.iter().any(|arg| arg == "--no-session-persistence"));
    assert!(args.iter().any(|arg| arg == "--safe-mode"));
    assert!(args.iter().any(|arg| arg == "--json-schema"));
}

#[tokio::test]
#[serial(claude_code)]
async fn permission_mode_change_rejects_pending_evaluation_and_approval() {
    for ask in [false, true] {
        let fixture = Fixture::new("permission");
        let mut handle = fixture.handle();
        handle.send(payload(fixture.root.path(), 1, Vec::new()));
        let mut history = Vec::new();
        loop {
            match output(&mut handle).await {
                EngineOutput::Event(EngineEvent::HistoryAppended { delta, .. }) => {
                    history.extend(delta.items)
                }
                EngineOutput::HostCall(HostCall::NativeToolEvaluation { reply, .. }) => {
                    if !ask {
                        handle.send(UiCommand::SetMode {
                            mode: protocol::AgentMode::parse("plan").unwrap(),
                        });
                        handle.send(UiCommand::SetMode {
                            mode: protocol::AgentMode::normal(),
                        });
                    }
                    reply
                        .send(ToolEvaluation {
                            decision: if ask { Decision::Ask } else { Decision::Allow },
                            metadata: Default::default(),
                        })
                        .ok();
                    if !ask {
                        break;
                    }
                }
                EngineOutput::Event(EngineEvent::RequestPermission { request_id, .. }) => {
                    handle.send(UiCommand::SetMode {
                        mode: protocol::AgentMode::parse("plan").unwrap(),
                    });
                    handle.send(UiCommand::SetMode {
                        mode: protocol::AgentMode::normal(),
                    });
                    handle.send(UiCommand::PermissionDecision {
                        request_id,
                        approved: true,
                        message: None,
                    });
                    break;
                }
                _ => {}
            }
        }
        finish(&mut handle, &mut history).await;
        let calls = fixture.calls();
        let response = calls
            .iter()
            .find(|call| call["response"]["request_id"] == "permission")
            .unwrap();
        assert_eq!(
            response["response"]["response"]["hookSpecificOutput"]["permissionDecision"],
            "deny"
        );
    }
}

#[tokio::test]
#[serial(claude_code)]
async fn cancelled_reads_preserve_fragmented_utf8_frames() {
    let fixture = Fixture::new("fragmented");
    let UiCommand::StartTurn(payload) = payload(fixture.root.path(), 1, Vec::new()) else {
        unreachable!()
    };
    let mut process = Process::spawn(transport::LaunchOptions {
        cwd: fixture.root.path(),
        target: &payload.model_target,
        mode: &payload.mode,
        effort: &payload.reasoning_effort,
        resume: None,
        resume_at: None,
        prompt: None,
        override_prompt: None,
        plans_directory: None,
        disallowed_tools: &[],
        tools: false,
        persist_session: false,
        fast_mode: false,
        schema: None,
    })
    .unwrap();
    process
        .write(control("smelt-init", json!({"subtype":"initialize"})))
        .await
        .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
    let mut cancellations = 0;
    loop {
        assert!(tokio::time::Instant::now() < deadline);
        match tokio::time::timeout(Duration::from_millis(1), process.read()).await {
            Err(_) => cancellations += 1,
            Ok(Ok(Some(frame))) if frame["type"] == "control_response" => process
                .write(user_message(vec![json!({"type":"text","text":"hello"})]))
                .await
                .unwrap(),
            Ok(Ok(Some(frame))) if frame["type"] == "result" => {
                assert_eq!(frame["result"], "fixture answer café 🌱");
                break;
            }
            Ok(Ok(Some(_))) => {}
            other => panic!("unexpected transport result: {other:?}"),
        }
    }
    assert!(cancellations > 10);
    process.close().await;
}

#[test]
fn reasoning_parts_finish_without_repeating_per_block_messages() {
    let (tx, _, mut rx) = crate::output_channel(HostCallbacks::Disabled);
    let mut transcript = Transcript::new(tx, "claude-code".into(), 1, 0);
    let stream = |event| json!({"type":"stream_event", "event":event});
    transcript.frame(
        &stream(json!({"type":"message_start","message":{"id":"same-message"}})),
        0,
    );
    for (index, kind, text) in [(0, "thinking", "think"), (1, "text", "answer")] {
        transcript.frame(
            &stream(
                json!({"type":"content_block_start","index":index,"content_block":{"type":kind}}),
            ),
            0,
        );
        let delta = if kind == "thinking" {
            json!({"type":"thinking_delta","thinking":text})
        } else {
            json!({"type":"text_delta","text":text})
        };
        transcript.frame(
            &stream(json!({"type":"content_block_delta","index":index,"delta":delta})),
            0,
        );
        transcript.frame(
            &stream(json!({"type":"content_block_stop","index":index})),
            0,
        );
        let block = if kind == "thinking" {
            json!({"type":kind,"thinking":text})
        } else {
            json!({"type":kind,"text":text})
        };
        transcript.frame(&json!({"type":"assistant","uuid":format!("block-{index}"),"message":{"id":"same-message","content":[block]}}), 0);
    }
    transcript.finish();
    let mut events = Vec::new();
    while let Ok(EngineOutput::Event(event)) = rx.try_recv() {
        events.push(event);
    }
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, EngineEvent::TextDelta { .. }))
            .count(),
        1
    );
    assert_eq!(events.iter().filter(|event| matches!(event, EngineEvent::ReasoningPartFinished { content, .. } if content == "think")).count(), 1);
    assert!(!events
        .iter()
        .any(|event| matches!(event, EngineEvent::Reasoning { .. })));
    assert_eq!(transcript.committed.len(), 2);
}

#[test]
fn native_tools_start_as_their_smelt_counterpart_and_report_execution_time() {
    let (tx, _, mut rx) = crate::output_channel(HostCallbacks::Disabled);
    let mut transcript = Transcript::new(tx, "claude-code".into(), 1, 0);
    let stream = |event| json!({"type":"stream_event", "event":event});
    // Claude Code's real order: the complete message precedes the block stop.
    transcript.frame(
        &stream(json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_1","name":"Bash"}})),
        1_000,
    );
    transcript.frame(
        &json!({"type":"assistant","uuid":"a1","message":{"content":[{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"ls","timeout":5000,"run_in_background":false}}]}}),
        2_000,
    );
    transcript.frame(
        &stream(json!({"type":"content_block_stop","index":0})),
        2_000,
    );
    transcript.frame(
        &json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"a.txt"}]}}),
        3_500,
    );
    let mut events = Vec::new();
    while let Ok(EngineOutput::Event(event)) = rx.try_recv() {
        events.push(event);
    }
    assert!(!events
        .iter()
        .any(|event| matches!(event, EngineEvent::ToolCallDraftFinished { .. })));
    assert!(events.iter().any(|event| matches!(event,
        EngineEvent::ToolCallDraftStarted { tool_name: Some(name), .. } if name == "bash")));
    let args = events
        .iter()
        .find_map(|event| match event {
            EngineEvent::ToolStarted {
                tool_name,
                args,
                called_at_ms: 2_000,
                ..
            } if tool_name == "bash" => Some(args),
            _ => None,
        })
        .expect("native Bash starts as bash");
    assert_eq!(args.get("command"), Some(&json!("ls")));
    assert_eq!(args.get("timeout_ms"), Some(&json!(5000)));
    assert_eq!(args.get("background"), Some(&json!(false)));
    assert!(events.iter().any(|event| matches!(
        event,
        EngineEvent::ToolFinished {
            elapsed_ms: Some(1_500),
            ..
        }
    )));
    let [HistoryItem::Assistant(step)] = transcript.committed.as_slice() else {
        panic!("one committed step: {:?}", transcript.committed);
    };
    let tool = &step.invocations[0];
    assert_eq!(tool.name, "bash");
    assert_eq!(tool.elapsed_ms, Some(1_500));
    assert_eq!(tool.provider, None);
}

#[test]
fn untranslated_native_tools_are_shown_under_their_provider() {
    let (tx, _, mut rx) = crate::output_channel(HostCallbacks::Disabled);
    let mut transcript = Transcript::new(tx, "claude".into(), 1, 0);
    transcript.frame(
        &json!({"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_1","name":"ToolSearch"}}}),
        1_000,
    );
    transcript.frame(
        &json!({"type":"assistant","uuid":"a1","message":{"content":[{"type":"tool_use","id":"toolu_1","name":"ToolSearch","input":{"query":"select:Read"}}]}}),
        1_000,
    );
    transcript.frame(
        &json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"found"}]}}),
        2_000,
    );
    let mut events = Vec::new();
    while let Ok(EngineOutput::Event(event)) = rx.try_recv() {
        events.push(event);
    }
    assert!(events.iter().any(|event| matches!(event,
        EngineEvent::ToolCallDraftStarted { tool_name: Some(name), .. } if name == "claude/ToolSearch")));
    assert!(events.iter().any(|event| matches!(event,
        EngineEvent::ToolStarted { tool_name, .. } if tool_name == "claude/ToolSearch")));
    let [HistoryItem::Assistant(step)] = transcript.committed.as_slice() else {
        panic!("one committed step: {:?}", transcript.committed);
    };
    let tool = &step.invocations[0];
    assert_eq!(tool.name, "ToolSearch");
    assert_eq!(tool.provider.as_deref(), Some("claude"));
    assert_eq!(tool.displayed_name(), "claude/ToolSearch");
}

#[test]
fn native_edit_results_carry_the_whole_file_for_a_diff() {
    let (tx, _, _rx) = crate::output_channel(HostCallbacks::Disabled);
    let mut transcript = Transcript::new(tx, "claude-code".into(), 1, 0);
    transcript.frame(
        &json!({"type":"assistant","uuid":"a1","message":{"content":[{"type":"tool_use","id":"toolu_1","name":"Edit","input":{"file_path":"/w/a.txt","old_string":"hello","new_string":"world"}}]}}),
        1_000,
    );
    // The shape Claude Code 2.1 reports for a successful Edit.
    transcript.frame(
        &json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"The file /w/a.txt has been updated successfully."}]},
            "tool_use_result":{"filePath":"/w/a.txt","oldString":"hello","newString":"world","originalFile":"hello there\nhello again\n","structuredPatch":[{"oldStart":1,"oldLines":2,"newStart":1,"newLines":2,"lines":["-hello there","+world there"," hello again"]}],"userModified":false,"replaceAll":false}}),
        2_000,
    );
    let [HistoryItem::Assistant(step)] = transcript.committed.as_slice() else {
        panic!("one committed step: {:?}", transcript.committed);
    };
    let result = &step.invocations[0].result;
    assert_eq!(step.invocations[0].name, "edit_file");
    assert!(!result.is_error);
    assert_eq!(result.metadata, Some(json!({"path":"/w/a.txt"})));
    let field = |name: &str| {
        result
            .display_content
            .iter()
            .find(|field| field.name == name)
            .map(|field| field.content.as_str().to_owned())
    };
    assert_eq!(
        field("old_content").as_deref(),
        Some("hello there\nhello again\n")
    );
    assert_eq!(
        field("new_content").as_deref(),
        Some("world there\nhello again\n")
    );
}

#[test]
fn edit_patches_apply_what_claude_code_changed_or_nothing() {
    use super::transcript::apply_patch;
    let hunk = |start: u64, lines: &[&str]| {
        let old = lines
            .iter()
            .filter(|line| line.starts_with([' ', '-']))
            .count();
        json!({"oldStart":start, "oldLines":old, "lines":lines})
    };
    // The file has a curly quote the call's old_string did not; the patch is
    // what Claude Code actually changed.
    let original = "a\nit\u{2019}s\nb\nc\n";
    assert_eq!(
        apply_patch(original, &[hunk(2, &["-it\u{2019}s", "+it is", " b"])]).as_deref(),
        Some("a\nit is\nb\nc\n")
    );
    assert_eq!(
        apply_patch(original, &[hunk(1, &[" a", "-b"])]),
        None,
        "a patch for another file is not applied"
    );
    assert_eq!(
        apply_patch("x\r\ny\r\n", &[hunk(2, &["-y", "+z"])]).as_deref(),
        Some("x\r\nz\r\n")
    );
    assert_eq!(
        apply_patch(
            "x\ny",
            &[hunk(
                2,
                &[
                    "-y",
                    "\\ No newline at end of file",
                    "+z",
                    "\\ No newline at end of file"
                ]
            )]
        )
        .as_deref(),
        Some("x\nz")
    );
}

#[test]
fn imported_history_is_readable_and_keeps_the_latest_turns() {
    let mut history = Vec::new();
    for turn in 0..400 {
        history.push(HistoryItem::user(Content::text(format!("question {turn}"))));
        history.push(HistoryItem::Assistant(protocol::AssistantStep::terminal(
            Some(Content::text("x".repeat(1_000))),
            None,
            Vec::new(),
        )));
    }
    let text = imported_history(&history);
    assert!(text.len() <= IMPORT_LIMIT + 1_000);
    assert!(text.contains("[Earlier conversation omitted]"));
    assert!(text.contains("User: question 399"));
    assert!(!text.contains("User: question 0\n"));
    assert!(text.ends_with("End of previous conversation."));
}

#[test]
fn cumulative_cost_time_and_model_usage_are_counted_once() {
    let (tx, _, mut rx) = crate::output_channel(HostCallbacks::Disabled);
    let mut tracker = UsageTracker::default();
    for count in [1, 2] {
        tracker.emit(&tx, &json!({"total_cost_usd":0.01 * count as f64,"duration_api_ms":400 * count,"modelUsage":{"sonnet":{"inputTokens":5 * count,"outputTokens":8 * count,"cacheReadInputTokens":3 * count,"cacheCreationInputTokens":2 * count}}}), false, Some(18));
        let EngineOutput::Event(EngineEvent::TokenUsage {
            usage,
            cost_usd,
            tokens_per_sec,
            ..
        }) = rx.try_recv().unwrap()
        else {
            unreachable!()
        };
        assert_eq!(usage.prompt_tokens, Some(5));
        assert_eq!(usage.completion_tokens, Some(8));
        assert_eq!(usage.cache_read_tokens, Some(3));
        assert_eq!(usage.context_tokens, Some(18));
        assert!((cost_usd.unwrap() - 0.01).abs() < f64::EPSILON);
        assert_eq!(tokens_per_sec, Some(20.0));
    }
}

#[tokio::test]
#[serial(claude_code)]
async fn workspace_change_moves_claude_code_and_continues_the_turn() {
    let fixture = Fixture::new("move");
    let destination = fixture.root.path().join("new-workspace");
    std::fs::create_dir(&destination).unwrap();
    let mut handle = fixture.handle();
    let request = |turn, history| {
        let UiCommand::StartTurn(mut request) = payload(fixture.root.path(), turn, history) else {
            unreachable!()
        };
        request.tools.push(
            serde_json::from_value(
                json!({"name":"fixture_tool", "description":"fixture", "parameters":{"type":"object"}}),
            )
            .unwrap(),
        );
        UiCommand::StartTurn(request)
    };
    handle.send(request(1, Vec::new()));
    let mut history = Vec::new();
    loop {
        match output(&mut handle).await {
            EngineOutput::Event(EngineEvent::HistoryAppended { delta, .. }) => {
                history.extend(delta.items)
            }
            EngineOutput::Event(EngineEvent::ToolEvaluationRequest { request_id, .. }) => handle
                .send(UiCommand::ToolEvaluationResponse {
                    request_id,
                    evaluation: ToolEvaluation {
                        decision: Decision::Allow,
                        metadata: Default::default(),
                    },
                }),
            EngineOutput::Event(EngineEvent::ToolDispatch {
                request_id,
                invocation_id,
                call_id,
                ..
            }) => {
                handle.send(UiCommand::UpdateAgentProjectContext(Box::new(
                    protocol::AgentProjectContext {
                        cwd: destination.clone(),
                        instructions: None,
                        skill_section: None,
                        system_prompt_override: None,
                        system_prompt: String::new(),
                        tools: Vec::new(),
                    },
                )));
                handle.send(UiCommand::ToolResult {
                    request_id,
                    invocation_id,
                    call_id,
                    content: "fixture-mcp-ok".into(),
                    is_error: false,
                    metadata: None,
                    display_content: Vec::new(),
                    attachment: None,
                });
                break;
            }
            EngineOutput::Event(EngineEvent::TurnError { message, .. }) => panic!("{message}"),
            _ => {}
        }
    }
    assert!(!finish(&mut handle, &mut history).await.interrupted);
    // The interruption is Claude Code's alone: the turn reads as one answer.
    let [.., HistoryItem::Assistant(tool), HistoryItem::Assistant(answer)] = history.as_slice()
    else {
        panic!("{history:?}")
    };
    assert_eq!(tool.invocations[0].result.content, "fixture-mcp-ok");
    assert_eq!(
        answer
            .content
            .as_ref()
            .map(Content::text_content)
            .as_deref(),
        Some("fixture answer café 🌱")
    );
    assert!(fixture
        .calls()
        .iter()
        .any(|call| call["message"]["content"][0]["text"]
            .as_str()
            .is_some_and(|text| text.contains(destination.to_str().unwrap()))));
    assert!(session::load(
        &fixture.root.path().join("sessions"),
        "test-session",
        &destination,
        &history
    )
    .is_some());
    handle.send(request(2, history.clone()));
    finish(&mut handle, &mut history).await;
    let launches = fixture
        .calls()
        .iter()
        .filter(|call| call.get("argv").is_some())
        .count();
    assert_eq!(launches, 1, "the moved process serves the next turn");
}

#[tokio::test]
#[serial(claude_code)]
async fn native_plan_approval_survives_mode_echo_and_keeps_native_identity() {
    let fixture = Fixture::new("plan");
    let mut handle = fixture.handle();
    let UiCommand::StartTurn(mut request) = payload(fixture.root.path(), 1, Vec::new()) else {
        unreachable!()
    };
    request.tools.push(serde_json::from_value(json!({"name":"present_plan", "description":"plan UI", "parameters":{}, "modes":["plan"]})).unwrap());
    handle.send(UiCommand::StartTurn(request));
    let mut history = Vec::new();
    let mut entered_plan = false;
    loop {
        match output(&mut handle).await {
            EngineOutput::Event(EngineEvent::NativeModeChanged { mode, .. }) => {
                assert_eq!(mode.as_str(), "plan");
                entered_plan = true;
                handle.send(UiCommand::SetMode { mode });
            }
            EngineOutput::Event(EngineEvent::ToolEvaluationRequest {
                request_id,
                mode,
                tool_name,
                ..
            }) => {
                assert!(entered_plan);
                assert_eq!(mode.as_str(), "plan");
                assert_eq!(tool_name, "present_plan");
                handle.send(UiCommand::ToolEvaluationResponse {
                    request_id,
                    evaluation: ToolEvaluation {
                        decision: Decision::Allow,
                        metadata: Default::default(),
                    },
                });
            }
            EngineOutput::Event(EngineEvent::ToolDispatch {
                request_id,
                invocation_id,
                call_id,
                ..
            }) => {
                assert_eq!(call_id, "interaction-tool");
                handle.send(UiCommand::SetMode {
                    mode: protocol::AgentMode::normal(),
                });
                handle.send(UiCommand::ToolResult {
                    request_id,
                    invocation_id,
                    call_id,
                    content: "approved plan".into(),
                    is_error: false,
                    metadata: Some(json!({"action":"approve"})),
                    display_content: vec![protocol::ToolDisplayContent::new(
                        "plan",
                        "# Fixture plan".into(),
                    )],
                    attachment: None,
                });
                break;
            }
            EngineOutput::Event(EngineEvent::HistoryAppended { delta, .. }) => {
                history.extend(delta.items)
            }
            EngineOutput::Event(EngineEvent::TurnError { message, .. }) => panic!("{message}"),
            _ => {}
        }
    }
    finish(&mut handle, &mut history).await;
    let calls = fixture.calls();
    let response = calls
        .iter()
        .find(|call| call["response"]["request_id"] == "interaction")
        .unwrap();
    assert_eq!(response["response"]["response"]["behavior"], "allow");
    let invocation = history
        .iter()
        .find_map(|item| match item {
            HistoryItem::Assistant(step) => step.invocations.first(),
            _ => None,
        })
        .unwrap();
    assert_eq!(invocation.name, "present_plan");
    assert_eq!(invocation.call_id, "interaction-tool");
    assert_eq!(
        invocation.result.metadata.as_ref().unwrap()["action"],
        "approve"
    );
    assert_eq!(
        invocation.result.display_content[0].content.as_str(),
        "# Fixture plan"
    );
}

#[tokio::test]
#[serial(claude_code)]
async fn native_question_policy_change_rejects_stale_evaluation() {
    let fixture = Fixture::new("question");
    let mut handle = fixture.handle();
    let UiCommand::StartTurn(mut request) = payload(fixture.root.path(), 1, Vec::new()) else {
        unreachable!()
    };
    request.tools.push(
        serde_json::from_value(
            json!({"name":"ask_user_question", "description":"question UI", "parameters":{}}),
        )
        .unwrap(),
    );
    handle.send(UiCommand::StartTurn(request));
    let mut history = Vec::new();
    loop {
        match output(&mut handle).await {
            EngineOutput::Event(EngineEvent::ToolEvaluationRequest { request_id, .. }) => {
                handle.send(UiCommand::SetMode {
                    mode: protocol::AgentMode::parse("plan").unwrap(),
                });
                handle.send(UiCommand::SetMode {
                    mode: protocol::AgentMode::normal(),
                });
                handle.send(UiCommand::ToolEvaluationResponse {
                    request_id,
                    evaluation: ToolEvaluation {
                        decision: Decision::Allow,
                        metadata: Default::default(),
                    },
                });
                break;
            }
            EngineOutput::Event(EngineEvent::HistoryAppended { delta, .. }) => {
                history.extend(delta.items)
            }
            _ => {}
        }
    }
    finish(&mut handle, &mut history).await;
    let calls = fixture.calls();
    let response = calls
        .iter()
        .find(|call| call["response"]["request_id"] == "interaction")
        .unwrap();
    assert_eq!(response["response"]["response"]["behavior"], "deny");
}

#[tokio::test]
#[serial(claude_code)]
async fn redaction_refusal_does_not_launch_native_process() {
    let fixture = Fixture::new("text");
    let mut handle = fixture.handle();
    let UiCommand::StartTurn(mut request) = payload(fixture.root.path(), 1, Vec::new()) else {
        unreachable!()
    };
    request.request_config.redact_secrets = true;
    handle.send(UiCommand::StartTurn(request));
    loop {
        if let EngineOutput::Event(EngineEvent::TurnError { message, .. }) =
            output(&mut handle).await
        {
            assert!(message.contains("redact_secrets"));
            break;
        }
    }
    assert!(!fixture.root.path().join("calls.jsonl").exists());
}

#[tokio::test]
#[serial(claude_code)]
async fn cancelled_native_control_ignores_late_permission_response() {
    let fixture = Fixture::new("permission-cancelled");
    let mut handle = fixture.handle();
    handle.send(payload(fixture.root.path(), 1, Vec::new()));
    let mut history = Vec::new();
    let mut pending = None;
    loop {
        match output(&mut handle).await {
            EngineOutput::Event(EngineEvent::HistoryAppended { delta, .. }) => {
                history.extend(delta.items)
            }
            EngineOutput::HostCall(HostCall::NativeToolEvaluation { reply, .. }) => {
                pending = Some(reply)
            }
            EngineOutput::Event(EngineEvent::TextDelta { .. }) => {
                let _ = pending.take().unwrap().send(ToolEvaluation {
                    decision: Decision::Allow,
                    metadata: Default::default(),
                });
                break;
            }
            _ => {}
        }
    }
    finish(&mut handle, &mut history).await;
    assert!(!fixture
        .calls()
        .iter()
        .any(|call| call["response"]["request_id"] == "cancelled-permission"));
    let invocation = history
        .iter()
        .find_map(|item| match item {
            HistoryItem::Assistant(step) => step.invocations.first(),
            _ => None,
        })
        .unwrap();
    assert!(invocation.result.is_error);
}

#[tokio::test]
#[serial(claude_code)]
async fn ephemeral_turns_keep_context_without_native_persistence_or_links() {
    let fixture = Fixture::new("text");
    let mut handle = fixture.handle();
    let mut history = Vec::new();
    for turn in 1..=2 {
        let UiCommand::StartTurn(mut request) = payload(fixture.root.path(), turn, history.clone())
        else {
            unreachable!()
        };
        request.ephemeral = true;
        handle.send(UiCommand::StartTurn(request));
        finish(&mut handle, &mut history).await;
    }
    assert!(!fixture.root.path().join("sessions/claude-code").exists());
    let calls = fixture.calls();
    let launches: Vec<_> = calls
        .iter()
        .filter(|call| call.get("argv").is_some())
        .collect();
    assert_eq!(launches.len(), 1, "the live process keeps the context");
    let args = launches[0]["argv"].as_array().unwrap();
    assert!(args.iter().any(|arg| arg == "--no-session-persistence"));
    assert!(!args.iter().any(|arg| arg == "--safe-mode"));
    let prompt = calls.iter().rfind(|call| call["type"] == "user").unwrap();
    assert_eq!(prompt["message"]["content"][0]["text"], "turn 2");
}

#[tokio::test]
#[serial(claude_code)]
async fn turns_share_one_process_until_launch_settings_change() {
    let fixture = Fixture::new("text");
    let mut handle = fixture.handle();
    let mut history = Vec::new();
    for turn in 1..=4 {
        let UiCommand::StartTurn(mut request) = payload(fixture.root.path(), turn, history.clone())
        else {
            unreachable!()
        };
        if turn >= 2 {
            request.mode = protocol::AgentMode::parse("plan").unwrap();
        }
        request.reasoning_effort = match turn {
            3 => ReasoningEffort::High,
            4 => ReasoningEffort::Low,
            _ => ReasoningEffort::Off,
        };
        handle.send(UiCommand::StartTurn(request));
        finish(&mut handle, &mut history).await;
    }
    let calls = fixture.calls();
    let launches: Vec<_> = calls
        .iter()
        .filter(|call| call.get("argv").is_some())
        .collect();
    assert_eq!(launches.len(), 2, "only turning thinking on relaunches");
    assert!(launches[1]["argv"]
        .as_array()
        .unwrap()
        .iter()
        .any(|arg| arg.as_str().is_some_and(|arg| arg.starts_with("--resume="))));
    assert_eq!(
        calls
            .iter()
            .filter(|call| call["request"]["subtype"] == "set_permission_mode")
            .count(),
        1,
        "a live process switches modes in place"
    );
    let flags: Vec<_> = calls
        .iter()
        .filter(|call| call["request"]["subtype"] == "apply_flag_settings")
        .map(|call| &call["request"]["settings"])
        .collect();
    assert_eq!(
        flags,
        [&json!({"effortLevel": "low"})],
        "effort changes in place"
    );
    let prompts: Vec<_> = calls
        .iter()
        .filter(|call| call["type"] == "user")
        .map(|call| call["message"]["content"][0]["text"].as_str().unwrap())
        .collect();
    assert_eq!(prompts, ["turn 1", "turn 2", "turn 3", "turn 4"]);
}

#[tokio::test]
#[serial(claude_code)]
async fn live_model_switches_are_tracked_for_the_next_turn() {
    let fixture = Fixture::new("text");
    let mut handle = fixture.handle();
    let mut history = Vec::new();
    let UiCommand::StartTurn(request) = payload(fixture.root.path(), 1, history.clone()) else {
        unreachable!()
    };
    let mut target = request.model_target.clone();
    handle.send(UiCommand::StartTurn(request));
    finish(&mut handle, &mut history).await;
    target.model = "opus".into();
    handle.send(UiCommand::SetTurnModel {
        target: Box::new(target),
        system_prompt: String::new(),
    });
    let recount = loop {
        match output(&mut handle).await {
            EngineOutput::Event(EngineEvent::ContextTokens { tokens }) => break tokens,
            EngineOutput::Event(EngineEvent::TurnError { message, .. }) => panic!("{message}"),
            _ => {}
        }
    };
    assert_eq!(
        recount, 1234,
        "a switch recounts the context for the new model"
    );
    handle.send(payload(fixture.root.path(), 2, history.clone()));
    finish(&mut handle, &mut history).await;
    let models: Vec<_> = fixture
        .calls()
        .iter()
        .filter(|call| call["request"]["subtype"] == "set_model")
        .map(|call| call["request"]["model"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(models, ["opus", "sonnet"]);
}

#[tokio::test]
#[serial(claude_code)]
async fn rewinding_to_an_earlier_answer_forks_the_native_session_there() {
    let fixture = Fixture::new("text");
    let mut handle = fixture.handle();
    let mut history = Vec::new();
    handle.send(payload(fixture.root.path(), 1, history.clone()));
    finish(&mut handle, &mut history).await;
    let first_turn = history.len();
    handle.send(payload(fixture.root.path(), 2, history.clone()));
    finish(&mut handle, &mut history).await;

    history.truncate(first_turn);
    handle.send(payload(fixture.root.path(), 3, history.clone()));
    finish(&mut handle, &mut history).await;
    let calls = fixture.calls();
    let launches: Vec<_> = calls
        .iter()
        .filter_map(|call| call["argv"].as_array())
        .collect();
    assert_eq!(launches.len(), 2, "the rewind relaunches once");
    let has = |arg: &str| launches[1].iter().any(|value| value == arg);
    assert!(launches[1]
        .iter()
        .any(|arg| arg.as_str().is_some_and(|arg| arg.starts_with("--resume="))));
    assert!(has("--resume-session-at=final") && has("--fork-session"));
    let prompt = calls.iter().rfind(|call| call["type"] == "user").unwrap();
    assert_eq!(prompt["message"]["content"][0]["text"], "turn 3");
}

fn allow(reply: oneshot::Sender<ToolEvaluation>) {
    let _ = reply.send(ToolEvaluation {
        decision: Decision::Allow,
        metadata: Default::default(),
    });
}

fn read_shell() -> UiCommand {
    UiCommand::ReadNativeProcess {
        session_id: "test-session".into(),
        id: "claude-code:shell-task".into(),
    }
}

#[tokio::test]
#[serial(claude_code)]
async fn background_shell_keeps_claude_code_until_it_wakes_smelt() {
    let fixture = Fixture::new("shell-background");
    let mut config = EngineConfig::new(
        fixture.root.path().into(),
        Arc::new(crate::clock::RealClock),
    );
    config.host_callbacks = HostCallbacks::Enabled;
    // Shorter than the first output read, which needs the process.
    config.native_idle_timeout = Duration::from_millis(200);
    let mut handle = crate::start(config, Box::new(crate::tools::EmptyDispatcher));
    handle.send(payload(fixture.root.path(), 1, Vec::new()));
    let mut lists = Vec::new();
    let mut history = Vec::new();
    let mut completions = 0;
    let mut woken = false;
    loop {
        match output(&mut handle).await {
            EngineOutput::Event(EngineEvent::NativeProcesses { processes }) => {
                lists.push(processes)
            }
            EngineOutput::Event(EngineEvent::HistoryAppended { delta, .. }) => {
                history.extend(delta.items)
            }
            EngineOutput::Event(EngineEvent::TurnComplete { .. }) => {
                completions += 1;
                // Outlast the idle timeout before looking at the output.
                tokio::time::sleep(Duration::from_millis(400)).await;
                handle.send(read_shell());
            }
            EngineOutput::Event(EngineEvent::NativeSessionState { active: false, .. }) => {
                panic!("Claude Code was released while its shell ran")
            }
            EngineOutput::Event(EngineEvent::NativeSessionState { working, .. }) => {
                if working && completions == 1 {
                    woken = true;
                } else if !working && woken {
                    break;
                }
            }
            EngineOutput::HostCall(HostCall::NativeToolEvaluation { reply, .. }) => allow(reply),
            EngineOutput::Event(EngineEvent::TurnError { message, .. }) => panic!("{message}"),
            _ => {}
        }
    }
    assert_eq!(completions, 1);
    // The level lists the shell by its description before it starts.
    let started = &lists.iter().rev().find(|list| !list.is_empty()).unwrap()[0];
    assert_eq!(started.id, "claude-code:shell-task");
    assert_eq!(started.command, "make serve");
    assert!(started.background);
    assert!(lists
        .iter()
        .any(|list| list.first().is_some_and(|shell| shell.output == "tick 1\n")));
    assert_eq!(lists.last().unwrap(), &Vec::new());
    let history = serde_json::to_string(&history).unwrap();
    assert_eq!(history.matches("SHELL_WAKE_REPLY").count(), 1);
}

#[tokio::test]
#[serial(claude_code)]
async fn headless_turns_wait_for_background_shells_and_their_wake() {
    let fixture = Fixture::new("shell-background");
    let mut handle = fixture.handle();
    let UiCommand::StartTurn(mut turn) = payload(fixture.root.path(), 1, Vec::new()) else {
        unreachable!()
    };
    turn.tool_visibility = protocol::ToolVisibility::Headless;
    handle.send(UiCommand::StartTurn(turn));
    let mut history = Vec::new();
    loop {
        match output(&mut handle).await {
            EngineOutput::Event(EngineEvent::HistoryAppended { delta, .. }) => {
                history.extend(delta.items)
            }
            EngineOutput::Event(EngineEvent::NativeProcesses { processes })
                if !processes.is_empty() =>
            {
                handle.send(read_shell())
            }
            EngineOutput::HostCall(HostCall::NativeToolEvaluation { reply, .. }) => allow(reply),
            EngineOutput::Event(EngineEvent::TurnComplete { .. }) => break,
            EngineOutput::Event(EngineEvent::TurnError { message, .. }) => panic!("{message}"),
            _ => {}
        }
    }
    let history = serde_json::to_string(&history).unwrap();
    assert_eq!(history.matches("SHELL_WAKE_REPLY").count(), 1);
}

#[tokio::test]
#[serial(claude_code)]
async fn foreground_shell_output_streams_into_its_tool_block() {
    let fixture = Fixture::new("shell-foreground");
    let mut handle = fixture.handle();
    handle.send(payload(fixture.root.path(), 1, Vec::new()));
    let mut lines = Vec::new();
    let mut foreground = false;
    loop {
        match output(&mut handle).await {
            EngineOutput::Event(EngineEvent::ToolOutput { call_id, line, .. }) => {
                assert_eq!(call_id, "shell-call");
                lines.push(line);
            }
            EngineOutput::Event(EngineEvent::NativeProcesses { processes }) => {
                foreground |= processes.first().is_some_and(|shell| !shell.background);
            }
            EngineOutput::HostCall(HostCall::NativeToolEvaluation { reply, .. }) => allow(reply),
            EngineOutput::Event(EngineEvent::TurnComplete { .. }) => break,
            EngineOutput::Event(EngineEvent::TurnError { message, .. }) => panic!("{message}"),
            _ => {}
        }
    }
    assert!(foreground);
    assert_eq!(lines, ["building", "halfway", "done"]);
}

#[tokio::test]
#[serial(claude_code)]
async fn retiring_claude_code_stops_its_shells_instead_of_orphaning_them() {
    let fixture = Fixture::new("shell-retire");
    let mut handle = fixture.handle();
    handle.send(payload(fixture.root.path(), 1, Vec::new()));
    let mut history = Vec::new();
    finish(&mut handle, &mut history).await;
    // Another session needs its own process, so this one is retired.
    let UiCommand::StartTurn(mut other) = payload(fixture.root.path(), 1, Vec::new()) else {
        unreachable!()
    };
    other.session_id = "other-session".into();
    handle.send(UiCommand::StartTurn(other));
    let mut cleared = false;
    loop {
        match output(&mut handle).await {
            EngineOutput::Event(EngineEvent::NativeProcesses { processes }) => {
                cleared = processes.is_empty()
            }
            EngineOutput::Event(EngineEvent::NativeSessionState { active: false, .. }) => break,
            _ => {}
        }
    }
    assert!(cleared);
    assert!(fixture
        .calls()
        .iter()
        .any(|call| call["request"] == json!({"subtype":"stop_task", "task_id":"shell-task"})));
}

#[tokio::test]
#[serial(claude_code)]
async fn shutting_the_engine_down_stops_claude_code_shells_before_it_returns() {
    let fixture = Fixture::new("shell-retire");
    let mut handle = fixture.handle();
    handle.send(payload(fixture.root.path(), 1, Vec::new()));
    let mut history = Vec::new();
    finish(&mut handle, &mut history).await;
    handle.shutdown().await;
    assert!(fixture
        .calls()
        .iter()
        .any(|call| call["request"] == json!({"subtype":"stop_task", "task_id":"shell-task"})));
}
