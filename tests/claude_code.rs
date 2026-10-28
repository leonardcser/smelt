#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn run(scenario: &str, extra_config: &str) -> (std::process::Output, Vec<serde_json::Value>) {
    run_with_args(scenario, extra_config, &[])
}

fn run_with_args(
    scenario: &str,
    extra_config: &str,
    args: &[&str],
) -> (std::process::Output, Vec<serde_json::Value>) {
    let root = tempfile::tempdir().unwrap();
    let native_cwd = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("probe.txt"), "inside").unwrap();
    std::fs::write(native_cwd.path().join("probe.txt"), "outside").unwrap();
    let fixture = root.path().join("claude");
    std::fs::write(&fixture, include_str!("fixtures/claude_code.py")).unwrap();
    std::fs::set_permissions(&fixture, std::fs::Permissions::from_mode(0o700)).unwrap();
    let config = root.path().join("init.lua");
    std::fs::write(
        &config,
        format!(
            r#"
smelt.settings.autoupgrade = "off"
smelt.settings.auto_continue = "off"
smelt.settings.auto_reload = false
smelt.settings.show_prediction = false
smelt.settings.auto_compact = false
{extra_config}
"#
        ),
    )
    .unwrap();
    let log = root.path().join("claude.jsonl");
    let mut child = Command::new(env!("CARGO_BIN_EXE_smelt"))
        .args([
            "--headless",
            "--type",
            "claude-code",
            "--model",
            "sonnet",
            "--format",
            "json",
            "hello",
        ])
        .args(args)
        .arg("--config")
        .arg(&config)
        .env("SMELT_CLAUDE_CODE_EXECUTABLE", fixture)
        .env("SMELT_CLAUDE_FIXTURE", scenario)
        .env("SMELT_CLAUDE_FIXTURE_CWD", native_cwd.path())
        .env("SMELT_CLAUDE_FIXTURE_LOG", &log)
        .env("HOME", root.path())
        .env("XDG_CONFIG_HOME", root.path().join("config"))
        .env("XDG_STATE_HOME", root.path().join("state"))
        .env("XDG_DATA_HOME", root.path().join("data"))
        .env("XDG_CACHE_HOME", root.path().join("cache"))
        .env_remove("CLAUDE_CONFIG_DIR")
        .env_remove("ANTHROPIC_API_KEY")
        .current_dir(root.path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let start = Instant::now();
    while child.try_wait().unwrap().is_none() {
        if start.elapsed() > Duration::from_secs(20) {
            child.kill().unwrap();
            let output = child.wait_with_output().unwrap();
            panic!(
                "smelt hung: {}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let output = child.wait_with_output().unwrap();
    let calls = std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    (output, calls)
}

#[test]
fn claude_code_catalog_is_native_first_and_keeps_custom_tools() {
    let (output, calls) = run(
        "catalog",
        r#"
smelt.tools.register({ name = "fixture_tool", description = "custom capability", parameters = { type = "object" }, execute = function() return "ok" end })
smelt.agent.add_system_prompt("fixture plugin guidance")
"#,
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let response = calls
        .iter()
        .find(|call| call["response"]["request_id"] == "mcp-list")
        .unwrap();
    let tools = response["response"]["response"]["mcp_response"]["result"]["tools"]
        .as_array()
        .unwrap();
    assert!(tools.iter().any(|tool| tool["name"] == "fixture_tool"));
    assert!(tools.iter().any(|tool| tool["name"] == "load_skill"));
    for name in [
        "read_file",
        "write_file",
        "edit_file",
        "edit_notebook",
        "bash",
        "glob",
        "grep",
        "web_fetch",
        "web_search",
        "read_process_output",
        "stop_process",
        "ask_user_question",
        "present_plan",
    ] {
        assert!(
            !tools.iter().any(|tool| tool["name"] == name),
            "Duplicate or internal tool exposed: {name}"
        );
    }
    let args = calls[0]["argv"].as_array().unwrap();
    let append = args[args
        .iter()
        .position(|arg| arg == "--append-system-prompt")
        .unwrap()
        + 1]
    .as_str()
    .unwrap();
    assert!(append.contains("fixture plugin guidance"));
    assert!(append.contains("Do not create a pull request, push a branch"));
    assert!(!append.contains("Use dedicated tools over bash"));
}

#[test]
fn claude_code_explicit_replacement_owns_native_capability() {
    let (output, calls) = run(
        "replacement",
        r#"
smelt.tools.register({ name = "my_shell", description = "custom shell", parameters = { type = "object" }, claude_code = { replaces = { "Bash" } }, permission_defaults = { normal = "allow" }, execute = function() return "replacement ran" end })
"#,
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let args = calls[0]["argv"].as_array().unwrap();
    let disabled = args[args
        .iter()
        .position(|arg| arg == "--disallowedTools")
        .unwrap()
        + 1]
    .as_str()
    .unwrap();
    assert_eq!(disabled, "Bash,EnterWorktree");
    let response = calls
        .iter()
        .find(|call| call["response"]["request_id"] == "ownership-call")
        .unwrap();
    assert_eq!(
        response["response"]["response"]["mcp_response"]["result"]["content"][0]["text"],
        "replacement ran"
    );
    assert!(calls
        .iter()
        .any(|call| call["response"]["request_id"] == "replaced-native"));
}

#[test]
fn claude_code_headless_does_not_fall_back_from_a_ui_only_replacement() {
    let (output, calls) = run(
        "permission",
        r#"
smelt.tools.register({ name = "ui_shell", headless = false, claude_code = { replaces = { "Bash" } }, execute = function() return "UI shell" end })
"#,
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let args = calls[0]["argv"].as_array().unwrap();
    assert_eq!(
        args[args
            .iter()
            .position(|arg| arg == "--disallowedTools")
            .unwrap()
            + 1],
        "Bash,EnterWorktree"
    );
    let response = calls
        .iter()
        .find(|call| call["response"]["request_id"] == "permission")
        .unwrap();
    assert_eq!(
        response["response"]["response"]["hookSpecificOutput"]["permissionDecision"],
        "deny"
    );
}

#[test]
fn claude_code_system_override_replaces_only_native_preset() {
    for replacement in ["custom native persona", ""] {
        let (output, calls) = run_with_args(
            "text",
            "smelt.agent.add_system_prompt(\"plugin addition\")",
            &["--system-prompt", replacement],
        );
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
        let args = calls[0]["argv"].as_array().unwrap();
        assert_eq!(
            args[args
                .iter()
                .position(|arg| arg == "--system-prompt")
                .unwrap()
                + 1],
            replacement
        );
        let append = args[args
            .iter()
            .position(|arg| arg == "--append-system-prompt")
            .unwrap()
            + 1]
        .as_str()
        .unwrap();
        assert!(append.contains("plugin addition"));
        assert!(append.contains("Do not create a pull request, push a branch"));
    }
}

#[test]
fn claude_code_hidden_tools_cannot_be_called_through_mcp() {
    let (output, calls) = run("hidden", "");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let response = calls
        .iter()
        .find(|call| call["response"]["request_id"] == "ownership-call")
        .unwrap();
    assert_eq!(
        response["response"]["response"]["mcp_response"]["result"]["isError"],
        true
    );
}

#[test]
fn claude_code_plan_artifact_grants_do_not_override_explicit_denies() {
    for (config, expected) in [
        ("", "allow"),
        (
            r#"smelt.permissions.extend({ plan = { tools = { deny = { "write_file" } } } })"#,
            "deny",
        ),
    ] {
        let (output, calls) = run_with_args("plan-write-denied", config, &["--mode", "plan"]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
        let args = calls[0]["argv"].as_array().unwrap();
        assert_eq!(
            args[args
                .iter()
                .position(|arg| arg == "--permission-mode")
                .unwrap()
                + 1],
            "plan"
        );
        let response = calls
            .iter()
            .find(|call| call["response"]["request_id"] == "plan-write")
            .unwrap();
        assert_eq!(
            response["response"]["response"]["hookSpecificOutput"]["permissionDecision"],
            expected
        );
    }
}

#[test]
fn claude_code_headless_interactions_deny_without_user_approval() {
    for scenario in ["question", "plan", "plan-missing"] {
        let (output, calls) = run(scenario, "");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
        let response = calls
            .iter()
            .find(|call| call["response"]["request_id"] == "interaction")
            .unwrap();
        assert_eq!(response["response"]["response"]["behavior"], "deny");
    }
}

#[test]
fn claude_code_headless_uses_native_protocol_and_streams_once() {
    for scenario in ["text", "fragmented"] {
        let (output, calls) = run(scenario, "");
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let text = String::from_utf8_lossy(&output.stdout);
        let events: Vec<serde_json::Value> = text
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect();
        assert_eq!(
            events
                .iter()
                .filter(|event| event.get("TextDelta").is_some())
                .count(),
            1,
            "{text}"
        );
        assert!(
            events
                .iter()
                .any(|event| event.get("TurnComplete").is_some()),
            "{text}"
        );
        assert_eq!(
            calls.iter().filter(|call| call["type"] == "user").count(),
            1
        );
        assert!(!calls[0]["argv"]
            .as_array()
            .unwrap()
            .iter()
            .any(|arg| arg == "--system-prompt"));
    }
}

#[test]
fn claude_code_errors_are_actionable_and_do_not_hang() {
    for scenario in ["error", "invalid", "early-exit"] {
        let (output, _) = run(scenario, "");
        assert!(!output.status.success());
        let text = String::from_utf8_lossy(&output.stdout);
        assert!(text.contains("TurnError"), "{text}");
        assert!(!text.contains("fixture answer"));
    }
}

#[test]
fn claude_code_foreground_child_uses_policy_and_has_an_isolated_transcript() {
    let (output, calls) = run(
        "subagent-foreground",
        r#"
smelt.permissions.extend({ default = { tools = { allow = { "Agent" }, deny = { "read_file" } } } })
"#,
    );
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let gate = calls
        .iter()
        .find(|call| call["response"]["request_id"] == "child-gate")
        .unwrap();
    assert_eq!(
        gate["response"]["response"]["hookSpecificOutput"]["permissionDecision"],
        "deny"
    );
    let events: Vec<serde_json::Value> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    assert!(events
        .iter()
        .any(|event| event["AgentUpdated"]["agent"]["status"] == "completed"));
    assert!(events.iter().any(
        |event| event["AgentOutput"]["event"]["ToolFinished"]["result"]["content"]
            == "CHILD_READ_DENIED"
    ));
    assert!(events
        .iter()
        .filter(|event| event.get("HistoryAppended").is_some())
        .all(|event| !event.to_string().contains("CHILD_")));
    assert!(!events
        .iter()
        .any(|event| event["ToolStarted"]["call_id"] == "child-read"));
}

#[test]
fn claude_code_headless_waits_for_background_child_and_wake_reply() {
    let (output, calls) = run(
        "subagent-background",
        r#"
smelt.permissions.extend({ default = { tools = { allow = { "Agent" }, deny = { "read_file" } } } })
"#,
    );
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let gate = calls
        .iter()
        .find(|call| call["response"]["request_id"] == "late-child-gate")
        .expect("late child permission request was answered");
    assert_eq!(
        gate["response"]["response"]["hookSpecificOutput"]["permissionDecision"],
        "deny"
    );
    let events: Vec<serde_json::Value> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    let completions: Vec<_> = events
        .iter()
        .enumerate()
        .filter(|(_, event)| event.get("TurnComplete").is_some())
        .map(|(index, _)| index)
        .collect();
    assert_eq!(completions.len(), 1, "{events:?}");
    let position = |matches: &dyn Fn(&serde_json::Value) -> bool| {
        events.iter().position(matches).expect("expected event")
    };
    let child_done = position(&|event| event["AgentUpdated"]["agent"]["status"] == "completed");
    let wake = position(&|event| {
        event.get("HistoryAppended").is_some()
            && event.to_string().contains("BACKGROUND_WAKE_REPLY")
    });
    assert!(child_done < completions[0] && wake < completions[0]);
    assert_eq!(
        events
            .iter()
            .filter(|event| event.to_string().contains("BACKGROUND_WAKE_REPLY")
                && event.get("HistoryAppended").is_some())
            .count(),
        1
    );
    assert!(events
        .iter()
        .filter(|event| event.get("HistoryAppended").is_some())
        .all(|event| !event.to_string().contains("CHILD_")));
}

#[test]
fn claude_code_native_tools_pass_through_smelt_policy() {
    for (expected, config) in [
        (
            "deny",
            r#"smelt.permissions.extend({ default = { patterns = { bash = { deny = { "echo fixture-native-tool" } } } } })"#,
        ),
        (
            "allow",
            r#"smelt.permissions.extend({ default = { patterns = { bash = { allow = { "echo fixture-native-tool" } } } } })"#,
        ),
    ] {
        let (output, calls) = run("permission", config);
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let reply = calls
            .iter()
            .find(|call| call["response"]["request_id"] == "permission")
            .unwrap();
        assert_eq!(
            reply["response"]["response"]["hookSpecificOutput"]["permissionDecision"],
            expected
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("ToolFinished"));
    }
}

#[test]
fn claude_code_relative_paths_use_native_cwd_without_expanding_workspace_trust() {
    let (output, calls) = run("cwd-permission", "");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let reply = calls
        .iter()
        .find(|call| call["response"]["request_id"] == "relative-permission")
        .unwrap();
    assert_eq!(
        reply["response"]["response"]["hookSpecificOutput"]["permissionDecision"],
        "deny"
    );
}

#[test]
fn claude_code_mcp_bridge_executes_registered_smelt_tool() {
    let (output, calls) = run(
        "mcp-parallel",
        r#"
smelt.tools.register({
  name = "fixture_tool", description = "fixture", parameters = { type = "object", properties = { text = { type = "string" } } },
  execute = function(args) return { content = args.text, metadata = { fixture = args.text }, display_content = { summary = "display " .. args.text } } end,
})
smelt.permissions.extend({ default = { tools = { allow = { "fixture_tool" } } } })
"#,
    );
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(calls
        .iter()
        .any(|call| call["response"]["request_id"] == "mcp-call"));
    let events: Vec<serde_json::Value> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    for id in ["first", "second"] {
        let finished = events
            .iter()
            .find_map(|event| {
                event
                    .get("ToolFinished")
                    .filter(|tool| tool["call_id"] == id)
            })
            .unwrap();
        assert_eq!(finished["result"]["content"], id);
        assert_eq!(finished["result"]["metadata"]["fixture"], id);
        assert_eq!(
            finished["result"]["display_content"][0]["content"],
            format!("display {id}")
        );
    }
}
