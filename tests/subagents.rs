//! End-user headless coverage for the optional Lua subagent plugin.
#![allow(dead_code)]
mod common;

use common::harness::Harness;
use serde_json::{json, Value};
use wiremock::matchers::method;
use wiremock::{Mock, Request, ResponseTemplate};

fn response(calls: Vec<(&str, &str, Value)>) -> ResponseTemplate {
    let tools = !calls.is_empty();
    let mut events = vec![
        json!({"type":"message_start","message":{"id":"test","type":"message","role":"assistant","content":[],"model":"test-model","usage":{"input_tokens":10,"output_tokens":1}}}),
    ];
    if tools {
        for (index, (id, name, args)) in calls.into_iter().enumerate() {
            events.push(json!({"type":"content_block_start","index":index,"content_block":{"type":"tool_use","id":id,"name":name,"input":{}}}));
            events.push(json!({"type":"content_block_delta","index":index,"delta":{"type":"input_json_delta","partial_json":args.to_string()}}));
            events.push(json!({"type":"content_block_stop","index":index}));
        }
    } else {
        events.push(json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}));
        events.push(json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"completed independently"}}));
        events.push(json!({"type":"content_block_stop","index":0}));
    }
    events.push(json!({"type":"message_delta","delta":{"stop_reason":if tools {"tool_use"} else {"end_turn"}},"usage":{"output_tokens":10}}));
    events.push(json!({"type":"message_stop"}));
    let body = events
        .into_iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect::<String>();
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_string(body)
}

fn tool_result<'a>(body: &'a Value, id: &str) -> Option<&'a Value> {
    body["messages"]
        .as_array()?
        .iter()
        .filter_map(|message| message["content"].as_array())
        .flatten()
        .find(|part| part["type"] == "tool_result" && part["tool_use_id"] == id)
}

