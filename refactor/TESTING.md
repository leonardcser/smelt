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
"stories" — Rust functions that render a specific UI state — snapshot-tested
in CI via `cargo nextest run --workspace`.

**Phase placement:** L3-prim landed during P10 alongside the parity walk
prep. L3-comp (Lua + MockEngine) lands when the first component story
needs it. All architectural prerequisites are in place:

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
- **P10.2 ✅** added `EngineHandle::for_test() -> (Self,
  Receiver<UiCommand>, Sender<EngineEvent>)` (~25 LOC additive in
  `engine/lib.rs`). Returns a handle whose channels are owned by the
  caller — no agent task spawned, no provider wiring. Drives L3-comp
  stories without booting a real engine.

### Two sub-layers, one harness

| Sub-layer | Boots                                                                | Drives                                                  | Catches                                                       |
| --------- | -------------------------------------------------------------------- | ------------------------------------------------------- | ------------------------------------------------------------- |
| **L3-prim** | `Ui` only (no Lua, no engine). Pure Rust.                            | Buffers + Windows + LayoutTree + Overlay assembled directly | Solver bugs, wrap edge cases, chrome/border paint, scrollbar, theme resolve, hit-test math |
| **L3-comp** | `Ui` + `Cells` + real `LuaRuntime` (autoloads built-ins) + `MockEngine` | Story publishes a cell value or fires an `EngineEvent`; the **real Lua** subscriber renders | Component composition: dialogs, statusline, picker, transcript blocks, completer, vim selection across wrap |

L3-comp **never reimplements components in Rust to test them.** A confirm
dialog test publishes the `confirm_requested` cell; `dialogs/confirm.lua`
runs unmodified; the Grid is whatever the user would see.

### Story shape

Stories are flat Rust functions registered at compile time via a
`macro_rules!` macro (no proc macro — keeps the dev-deps light). One
file per group under `crates/tui/tests/storybook/stories/`.

```rust
// crates/tui/tests/storybook/stories/layout.rs
story!(vbox_three_panes, |ctx| {
    ctx.set_viewport(40, 8);
    let top = ctx.buf_with_lines(["top pane"]);
    let bot = ctx.buf_with_lines(["bottom pane"]);
    let w_top = ctx.open_split(top, pane_config("top"));
    let w_bot = ctx.open_split(bot, pane_config("bot"));
    ctx.set_layout(LayoutTree::vbox(vec![
        (Constraint::Length(2), LayoutTree::leaf(w_top)),
        (Constraint::Length(2), LayoutTree::leaf(w_bot)),
    ]));
    ctx.assert_snapshot();
});
```

`story!` emits a `#[test]` that constructs a fresh `StoryCtx`, runs
the body, and panics on snapshot drift. The interactive
"explore"/"sweep" runners are deferred — neither has a current
consumer, both add a proc-macro + `inventory` dependency surface
that isn't paying for itself yet.

**Stories are tests.** No parallel sets.

### `StoryCtx` API (as landed)

L3-prim shape — no Lua, no engine. Stories that need component
composition will extend this.

```rust
pub struct StoryCtx {
    pub ui: Ui,
    name: String,
    snapshot_index: u32,
}

impl StoryCtx {
    pub fn new(name: &str) -> Self;
    pub fn set_viewport(&mut self, w: u16, h: u16);

    // Setup
    pub fn buf(&mut self) -> BufId;
    pub fn buf_with_lines<I, S>(&mut self, lines: I) -> BufId;
    pub fn open_split(&mut self, buf: BufId, config: SplitConfig) -> WinId;
    pub fn set_layout(&mut self, tree: LayoutTree);
    pub fn theme_mut(&mut self) -> &mut Theme;

    // Drive: stories reach into `ctx.ui` directly for `set_focus`,
    // `dispatch_event`, `overlay_open`, etc. No prescribed wrappers
    // until a flood of stories says otherwise.

    // Assert: writes `<name>.snap` (text rows) and
    // `<name>.styles.snap` (per-cell style sidecar). Multi-step
    // stories get auto-suffixed `step-1`, `step-2`, ….
    pub fn assert_snapshot(&mut self);
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

### Runners

| Mode               | Command                                                | Purpose                                                                  |
| ------------------ | ------------------------------------------------------ | ------------------------------------------------------------------------ |
| **CI / drift gate** | `cargo nextest run --workspace`                        | Each story is a `#[test]`; insta fails on any unblessed snapshot. Runs alongside the rest of the suite. |
| **Bless**          | `cargo insta review` or `INSTA_UPDATE=always cargo nextest run --workspace` | Walk drifted stories, accept (`a`) or reject (`r`) per snapshot, or auto-bless every story in one pass. |

Interactive ("Explore") and matrix-sweep runners are deferred until
a concrete consumer materializes (review the snapshot files
directly until then).

### Snapshot serialization

Each story's `Grid` serializes as plain text (rows joined by `\n`,
trailing whitespace stripped per row) plus a sidecar styles table
mapping `(row, col, len) → resolved Style`. Two files per story:
`<story>.snap` (text) and `<story>.styles.snap` (table); multi-step
stories add `.step-N` suffixes. Diffs stay surgical — a colour-only
change touches only the styles file; a wrap regression touches only
the text file. Both go through insta's review flow.

