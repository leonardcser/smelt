import { copyButton, escapeHtml, fmtBytes, fmtCost, fmtMs, fmtNumber, fmtTime, kv, modeBadge, stat } from "./format.js";
import { highlightedJson } from "./json_view.js";

export function renderOverview(session, requests) {
  const stats = session?.request_stats || summarizeRequests(requests);
  const usage = session?.session_usage || {};
  const metaRows = [
    ["ID", `${escapeHtml(session.id)} ${copyButton("copy", session.id)}`],
    ["Title", escapeHtml(session.title || session.slug || "—")],
    ["Mode", modeBadge(session.mode)],
    ["Model", escapeHtml(session.model || stats.latest_model || "—")],
    ["Project", escapeHtml(session.project || projectName(session.cwd) || "—")],
    ["Path", escapeHtml(session.cwd || "—")],
    ["Size", escapeHtml(fmtBytes(session.text_bytes))],
    ["Created", escapeHtml(fmtTime(session.created_at_ms))],
    ["Updated", escapeHtml(fmtTime(session.updated_at_ms))],
  ];
  const requestRows = [
    ["Provider", escapeHtml(stats.latest_provider_kind || "—")],
    ["Latest model", escapeHtml(stats.latest_model || "—")],
    ["Requests", escapeHtml(fmtNumber(stats.request_count))],
    ["Errors", escapeHtml(fmtNumber(stats.error_count))],
    ["Streaming", escapeHtml(fmtNumber(stats.streaming_count))],
    ["Raw responses", escapeHtml(fmtNumber(stats.raw_response_count))],
    ["Elapsed", escapeHtml(fmtMs(stats.total_elapsed_ms))],
    ["Cost", escapeHtml(fmtCost(stats.total_cost_usd))],
  ];

  return `
    <section class="card">
      <h3>Overview</h3>
      <div class="body flush"><div class="grid stats">
        ${stat("Requests", fmtNumber(stats.request_count), `${fmtNumber(stats.error_count)} errors`)}
        ${stat("Cost", fmtCost(stats.total_cost_usd), "request log total")}
        ${stat("Elapsed", fmtMs(stats.total_elapsed_ms), "request log total")}
        ${stat("Context", fmtNumber(stats.latest_context_tokens ?? session.display_context_tokens ?? session.context_tokens), `max ${fmtNumber(stats.max_context_tokens)}`)}
        ${stat("Prompt tokens", fmtNumber(stats.total_prompt_tokens || usage.prompt_tokens), `cache read ${fmtNumber(stats.total_cache_read_tokens)}`)}
        ${stat("Completion", fmtNumber(stats.total_completion_tokens || usage.completion_tokens), `reasoning ${fmtNumber(stats.total_reasoning_tokens || usage.reasoning_tokens)}`)}
      </div></div>
    </section>
    <section class="card"><h3>Session</h3><div class="body flush">${kv(metaRows)}</div></section>
    <section class="card"><h3>Requests</h3><div class="body flush">${kv(requestRows)}</div></section>
    ${session.checkpoint ? `<section class="card"><h3>Checkpoint</h3>${highlightedJson(session.checkpoint)}</section>` : ""}
  `;
}

function summarizeRequests(requests) {
  return (requests || []).reduce((stats, entry) => {
    stats.request_count += 1;
    if (entry.error) stats.error_count += 1;
    if (entry.stream) stats.streaming_count += 1;
    if (entry.response?.raw) stats.raw_response_count += 1;
    stats.total_cost_usd += entry.cost_usd || 0;
    stats.total_elapsed_ms += entry.elapsed_ms || 0;
    stats.latest_provider_kind = entry.provider_kind || stats.latest_provider_kind;
    stats.latest_model = entry.model || stats.latest_model;
    if (entry.usage) {
      stats.total_prompt_tokens += entry.usage.prompt_tokens || 0;
      stats.total_completion_tokens += entry.usage.completion_tokens || 0;
      stats.total_cache_read_tokens += entry.usage.cache_read_tokens || 0;
      stats.total_reasoning_tokens += entry.usage.reasoning_tokens || 0;
      stats.latest_context_tokens = entry.usage.context_tokens ?? stats.latest_context_tokens;
      stats.max_context_tokens = Math.max(stats.max_context_tokens || 0, entry.usage.context_tokens || 0) || null;
    }
    return stats;
  }, { request_count: 0, error_count: 0, streaming_count: 0, raw_response_count: 0, total_cost_usd: 0, total_elapsed_ms: 0, total_prompt_tokens: 0, total_completion_tokens: 0, total_cache_read_tokens: 0, total_reasoning_tokens: 0 });
}

function projectName(cwd) {
  if (!cwd) return null;
  return String(cwd).split(/[\\/]/).filter(Boolean).pop();
}
