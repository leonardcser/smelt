use crate::{CancellationToken, ProviderError};
use futures_util::StreamExt;

#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct StreamSummary {
    pub data_events: usize,
    pub saw_done: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Event {
    pub name: String,
    pub data: Vec<u8>,
    pub index: usize,
}

impl Event {
    pub fn json(&self) -> Result<serde_json::Value, ProviderError> {
        serde_json::from_slice(&self.data).map_err(|error| {
            ProviderError::InvalidResponse(format!(
                "malformed JSON in SSE data event (event={}, line={}, column={}, event_bytes={})",
                self.index,
                error.line(),
                error.column(),
                self.data.len()
            ))
        })
    }
}

/// Retain unfinished events and the scan position across network chunks.
#[derive(Default)]
pub struct Decoder {
    buf: Vec<u8>,
    scan: usize,
    line_start: usize,
    summary: StreamSummary,
    error: Option<ProviderError>,
}

impl Decoder {
    pub fn summary(&self) -> StreamSummary {
        self.summary
    }

    pub fn buffered_bytes(&self) -> usize {
        self.buf.len()
    }

    fn diagnostic(&self) -> String {
        format!(
            "data_events={}, done={}, buffered_bytes={}",
            self.summary.data_events,
            self.summary.saw_done,
            self.buffered_bytes()
        )
    }

    pub fn feed(
        &mut self,
        bytes: &[u8],
        cancel: &CancellationToken,
        handler: &mut impl FnMut(&Event) -> Result<(), ProviderError>,
    ) -> Result<(), ProviderError> {
        if let Some(error) = &self.error {
            return Err(error.clone());
        }
        self.buf.extend_from_slice(bytes);
        let mut consumed = 0;
        let result = (|| {
            if cancel.is_cancelled() {
                return Err(ProviderError::Cancelled);
            }
            while self.scan < self.buf.len() {
                let end = self.scan;
                self.scan += 1;
                if self.buf[end] != b'\n' {
                    continue;
                }
                let line = self.buf[self.line_start..end]
                    .strip_suffix(b"\r")
                    .unwrap_or(&self.buf[self.line_start..end]);
                let frame_end = self.line_start;
                self.line_start = self.scan;
                if !line.is_empty() {
                    continue;
                }
                let event = parse_event(&self.buf[consumed..frame_end]);
                consumed = self.scan;
                let Some(mut event) = event else { continue };
                self.summary.data_events += 1;
                event.index = self.summary.data_events;
                if event.name.is_empty() || event.name == "message" {
                    // Empty messages are commonly used as keepalives.
                    if event.data.is_empty() {
                        continue;
                    }
                    if event.data == b"[DONE]" {
                        self.summary.saw_done = true;
                        continue;
                    }
                }
                if cancel.is_cancelled() {
                    return Err(ProviderError::Cancelled);
                }
                let result = handler(&event);
                if cancel.is_cancelled() {
                    return Err(ProviderError::Cancelled);
                }
                result?;
            }
            Ok(())
        })();
        self.buf.drain(..consumed);
        self.scan -= consumed;
        self.line_start -= consumed;
        if let Err(error) = &result {
            self.error = Some(error.clone());
        }
        result
    }

    pub fn finish(&self) -> Result<StreamSummary, ProviderError> {
        if let Some(error) = &self.error {
            return Err(error.clone());
        }
        let comments_only = self.buf.split(|byte| *byte == b'\n').all(|line| {
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            line.is_empty() || line.starts_with(b":")
        });
        if !comments_only {
            return Err(ProviderError::InvalidResponse(format!(
                "EOF with incomplete SSE event ({})",
                self.diagnostic()
            )));
        }
        Ok(self.summary())
    }
}

