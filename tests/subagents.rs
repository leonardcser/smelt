//! End-user headless coverage for the optional Lua subagent plugin.
#![allow(dead_code)]
mod common;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subagent_restore_reports_catalog_errors_instead_of_treating_archives_as_empty() {
    assert_catalog_failure_prevents_spawn("spawn_agent").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn direct_subagent_forks_cannot_bypass_catalog_restoration() {
    assert_catalog_failure_prevents_spawn("direct_fork").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn direct_subagent_forks_await_restoration_without_plugin_polling() {
    let harness = Harness::new().await;
    harness.write_config("anthropic-compatible", "test-model");
    harness.write_init_lua(r#"
        smelt.agent.enable_forks()
        smelt.tools.register({
            name = 'direct_fork', description = 'Direct native fork',
            permission_defaults = { normal = 'allow', plan = 'allow', apply = 'allow' },
            effect = 'process', parameters = { type = 'object', properties = {} },
            execute = function()
                return smelt.json.encode(smelt.agent.fork('You are a subagent. Review independently', 1, 'Review'))
            end,
        })
    "#);
    Mock::given(method("POST"))
        .respond_with(|request: &Request| {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            if is_child(&body) || tool_result(&body, "spawn-call").is_some() {
                return response(vec![]);
            }
            response(vec![("spawn-call", "direct_fork", json!({}))])
        })
        .mount(&harness.mock)
        .await;
    let output = harness.run_with_tool_calling("Delegate a review", "test/test-model", true);
    assert_eq!(output.status, 0, "{}\n{:?}", output.stderr, output.events);
    let requests = harness.captured_request_bodies().await;
    let result = requests
        .iter()
        .find_map(|body| tool_result(body, "spawn-call"))
        .unwrap();
    assert_ne!(
        result["is_error"], true,
        "native fork must await readiness: {result}"
    );
    let runs: Vec<Value> = serde_json::from_str(result["content"].as_str().unwrap()).unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(requests.iter().filter(|body| is_child(body)).count(), 1);
}

async fn assert_catalog_failure_prevents_spawn(tool_name: &'static str) {
    let harness = Harness::new().await;
    harness.write_config("anthropic-compatible", "test-model");
    harness.write_init_lua(r#"
        require('smelt.plugins.subagents')
        smelt.tools.register({
            name = 'direct_fork', description = 'Direct native fork',
            permission_defaults = { normal = 'allow', plan = 'allow', apply = 'allow' },
            effect = 'process', parameters = { type = 'object', properties = {} },
            execute = function()
                return smelt.json.encode(smelt.agent.fork('You are a subagent. Review independently', 1, 'Review'))
            end,
        })
    "#);
    let state = harness.config_dir.path().join("state/smelt");
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(state.join("sessions"), "occupied").unwrap();
    Mock::given(method("POST"))
        .respond_with(move |request: &Request| {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            if is_child(&body) || tool_result(&body, "spawn-call").is_some() {
                return response(vec![]);
            }
            response(vec![(
                "spawn-call",
                tool_name,
                json!({"title":"Review", "prompt":"Review independently"}),
            )])
        })
        .mount(&harness.mock)
        .await;
    let output = harness.run_with_tool_calling("Delegate a review", "test/test-model", true);
    assert_eq!(output.status, 0, "{}\n{:?}", output.stderr, output.events);
    let requests = harness.captured_request_bodies().await;
    let result = requests
        .iter()
        .find_map(|body| tool_result(body, "spawn-call"))
        .unwrap();
    assert_eq!(
        result["is_error"], true,
        "catalog failures must remain explicit: {result}"
    );
    assert!(
        result["content"]
            .as_str()
            .unwrap()
            .contains("session catalog"),
        "catalog failure must not be masked by a tool timeout: {result}"
    );
    assert!(
        !requests.iter().any(is_child),
        "no worker may launch with unknown archived identities"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delegation_tools_default_to_parallel_work_without_redundant_swarms() {
    let harness = common::harness::Harness::new().await;
    harness.write_config("anthropic-compatible", "test-model");
    harness.write_init_lua("require('smelt.plugins.subagents')");
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .respond_with(response(vec![]))
        .mount(&harness.mock)
        .await;
    let output = harness.run_with_tool_calling("Find the parser", "test/test-model", true);
    assert_eq!(output.status, 0, "{}", output.stderr);
    let requests = harness.captured_request_bodies().await;
    let tools = requests[0]["tools"].as_array().unwrap();
    assert!(!tools.iter().any(|tool| tool["name"] == "swarm"));
    let spawn = tools
        .iter()
        .find(|tool| tool["name"] == "spawn_agent")
        .unwrap();
    let description = spawn["description"].as_str().unwrap();
    assert!(description.contains("parallel"), "{description}");
    assert!(description.contains("explicitly"), "{description}");
    assert!(spawn["input_schema"]["properties"].get("title").is_some());
    for name in ["follow_up_agent", "report_agent", "stop_agents"] {
        assert!(tools.iter().any(|tool| tool["name"] == name), "{name}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn named_subagent_reports_and_follow_ups_keep_their_own_durable_context() {
    let harness = Harness::new().await;
    harness.write_config("anthropic-compatible", "test-model");
    harness.write_init_lua("require('smelt.plugins.subagents')");
    Mock::given(method("POST")).respond_with(|request: &Request| {
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        if is_child(&body) {
            let repeated = body.to_string().contains("Recheck the marker");
            let follow_up = repeated || body.to_string().contains("Verify the marker");
            let id = if repeated { "recheck-report" } else if follow_up { "follow-report" } else { "initial-report" };
            if tool_result(&body, id).is_some() { return response(vec![]); }
            return response(vec![(id, "report_agent", json!({
                "status": if follow_up { "completed" } else { "blocked" },
                "report": if follow_up { "Verified the marker with the worker's context." } else { "Initial finding: the marker requires further verification." },
            }))]);
        }
        let Some(spawned) = tool_result(&body, "spawn-call") else {
            return response(vec![("spawn-call", "spawn_agent", json!({"title":"Review marker", "prompt":"Find the marker independently"}))]);
        };
        let runs: Vec<Value> = serde_json::from_str(spawned["content"].as_str().unwrap()).unwrap();
        let name = &runs[0]["name"];
        if tool_result(&body, "initial-wait").is_none() { return response(vec![("initial-wait", "wait_agents", json!({"ids":[name]}))]); }
        if tool_result(&body, "follow-call").is_none() {
            return response_with_text(vec![("follow-call", "follow_up_agent", json!({"id":name, "prompt":"Verify the marker"}))], Some("parent-only-update"));
        }
        if tool_result(&body, "follow-wait").is_none() { return response(vec![("follow-wait", "wait_agents", json!({"ids":[name]}))]); }
        if tool_result(&body, "recheck-call").is_none() {
            return response_with_text(vec![("recheck-call", "follow_up_agent", json!({"id":name, "prompt":"Recheck the marker"}))], Some("second-parent-only-update"));
        }
        if tool_result(&body, "recheck-wait").is_none() { return response(vec![("recheck-wait", "wait_agents", json!({"ids":[name]}))]); }
        response(vec![])
    }).mount(&harness.mock).await;
    let output = harness.run_with_tool_calling("Delegate a marker review", "test/test-model", true);
    assert_eq!(output.status, 0, "{}\n{:?}", output.stderr, output.events);
    let requests = harness.captured_request_bodies().await;
    let follow = requests
        .iter()
        .find(|body| is_child(body) && body.to_string().contains("Verify the marker"))
        .unwrap();
    assert!(
        tool_result(follow, "initial-report").is_some(),
        "worker history survives the follow-up"
    );
    assert!(
        !follow.to_string().contains("parent-only-update"),
        "subsequent parent context must not leak into the worker"
    );
    let repeated = requests
        .iter()
        .find(|body| is_child(body) && body.to_string().contains("Recheck the marker"))
        .unwrap();
    assert!(tool_result(repeated, "initial-report").is_some());
    assert!(tool_result(repeated, "follow-report").is_some());
    assert!(!repeated.to_string().contains("parent-only-update"));
    let final_parent = requests
        .iter()
        .find(|body| !is_child(body) && tool_result(body, "recheck-wait").is_some())
        .unwrap();
    let reports: Vec<Value> = serde_json::from_str(
        tool_result(final_parent, "initial-wait").unwrap()["content"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(reports[0]["status"], "blocked");
    assert!(reports[0]["result"]
        .as_str()
        .unwrap()
        .starts_with("Initial finding:"));
    let continued: Value = serde_json::from_str(
        tool_result(final_parent, "follow-call").unwrap()["content"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(continued["name"], reports[0]["name"]);
    assert_eq!(continued["id"], reports[0]["id"]);
    assert!(
        continued.get("session_id").is_none(),
        "model handles stay compact"
    );
    let identities = ["spawn-call", "follow-call", "recheck-call"].map(|call_id| {
        let result = output
            .events
            .iter()
            .filter_map(|event| event.get("ToolFinished"))
            .find(|tool| tool["call_id"] == call_id)
            .unwrap();
        let identity = result["result"]["metadata"]["agents"][0]["session_id"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(!identity.is_empty());
        identity
    });
    assert!(
        identities.iter().all(|identity| identity == &identities[0]),
        "renderer identity survives repeated follow-ups"
    );
    let storage =
        smelt_core::session::SessionStorage::new(harness.config_dir.path().join("state/smelt"));
    let saved = storage.list_sessions();
    assert_eq!(saved.len(), 1, "one durable worker session is reused");
    let session = storage.load_full(&saved[0].id).unwrap();
    assert!(format!("{:?}", session.history).contains("Verify the marker"));
    let metadata: Value = serde_json::from_slice(
        &std::fs::read(storage.artifact_dir_for(&session).join("agent.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(metadata["info"]["status"], "completed");
    assert_eq!(metadata["info"]["persistence_error"], Value::Null);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn headless_subagents_finish_and_persist_even_when_the_parent_does_not_wait() {
    let harness = Harness::new().await;
    harness.write_config("anthropic-compatible", "test-model");
    harness.write_init_lua("require('smelt.plugins.subagents')");
    Mock::given(method("POST"))
        .respond_with(|request: &Request| {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            if is_child(&body) {
                return response_with_text(vec![], Some("Unawaited worker finished"))
                    .set_delay(std::time::Duration::from_millis(100));
            }
            if tool_result(&body, "spawn-call").is_some() {
                return response(vec![]);
            }
            response(vec![(
                "spawn-call",
                "spawn_agent",
                json!({"title":"Independent review", "prompt":"Review the marker"}),
            )])
        })
        .mount(&harness.mock)
        .await;
    let output =
        harness.run_with_tool_calling("Assign an independent review", "test/test-model", true);
    assert_eq!(output.status, 0, "{}", output.stderr);
    assert!(output
        .events
        .iter()
        .filter_map(|event| event.get("Subagent"))
        .any(|event| event["event"].get("TurnComplete").is_some()));
    let storage =
        smelt_core::session::SessionStorage::new(harness.config_dir.path().join("state/smelt"));
    let saved = storage.list_sessions();
    assert_eq!(saved.len(), 1);
    let session = storage.load_full(&saved[0].id).unwrap();
    let metadata: Value = serde_json::from_slice(
        &std::fs::read(storage.artifact_dir_for(&session).join("agent.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(metadata["info"]["result"], "Unawaited worker finished");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn headless_flushes_more_than_sixty_four_total_workers_without_wait_tools() {
    let harness = Harness::new().await;
    harness.write_config("anthropic-compatible", "test-model");
    harness.write_init_lua(r#"
        smelt.agent.enable_forks()
        smelt.tools.register({
            name = 'spawn_many', description = 'Allocate independent reviews',
            permission_defaults = { normal = 'allow', plan = 'allow', apply = 'allow' },
            effect = 'process', watchdog_timeout_ms = 0,
            parameters = { type = 'object', properties = {} },
            execute = function(_, ctx)
                local function pending()
                    local count = 0
                    for _, run in ipairs(smelt.agent.runs(ctx.session_id)) do
                        if run.status == 'queued' or run.status == 'running' then count = count + 1 end
                    end
                    return count
                end
                for _ = 1, 9 do
                    -- Wait only for queue capacity, leaving final workers unawaited.
                    while pending() > 56 do smelt.sleep(10) end
                    smelt.agent.fork('You are a subagent. Review independently', 8, 'Review')
                end
                return 'Allocated 72 independent reviews.'
            end,
        })
    "#);
    Mock::given(method("POST"))
        .respond_with(|request: &Request| {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            if is_child(&body) || tool_result(&body, "spawn-call").is_some() {
                return response(vec![]);
            }
            response(vec![("spawn-call", "spawn_many", json!({}))])
        })
        .mount(&harness.mock)
        .await;
    let output = harness.run_with_tool_calling(
        "Delegate the requested independent reviews",
        "test/test-model",
        true,
    );
    assert_eq!(output.status, 0, "{}\n{:?}", output.stderr, output.events);
    assert_eq!(
        output
            .events
            .iter()
            .filter_map(|event| event.get("Subagent"))
            .filter(|event| event["event"].get("TurnComplete").is_some())
            .count(),
        72,
        "every worker must finish before shutdown: {:?}",
        output.events
    );
    let storage =
        smelt_core::session::SessionStorage::new(harness.config_dir.path().join("state/smelt"));
    let ready = storage.wait_for_session_catalog(std::time::Duration::from_secs(5));
    let page = storage
        .list_session_page_result(Default::default())
        .unwrap();
    assert!(ready, "catalog readiness: {:?}", page.catalog);
    assert_eq!(
        page.catalog.state,
        smelt_core::session::SessionCatalogState::Ready
    );
    let saved = storage.list_sessions();
    assert_eq!(saved.len(), 72);
    for saved in saved {
        let session = storage.load_full(&saved.id).unwrap();
        let metadata: Value = serde_json::from_slice(
            &std::fs::read(storage.artifact_dir_for(&session).join("agent.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(metadata["info"]["status"], "completed");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subagent_request_budget_blocks_an_unfinished_assignment_between_requests() {
    let harness = Harness::new().await;
    harness.write_config("anthropic-compatible", "test-model");
    harness.write_init_lua("require('smelt.plugins.subagents').setup({ max_requests = 1 })");
    Mock::given(method("POST"))
        .respond_with(|request: &Request| {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            if is_child(&body) {
                return response(vec![("probe", "glob", json!({"pattern":"*.lua"}))]);
            }
            if tool_result(&body, "wait-call").is_some() {
                return response(vec![]);
            }
            if let Some(spawned) = tool_result(&body, "spawn-call") {
                let runs: Vec<Value> =
                    serde_json::from_str(spawned["content"].as_str().unwrap()).unwrap();
                return response(vec![(
                    "wait-call",
                    "wait_agents",
                    json!({"ids":[runs[0]["name"]]}),
                )]);
            }
            response(vec![(
                "spawn-call",
                "spawn_agent",
                json!({"title":"Bounded review", "prompt":"Review independently"}),
            )])
        })
        .mount(&harness.mock)
        .await;
    let output =
        harness.run_with_tool_calling("Review with a request budget", "test/test-model", true);
    assert_eq!(output.status, 0, "{}", output.stderr);
    let requests = harness.captured_request_bodies().await;
    assert_eq!(requests.iter().filter(|body| is_child(body)).count(), 1);
    let parent = requests
        .iter()
        .find(|body| tool_result(body, "wait-call").is_some())
        .unwrap();
    let reports: Vec<Value> = serde_json::from_str(
        tool_result(parent, "wait-call").unwrap()["content"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(reports[0]["status"], "blocked");
    assert_eq!(reports[0]["error"], "subagent request budget reached");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subagent_compaction_projects_its_own_history_without_parent_hooks() {
    let harness = Harness::new().await;
    harness.write_config("anthropic-compatible", "test-model");
    harness.write_init_lua(
        "require('smelt.plugins.subagents').setup({ compact_at_tokens = 1, max_requests = 12 })",
    );
    Mock::given(method("POST")).respond_with(|request: &Request| {
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        let text = body.to_string();
        if text.contains("Produce a concise context checkpoint") { return ResponseTemplate::new(200).set_body_json(json!({"id":"checkpoint", "type":"message", "role":"assistant", "content":[{"type":"text", "text":"worker-context-checkpoint: preserve the delegated marker review"}], "model":"test-model", "stop_reason":"end_turn", "usage":{"input_tokens":10, "output_tokens":10}})); }
        if text.contains("worker-context-checkpoint") { return response_with_text(vec![], Some("Compacted worker completed")); }
        if is_child(&body) {
            let count = body["messages"].as_array().unwrap().len();
            return response(vec![(&format!("probe-{count}"), "glob", json!({"pattern":"*.lua"}))]);
        }
        if tool_result(&body, "wait-call").is_some() { return response(vec![]); }
        if let Some(spawned) = tool_result(&body, "spawn-call") {
            let runs: Vec<Value> = serde_json::from_str(spawned["content"].as_str().unwrap()).unwrap();
            return response(vec![("wait-call", "wait_agents", json!({"ids":[runs[0]["id"]]}))]);
        }
        response(vec![("spawn-call", "spawn_agent", json!({"title":"Long review", "prompt":"Review the marker independently"}))])
    }).mount(&harness.mock).await;
    let output =
        harness.run_with_tool_calling("Assign a long marker review", "test/test-model", true);
    assert_eq!(output.status, 0, "{}\n{:?}", output.stderr, output.events);
    let storage =
        smelt_core::session::SessionStorage::new(harness.config_dir.path().join("state/smelt"));
    let saved = storage.list_sessions();
    assert_eq!(saved.len(), 1);
    let session = storage.load_full(&saved[0].id).unwrap();
    assert!(session.checkpoint.is_some());
    assert!(
        session.history.len() > session.model_history().len(),
        "canonical history is preserved while the model view is compacted"
    );
    let requests = harness.captured_request_bodies().await;
    let resumed = requests
        .iter()
        .find(|body| body.to_string().contains("worker-context-checkpoint"))
        .unwrap();
    assert!(resumed["system"].is_array());
}

use common::harness::Harness;
use serde_json::{json, Value};
use wiremock::matchers::method;
use wiremock::{Mock, Request, ResponseTemplate};

fn response(calls: Vec<(&str, &str, Value)>) -> ResponseTemplate {
    response_with_text(calls, None)
}

fn response_with_text(calls: Vec<(&str, &str, Value)>, text: Option<&str>) -> ResponseTemplate {
    let tools = !calls.is_empty();
    let text = text.or_else(|| (!tools).then_some("completed independently"));
    let mut events = vec![
        json!({"type":"message_start","message":{"id":"test","type":"message","role":"assistant","content":[],"model":"test-model","usage":{"input_tokens":10,"output_tokens":1}}}),
    ];
    if let Some(text) = text {
        events.push(json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}));
        events.push(json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":text}}));
        events.push(json!({"type":"content_block_stop","index":0}));
    }
    for (index, (id, name, args)) in calls.into_iter().enumerate() {
        let index = index + usize::from(text.is_some());
        events.push(json!({"type":"content_block_start","index":index,"content_block":{"type":"tool_use","id":id,"name":name,"input":{}}}));
        events.push(json!({"type":"content_block_delta","index":index,"delta":{"type":"input_json_delta","partial_json":args.to_string()}}));
        events.push(json!({"type":"content_block_stop","index":index}));
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
require("smelt.plugins.subagents").setup({ swarm = true, max_concurrent = 4 })
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
                            json!({"title":"Independent task", "prompt":"forbidden nested task"}),
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
                    json!({"title":"Independent task", "prompt":"Perform the independent probe", "n":5}),
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
            5,
            "only handle, status and final report: {run}"
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
    harness.write_init_lua("require('smelt.plugins.subagents').setup({ swarm = true })");
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
                json!({"title":"Independent task", "prompt":"Independent task", "n":2}),
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
        json!({"id":runs[0]["id"], "name":runs[0]["name"], "title":"Independent task", "status":"cancelled", "error":"subagent was cancelled"})
    );
    assert_eq!(
        runs[1],
        json!({"id":runs[1]["id"], "name":runs[1]["name"], "title":"Independent task", "status":"completed", "result":"completed independently"})
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ten_subagents_run_in_parallel_by_default() {
    let harness = Harness::new().await;
    harness.write_config("anthropic-compatible", "test-model");
    harness.write_init_lua("require('smelt.plugins.subagents').setup({ swarm = true })");
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
                json!({"title":"Independent task", "prompt":"Independent task", "n":10}),
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
async fn peek_agent_reads_running_assistant_output_without_waiting_or_consuming_it() {
    let harness = Harness::new().await;
    harness.write_config("anthropic-compatible", "test-model");
    harness.write_init_lua(
        r#"
require('smelt.plugins.subagents')
for _, name in ipairs({ 'hold_child', 'await_child', 'release_child' }) do
    smelt.tools.register({
        name = name, description = 'Synchronize the child output fixture.', effect = 'read',
        permission_defaults = { normal = 'allow', plan = 'allow', apply = 'allow' },
        parameters = { type = 'object', properties = {} },
        execute = function()
            if name == 'hold_child' then
                _G.peek_ready = true
                while not _G.peek_release do smelt.sleep(5) end
            elseif name == 'await_child' then
                while not _G.peek_ready do smelt.sleep(5) end
            else
                _G.peek_release = true
            end
            return 'private fixture tool output'
        end,
    })
end
"#,
    );
    Mock::given(method("POST"))
        .respond_with(|request: &Request| {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            if is_child(&body) {
                return if tool_result(&body, "hold-call").is_some() {
                    response(vec![])
                } else {
                    response_with_text(
                        vec![("hold-call", "hold_child", json!({}))],
                        Some("Reviewing the parser.\nFound a boundary case."),
                    )
                };
            }
            if let Some(spawned) = tool_result(&body, "spawn-call") {
                let runs: Vec<Value> =
                    serde_json::from_str(spawned["content"].as_str().unwrap()).unwrap();
                let id = &runs[0]["id"];
                for (call, name, args) in [
                    ("ready-call", "await_child", json!({})),
                    ("peek-call", "peek_agent", json!({"id":id})),
                    ("release-call", "release_child", json!({})),
                    ("wait-call", "wait_agents", json!({"ids":[id]})),
                    ("peek-final-call", "peek_agent", json!({"id":id})),
                ] {
                    if tool_result(&body, call).is_none() {
                        return response(vec![(call, name, args)]);
                    }
                }
                return response(vec![]);
            }
            response(vec![(
                "spawn-call",
                "spawn_agent",
                json!({"title":"Independent task", "prompt":"Review parser independently"}),
            )])
        })
        .mount(&harness.mock)
        .await;
    let output = harness.run_with_tool_calling(
        "Inherited parent context must not appear in a peek",
        "test/test-model",
        true,
    );
    assert_eq!(output.status, 0, "{}\n{:?}", output.stderr, output.events);
    let requests = harness.captured_request_bodies().await;
    let peek = requests
        .iter()
        .find_map(|body| tool_result(body, "peek-call"))
        .unwrap();
    assert_ne!(peek["is_error"], true, "{peek}");
    let text = peek["content"].as_str().unwrap();
    assert!(text.contains("running"), "{text}");
    assert!(
        text.contains("Reviewing the parser.\nFound a boundary case."),
        "{text}"
    );
    assert!(!text.contains("completed independently"), "{text}");
    assert!(!text.contains("Inherited parent context"), "{text}");
    assert!(!text.contains("private fixture tool output"), "{text}");
    let finished = requests
        .iter()
        .find_map(|body| tool_result(body, "peek-final-call"))
        .unwrap();
    let text = finished["content"].as_str().unwrap();
    assert!(text.contains("completed"), "{text}");
    assert!(
        text.contains("Reviewing the parser.\nFound a boundary case."),
        "{text}"
    );
    assert!(text.contains("completed independently"), "{text}");
    assert!(!text.contains("private fixture tool output"), "{text}");
    let tool = requests[0]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "peek_agent")
        .unwrap();
    let description = tool["description"].as_str().unwrap();
    assert!(description.contains("Do not poll"), "{description}");
    assert!(description.contains("wait_agents"), "{description}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_wait_returns_only_final_results_after_running_and_queued_children_finish() {
    let harness = Harness::new().await;
    harness.write_config("anthropic-compatible", "test-model");
    harness.write_init_lua(
        "require('smelt.plugins.subagents').setup({ swarm = true, max_concurrent = 1 })",
    );
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
                json!({"title":"Independent task", "prompt":"Independent task", "n":2}),
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
            json!({"id":run["id"], "name":run["name"], "title":"Independent task", "status":"completed", "result":"completed independently"})
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
        .map(|run| {
            format!(
                "{} - completed\ncompleted independently",
                run["name"].as_str().unwrap()
            )
        })
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
