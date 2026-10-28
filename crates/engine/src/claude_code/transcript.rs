use crate::EngineEventSender;
use protocol::{
    displayed_tool_name, AssistantStep, Content, EngineEvent, HistoryItem, InvocationId,
    ReasoningKind, ToolInvocation, ToolOutcome,
};
use serde_json::Value;
use std::collections::{HashMap, HashSet, VecDeque};

struct Step {
    content: Option<Content>,
    reasoning: Option<String>,
    tools: Vec<ToolInvocation>,
    remaining: HashSet<String>,
}

pub(super) struct Transcript {
    pub(super) committed: Vec<HistoryItem>,
    pub(super) context_tokens: Option<u32>,
    pending: VecDeque<Step>,
    calls: HashMap<String, InvocationId>,
    retained_results: HashMap<String, ToolOutcome>,
    started: HashMap<String, u64>,
    claimed: HashSet<String>,
    seen: HashSet<String>,
    partial_text: String,
    partial_reasoning: String,
    reasoning_id: String,
    thinking: HashMap<u64, (String, String)>,
    drafts: HashMap<u64, (String, String, String)>,
    steers: Vec<HistoryItem>,
    tx: EngineEventSender,
    provider: String,
    turn_id: u64,
    next_index: usize,
}

impl Transcript {
    pub(super) fn new(
        tx: EngineEventSender,
        provider: String,
        turn_id: u64,
        next_index: usize,
    ) -> Self {
        Self {
            committed: Vec::new(),
            context_tokens: None,
            pending: VecDeque::new(),
            calls: HashMap::new(),
            retained_results: HashMap::new(),
            started: HashMap::new(),
            claimed: HashSet::new(),
            seen: HashSet::new(),
            partial_text: String::new(),
            partial_reasoning: String::new(),
            reasoning_id: String::new(),
            thinking: HashMap::new(),
            drafts: HashMap::new(),
            steers: Vec::new(),
            tx,
            provider,
            turn_id,
            next_index,
        }
    }

    pub(super) fn route(&mut self, tx: EngineEventSender) {
        self.tx = tx;
    }

    /// Continues on another turn, numbering history from `next_index`.
    pub(super) fn retarget(&mut self, tx: EngineEventSender, turn_id: u64, next_index: usize) {
        self.tx = tx;
        self.turn_id = turn_id;
        self.next_index = next_index;
    }

    /// The provider a tool call ran on, when it is one of its native tools.
    fn native_provider(&self, native: bool) -> Option<&str> {
        native.then_some(self.provider.as_str())
    }

    pub(super) fn emit(&self, event: EngineEvent) {
        let _ = self.tx.send(event);
    }

    pub(super) fn invocation(&mut self, id: &str) -> InvocationId {
        *self
            .calls
            .entry(id.to_owned())
            .or_insert_with(crate::agent::next_invocation_id)
    }

    /// Shows lines a running tool printed in its block.
    pub(super) fn printed(&self, id: &str, lines: Vec<String>) {
        let Some(invocation_id) = self.calls.get(id).copied() else {
            return;
        };
        if !self.pending.iter().any(|step| step.remaining.contains(id)) {
            return;
        }
        for line in lines {
            self.emit(EngineEvent::ToolOutput {
                invocation_id,
                call_id: id.into(),
                line,
            });
        }
    }

    pub(super) fn retain_result(&mut self, id: &str, result: ToolOutcome) {
        self.retained_results.insert(id.to_owned(), result);
    }

