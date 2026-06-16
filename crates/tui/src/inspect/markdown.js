import { escapeHtml } from "./format.js";

const browser = globalThis.window ?? globalThis;

if (browser.marked?.setOptions) {
  browser.marked.setOptions({ gfm: true, breaks: false, mangle: false, headerIds: false });
}

export function markdown(text) {
  const source = String(text ?? "");
  if (!source.trim()) return "";
  if (!browser.marked || !browser.DOMPurify) {
    return `<div class="markdown text">${escapeHtml(source)}</div>`;
  }
  const raw = browser.marked.parse(source);
  const safe = browser.DOMPurify.sanitize(raw, {
    USE_PROFILES: { html: true },
    ADD_ATTR: ["target", "rel"],
  });
  return `<div class="markdown">${safe}</div>`;
}

export function markdownBlock(text, fallback = "") {
  const source = String(text ?? "");
  if (!source.trim()) return fallback;
  return markdown(source);
}
