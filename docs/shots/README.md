# Docs terminal replays and the README demo GIF

Records the terminal sessions embedded in the docs by driving a real `smelt` binary in tmux.
Each one is written as a replay (`docs/docs/assets/replays/*.json`) that
`docs/docs/javascripts/replay.js` plays back as real text, in both color schemes. The README
cannot run the player, so the `demo` scene is also rendered to `assets/demo.gif` by that same
player, frame by frame, in headless Chromium.

| Output | Source | Provider |
| ------ | ------ | -------- |
| `replays/{prompt,approval,goal}.json` (one frame each) | `stills` in `src/scenes.ts` | Scripted mock (deterministic, offline) |
| `replays/<scene>.json`, plus `assets/demo.gif` for `demo` | same-named entries in `scenes` | Live `codex/gpt-6-luna` (`paste` and `plugin` send no prompt; `files`, `resize`, `vim`, `cursor`, `models` and the theme scenes use the scripted mock) |

## Run

```bash
cargo build                      # or set SMELT_BIN to any smelt binary
cd docs/shots
bun install
bunx playwright-core install chromium   # once, for the README GIF; or set CHROME_PATH
bun run stills                   # the one-frame replays
bun run record                   # every scene, real agent and live model
bun run record queue vim         # only the named scenes
bun test src/session.test.ts     # offline screen-wait regressions
bun run record -- --cached       # re-render the last recordings, no model calls
bun run all
```

## Social card preview

```bash
cd docs/shots
bun src/social.ts                                # homepage title
bun src/social.ts 'Themes and keymaps' Customize   # custom title and section
bun test src/social.test.ts
```

Writes a 1200 x 630 preview to `.cache/social/card.png`, using the website's
`home.js` background and local fonts. Titles shrink to at most two lines.
To publish the default card, run without arguments, then copy the PNG to
`../docs/assets/social-card.png`. All pages share that checked-in image;
normal docs builds do not need Bun or Chromium.

## Plugin gallery

The gallery page (`docs/docs/gallery/index.md`) shows the plugins in `gallery/`, each recorded in
its own smelt at 60x26, in the default theme, over the same scripted exchange: the lamp fix and a
thanks. `src/gallery.ts` lists them, with their section and how each is set off (a command, the
mouse, scrolling), and writes `replays/gallery/<slug>.json`, every card's poster in
`replays/gallery/stills.json` and the cards in `assets/gallery.json`.

```bash
bun run gallery                  # every plugin
bun run gallery -- rain pong     # only these, then the posters and the cards
bun run gallery -- --index       # only the posters and the cards
```

`SHOTS_SLOT=<n>` gives a run its own home, project and tmux session, so several record at once.

Requirements: `bun`, `tmux`, `ffmpeg`, a Rust toolchain. The `files` scene also needs `nvim`.

## Notes

- Live scenes reuse your `smelt auth` login for `codex`. They run in yolo mode with medium
  reasoning effort and pin the model, against a throwaway state dir, so nothing
  lands in your session history or remembered state.
- The project is a throwaway fixture at `/tmp/acme-app` with a real
  Rust crate with one deliberate bug, so `cargo test` compiles and streams for real.
  It needs a Rust toolchain.
  A scene's `projectFiles` copies more files in from `fixtures/`, like the `image` scene's screenshot.
- The build identity is blanked in captures. Clocks are pinned to a fixed time by the generated `init.lua`.
- To add a still, append a `{ shot: "name", rows: N }` step to `stills` in `src/scenes.ts`.
  To embed any replay in a page:
  `<figure class="term" data-replay="../../assets/replays/name.json" aria-label="..." markdown="0"><pre class="term__screen" aria-hidden="true"></pre></figure>`
- Docs pages put the macOS Ventura dark wallpaper (`docs/docs/assets/wallpaper.jpg`, downloaded
  once) behind every terminal; it shows around the window when a scene resizes it.
- Recordings are cached in `.cache/frames/`, so `--cached` only re-renders.
- Unless a scene is `realtime`, stretches with no typing nearby (boot, model thinking, tool runs) are sped up 3x
  and the last screen is held for 2 s; see `IDLE_SPEEDUP` in `src/main.ts`.
- Use `{ wait: /screen text/ }` to check a screen transition, including an already-visible match.
  Unlike `until`, it does not approve dialogs. `pause` sets viewing time, not readiness.
- `vim: true` enables vim mode.
- Besides keys, a scene can `paste` text (bracketed paste), `drag` a screen line with the left button
  held (the prompt's top bar, a dialog's top bar, or text to select) and turn the mouse `wheel`.
- Every scene overlays the commands it presses as keycaps in the wallpaper below the terminal
  (`keys: false` turns it off). Text typed into the prompt and anything pressed in vim's insert
  mode is left out. The home page hides the overlay, except for the vim and cursor replays, which keep it in
  the window's top-right corner (`data-keys="corner"`), and the README GIF has none.
- `kitty: true` (the `math` and `image` scenes) attaches a client to the tmux session, as Ghostty,
  that answers smelt's Kitty graphics probes through passthrough and keeps the images smelt sends. The replay stores
  them under `img`, and `replay.js` draws them over their Unicode placeholder cells.
- Each scene is recorded once in the dark scheme; the player derives the light one by flipping
  the lightness of every color.
