//! Integration scenarios driving the `smelt` binary against a wiremock'd
//! provider. Each `#[tokio::test]` is one flow: prompt + canned LLM
//! response → assertions on the JSONL event stream.

mod common;

use common::harness::Harness;

/// Smoke: harness compiles, wiremock spins up, tempdir resolves.
/// Doesn't drive the binary.
#[tokio::test]
async fn smoke_harness_starts() {
    let h = Harness::new().await;
    assert!(h.mock.uri().starts_with("http://"));
    h.write_config("anthropic", "claude-test");
    h.write_init_lua("");
    let cfg = h.config_dir.path().join("smelt").join("config.yaml");
    assert!(cfg.exists());
}

/// Plain turn: user types a prompt; provider returns a single text
/// content block; engine emits the streaming + completion events.
#[tokio::test]
async fn plain_turn() {
    let h = Harness::new().await;
    h.write_config("anthropic", "claude-test");
    h.write_init_lua("");
    h.mount_anthropic_sse(&[
        serde_json::json!({
            "type": "message_start",
            "message": {
                "id": "msg_test",
                "type": "message",
                "role": "assistant",
                "model": "claude-test",
                "content": [],
                "stop_reason": null,
                "stop_sequence": null,
                "usage": { "input_tokens": 10, "output_tokens": 0 }
            }
        }),
        serde_json::json!({
            "type": "content_block_start",
            "index": 0,
            "content_block": { "type": "text", "text": "" }
        }),
        serde_json::json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": { "type": "text_delta", "text": "hello" }
        }),
        serde_json::json!({
            "type": "content_block_stop",
            "index": 0
        }),
        serde_json::json!({
            "type": "message_delta",
            "delta": { "stop_reason": "end_turn", "stop_sequence": null },
            "usage": { "output_tokens": 1 }
        }),
        serde_json::json!({ "type": "message_stop" }),
    ])
    .await;

    let out = h.run("hi", "test/claude-test");
    insta::assert_json_snapshot!(out.events, {
        "[].TurnComplete.meta.elapsed_ms" => "[elapsed_ms]",
        "[].TurnComplete.meta.avg_tps" => "[avg_tps]",
        "[].TokenUsage.tokens_per_sec" => "[tps]",
    });
}

/// Thinking + text: provider streams a `thinking_delta` then a
/// `text_delta`. Engine emits ThinkingDelta, then TextDelta, then
/// Messages with the assistant content (thinking is dropped from the
/// persisted message tail when reasoning effort is off).
#[tokio::test]
async fn thinking_then_text() {
    let h = Harness::new().await;
    h.write_config("anthropic", "claude-test");
    h.write_init_lua("");
    h.mount_anthropic_sse(&[
        serde_json::json!({
            "type": "message_start",
            "message": {
                "id": "msg_test",
                "type": "message",
                "role": "assistant",
                "model": "claude-test",
                "content": [],
                "stop_reason": null,
                "stop_sequence": null,
                "usage": { "input_tokens": 5, "output_tokens": 0 }
            }
        }),
        serde_json::json!({
            "type": "content_block_start",
            "index": 0,
            "content_block": { "type": "thinking", "thinking": "" }
        }),
        serde_json::json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": { "type": "thinking_delta", "thinking": "let me think" }
        }),
        serde_json::json!({
            "type": "content_block_stop",
            "index": 0
        }),
        serde_json::json!({
            "type": "content_block_start",
            "index": 1,
            "content_block": { "type": "text", "text": "" }
        }),
        serde_json::json!({
            "type": "content_block_delta",
            "index": 1,
            "delta": { "type": "text_delta", "text": "answer" }
        }),
        serde_json::json!({
            "type": "content_block_stop",
            "index": 1
        }),
        serde_json::json!({
            "type": "message_delta",
            "delta": { "stop_reason": "end_turn", "stop_sequence": null },
            "usage": { "output_tokens": 3 }
        }),
        serde_json::json!({ "type": "message_stop" }),
    ])
    .await;

    let out = h.run("solve it", "test/claude-test");
    insta::assert_json_snapshot!(out.events, {
        "[].TurnComplete.meta.elapsed_ms" => "[elapsed_ms]",
        "[].TurnComplete.meta.avg_tps" => "[avg_tps]",
        "[].TokenUsage.tokens_per_sec" => "[tps]",
    });
}