    pub(super) fn matching_calls<'a>(
        &'a self,
        name: &'a str,
        args: &'a HashMap<String, Value>,
    ) -> impl Iterator<Item = &'a str> {
        self.pending
            .iter()
            .flat_map(|step| &step.tools)
            .filter(move |tool| {
                tool.name == name
                    && serde_json::from_str::<HashMap<String, Value>>(&tool.arguments)
                        .is_ok_and(|input| &input == args)
                    && !self.claimed.contains(&tool.call_id)
            })
            .map(|tool| tool.call_id.as_str())
    }

    pub(super) fn claim_call(&mut self, id: String) -> (String, InvocationId) {
        self.claimed.insert(id.clone());
        let invocation = self.invocation(&id);
        (id, invocation)
    }

    pub(super) fn append(&mut self, item: HistoryItem) {
        self.emit(EngineEvent::HistoryAppended {
            turn_id: self.turn_id,
            delta: protocol::CanonicalHistoryDelta::new(self.next_index, vec![item.clone()]),
        });
        self.next_index += 1;
        self.committed.push(item);
    }

    /// Whether every tool call has its result.
    pub(super) fn settled(&self) -> bool {
        self.pending.is_empty()
    }

    /// Records a steer Claude Code took. It joins the conversation after the
    /// steps still waiting for their tool results, which it accompanies.
    pub(super) fn steer(&mut self, item: HistoryItem) {
        self.steers.push(item);
        self.flush();
    }

    /// Project a frame after the native adapter has selected its owning transcript.
    pub(super) fn frame(&mut self, frame: &Value, now_ms: u64) {
        match frame["type"].as_str() {
            Some("stream_event") => self.stream(&frame["event"]),
            Some("assistant") => self.assistant(frame, now_ms),
            Some("user") => self.results(frame, now_ms),
            _ => {}
        }
    }

    fn stream(&mut self, event: &Value) {
        let index = event["index"].as_u64().unwrap_or(0);
        match event["type"].as_str() {
            Some("message_start") => {
                self.partial_text.clear();
                self.partial_reasoning.clear();
                self.reasoning_id = event["message"]["id"]
                    .as_str()
                    .unwrap_or("claude-thinking")
                    .to_owned();
            }
            Some("content_block_start") => {
                let block = &event["content_block"];
                match block["type"].as_str() {
                    Some("tool_use") => {
                        let id = block["id"].as_str().unwrap_or("").to_owned();
                        let (name, _, native) =
                            presented(block["name"].as_str().unwrap_or(""), HashMap::new());
                        let name = displayed_tool_name(self.native_provider(native), &name);
                        self.emit(EngineEvent::ToolCallDraftStarted {
                            stream_id: id.clone(),
                            call_id: Some(id.clone()),
                            tool_name: Some(name.clone()),
                        });
                        self.drafts.insert(index, (id, name, String::new()));
                    }
                    Some("thinking") => {
                        let id = format!("{}:{index}", self.reasoning_id);
                        self.thinking.insert(index, (id.clone(), String::new()));
                        self.emit(EngineEvent::ReasoningPartStarted {
                            id,
                            kind: ReasoningKind::Raw,
                        });
                    }
                    _ => {}
                }
            }
            Some("content_block_delta") => {
                let delta = &event["delta"];
                match delta["type"].as_str() {
                    Some("text_delta") => {
                        let text = delta["text"].as_str().unwrap_or("");
                        self.partial_text.push_str(text);
                        self.emit(EngineEvent::TextDelta {
                            delta: text.to_owned(),
                        });
                    }
                    Some("thinking_delta") => {
                        let text = delta["thinking"].as_str().unwrap_or("");
                        self.partial_reasoning.push_str(text);
                        if let Some((id, content)) = self.thinking.get_mut(&index) {
                            content.push_str(text);
                            let id = id.clone();
                            self.emit(EngineEvent::ReasoningPartDelta {
                                id,
                                kind: ReasoningKind::Raw,
                                delta: text.to_owned(),
                                title: None,
                            });
                        }
                    }
                    Some("input_json_delta") => {
                        if let Some((id, name, args)) = self.drafts.get_mut(&index) {
                            let delta = delta["partial_json"].as_str().unwrap_or("");
                            args.push_str(delta);
                            let event = EngineEvent::ToolCallDraftDelta {
                                stream_id: id.clone(),
                                call_id: Some(id.clone()),
                                tool_name: Some(name.clone()),
                                delta: delta.to_owned(),
                            };
                            self.emit(event);
                        }
                    }
                    _ => {}
                }
            }
            Some("content_block_stop") => {
                if let Some((id, content)) = self.thinking.remove(&index) {
                    self.emit(EngineEvent::ReasoningPartFinished {
                        id,
                        kind: ReasoningKind::Raw,
                        title: None,
                        content,
                    });
                }
                if let Some((id, name, args)) = self.drafts.remove(&index) {
                    self.emit(EngineEvent::ToolCallDraftFinished {
                        stream_id: id.clone(),
                        call_id: id,
                        tool_name: name,
                        arguments: if args.is_empty() { "{}".into() } else { args },
                    });
                }
            }
            _ => {}
        }
    }

    fn assistant(&mut self, frame: &Value, now_ms: u64) {
        if let Some(uuid) = frame["uuid"].as_str() {
            if !self.seen.insert(uuid.to_owned()) {
                return;
            }
        }
        let Some(blocks) = frame["message"]["content"].as_array() else {
            return;
        };
        let usage = &frame["message"]["usage"];
        if usage["input_tokens"].is_u64() {
            let total = [
                "input_tokens",
                "output_tokens",
                "cache_read_input_tokens",
                "cache_creation_input_tokens",
            ]
            .iter()
            .fold(0u64, |sum, key| {
                sum.saturating_add(usage[*key].as_u64().unwrap_or(0))
            });
            self.context_tokens = Some(total.min(u32::MAX as u64) as u32);
        }
        let mut text = String::new();
        let mut reasoning = String::new();
        let mut tools = Vec::new();
        let mut remaining = HashSet::new();
        for block in blocks {
            match block["type"].as_str() {
                Some("text") => text.push_str(block["text"].as_str().unwrap_or("")),
                Some("thinking") => reasoning.push_str(block["thinking"].as_str().unwrap_or("")),
                Some("tool_use") => {
                    let Some(id) = block["id"].as_str() else {
                        continue;
                    };
                    let raw_name = block["name"].as_str().unwrap_or("unknown");
                    if self.started.insert(id.to_owned(), now_ms).is_some() {
                        continue;
                    }
                    // Claude Code sends the complete message before the block's
                    // stream stop; the started tool supersedes its draft.
                    self.drafts.retain(|_, (draft, _, _)| draft != id);
                    let invocation_id = self.invocation(id);
                    let input = block["input"]
                        .as_object()
                        .cloned()
                        .unwrap_or_default()
                        .into_iter()
                        .collect();
                    let (name, args, native) = presented(raw_name, input);
                    let provider = self.native_provider(native).map(str::to_owned);
                    let arguments = serde_json::to_string(&args).unwrap_or_default();
                    self.emit(EngineEvent::ToolStarted {
                        invocation_id,
                        call_id: id.into(),
                        tool_name: displayed_tool_name(provider.as_deref(), &name),
                        args,
                        called_at_ms: now_ms,
                    });
                    remaining.insert(id.to_owned());
                    tools.push(ToolInvocation {
                        call_id: id.into(),
                        name,
                        arguments,
                        result: ToolOutcome::new(
                            "Claude Code did not return a tool result".into(),
                            true,
                            None,
                        ),
                        elapsed_ms: None,
                        called_at_ms: Some(now_ms),
                        provider,
                    });
                }
                _ => {}
            }
        }
        if !text.is_empty() {
            if let Some(rest) = self.partial_text.strip_prefix(&text) {
                self.partial_text = rest.to_owned();
            } else {
                self.emit(EngineEvent::TextDelta {
                    delta: text.clone(),
                });
            }
        }
        if !reasoning.is_empty() {
            if let Some(rest) = self.partial_reasoning.strip_prefix(&reasoning) {
                self.partial_reasoning = rest.to_owned();
            } else {
                self.emit(EngineEvent::Reasoning {
                    kind: ReasoningKind::Raw,
                    title: None,
                    content: reasoning.clone(),
                });
            }
        }
        self.emit(EngineEvent::ResponseDraftAccepted);
        if text.is_empty() && reasoning.is_empty() && tools.is_empty() {
            return;
        }
        self.pending.push_back(Step {
            content: (!text.is_empty()).then(|| Content::text(text)),
            reasoning: (!reasoning.is_empty()).then_some(reasoning),
            tools,
            remaining,
        });
        self.flush();
    }

    fn results(&mut self, frame: &Value, now_ms: u64) {
        let Some(blocks) = frame["message"]["content"].as_array() else {
            return;
        };
        // `tool_use_result` describes the frame's only result.
        let structured = (blocks.len() == 1)
            .then(|| &frame["tool_use_result"])
            .filter(|result| result.is_object());
        for block in blocks {
            if block["type"].as_str() != Some("tool_result") {
                continue;
            }
            let Some(id) = block["tool_use_id"].as_str() else {
                continue;
            };
            let Some(invocation_id) = self.calls.get(id).copied() else {
                continue;
            };
            let is_error = block["is_error"].as_bool().unwrap_or(false);
            let mut content = text_content(&block["content"]);
            if is_error {
                content = strip_tool_use_error(&content);
            }
            let result = self.retained_results.remove(id).unwrap_or_else(|| {
                structured
                    .filter(|_| !is_error)
                    .and_then(|result| edit_result(&content, result))
                    .unwrap_or_else(|| ToolOutcome::new(content, is_error, None))
            });
            let elapsed_ms = self.started.get(id).map(|at| now_ms.saturating_sub(*at));
            for step in &mut self.pending {
                if step.remaining.remove(id) {
                    if let Some(tool) = step.tools.iter_mut().find(|tool| tool.call_id == id) {
                        tool.result = result.clone();
                        tool.elapsed_ms = elapsed_ms;
                    }
                    let _ = self.tx.send(EngineEvent::ToolFinished {
                        invocation_id,
                        call_id: id.into(),
                        result,
                        elapsed_ms,
                    });
                    break;
                }
            }
        }
        self.flush();
    }

    fn flush(&mut self) {
        while self
            .pending
            .front()
            .is_some_and(|step| step.remaining.is_empty())
        {
            let step = self.pending.pop_front().expect("checked front");
            self.append(HistoryItem::Assistant(AssistantStep::with_invocations(
                step.content,
                step.reasoning,
                Vec::new(),
                step.tools,
            )));
        }
        if self.pending.is_empty() && !self.steers.is_empty() {
            let items = std::mem::take(&mut self.steers);
            self.emit(EngineEvent::Steered {
                turn_id: self.turn_id,
                delta: protocol::CanonicalHistoryDelta::new(self.next_index, items.clone()),
            });
            self.next_index += items.len();
            self.committed.extend(items);
        }
    }

    /// Forgets what the turn had not finished, without recording it.
    pub(super) fn abandon(&mut self) {
        self.pending.clear();
        self.thinking.clear();
        self.drafts.clear();
        self.steers.clear();
        self.partial_text.clear();
        self.partial_reasoning.clear();
    }

    pub(super) fn finish(&mut self) {
        for (_, (id, content)) in std::mem::take(&mut self.thinking) {
            self.emit(EngineEvent::ReasoningPartFinished {
                id,
                kind: ReasoningKind::Raw,
                title: None,
                content,
            });
        }
        for step in &mut self.pending {
            for id in step.remaining.drain() {
                if let Some(invocation_id) = self.calls.get(&id) {
                    let result = self.retained_results.remove(&id).unwrap_or_else(|| {
                        ToolOutcome::new(
                            "Claude Code turn ended before this tool returned".into(),
                            true,
                            None,
                        )
                    });
                    if let Some(tool) = step.tools.iter_mut().find(|tool| tool.call_id == id) {
                        tool.result = result.clone();
                    }
                    let _ = self.tx.send(EngineEvent::ToolFinished {
                        invocation_id: *invocation_id,
                        call_id: id,
                        result,
                        elapsed_ms: None,
                    });
                }
            }
        }
        if !self.partial_text.is_empty() || !self.partial_reasoning.is_empty() {
            let text = std::mem::take(&mut self.partial_text);
            let reasoning = std::mem::take(&mut self.partial_reasoning);
            self.emit(EngineEvent::ResponseDraftAccepted);
            self.pending.push_back(Step {
                content: (!text.is_empty()).then(|| Content::text(text)),
                reasoning: (!reasoning.is_empty()).then_some(reasoning),
                tools: Vec::new(),
                remaining: HashSet::new(),
            });
        }
        self.flush();
    }
}

