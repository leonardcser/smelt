// Drives a real smelt binary inside tmux against a scripted provider and a throwaway project.
import { $ } from "bun";
import { existsSync } from "node:fs";
import { copyFile, mkdir, readFile, rm, writeFile } from "node:fs/promises";
import path from "node:path";
import { MODEL, startMock } from "./mock";
import { CACHE, PROJECT, TMUX } from "./paths";
import { pickMove, readBoard } from "./steer";
import { hoverAt } from "./hover";

export type Scheme = "dark" | "light";

export type Step =
  /** Type text key by key, or all at once when `instant`. */
  | { type: string; instant?: boolean }
  | { key: string }
  | { press: string[]; gap?: number }
  | { resize: { cols: number; rows: number }; over?: number }
  /** Paste text through the terminal's bracketed paste, as Cmd+V would. */
  | { paste: string }
  /** Tell smelt the terminal window gained (`true`) or lost (`false`) focus, as a terminal reports it. */
  | { focus: boolean }
  /** Hold the left button on the last screen line matching `grab` (at `col`) and drag it `dy` rows (negative is up). */
  | { drag: { grab: RegExp; dy: number; col?: number; over?: number } }
  /** Turn the mouse wheel `lines` notches (negative is up) over the last screen line matching `at`. */
  | { wheel: { at: RegExp; lines: number; gap?: number } }
  /** Move the mouse with no button held for `hover` ms, along `hoverAt`'s path. */
  | { hover: number }
  /** Glide the mouse over `over` ms onto the first text matching `to`, then hold it there `hold` ms. */
  | { glide: { to: RegExp; over?: number; hold?: number } }
  | { pause: number }
  /** From here on, images smelt shows are photos, kept in their own colors in the light scheme, not flipped like math. */
  | { photos: boolean }
  /** From here on, play the recording this many times faster (1 is real time). */
  | { speed: number }
  /** Wait for a screen match, including one already visible, without approving dialogs. */
  | { wait: RegExp; timeout?: number }
  /** Run the turn until the screen matches, approving any dialog the match is not waiting for. */
  | { until: RegExp }
  /** Play the open snake window for this many ms, steering toward the food. */
  | { steer: number }
  | { shot: string; rows: number };

export type Frame = { at: number; ansi: string; cols: number; rows: number };

/** Where the mouse pointer moved to, in 0-based cells. */
export type PointerEvent = { at: number; x: number; y: number };

/** A change of playback speed, set by a `speed` step. */
export type SpeedMark = { at: number; speed: number };

/** One keystroke, labeled the way a key-capture overlay would show it; `typed` marks prompt text, not a command. */
export type KeyEvent = { at: number; label: string; typed?: boolean };

export type Capture = { name: string; rows: number; ansi: string };

/** A Kitty graphics image smelt placed with Unicode placeholders, `cols` by `rows` cells. */
export type KittyImage = { png: string; cols: number; rows: number; photo?: true };

export type RunOptions = {
  scheme: Scheme;
  cols: number;
  rows: number;
  /** Milliseconds between typed characters; 0 sends each string at once. */
  typeDelay: number;
  /** Milliseconds between streamed words from the mock provider. */
  wordDelay: number;
  /** Use the real codex provider from the local login instead of the scripted mock. */
  live: boolean;
  /** Vi keybindings in the prompt. */
  vim: boolean;
  /** Show tokens per second in the status line. */
  tps: boolean;
  /** Extra files for the config dir, keyed by path under `~/.config/smelt/`. */
  files?: Record<string, string>;
  /** Extra project files, keyed by path in the project, copied from `fixtures/` by name. */
  projectFiles?: Record<string, string>;
  /** Poll the screen at this interval to record animation frames; 0 disables recording. */
  recordEvery: number;
  /** Attach a terminal that speaks the Kitty graphics protocol, so math renders as images. */
  kitty?: boolean;
  /** The screen smelt is ready on; the statusline's mode by default. */
  ready?: RegExp;
  /** Extra command-line arguments for smelt. */
  args?: string[];
  /** Extra `init.lua`, given the scripted provider's port (null on a live run). */
  init?: (port: number | null) => string;
  /** Run smelt with the throwaway home as `$HOME` and its config in `~/.config`, so paths under it
   * read as they would on a real machine. */
  home?: boolean;
  /** The project repo's branch, instead of git's default. */
  branch?: string;
};

