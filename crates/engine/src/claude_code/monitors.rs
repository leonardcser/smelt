//! Claude Code sends monitor deliveries to its model but not its SDK stream.
//! Follow the root journal named by its Monitor permission hook, from launch
//! onward. Reading is independent of the reply stream and optional.

use protocol::{HistoryNote, ProcessStatusEvent};
use serde_json::Value;
use std::{collections::HashSet, path::Path, time::Duration};
use tokio::{
    fs::File,
    io::{AsyncReadExt, AsyncSeekExt},
    sync::mpsc,
};

pub(super) async fn follow(input: &Value, provider: String) -> Option<mpsc::Receiver<HistoryNote>> {
    let session = input["session_id"].as_str()?;
    let path = Path::new(input["transcript_path"].as_str()?);
    if !path.is_absolute() || path.file_stem()?.to_str()? != session || path.extension()? != "jsonl"
    {
        return None;
    }
    let mut file = File::open(path).await.ok()?;
    if !file.metadata().await.ok()?.is_file() {
        return None;
    }
    file.seek(std::io::SeekFrom::End(0)).await.ok()?;
    let session = session.to_owned();
    let (tx, rx) = mpsc::channel(16);
    tokio::spawn(async move {
        let mut buf = [0; 16384];
        let mut line = Vec::new();
        let mut oversized = false;
        let mut seen = HashSet::new();
        loop {
            if tx.is_closed() {
                break;
            }
            match file.read(&mut buf).await {
                Ok(0) => tokio::select! {
                    _ = tx.closed() => break,
                    _ = tokio::time::sleep(Duration::from_millis(100)) => {},
                },
                Ok(n) => {
                    for part in buf[..n].split_inclusive(|b| *b == b'\n') {
                        oversized |= line.len() + part.len() > 1024 * 1024;
                        if !oversized {
                            line.extend_from_slice(part);
                        }
                        if part.ends_with(b"\n") {
                            if !oversized {
                                if let Some((id, note)) = delivery(&line, &session, &provider) {
                                    if seen.insert(id) && tx.send(note).await.is_err() {
                                        return;
                                    }
                                }
                            }
                            line.clear();
                            oversized = false;
                        }
                    }
                }
                Err(_) => break,
            }
        }
    });
    Some(rx)
}

fn delivery(line: &[u8], session: &str, provider: &str) -> Option<(String, HistoryNote)> {
    let row: Value = serde_json::from_slice(line).ok()?;
    if row["type"] != "user"
        || row["sessionId"] != session
        || row["isSidechain"] != false
        || row["origin"]["kind"] != "task-notification"
    {
        return None;
    }
    let text = row["message"]["content"]
        .as_str()?
        .trim()
        .strip_prefix("<task-notification>")?
        .trim_start();
    let (task, rest) = text.strip_prefix("<task-id>")?.split_once("</task-id>")?;
    let (name, rest) = rest
        .trim_start()
        .strip_prefix("<summary>Monitor event: \"")?
        .split_once("\"</summary>")?;
    let (body, _) = rest
        .trim_start()
        .strip_prefix("<event>")?
        .split_once("</event>")?;
    let decode = |s: &str| {
        s.replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&quot;", "\"")
            .replace("&apos;", "'")
            .replace("&amp;", "&")
    };
    Some((
        row["uuid"].as_str()?.into(),
        HistoryNote::process_status_with_output(
            ProcessStatusEvent::MonitorEvent {
                process_id: super::native_id(provider, task),
                name: decode(name),
            },
            decode(body),
        ),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[tokio::test]
    async fn only_new_root_deliveries_from_this_session_are_reported_once() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("root.jsonl");
        let mut file = std::fs::File::create(&path).unwrap();
        let mut row = json!({"type":"user","sessionId":"root","isSidechain":false,"origin":{"kind":"task-notification"},"uuid":"old","message":{"content":"<task-notification>\n<task-id>b1</task-id>\n<summary>Monitor event: \"watch\"</summary>\n<event># one &amp; two</event>\n</task-notification>"}});
        writeln!(file, "{row}").unwrap();
        let mut rx = follow(
            &json!({"transcript_path":path,"session_id":"root"}),
            "cc".into(),
        )
        .await
        .unwrap();
        row["uuid"] = json!("one");
        row["origin"]["kind"] = json!("human");
        writeln!(file, "{row}").unwrap();
        row["origin"]["kind"] = json!("task-notification");
        for id in ["one", "one", "two"] {
            row["uuid"] = json!(id);
            row["message"]["content"] = json!(row["message"]["content"]
                .as_str()
                .unwrap()
                .replace("# one", &format!("# {id}")));
            writeln!(file, "{row}").unwrap();
        }
        for expected in ["# one & two", "# two & two"] {
            let note = tokio::time::timeout(Duration::from_secs(2), rx.recv())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(note.text(), "event from monitor watch");
            assert_eq!(note.process_output(), Some(expected));
        }
    }
}
