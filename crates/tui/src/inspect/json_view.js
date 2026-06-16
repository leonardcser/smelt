import { createHighlighterCore } from "./vendor/shiki-core.mjs";
import jsonLang from "./vendor/shiki-json.mjs";
import githubDarkDefault from "./vendor/shiki-github-dark-default.mjs";
import { createJavaScriptRegexEngine } from "./vendor/shiki-engine-javascript.mjs";
import { escapeHtml, prettyJson } from "./format.js";

const highlighterPromise = createHighlighterCore({
  themes: [githubDarkDefault],
  langs: [jsonLang],
  engine: createJavaScriptRegexEngine(),
});

export function jsonTree(value, depth = 0) {
  if (value === null) return `<span class="j-null">null</span>`;
  if (typeof value === "string") return `<span class="j-str">${escapeHtml(JSON.stringify(value))}</span>`;
  if (typeof value === "number") return `<span class="j-num">${escapeHtml(value)}</span>`;
  if (typeof value === "boolean") return `<span class="j-bool">${escapeHtml(value)}</span>`;
  if (Array.isArray(value)) {
    if (value.length === 0) return "[]";
    return `<details class="json-node" ${depth < 1 ? "open" : ""}><summary>[${value.length}]</summary><div class="json-children">${value.map((item, i) => `<div><span class="j-key">${i}</span>: ${jsonTree(item, depth + 1)}</div>`).join("")}</div></details>`;
  }
  if (typeof value === "object") {
    const entries = Object.entries(value);
    if (entries.length === 0) return "{}";
    return `<details class="json-node" ${depth < 1 ? "open" : ""}><summary>{${entries.length}}</summary><div class="json-children">${entries.map(([key, item]) => `<div><span class="j-key">${escapeHtml(key)}</span>: ${jsonTree(item, depth + 1)}</div>`).join("")}</div></details>`;
  }
  return escapeHtml(String(value));
}

export function highlightedJson(value) {
  return `<pre data-json-highlight><code>${escapeHtml(prettyJson(value))}</code></pre>`;
}

export async function highlightJsonBlocks(root = document) {
  const blocks = [...root.querySelectorAll("pre[data-json-highlight]")];
  if (!blocks.length) return;
  let highlighter;
  try {
    highlighter = await highlighterPromise;
  } catch (error) {
    console.warn("Shiki failed to initialize", error);
    return;
  }
  for (const block of blocks) {
    if (!block.isConnected || block.dataset.highlighted) continue;
    const code = block.textContent || "";
    try {
      const html = highlighter.codeToHtml(code, { lang: "json", theme: "github-dark-default" });
      block.outerHTML = html;
    } catch (error) {
      block.dataset.highlighted = "failed";
      console.warn("JSON highlight failed", error);
    }
  }
}
