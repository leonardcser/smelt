// Records the plugin gallery (docs/docs/gallery/index.md): every plugin in its own smelt, the same size,
// in the default theme on a dark terminal, over the same exchange (the lamp fix and a thanks), so
// from one card to the next only the plugin changes. Writes each recording to
// docs/docs/assets/replays/gallery/<slug>.json, every card's poster to stills.json there, and the
// cards to docs/docs/assets/gallery.json.
//
//   bun src/gallery.ts             # record every plugin
//   bun src/gallery.ts rain pong   # record only these, then rewrite the posters and the cards
//   bun src/gallery.ts --index     # only rewrite the posters and the cards
//
// SHOTS_SLOT=<n> gives a run its own home, project and tmux session, so several record at once.
import { existsSync, readFileSync } from "node:fs";
import { mkdir, readdir, rm } from "node:fs/promises";
import path from "node:path";
import { run, type RunOptions, type Step } from "./session";

const repo = path.resolve(import.meta.dir, "../../..");
const plugins = path.join(import.meta.dir, "../gallery");
const out = path.join(repo, "docs/docs/assets/replays/gallery");
const index = path.join(repo, "docs/docs/assets/gallery.json");

const COLS = 60;
const ROWS = 26;
/** The size of a wide recording, for the home page's cards. */
const WIDE = { cols: 100, rows: 22 };
const RECORD_EVERY_MS = 100;
/** The last screen of a recording, before it loops. */
const HOLD_MS = 1000;

const LAMP = "10% off a $50 lamp makes the lamp free. fix it";
// The prompt bar once a turn is done; a working indicator of a plugin's own may leave out the dot.
const DONE = /─ (● )?done/;

type Category = "Themes" | "Transcripts" | "Bars" | "Layouts" | "Workflow" | "Hooks" | "Tools" | "Commands" | "Events" | "While it works" | "Mouse and keys" | "Games";

interface Plugin {
  slug: string;
  category: Category;
  title?: string;
  /** Its source, from the repo root; `docs/shots/gallery/<slug>.lua` by default. */
  file?: string;
  /** Ships with smelt, so nothing is installed for it. */
  bundled?: boolean;
  /** The card's line, where the plugin's own comment does not read well as one. */
  summary?: string;
  /** Typed once the exchange is done, or before it for a `live` one: `repeat` times, a moment apart. */
  cmd?: string;
  repeat?: number;
  /** Runs the whole exchange this many times, for an effect `cmd` sets off by clearing it away. The
   * lamp is fixed once, so these `ask` something else. */
  again?: number;
  /** Reacts to smelt at work, so the recording starts at the question, the answer streaming slowly. */
  live?: boolean;
  /** The screen smelt is ready on, for a plugin that hides the statusline's mode. */
  ready?: RegExp;
  /** The screen a turn is done on, for a plugin that redraws the prompt bar. */
  done?: RegExp;
  /** The whole session, for one the usual exchange does not show. */
  script?: Step[];
  /** Where the card's poster is taken, to show the effect: how far into the recording, from 0 to
   * 1, or the frame where this global pattern matches most. The frame with the most text otherwise. */
  poster?: number | RegExp;
  /** Also recorded wide, as `<slug>-wide.json`, for the home page. */
  wide?: boolean;
  /** Asks this instead of the lamp fix. */
  ask?: string;
  /** What happens once the exchange is done: the mouse moving, the conversation scrolling, the
   * terminal losing focus, a line typed, the snake played, or steps of its own; seven seconds of
   * waiting otherwise. */
  then?: "hover" | "scroll" | "blur" | { type: string } | { steer: number } | { steps: Step[] };
}

