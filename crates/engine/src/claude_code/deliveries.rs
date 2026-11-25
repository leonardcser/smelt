//! What joins a Claude Code conversation on its own. Claude Code takes each
//! delivery as a prompt of its own. It echoes the messages of other agents,
//! wrapped in an envelope naming the sender, but sends monitor events only to
//! its model. Those are read from the root journal its Monitor permission
//! hook names, from launch onward. Reading is independent of the reply stream
//! and optional.

use protocol::{DeliveryEvent, HistoryNote};
use serde_json::Value;
use std::{collections::HashSet, path::Path, time::Duration};
use tokio::{
    fs::File,
    io::{AsyncReadExt, AsyncSeekExt},
    sync::mpsc,
};

/// Envelope tags, with the attributes that name the sender, best first.
const ENVELOPES: &[(&str, &[&str])] = &[
    ("cross-session-message", &["from-name", "from"]),
    ("teammate-message", &["teammate_id", "from", "name"]),
    ("channel", &["source"]),
];

/// The messages in a prompt Claude Code took on its own. A prompt a person
/// typed carries none, even when it quotes an envelope.
pub(super) fn messages(frame: &Value) -> Vec<HistoryNote> {
    let origin = &frame["origin"];
    if origin["kind"] == "human" {
        return Vec::new();
    }
    let text = super::transcript::text_content(&frame["message"]["content"]);
    if origin["kind"] == "peer" {
        let enveloped = envelopes(&text).into_iter().next();
        let from = [&origin["name"], &origin["from"]]
            .into_iter()
            .filter_map(Value::as_str)
            .find(|name| !name.is_empty())
            .map(str::to_owned)
            .or_else(|| enveloped.as_ref().map(|(from, _)| from.clone()))
            .unwrap_or_else(|| "another agent".into());
        let body = origin["body"]
            .as_str()
            .map(str::to_owned)
            .or_else(|| enveloped.map(|(_, body)| body))
            .unwrap_or_default();
        return vec![message(from, &body)];
    }
    envelopes(&text)
        .into_iter()
        .map(|(from, body)| message(from, &body))
        .collect()
}

fn message(from: String, body: &str) -> HistoryNote {
    HistoryNote::delivery(DeliveryEvent::AgentMessage { from }, body.trim())
}

/// Each envelope in `text`, as its sender and body.
fn envelopes(text: &str) -> Vec<(String, String)> {
    let mut found = Vec::new();
    let mut rest = text;
    while let Some((tag, senders, start)) = ENVELOPES
        .iter()
        .filter_map(|(tag, senders)| Some((*tag, *senders, rest.find(&format!("<{tag}"))?)))
        .min_by_key(|(_, _, start)| *start)
    {
        let after = &rest[start + 1 + tag.len()..];
        // The tag name must end here, so `<channel` does not open `<channels`.
        if !after.starts_with([' ', '>', '\n', '\t']) {
            rest = after;
            continue;
        }
        let Some(open_end) = after.find('>') else {
            break;
        };
        let attributes = &after[..open_end];
        let content = &after[open_end + 1..];
        let close = format!("</{tag}>");
        let (body, next) = match content.find(&close) {
            Some(end) => (&content[..end], &content[end + close.len()..]),
            None => (content, ""),
        };
        let from = senders
            .iter()
            .find_map(|name| attribute(attributes, name))
            .unwrap_or_else(|| "another agent".into());
        found.push((from, body.trim().to_owned()));
        rest = next;
    }
    found
}

/// The value of attribute `name` in an opening tag's attribute text.
fn attribute(attributes: &str, name: &str) -> Option<String> {
    let mut rest = attributes;
    while let Some(at) = rest.find(name) {
        let before = rest[..at].chars().next_back();
        let after = &rest[at + name.len()..];
        rest = after;
        if before.is_some_and(|c| !c.is_whitespace()) {
            continue;
        }
        let Some(value) = after.trim_start().strip_prefix('=') else {
            continue;
        };
        let value = value.trim_start().strip_prefix('"')?;
        let value = &value[..value.find('"')?];
        if !value.is_empty() {
            return Some(value.to_owned());
        }
    }
    None
}

