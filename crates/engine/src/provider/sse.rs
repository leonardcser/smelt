use super::ProviderError;
use crate::cancel::CancellationToken;
use futures_util::StreamExt;

/// Drain complete SSE events from `buf`. Strips processed lines; partial lines
/// remain. `data:` lines are JSON-parsed; `[DONE]` markers and non-data lines
/// are skipped. Pure — called repeatedly as new chunks arrive.
pub(super) fn drain_sse_events(buf: &mut String) -> Vec<serde_json::Value> {
    let mut events = Vec::new();
    while let Some(pos) = buf.find('\n') {
        let raw: String = buf.drain(..pos + 1).collect();
        let line = raw.trim_end_matches('\n').trim_end_matches('\r');

        let data = if let Some(rest) = line.strip_prefix("data:") {
            rest.strip_prefix(' ').unwrap_or(rest)
        } else {
            continue;
        };
        if data == "[DONE]" {
            continue;
        }

        if let Ok(ev) = serde_json::from_str::<serde_json::Value>(data) {
            events.push(ev);
        }
    }
    events
}

pub(super) async fn read_events(
    resp: reqwest::Response,
    cancel: &CancellationToken,
    mut handler: impl FnMut(&serde_json::Value),
) -> Result<(), ProviderError> {
    let mut buf = String::new();
    let mut stream = resp.bytes_stream();

    loop {
        let chunk = tokio::select! {
            _ = cancel.cancelled() => return Err(ProviderError::Cancelled),
            chunk = stream.next() => chunk,
        };
        let chunk = match chunk {
            Some(Ok(bytes)) => bytes,
            Some(Err(e)) => return Err(ProviderError::Network(e.to_string())),
            None => break,
        };
        buf.push_str(&String::from_utf8_lossy(&chunk));

        for event in drain_sse_events(&mut buf) {
            handler(&event);
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn drain_returns_events_for_complete_data_lines() {
        let mut buf = String::from("data: {\"a\":1}\ndata: {\"b\":2}\n");
        let events = drain_sse_events(&mut buf);
        assert_eq!(events, vec![json!({"a": 1}), json!({"b": 2})]);
        assert!(buf.is_empty());
    }

    #[test]
    fn drain_preserves_partial_trailing_line() {
        let mut buf = String::from("data: {\"a\":1}\ndata: {\"b\"");
        let events = drain_sse_events(&mut buf);
        assert_eq!(events, vec![json!({"a": 1})]);
        assert_eq!(buf, "data: {\"b\"");
    }

    #[test]
    fn drain_handles_carriage_return_line_endings() {
        let mut buf = String::from("data: {\"a\":1}\r\n");
        let events = drain_sse_events(&mut buf);
        assert_eq!(events, vec![json!({"a": 1})]);
    }

    #[test]
    fn drain_accepts_data_prefix_without_leading_space() {
        let mut buf = String::from("data:{\"a\":1}\n");
        let events = drain_sse_events(&mut buf);
        assert_eq!(events, vec![json!({"a": 1})]);
    }

    #[test]
    fn drain_skips_done_marker() {
        let mut buf = String::from("data: {\"a\":1}\ndata: [DONE]\n");
        let events = drain_sse_events(&mut buf);
        assert_eq!(events, vec![json!({"a": 1})]);
    }

    #[test]
    fn drain_skips_non_data_lines() {
        let mut buf = String::from("event: ping\ndata: {\"a\":1}\n: comment\n\n");
        let events = drain_sse_events(&mut buf);
        assert_eq!(events, vec![json!({"a": 1})]);
    }

    #[test]
    fn drain_skips_invalid_json_silently() {
        let mut buf = String::from("data: {not valid}\ndata: {\"ok\":true}\n");
        let events = drain_sse_events(&mut buf);
        assert_eq!(events, vec![json!({"ok": true})]);
    }

    #[test]
    fn drain_empty_buffer_returns_no_events() {
        let mut buf = String::new();
        let events = drain_sse_events(&mut buf);
        assert!(events.is_empty());
    }

    #[test]
    fn drain_buffer_without_newline_returns_no_events_keeps_buf() {
        let mut buf = String::from("data: {\"a\":1}");
        let events = drain_sse_events(&mut buf);
        assert!(events.is_empty());
        assert_eq!(buf, "data: {\"a\":1}");
    }

    #[test]
    fn drain_is_idempotent_across_chunked_input() {
        let mut buf = String::new();
        buf.push_str("data: {\"par");
        assert!(drain_sse_events(&mut buf).is_empty());
        buf.push_str("t\":1}\n");
        let events = drain_sse_events(&mut buf);
        assert_eq!(events, vec![json!({"part": 1})]);
        assert!(buf.is_empty());
    }

    #[test]
    fn drain_handles_blank_lines_between_events() {
        // SSE spec allows event separators (blank lines); they're non-data and dropped.
        let mut buf = String::from("data: {\"a\":1}\n\ndata: {\"b\":2}\n\n");
        let events = drain_sse_events(&mut buf);
        assert_eq!(events, vec![json!({"a": 1}), json!({"b": 2})]);
    }
}
