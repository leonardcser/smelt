import { contentText, escapeHtml, parseMaybeJson, roleTag, shortText } from "./format.js";
import { highlightedJson, jsonTree } from "./json_view.js";
import { markdown, markdownBlock } from "./markdown.js";

export function renderConversation(session, limit = 100) {
  const history = session?.history || [];
  if (!history.length) return `<div class="empty">No committed conversation history.</div>`;
  const visible = history.slice(0, limit);
  const remaining = history.length - visible.length;
  return `<div class="timeline conversation">
    ${visible.map(renderHistoryItem).join("")}
    ${remaining > 0 ? `<button class="load-more" data-load-conversation>${remaining} more turns — load next 100</button>` : ""}
  </div>`;
}

function renderHistoryItem(item, index) {
  const kind = item.kind || "note";
  if (kind === "assistant") return renderAssistant(item, index);
  if (kind === "user") return renderTextTurn("user", item.display || contentText(item.content), index);
  if (kind === "system") return renderTextTurn("system", contentText(item.content), index);
  if (kind === "note") return renderTextTurn("note", noteText(item), index, item.note_kind || "note");
  return renderTextTurn(kind, JSON.stringify(item, null, 2), index);
}

function renderTextTurn(role, text, index, label = role) {
  return `<article class="turn role ${escapeHtml(role)}">
    <header><div>${roleTag(label)} <span class="turn-index">${turnNumber(index)}</span></div><div class="turn-preview">${escapeHtml(shortText(text, 120))}</div></header>
    <div class="turn-body">${markdownBlock(text, `<div class="muted">empty</div>`)}</div>
  </article>`;
}

function renderAssistant(item, index) {
  const reasoning = item.reasoning || item.reasoning_content;
  const content = contentText(item.content);
  const blocks = item.reasoning_blocks || item.reasoning_details || [];
  const invocations = item.invocations || [];
  const toolCalls = item.tool_calls || [];
  const preview = content || reasoning || invocations.map((tool) => tool.name || "tool").join(", ");
  return `<article class="turn role assistant">
    <header><div>${roleTag("assistant")} <span class="turn-index">${turnNumber(index)}</span></div><div class="turn-preview">${escapeHtml(shortText(preview, 120))}</div></header>
    <div class="turn-body">
      ${reasoningBlock(reasoning, "Thinking")}
      ${blocks.map((block, i) => reasoningDetail(block, i)).join("")}
      ${markdownBlock(content, invocations.length || toolCalls.length ? "" : `<div class="muted">empty assistant message</div>`)}
      ${invocations.map(renderInvocation).join("")}
      ${toolCalls.map(renderToolCall).join("")}
    </div>
  </article>`;
}

export function reasoningBlock(text, title = "Reasoning") {
  if (!String(text || "").trim()) return "";
  return `<section class="thinking"><div class="thinking-title">${escapeHtml(title)}</div><div class="thinking-body">${markdown(text)}</div></section>`;
}

export function reasoningDetail(block, index = 0) {
  const text = extractReasoningText(block);
  const provider = block?.provider ? ` · ${block.provider}` : "";
  if (text) return reasoningBlock(text, `Reasoning block ${index + 1}${provider}`);
  return `<details class="thinking thinking-raw"><summary>${escapeHtml(`Reasoning block ${index + 1}${provider}`)}</summary><div class="thinking-body">${highlightedJson(block)}</div></details>`;
}

function extractReasoningText(value) {
  const data = value?.data ?? value;
  if (typeof data === "string") return data;
  if (!data || typeof data !== "object") return "";
  for (const key of ["text", "thinking", "summary", "content"]) {
    if (typeof data[key] === "string" && data[key].trim()) return data[key];
  }
  if (Array.isArray(data.content)) {
    return data.content.map(extractReasoningText).filter(Boolean).join("\n\n");
  }
  return "";
}

function renderInvocation(invocation) {
  const result = invocation.result || {};
  const cls = result.is_error ? "error" : "success";
  return `<details class="tool-call ${cls}">
    <summary>${escapeHtml(invocation.name || "tool")} <span class="muted mono">${escapeHtml(invocation.call_id || "")}</span></summary>
    <div class="tool-inner">
      <div class="muted">Arguments</div>
      ${renderArgs(invocation.arguments)}
      <div class="muted">Result</div>
      ${markdownBlock(result.content || "", `<div class="muted">empty result</div>`)}
      ${result.metadata ? `<div class="muted">Metadata</div>${highlightedJson(result.metadata)}` : ""}
    </div>
  </details>`;
}

export function renderToolCall(call) {
  const fn = call.function || {};
  return `<details class="tool-call">
    <summary>${escapeHtml(fn.name || "tool call")} <span class="muted mono">${escapeHtml(call.id || "")}</span></summary>
    <div class="tool-inner">${renderArgs(fn.arguments)}</div>
  </details>`;
}

function renderArgs(args) {
  const parsed = parseMaybeJson(args || "{}");
  if (typeof parsed === "string") return `<pre><code>${escapeHtml(parsed)}</code></pre>`;
  return `<div class="mono">${jsonTree(parsed)}</div>`;
}

function turnNumber(index) {
  return String(index + 1).padStart(3, "0");
}

function noteText(item) {
  if (item.text) return item.text;
  if (item.note_kind === "mode_change") return item.mode ? `Mode changed to ${item.mode}` : "Mode changed";
  return JSON.stringify(item, null, 2);
}
