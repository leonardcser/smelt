import { contentText, copyButton, escapeHtml, fmtCost, fmtMs, fmtNumber, fmtTime, icon, kv, parseMaybeJson, statusBadge } from "./format.js";
import { highlightedJson } from "./json_view.js";
import { markdownBlock } from "./markdown.js";
import { reasoningBlock, reasoningDetail, renderToolCall } from "./render_conversation.js";

const searchText = new WeakMap();

export function renderRequests(requests) {
  if (!requests?.length) return `<div class="empty">No provider requests logged for this session.</div>`;
  const filtered = filterRequests(requests);
  return `
    <div class="request-tools">
      <label class="search-field request-search-field">${icon("search")}<input type="search" id="request-search" placeholder="Search requests, models, messages…" value="${escapeHtml(window.inspectorRequestQuery || "")}"></label>
      <select id="request-status">
        ${option("all", "All", window.inspectorRequestStatus)}
        ${option("errors", "Errors", window.inspectorRequestStatus)}
        ${option("ok", "OK", window.inspectorRequestStatus)}
        ${option("raw", "Raw", window.inspectorRequestStatus)}
      </select>
      <span class="muted">${fmtNumber(filtered.length)} / ${fmtNumber(requests.length)} attempts</span>
    </div>
    <div class="timeline">${filtered.map(({ entry, index }) => renderRequest(entry, index)).join("") || `<div class="empty">No requests match the filter.</div>`}</div>
  `;
}

export function renderRequestPanel(type, entry, index) {
  if (!entry) return `<div class="empty">Request no longer available.</div>`;
  if (entry.payload_error && type !== "summary") return `<div class="empty">Failed to load request payload: ${escapeHtml(entry.payload_error)}</div>`;
  if (type === "messages") return renderMessages(entry);
  if (type === "response") return renderResponse(entry, index);
  if (type === "error") return renderError(entry.error);
  if (type === "raw") return renderRaw(entry);
  return renderSummary(entry);
}

export function renderResponseRaw(entry) {
  return entry?.response?.raw ? highlightedJson(entry.response.raw) : `<div class="empty">No raw response JSON.</div>`;
}

function option(value, label, selected) {
  return `<option value="${value}" ${value === (selected || "all") ? "selected" : ""}>${label}</option>`;
}

function filterRequests(requests) {
  const q = String(window.inspectorRequestQuery || "").toLowerCase();
  const status = window.inspectorRequestStatus || "all";
  return requests.map((entry, index) => ({ entry, index })).filter(({ entry }) => {
    if (status === "errors" && !entry.error) return false;
    if (status === "ok" && entry.error) return false;
    if (status === "raw" && !(entry.response?.raw || entry.has_raw_response)) return false;
    if (!q) return true;
    return requestSearchText(entry).includes(q);
  });
}

function requestSearchText(entry) {
  const cached = searchText.get(entry);
  if (cached) return cached;
  const messages = (entry.messages || []).map((message) => [message.role, contentText(message.content), message.reasoning_content].join(" "));
  const text = [
    entry.request_id,
    entry.kind,
    entry.provider_kind,
    entry.model,
    entry.url,
    entry.http_status,
    entry.error?.kind,
    entry.error?.message,
    entry.error_summary,
    entry.response?.content,
    entry.response?.reasoning,
    entry.response_summary,
    entry.raw_body_size,
    ...messages,
  ].filter(Boolean).join(" ").toLowerCase();
  searchText.set(entry, text);
  return text;
}

function renderRequest(entry, index) {
  const tabs = ["summary", "messages", "response"];
  if (entry.error) tabs.push("error");
  tabs.push("raw");
  const id = `req-${index}`;
  return `<article class="request">
    <header>
      <div>
        ${statusBadge(entry)} <span class="badge">${escapeHtml(entry.kind || "request")}</span> <span class="badge">attempt ${escapeHtml(entry.attempt ?? 0)}</span>
        ${entry.stream ? `<span class="badge cache">stream</span>` : ""}
        ${entry.prompt_cache_key ? `<span class="badge cache">cache key</span>` : ""}
      </div>
      <div class="muted mono">${escapeHtml(entry.provider_kind || "provider")}/${escapeHtml(entry.model || "model")} · ${escapeHtml(fmtTime(entry.timestamp_ms))}</div>
    </header>
    <div class="tabs">${tabs.map((type, i) => `<button class="tab ${i === 0 ? "active" : ""}" data-request-tab="${id}-${type}" data-request-panel="${type}" data-request-index="${index}">${icon(tabIcon(type))}${escapeHtml(tabLabel(type))}</button>`).join("")}</div>
    ${tabs.map((type, i) => `<section id="${id}-${type}" class="request-body" data-panel-type="${type}" data-request-index="${index}" ${i === 0 ? "data-rendered=\"true\"" : "hidden"}>${i === 0 ? renderSummary(entry) : `<div class="empty">Open ${escapeHtml(tabLabel(type))} to render.</div>`}</section>`).join("")}
  </article>`;
}