/// Claude Code's structured Edit result in the shape of smelt's `edit_file`:
/// the file before and after the edit, rendered as a full-file diff.
fn edit_result(content: &str, result: &Value) -> Option<ToolOutcome> {
    let original = result["originalFile"].as_str()?;
    let hunks = result["structuredPatch"]
        .as_array()
        .filter(|hunks| !hunks.is_empty())?;
    let edited = apply_patch(original, hunks)?;
    ToolOutcome::try_from_parts(
        content.into(),
        false,
        Some(serde_json::json!({"path": result["filePath"]})),
        vec![
            protocol::ToolDisplayContent::new("old_content", original.into()),
            protocol::ToolDisplayContent::new("new_content", edited),
        ],
        None,
    )
    .ok()
}

/// Applies the hunks Claude Code reports for an edit to the original file.
/// They describe what it actually changed, which can differ from the call's
/// `old_string` when its matching normalized quotes. Every context and
/// removed line must match, so a patch that does not fit yields nothing.
pub(super) fn apply_patch(original: &str, hunks: &[Value]) -> Option<String> {
    let lines: Vec<&str> = original.split_inclusive('\n').collect();
    let ending = if original.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let mut edited = String::with_capacity(original.len());
    let mut next = 0;
    for hunk in hunks {
        let start = usize::try_from(hunk["oldStart"].as_u64()?).ok()?;
        let at = if hunk["oldLines"].as_u64()? == 0 {
            start
        } else {
            start.checked_sub(1)?
        };
        edited.extend(lines.get(next..at)?.iter().copied());
        next = at;
        let mut added = false;
        for line in hunk["lines"].as_array()? {
            let line = line.as_str()?;
            if let Some(text) = line.strip_prefix('+') {
                edited.push_str(text);
                edited.push_str(ending);
                added = true;
            } else if line.starts_with('\\') {
                // "\ No newline at end of file" after an added last line.
                if let Some(kept) = edited.strip_suffix(ending).filter(|_| added) {
                    edited.truncate(kept.len());
                }
            } else {
                let text = line.strip_prefix(' ').or_else(|| line.strip_prefix('-'))?;
                let current = *lines.get(next)?;
                if current.trim_end_matches(['\r', '\n']) != text.trim_end_matches('\r') {
                    return None;
                }
                if line.starts_with(' ') {
                    edited.push_str(current);
                }
                next += 1;
                added = false;
            }
        }
    }
    edited.extend(lines.get(next..)?.iter().copied());
    Some(edited)
}

