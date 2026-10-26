import { $ } from "bun";
import { existsSync } from "node:fs";
import { mkdir } from "node:fs/promises";
import path from "node:path";
import { encodeGif } from "./gif";
import { renderReplay } from "./render";
import { scenes, stills, type Scene } from "./scenes";
import { run, type Frame, type KeyEvent, type KittyImage, type RunOptions, type Scheme, type SpeedMark } from "./session";

const repo = path.resolve(import.meta.dir, "../../..");
const replaysDir = path.join(repo, "docs/docs/assets/replays");
const cols = 110;
const wallpaperUrl =
  "https://4kwallpapers.com/images/wallpapers/macos-ventura-macos-13-macos-2022-stock-dark-mode-5k-retina-5120x2880-8133.jpg";

/** The macOS Ventura dark wallpaper the docs site puts behind every terminal, downsized once. */
async function makeWallpaper() {
  const file = path.join(repo, "docs/docs/assets/wallpaper.jpg");
  if (existsSync(file)) return;
  const source = path.join(import.meta.dir, "../.cache/wallpaper-source.jpg");
  if (!existsSync(source)) {
    // The host refuses requests that look like hotlinking, so ask the way a browser does.
    const res = await fetch(wallpaperUrl, {
      headers: { "user-agent": "Mozilla/5.0 (X11; Linux x86_64) Chrome/124 Safari/537.36", referer: "https://4kwallpapers.com/" },
    });
    if (!res.ok) throw new Error(`wallpaper download failed (${res.status})`);
    await Bun.write(source, res);
  }
  await $`ffmpeg -y -loglevel error -i ${source} -vf scale=1920:-1 -q:v 6 ${file}`;
  console.log("wrote", path.relative(repo, file));
}

const base: RunOptions = { scheme: "dark", cols, rows: 30, typeDelay: 0, wordDelay: 0, live: false, vim: false, tps: false, recordEvery: 0 };

/** Each `shot` of the stills run becomes a one-frame replay of its top `rows` lines. */
async function makeStills() {
  const { captures } = await run(stills, base);
  for (const c of captures) {
    const ansi = c.ansi.split("\n").slice(0, c.rows).join("\n");
    await writeReplay(c.name, { cols, rows: c.rows }, [{ ansi, cols, rows: c.rows, keys: [], ms: END_HOLD_MS }]);
  }
}

type Recording = { frames: Frame[]; keylog: KeyEvent[]; marks: SpeedMark[]; images?: Record<number, KittyImage> };

/** How long a keystroke stays on the overlay, and how many keycaps it holds. */
const KEY_LINGER_MS = 1400;
const KEY_MAX = 8;
/** Unless a scene is `realtime`, stretches with no typing nearby (boot, model thinking, tool runs) play this many times faster. */
const IDLE_SPEEDUP = 3;
const IDLE_BEFORE_MS = 1500;
const IDLE_AFTER_MS = 500;
/** How long the screen before the first keystroke shows. */
const LEAD_MS = 600;
/** How long a `fromSubmit` scene shows its typed prompt before it is sent. */
const PROMPT_MS = 1200;
/** The last screen is held this long so the result can be read. */
const END_HOLD_MS = 2000;

type Moment = { ansi: string; cols: number; rows: number; keys: string[]; ms: number };

