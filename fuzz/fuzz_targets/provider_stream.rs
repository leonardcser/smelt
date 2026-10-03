#![no_main]

use arbitrary::{Arbitrary, Unstructured};
use libfuzzer_sys::fuzz_target;
use smelt_provider::{
    fuzz_parse_provider_response, fuzz_parse_provider_stream, fuzz_parse_sse_event,
    CancellationToken, FuzzProviderSummary, FuzzSseDecoder, FuzzSseSummary, ProviderError,
};

#[derive(Debug)]
struct ExpectedSummary {
    ok: bool,
    content_len: usize,
    reasoning_len: usize,
    text_deltas: usize,
    thinking_deltas: usize,
    tool_arg_deltas: usize,
}

#[derive(Clone, Debug)]
enum Event {
    Raw(String),
    ChatText(String),
    ChatThinking(String),
    ChatTool {
        idx: u8,
        id: String,
        name: String,
        args: String,
    },
    ChatDone,
    OpenAiText(String),
    OpenAiThinking(String),
    OpenAiToolStart {
        item: String,
        call: String,
        name: String,
    },
    OpenAiToolArgs {
        item: String,
        args: String,
    },
    OpenAiDone,
    AnthropicText(String),
    AnthropicThinking {
        idx: u8,
        text: String,
    },
    AnthropicToolStart {
        idx: u8,
        id: String,
        name: String,
    },
    AnthropicToolArgs {
        idx: u8,
        args: String,
    },
    AnthropicDone,
}

impl<'a> Arbitrary<'a> for Event {
    fn arbitrary(u: &mut Unstructured<'a>) -> arbitrary::Result<Self> {
        Ok(match u.int_in_range(0u8..=14)? {
            0 => Event::Raw(short_string(u, 256)?),
            1 => Event::ChatText(short_string(u, 64)?),
            2 => Event::ChatThinking(short_string(u, 64)?),
            3 => Event::ChatTool {
                idx: u.arbitrary()?,
                id: short_string(u, 24)?,
                name: short_string(u, 24)?,
                args: short_string(u, 64)?,
            },
            4 => Event::ChatDone,
            5 => Event::OpenAiText(short_string(u, 64)?),
            6 => Event::OpenAiThinking(short_string(u, 64)?),
            7 => Event::OpenAiToolStart {
                item: short_string(u, 24)?,
                call: short_string(u, 24)?,
                name: short_string(u, 24)?,
            },
            8 => Event::OpenAiToolArgs {
                item: short_string(u, 24)?,
                args: short_string(u, 64)?,
            },
            9 => Event::OpenAiDone,
            10 => Event::AnthropicText(short_string(u, 64)?),
            11 => Event::AnthropicThinking {
                idx: u.arbitrary()?,
                text: short_string(u, 64)?,
            },
            12 => Event::AnthropicToolStart {
                idx: u.arbitrary()?,
                id: short_string(u, 24)?,
                name: short_string(u, 24)?,
            },
            13 => Event::AnthropicToolArgs {
                idx: u.arbitrary()?,
                args: short_string(u, 64)?,
            },
            _ => Event::AnthropicDone,
        })
    }
}

#[derive(Debug)]
struct Input {
    wire: u8,
    chunks: Vec<Vec<u8>>,
    partitions: Vec<u8>,
    events: Vec<Event>,
}

impl<'a> Arbitrary<'a> for Input {
    fn arbitrary(u: &mut Unstructured<'a>) -> arbitrary::Result<Self> {
        let wire = u.arbitrary()?;
        let chunk_count = u.int_in_range(0u8..=16)?;
        let mut chunks = Vec::with_capacity(chunk_count as usize);
        for _ in 0..chunk_count {
            chunks.push(short_bytes(u, 128)?);
        }
        let partition_count = u.int_in_range(0u8..=32)?;
        let mut partitions = Vec::with_capacity(partition_count as usize);
        for _ in 0..partition_count {
            partitions.push(u.arbitrary()?);
        }
        let event_count = u.int_in_range(0u8..=48)?;
        let mut events = Vec::with_capacity(event_count as usize);
        for _ in 0..event_count {
            events.push(u.arbitrary()?);
        }
        Ok(Input {
            wire,
            chunks,
            partitions,
            events,
        })
    }
}

