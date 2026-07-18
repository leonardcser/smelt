use crate::{CancellationToken, ProviderError};
use futures_util::StreamExt;

/// Drain complete SSE events from a network byte buffer. Processed lines are
/// removed and partial lines remain. Keeping incomplete lines as bytes prevents
/// a UTF-8 code point split across response chunks from being replaced once per
/// chunk. Lossy conversion happens exactly once, after a full SSE line arrives.
pub(crate) fn drain_sse_bytes(buf: &mut Vec<u8>) -> Vec<serde_json::Value> {
    let Some(complete_len) = buf
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map(|position| position + 1)
    else {
        return Vec::new();
    };

    let mut events = Vec::new();
    for raw in buf[..complete_len].split(|byte| *byte == b'\n') {
        let raw = raw.strip_suffix(b"\r").unwrap_or(raw);
        let line = String::from_utf8_lossy(raw);
        if let Some(event) = parse_data_line(&line) {
            events.push(event);
        }
    }
    buf.drain(..complete_len);
    events
}

fn parse_data_line(line: &str) -> Option<serde_json::Value> {
    let data = line
        .strip_prefix("data:")
        .map(|rest| rest.strip_prefix(' ').unwrap_or(rest))?;
    if data == "[DONE]" {
        return None;
    }
    serde_json::from_str(data).ok()
}

pub async fn read_events(
    resp: reqwest::Response,
    cancel: &CancellationToken,
    mut handler: impl FnMut(&serde_json::Value),
) -> Result<(), ProviderError> {
    let mut buf = Vec::new();
    let mut stream = resp.bytes_stream();

    loop {
        let chunk = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(ProviderError::Cancelled),
            chunk = stream.next() => chunk,
        };
        let chunk = match chunk {
            Some(Ok(bytes)) => bytes,
            Some(Err(e)) => return Err(ProviderError::Network(e.to_string())),
            None => break,
        };
        buf.extend_from_slice(&chunk);

        for event in drain_sse_bytes(&mut buf) {
            handler(&event);
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn bytes(value: &str) -> Vec<u8> {
        value.as_bytes().to_vec()
    }

    #[test]
    fn drain_returns_events_for_complete_data_lines() {
        let mut buf = bytes("data: {\"a\":1}\ndata: {\"b\":2}\n");
        let events = drain_sse_bytes(&mut buf);
        assert_eq!(events, vec![json!({"a": 1}), json!({"b": 2})]);
        assert!(buf.is_empty());
    }

    #[test]
    fn drain_preserves_partial_trailing_line() {
        let mut buf = bytes("data: {\"a\":1}\ndata: {\"b\"");
        let events = drain_sse_bytes(&mut buf);
        assert_eq!(events, vec![json!({"a": 1})]);
        assert_eq!(buf, b"data: {\"b\"");
    }

    #[test]
    fn drain_handles_carriage_return_line_endings() {
        let mut buf = bytes("data: {\"a\":1}\r\n");
        let events = drain_sse_bytes(&mut buf);
        assert_eq!(events, vec![json!({"a": 1})]);
    }

    #[test]
    fn drain_accepts_data_prefix_without_leading_space() {
        let mut buf = bytes("data:{\"a\":1}\n");
        let events = drain_sse_bytes(&mut buf);
        assert_eq!(events, vec![json!({"a": 1})]);
    }

    #[test]
    fn drain_skips_done_marker() {
        let mut buf = bytes("data: {\"a\":1}\ndata: [DONE]\n");
        let events = drain_sse_bytes(&mut buf);
        assert_eq!(events, vec![json!({"a": 1})]);
    }

    #[test]
    fn drain_skips_non_data_lines() {
        let mut buf = bytes("event: ping\ndata: {\"a\":1}\n: comment\n\n");
        let events = drain_sse_bytes(&mut buf);
        assert_eq!(events, vec![json!({"a": 1})]);
    }

    #[test]
    fn drain_skips_invalid_json_silently() {
        let mut buf = bytes("data: {not valid}\ndata: {\"ok\":true}\n");
        let events = drain_sse_bytes(&mut buf);
        assert_eq!(events, vec![json!({"ok": true})]);
    }

    #[test]
    fn drain_empty_buffer_returns_no_events() {
        let mut buf = Vec::new();
        let events = drain_sse_bytes(&mut buf);
        assert!(events.is_empty());
    }

    #[test]
    fn drain_buffer_without_newline_returns_no_events_keeps_buf() {
        let mut buf = bytes("data: {\"a\":1}");
        let events = drain_sse_bytes(&mut buf);
        assert!(events.is_empty());
        assert_eq!(buf, b"data: {\"a\":1}");
    }

    #[test]
    fn drain_is_idempotent_across_chunked_input() {
        let mut buf = Vec::new();
        buf.extend_from_slice(b"data: {\"par");
        assert!(drain_sse_bytes(&mut buf).is_empty());
        buf.extend_from_slice(b"t\":1}\n");
        let events = drain_sse_bytes(&mut buf);
        assert_eq!(events, vec![json!({"part": 1})]);
        assert!(buf.is_empty());
    }

    #[test]
    fn byte_drain_preserves_utf8_split_across_network_chunks() {
        let raw = "data: {\"text\":\"界\"}\n".as_bytes();
        let split = raw
            .windows("界".len())
            .position(|window| window == "界".as_bytes())
            .unwrap()
            + 1;
        let mut buf = raw[..split].to_vec();
        assert!(drain_sse_bytes(&mut buf).is_empty());
        buf.extend_from_slice(&raw[split..]);
        assert_eq!(drain_sse_bytes(&mut buf), vec![json!({"text": "界"})]);
        assert!(buf.is_empty());
    }

    #[test]
    fn drain_handles_blank_lines_between_events() {
        // SSE spec allows event separators (blank lines); they're non-data and dropped.
        let mut buf = bytes("data: {\"a\":1}\n\ndata: {\"b\":2}\n\n");
        let events = drain_sse_bytes(&mut buf);
        assert_eq!(events, vec![json!({"a": 1}), json!({"b": 2})]);
    }
}
