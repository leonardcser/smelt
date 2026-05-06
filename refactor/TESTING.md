# Testing — strategy

How smelt is tested. Three layers, each owns its own scope. Updated when a
layer's harness changes shape.

For meta-rules and the doc index, see `README.md`.

## The three layers

| Layer  | Scope                                                                 | Harness                                                                          | Assertion                                                                            |
| ------ | --------------------------------------------------------------------- | -------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------ |
| **L1** | Pure data: `Buffer` extmark math, vim motions / text-objects / operators, `Cells` fan-out, `LayoutTree` solver, wrap math | `#[cfg(test)] mod tests` next to the code under test                              | Imperative — `TestHarness::new("hello world").handle(key('w')); assert_eq!(...)`     |
| **L2** | Engine ↔ Lua ↔ tools ↔ permissions ↔ persistence (one binary spawn per scenario) | `smelt --headless --format json` against a wiremock'd LLM + custom `init.lua` via `XDG_CONFIG_HOME` | `insta` snapshot of the JSONL `EngineEvent` stream                                   |
| **L3** | Visual rendering: layout chrome, dialogs, statusline, picker, transcript blocks, vim selection across wrap, theme switches, focus chain | Storybook — Rust stories that drive real `Ui` + real `LuaRuntime` + a `MockEngine`, render to `Grid` | `insta::assert_snapshot!` of the serialized `Grid` (text + sidecar styles)           |

Each layer tests what it owns. Don't drive engine events through `Buffer`
mutations (L1); don't build dialogs in Rust to test them (L3 — drive the Lua).

## L1 — model state (imperative unit tests)

Lives in `#[cfg(test)] mod tests` blocks next to the code under test:

- `crates/core/src/buffer.rs`, `cells.rs`
- `crates/tui/src/ui/window.rs`, `vim.rs`, `layout.rs`, `overlay.rs`,
  `text.rs`, `text_objects.rs`, `motions.rs`, `compositor.rs`, `flush.rs`
- `crates/tui/src/content/*.rs`

Most tests follow a small per-module `TestHarness` pattern: build the data
structure, dispatch primitives, assert on observable state.

```rust
#[test]
fn test_word_forward() {
    let mut h = TestHarness::new("hello world foo");
    h.handle(key('w'));
    assert_eq!(h.cpos, 6);
    h.handle(key('w'));
    assert_eq!(h.cpos, 12);
}
```

No event loop, no rendering. Pure data round-trip. The originally planned
Helix-style marker DSL (`("foo #[bar|]# baz", "diw", ...)`) didn't justify
itself once the imperative pattern was in place; the 100+ tests in `vim.rs`
read fine without it. **Open: revisit if a flood of cursor / selection
states starts repeating boilerplate.**

## L2 — engine integration (headless + wiremock)

Drive the `smelt` binary in headless JSON mode against a wiremock'd LLM. Each
scenario:

1. Spin up wiremock with canned SSE responses (cassettes).
2. Tempdir + `XDG_CONFIG_HOME` → write `init.lua` (registers test tools, sets
   permissions, etc.).
3. Run `smelt --headless --format json --no-tool-calling -m <model> <prompt>`.
4. Parse stdout as JSONL `EngineEvent`s and wait for `TurnComplete`.
5. Snapshot the JSONL event stream via `insta`.

Layout (top-level, since it spawns the workspace binary):

```
tests/
  common/
    harness.rs          # wiremock + tempdir + binary spawn
    mod.rs              # re-exports
  scenarios.rs          # one #[tokio::test] per scenario
  snapshots/            # insta defaults
```

Live scenarios as of 2026-05-06: `smoke_harness_starts`, `plain_turn`,
`thinking_then_text`, `streaming_concat_across_deltas`,
`provider_auth_error`, `incomplete_stream`. Five `.snap` files.

Deps (dev-only, in workspace root `Cargo.toml`): `wiremock`, `insta`,
`tempfile`.

The event stream shape lives in `protocol::EngineEvent` — that's the wire
contract the goldens pin. These snapshots are the practical "no feature
dropped" gate; they pin the externally visible headless stream rather than
persisted session state.

## L3 — Storybook (visual + integration)

Storybook supersedes the earlier L3a (`Grid::with_lines` + widget render)
and L3b (Pilot) sketches; neither landed. The new shape is one registry of
"stories" — Rust functions that render a specific UI state — exposed in
two modes over the same code: snapshot-tested in CI, browsable
interactively for authoring and exploration.

**Phase placement:** L3 is the future visual-test phase (post-P10).
Authoring lands once, stories accumulate forever. Two architectural
prerequisites identified during the 2026-05-06 audit are bundled into
phases that already touch the relevant surfaces:

