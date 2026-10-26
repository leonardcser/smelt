import { readFileSync } from "node:fs";
import type { Scheme, Step } from "./session";

/** Still screens embedded in the docs: a one-frame replay per `shot`. */
export const stills: Step[] = [
  { type: "explain how @src/pricing.rs applies a discount" },
  { key: "Enter" },
  { until: /─ done/ },
  { shot: "prompt", rows: 11 },
  { type: "run the tests" },
  { key: "Enter" },
  { until: /Allow\?/ },
  { shot: "approval", rows: 20 },
  { key: "Escape" },
  { pause: 1500 },
  { type: "/new" },
  { key: "Enter" },
  { pause: 1500 },
  { type: "/goal implement the approved migration and validate every package" },
  { key: "Enter" },
  { until: /GOAL[\s\S]*─ done/ },
  { shot: "goal", rows: 11 },
];

/** A recorded run of the real agent against the real model, in yolo mode so nothing stops for approval. */
export type Scene = {
  /** Also render a GIF to `assets/demo.gif` for the README, which cannot run the replay player. */
  readme?: boolean;
  /** Terminal size, 110x30 unless set. */
  cols?: number;
  rows?: number;
  /** Overlay pressed commands under the terminal, like a key-capture app. On unless set to false. */
  keys?: boolean;
  /** Vi keybindings in the prompt. */
  vim?: boolean;
  /** Answer with the scripted mock provider instead of the live model, so a scene can be recorded twice identically. */
  mock?: boolean;
  /** Record on a light terminal; the player shows it as is in the light scheme instead of flipping a dark one. */
  scheme?: Scheme;
  /** Record through a Kitty graphics terminal, so math renders as images. */
  kitty?: boolean;
  /** Play in real time. Otherwise stretches with no typing nearby are sped up. */
  realtime?: boolean;
  /** Open on the first prompt, fully typed, just before it is sent. */
  fromSubmit?: boolean;
  /** Open on the first screen matching this, skipping everything before it. */
  from?: RegExp;
  /** Extra files for the config dir, keyed by path under `~/.config/smelt/`. */
  files?: Record<string, string>;
  steps: Step[];
};

/** The plugin from the "Anatomy of a plugin" section of docs/docs/extend/index.md. */
const helloPlugin = `local M = smelt.plugin("hello")

M.state.greetings = M.state.greetings or 0
smelt.cmd.register("hello", function(arg)
  M.state.greetings = M.state.greetings + 1
  local name = (arg and arg ~= "") and arg or "world"
  smelt.notify.info("hello, " .. name)
end, { desc = "greet someone" })

return M
`;

/** For the theme replays: once the answer is in, steps through bundled themes, naming each. */
const themeCycle = (themes: string[]) => `local themes = { "${themes.join('", "')}" }
smelt.events.on("turn_end", function()
  local i = 0
  local timer
  timer = smelt.timer.every(1800, function()
    i = i + 1
    if i > #themes then return timer:remove() end
    smelt.theme.use(themes[i])
    smelt.notify.info(smelt.theme.info(themes[i]).name)
  end)
end)
`;

/** A short answer with a heading, code and a list, recolored by a run of themes. */
const themes = (scheme: "dark" | "light", list: string[]): Scene => ({
  mock: true,
  scheme,
  from: /─ done/,
  realtime: true,
  cols: 80,
  rows: 24,
  files: { "plugins/theme_cycle.lua": themeCycle(list) },
  steps: [
    { pause: 1200 },
    { type: "how should apply_discount handle the rounding?" },
    { pause: 400 },
    { key: "Enter" },
    { until: /─ done/ },
    { pause: 1800 * list.length + 1500 },
  ],
});

/** The plugins shown on the homepage and in the plugin gallery, from `plugins/`. */
const plugin = (name: string) => readFileSync(new URL(`../plugins/${name}.lua`, import.meta.url), "utf8");

