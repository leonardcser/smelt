#!/usr/bin/env python3
"""Deterministic Claude Code subprocess contract fixture. No network or credentials."""
import json
import os
import queue
import sys
import threading
import time

SESSION = "12345678-1234-4234-8234-123456789abc"
SCENARIO = os.environ.get("SMELT_CLAUDE_FIXTURE", "text")
LOG = os.environ.get("SMELT_CLAUDE_FIXTURE_LOG")
TOOLS_ENABLED = sys.argv[sys.argv.index("--tools") + 1] == "default"
MODEL = sys.argv[sys.argv.index("--model") + 1]
# Messages read early that the main loop takes next.
PENDING = []
# Lines from stdin, read on a thread so a wait for the next one can time out.
INBOX = queue.Queue()


def read_stdin():
    for line in iter(sys.stdin.readline, ""):
        INBOX.put(line)
    INBOX.put("")


threading.Thread(target=read_stdin, daemon=True).start()


def log(value):
    if LOG:
        with open(LOG, "a", encoding="utf-8") as output:
            output.write(json.dumps(value) + "\n")


def send(value):
    data = json.dumps(value, ensure_ascii=False) + "\n"
    if SCENARIO == "fragmented":
        raw = data.encode("utf-8")
        for byte in raw:
            sys.stdout.buffer.write(bytes([byte]))
            sys.stdout.buffer.flush()
            time.sleep(0.0002)
    else:
        print(data, end="", flush=True)


def receive(timeout=None):
    """The next message. Raises queue.Empty when none arrives within the timeout."""
    line = INBOX.get(timeout=timeout)
    if not line:
        sys.exit(0)
    value = json.loads(line)
    log(value)
    return value


def control(request_id, request):
    send({"type": "control_request", "request_id": request_id, "request": request})
    while True:
        response = receive()
        if response.get("type") == "control_response" and response["response"]["request_id"] == request_id:
            return response["response"]["response"]
        if response.get("request", {}).get("subtype") == "interrupt":
            sys.exit(0)


def wait_for_dialog(expected):
    deadline = time.monotonic() + 3
    index = 0
    while time.monotonic() < deadline:
        index += 1
        probe = "dialog-" + expected + str(index)
        result = control(probe, {"subtype": "mcp_message", "server_name": "smelt", "message": {"jsonrpc": "2.0", "id": probe, "method": "tools/call", "params": {"name": "fixture_dialog_state", "arguments": {}}}})
        if result["mcp_response"]["result"]["content"][0]["text"] == expected:
            return
        time.sleep(0.03)
    raise AssertionError("Dialog never became " + expected)


def assistant(blocks, uuid="assistant"):
    send({"type": "assistant", "uuid": uuid, "message": {"id": uuid, "content": blocks}, "parent_tool_use_id": None})


def task_output(task_id, output):
    """Answers smelt's next read of a shell task's output."""
    while True:
        request = receive()
        if request.get("request", {}) == {"subtype": "get_task_output", "task_id": task_id}:
            break
    send({"type": "control_response", "response": {"subtype": "success", "request_id": request["request_id"], "response": {
        "output": output, "total_bytes": len(output.encode()), "truncated": False,
    }}})


def tool_result(call_id, content, is_error=False):
    send({"type": "user", "message": {"content": [{"type": "tool_result", "tool_use_id": call_id, "content": content, "is_error": is_error}]}})


log({"argv": sys.argv[1:], "cwd": os.getcwd(), "pid": os.getpid()})
assert "--input-format" in sys.argv and "stream-json" in sys.argv
assert "--permission-prompt-tool" in sys.argv and "stdio" in sys.argv
assert "--dangerously-skip-permissions" not in sys.argv
init = receive()
assert init["request"]["subtype"] == "initialize"
if SCENARIO.startswith("subagent") and TOOLS_ENABLED:
    assert init["request"]["forwardSubagentText"] is True
if SCENARIO == "invalid":
    print("{broken", flush=True)
    sys.exit(1)
if SCENARIO == "early-exit":
    print("Error: fixture crashed with token sk-ant-fixture-secret", file=sys.stderr, flush=True)
    sys.exit(2)