const SECTIONS: { name: Category; blurb: string }[] = [
  { name: "Themes", blurb: "Restyle the whole interface: colors, mood, era." },
  { name: "Transcripts", blurb: "The conversation drawn as something else: a messenger, a timeline, panels." },
  { name: "Bars", blurb: "Statuslines, header lines, the prompt and its indicators, and notifications." },
  { name: "Layouts", blurb: "Rearrange the screen: sidebars, panes and whole desktops." },
  { name: "Workflow", blurb: "Keys and indicators that make smelt quicker to drive." },
  { name: "Tools", blurb: "New tools the agent can call, each drawn its own way in the transcript." },
  { name: "Hooks", blurb: "Code that runs when smelt acts: before a command runs, after a file changes." },
  { name: "Commands", blurb: "Effects you call up with a slash command, and that go away on their own or with a key." },
  { name: "Events", blurb: "Effects smelt sets off itself: a failed tool, a long turn finished, a cleared screen, you stepping away." },
  { name: "While it works", blurb: "Effects that come alive while the model streams and settle when the turn is done." },
  { name: "Mouse and keys", blurb: "Effects that follow the mouse and the keyboard." },
  { name: "Games", blurb: "Open one with its command and play while the agent works." },
];

const FEATURED = ["lualine", "bubbles", "win95", "chart", "weather", "xray", "decode", "snake"];

