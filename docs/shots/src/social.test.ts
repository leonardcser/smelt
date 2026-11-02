import { expect, test } from "bun:test";
import { card, escapeHtml } from "./social";

test("escape page titles and sections as text", () => {
  expect(escapeHtml(`<script>"A&B"</script>'`)).toBe(
    "&lt;script&gt;&quot;A&amp;B&quot;&lt;/script&gt;&#39;",
  );
  const html = card("<img src=x onerror=alert(1)>", "Guide & reference");
  expect(html).not.toContain("<img src=x");
  expect(html).toContain("Guide &amp; reference");
});

test("card has the logo and title without wallpaper, install copy or url", () => {
  const html = card("Getting started", "Guide");
  expect(html).toContain("<h1>Getting started</h1>");
  expect(html).toContain('<div class="section">Guide</div>');
  expect(html).toContain('src="data:image/svg+xml;base64,');
  expect(html).not.toMatch(
    /wallpaper|curl|install\.sh|smeltagent\.dev|border-top/,
  );
});

test("home card omits empty section", () => {
  expect(card("smelt", "")).not.toContain('<div class="section">');
});
