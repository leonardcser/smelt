//! Messages other agents add to a conversation. Claude Code takes each as a
//! prompt of its own and echoes it, wrapped in an envelope naming the sender.

use protocol::{AgentMessageEvent, HistoryNote};
use serde_json::Value;

/// Envelope tags, with the attributes that name the sender, best first.
const ENVELOPES: &[(&str, &[&str])] = &[
    ("cross-session-message", &["from-name", "from"]),
    ("teammate-message", &["teammate_id", "from", "name"]),
    ("channel", &["source"]),
];

/// The messages in a prompt Claude Code took on its own. A prompt a person
/// typed carries none, even when it quotes an envelope.
pub(super) fn received(frame: &Value) -> Vec<HistoryNote> {
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
        return vec![note(from, &body)];
    }
    envelopes(&text)
        .into_iter()
        .map(|(from, body)| note(from, &body))
        .collect()
}

fn note(from: String, body: &str) -> HistoryNote {
    HistoryNote::agent_message(AgentMessageEvent::MessageReceived { from }, body.trim())
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn event(note: &HistoryNote) -> (&str, &str) {
        let HistoryNote::AgentMessage { event, body, .. } = note else {
            panic!("not an agent message: {note:?}");
        };
        (event.sender(), body)
    }

    #[test]
    fn a_peer_message_names_its_sender_and_body() {
        let frame = json!({"type":"user", "isReplay":true, "isSynthetic":true,
            "message":{"role":"user", "content":"Another Claude session sent a message:\n<cross-session-message from=\"uds:/run/x.sock\" from-name=\"worker\">\nHi there\n</cross-session-message>\n\nThis came from another Claude session."},
            "origin":{"kind":"peer", "from":"uds:/run/x.sock", "name":"worker", "body":"Hi there"}});
        let notes = received(&frame);
        assert_eq!(notes.len(), 1);
        assert_eq!(event(&notes[0]), ("worker", "Hi there"));
        assert_eq!(notes[0].text(), "message from worker");
    }

    #[test]
    fn a_peer_message_without_a_body_reads_its_envelope() {
        let frame = json!({"type":"user",
            "message":{"role":"user", "content":"<cross-session-message from=\"uds:/run/x.sock\" from-name=\"big\">\nlong body\n</cross-session-message>"},
            "origin":{"kind":"peer", "from":"uds:/run/x.sock"}});
        assert_eq!(
            event(&received(&frame)[0]),
            ("uds:/run/x.sock", "long body")
        );
    }

    #[test]
    fn each_teammate_envelope_is_a_message() {
        let frame = json!({"type":"user", "message":{"role":"user", "content":[{"type":"text",
            "text":"<teammate-message teammate_id=\"researcher\" color=\"blue\">found it</teammate-message>\n<teammate-message teammate_id=\"tester\">tests pass</teammate-message>"}]}});
        let notes = received(&frame);
        assert_eq!(
            notes.iter().map(event).collect::<Vec<_>>(),
            [("researcher", "found it"), ("tester", "tests pass")]
        );
    }

    #[test]
    fn a_typed_prompt_quoting_an_envelope_is_not_a_message() {
        let frame = json!({"type":"user", "origin":{"kind":"human"},
            "message":{"role":"user", "content":"what does <channel source=\"x\">y</channel> mean?"}});
        assert!(received(&frame).is_empty());
        let plain =
            json!({"type":"user", "message":{"role":"user", "content":"<channels>no</channels>"}});
        assert!(received(&plain).is_empty());
    }
}