fn is_child(body: &Value) -> bool {
    body["messages"].as_array().unwrap().iter().any(|message| {
        message["role"] == "user"
            && message["content"].as_array().is_some_and(|parts| {
                parts.iter().any(|part| {
                    part["type"] == "text"
                        && part["text"]
                            .as_str()
                            .is_some_and(|text| text.starts_with("You are a subagent."))
                })
            })
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn swarm_runs_queued_children_with_identical_context_and_denies_nested_spawns() {
    let harness = Harness::new().await;
    harness.write_config("anthropic-compatible", "test-model");
    harness.write_init_lua(
        r#"
require("smelt.plugins.subagents").setup({ max_concurrent = 4 })
smelt.tools.register({
  name = "probe",
  description = "Test a yielding child tool.",
  permission_defaults = { normal = "allow", plan = "allow", apply = "allow" },
  effect = "read",
  parameters = { type = "object", properties = {} },
  execute = function(_, ctx)
    local session_id = ctx.session_id
    ctx.session_id = "forged parent"
    smelt.sleep(5)
    local ok, err = pcall(smelt.agent.fork, "nested after yielding", 1)
    assert(not ok and tostring(err):find("cannot create subagents", 1, true))
    return "probe executed for " .. session_id
  end,
})
"#,
    );
    Mock::given(method("POST"))
        .respond_with(|request: &Request| {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            if is_child(&body) {
                if tool_result(&body, "probe-call").is_some() {
                    response(vec![])
                } else {
                    response(vec![
                        (
                            "nested-call",
                            "spawn_agent",
                            json!({"prompt":"forbidden nested task"}),
                        ),
                        ("probe-call", "probe", json!({})),
                        ("glob-call", "glob", json!({"pattern":"*.lua"})),
                    ])
                }
            } else if tool_result(&body, "wait-call").is_some() {
                response(vec![])
            } else if let Some(spawned) = tool_result(&body, "swarm-call") {
                let content = spawned["content"].as_str().expect("spawn result string");
                let runs: Value = serde_json::from_str(content).expect("spawn result JSON");
                let ids: Vec<_> = runs
                    .as_array()
                    .expect("spawn results array")
                    .iter()
                    .map(|run| run["id"].clone())
                    .collect();
                response(vec![("wait-call", "wait_agents", json!({"ids":ids}))])
            } else {
                response(vec![(
                    "swarm-call",
                    "swarm",
                    json!({"prompt":"Perform the independent probe", "n":5}),
                )])
            }
        })
        .mount(&harness.mock)
        .await;

    let output =
        harness.run_with_tool_calling("Delegate the probe to a swarm", "test/test-model", true);
    assert_eq!(output.status, 0, "{}\n{:?}", output.stderr, output.events);
    let requests = harness.captured_request_bodies().await;
    let initial_children: Vec<_> = requests
        .iter()
        .filter(|body| is_child(body) && tool_result(body, "probe-call").is_none())
        .collect();
    assert_eq!(
        initial_children.len(),
        5,
        "{}\n{:?}",
        output.stderr,
        output.events
    );
    for child in &initial_children {
        assert_eq!(child["tools"], requests[0]["tools"]);
        assert_eq!(child["system"], requests[0]["system"]);
        assert_eq!(child["model"], requests[0]["model"]);
        let parent_messages = requests[0]["messages"].as_array().unwrap();
        assert_eq!(
            &child["messages"].as_array().unwrap()[..parent_messages.len()],
            parent_messages,
            "child must preserve the serialized parent prefix, including cache markers"
        );
        assert_eq!(*child, initial_children[0]);
    }
    let finished_children: Vec<_> = requests
        .iter()
        .filter(|body| is_child(body) && tool_result(body, "probe-call").is_some())
        .collect();
    assert_eq!(finished_children.len(), 5);
    for child in finished_children {
        let denied = tool_result(child, "nested-call").expect("nested call result");
        assert!(denied["content"]
            .to_string()
            .contains("cannot create subagents"));
        assert!(tool_result(child, "probe-call").unwrap()["content"]
            .to_string()
            .contains("probe executed"));
    }
    let denied_events = output
        .events
        .iter()
        .filter_map(|event| event.get("Subagent"))
        .filter_map(|child| child["event"].get("ToolFinished"))
        .filter(|tool| tool["call_id"] == "nested-call" && tool["result"]["is_error"] == true)
        .count();
    assert_eq!(denied_events, 5);
    let glob_results: Vec<_> = output
        .events
        .iter()
        .filter_map(|event| event.get("Subagent"))
        .filter_map(|child| child["event"].get("ToolFinished"))
        .filter(|tool| tool["call_id"] == "glob-call")
        .collect();
    assert_eq!(glob_results.len(), 5);
    for tool in glob_results {
        assert_eq!(tool["result"]["is_error"], false, "{tool}");
    }
    let final_parent = requests
        .iter()
        .find(|body| !is_child(body) && tool_result(body, "wait-call").is_some())
        .expect("parent collected child results");
    let result = tool_result(final_parent, "wait-call").unwrap()["content"]
        .as_str()
        .unwrap();
    let runs: Vec<Value> = serde_json::from_str(result).unwrap();
    assert_eq!(runs.len(), 5);
    assert!(runs
        .iter()
        .all(|run| run["status"] == "completed" && run["result"] == "completed independently"));
    for run in &runs {
        assert_eq!(
            run.as_object().unwrap().len(),
            3,
            "only id, status and final report: {run}"
        );
        let usage: Vec<_> = output
            .events
            .iter()
            .filter_map(|event| event.get("Subagent"))
            .filter(|child| child["id"] == run["id"])
            .filter_map(|child| child["event"].get("TokenUsage"))
            .map(|event| &event["usage"])
            .collect();
        for field in ["prompt_tokens", "completion_tokens"] {
            assert_eq!(
                usage
                    .iter()
                    .map(|usage| usage[field].as_u64().unwrap_or(0))
                    .sum::<u64>(),
                20
            );
        }
    }
    let spawn = tool_result(final_parent, "swarm-call").unwrap()["content"]
        .as_str()
        .unwrap();
    let runs: Vec<Value> = serde_json::from_str(spawn).unwrap();
    assert_eq!(
        runs.iter().filter(|run| run["status"] == "running").count(),
        4
    );
    assert_eq!(
        runs.iter().filter(|run| run["status"] == "queued").count(),
        1
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stopping_one_child_keeps_siblings_and_parent_running() {
    let harness = Harness::new().await;
    harness.write_config("anthropic-compatible", "test-model");
    harness.write_init_lua("require('smelt.plugins.subagents')");
    Mock::given(method("POST"))
        .respond_with(|request: &Request| {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            if is_child(&body) {
                return response(vec![]).set_delay(std::time::Duration::from_secs(1));
            }
            if tool_result(&body, "wait-call").is_some() {
                return response(vec![]);
            }
            if let Some(spawned) = tool_result(&body, "swarm-call") {
                let runs: Vec<Value> =
                    serde_json::from_str(spawned["content"].as_str().unwrap()).unwrap();
                if tool_result(&body, "stop-call").is_none() {
                    return response(vec![(
                        "stop-call",
                        "stop_agent",
                        json!({"id":runs[0]["id"]}),
                    )]);
                }
                let ids: Vec<_> = runs.iter().map(|run| run["id"].clone()).collect();
                return response(vec![("wait-call", "wait_agents", json!({"ids":ids}))]);
            }
            response(vec![(
                "swarm-call",
                "swarm",
                json!({"prompt":"Independent task", "n":2}),
            )])
        })
        .mount(&harness.mock)
        .await;
    let output = harness.run_with_tool_calling(
        "Start two agents and cancel only the first",
        "test/test-model",
        true,
    );
    assert_eq!(output.status, 0, "{}\n{:?}", output.stderr, output.events);
    let requests = harness.captured_request_bodies().await;
    let collected = requests
        .iter()
        .find(|body| !is_child(body) && tool_result(body, "wait-call").is_some())
        .expect("parent collected results");
    let result = tool_result(collected, "wait-call").unwrap()["content"]
        .as_str()
        .unwrap();
    let runs: Vec<Value> = serde_json::from_str(result).unwrap();
    assert_eq!(runs.len(), 2);
    assert_eq!(
        runs[0],
        json!({"id":runs[0]["id"], "status":"cancelled", "error":"subagent was cancelled"})
    );
    assert_eq!(
        runs[1],
        json!({"id":runs[1]["id"], "status":"completed", "result":"completed independently"})
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ten_subagents_run_in_parallel_by_default() {
    let harness = Harness::new().await;
    harness.write_config("anthropic-compatible", "test-model");
    harness.write_init_lua("require('smelt.plugins.subagents')");
    Mock::given(method("POST"))
        .respond_with(|request: &Request| {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            if is_child(&body) || tool_result(&body, "wait-call").is_some() {
                return response(vec![]);
            }
            if let Some(spawned) = tool_result(&body, "swarm-call") {
                let runs: Vec<Value> =
                    serde_json::from_str(spawned["content"].as_str().unwrap()).unwrap();
                let ids: Vec<_> = runs.iter().map(|run| run["id"].clone()).collect();
                return response(vec![("wait-call", "wait_agents", json!({"ids":ids}))]);
            }
            response(vec![(
                "swarm-call",
                "swarm",
                json!({"prompt":"Independent task", "n":10}),
            )])
        })
        .mount(&harness.mock)
        .await;
    let output =
        harness.run_with_tool_calling("Run ten independent agents", "test/test-model", true);
    assert_eq!(output.status, 0, "{}\n{:?}", output.stderr, output.events);
    let requests = harness.captured_request_bodies().await;
    let spawn = requests
        .iter()
        .find_map(|body| tool_result(body, "swarm-call"))
        .unwrap();
    let runs: Vec<Value> = serde_json::from_str(spawn["content"].as_str().unwrap()).unwrap();
    assert_eq!(runs.len(), 10);
    assert!(
        runs.iter().all(|run| run["status"] == "running"),
        "{runs:?}"
    );
    let result = requests
        .iter()
        .find_map(|body| tool_result(body, "wait-call"))
        .unwrap();
    let runs: Vec<Value> = serde_json::from_str(result["content"].as_str().unwrap()).unwrap();
    assert!(
        runs.iter().all(|run| run["status"] == "completed"),
        "{runs:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_wait_returns_only_final_results_after_running_and_queued_children_finish() {
    let harness = Harness::new().await;
    harness.write_config("anthropic-compatible", "test-model");
    harness.write_init_lua("require('smelt.plugins.subagents').setup({ max_concurrent = 1 })");
    Mock::given(method("POST"))
        .respond_with(|request: &Request| {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            if is_child(&body) {
                return response(vec![]).set_delay(std::time::Duration::from_millis(300));
            }
            if tool_result(&body, "wait-call").is_some() {
                return response(vec![]);
            }
            if let Some(spawned) = tool_result(&body, "swarm-call") {
                let runs: Vec<Value> =
                    serde_json::from_str(spawned["content"].as_str().unwrap()).unwrap();
                let ids: Vec<_> = runs.iter().map(|run| run["id"].clone()).collect();
                return response(vec![(
                    "wait-call",
                    "wait_agents",
                    json!({"ids":ids, "timeout_ms":0}),
                )]);
            }
            response(vec![(
                "swarm-call",
                "swarm",
                json!({"prompt":"Independent task", "n":2}),
            )])
        })
        .mount(&harness.mock)
        .await;
    let output = harness.run_with_tool_calling(
        "Delegate, then wait once for the final reports",
        "test/test-model",
        true,
    );
    assert_eq!(output.status, 0, "{}\n{:?}", output.stderr, output.events);
    let requests = harness.captured_request_bodies().await;
    let collected = requests
        .iter()
        .find_map(|body| tool_result(body, "wait-call"))
        .unwrap();
    let runs: Value = serde_json::from_str(collected["content"].as_str().unwrap()).unwrap();
    let runs = runs
        .as_array()
        .expect("a wait returns terminal results, never a running snapshot");
    assert_eq!(runs.len(), 2);
    for run in runs {
        assert_eq!(
            *run,
            json!({"id":run["id"], "status":"completed", "result":"completed independently"})
        );
    }
    assert_eq!(
        requests.iter().filter(|body| !is_child(body)).count(),
        3,
        "spawn, wait, final answer only"
    );
    let wait_finished = output
        .events
        .iter()
        .position(|event| {
            event
                .get("ToolFinished")
                .is_some_and(|tool| tool["call_id"] == "wait-call")
        })
        .unwrap();
    assert_eq!(
        output.events[..wait_finished]
            .iter()
            .filter(|event| {
                event
                    .get("Subagent")
                    .is_some_and(|child| child["event"].get("TurnComplete").is_some())
            })
            .count(),
        2,
        "the wait cannot return before every child terminates"
    );
    let preview = runs
        .iter()
        .map(|run| format!("agent #{} - completed\ncompleted independently", run["id"]))
        .collect::<Vec<_>>()
        .join("\n\n");
    assert_eq!(
        output.events[wait_finished]["ToolFinished"]["result"]["display_content"],
        json!([{"name":"results", "content":preview}])
    );
    let tool = requests[0]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "wait_agents")
        .unwrap();
    assert!(
        tool["input_schema"]["properties"]
            .get("timeout_ms")
            .is_none(),
        "{tool}"
    );
}
