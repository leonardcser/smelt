// Renders a replay frame by frame with the site's own player (docs/docs/javascripts/replay.js) in
// headless Chromium, for the README GIF, so it looks exactly like the docs.
import { chromium } from "playwright-core";
import path from "node:path";

const root = path.resolve(import.meta.dir, "..");
const site = path.resolve(root, "../docs");
const fonts = path.join(root, "node_modules/jetbrains-mono/fonts/webfonts");

/** Each frame of `docs/docs/assets/replays/<name>.json` with `cell` px wide columns, and how long it shows. */
export async function renderReplay(name: string, cell = 9): Promise<{ pngs: Buffer[]; ms: number[] }> {
  const replay = await Bun.file(path.join(site, `assets/replays/${name}.json`)).json();
  // The terminal sits on the docs wallpaper; the page is served beside the site so its relative URLs resolve.
  const page = `<!doctype html><meta charset="utf-8">
<link rel="stylesheet" href="/stylesheets/extra.css">
<style>
  @font-face{font-family:"JetBrains Mono";font-weight:400;src:url(/__fonts/JetBrainsMono-Regular.woff2)}
  @font-face{font-family:"JetBrains Mono";font-weight:700;src:url(/__fonts/JetBrainsMono-Bold.woff2)}
  :root{--md-code-font-family:"JetBrains Mono"}
  body{margin:0}
  .term__keys{display:none}
  .md-typeset figure.term{width:${replay.cols * cell}px}
</style>
<div class="md-typeset"><figure class="term" data-replay="/assets/replays/${name}.json"><pre class="term__screen"></pre></figure></div>
<script src="/javascripts/replay.js"></script>`;
  const server = Bun.serve({
    port: 0,
    fetch(req) {
      const url = new URL(req.url).pathname;
      if (url === "/") return new Response(page, { headers: { "content-type": "text/html" } });
      if (url.startsWith("/__fonts/")) return new Response(Bun.file(path.join(fonts, path.basename(url))));
      return new Response(Bun.file(path.join(site, path.normalize(url))));
    },
  });
  const browser = await chromium.launch({ executablePath: process.env.CHROME_PATH });
  try {
    const tab = await browser.newPage({ viewport: { width: replay.cols * cell + 100, height: 1000 } });
    await tab.goto(server.url.href);
    const figure = tab.locator("figure.term");
    await figure.evaluate(async (el) => {
      const r = (el as unknown as { __replay: { load(): Promise<void>; pause(): void } }).__replay;
      await r.load();
      r.pause();
      await document.fonts.ready;
    });
    const pngs: Buffer[] = [];
    for (let n = 0; n < replay.frames.length; n++) {
      await figure.evaluate((el, n) => (el as unknown as { __replay: { show(n: number, jump: boolean): void } }).__replay.show(n, n === 0), n);
      pngs.push(await figure.screenshot());
    }
    return { pngs, ms: replay.frames.map((f: { ms: number }) => f.ms) };
  } finally {
    await browser.close();
    server.stop(true);
  }
}