/** Every plugin, by section, in the order the gallery shows them. */
const PLUGINS: Plugin[] = [
  { slug: "light", category: "Themes" },
  { slug: "mono", category: "Themes", poster: 1, cmd: "mono amber" },
  { slug: "vaporwave", category: "Themes" },
  { slug: "huedrift", category: "Themes", title: "Hue drift" },
  { slug: "nightshift", category: "Themes", title: "Night shift", live: true, cmd: "nightshift on" },
  { slug: "colorblind", category: "Themes", live: true, cmd: "colorblind deutan" },
  { slug: "bloom", category: "Themes", live: true },

  { slug: "panels", category: "Transcripts" },
  { slug: "timeline", category: "Transcripts" },
  { slug: "bubbles", category: "Transcripts" },

  { slug: "lualine", category: "Bars", file: "docs/shots/plugins/lualine.lua", summary: "A lualine-style statusline with the mode, working directory, branch and changed-file counts." },
  { slug: "glitch-spinner", category: "Bars", poster: 0.12, title: "Glitch spinner", file: "docs/shots/plugins/glitch.lua", live: true, summary: "A working indicator whose status words decode in from noise, flicker, then corrupt into the next." },
  { slug: "pills", category: "Bars", live: true },
  { slug: "airline", category: "Bars", live: true, ready: /READY/ },
  { slug: "marquee", category: "Bars" },
  { slug: "filled-prompt", category: "Bars", title: "Filled prompt", done: /tests pass\./ },
  { slug: "shell-prompt", category: "Bars", title: "Shell prompt", done: /● done/, live: true },
  { slug: "toasts", category: "Bars", poster: 0.2, live: true },

  { slug: "c64", category: "Layouts", title: "Commodore 64", live: true },
  { slug: "win95", category: "Layouts", title: "Windows 95", live: true },

  { slug: "weather", category: "Commands", poster: 0.71, cmd: "weather rain", then: { steps: ["snow", "autumn", "spring"].flatMap((kind): Step[] => [{ pause: 4500 }, { type: `/weather ${kind}`, instant: true }, { pause: 300 }, { key: "Enter" }]).concat([{ pause: 4500 }]) } },
  { slug: "matrix", category: "Commands", poster: 0.57, cmd: "matrix" },
  { slug: "bounce", category: "Commands", poster: 0.43, title: "DVD", cmd: "dvd" },
  { slug: "rainbow", category: "Commands", poster: 0.43, cmd: "rainbow" },
  { slug: "flashlight", category: "Commands", poster: 0.71, cmd: "flashlight", then: "hover" },
  { slug: "donut", category: "Commands", poster: 0.71, file: "runtime/lua/smelt/plugins/donut.lua", bundled: true, cmd: "donut", summary: "Andy Sloane's donut.c, spinning over everything on screen." },
  { slug: "barrelroll", category: "Commands", wide: true, poster: 0.42, title: "Barrel roll", cmd: "barrelroll", repeat: 3 },
  { slug: "flip", category: "Commands", poster: 0.25, cmd: "flip", repeat: 4 },
  { slug: "bigclock", category: "Commands", poster: 0.57, title: "Big clock", cmd: "bigclock" },

  { slug: "screensaver", category: "Events", poster: 0.57, live: true, script: [{ pause: 300 }, ...say(LAMP), { pause: 800 }, { type: "/screensaver" }, { key: "Enter" }, { pause: 14000 }, { key: "Escape" }, { pause: 1500 }] },
  { slug: "confetti", category: "Events", poster: 0.78, cmd: "confetti", then: { steps: [{ pause: 6000 }] } },
  { slug: "lightning", category: "Events", poster: /[╱╲]/g, ask: "run the checkout tests" },
  { slug: "gravity", category: "Events", poster: 0.11, cmd: "clear", again: 2, ask: "what does apply_discount do?" },
  { slug: "dissolve", category: "Events", poster: 0.33, cmd: "clear", again: 2, ask: "what does apply_discount do?" },
  { slug: "away", category: "Events", poster: 0.57, then: "blur" },

  { slug: "decode", category: "While it works", poster: 0.3, ask: "how does the discount math work?", live: true },
  { slug: "freshink", category: "While it works", poster: 0.3, ask: "how does the discount math work?", title: "Fresh ink", live: true },
  { slug: "sparkler", category: "While it works", poster: 0.3, ask: "how does the discount math work?", live: true },
  { slug: "cat", category: "While it works", live: true },

  { slug: "trail", category: "Mouse and keys", poster: 0.56, then: "hover" },
  { slug: "xray", category: "Mouse and keys", title: "X-ray", poster: 0.95, then: { steps: [{ pause: 500 }, { hover: 4000 }, { glide: { to: /price_cents/, hold: 3500 } }] } },
  { slug: "focus", category: "Mouse and keys", poster: 0.44, then: "hover" },
  { slug: "sparks", category: "Mouse and keys", poster: 0.33, then: { type: "now make the tests cover rounding at the edges" } },

  { slug: "snake", category: "Games", poster: 0.6, file: "runtime/lua/smelt/plugins/snake.lua", bundled: true, cmd: "snake", then: { steer: 9000 }, summary: "Snake in a floating window while the agent works. Steer with hjkl or the arrows." },
  { slug: "invaders", category: "Games", poster: 0.4, cmd: "invaders" },
  { slug: "pong", category: "Games", poster: 0.4, cmd: "pong" },
  { slug: "breakout", category: "Games", poster: 0.6, cmd: "breakout" },
  { slug: "flappy", category: "Games", poster: 0.6, cmd: "flappy" },

  { slug: "crates", category: "Tools", title: "Crate lookup", file: "docs/shots/plugins/crates.lua", live: true, ask: "which decimal crate should I use for our money math?", summary: "A new tool for the agent: the latest version, downloads and description of a crate, from crates.io." },
  { slug: "todo", category: "Tools", title: "Plan", live: true, ask: "plan a test for 0% off" },
  { slug: "chart", category: "Tools", ask: "how big is each file?" },

  { slug: "autoformat", category: "Hooks", title: "Auto-format" },

  { slug: "whichkey", category: "Workflow", poster: 0.56, title: "Which-key", then: { steps: [{ pause: 800 }, { key: "C-Space" }, { pause: 3000 }, { key: "Escape" }, { pause: 1200 }] } },
  { slug: "queue-chips", category: "Workflow", title: "Queue chips", live: true, script: [{ pause: 300 }, ...say("give me a quick tour"), { pause: 1500 }, { type: "also add a test for 0% off" }, { key: "Enter" }, { pause: 400 }, { type: "and update the changelog" }, { key: "Enter" }, { pause: 9000 }] },
];