const root = path.resolve(import.meta.dir, "..");
const cache = CACHE;
const project = PROJECT;
const tmuxName = TMUX;

function smeltBinary() {
  const candidates = [
    process.env.SMELT_BIN,
    path.resolve(root, "../../target/release-fast/smelt"),
    path.resolve(root, "../../target/release/smelt"),
    path.resolve(root, "../../target/debug/smelt"),
  ];
  const found = candidates.find((c) => c && existsSync(c));
  if (!found) throw new Error("smelt binary not found; run `cargo build` or set SMELT_BIN");
  return found;
}

const fixture: Record<string, string> = {
  ".gitignore": "target/\nCargo.lock\n",
  "Cargo.toml": `[package]\nname = "acme-app"\nversion = "0.1.0"\nedition = "2021"\n`,
  "src/lib.rs": `pub mod cart;\npub mod pricing;\n`,
  "src/pricing.rs": `/// Price in cents after taking \`percent\` percent off.\npub fn apply_discount(price_cents: u32, percent: u32) -> u32 {\n    price_cents - price_cents * percent / 10\n}\n`,
  "src/cart.rs": `use crate::pricing::apply_discount;\n\npub struct Item {\n    pub name: String,\n    pub price_cents: u32,\n    pub discount_percent: u32,\n}\n\n#[derive(Default)]\npub struct Cart {\n    pub items: Vec<Item>,\n}\n\nimpl Cart {\n    /// Total in cents after per-item discounts.\n    pub fn total(&self) -> u32 {\n        self.items\n            .iter()\n            .map(|i| apply_discount(i.price_cents, i.discount_percent))\n            .sum()\n    }\n}\n`,
  "tests/cart.rs": `use acme_app::cart::{Cart, Item};\n\nfn cart_with(price_cents: u32, discount_percent: u32) -> Cart {\n    Cart {\n        items: vec![Item { name: "lamp".into(), price_cents, discount_percent }],\n    }\n}\n\n#[test]\nfn empty_cart_is_free() {\n    assert_eq!(Cart::default().total(), 0);\n}\n\n#[test]\nfn no_discount_keeps_price() {\n    assert_eq!(cart_with(5000, 0).total(), 5000);\n}\n\n#[test]\nfn discount_applies_to_total() {\n    assert_eq!(cart_with(5000, 10).total(), 4500);\n}\n\n#[test]\nfn full_discount_is_free() {\n    assert_eq!(cart_with(5000, 100).total(), 0);\n}\n`,
};

/** The config dir under the throwaway home. */
const configDir = (opts: RunOptions) => (opts.home ? ".config" : "config");