### Coverage (current — 58 stories across 6 groups)

L3-prim today. L3-comp groups (`dialogs.rs`, `transcript.rs`,
`statusline.rs`) land when the first story in each needs Lua + a
`MockEngine`.

| File          | Stories | Examples                                                                                              | What it hunts                                                |
| ------------- | ------- | ----------------------------------------------------------------------------------------------------- | ------------------------------------------------------------ |
| `buffer.rs`   | 10      | `cjk_double_width_glyphs_render`, `emoji_double_width_glyphs_render`, `mixed_ascii_and_cjk`, `decoration_fill_bg_paints_full_row`, `highlight_hl_eol_paints_to_line_end`, `highlight_range_paints_in_styles`, `multiple_highlights_same_line_layered`, `virt_text_eol_appends_after_content`, `line_longer_than_viewport_truncates`, `many_lines_more_than_viewport_height` | unicode width, layered highlights, hl_eol fill, virt_text positioning, decoration fill_bg |
| `chrome.rs`   |  6      | `border_single_with_title`, `border_rounded_with_title`, `border_double_with_title`, `border_single_no_title`, `border_none_omits_frame`, `title_truncates_in_narrow_border` | border style enum, title truncation, no-border passthrough   |
| `layout.rs`   | 14      | `vbox_three_panes`, `vbox_max_clamps_pane_height`, `vbox_min_competes_with_fill`, `vbox_percentage_split`, `vbox_nested_in_hbox`, `hbox_three_fill_split_evenly`, `hbox_ratio_split_one_to_two`, `hbox_two_columns_with_gap`, `nested_borders_inset_correctly`, `splits_paint_border_and_title`, … | chrome painting on splits + overlays, gap inflation, constraint solver |
| `overlays.rs` |  8      | `overlay_centered_modal_over_splits`, `two_overlays_stack_by_z`, `anchor_screen_at_topleft_corner`, `anchor_screen_at_topright_corner`, `anchor_screen_at_bottomleft_corner`, `anchor_screen_bottom_docked`, `anchor_win_attaches_above_target`, `anchor_clamped_when_offscreen` | anchor edges, z-order, clamping when off-screen              |
| `theme.rs`    |  7      | `default_theme_normal_fg`, `theme_link_a_to_b_resolves_b`, `theme_link_chain_three_hops`, `theme_link_cycle_falls_back_to_default`, `theme_swap_repaints_without_buffer_edit`, `theme_unknown_group_returns_default` | HlGroup interning, link chains, cycle fallback, theme-swap-without-rewrite invariant |
| `vim.rs`      | 13      | `normal_w_word_motion`, `normal_caret_jumps_to_first_nonblank`, `normal_dollar_jumps_to_eol`, `normal_gg_jumps_to_first_line`, `normal_count_prefix_3w`, `operator_dw_removes_word`, `operator_dd_removes_line`, `operator_2x_deletes_two_chars`, `operator_yy_then_p_pastes_below`, `visual_char_extends_with_l`, `visual_line_o_swaps_anchor`, `visual_line_paints_selection_bg`, `empty_buffer_normal_mode_no_panic` | motions, operators, visual-mode anchor flip, selection paint |

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
| **P1–P5**            | L1 imperative tests landed alongside the code they cover (956 across the workspace). ✅                  |
| **P10**              | L3-prim storybook lands: `StoryCtx`, `Ui::snapshot`, `EngineHandle::for_test`, 58 stories across 6 groups (`buffer / chrome / layout / overlays / theme / vim`) under `crates/tui/tests/storybook/`. Interactive viewer at `crates/tui/examples/stories.rs` reads blessed snapshots. ✅ The tmux walk row in `ARCHITECTURE.md § Testing TUI changes` covers what L3 can't (mouse, real terminal). |
| **post-P10**         | L3-comp lands when the first dialog / transcript / statusline story needs it. Adds `Cells` + `LuaRuntime` + `MockEngine` to `StoryCtx`. |

L2 was the parity gate for the demolition. L1 grew with each phase. L3
lands once: stories accumulate across phase boundaries, blessed once,
re-blessed only on intended changes.

## How to add a test

- **L1** — add `#[test]` next to the function under test. Imperative
  `TestHarness` style.
- **L2** — add a `#[tokio::test]` in `tests/scenarios.rs`; extend
  `tests/common/harness.rs` for new SSE shapes. Run `cargo insta review`.
- **L3** — add `story!(name, |ctx| { … })` in
  `crates/tui/tests/storybook/stories/<group>.rs` (new groups need a
  one-liner in `stories/mod.rs`). Run `cargo insta review` to bless;
  or `INSTA_UPDATE=always cargo nextest run --workspace` to bless
  everything in one pass.

Run all: `cargo nextest run --workspace`. Review snapshot diffs:
`cargo insta review`. Browse blessed L3 frames interactively:
`cargo run -p tui --example stories` (`j`/`k` navigate, `g`/`G`
jump, `q` or `Esc` quits).