fn short_bytes(u: &mut Unstructured<'_>, max: usize) -> arbitrary::Result<Vec<u8>> {
    let len = u.int_in_range(0..=max)?;
    (0..len).map(|_| u.arbitrary::<u8>()).collect()
}

fn short_string(u: &mut Unstructured<'_>, max: usize) -> arbitrary::Result<String> {
    Ok(String::from_utf8_lossy(&short_bytes(u, max)?).into_owned())
}

fn event_json(event: &Event) -> serde_json::Value {
    match event {
        Event::Raw(s) => serde_json::from_str(s).unwrap_or_else(|_| serde_json::json!({"raw": s})),
        Event::ChatText(text) => serde_json::json!({"choices":[{"delta":{"content":text}}]}),
        Event::ChatThinking(text) => {
            serde_json::json!({"choices":[{"delta":{"reasoning_content":text}}]})
        }
        Event::ChatTool {
            idx,
            id,
            name,
            args,
        } => serde_json::json!({
            "choices":[{"delta":{"tool_calls":[{"index":idx,"id":id,"function":{"name":name,"arguments":args}}]}}]
        }),
        Event::ChatDone => serde_json::json!({"choices":[{"finish_reason":"stop","delta":{}}]}),
        Event::OpenAiText(text) => {
            serde_json::json!({"type":"response.output_text.delta","delta":text})
        }
        Event::OpenAiThinking(text) => {
            serde_json::json!({"type":"response.reasoning.delta","delta":text})
        }
        Event::OpenAiToolStart { item, call, name } => serde_json::json!({
            "type":"response.output_item.added",
            "item":{"type":"function_call","id":item,"call_id":call,"name":name}
        }),
        Event::OpenAiToolArgs { item, args } => serde_json::json!({
            "type":"response.function_call_arguments.delta","item_id":item,"delta":args
        }),
        Event::OpenAiDone => {
            serde_json::json!({"type":"response.completed","response":{"usage":{"input_tokens":1,"output_tokens":1}}})
        }
        Event::AnthropicText(text) => {
            serde_json::json!({"type":"content_block_delta","delta":{"type":"text_delta","text":text}})
        }
        Event::AnthropicThinking { idx, text } => serde_json::json!({
            "type":"content_block_delta","index":idx,"delta":{"type":"thinking_delta","thinking":text}
        }),
        Event::AnthropicToolStart { idx, id, name } => serde_json::json!({
            "type":"content_block_start","index":idx,"content_block":{"type":"tool_use","id":id,"name":name}
        }),
        Event::AnthropicToolArgs { idx, args } => serde_json::json!({
            "type":"content_block_delta","index":idx,"delta":{"type":"input_json_delta","partial_json":args}
        }),
        Event::AnthropicDone => serde_json::json!({"type":"message_stop"}),
    }
}