/// How a native tool call is presented: smelt's own tools under their smelt
/// name, native tools as the smelt tool sharing their capability, and the
/// rest under their native name, flagged as Claude Code's own.
pub(super) fn presented(
    raw_name: &str,
    input: HashMap<String, Value>,
) -> (String, HashMap<String, Value>, bool) {
    if let Some(name) = raw_name.strip_prefix("mcp__smelt__") {
        return (name.to_owned(), input, false);
    }
    match super::catalog::smelt_tool(raw_name, &input) {
        Some((name, args)) => (name.to_owned(), args, false),
        None => (raw_name.to_owned(), input, true),
    }
}

pub(super) fn text_content(value: &Value) -> String {
    if let Some(text) = value.as_str() {
        return text.to_owned();
    }
    value
        .as_array()
        .map(|blocks| {
            blocks
                .iter()
                .filter_map(|b| b["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

/// Claude Code wraps its own tool failures in `<tool_use_error>` tags; the
/// error flag already carries that meaning, so show only the message.
fn strip_tool_use_error(content: &str) -> String {
    content
        .trim()
        .strip_prefix("<tool_use_error>")
        .and_then(|rest| rest.strip_suffix("</tool_use_error>"))
        .unwrap_or(content)
        .trim()
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::strip_tool_use_error;

    #[test]
    fn strips_wrapping_tool_use_error_tags() {
        assert_eq!(
            strip_tool_use_error("<tool_use_error>Blocked: sleep 150</tool_use_error>\n"),
            "Blocked: sleep 150"
        );
    }

    #[test]
    fn leaves_other_error_text_untouched() {
        assert_eq!(strip_tool_use_error("exit code 1"), "exit code 1");
        assert_eq!(
            strip_tool_use_error("<tool_use_error>unclosed"),
            "<tool_use_error>unclosed"
        );
    }
}