function tabIcon(type) {
  if (type === "messages") return "comment-discussion";
  if (type === "response") return "pulse";
  if (type === "error") return "error";
  if (type === "raw") return "json";
  return "dashboard";
}

function tabLabel(type) {
  return type[0].toUpperCase() + type.slice(1);
}

function renderSummary(entry) {
  const usage = entry.usage || {};
  return `<div class="split">
    <div>${kv([
      ["URL", `${escapeHtml(entry.url || "—")} ${copyButton("copy", entry.url || "")}`],
      ["HTTP", escapeHtml(entry.http_status || entry.error?.status || "—")],
      ["Elapsed", escapeHtml(fmtMs(entry.elapsed_ms))],
      ["Cost", escapeHtml(fmtCost(entry.cost_usd))],
      ["Tokens/sec", escapeHtml(entry.tokens_per_sec ? Number(entry.tokens_per_sec).toFixed(1) : "—")],
      ["Request ID", escapeHtml(entry.request_id)],
    ])}</div>
    <div>${kv([
      ["Prompt", escapeHtml(fmtNumber(usage.prompt_tokens))],
      ["Completion", escapeHtml(fmtNumber(usage.completion_tokens))],
      ["Reasoning", escapeHtml(fmtNumber(usage.reasoning_tokens))],
      ["Cache read", escapeHtml(fmtNumber(usage.cache_read_tokens))],
      ["Cache write", escapeHtml(fmtNumber(usage.cache_write_tokens))],
      ["Context", escapeHtml(fmtNumber(usage.context_tokens))],
    ])}</div>
  </div>`;
}

function renderMessages(entry) {
  const system = entry.system_prompt ? `<article class="turn"><header><span class="tag system">system prompt</span></header><div class="turn-body">${markdownBlock(entry.system_prompt)}</div></article>` : "";
  const messages = (entry.messages || []).map((message, i) => `<article class="turn">
    <header><span class="tag ${escapeHtml(message.role || "message")}">${escapeHtml(message.role || "message")}</span><span class="muted mono">#${i + 1}</span></header>
    <div class="turn-body">
      ${reasoningBlock(message.reasoning_content, "Reasoning")}
      ${(message.reasoning_details || []).map(reasoningDetail).join("")}
      ${markdownBlock(contentText(message.content), `<div class="muted">empty</div>`)}
      ${(message.tool_calls || []).map(renderToolCall).join("")}
      ${message.tool_call_id ? `<div class="muted mono">tool_call_id ${escapeHtml(message.tool_call_id)}</div>` : ""}
    </div>
  </article>`).join("");
  const fallback = entry.body
    ? `<details class="tool-call"><summary>Raw request body</summary><div class="tool-inner">${highlightedJson(entry.body)}</div></details>`
    : payloadNotStored(entry, "body")
      ? requestPayloadNotice("Provider messages were not stored for this attempt.")
      : `<div class="empty">No provider messages captured.</div>`;
  return `<div class="timeline">${system}${messages || fallback}</div>`;
}

function renderRaw(entry) {
  const notice = payloadNotStored(entry) ? requestPayloadNotice("Full request/response payloads were not stored for this attempt.") : "";
  return `${notice}${highlightedJson(entry)}`;
}

function payloadNotStored(entry, part = "any") {
  if (!entry?.payload_loaded) return false;
  if (part === "body") return entry.raw_body_size > 0 && !entry.has_body && !entry.body;
  if (part === "response") return Boolean(entry.response && !entry.has_response);
  return Boolean((entry.raw_body_size > 0 && !entry.has_body && !entry.body)
    || (entry.response && !entry.has_response)
    || (entry.error && !entry.has_error));
}

function requestPayloadNotice(message) {
  return `<div class="empty">${escapeHtml(message)} Set <code>smelt.settings.request_audit = "full"</code> or <code>SMELT_REQUEST_AUDIT=full</code> to capture full payloads for future sessions.</div>`;
}

function renderResponse(entry, index) {
  const response = entry.response;
  if (!response) return `<div class="empty">No parsed response captured.</div>`;
  const notice = payloadNotStored(entry, "response") ? requestPayloadNotice("Only the parsed response summary was stored for this attempt.") : "";
  return `
    ${notice}
    ${reasoningBlock(response.reasoning, "Response reasoning")}
    ${markdownBlock(response.content, `<div class="muted">No response content.</div>`)}
    ${(response.tool_calls || []).map(renderToolCall).join("")}
    ${response.raw ? `<details class="tool-call" data-response-raw-index="${index}"><summary>Raw response JSON</summary><div class="tool-inner"><div class="empty">Open to render raw response.</div></div></details>` : ""}
  `;
}

function renderError(error) {
  const body = error?.body ? parseMaybeJson(error.body) : null;
  return `<div class="error-box">${escapeHtml(error?.kind || "error")}: ${escapeHtml(error?.message || "")}</div>
    ${body ? (typeof body === "string" ? `<pre><code>${escapeHtml(body)}</code></pre>` : highlightedJson(body)) : ""}`;
}