const sourceOf = (p: Plugin) => p.file ?? `docs/shots/gallery/${p.slug}.lua`;

/** The scripted provider as a local model, so a session reads as running on ollama. */
const init = (port: number | null) => `smelt.provider.register("ollama", {
  type = "openai-compatible",
  api_base = "http://127.0.0.1:${port}/v1",
  models = { { name = "qwen3.8:27b", context_window = 32768 } },
})
smelt.settings.show_cost = false
smelt.settings.restrict_to_workspace = false
`;

/** A message sent at once, held a moment so it can be read: the clip gets to the plugin sooner. */
function say(text: string): Step[] {
  return [{ type: text, instant: true }, { pause: 700 }, { key: "Enter" }];
}

function steps(p: Plugin): Step[] {
  if (p.script) return p.script;
  const command: Step[] = p.cmd
    ? Array.from({ length: p.repeat ?? 1 }, (_, i): Step[] => [{ pause: i ? 1500 : 400 }, { type: `/${p.cmd}` }, { key: "Enter" }]).flat()
    : [];
  const done = p.done ?? DONE;
  const exchange: Step[] = p.ask
    ? [...say(p.ask), { until: done }]
    : [...say(LAMP), { until: done }, { pause: 900 }, ...say("thanks!"), { until: /Anytime\./ }];
  const then: Step[] =
    p.then === "hover"
      ? [{ pause: 500 }, { hover: 7000 }]
      : p.then === "scroll"
        ? [-8, 8, -8, 8].flatMap((lines): Step[] => [{ pause: 1100 }, { wheel: { at: /lamp/, lines, gap: 70 } }]).concat([{ pause: 1500 }])
        : p.then === "blur"
          ? Array.from({ length: 3 }, (): Step[] => [{ pause: 700 }, { focus: false }, { pause: 1500 }, { focus: true }]).flat().concat([{ pause: 1000 }])
          : p.then && "type" in p.then
            ? [{ pause: 600 }, { type: p.then.type }, { pause: 3000 }]
            : p.then && "steer" in p.then
              ? [{ steer: p.then.steer }]
              : p.then && "steps" in p.then
                ? p.then.steps
                : [{ pause: 7000 }];
  return [
    { pause: 300 },
    ...(p.live ? command : []),
    ...Array.from({ length: p.again ?? 1 }, (_, i): Step[] => [...(i ? [{ pause: 500 }] : []), ...exchange, ...(p.live ? [] : command)]).flat(),
    ...then,
  ];
}

type Line = string | null;
/** A frame's changed lines, and `p`, the mouse pointer's cell, when it moved. */
type Replay = { cols: number; rows: number; scheme?: "light"; frames: { ms: number; l: Line[]; p?: [number, number] }[] };

