# Docs terminal replays and the README demo GIF

Records the terminal sessions embedded in the docs by driving a real `smelt` binary in tmux.
Each one is written as a replay (`docs/docs/assets/replays/*.json`) that
`docs/docs/javascripts/replay.js` plays back as real text, in both color schemes. The README
cannot run the player, so the `demo` scene is also rendered to `assets/demo.gif` by that same
player, frame by frame, in headless Chromium.

| Output | Source | Provider |
| ------ | ------ | -------- |
| `replays/{prompt,approval,goal}.json` (one frame each) | `stills` in `src/scenes.ts` | Scripted mock (deterministic, offline) |
| `replays/<scene>.json`, plus `assets/demo.gif` for `demo` | same-named entries in `scenes` | Live `codex/gpt-6-luna` (`vim`, `layout` and `plugin` send no prompt) |

## Run

```bash
cargo build                      # or set SMELT_BIN to any smelt binary
cd docs/shots
bun install
bunx playwright-core install chromium   # once, for the README GIF; or set CHROME_PATH
bun run stills                   # the one-frame replays
bun run record                   # every scene, real agent and live model
bun run record queue vim         # only the named scenes
bun run record -- --cached       # re-render the last recordings, no model calls
bun run all
```

Requirements: `bun`, `tmux`, `ffmpeg`, a Rust toolchain.

## Notes

- Live scenes reuse your `smelt auth` login for `codex`. They run in yolo mode with medium
  reasoning effort and pin the model, against a throwaway state dir, so nothing
  lands in your session history or remembered state.
- The project is a throwaway fixture at `/tmp/acme-app` with a real
  Rust crate with one deliberate bug, so `cargo test` compiles and streams for real.
  It needs a Rust toolchain.
- Clock times and build identity are stripped from captures.
- To add a still, append a `{ shot: "name", rows: N }` step to `stills` in `src/scenes.ts`.
  To embed any replay in a page:
  `<figure class="term" data-replay="../../assets/replays/name.json" aria-label="..." markdown="0"><pre class="term__screen" aria-hidden="true"></pre></figure>`
- Docs pages put the macOS Ventura dark wallpaper (`docs/docs/assets/wallpaper.jpg`, downloaded
  once) behind every terminal; it shows around the window when a scene resizes it.
- Recordings are cached in `.cache/frames/`, so `--cached` only re-renders.
- Unless a scene is `realtime`, stretches with no typing nearby (boot, model thinking, tool runs) are sped up 3x
  and the last screen is held for 2 s; see `IDLE_SPEEDUP` in `src/main.ts`.
- `vim: true` enables vim mode.
- Every scene overlays the commands it presses as keycaps in the wallpaper below the terminal
  (`keys: false` turns it off). Text typed into the prompt and anything pressed in vim's insert
  mode is left out. The home page hides the overlay, except for the vim replay, which keeps it in
  the window's top-right corner (`data-keys="corner"`), and the README GIF has none.
- `kitty: true` (the `math` scene) attaches a client to the tmux session that answers smelt's
  Kitty graphics probes through passthrough and keeps the images smelt sends. The replay stores
  them under `img`, and `replay.js` draws them over their Unicode placeholder cells.
- Each scene is recorded once in the dark scheme; the player derives the light one by flipping
  the lightness of every color.