fn expected_stream_summary(wire: u8, events: &[Event]) -> ExpectedSummary {
    let mut summary = ExpectedSummary {
        ok: false,
        content_len: 0,
        reasoning_len: 0,
        text_deltas: 0,
        thinking_deltas: 0,
        tool_arg_deltas: 0,
    };
    match wire % 3 {
        0 => {
            let mut tools = std::collections::HashMap::new();
            for event in events {
                match event {
                    Event::ChatText(text) if !text.is_empty() => {
                        summary.content_len += text.len();
                        summary.text_deltas += 1;
                    }
                    Event::ChatThinking(text) if !text.is_empty() => {
                        summary.reasoning_len += text.len();
                        summary.thinking_deltas += 1;
                    }
                    Event::ChatTool {
                        idx,
                        id,
                        name,
                        args,
                    } => {
                        let tool = tools.entry(*idx).or_insert(("", "", String::new()));
                        if tool.0.is_empty() {
                            tool.0 = id.as_str();
                        }
                        if tool.1.is_empty() {
                            tool.1 = name.as_str();
                        }
                        tool.2.push_str(args);
                        if !args.is_empty() {
                            summary.tool_arg_deltas += 1;
                        }
                    }
                    Event::ChatDone => summary.ok = true,
                    _ => {}
                }
            }
            summary.ok &= tools.values().all(|(id, name, args)| {
                !id.is_empty()
                    && !name.is_empty()
                    && serde_json::from_str::<serde_json::Value>(args).is_ok()
            });
        }
        1 => {
            let mut active_tools = std::collections::HashSet::new();
            for event in events {
                match event {
                    Event::OpenAiText(text) if !text.is_empty() => {
                        summary.content_len += text.len();
                        summary.text_deltas += 1;
                    }
                    Event::OpenAiThinking(text) if !text.is_empty() => {
                        summary.reasoning_len += text.len();
                        summary.thinking_deltas += 1;
                    }
                    Event::OpenAiToolStart { item, .. } if !item.is_empty() => {
                        active_tools.insert(item.as_str());
                    }
                    Event::OpenAiToolArgs { item, args }
                        if !args.is_empty() && active_tools.contains(item.as_str()) =>
                    {
                        summary.tool_arg_deltas += 1;
                    }
                    Event::OpenAiDone => summary.ok = true,
                    _ => {}
                }
            }
        }
        _ => {
            let mut active_tools = std::collections::HashSet::new();
            for event in events {
                match event {
                    Event::AnthropicText(text) if !text.is_empty() => {
                        summary.content_len += text.len();
                        summary.text_deltas += 1;
                    }
                    Event::AnthropicThinking { text, .. } if !text.is_empty() => {
                        summary.reasoning_len += text.len();
                        summary.thinking_deltas += 1;
                    }
                    Event::AnthropicToolStart { idx, .. } => {
                        active_tools.insert(*idx);
                    }
                    Event::AnthropicToolArgs { idx, args }
                        if !args.is_empty() && active_tools.contains(idx) =>
                    {
                        summary.tool_arg_deltas += 1;
                    }
                    Event::AnthropicDone => summary.ok = true,
                    _ => {}
                }
            }
        }
    }
    if !summary.ok {
        summary.content_len = 0;
        summary.reasoning_len = 0;
    }
    summary
}

fn assert_controlled_summary(actual: &FuzzProviderSummary, expected: ExpectedSummary) {
    assert_eq!(
        actual.ok, expected.ok,
        "provider stream completion status changed"
    );
    assert_eq!(
        actual.content_len, expected.content_len,
        "provider stream content length changed"
    );
    assert_eq!(
        actual.reasoning_len, expected.reasoning_len,
        "provider stream reasoning length changed"
    );
    assert_eq!(
        actual.text_deltas, expected.text_deltas,
        "provider stream text delta count changed"
    );
    assert_eq!(
        actual.thinking_deltas, expected.thinking_deltas,
        "provider stream thinking delta count changed"
    );
    assert_eq!(
        actual.tool_arg_deltas, expected.tool_arg_deltas,
        "provider stream tool argument delta count changed"
    );
}

fn terminal_event(wire: u8) -> Event {
    match wire % 3 {
        0 => Event::ChatDone,
        1 => Event::OpenAiDone,
        _ => Event::AnthropicDone,
    }
}