send({"type": "control_response", "response": {"subtype": "success", "request_id": init["request_id"], "response": {"models": [
    {"value": "default", "displayName": "Default (recommended)"},
    {"value": "opus", "displayName": "Opus 5.5", "supportsEffort": True, "supportedEffortLevels": ["low", "high"]},
]}}})
while True:
    prompt = PENDING.pop(0) if PENDING else receive()
    if prompt.get("type") != "user":
        if prompt.get("request", {}).get("subtype") == "interrupt":
            # An idle Claude Code has nothing to interrupt.
            send({"type": "control_response", "response": {"subtype": "success", "request_id": prompt["request_id"], "response": {"still_queued": []}}})
        if prompt.get("request", {}).get("subtype") == "get_usage":
            send({"type": "control_response", "response": {"subtype": "success", "request_id": prompt["request_id"], "response": {
                "rate_limits_available": True,
                "rate_limits": {"limits": [{"kind": "session", "group": "session", "percent": 42, "resets_at": None, "scope": None}]},
            }}})
        if prompt.get("request", {}).get("subtype") == "get_context_usage":
            # Like Claude Code, this reports the launch model's window even after set_model.
            send({"type": "control_response", "response": {"subtype": "success", "request_id": prompt["request_id"], "response": {
                "totalTokens": 1234, "maxTokens": 1000000 if MODEL == "opus" else 200000,
            }}})
        continue
    if prompt.get("uuid"):
        # Claude Code says when it starts on a message, and with
        # --replay-user-messages echoes it later. A wake test leaves the echo
        # out, so only the start can start the turn.
        send({"type": "command_lifecycle", "command_uuid": prompt["uuid"], "state": "started"})
        if not SCENARIO.startswith("monitor-wake"):
            send({"type": "user", "isReplay": True, "uuid": prompt["uuid"], "message": prompt["message"], "parent_tool_use_id": None})
    send({"type": "system", "subtype": "session_state_changed", "state": "running", "sdk_host_only": True})
    if SCENARIO == "hang":
        while True:
            receive()
    if SCENARIO == "error":
        send({"type": "result", "subtype": "success", "is_error": True, "result": "Not logged in. Run claude auth login."})
        continue
    if SCENARIO == "permission-native-cancel" and TOOLS_ENABLED:
        args = {"command": "echo fixture-native-tool"}
        assistant([{"type": "tool_use", "id": "cancelled-tool", "name": "Bash", "input": args}])
        send({"type": "control_request", "request_id": "interaction", "request": {"subtype": "hook_callback", "callback_id": "smelt-permission", "tool_use_id": "cancelled-tool", "input": {"tool_name": "Bash", "tool_input": args}}})
        wait_for_dialog("open")
        send({"type": "control_cancel_request", "request_id": "interaction"})
        tool_result("cancelled-tool", "Native permission cancelled", True)
        wait_for_dialog("closed")
    if SCENARIO == "permission-cancelled" and TOOLS_ENABLED:
        args = {"command": "echo fixture-native-tool"}
        assistant([{"type": "tool_use", "id": "cancelled-tool", "name": "Bash", "input": args}])
        send({"type": "control_request", "request_id": "cancelled-permission", "request": {"subtype": "hook_callback", "callback_id": "smelt-permission", "tool_use_id": "cancelled-tool", "input": {"tool_name": "Bash", "tool_input": args}}})
        send({"type": "control_cancel_request", "request_id": "cancelled-permission"})
    if SCENARIO == "cwd-permission" and TOOLS_ENABLED:
        args = {"file_path": "probe.txt"}
        assistant([{"type": "tool_use", "id": "relative-tool", "name": "Read", "input": args}])
        answer = control("relative-permission", {"subtype": "hook_callback", "callback_id": "smelt-permission", "tool_use_id": "relative-tool", "input": {"tool_name": "Read", "tool_input": args, "cwd": os.environ["SMELT_CLAUDE_FIXTURE_CWD"]}})
        allowed = answer["hookSpecificOutput"]["permissionDecision"] == "allow"
        tool_result("relative-tool", "outside" if allowed else "blocked", not allowed)
    if SCENARIO == "permission" and TOOLS_ENABLED:
        args = {"command": "echo fixture-native-tool"}
        assistant([{"type": "tool_use", "id": "native-tool", "name": "Bash", "input": args}])
        answer = control("permission", {"subtype": "hook_callback", "callback_id": "smelt-permission", "tool_use_id": "native-tool", "input": {"hook_event_name": "PreToolUse", "tool_name": "Bash", "tool_input": args}})
        allowed = answer["hookSpecificOutput"]["permissionDecision"] == "allow"
        tool_result("native-tool", "fixture-native-tool" if allowed else "blocked", not allowed)
    if SCENARIO in ("plan-file", "plan-save", "plan-write-denied") and TOOLS_ENABLED:
        directory = json.loads(sys.argv[sys.argv.index("--settings") + 1])["plansDirectory"]
        plan_path = os.path.join(directory, "fixture.md")
        os.makedirs(directory, exist_ok=True)
        args = {"file_path": plan_path, "content": "# Fixture plan\n\nImplement the selected fixture features."}
        assistant([{"type": "tool_use", "id": "plan-write", "name": "Write", "input": args}], "plan-write")
        result = control("plan-write", {"subtype": "hook_callback", "callback_id": "smelt-permission", "input": {"tool_name": "Write", "tool_input": args, "tool_use_id": "plan-write"}})
        allowed = result["hookSpecificOutput"]["permissionDecision"] == "allow"
        if allowed:
            with open(plan_path, "w") as output:
                output.write(args["content"])
        tool_result("plan-write", "Plan saved" if allowed else "Plan write denied", not allowed)
        if allowed and SCENARIO == "plan-file":
            notes = os.path.join(directory, "notes.md")
            with open(notes, "w") as output:
                output.write("# Unrelated notes")
            read_args = {"file_path": notes}
            assistant([{"type": "tool_use", "id": "plan-notes", "name": "Read", "input": read_args}], "plan-notes")
            read = control("plan-notes", {"subtype": "hook_callback", "callback_id": "smelt-permission", "input": {"tool_name": "Read", "tool_input": read_args, "tool_use_id": "plan-notes"}})
            tool_result("plan-notes", "# Unrelated notes", read["hookSpecificOutput"]["permissionDecision"] != "allow")
            failed_args = {"file_path": notes, "content": "# Unrelated notes"}
            assistant([{"type": "tool_use", "id": "plan-failed", "name": "Write", "input": failed_args}], "plan-failed")
            failed = control("plan-failed", {"subtype": "hook_callback", "callback_id": "smelt-permission", "input": {"tool_name": "Write", "tool_input": failed_args, "tool_use_id": "plan-failed"}})
            assert failed["hookSpecificOutput"]["permissionDecision"] == "allow", failed
            tool_result("plan-failed", "Fixture write failed", True)
    if SCENARIO in ("question", "question-multi", "question-free", "question-cancel", "question-native-cancel", "plan", "plan-draft", "plan-apply", "plan-dismiss", "plan-missing", "plan-file", "plan-resume") and TOOLS_ENABLED:
        is_question = SCENARIO.startswith("question")
        name = "AskUserQuestion" if is_question else "ExitPlanMode"
        args = {"questions": [{"question": "Choose fixture features", "header": "Features", "options": [{"label": "Alpha", "description": "First feature"}, {"label": "Beta", "description": "Second feature"}], "multiSelect": SCENARIO == "question-multi"}]} if is_question else {"plan": "# Fixture plan\n\nImplement the selected fixture features."}
        if SCENARIO in ("plan-missing", "plan-file", "plan-resume"):
            args = {}
        assistant([{"type": "tool_use", "id": "interaction-tool", "name": name, "input": args}])
        hook = control("interaction-hook", {"subtype": "hook_callback", "callback_id": "smelt-permission", "tool_use_id": "interaction-tool", "input": {"tool_name": name, "tool_input": args}})
        assert hook == {}, hook
        request = {"subtype": "can_use_tool", "tool_name": name, "tool_use_id": "interaction-tool", "input": args}
        if SCENARIO == "question-native-cancel":
            send({"type": "control_request", "request_id": "interaction", "request": request})
            wait_for_dialog("open")
            send({"type": "control_cancel_request", "request_id": "interaction"})
            tool_result("interaction-tool", "Native question cancelled", True)
            wait_for_dialog("closed")
        else:
            result = control("interaction", request)
            tool_result("interaction-tool", json.dumps(result), result["behavior"] != "allow")
    if SCENARIO in ("catalog", "replacement", "hidden") and TOOLS_ENABLED:
        control("mcp-list", {"subtype": "mcp_message", "server_name": "smelt", "message": {"jsonrpc": "2.0", "id": 2, "method": "tools/list"}})
    if SCENARIO in ("replacement", "hidden") and TOOLS_ENABLED:
        name = "my_shell" if SCENARIO == "replacement" else "read_file"
        args = {"command": "echo fixture-native-tool"} if SCENARIO == "replacement" else {"file_path": "probe.txt"}
        assistant([{"type": "tool_use", "id": "ownership-tool", "name": "mcp__smelt__" + name, "input": args}])
        result = control("ownership-call", {"subtype": "mcp_message", "server_name": "smelt", "message": {"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": name, "arguments": args}}})
        tool_result("ownership-tool", json.dumps(result))
        if SCENARIO == "replacement":
            result = control("replaced-native", {"subtype": "hook_callback", "callback_id": "smelt-permission", "input": {"tool_name": "Bash", "tool_input": args}})
            assert result["hookSpecificOutput"]["permissionDecision"] == "deny", result
    if SCENARIO in ("mcp", "mcp-parallel", "move") and TOOLS_ENABLED:
        notification = control("mcp-notification", {"subtype": "mcp_message", "server_name": "smelt", "message": {"jsonrpc": "2.0", "method": "notifications/initialized"}})
        assert "mcp_response" not in notification, notification
        unknown = control("mcp-unknown", {"subtype": "mcp_message", "server_name": "smelt", "message": {"jsonrpc": "2.0", "id": 0, "method": "unknown"}})
        assert unknown["mcp_response"]["error"]["code"] == -32601, unknown
        control("mcp-init", {"subtype": "mcp_message", "server_name": "smelt", "message": {"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2024-11-05"}}})
        tools = control("mcp-list", {"subtype": "mcp_message", "server_name": "smelt", "message": {"jsonrpc": "2.0", "id": 2, "method": "tools/list"}})
        assert any(tool["name"] == "fixture_tool" for tool in tools["mcp_response"]["result"]["tools"])
        assistant([{"type": "tool_use", "id": "mcp-tool", "name": "mcp__smelt__fixture_tool", "input": {"text": "fixture-mcp-ok"}}])
        result = control("mcp-call", {"subtype": "mcp_message", "server_name": "smelt", "message": {"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "fixture_tool", "arguments": {"text": "fixture-mcp-ok"}}}})
        assert result["mcp_response"]["result"]["content"][0]["text"] == "fixture-mcp-ok", result
        tool_result("mcp-tool", "fixture-mcp-ok")
        if SCENARIO == "move":
            # The tool moved the workspace: Claude Code is interrupted, moves
            # once it trusts the directory, and is told to continue.
            interrupt = receive()
            assert interrupt["request"] == {"subtype": "interrupt"}, interrupt
            send({"type": "control_response", "response": {"subtype": "success", "request_id": interrupt["request_id"], "response": {"still_queued": []}}})
            send({"type": "result", "subtype": "error_during_execution", "is_error": True, "terminal_reason": "aborted_streaming", "session_id": SESSION, "usage": {"input_tokens": 1, "output_tokens": 0}})
            asked = receive()
            assert asked["request"]["subtype"] == "set_cwd" and "trust_accepted" not in asked["request"], asked
            directory = asked["request"]["path"]
            send({"type": "control_response", "response": {"subtype": "success", "request_id": asked["request_id"], "response": {"status": "needs_trust", "directory": directory, "trust_root": directory}}})
            trusted = receive()
            assert trusted["request"] == {"subtype": "set_cwd", "path": directory, "trust_accepted": True, "trusted_directory": directory}, trusted
            send({"type": "system", "subtype": "init", "cwd": directory, "session_id": SESSION})
            send({"type": "result", "subtype": "success", "is_error": False, "session_id": SESSION, "result": ""})
            send({"type": "control_response", "response": {"subtype": "success", "request_id": trusted["request_id"], "response": {"status": "ok", "cwd": directory, "changed": True, "transcript_relocated": True}}})
            send({"type": "system", "subtype": "session_state_changed", "state": "idle", "sdk_host_only": True})
            SCENARIO = "text"
            continue
        if SCENARIO == "mcp-parallel":
            assistant([{"type": "tool_use", "id": "first", "name": "mcp__smelt__fixture_tool", "input": {"text": "first"}}, {"type": "tool_use", "id": "second", "name": "mcp__smelt__fixture_tool", "input": {"text": "second"}}], "parallel")
            for call_id in ("second", "first"):
                result = control("mcp-" + call_id, {"subtype": "mcp_message", "server_name": "smelt", "message": {"jsonrpc": "2.0", "id": call_id, "method": "tools/call", "params": {"name": "fixture_tool", "arguments": {"text": call_id}}}})
                assert result["mcp_response"]["result"]["content"][0]["text"] == call_id, result
                tool_result(call_id, call_id)
    if SCENARIO == "subagent-stop" and TOOLS_ENABLED:
        for task in ("one", "two"):
            launch = "launch-" + task
            args = {"description": task, "prompt": "Produce a report"}
            assistant([{"type": "tool_use", "id": launch, "name": "Agent", "input": args}], launch)
            control("gate-" + task, {"subtype": "hook_callback", "callback_id": "smelt-permission", "tool_use_id": launch, "input": {"tool_name": "Agent", "tool_input": args}})
            send({"type": "system", "subtype": "task_started", "task_id": task, "tool_use_id": launch, "task_type": "local_agent", "description": task})
        stopped = receive()
        assert stopped["request"]["subtype"] == "stop_task", stopped
        assert stopped["request"]["task_id"] == "one", stopped
        send({"type": "control_response", "response": {"subtype": "success", "request_id": stopped["request_id"], "response": {}}})
        send({"type": "system", "subtype": "task_updated", "task_id": "one", "patch": {"status": "killed"}})
        tool_result("launch-one", "Child stopped", True)
        send({"type": "assistant", "uuid": "sibling", "parent_tool_use_id": "launch-two", "message": {"content": [{"type": "text", "text": "CHILD_SIBLING_REPORT"}]}})
        send({"type": "system", "subtype": "task_notification", "task_id": "two", "status": "completed", "summary": "Sibling completed"})
        tool_result("launch-two", "Sibling completed")
    if SCENARIO in ("subagent-background", "subagent-foreground") and TOOLS_ENABLED:
        background = SCENARIO == "subagent-background"
        args = {"description": "Fixture child", "prompt": "Produce a report", "run_in_background": background}
        assistant([{"type": "tool_use", "id": "agent-launch", "name": "Agent", "input": args}], "agent-launch")
        launch_gate = control("agent-gate", {"subtype": "hook_callback", "callback_id": "smelt-permission", "tool_use_id": "agent-launch", "input": {"tool_name": "Agent", "tool_input": args}})
        assert launch_gate["hookSpecificOutput"]["permissionDecision"] == "allow", launch_gate
        if not background:
            send({"type": "assistant", "uuid": "child-before-start", "parent_tool_use_id": "agent-launch", "message": {"content": [{"type": "text", "text": "CHILD_BEFORE_START"}]}})
        send({"type": "system", "subtype": "task_started", "task_id": "child-task", "task_type": "local_agent", "tool_use_id": "agent-launch", "description": "Fixture child", "is_backgrounded": background})
        if not background:
            child_args = {"file_path": "probe.txt"}
            send({"type": "assistant", "uuid": "child-read", "parent_tool_use_id": "agent-launch", "message": {"content": [{"type": "tool_use", "id": "child-read", "name": "Read", "input": child_args}]}})
            answer = control("child-gate", {"subtype": "hook_callback", "callback_id": "smelt-permission", "tool_use_id": "child-read", "input": {"tool_name": "Read", "tool_input": child_args}})
            denied = answer["hookSpecificOutput"]["permissionDecision"] == "deny"
            send({"type": "user", "parent_tool_use_id": "agent-launch", "message": {"content": [{"type": "tool_result", "tool_use_id": "child-read", "content": "CHILD_READ_DENIED" if denied else "CHILD_READ_ALLOWED", "is_error": denied}]}})
            send({"type": "system", "subtype": "task_notification", "task_id": "child-task", "tool_use_id": "agent-launch", "status": "completed", "summary": "Fixture child complete"})
        tool_result("agent-launch", "async_launched: child-task" if background else "Fixture child complete")
    if SCENARIO.startswith("shell-") and TOOLS_ENABLED:
        background = SCENARIO != "shell-foreground"
        args = {"command": "make serve", "run_in_background": background}
        assistant([{"type": "tool_use", "id": "shell-call", "name": "Bash", "input": args}], "shell-call")
        gate = control("shell-gate", {"subtype": "hook_callback", "callback_id": "smelt-permission", "tool_use_id": "shell-call", "input": {"tool_name": "Bash", "tool_input": args}})
        assert gate["hookSpecificOutput"]["permissionDecision"] == "allow", gate
        if background:
            send({"type": "system", "subtype": "background_tasks_changed", "tasks": [{"task_id": "shell-task", "task_type": "local_bash", "description": "Serve"}]})
        send({"type": "system", "subtype": "task_started", "task_id": "shell-task", "task_type": "local_bash", "tool_use_id": "shell-call", "description": "Serve", "is_backgrounded": background})
        if background:
            tool_result("shell-call", "Command running in background with ID: shell-task.")
        else:
            task_output("shell-task", "building\nhalf")
            task_output("shell-task", "building\nhalfway\ndone\n")
            send({"type": "system", "subtype": "task_notification", "task_id": "shell-task", "tool_use_id": "shell-call", "status": "completed", "output_file": "", "summary": "Serve"})
            tool_result("shell-call", "building\nhalfway\ndone")
    if SCENARIO == "cancel-continue":
        send({"type": "system", "subtype": "background_tasks_changed", "tasks": [{"task_id": "kept-shell", "task_type": "local_bash", "description": "Serve"}]})
        send({"type": "system", "subtype": "task_started", "task_id": "kept-shell", "task_type": "local_bash", "description": "Serve", "is_backgrounded": True})
        assistant([{"type": "text", "text": "KEPT_STEP"}], "kept")
        assistant([{"type": "tool_use", "id": "cut-tool", "name": "ToolSearch", "input": {"query": "x"}}], "cut")
        interrupt = receive()
        while interrupt.get("request", {}).get("subtype") != "interrupt":
            interrupt = receive()
        # Claude Code finishes the step it was in before it stops.
        tool_result("cut-tool", "LATE_RESULT")
        assistant([{"type": "text", "text": "LATE_FRAME"}], "late")
        send({"type": "result", "subtype": "error_during_execution", "is_error": True, "terminal_reason": "aborted_tools", "session_id": SESSION, "usage": {"input_tokens": 1, "output_tokens": 1}})
        send({"type": "system", "subtype": "session_state_changed", "state": "idle", "sdk_host_only": True})
        SCENARIO = "text"
        continue
    if SCENARIO in ("steer", "steer-cancel"):
        assistant([{"type": "tool_use", "id": "steer-tool", "name": "ToolSearch", "input": {"query": "x"}}], "steer-tool")
        steer = receive()
        while steer.get("type") != "user":
            steer = receive()
        if SCENARIO == "steer":
            # Claude Code takes a steer with the running tool's result.
            send({"type": "user", "isReplay": True, "uuid": steer["uuid"], "message": steer["message"], "parent_tool_use_id": None})
            tool_result("steer-tool", "found")
        else:
            cancel = receive()
            assert cancel["request"] == {"subtype": "cancel_async_message", "message_uuid": steer["uuid"]}, cancel
            send({"type": "control_response", "response": {"subtype": "success", "request_id": cancel["request_id"], "response": {"cancelled": True}}})
            tool_result("steer-tool", "found")
    text = "fixture answer café 🌱"
    send({"type": "stream_event", "event": {"type": "message_start", "message": {"id": "message"}}})
    send({"type": "stream_event", "event": {"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": text}}})
    assistant([{"type": "text", "text": text}], "final")
    send({"type": "result", "subtype": "success", "is_error": False, "session_id": SESSION, "result": text, "total_cost_usd": 0.01, "usage": {"input_tokens": 5, "output_tokens": 8, "cache_read_input_tokens": 3, "cache_creation_input_tokens": 2}, "structured_output": {"title": "fixture title"}})
    if SCENARIO == "subagent-background" and TOOLS_ENABLED:
        # The child outlives the parent's answer; Claude Code goes idle meanwhile.
        send({"type": "system", "subtype": "session_state_changed", "state": "idle", "sdk_host_only": True})
        time.sleep(0.05)
        child_args = {"file_path": "probe.txt"}
        send({"type": "assistant", "uuid": "late-child-read", "parent_tool_use_id": "agent-launch", "agent_id": "child-task", "message": {"content": [{"type": "tool_use", "id": "late-child-read", "name": "Read", "input": child_args}]}})
        answer = control("late-child-gate", {"subtype": "hook_callback", "callback_id": "smelt-permission", "tool_use_id": "late-child-read", "input": {"tool_name": "Read", "tool_input": child_args, "agent_id": "child-task"}})
        denied = answer["hookSpecificOutput"]["permissionDecision"] == "deny"
        send({"type": "user", "parent_tool_use_id": "agent-launch", "message": {"content": [{"type": "tool_result", "tool_use_id": "late-child-read", "content": "CHILD_READ_DENIED" if denied else "CHILD_READ_ALLOWED", "is_error": denied}]}})
        send({"type": "assistant", "uuid": "child-report", "parent_tool_use_id": "agent-launch", "agent_id": "child-task", "message": {"content": [{"type": "text", "text": "CHILD_REPORT_ONLY"}]}})
        send({"type": "system", "subtype": "task_notification", "task_id": "child-task", "tool_use_id": "agent-launch", "status": "completed", "summary": "CHILD_REPORT_ONLY", "usage": {"total_tokens": 7300}})
        send({"type": "system", "subtype": "background_tasks_changed", "tasks": []})
        send({"type": "system", "subtype": "session_state_changed", "state": "running", "sdk_host_only": True})
        assistant([{"type": "text", "text": "BACKGROUND_WAKE_REPLY"}], "wake-reply")
        send({"type": "result", "subtype": "success", "is_error": False, "session_id": SESSION, "result": "BACKGROUND_WAKE_REPLY", "total_cost_usd": 0.02, "usage": {"input_tokens": 2, "output_tokens": 3}})
    if SCENARIO == "shell-background" and TOOLS_ENABLED:
        send({"type": "system", "subtype": "session_state_changed", "state": "idle", "sdk_host_only": True})
        task_output("shell-task", "tick 1\n")
        # Claude Code wakes to report a background shell that ended.
        send({"type": "system", "subtype": "task_updated", "task_id": "shell-task", "patch": {"status": "completed"}})
        send({"type": "system", "subtype": "task_notification", "task_id": "shell-task", "tool_use_id": "shell-call", "status": "completed", "output_file": "", "summary": "Serve"})
        send({"type": "system", "subtype": "background_tasks_changed", "tasks": []})
        send({"type": "system", "subtype": "session_state_changed", "state": "running", "sdk_host_only": True})
        assistant([{"type": "text", "text": "SHELL_WAKE_REPLY"}], "shell-wake")
        send({"type": "result", "subtype": "success", "is_error": False, "session_id": SESSION, "result": "SHELL_WAKE_REPLY", "total_cost_usd": 0.02, "usage": {"input_tokens": 2, "output_tokens": 3}})
    if SCENARIO in ("monitor-wake", "monitor-wake-held"):
        # A monitor event wakes Claude Code, which thinks until it is
        # interrupted or, unless held, a prompt arrives. The prompt waits in its
        # queue until the wake reply ends; an interrupt can withdraw it.
        send({"type": "system", "subtype": "session_state_changed", "state": "idle", "sdk_host_only": True})
        send({"type": "system", "subtype": "session_state_changed", "state": "running", "sdk_host_only": True})
        log({"wake": "started"})
        deadline = time.monotonic() + 5
        while True:
            try:
                message = receive(max(0, deadline - time.monotonic()))
            except queue.Empty:
                break
            request = message.get("request", {})
            if request.get("subtype") == "interrupt" and request.get("cancel_queued"):
                for queued in [queued for queued in PENDING if queued.get("type") == "user"]:
                    log({"withdrawn": queued["uuid"]})
                    PENDING.remove(queued)
            PENDING.append(message)
            if message.get("type") == "user":
                log({"wake": "prompted"})
                if SCENARIO == "monitor-wake":
                    break
            if request.get("subtype") == "interrupt":
                break
        assistant([{"type": "text", "text": "MONITOR_WAKE_REPLY"}], "monitor-wake")
        send({"type": "result", "subtype": "success", "is_error": False, "session_id": SESSION, "result": "MONITOR_WAKE_REPLY", "total_cost_usd": 0.02, "usage": {"input_tokens": 2, "output_tokens": 3}})
    send({"type": "system", "subtype": "session_state_changed", "state": "idle", "sdk_host_only": True})
    # The process persists across turns; later prompts are plain questions.
    SCENARIO = "text"