const plain = (ansi: string) => ansi.replace(/\x1b\[[0-9;:]*m/g, "");

async function record(p: Plugin, { cols, rows } = { cols: COLS, rows: ROWS }): Promise<Replay> {
  const source = readFileSync(path.join(repo, sourceOf(p)), "utf8");
  const files: Record<string, string> = p.bundled ? {} : { [`plugins/${p.slug}.lua`]: source };
  if (/require\("fx"\)/.test(source)) files["lua/fx.lua"] = readFileSync(path.join(plugins, "fx.lua"), "utf8");
  const opts: RunOptions = {
    scheme: "dark", cols, rows, typeDelay: 55, wordDelay: p.live ? 45 : 10, live: false, vim: false, tps: false,
    recordEvery: RECORD_EVERY_MS, ready: p.ready, args: ["--mode", "yolo", "--model", "ollama/qwen3.8:27b"], init, home: true, files,
  };
  const { frames, pointer } = await run(steps(p), opts);
  // A live one opens on smelt's first screen; the rest once the lamp is fixed.
  const first = p.live ? frames.findIndex((f) => plain(f.ansi).trim() !== "") : frames.findIndex((f) => (p.done ?? DONE).test(plain(f.ansi)));
  const kept = frames.slice(Math.max(0, first));
  const replay: Replay = { cols, rows, frames: [] };
  let prev: string[] = [];
  let shown = "";
  /** The pointer's cell at `at`, once the mouse has moved. */
  const pointerAt = (at: number) => pointer.findLast((e) => e.at <= at);
  for (const [i, f] of kept.entries()) {
    const ms = i + 1 < kept.length ? kept[i + 1].at - f.at : HOLD_MS;
    const at = pointerAt(f.at);
    const cell = at ? `${at.x},${at.y}` : "";
    if (i && f.ansi === kept[i - 1].ansi && cell === shown) {
      replay.frames.at(-1)!.ms += ms;
      continue;
    }
    const lines = f.ansi.replace(/\n$/, "").split("\n");
    replay.frames.push({ ms, l: lines.map((line, j) => (line === prev[j] ? null : line)), ...(at && cell !== shown ? { p: [at.x, at.y] } : {}) });
    prev = lines;
    shown = cell;
  }
  return replay;
}

/** The whole screen at frame `at`, each frame holding only the lines that changed. */
function screen(r: Replay, at: number): string[] {
  const lines: string[] = [];
  for (const f of r.frames.slice(0, at + 1)) {
    lines.length = Math.min(lines.length, f.l.length);
    f.l.forEach((l, i) => {
      if (l !== null) lines[i] = l;
    });
  }
  return Array.from({ length: r.rows }, (_, i) => lines[i] ?? "");
}

/** Whether most of a screen's cells sit on a light background, as a light theme paints them on a
 * dark terminal. A cell with no background of its own is the terminal's, which is dark. */
function isLight(lines: string[]): boolean {
  let light = 0, cells = 0;
  for (const line of lines) {
    let bright = false;
    for (const [, params, text] of line.matchAll(/(?:\x1b\[([0-9;:]*)m)?([^\x1b]*)/g)) {
      if (params !== undefined) {
        const rgb = params.match(/(?:^|;)48;2;(\d+);(\d+);(\d+)/);
        if (rgb) {
          const [r, g, b] = rgb.slice(1).map(Number);
          bright = 0.2126 * r + 0.7152 * g + 0.0722 * b > 140;
        } else if (/(?:^|;)(0|49)?$|(?:^|;)(0|49);|(?:^|;)48;5;/.test(params)) {
          bright = false;
        }
      }
      const n = [...text].length;
      cells += n;
      if (bright) light += n;
    }
  }
  return light * 2 > cells;
}

/** The frame `p.poster` of the way into the recording, or else the one showing the most text, so a
 * card's poster is never an empty screen. */
function poster(p: Plugin, r: Replay): number {
  if (p.poster instanceof RegExp) {
    const pattern = p.poster;
    let best = 0, most = -1;
    for (let i = 0; i < r.frames.length; i++) {
      const hits = screen(r, i).reduce((n, l) => n + (plain(l).match(pattern)?.length ?? 0), 0);
      if (hits > most) [best, most] = [i, hits];
    }
    return best;
  }
  if (p.poster !== undefined) {
    const total = r.frames.reduce((t, f) => t + f.ms, 0);
    let t = 0;
    for (let i = 0; i < r.frames.length; i++) {
      t += r.frames[i].ms;
      if (t >= total * p.poster) return i;
    }
    return r.frames.length - 1;
  }
  let best = 0, most = -1;
  for (let i = 0; i < r.frames.length; i++) {
    const ink = screen(r, i).reduce((n, l) => n + plain(l).trim().length, 0);
    if (ink >= most) [best, most] = [i, ink];
  }
  return best;
}

/** The plugin's opening comment, without a leading "Name:" or "/command:": its first sentence for
 * the card, and the rest for its dialog. */
function describe(p: Plugin): { summary: string; detail: string } {
  const comment = readFileSync(path.join(repo, sourceOf(p)), "utf8")
    .split("\n")
    .filter((_, i, all) => all.slice(0, i + 1).every((l) => l.startsWith("--")))
    .map((l) => l.replace(/^--\s?/, ""))
    .join(" ")
    .trim();
  // smelt keeps its lowercase s at the start of a sentence.
  const text = comment.replace(/^(\/\S+|(?!(A|An|The) )[A-Z0-9][\w-]*( [a-z][\w-]*)?): /, "").replace(/^(?!smelt)./, (c) => c.toUpperCase());
  const first = text.match(/^(.+?[.!?])(\s|$)/)?.[1] ?? text;
  return { summary: p.summary ?? first, detail: text.slice(first.length).trim() };
}

async function writeIndex() {
  const stills: Record<string, Replay> = {};
  const items = [];
  for (const p of PLUGINS) {
    const file = path.join(out, `${p.slug}.json`);
    if (!existsSync(file)) throw new Error(`${p.slug} has no recording; run bun src/gallery.ts ${p.slug}`);
    const r = JSON.parse(readFileSync(file, "utf8")) as Replay;
    const light = isLight(screen(r, r.frames.length - 1));
    if (light !== (r.scheme === "light")) {
      if (light) r.scheme = "light";
      else delete r.scheme;
      await Bun.write(file, JSON.stringify(r));
    }
    const at = poster(p, r);
    const pointer = r.frames.slice(0, at + 1).findLast((f) => f.p)?.p;
    stills[p.slug] = { cols: r.cols, rows: r.rows, ...(r.scheme ? { scheme: r.scheme } : {}), frames: [{ ms: 1000, l: screen(r, at), ...(pointer ? { p: pointer } : {}) }] };
    const { summary, detail } = describe(p);
    const cmd = readFileSync(path.join(repo, sourceOf(p)), "utf8").match(/smelt\.cmd\.register\(\s*"([^"]+)"/)?.[1];
    items.push({
      slug: p.slug,
      title: p.title ?? p.slug.charAt(0).toUpperCase() + p.slug.slice(1),
      category: p.category,
      summary,
      detail,
      command: cmd ? `/${cmd}` : null,
      source: sourceOf(p),
      bundled: p.bundled ?? false,
    });
  }
  // Recordings of plugins no longer in the gallery.
  for (const f of await readdir(out)) {
    if (f !== "stills.json" && !PLUGINS.some((p) => `${p.slug}.json` === f || (p.wide && `${p.slug}-wide.json` === f))) await rm(path.join(out, f));
  }
  await Bun.write(path.join(out, "stills.json"), JSON.stringify(stills));
  await Bun.write(index, `${JSON.stringify({ featured: FEATURED, sections: SECTIONS, items }, null, 1)}\n`);
  console.log(`gallery.json: ${items.length} plugins`);
}

const args = process.argv.slice(2);
if (!args.includes("--index")) {
  const wanted = args.length ? args.map((s) => PLUGINS.find((p) => p.slug === s) ?? (() => { throw new Error(`unknown plugin ${s}`); })()) : PLUGINS;
  await mkdir(out, { recursive: true });
  for (const p of wanted) {
    const r = await record(p);
    const json = JSON.stringify(r);
    await Bun.write(path.join(out, `${p.slug}.json`), json);
    const s = r.frames.reduce((n, f) => n + f.ms, 0) / 1000;
    if (p.wide) await Bun.write(path.join(out, `${p.slug}-wide.json`), JSON.stringify(await record(p, WIDE)));
    console.log(`${p.slug}: ${r.frames.length} frames, ${s.toFixed(1)} s, ${Math.round(json.length / 1000)} kB`);
  }
}
await writeIndex();