async function prepare(port: number | null, opts: RunOptions) {
  await rm(project, { recursive: true, force: true });
  for (const [file, body] of Object.entries(fixture)) {
    await mkdir(path.dirname(path.join(project, file)), { recursive: true });
    await writeFile(path.join(project, file), body);
  }
  for (const [file, source] of Object.entries(opts.projectFiles ?? {})) {
    await mkdir(path.dirname(path.join(project, file)), { recursive: true });
    await copyFile(path.join(root, "fixtures", source), path.join(project, file));
  }

  // A committed repo, so `git status` in the demo is clean instead of an error.
  const git = ["-C", project, "-c", "user.name=smelt", "-c", "user.email=docs@smelt.invalid"];
  await $`git ${git} init -q ${opts.branch ? ["-b", opts.branch] : []}`.quiet();
  await $`git ${git} add -A`.quiet();
  await $`git ${git} commit -q -m ${"initial commit"}`.quiet();

  const home = path.join(cache, "home");
  const config = configDir(opts);
  await rm(home, { recursive: true, force: true });
  for (const d of [`${config}/smelt`, "state", "data", "cache"]) await mkdir(path.join(home, d), { recursive: true });
  const provider =
    port === null
      ? ""
      : `smelt.provider.register("codex", {
  type = "openai-compatible",
  api_base = "http://127.0.0.1:${port}/v1",
  models = { { name = "${MODEL}", context_window = 200000 } },
})
`;
  await writeFile(
    path.join(home, config, "smelt/init.lua"),
    `${provider}smelt.defaults.set({ model = "codex/${MODEL}" })
smelt.settings.autoupgrade = "off"
-- No build identity under the splash, so nothing drawn over it can leave part of it in a capture.
smelt.build.display = ""
smelt.settings.show_tips = false
smelt.settings.show_slug = false
smelt.settings.file_icons = true
smelt.settings.show_tps = ${opts.tps}
smelt.settings.vim = ${opts.vim}
smelt.settings.auto_continue = "off"
-- Message and tool times count from a fixed moment, so every recording shows the same clock.
do
  local transcript = require("smelt.transcript.defaults")
  local timestamp, start = transcript.timestamp, smelt.time.now_ms()
  local base = os.time({ year = 2026, month = 3, day = 21, hour = 14, min = 37, sec = 3 }) * 1000
  transcript.timestamp = function(at_ms, now_ms)
    local shift = function(ms) return tonumber(ms) and base + tonumber(ms) - start end
    return timestamp(shift(at_ms), shift(now_ms))
  end
end
${opts.init?.(port) ?? ""}`,
  );
  for (const [file, body] of Object.entries(opts.files ?? {})) {
    const target = path.join(home, config, "smelt", file);
    await mkdir(path.dirname(target), { recursive: true });
    await writeFile(target, body);
  }
  return home;
}

// Seeded so a regenerated GIF types the same way every time.
let seed = 7;
const rand = () => {
  seed = (seed + 0x6d2b79f5) | 0;
  let t = Math.imul(seed ^ (seed >>> 15), 1 | seed);
  t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
  return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
};

/** Human-ish keystroke gap: jittered, slower after spaces and punctuation, with the odd hesitation. */
function keyDelay(base: number, ch: string, prev = "") {
  let d = base * (0.55 + rand() * 0.9);
  if (prev === " ") d *= 1.5;
  if (/[,.?]/.test(prev)) d *= 2.2;
  if (ch === " " && rand() < 0.08) d += base * 6;
  return d;
}

const keyNames: Record<string, string> = {
  Enter: "⏎", Escape: "esc", Tab: "⇥", PageUp: "PgUp", PageDown: "PgDn", BSpace: "⌫", Space: "␣", Up: "↑", Down: "↓", Left: "←", Right: "→",
};

/** tmux key name or literal character to its overlay label (`C-r` becomes `⌃R`). */
function keyLabel(key: string) {
  if (keyNames[key]) return keyNames[key];
  const ctrl = /^C-(.)$/.exec(key);
  return ctrl ? `⌃${ctrl[1].toUpperCase()}` : key;
}

const tmux = (...args: (string | number)[]) => $`tmux ${args.map(String)}`.quiet();

/** An SGR mouse report for the 0-based cell, which smelt reads as it would from the terminal. */
const mouse = (code: number, col: number, row: number, release = false) => `\x1b[<${code};${col + 1};${row + 1}${release ? "m" : "M"}`;

// Strip the build identity so captures are reproducible.
const clean = (s: string) => s.replace(/v\d+\.\d+\.\d+[-\w.+]*/g, (v) => " ".repeat(v.length));

