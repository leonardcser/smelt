use super::ProviderError;
use crate::cancel::CancellationToken;
use futures_util::StreamExt;

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

        while let Some(pos) = buf.find('\n') {
            let raw: String = buf.drain(..pos + 1).collect();
            let line = raw.trim_end_matches('\n').trim_end_matches('\r');

            if !line.starts_with("data: ") {
                continue;
            }
            let data = &line[6..];
            if data == "[DONE]" {
                continue;
            }

            if let Ok(ev) = serde_json::from_str::<serde_json::Value>(data) {
                handler(&ev);
            }
        }
    }

    Ok(())
}