fn parse_event(frame: &[u8]) -> Option<Event> {
    let mut name = String::new();
    let mut data = Vec::new();
    let mut has_data = false;
    for raw in frame.split(|byte| *byte == b'\n') {
        let line = raw.strip_suffix(b"\r").unwrap_or(raw);
        let (field, value) = match line.iter().position(|byte| *byte == b':') {
            Some(colon) => (&line[..colon], &line[colon + 1..]),
            None => (line, &b""[..]),
        };
        let value = value.strip_prefix(b" ").unwrap_or(value);
        if field == b"event" {
            name = String::from_utf8_lossy(value).into_owned();
        } else if field == b"data" {
            if has_data {
                data.push(b'\n');
            }
            has_data = true;
            data.extend_from_slice(value);
        }
    }
    has_data.then_some(Event {
        name,
        data,
        index: 0,
    })
}

pub async fn read_events(
    resp: reqwest::Response,
    cancel: &CancellationToken,
    mut handler: impl FnMut(&Event) -> Result<(), ProviderError>,
) -> Result<StreamSummary, ProviderError> {
    let mut decoder = Decoder::default();
    let mut stream = resp.bytes_stream();
    loop {
        let chunk = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(ProviderError::Cancelled),
            chunk = stream.next() => chunk,
        };
        match chunk {
            Some(Ok(bytes)) => decoder.feed(&bytes, cancel, &mut handler)?,
            Some(Err(_)) => {
                return Err(ProviderError::Network(format!(
                    "SSE network read failed ({})",
                    decoder.diagnostic()
                )))
            }
            None => return decoder.finish(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn decode(chunks: &[&[u8]]) -> (Decoder, Vec<serde_json::Value>) {
        let mut decoder = Decoder::default();
        let mut events = Vec::new();
        for chunk in chunks {
            decoder
                .feed(chunk, &CancellationToken::new(), &mut |event| {
                    events.push(event.json()?);
                    Ok(())
                })
                .unwrap();
        }
        (decoder, events)
    }

    async fn response(body: &str) -> reqwest::Response {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut reader = tokio::io::BufReader::new(stream);
            let mut line = Vec::new();
            loop {
                line.clear();
                assert_ne!(reader.read_until(b'\n', &mut line).await.unwrap(), 0);
                if line == b"\r\n" {
                    break;
                }
            }
            reader
                .get_mut()
                .write_all(response.as_bytes())
                .await
                .unwrap();
        });
        reqwest::get(format!("http://{addr}")).await.unwrap()
    }

    #[tokio::test]
    async fn shared_reader_preserves_provider_completion_rules() {
        for (wire, marker) in [
            (
                crate::WireApi::ChatCompletions,
                json!({"choices": [{"finish_reason": "stop"}]}),
            ),
            (
                crate::WireApi::OpenAiResponses,
                json!({"type": "response.completed", "response": {"usage": {}}}),
            ),
            (
                crate::WireApi::AnthropicMessages,
                json!({"type": "message_stop"}),
            ),
        ] {
            let cancel = CancellationToken::new();
            let resp = response(&format!("data: {marker}\n\ndata: [DONE]\n\n")).await;
            assert!(wire.read_stream(resp, &cancel, &|_| {}, 0).await.is_ok());
            let resp = response("data: [DONE]\n\n").await;
            assert!(wire.read_stream(resp, &cancel, &|_| {}, 0).await.is_err());
        }
    }

    #[tokio::test]
    async fn cancellation_precedes_malformed_data() {
        let resp = response("data: {malformed}\n\n").await;
        let cancel = CancellationToken::new();
        cancel.cancel();
        let result = read_events(resp, &cancel, |_| panic!("cancelled event dispatched")).await;
        assert!(matches!(result, Err(ProviderError::Cancelled)));
    }

    #[tokio::test]
    async fn cancellation_after_delta_precedes_parse_and_eof_errors() {
        for tail in ["data: {", "data: {malformed}\n\n"] {
            let resp = response(&format!("data: {{}}\n\n{tail}")).await;
            let cancel = CancellationToken::new();
            let mut events = 0;
            let result = read_events(resp, &cancel, |_| {
                events += 1;
                cancel.cancel();
                Ok(())
            })
            .await;
            assert_eq!(events, 1);
            assert!(matches!(result, Err(ProviderError::Cancelled)));
        }
    }

    #[test]
    fn framing_is_partition_invariant() {
        for raw in [
            "data:{\"text\":\"界\"}\n\ndata: [DONE]\n\n",
            "event: message\r\ndata: {\"text\":\r\n: keepalive\r\ndata: \"界\"}\r\n\r\ndata: [DONE]\n\n",
        ] {
            let raw = raw.as_bytes();
            for split in 0..=raw.len() {
                let (decoder, events) = decode(&[&raw[..split], &raw[split..]]);
                assert_eq!(events, vec![json!({"text": "界"})]);
                assert_eq!(decoder.buffered_bytes(), 0);
                assert_eq!(decoder.finish().unwrap(), StreamSummary { data_events: 2, saw_done: true });
            }
            let (decoder, events) = decode(&raw.chunks(1).collect::<Vec<_>>());
            assert_eq!(events, vec![json!({"text": "界"})]);
            assert!(decoder.finish().unwrap().saw_done);
        }
    }

    #[test]
    fn ignores_comments_metadata_and_empty_keepalives() {
        let (decoder, events) = decode(&[b": ping\n\nid: 1\nevent: ping\n\ndata:\n\ndata: {\"usage\":{},\"error\":null}\n\n: tail"]);
        assert_eq!(events, vec![json!({"usage": {}, "error": null})]);
        assert_eq!(decoder.finish().unwrap().data_events, 2);
        assert!(decode(&[b""]).0.finish().is_ok());
    }

    #[test]
    fn retains_partial_events_without_dispatching_at_eof() {
        for tail in [
            "data: {\"b\"",
            "data: {\"b\":2}",
            "data: {\"b\":2}\n",
            "event: message\n",
        ] {
            let raw = format!("data: {{\"a\":1}}\n\n{tail}");
            let (decoder, events) = decode(&[raw.as_bytes()]);
            assert_eq!(events, vec![json!({"a": 1})]);
            assert_eq!(decoder.buffered_bytes(), tail.len());
            let error = decoder.finish().unwrap_err().to_string();
            assert!(error.contains("EOF with incomplete SSE event"));
            assert!(error.contains(&format!("buffered_bytes={}", tail.len())));
        }
        let (mut decoder, events) = decode(&[b"data: {\"ok\":true}\n"]);
        assert!(events.is_empty());
        let mut events = Vec::new();
        decoder
            .feed(b"\n", &CancellationToken::new(), &mut |event| {
                events.push(event.json()?);
                Ok(())
            })
            .unwrap();
        assert_eq!(events, vec![json!({"ok": true})]);
        assert!(decoder.finish().is_ok());
    }

    #[test]
    fn callback_failure_is_terminal_at_every_partition() {
        for failure in [
            "{sensitive-fixture}",
            "{\"error\":{\"message\":\"sensitive-fixture\"}}",
        ] {
            let raw =
                format!("data: {{\"ok\":true}}\n\ndata: {failure}\n\ndata: {{\"later\":true}}\n\n");
            for split in 0..=raw.len() {
                let mut decoder = Decoder::default();
                let mut events = Vec::new();
                let cancel = CancellationToken::new();
                let mut handler = |event: &Event| {
                    events.push(crate::error::parse_openai_stream_event(event)?);
                    Ok(())
                };
                let result = decoder
                    .feed(&raw.as_bytes()[..split], &cancel, &mut handler)
                    .and_then(|_| decoder.feed(&raw.as_bytes()[split..], &cancel, &mut handler));
                let error = result.unwrap_err().to_string();
                assert!(!error.contains("sensitive-fixture"));
                assert_eq!(events, vec![json!({"ok": true})]);
                assert_eq!(decoder.summary().data_events, 2);
                let buffered = decoder.buffered_bytes();
                let terminal = decoder.feed(b"data: {}\n\n", &cancel, &mut |_| {
                    panic!("event after failure")
                });
                assert_eq!(terminal.unwrap_err().to_string(), error);
                assert_eq!(decoder.buffered_bytes(), buffered);
                assert_eq!(decoder.finish().unwrap_err().to_string(), error);
            }
        }
    }

    #[test]
    fn cancellation_stops_same_batch_and_precedes_callback_error() {
        for fail in [false, true] {
            let mut decoder = Decoder::default();
            let cancel = CancellationToken::new();
            let mut events = 0;
            let result = decoder.feed(b"data: {}\n\ndata: {}\n\n", &cancel, &mut |_| {
                events += 1;
                cancel.cancel();
                if fail {
                    Err(ProviderError::InvalidResponse("callback failed".into()))
                } else {
                    Ok(())
                }
            });
            assert_eq!(events, 1);
            assert!(matches!(result, Err(ProviderError::Cancelled)));
            assert!(matches!(decoder.finish(), Err(ProviderError::Cancelled)));
        }
    }

    #[tokio::test]
    async fn provider_handlers_reject_safe_named_and_envelope_errors() {
        for wire in [
            crate::WireApi::ChatCompletions,
            crate::WireApi::OpenAiResponses,
            crate::WireApi::AnthropicMessages,
        ] {
            for raw in [
                "data: {\"error\":{\"message\":\"sensitive-fixture\"}}\n\n",
                "data: {\"type\":\"error\",\"message\":\"sensitive-fixture\"}\n\n",
                "event: error\ndata: sensitive-fixture\n\n",
                "event: error\ndata:\n\n",
                "event: error\ndata: [DONE]\n\n",
            ] {
                let error = wire
                    .read_stream(response(raw).await, &CancellationToken::new(), &|_| {}, 0)
                    .await
                    .err()
                    .expect("upstream error must fail")
                    .to_string();
                assert!(error.contains("upstream error event"));
                assert!(!error.contains("sensitive-fixture"));
            }
        }
    }

    #[tokio::test]
    async fn provider_handlers_preserve_typed_failures_without_payloads() {
        for wire in [
            crate::WireApi::ChatCompletions,
            crate::WireApi::OpenAiResponses,
        ] {
            for code in [
                "insufficient_quota",
                "rate_limit_exceeded",
                "context_length_exceeded",
                "cyber_policy",
            ] {
                let body = format!("event: error\ndata: {{\"error\":{{\"code\":\"{code}\",\"message\":\"sensitive-fixture\"}}}}\n\n");
                let error = wire
                    .read_stream(response(&body).await, &CancellationToken::new(), &|_| {}, 0)
                    .await
                    .err()
                    .expect("upstream error must fail");
                assert!(!format!("{error:?}").contains("sensitive-fixture"));
                assert!(match code {
                    "insufficient_quota" => matches!(error, ProviderError::QuotaExceeded { .. }),
                    "rate_limit_exceeded" => matches!(error, ProviderError::RateLimited { .. }),
                    "context_length_exceeded" =>
                        error.to_string().contains("context_length_exceeded"),
                    _ => matches!(error, ProviderError::CyberPolicy { .. }),
                });
            }
        }
        for kind in [
            "rate_limit_error",
            "authentication_error",
            "permission_error",
            "not_found_error",
            "overloaded_error",
            "api_error",
        ] {
            let body = format!("event: error\ndata: {{\"type\":\"error\",\"error\":{{\"type\":\"{kind}\",\"message\":\"sensitive-fixture\"}}}}\n\n");
            let error = crate::WireApi::AnthropicMessages
                .read_stream(response(&body).await, &CancellationToken::new(), &|_| {}, 0)
                .await
                .err()
                .expect("upstream error must fail");
            assert!(!format!("{error:?}").contains("sensitive-fixture"));
            assert!(match kind {
                "rate_limit_error" => matches!(error, ProviderError::RateLimited { .. }),
                "authentication_error" | "permission_error" =>
                    matches!(error, ProviderError::Auth(_)),
                "not_found_error" => matches!(error, ProviderError::NotFound(_)),
                _ => matches!(error, ProviderError::Server { .. }),
            });
        }
    }
}
