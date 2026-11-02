// Render a social card locally; the site serves the checked-in PNG.
import { readFileSync } from "node:fs";
import { mkdir } from "node:fs/promises";
import path from "node:path";
import { chromium } from "playwright-core";

const root = path.resolve(import.meta.dir, "..");
const out = path.join(root, ".cache/social");
const logo = `data:image/svg+xml;base64,${readFileSync(path.join(root, "../docs/logo-dark.svg")).toString("base64")}`;
const homeTitle =
  "A fast, open-source, Lua-scriptable coding agent for the terminal";

// One cell of the background is one pixel of the logo (20 x 11 pixels, 1 x 1.1 units each).
const CELL_W = 17;
const CELL_H = 18.7;
const LOGO = { col: 4, row: 2, cols: 20, rows: 11 };

export function escapeHtml(value: string): string {
  return value.replace(
    /[&<>"']/g,
    (char) =>
      ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[
        char
      ]!,
  );
}

export function card(title: string, section: string): string {
  const subtitle = section
    ? `<div class="section">${escapeHtml(section)}</div>`
    : "";
  return `<!doctype html><html lang="en"><meta charset="utf-8"><title>${escapeHtml(title)}</title>
<style>
@font-face{font-family:Geist;src:url(/geist.woff2);font-weight:400}
@font-face{font-family:Geist;src:url(/geist-bold.woff2);font-weight:700}
@font-face{font-family:Mono;src:url(/mono.woff2)}
*{box-sizing:border-box}
/* The docs' slate palette and h1 typography. */
body{margin:0;position:relative;overflow:hidden;width:1200px;height:630px;background:#0b0c0f;color:#fff;font-family:Geist,sans-serif}
pre{position:absolute;inset:0;margin:0;overflow:hidden;opacity:.38;font:${CELL_W / 0.6}px/${CELL_H}px Mono,monospace;font-variant-ligatures:none;
  -webkit-mask-image:radial-gradient(ellipse ${LOGO.cols * CELL_W * 0.8}px ${LOGO.rows * CELL_H * 1.6}px at var(--logo-x,${(LOGO.col + LOGO.cols / 2) * CELL_W}px) var(--logo-y,${(LOGO.row + LOGO.rows / 2) * CELL_H}px),transparent 40%,#000);mask-image:radial-gradient(ellipse ${LOGO.cols * CELL_W * 0.8}px ${LOGO.rows * CELL_H * 1.6}px at var(--logo-x,${(LOGO.col + LOGO.cols / 2) * CELL_W}px) var(--logo-y,${(LOGO.row + LOGO.rows / 2) * CELL_H}px),transparent 40%,#000)}
.logo{display:block;width:${LOGO.cols * CELL_W}px;height:${LOGO.rows * CELL_H}px;margin-bottom:48px}
main{position:absolute;left:${LOGO.col * CELL_W}px;right:${LOGO.col * CELL_W}px;top:0;bottom:0;display:flex;flex-direction:column;justify-content:center;align-items:flex-start;gap:20px}
h1{font-size:76px;line-height:1.1;letter-spacing:-.025em;font-weight:700;margin:0;text-wrap:balance}
.section{font-size:36px;line-height:1.3;color:rgba(226,228,233,.56)}
</style><body class="home"><pre class="ascii"></pre><main><img class="logo" src="${logo}" alt="smelt">
<h1>${escapeHtml(title)}</h1>${subtitle}
</main></body></html>`;
}

async function generate() {
  await mkdir(out, { recursive: true });
  const fonts: Record<string, string> = {
    "/geist.woff2": "@fontsource/geist/files/geist-latin-400-normal.woff2",
    "/geist-bold.woff2": "@fontsource/geist/files/geist-latin-700-normal.woff2",
    "/mono.woff2": "jetbrains-mono/fonts/webfonts/JetBrainsMono-Regular.woff2",
  };
  const server = Bun.serve({
    port: 0,
    hostname: "127.0.0.1",
    fetch(req) {
      const font = fonts[new URL(req.url).pathname];
      return font
        ? new Response(Bun.file(path.join(root, "node_modules", font)))
        : new Response("Not found", { status: 404 });
    },
  });
  let browser;
  try {
    browser = await chromium.launch({
      executablePath: process.env.CHROME_PATH,
    });
    const page = await browser.newPage({
      viewport: { width: 1200, height: 630 },
      deviceScaleFactor: 1,
    });
    await page.goto(server.url.href);
    const html = card(
      process.argv[2] ?? homeTitle,
      process.argv[3] ?? "",
    ).replace(
      '<meta charset="utf-8">',
      `<meta charset="utf-8"><base href="${server.url}">`,
    );
    await page.setContent(html);
    await page.evaluate(async () => {
      await document.fonts.load("700 76px Geist");
      await document.fonts.load("40px Mono");
      await document.fonts.ready;
      if (!document.fonts.check("700 76px Geist"))
        throw new Error("Card fonts did not load");
      const image = document.querySelector("img")!;
      await image.decode();
      if (!image.naturalWidth) throw new Error("Logo did not load");
      const title = document.querySelector("h1")!;
      // Logo, title and section flow from the top. At most two lines, and inside the card;
      // shrink the title until both hold.
      const stack = () =>
        main.lastElementChild!.getBoundingClientRect().bottom -
        image.getBoundingClientRect().top;
      const main = document.querySelector("main")!;
      const size = () => parseFloat(getComputedStyle(title).fontSize);
      const lines = () =>
        Math.round(
          title.getBoundingClientRect().height /
            parseFloat(getComputedStyle(title).lineHeight),
        );
      const fits = () => lines() <= 2 && stack() <= 630 - 4 * 20.9;
      while (!fits() && size() > 28) {
        title.style.fontSize = `${size() - 2}px`;
      }
      if (!fits() || title.scrollWidth > title.clientWidth)
        throw new Error("Card title does not fit");
      // The field clears around the logo wherever the stack put it.
      const box = image.getBoundingClientRect();
      document.body.style.setProperty(
        "--logo-x",
        `${box.left + box.width / 2}px`,
      );
      document.body.style.setProperty(
        "--logo-y",
        `${box.top + box.height / 2}px`,
      );
    });
    await page.addScriptTag({
      path: path.join(root, "../docs/javascripts/home.js"),
    });
    await page.waitForFunction(() => document.querySelector(".ascii span"));
    const file = path.join(out, "card.png");
    await page.screenshot({ path: file });
    console.log(file);
  } finally {
    await browser?.close();
    server.stop(true);
  }
}

if (import.meta.main) await generate();