- **P9.o.1 ✅** added a `UI_HOST` TLS slot holding `*mut dyn UiHost`
  alongside the existing concrete `APP` slot in
  `crates/tui/src/lua/app_ref.rs` (mirrors P8.f's Host-tier split).
  L3-comp stories can install through the trait-object slot
  without booting a full `TuiApp`. Existing UiHost-tier bindings
  still reach through `with_app(|app| ...)` — pulling them onto
  `with_ui_host` is mechanical and lands when L3 needs it.
- **P10.1 ✅** made `TuiApp::new` state-injectable (drops the internal
  `state::State::load()` call). Constructor takes `SessionCache` as
  a parameter; `main.rs` reads disk once via `startup::resolve` and
  threads it through. Story construction is no longer filesystem-coupled.

A third prerequisite (`EngineHandle::for_test() -> (Self, Sender, Receiver)`,
~10 LOC additive in `engine/lib.rs`) lands with L3 itself — no
overlap with current phases, no rework risk.

### Two sub-layers, one harness

| Sub-layer | Boots                                                                | Drives                                                  | Catches                                                       |
| --------- | -------------------------------------------------------------------- | ------------------------------------------------------- | ------------------------------------------------------------- |
| **L3-prim** | `Ui` only (no Lua, no engine). Pure Rust.                            | Buffers + Windows + LayoutTree + Overlay assembled directly | Solver bugs, wrap edge cases, chrome/border paint, scrollbar, theme resolve, hit-test math |
| **L3-comp** | `Ui` + `Cells` + real `LuaRuntime` (autoloads built-ins) + `MockEngine` | Story publishes a cell value or fires an `EngineEvent`; the **real Lua** subscriber renders | Component composition: dialogs, statusline, picker, transcript blocks, completer, vim selection across wrap |

L3-comp **never reimplements components in Rust to test them.** A confirm
dialog test publishes the `confirm_requested` cell; `dialogs/confirm.lua`
runs unmodified; the Grid is whatever the user would see.

### Story shape

Stories are flat Rust functions registered at link time. One file per
group under `crates/tui/examples/stories/stories/`.

```rust
#[story("dialogs/confirm/with_diff")]
fn confirm_with_diff(ctx: &mut StoryCtx) {
    ctx.lua_eval(r#"
        smelt.cell("confirm_requested"):set({
            handle_id = 1, tool_name = "edit_file",
            args = { path = "src/foo.rs", old_string = "...", new_string = "..." },
            reason = "edit src/foo.rs",
        })
    "#);
    ctx.tick();                      // drain Lua callback queue + render
    ctx.assert_snapshot();           // insta::assert_snapshot!
}

#[story("vim/visual_across_wrap")]
fn visual_across_wrap(ctx: &mut StoryCtx) {
    ctx.set_viewport(40, 12);        // narrow → forces wrap
    ctx.fill_buffer(VIM_BUF, LONG_LINE_WITH_CJK);
    ctx.press_seq("v$");
    ctx.tick();
    ctx.assert_snapshot();
}
```

`#[story]` is a small proc macro that:
1. Registers the function in an `inventory::submit!` block (interactive runner).
2. Emits a `#[test]` that constructs a fresh `StoryCtx`, runs the body,
   and panics on snapshot drift (CI runner).

**Stories are tests.** No parallel sets.

### `StoryCtx` API (sketch)

```rust
pub struct StoryCtx<'a> {
    pub ui:       &'a mut Ui,
    pub cells:    &'a mut Cells,
    pub theme:    &'a mut Theme,
    pub mock:     &'a mut MockEngine,
    pub lua:      Option<&'a mut LuaRuntime>,   // None for L3-prim stories
    pub viewport: (u16, u16),
    pub theme_preset: ThemePreset,
}

impl StoryCtx<'_> {
    // Setup
    pub fn buf(&mut self) -> BufId;
    pub fn open_window(&mut self, buf: BufId, opts: WinOpts) -> WinId;
    pub fn open_overlay(&mut self, layout: LayoutTree, anchor: Anchor, modal: Modal);
    pub fn set_viewport(&mut self, w: u16, h: u16);

    // Drive
    pub fn lua_eval(&mut self, code: &str);
    pub fn press(&mut self, key: &str);
    pub fn press_seq(&mut self, keys: &str);
    pub fn type_text(&mut self, text: &str);
    pub fn tick(&mut self);                       // one full event-loop iteration

    // Assert
    pub fn assert_snapshot(&mut self);            // implicit name = story id
    pub fn assert_snapshot_at(&mut self, label: &str);  // multi-step stories
}
```

### `MockEngine`

Same channel boundary as the real engine. Stories fan canned events into
`EngineClient`'s `event_rx` exactly like a provider would.

```rust
mock.text_delta("hello world");
mock.tool_started(call_id, "edit_file", args);
mock.tool_output(call_id, "wrote 42 lines");
mock.tool_finished(call_id, ToolResult::Ok(...));
mock.turn_complete(meta);
mock.token_usage(usage);
mock.request_permission(req);                 // triggers confirm dialog
mock.stream(LONG_TEXT, 4, Duration::from_millis(20));   // drip-feed
```

### Two runners over one registry

| Mode               | Command                                                | Purpose                                                                  |
| ------------------ | ------------------------------------------------------ | ------------------------------------------------------------------------ |
| **CI / drift gate** | `cargo nextest run --workspace`                        | Each story is a `#[test]`; insta fails the test on any unblessed change. |
| **Bless**          | `cargo insta review`                                   | Walk drifted stories, accept (`a`) or reject (`r`) per snapshot.         |
| **Explore**        | `cargo run --example stories`                          | 3-pane interactive TUI (groups / stories / preview) over the same registry. Theme cycle (Tab), width sweep (`+/-`), full-screen (Enter), per-story keymap layered above harness. Real terminal. |
| **Sweep**          | `cargo run --example stories -- dump`                  | Render all stories × theme × width in-process, write one HTML page. Eyeball drift across the matrix when changing themes / wrap. |

### Snapshot serialization

Each story's `Grid` serializes as plain text (one row per line) plus a
sidecar styles table mapping `(row, col, len) → HlGroup`. Two files per
story: `<story>.snap` (text) and `<story>.styles.snap` (table). Diffs
stay surgical — a colour-only change touches only the styles file; a
wrap regression touches only the text file. Both go through insta's
review flow.

### Coverage targets (initial story matrix)

| File                | Stories                                                                                      | What it hunts                                            |
| ------------------- | -------------------------------------------------------------------------------------------- | -------------------------------------------------------- |
| `dialogs.rs`        | confirm_with_diff, confirm_long_command, permissions, agents, picker_100, ask_user_question_4options | dialog stacking, focus chain, Tab-back-add-message       |
| `transcript.rs`     | text_long_streaming, code_block_rust, code_block_unclosed_at_turn_end, tool_block_bash, tool_block_web_fetch, diff_inline_truncated, mixed_blocks_25 | streaming → on_block, mid-block turn end, block spacing  |
| `vim.rs`            | visual_across_wrap, visual_line_with_emoji, dot_repeat, dd_undo, registers_named, paste_after_yank | wrap math, register text, undo grouping                  |
| `overlays.rs`       | cursor_anchor_completer, screen_center_modal, win_attach_top_toast, draggable_position       | anchor edges, z-order, hit-testing through overlays      |
| `theme.rs`          | all_presets_swatch, accent_swap_buffer_unchanged, light_dark_toggle, custom_ansi             | HlGroup interning, theme-switch-without-rewrite invariant |
| `layout.rs`         | vbox_chrome_borders, hbox_separator_styles, border_top_only, deeply_nested                   | chrome painting, gap inflation, constraint solver        |
| `statusline.rs`     | tokens_climb, spinner_phases, model_swap, mode_change_diff_payload                           | cell `(new, old)` payload routing                        |

### What L3 doesn't catch

- **Animation timing** at sub-tick resolution. `ctx.tick()` is one logical
  step; jitter on a real terminal isn't reproducible. Author one story per
  *frame* of the animation if it matters.
- **Real-terminal SGR quirks** (color profile differences, font kerning,
  curses bugs). Storybook tests `Grid` content, not what the terminal
  displays. Acceptable — those aren't smelt's bugs.
- **Mouse interactions** (drag-extend, scrollbar drag, click-promote).
  Synthesizing `MouseEvent`s through `ctx.dispatch(...)` works; visual
  confirmation is hard. Stories help; the tmux parity walk in P10 catches
  what they miss.

## Determinism rules

- **Fixed terminal size** per L3 test (the story decides; default 100×40).
  Coordinates become stable.
- **Freeze time** — mock `Instant::now` / `SystemTime::now`. Clock
  injection seam co-located with `Cells::tick`.
- **Pin `now` / `spinner_frame` cells** to known values per story.
- **`tokio::time::pause` + `advance`** — never real `sleep` in tests.
- **`insta` filters** — strip dynamic IDs, durations, paths, timestamps.
- **No real network** — wiremock only; CI fails on outbound HTTP.

## Sequencing across the refactor

| Phase                | What lands                                                                                               |
| -------------------- | -------------------------------------------------------------------------------------------------------- |
| **Pre-P0**           | L2 harness + 5–10 baseline scenarios on today's binary. Locked behaviour before demolition. ✅           |
| **P1–P5**            | L1 imperative tests landed alongside the code they cover (958 across the workspace). ✅                  |
| **P9 tail**          | L3 storybook crate skeleton + `StoryCtx` + `MockEngine` + initial L3-prim story matrix. Each subsequent UI bug-fix gets a story. |
| **P10**              | L3 story matrix is the parity gate. The tmux walk row in `ARCHITECTURE.md § Testing TUI changes` covers what L3 can't (mouse, real terminal). |

L2 was the parity gate for the demolition. L1 grew with each phase. L3
lands once: stories accumulate across phase boundaries, blessed once,
re-blessed only on intended changes.

## How to add a test

- **L1** — add `#[test]` next to the function under test. Imperative
  `TestHarness` style.
- **L2** — add a `#[tokio::test]` in `tests/scenarios.rs`; extend
  `tests/common/harness.rs` for new SSE shapes. Run `cargo insta review`.
- **L3** — add a `#[story("group/name")]` fn in
  `crates/tui/examples/stories/stories/<group>.rs`. Run `cargo insta review`
  to bless. `cargo run --example stories` to walk it interactively.

Run all: `cargo nextest run --workspace`. Review snapshot diffs:
`cargo insta review`.