export const scenes: Record<string, Scene> = {
  /** The agent calls a tool a plugin gave it. */
  tool: {
    fromSubmit: true,
    realtime: true,
    cols: 80,
    rows: 24,
    files: { "plugins/crates.lua": plugin("crates") },
    steps: [
      { pause: 1200 },
      { type: "use crate_info to compare rust_decimal and bigdecimal for our money math" },
      { pause: 400 },
      { key: "Enter" },
      { until: /─ done/ },
      { pause: 2500 },
    ],
  },

  /** Display math in an answer, drawn as images by a Kitty graphics terminal. */
  math: {
    fromSubmit: true,
    realtime: true,
    kitty: true,
    cols: 80,
    rows: 24,
    steps: [
      { pause: 1200 },
      { type: "what are the equations for gradient descent and adam? keep it brief" },
      { pause: 400 },
      { key: "Enter" },
      { until: /─ done/ },
      { pause: 2500 },
    ],
  },

  /** Bundled dark themes, played in the dark scheme. */
  "themes-dark": themes("dark", ["catppuccin-mocha", "dracula", "gruvbox-dark", "nord", "solarized-dark", "one-half-dark"]),

  /** Bundled light themes, played in the light scheme. */
  "themes-light": themes("light", ["catppuccin-latte", "gruvbox-light", "solarized-light", "one-half-light", "github", "coldark-cold"]),

  /** A lualine-style statusline: mode, working directory, branch and changed-file counts. */
  lualine: {
    fromSubmit: true,
    realtime: true,
    cols: 110,
    rows: 14,
    files: { "plugins/lualine.lua": plugin("lualine") },
    steps: [
      { pause: 1200 },
      { type: "run the tests and fix whatever fails" },
      { pause: 400 },
      { key: "Enter" },
      // Skip ahead through the model's first think, then play the turn in real time.
      { speed: 8 },
      { until: /^\* \w/m },
      { speed: 1 },
      { until: /─ done/ },
      { pause: 2500 },
    ],
  },

  /** A changed-files sidebar beside the default layout, updating as the agent edits. */
  changes: {
    fromSubmit: true,
    realtime: true,
    cols: 110,
    rows: 24,
    files: { "plugins/changes.lua": plugin("changes") },
    steps: [
      { pause: 1200 },
      { type: "run the tests and fix whatever fails" },
      { pause: 400 },
      { key: "Enter" },
      // Skip ahead through the model's first think, then play the turn in real time.
      { speed: 8 },
      { until: /^\* \w/m },
      { speed: 1 },
      { until: /─ done/ },
      { pause: 2500 },
    ],
  },

  /** Playing snake (steered from the screen) in its window while the agent works underneath. */
  snake: {
    fromSubmit: true,
    realtime: true,
    rows: 24,
    steps: [
      { pause: 1200 },
      { type: "which edge cases does apply_discount miss?" },
      { pause: 400 },
      { key: "Enter" },
      { pause: 1200 },
      { type: "/snake" },
      { pause: 400 },
      { key: "Enter" },
      { steer: 16000 },
      { pause: 1500 },
    ],
  },

  /** Find a failing test, fix it, and confirm. */
  demo: {
    fromSubmit: true,
    readme: true,
    realtime: true,
    steps: [
      { pause: 1200 },
      { type: "the cart tests are failing, can you find the bug and fix it? run them again after" },
      { pause: 500 },
      { key: "Enter" },
      { until: /─ done/ },
      { pause: 1200 },
    ],
  },

  /** Let the agent build up context, then compact it by hand. */
  compact: {
    steps: [
      { pause: 1200 },
      { type: "give me a quick tour of this project: what does each file do?" },
      { pause: 400 },
      { key: "Enter" },
      { until: /─ done/ },
      { pause: 1000 },
      { type: "explain how Cart::total and apply_discount fit together" },
      { pause: 400 },
      { key: "Enter" },
      { until: /─ done/ },
      { pause: 1000 },
      { type: "/compact keep the discount details" },
      { pause: 400 },
      { key: "Enter" },
      { until: /compact(ed|ion)|─ done/i },
      { pause: 2500 },
    ],
  },

  /** Leave a session, come back with /resume, then review what changed with /diff. */
  resume: {
    realtime: true,
    steps: [
      { speed: 4 },
      { pause: 1200 },
      { type: "add a doc comment to Cart::total explaining how discounts are applied" },
      { pause: 400 },
      { key: "Enter" },
      { until: /─ done/ },
      { pause: 1000 },
      { type: "/new" },
      { pause: 300 },
      { key: "Enter" },
      { pause: 3500 },
      { type: "which rust edition does this project use?" },
      { pause: 400 },
      { key: "Enter" },
      { until: /─ done/ },
      { pause: 1000 },
      { speed: 1 },
      { type: "/resume" },
      { pause: 400 },
      { key: "Enter" },
      { pause: 3500 },
      { press: ["Down"], gap: 2500 },
      { key: "Enter" },
      { pause: 2500 },
      { type: "/diff" },
      { pause: 400 },
      { key: "Enter" },
      { pause: 3000 },
      { press: ["Tab", "j", "j", "]"], gap: 900 },
      { pause: 1500 },
      { press: ["q"] },
      { pause: 1000 },
    ],
  },

  /** Vim editing in the prompt, with the key presses on screen. */
  vim: {
    vim: true,
    rows: 9,
    from: /keep the public api unchanged/,
    steps: [
      { pause: 1200 },
      { type: "fix the rounding bug in apply_discount" },
      { press: ["C-j"] },
      { type: "add a test for a 100 percent discount" },
      { press: ["C-j"] },
      { type: "keep the public api unchanged" },
      { pause: 700 },
      { press: ["Escape"], gap: 700 },
      { press: ["g", "g", "0", "w", "w", "w"], gap: 520 },
      { press: ["c", "i", "w"], gap: 420 },
      { type: "overflow" },
      { press: ["Escape"], gap: 700 },
      { press: ["$", "b", "v", "i", "w", "y"], gap: 520 },
      { pause: 600 },
      { press: ["j", "0", "f", "1", "d", "w"], gap: 520 },
      { pause: 600 },
      { press: ["u"], gap: 700 },
      { press: ["V", "j", "j"], gap: 520 },
      { pause: 700 },
      { press: ["Escape", "G", "A"], gap: 520 },
      { type: " and update the docs" },
      { pause: 900 },
      { press: ["Escape"], gap: 700 },
      { pause: 1200 },
    ],
  },

  /** Run the command registered by the hello plugin. */
  plugin: {
    files: { "plugins/hello.lua": helloPlugin },
    steps: [
      { pause: 1500 },
      { type: "/hel" },
      { pause: 1500 },
      { type: "lo smelt" },
      { pause: 700 },
      { key: "Enter" },
      { pause: 2500 },
      { type: "/hello" },
      { pause: 700 },
      { key: "Enter" },
      { pause: 2500 },
    ],
  },

  /** Search the transcript with `/`, jump between matches, then go back to the prompt. */
  search: {
    vim: true,
    steps: [
      { pause: 1200 },
      { type: "give me a quick tour of this project: what does each file do?" },
      { pause: 400 },
      { key: "Enter" },
      { until: /─ done/ },
      { pause: 1000 },
      { press: ["Escape"], gap: 700 },
      { press: ["C-w", "k"], gap: 250 },
      { pause: 600 },
      { press: ["/"], gap: 500 },
      { type: "discount" },
      { pause: 700 },
      { key: "Enter" },
      { pause: 1500 },
      { press: ["n", "n"], gap: 1300 },
      { press: ["N"], gap: 1300 },
      { pause: 1000 },
      { press: ["C-c"], gap: 800 },
      { pause: 800 },
    ],
  },

  /** A multi-line prompt, then the window and a dialog reflowing as the terminal is resized. */
  layout: {
    steps: [
      { pause: 1200 },
      { type: "refactor the pricing module:" },
      { press: ["C-j"] },
      { type: "- move the discount rules into their own file" },
      { press: ["C-j"] },
      { type: "- add tests for rounding at the edges" },
      { press: ["C-j"] },
      { type: "- keep the public api unchanged" },
      { pause: 1200 },
      { resize: { cols: 72, rows: 30 }, over: 1600 },
      { pause: 1400 },
      { resize: { cols: 110, rows: 30 }, over: 1600 },
      { pause: 1000 },
      { press: ["F1"], gap: 1200 },
      { pause: 1000 },
      { resize: { cols: 80, rows: 20 }, over: 1600 },
      { pause: 1400 },
      { resize: { cols: 110, rows: 30 }, over: 1600 },
      { pause: 1000 },
      { press: ["Escape"], gap: 800 },
      { pause: 800 },
    ],
  },

  /** Select a line of a multi-line prompt in visual mode and send only that. */
  select: {
    vim: true,
    steps: [
      { pause: 1200 },
      { type: "what does apply_discount do?" },
      { press: ["C-j"] },
      { type: "how is Cart::total tested?" },
      { press: ["C-j"] },
      { type: "list the public api of this crate" },
      { pause: 900 },
      { press: ["Escape"], gap: 700 },
      { press: ["g", "g", "V"], gap: 600 },
      { pause: 1000 },
      { press: ["Enter"], gap: 500 },
      { until: /─ done/ },
      { pause: 1200 },
      { press: ["V"], gap: 700 },
      { pause: 900 },
      { press: ["Enter"], gap: 500 },
      { until: /─ done/ },
      { pause: 1800 },
    ],
  },

  /** Queue messages while the agent works, promote the queue, then interrupt. */
  queue: {
    steps: [
      { pause: 1200 },
      { type: "run the test suite and walk me through any failures" },
      { pause: 400 },
      { key: "Enter" },
      { until: /working/ },
      { pause: 2500 },
      { type: "after that, check the pricing module for edge cases" },
      { pause: 300 },
      { key: "Enter" },
      { pause: 1500 },
      { type: "then summarize everything in one sentence" },
      { pause: 300 },
      { key: "Enter" },
      { pause: 2200 },
      { press: ["Enter"], gap: 500 },
      { pause: 2200 },
      { press: ["Enter"], gap: 500 },
      { pause: 1500 },
      { until: /─ done/ },
      { pause: 2000 },
    ],
  },
};