const plain = (s: string) => s.replace(/\x1b\[[0-9;:]*m/g, "");

/** Applies one SGR sequence to `pen`, which maps each attribute slot to the parameters that set it. */
function sgr(pen: Map<string, string>, params: string) {
  const p = params === "" ? ["0"] : params.split(";");
  for (let i = 0; i < p.length; i++) {
    const [head] = p[i].split(":");
    const c = +head;
    if (c === 0) pen.clear();
    else if (c === 38 || c === 48 || c === 58) {
      // Extended colors take their arguments as parameters of their own, unless written with colons.
      const n = p[i].includes(":") ? 1 : p[i + 1] === "5" ? 3 : 5;
      pen.set(head, p.slice(i, i + n).join(";"));
      i += n - 1;
    } else if (c === 39 || c === 49 || c === 59) pen.delete(String(c - 1));
    else if ((c >= 30 && c <= 37) || (c >= 90 && c <= 97)) pen.set("38", head);
    else if ((c >= 40 && c <= 47) || (c >= 100 && c <= 107)) pen.set("48", head);
    else if (c === 22) pen.delete("1"), pen.delete("2");
    else if (c >= 23 && c <= 29) pen.delete(String(c - 20));
    else pen.set(head, p[i]);
  }
}

/** tmux writes a capture as one stream, so a line starts with the pen the line before ended with.
 * Each line here restates that pen first, so a player can draw any line on its own. A blank line
 * stays blank: tmux writes every colored cell out as a space, so nothing on it is colored. */
function selfContained(ansi: string) {
  const pen = new Map<string, string>();
  return ansi
    .split("\n")
    .map((line) => {
      const start = pen.size && plain(line) ? `\x1b[${[...pen.values()].join(";")}m` : "";
      for (const m of line.matchAll(/\x1b\[([0-9;:]*)m/g)) sgr(pen, m[1]);
      return start + line;
    })
    .join("\n");
}

/**
 * The screen and the window size it was drawn at, read in one tmux call so they always agree.
 * `full` keeps each line's trailing cells, so a background a line was erased to is recorded.
 */
async function snapshot(full = false): Promise<{ ansi: string; cols: number; rows: number }> {
  const keep = full ? ["-N"] : [];
  const out = await $`tmux capture-pane -t ${tmuxName} -e -p ${keep} ${";"} display-message -p -t ${tmuxName} ${"#{window_width} #{window_height}"}`
    .quiet()
    .text();
  const end = out.trimEnd().lastIndexOf("\n");
  const [cols, rows] = out.slice(end + 1).trim().split(" ").map(Number);
  return { ansi: selfContained(clean(out.slice(0, end + 1))), cols, rows };
}

const screen = async () => (await snapshot()).ansi;

/**
 * Attaches to the session as the outer terminal smelt sees through tmux passthrough: it answers
 * the Kitty graphics probe and the cell size query, and keeps every image smelt transmits.
 */
function attachKitty(images: Record<number, KittyImage>, photos: () => boolean) {
  const decoder = new TextDecoder();
  let pending = "";
  let current: number | undefined;
  // tmux tells smelt which terminal its client runs in, and smelt trusts Ghostty with graphics.
  const client = Bun.spawn(["tmux", "attach", "-t", tmuxName], {
    env: { ...process.env, TMUX: "", TERM: "xterm-ghostty" },
    terminal: {
      cols: 200,
      rows: 60,
      data(term, bytes) {
        pending += decoder.decode(bytes, { stream: true });
        if (pending.includes("\x1b[16t")) term.write("\x1b[6;32;16t");
        pending = pending.replaceAll("\x1b[16t", "");
        // Commands are `ESC _G keys ; payload ESC \`; an image may arrive in several chunks.
        let m: RegExpExecArray | null;
        const command = /\x1b_G([^;\x1b]*);?([^\x1b]*)\x1b\\/g;
        let end = 0;
        while ((m = command.exec(pending))) {
          end = command.lastIndex;
          const keys = Object.fromEntries(m[1].split(",").map((kv) => kv.split("=")));
          if (keys.a === "q") term.write(`\x1b_Gi=${keys.i};OK\x1b\\`);
          else if (keys.a === "T") {
            current = Number(keys.i);
            images[current] = { png: m[2], cols: Number(keys.c), rows: Number(keys.r), ...(images[current]?.photo || photos() ? { photo: true as const } : {}) };
          } else if (keys.a === undefined && current !== undefined) images[current].png += m[2];
          if (keys.m === "0" || (keys.a === "T" && keys.m !== "1")) current = undefined;
        }
        // Keep an unfinished command for the next read.
        const open = pending.lastIndexOf("\x1b_G");
        pending = open >= end ? pending.slice(open) : "";
      },
    },
  });
  return client;
}

export async function run(
  steps: Step[],
  opts: RunOptions,
): Promise<{ captures: Capture[]; frames: Frame[]; keylog: KeyEvent[]; marks: SpeedMark[]; pointer: PointerEvent[]; images: Record<number, KittyImage> }> {
  const mock = opts.live ? null : startMock(opts.wordDelay);
  const home = await prepare(mock?.port ?? null, opts);
  // Live runs get a throwaway state dir holding a copy of the real codex login, so sessions and
  // permissions stay out of the real state. The copy is written back if smelt refreshed the token.
  const state = `${home}/state`;
  const realAuth = path.join(process.env.XDG_STATE_HOME ?? path.join(process.env.HOME!, ".local/state"), "smelt/codex_auth.json");
  const tempAuth = path.join(state, "smelt/codex_auth.json");
  if (opts.live) {
    await mkdir(path.dirname(tempAuth), { recursive: true });
    await copyFile(realAuth, tempAuth);
  }
  const fg = opts.scheme === "light" ? "0;15" : "15;0";
  const env = [
    `XDG_CONFIG_HOME=${home}/${configDir(opts)}`,
    // Cargo and rustup find their toolchains through $HOME unless told where they are.
    ...(opts.home
      ? [`HOME=${home}`, `CARGO_HOME=${process.env.CARGO_HOME ?? `${process.env.HOME}/.cargo`}`, `RUSTUP_HOME=${process.env.RUSTUP_HOME ?? `${process.env.HOME}/.rustup`}`]
      : []),
    // A scripted run never finds the host's Claude Code, so its models never join the list.
    ...(opts.live ? [] : ["SMELT_CLAUDE_CODE_EXECUTABLE=/nonexistent/claude"]),
    `XDG_STATE_HOME=${state}`,
    `XDG_DATA_HOME=${home}/data`,
    `XDG_CACHE_HOME=${home}/cache`,
    `COLORFGBG="${fg}"`,
    "TERM=xterm-256color",
    "COLORTERM=truecolor",
  ].join(" ");
  await tmux("kill-session", "-t", tmuxName).nothrow();
  await tmux(
    "new-session", "-d", "-s", tmuxName, "-x", opts.cols, "-y", opts.rows,
    `${opts.kitty ? "tmux wait-for smelt-kitty; " : ""}cd ${project} && env -u LLM_API_BASE_URL -u LLM_API_KEY ${env} ${smeltBinary()}${opts.live ? ` --model codex/${MODEL} --mode yolo --reasoning-effort medium` : ""}${(opts.args ?? []).map((a) => ` ${a}`).join("")}; sleep 3`,
  );

  // Keep the window at the size we ask for, whatever else attaches to tmux.
  await tmux("set-option", "-t", tmuxName, "window-size", "manual");

  const images: Record<number, KittyImage> = {};
  let photos = false;
  const kitty = opts.kitty ? attachKitty(images, () => photos) : null;
  if (kitty) {
    await tmux("set-option", "-t", tmuxName, "allow-passthrough", "on");
    while (!(await $`tmux list-clients -t ${tmuxName}`.quiet().text()).trim()) await Bun.sleep(50);
    await tmux("wait-for", "-S", "smelt-kitty");
  }

  const captures: Capture[] = [];
  const frames: Frame[] = [];
  const keylog: KeyEvent[] = [];
  const marks: SpeedMark[] = [];
  const pointer: PointerEvent[] = [];
  const point = (x: number, y: number) => pointer.push({ at: Date.now() - t0, x, y });
  const t0 = Date.now();
  const log = (label: string, typed = false) => keylog.push({ at: Date.now() - t0, label, ...(typed ? { typed } : {}) });
  let recording = opts.recordEvery > 0;
  const recorder = (async () => {
    while (recording) {
      frames.push({ at: Date.now() - t0, ...(await snapshot(true)) });
      await Bun.sleep(opts.recordEvery);
    }
  })();

  const waitFor = async (re: RegExp, timeout = 60_000) => {
    const deadline = Date.now() + timeout;
    do {
      if (re.test(plain(await screen()))) return;
      await Bun.sleep(100);
    } while (Date.now() < deadline);
    throw new Error(`timed out waiting for ${re}\n${await screen()}`);
  };

  try {
    await waitFor(opts.ready ?? /normal|yolo/);
    for (const step of steps) {
      if ("type" in step) {
        if (opts.typeDelay && !step.instant) {
          for (const [i, ch] of [...step.type].entries()) {
            await tmux("send-keys", "-t", tmuxName, "-l", ch);
            log(ch === " " ? "␣" : ch, true);
            await Bun.sleep(keyDelay(opts.typeDelay, ch, step.type[i - 1]));
          }
        } else {
          await tmux("send-keys", "-t", tmuxName, "-l", step.type);
          await Bun.sleep(300);
        }
      } else if ("key" in step) {
        await tmux("send-keys", "-t", tmuxName, step.key);
        log(keyLabel(step.key));
      } else if ("press" in step) {
        for (const k of step.press) {
          await tmux("send-keys", "-t", tmuxName, ...(k.length === 1 ? ["-l"] : []), k);
          log(keyLabel(k));
          await Bun.sleep((step.gap ?? 420) * (0.8 + rand() * 0.5));
        }
      } else if ("focus" in step) {
        await tmux("send-keys", "-t", tmuxName, "-l", step.focus ? "\x1b[I" : "\x1b[O");
      } else if ("paste" in step) {
        await tmux("set-buffer", "-b", "shots-paste", "--", step.paste);
        await tmux("paste-buffer", "-p", "-d", "-b", "shots-paste", "-t", tmuxName);
        await Bun.sleep(300);
      } else if ("drag" in step) {
        const row = (await screen()).split("\n").findLastIndex((l) => step.drag.grab.test(plain(l)));
        if (row < 0) throw new Error(`nothing to grab for ${step.drag.grab}\n${await screen()}`);
        const col = step.drag.col ?? 4;
        const send = (code: number, y: number, release = false) => {
          point(col, y);
          return tmux("send-keys", "-t", tmuxName, "-l", mouse(code, col, y, release));
        };
        log("drag");
        await send(0, row);
        const n = Math.max(1, Math.abs(step.drag.dy));
        for (let i = 1; i <= n; i++) {
          await Bun.sleep((step.drag.over ?? 900) / n);
          await send(32, row + Math.round((step.drag.dy * i) / n));
        }
        await send(0, row + step.drag.dy, true);
      } else if ("wheel" in step) {
        const row = (await screen()).split("\n").findLastIndex((l) => step.wheel.at.test(plain(l)));
        if (row < 0) throw new Error(`nothing to scroll for ${step.wheel.at}\n${await screen()}`);
        log("scroll");
        point(8, row);
        for (let i = 0; i < Math.abs(step.wheel.lines); i++) {
          await tmux("send-keys", "-t", tmuxName, "-l", mouse(step.wheel.lines < 0 ? 64 : 65, 8, row));
          await Bun.sleep(step.wheel.gap ?? 120);
        }
      } else if ("hover" in step) {
        log(`hover ${step.hover}`);
        const start = Date.now();
        for (let t = 0; t < step.hover; t = Date.now() - start) {
          const [x, y] = hoverAt(t, opts.cols, opts.rows);
          point(x, y);
          await tmux("send-keys", "-t", tmuxName, "-l", mouse(35, x, y));
          await Bun.sleep(33);
        }
      } else if ("glide" in step) {
        const lines = plain(await screen()).split("\n");
        const row = lines.findIndex((l) => step.glide.to.test(l));
        if (row < 0) throw new Error(`nothing to glide to for ${step.glide.to}\n${lines.join("\n")}`);
        const col = lines[row].search(step.glide.to) + 1;
        const from = pointer.at(-1) ?? { x: col, y: row };
        log("glide");
        const over = step.glide.over ?? 700;
        for (let t = 0; t <= over; t += 33) {
          const k = 1 - (1 - t / over) ** 3;
          const x = Math.round(from.x + (col - from.x) * k), y = Math.round(from.y + (row - from.y) * k);
          point(x, y);
          await tmux("send-keys", "-t", tmuxName, "-l", mouse(35, x, y));
          await Bun.sleep(33);
        }
        point(col, row);
        await tmux("send-keys", "-t", tmuxName, "-l", mouse(35, col, row));
        await Bun.sleep(step.glide.hold ?? 2500);
      } else if ("resize" in step) {
        // Drag the window edge: step the size so the UI reflows frame by frame.
        const [w0, h0] = (await $`tmux display-message -p -t ${tmuxName} ${"#{window_width} #{window_height}"}`.quiet().text())
          .trim().split(" ").map(Number);
        const n = Math.max(1, Math.round((step.over ?? 1200) / 80));
        for (let i = 1; i <= n; i++) {
          const x = Math.round(w0 + ((step.resize.cols - w0) * i) / n);
          const y = Math.round(h0 + ((step.resize.rows - h0) * i) / n);
          await tmux("resize-window", "-t", tmuxName, "-x", x, "-y", y);
          await Bun.sleep(80);
        }
      } else if ("speed" in step) {
        marks.push({ at: Date.now() - t0, speed: step.speed });
      } else if ("pause" in step) {
        await Bun.sleep(step.pause);
      } else if ("photos" in step) {
        photos = step.photos;
      } else if ("steer" in step) {
        // The head is the snake cell that appeared since the last read; steer once per tick.
        let dir = "Right";
        let seen = new Set<number>();
        for (const end = Date.now() + step.steer; Date.now() < end; await Bun.sleep(25)) {
          const board = readBoard(await screen());
          if (!board) continue;
          const head = [...board.snake].find((k) => !seen.has(k));
          seen = board.snake;
          if (head === undefined) continue;
          const next = pickMove(board, head, dir);
          if (next !== dir) {
            await tmux("send-keys", "-t", tmuxName, next);
            log(keyLabel(next));
            dir = next;
          }
        }
      } else if ("wait" in step) {
        await waitFor(step.wait, step.timeout);
      } else if ("until" in step) {
        // A match already on screen (the last turn's `─ ● done`) only counts once this turn has started.
        let started = !step.until.test(plain(await screen()));
        for (let i = 0; i < 1200; i++) {
          const text = plain(await screen());
          started ||= /working|compacting|summariz/i.test(text);
          if (started && step.until.test(text)) {
            break;
          } else if (/1\. allow once/.test(text)) {
            await Bun.sleep(1600);
            await tmux("send-keys", "-t", tmuxName, "Enter");
            await Bun.sleep(600);
          } else {
            await Bun.sleep(150);
          }
          if (i === 1199) throw new Error(`turn did not finish\n${text}`);
        }
      } else {
        await tmux("resize-window", "-t", tmuxName, "-x", opts.cols, "-y", step.rows);
        await Bun.sleep(1200);
        captures.push({ name: step.shot, rows: step.rows, ansi: await screen() });
        await tmux("resize-window", "-t", tmuxName, "-x", opts.cols, "-y", opts.rows);
        await Bun.sleep(600);
      }
    }
  } finally {
    recording = false;
    await recorder;
    await tmux("kill-session", "-t", tmuxName).nothrow();
    kitty?.kill();
    mock?.stop(true);
    if (opts.live) {
      const [before, after] = await Promise.all([readFile(realAuth), readFile(tempAuth).catch(() => null)]);
      if (after && !before.equals(after)) await copyFile(tempAuth, realAuth);
    }
  }
  return { captures, frames, keylog, marks, pointer, images };
}