fn decode_sse<'a>(
    wire: u8,
    chunks: impl IntoIterator<Item = &'a [u8]>,
) -> (
    Vec<serde_json::Value>,
    FuzzSseSummary,
    Option<std::mem::Discriminant<ProviderError>>,
) {
    let mut decoder = FuzzSseDecoder::default();
    let cancel = CancellationToken::new();
    let mut events = Vec::new();
    for chunk in chunks {
        let result = decoder.feed(chunk, &cancel, &mut |event| {
            events.push(fuzz_parse_sse_event(wire, event)?);
            Ok(())
        });
        if let Err(error) = result {
            let summary = decoder.summary();
            let buffered = decoder.buffered_bytes();
            let terminal = decoder.feed(b"data: {}\n\n", &cancel, &mut |_| {
                panic!("event after failure")
            });
            assert_eq!(
                std::mem::discriminant(&terminal.unwrap_err()),
                std::mem::discriminant(&error)
            );
            assert_eq!(decoder.summary(), summary);
            assert_eq!(decoder.buffered_bytes(), buffered);
            return (events, summary, Some(std::mem::discriminant(&error)));
        }
    }
    let error = decoder
        .finish()
        .err()
        .map(|error| std::mem::discriminant(&error));
    (events, decoder.summary(), error)
}

fn assert_sse_partition_invariance(wire: u8, raw: &[u8], partitions: &[u8]) {
    let mut chunks = Vec::new();
    let mut offset = 0;
    for partition in partitions {
        let remaining = raw.len() - offset;
        if remaining == 0 {
            break;
        }
        let end = offset + usize::from(*partition) % remaining + 1;
        chunks.push(&raw[offset..end]);
        offset = end;
    }
    chunks.push(&raw[offset..]);
    assert_eq!(
        decode_sse(wire, chunks),
        decode_sse(wire, [raw]),
        "SSE prefix, error, or termination depends on chunk boundaries"
    );
}

fuzz_target!(|input: Input| {
    let Input {
        wire,
        chunks,
        partitions,
        events: input_events,
    } = input;
    let raw = chunks.concat();
    let decoded = decode_sse(wire, chunks.iter().map(Vec::as_slice));
    assert_eq!(decoded, decode_sse(wire, [raw.as_slice()]));
    assert_sse_partition_invariance(wire, &raw, &partitions);
    let (drained, _, _) = decoded;
    for event in &drained {
        let _ = fuzz_parse_provider_response(wire, event);
    }

    let mut uncontrolled: Vec<_> = input_events
        .iter()
        .filter(|event| matches!(event, Event::Raw(_)))
        .map(event_json)
        .collect();
    let input_events: Vec<_> = input_events
        .into_iter()
        .filter(|event| !matches!(event, Event::Raw(_)))
        .collect();
    let controlled: Vec<_> = input_events.iter().map(event_json).collect();
    let expected = expected_stream_summary(wire, &input_events);
    let controlled_summary = fuzz_parse_provider_stream(wire, &controlled);
    assert_controlled_summary(&controlled_summary, expected);

    let mut complete_events = input_events.clone();
    complete_events.push(terminal_event(wire));
    let complete: Vec<_> = complete_events.iter().map(event_json).collect();
    let complete_expected = expected_stream_summary(wire, &complete_events);
    let complete_summary = fuzz_parse_provider_stream(wire, &complete);
    assert_controlled_summary(&complete_summary, complete_expected);
    if wire % 3 != 0 {
        assert!(
            complete_summary.ok,
            "terminal provider event did not complete lifecycle"
        );
    }
    let mut raw = Vec::new();
    for event in complete.iter().chain(&uncontrolled) {
        raw.extend_from_slice(format!("data: {event}\n\n").as_bytes());
    }
    raw.extend_from_slice(b"data: [DONE]\n\n");
    assert_sse_partition_invariance(wire, &raw, &partitions);

    let mut all_events = drained;
    all_events.append(&mut uncontrolled);
    all_events.extend(controlled);
    let summary = fuzz_parse_provider_stream(wire, &all_events);
    assert!(summary.content_len <= 4096 * all_events.len().max(1));
});