const plain = (ansi: string) => ansi.replace(/\x1b\[[0-9;:]*m/g, "");

const framesFile = (name: string) => path.join(import.meta.dir, `../.cache/frames/${name}.json`);

async function record(scene: Scene): Promise<Recording> {
  const opts: RunOptions = { ...base, scheme: scene.scheme ?? "dark", cols: scene.cols ?? cols, rows: scene.rows ?? 30, typeDelay: 55, live: !scene.mock, wordDelay: 25, vim: scene.vim ?? false, files: scene.files, tps: true, recordEvery: 100, kitty: scene.kitty };
  const { frames, keylog, marks, images } = await run(scene.steps, opts);
  return { frames, keylog, marks, images };
}

/** Collapses a recording into unique screens (and overlays), each lasting as long as its run, at playback speed. */
function timeline(rec: Recording, scene: Scene): Moment[] {
  const { keylog, marks = [] } = rec;
  const frames = rec.frames.map((f) => ({ ...f, cols: f.cols ?? scene.cols ?? cols, rows: f.rows ?? scene.rows ?? 30 }));
  // Smelt takes a moment to draw; skip the blank screen before it appears.
  const firstDrawn = frames.findIndex((f) => plain(f.ansi).trim() !== "");
  frames.splice(0, Math.max(firstDrawn, 0));
  // Open just before the first keystroke, past smelt's boot, on the typed prompt as it is sent,
  // or on the first screen a scene asks for.
  const sent = scene.fromSubmit ? keylog.find((k) => k.label === "⏎") : undefined;
  const first = scene.from
    ? frames.findIndex((f) => scene.from!.test(plain(f.ansi)))
    : sent
      ? frames.findLastIndex((f) => f.at <= sent.at)
      : frames.findIndex((f) => f.at > (keylog[0]?.at ?? 0) - LEAD_MS) - 1;
  frames.splice(0, Math.max(first, 0));
  // Text typed into the prompt, and anything pressed in vim's insert mode, is plain typing, so only
  // commands show; Esc leaves insert mode.
  const inserting = (at: number) => /INSERT/.test(plain(rec.frames.findLast((f) => f.at <= at)?.ansi ?? ""));
  const shown = keylog.filter((k) => !k.typed && (k.label === "esc" || !inserting(k.at)));
  const keysAt = (t: number) =>
    scene.keys !== false ? shown.filter((k) => k.at <= t && k.at > t - KEY_LINGER_MS).slice(-KEY_MAX).map((k) => k.label) : [];
  const idleAt = (t: number) => !keylog.some((k) => k.at > t - IDLE_BEFORE_MS && k.at < t + IDLE_AFTER_MS);
  const moments: Moment[] = [];
  for (const [i, f] of frames.entries()) {
    const last = i === frames.length - 1;
    const marked = marks.findLast((k) => k.at <= f.at)?.speed ?? 1;
    const speed = scene.realtime ? marked : idleAt(f.at) ? IDLE_SPEEDUP : 1;
    const ms = last ? END_HOLD_MS : (frames[i + 1].at - f.at) / speed;
    const keys = keysAt(f.at);
    const prev = moments.at(-1);
    if (prev?.ansi === f.ansi && prev.cols === f.cols && prev.rows === f.rows && prev.keys.join() === keys.join()) prev.ms += ms;
    else moments.push({ ansi: f.ansi, cols: f.cols, rows: f.rows, keys, ms });
  }
  if (moments.length) moments[0].ms = sent ? PROMPT_MS : Math.min(moments[0].ms, LEAD_MS);
  return moments;
}

/**
 * The site plays these in the browser as real text (see docs/docs/javascripts/replay.js).
 * Each frame lists its screen lines, with `null` for a line unchanged since the previous frame,
 * and `k`, the keys to overlay, when the scene shows them. `img` holds the Kitty graphics images
 * the screens place, by id. `scheme: "light"` marks a recording made on a light terminal.
 */
async function writeReplay(name: string, size: { cols: number; rows: number; scheme?: Scheme }, moments: Moment[], images: Record<number, KittyImage> = {}) {
  const file = path.join(replaysDir, `${name}.json`);
  let prev: string[] = [];
  const frames = moments.map((m) => {
    const lines = m.ansi.replace(/\n$/, "").split("\n");
    const l = lines.map((line, i) => (line === prev[i] ? null : line));
    prev = lines;
    return {
      ms: Math.round(m.ms),
      l,
      ...(m.keys.length ? { k: m.keys } : {}),
      ...(m.cols !== size.cols || m.rows !== size.rows ? { cols: m.cols, rows: m.rows } : {}),
    };
  });
  await mkdir(replaysDir, { recursive: true });
  const img = Object.keys(images).length ? { img: images } : {};
  await Bun.write(file, JSON.stringify({ ...size, ...img, frames }));
  console.log("wrote", path.relative(repo, file), `from ${frames.length} frames`);
}

async function makeScene(name: string, scene: Scene) {
  // The recording is the slow, billed part; `--cached` re-renders the last one.
  let rec: Recording;
  if (process.argv.includes("--cached")) {
    rec = await Bun.file(framesFile(name)).json();
  } else {
    rec = await record(scene);
    await Bun.write(framesFile(name), JSON.stringify(rec));
  }
  const moments = timeline(rec, scene);
  await writeReplay(name, { cols: scene.cols ?? cols, rows: scene.rows ?? 30, ...(scene.scheme === "light" ? { scheme: scene.scheme } : {}) }, moments, rec.images);
  if (scene.readme) {
    const { pngs, ms } = await renderReplay(name);
    const file = path.join(repo, "assets/demo.gif");
    await encodeGif(pngs, ms, file);
    console.log("wrote", path.relative(repo, file), `from ${pngs.length} frames`);
  }
}

const [cmd = "all", ...rest] = process.argv.slice(2).filter((a) => !a.startsWith("--"));
if (!["stills", "record", "wallpaper", "all"].includes(cmd)) {
  console.error(`usage: bun src/main.ts [stills|record|wallpaper|all] [${Object.keys(scenes).join("|")}] [--cached]`);
  process.exit(2);
}
if (cmd === "wallpaper" || cmd === "all") await makeWallpaper();
if (cmd === "stills" || cmd === "all") await makeStills();
if (cmd === "record" || cmd === "all") {
  for (const name of rest.length ? rest : Object.keys(scenes)) {
    if (!scenes[name]) throw new Error(`unknown scene ${name}`);
    await makeScene(name, scenes[name]);
  }
}