/// Follows the root journal of the session the hook `input` names, and
/// sends each monitor event recorded after now.
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
                                if let Some((id, note)) = journal_row(&line, &session, &provider) {
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

/// A monitor event the root agent of `session` received, with its row's ID.
fn journal_row(line: &[u8], session: &str, provider: &str) -> Option<(String, HistoryNote)> {
    let row: Value = serde_json::from_slice(line).ok()?;
    if row["type"] != "user"
        || row["sessionId"] != session
        || row["isSidechain"] != false
        || row["origin"]["kind"] != "task-notification"
    {
        return None;
    }
    let note = monitor_event(row["message"]["content"].as_str()?, provider)?;
    Some((row["uuid"].as_str()?.into(), note))
}

/// The monitor event a `<task-notification>` carries: output a monitor
/// reported, or the last output of one whose command ended. The end itself
/// reaches smelt as the end of the monitor's task.
fn monitor_event(text: &str, provider: &str) -> Option<HistoryNote> {
    let text = text.trim().strip_prefix("<task-notification>")?;
    let task = tag(text, "task-id")?;
    let summary = tag(text, "summary")?;
    let name = summary
        .strip_prefix("Monitor event: \"")
        .and_then(|rest| rest.strip_suffix('"'))
        .or_else(|| {
            summary
                .strip_prefix("Monitor \"")?
                .strip_suffix("\" stream ended")
        })?;
    let body = decode(tag(text, "event")?.trim());
    if body.is_empty() {
        return None;
    }
    Some(HistoryNote::delivery(
        DeliveryEvent::MonitorEvent {
            process_id: super::native_id(provider, task),
            name: decode(name),
        },
        body,
    ))
}

/// The text inside the first `<name>` element of `text`.
fn tag<'a>(text: &'a str, name: &str) -> Option<&'a str> {
    let open = format!("<{name}>");
    let start = text.find(&open)? + open.len();
    let end = text[start..].find(&format!("</{name}>"))?;
    Some(&text[start..start + end])
}

fn decode(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn event(note: &HistoryNote) -> (&str, Option<&str>) {
        let Some(event) = note.delivery_event() else {
            panic!("not a delivery: {note:?}");
        };
        (event.source_name(), note.delivery_body())
    }

    #[test]
    fn a_peer_message_names_its_sender_and_body() {
        let frame = json!({"type":"user", "isReplay":true, "isSynthetic":true,
            "message":{"role":"user", "content":"Another Claude session sent a message:\n<cross-session-message from=\"uds:/run/x.sock\" from-name=\"worker\">\nHi there\n</cross-session-message>\n\nThis came from another Claude session."},
            "origin":{"kind":"peer", "from":"uds:/run/x.sock", "name":"worker", "body":"Hi there"}});
        let notes = messages(&frame);
        assert_eq!(notes.len(), 1);
        assert_eq!(event(&notes[0]), ("worker", Some("Hi there")));
        assert_eq!(notes[0].text(), "message from worker");
    }

    #[test]
    fn a_peer_message_without_a_body_reads_its_envelope() {
        let frame = json!({"type":"user",
            "message":{"role":"user", "content":"<cross-session-message from=\"uds:/run/x.sock\" from-name=\"big\">\nlong body\n</cross-session-message>"},
            "origin":{"kind":"peer", "from":"uds:/run/x.sock"}});
        assert_eq!(
            event(&messages(&frame)[0]),
            ("uds:/run/x.sock", Some("long body"))
        );
    }

    #[test]
    fn each_teammate_envelope_is_a_message() {
        let frame = json!({"type":"user", "message":{"role":"user", "content":[{"type":"text",
            "text":"<teammate-message teammate_id=\"researcher\" color=\"blue\">found it</teammate-message>\n<teammate-message teammate_id=\"tester\">tests pass</teammate-message>"}]}});
        let notes = messages(&frame);
        assert_eq!(
            notes.iter().map(event).collect::<Vec<_>>(),
            [
                ("researcher", Some("found it")),
                ("tester", Some("tests pass"))
            ]
        );
    }

    #[test]
    fn a_typed_prompt_quoting_an_envelope_is_not_a_message() {
        let frame = json!({"type":"user", "origin":{"kind":"human"},
            "message":{"role":"user", "content":"what does <channel source=\"x\">y</channel> mean?"}});
        assert!(messages(&frame).is_empty());
        let plain =
            json!({"type":"user", "message":{"role":"user", "content":"<channels>no</channels>"}});
        assert!(messages(&plain).is_empty());
    }

    #[test]
    fn a_monitor_reports_its_events_and_the_last_output_of_its_stream() {
        let event = monitor_event(
            "<task-notification>\n<task-id>b1</task-id>\n<summary>Monitor event: \"CI &quot;main&quot;\"</summary>\n<event>CI push completed success</event>\nIf this event is something the user would act on now, send a PushNotification.\n</task-notification>",
            "cc",
        )
        .unwrap();
        assert_eq!(event.text(), "event from monitor CI \"main\"");
        assert_eq!(event.delivery_body(), Some("CI push completed success"));
        let ended = monitor_event(
            "<task-notification>\n<task-id>b2</task-id>\n<tool-use-id>toolu_1</tool-use-id>\n<output-file>/tmp/b2.output</output-file>\n<status>completed</status>\n<summary>Monitor \"suite\" stream ended</summary>\n<event>--- exit 0</event>\n</task-notification>",
            "cc",
        )
        .unwrap();
        assert_eq!(
            ended.delivery_event(),
            Some(&DeliveryEvent::MonitorEvent {
                process_id: "cc:b2".into(),
                name: "suite".into()
            })
        );
        assert_eq!(ended.delivery_body(), Some("--- exit 0"));
        let shell = "<task-notification>\n<task-id>b3</task-id>\n<status>completed</status>\n<summary>Background command \"make\" completed (exit code 0)</summary>\n</task-notification>";
        assert!(monitor_event(shell, "cc").is_none());
    }

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
            assert_eq!(note.delivery_body(), Some(expected));
        }
    }
}
