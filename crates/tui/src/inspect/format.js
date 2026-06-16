export function escapeHtml(value) {
  return String(value ?? "")
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;")
    .replaceAll('"', "&quot;")
    .replaceAll("'", "&#39;");
}

export function attr(value) {
  return escapeHtml(value);
}

export function icon(name) {
  return `<span class="icon" style="--icon:url('/assets/icons/${attr(name)}.svg')" aria-hidden="true"></span>`;
}

export function coalesce(...values) {
  return values.find((value) => value !== undefined && value !== null && value !== "") ?? null;
}

export function fmtNumber(value) {
  if (value === null || value === undefined || Number.isNaN(Number(value))) return "—";
  return Number(value).toLocaleString();
}

export function fmtBytes(value) {
  if (value === null || value === undefined || Number.isNaN(Number(value))) return "—";
  const bytes = Number(value);
  if (bytes < 1024) return `${bytes} B`;
  const units = ["KiB", "MiB", "GiB"];
  let n = bytes / 1024;
  for (const unit of units) {
    if (n < 1024) return `${n >= 10 ? n.toFixed(0) : n.toFixed(1)} ${unit}`;
    n /= 1024;
  }
  return `${n.toFixed(1)} TiB`;
}

export function fmtCost(value) {
  if (!value) return "$0.00";
  if (value < 0.01) return `$${Number(value).toFixed(4)}`;
  return `$${Number(value).toFixed(2)}`;
}

export function fmtMs(value) {
  if (value === null || value === undefined) return "—";
  const ms = Number(value);
  if (ms < 1000) return `${Math.round(ms)} ms`;
  if (ms < 60_000) return `${(ms / 1000).toFixed(1)} s`;
  return `${Math.floor(ms / 60_000)}m ${Math.round((ms % 60_000) / 1000)}s`;
}

export function fmtTime(value) {
  if (!value) return "—";
  return new Date(Number(value)).toLocaleString();
}

export function relativeTime(value) {
  if (!value) return "—";
  const delta = Date.now() - Number(value);
  const abs = Math.abs(delta);
  const suffix = delta >= 0 ? "ago" : "from now";
  if (abs < 60_000) return "just now";
  if (abs < 3_600_000) return `${Math.round(abs / 60_000)}m ${suffix}`;
  if (abs < 86_400_000) return `${Math.round(abs / 3_600_000)}h ${suffix}`;
  return `${Math.round(abs / 86_400_000)}d ${suffix}`;
}

export function modeBadge(mode) {
  const name = String(mode || "normal").toLowerCase();
  const cls = ["normal", "plan", "apply", "yolo"].includes(name) ? name : "normal";
  const iconName = cls === "apply" ? "arrow-right" : cls === "yolo" ? "zap" : "circle-outline";
  return `<span class="badge mode-${cls}">${icon(iconName)}${escapeHtml(name)}</span>`;
}

export function statusBadge(entry) {
  if (entry?.error) return `<span class="badge err">${icon("error")}error</span>`;
  const status = entry?.http_status ?? entry?.error?.status;
  if (status) {
    const cls = status >= 200 && status < 300 ? "ok" : "err";
    const iconName = cls === "ok" ? "check" : "error";
    return `<span class="badge ${cls}">${icon(iconName)}HTTP ${escapeHtml(status)}</span>`;
  }
  return `<span class="badge ok">${icon("check")}ok</span>`;
}

export function roleTag(role, extra = "") {
  const value = String(role || "note").toLowerCase();
  const iconName = value === "user" ? "account" : value === "assistant" ? "hubot" : value === "tool" ? "tools" : value === "system" ? "gear" : "note";
  return `<span class="tag ${escapeHtml(value)} ${escapeHtml(extra)}">${icon(iconName)}${escapeHtml(value)}</span>`;
}

export function contentText(content) {
  if (content === null || content === undefined) return "";
  if (typeof content === "string") return content;
  if (Array.isArray(content)) {
    return content.map((part) => {
      if (typeof part === "string") return part;
      if (part?.type === "text") return part.text || "";
      if (part?.type === "image_url") return `[${part.label || "image"}]`;
      return JSON.stringify(part);
    }).filter(Boolean).join("\n");
  }
  if (typeof content === "object") {
    if (typeof content.text === "string") return content.text;
    if (typeof content.content === "string") return content.content;
  }
  return JSON.stringify(content, null, 2);
}

export function shortText(value, max = 120) {
  const text = String(value || "").replace(/\s+/g, " ").trim();
  if (text.length <= max) return text;
  return `${text.slice(0, max - 1)}…`;
}

export function stat(label, value, hint = "") {
  return `<div class="stat"><div class="label">${escapeHtml(label)}</div><div class="value">${escapeHtml(value)}</div>${hint ? `<div class="hint">${escapeHtml(hint)}</div>` : ""}</div>`;
}

export function kv(rows) {
  const body = rows
    .filter(([, value]) => value !== undefined && value !== null && value !== "")
    .map(([key, value]) => `<dt>${escapeHtml(key)}</dt><dd>${value}</dd>`)
    .join("");
  return `<dl class="kv">${body || `<dt>empty</dt><dd class="muted">No data</dd>`}</dl>`;
}

export function prettyJson(value) {
  if (typeof value === "string") {
    try { return JSON.stringify(JSON.parse(value), null, 2); } catch { return value; }
  }
  return JSON.stringify(value ?? null, null, 2);
}

export function copyButton(label, value) {
  return `<button data-copy="${attr(String(value ?? ""))}">${icon("copy")}${escapeHtml(label)}</button>`;
}

export function parseMaybeJson(value) {
  if (typeof value !== "string") return value;
  try { return JSON.parse(value); } catch { return value; }
}
