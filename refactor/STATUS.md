# Status

Where we are right now. Updated at session end.

For the entry point and meta-rules, read `README.md` first.

## Where we are

**Phase:** P1.a closed as **foundation laid**. Extmark + namespace
model + YankSubst + wrap_at + BufferParser rename are in place; the
remaining structural work (transcript-pipeline migration onto
`BufferParser` + `transcript_cache.rs` deletion + `edit_buffer.rs`
merge) is gated on the transcript renderers moving onto Buffer
first, which is itself a multi-session keystone. Deferred to a
dedicated P1.a-tail effort.

**P1.b LayoutTree complete.** All four sub-commits (B.1 → B.4)
landed in this session. `LayoutTree` is now in target shape:
`Vbox`/`Hbox`/`Leaf(WinId)` with explicit chrome (`gap`, `border`,
`title`) and `Items: Vec<(Constraint, LayoutTree)>`. Constraints
expanded to `Length`/`Percentage`/`Ratio`/`Min`/`Max`/`Fill`/`Fit`
with proper resolution semantics. `Direction` enum deleted.
`resolve_layout` now returns `HashMap<WinId, Rect>`.

**P1.c in progress** — sixteen commits landed (foundation
+ paint pipeline + first float migration + leaf event
routing). Foundation: `40f0c82`, `702305a`, `8fa6760`,
`d3c4a83`, `44fe779`, `434eee8`/`16ca777`, `7cee24c`,
`d94d12c`, `2713f01`/`50e2ba5`, `dcb0e8b`, `f80d1d0`
(target types + `resolve_anchor`; `Ui::overlay_*` API +
storage; per-frame resolution; chrome `SeparatorStyle`;
focus + hit-test primitives; canonical Win-typed focus
API; overlay/focus structural glue; unified `Ui::hit_test`;
modal-aware Tab cycling). Paint pipeline + first migration
+ leaf-event routing: `5a467d5`, `0836ae1`, `41432a8`,
`d77d513`, `0922dd0` (`Compositor::render_with` overlay
paint hook; minimal `Window::render(buf, slice, ctx)`;
`paint_chrome` + `Ui::render` walks resolved overlays after
layer paint; text_modal migrated to Overlay; modal-Esc-
dismiss built-in; `Ui::overlay_focus` field +
`overlay_for_leaf` helper + `set_focus` accepts overlay
leaves + `handle_key_with_lua` routes via `focus()`; q +
Ctrl+C dismiss restored on text_modal via leaf callbacks).
The P1.c data + resolution + focus + hit-test + paint +
event-routing layer is operational end-to-end — `/stats`
and `/cost` render as Overlays with full Esc/q/Ctrl+C
dismiss parity vs the old DialogConfig path. 68 P1.c unit
tests total, co-located with the code they cover.

**C.5 first migration shipped + parity restored.**
text_modal lives as `Overlay { layout: vbox(border+title,
hbox(leaf)), anchor: ScreenCenter, modal: true }`. Three
dismiss vectors:

1. **Esc** — Ui built-in (universal dismiss; fundamental,
   not user-customisable).
2. **`q`** — leaf callback registered in text_modal via
   `win_set_keymap`.
3. **`Ctrl+C`** — leaf callback registered in text_modal
   via `win_set_keymap`.

Both leaf callbacks call `ctx.ui.overlay_for_leaf(ctx.win)`
to resolve the containing overlay, then `overlay_close`.
This is the same shape every future overlay-based dialog
will use: register callbacks per leaf WinId, route through
`Ui::focus()` which prefers `overlay_focus`. No regressions
vs the prior `dialog_open` path.

**Window::render scope.** The pulled-forward helper is
read-only viewer scope: paints visible buffer lines from
`scroll_top` into a slice, no extmark highlights / no
transient selection / no scrollbar / no gutters. The full
surface (extmark layered paint + scrollbar + gutters +
selection paint) folds in alongside the `BufferView`
deletion in P1.d; the helper exists today as the first
concrete pin for that phase.

**C.6+ — remaining float migrations + deletions:** still
ahead. The paint pipeline can take more migrations now —
each remaining float (`/help`, `/ps`, `/resume`, `/agents`,
notification, picker, completer) needs its dialog
config + dismiss keys converted to Overlay shape, and
event routing for non-Esc keys is the gating piece.
`FloatConfig` / `PanelWidget` / `Placement` deletions
follow once every dialog flips.

Phase log: see `P1.md` for closed-sub-phase summary, decisions
made while coding, and per-section file/type changes.

**Tree:** green. `cargo nextest run --workspace` — 1021 passed
(14 new since C.4-tail₆: 4 `paint_chrome_*`, 3 `Window::render*`,
1 `render_with_paints_after_layers`, 1 `render_paints_overlay_leaf_buffer`,
2 `handle_key_esc_*` modal-dismiss, 3 `overlay_open_modal_focuses_*` /
`set_focus_accepts_overlay_leaf` / `handle_key_routes_to_overlay_leaf_callback`).
`cargo clippy --workspace --all-targets -- -D warnings` clean. Manual
TUI parity walk: `/stats` and `/cost` open as bordered+titled
centered modals; Esc, q, and Ctrl+C all dismiss; focus restores
to prompt.

**Last update:** 2026-04-29. P1.0 theme registry landing across 12
commits (`decb0ab`..`e489a79`):

- `decb0ab` — `ui::Theme` registry type (HashMap groups + links).
- `177ac4c` — plumbed through `DrawContext`; `Ui` owns it.
- `bb9cc63` — `populate_ui_theme()` mirrors host constants.
- `9bf1912` — first call site batch: render_loop, events,
  status_bar, ui_ops.
- `31cfb56` — renamed `crate::theme::Theme` snapshot → `Snapshot`.
- `d92a715` — status separator color + notification error_label.
- `da75d3a` — re-populate registry each frame so Lua-driven mutations
  propagate without a separate notification path.
- `387f4d2` — replaced `crate::theme::Snapshot` with `&ui::Theme` in
  the entire render pipeline; deleted the snapshot type.
- `1786716` — added `ColorRole::Agent / Success / ErrorMsg`; migrated
  `transcript_present/*` renderers off the const colors.
- `16beb71` — threaded `&ui::Theme` through `compute_prompt` /
  `reasoning_color` / `WindowView::draw_scrollbar`; migrated
  `confirm_preview` notebook title; added `ColorRole::Apply / Plan /
  Exec / Heading / ReasonLow / ReasonMed / ReasonHigh / ReasonMax`.
- `7aadcd2` — threaded `&ui::Theme` through
  `WindowView::set_soft_cursor` (last renderer-side `theme::is_light`
  caller).
- `e489a79` — collapsed atomics into `ui::Theme`. `accent`, `slug`,
  `is_light` live as fields with proper accessors; the per-frame
  `populate_ui_theme()` reads them and rewrites the `Smelt*` groups.
  Lua API closures access theme via `with_app(|app| app.ui.theme())`
  — no global state on either side. Inline ANSI helpers in
  `headless.rs` use literal `Color::Red` / `Color::AnsiValue(77)`
  rather than const aliases. Syntect's light/dark hint moved into
  `content/highlight/mod.rs` as a self-contained mirror updated by
  `populate_ui_theme()` (avoids threading `&Theme` through 14 syntax
  call sites for one branch).
- `50b8923` — **P1.a kickoff**: extmark + namespace model in
  `ui::Buffer`. New API: `create_namespace`, `set_extmark`,
  `del_extmark`, `clear_namespace`, `extmarks(ns)`. Single
  `BTreeMap<ExtmarkId, Extmark>` per namespace. The convenience
  methods `add_highlight` / `set_decoration` / `set_virtual_text` /
  `set_mark` create extmarks in the well-known namespaces
  (`NS_HIGHLIGHTS`, `NS_DECORATIONS`, `NS_VIRT_TEXT`, `NS_MARKS`).
  Per-line getters (`highlights_at`, `decoration_at`,
  `virtual_text_at`, `get_mark`) read directly from the extmark
  store. `highlights_arc` / `decorations_arc` materialize on demand
  with a `(changedtick + marks_tick)` cache so `BufferView::sync_from_buffer`
  still gets `Arc::clone` semantics. `sync_from_buffer` and
  `build_panels` switched to `&mut Buffer` / `&mut dyn BufferResolver`
  to thread the lazy materialization.
- `67603de` — `YankSubst` extmark field +
  `Buffer::yank_text_for_range(start_row, start_col, end_row, end_col)`.
  An extmark with `yank: Some(YankSubst::Empty)` elides covered
  bytes on yank; `Some(YankSubst::Static(s))` substitutes them.
  Walks every namespace, sorts substitutions in source order,
  emits literal text for uncovered bytes. No callers yet — building
  block for hidden-thinking elision and prompt attachment expansion.
- `26701d3` — `Buffer::wrap_at(width)` soft-wrap cache keyed by
  `(changedtick, width)`. Reuses the result across repeated calls
  and (eventually) across multiple Windows on the same Buffer.
  Cache invalidates on any line mutation. No callers yet — wrap
  state today still lives in WindowView; migration is downstream
  P1.a work.
- `385e9d0` — **`Buffer::attach(spec)` foundation commit 1**:
  `BufferFormatter` trait → `BufferParser`; `render` method →
  `parse`; `with_formatter` builder → `attach`; `set_formatter` →
  `set_parser`. New `BufferParser::on_attach(&mut Buffer)`
  lifecycle hook (default no-op) fires once when the parser is
  installed — entry point for parsers to register custom
  namespaces and seed initial state. Tests + 4 call sites updated
  (`format.rs` ModeFormatter → ModeParser, `lua/api/widgets.rs`,
  `lua/ui_ops.rs`). Pure rename + one new hook; no behavior
  change. Sets up the API shape for the deeper parser-hook surgery
  (incremental `on_change` / `on_render` hooks) without committing
  to the full restructure.

`crate::theme::*` is now narrow:
- `populate_ui_theme(&mut Theme)` — initializes `Smelt*` highlight
  groups from `theme.is_light()` + `theme.accent()` + `theme.slug()`.
- `detect_background(&mut Theme)` — OS-level OSC 11 / `$COLORFGBG`
  probe; sets `theme.set_light(...)` if successful.
- `PRESETS` — preset accent picker list (12 colors).
- `preset_by_name(&str) -> Option<u8>` — Lua API helper.
- `DEFAULT_ACCENT` re-export from `ui::theme::DEFAULT_ACCENT`.

P0 closed: 4 of 9 deletions shipped (orthogonal); 5 structural
deletions still deferred to later P1 sub-phases (paired with
replacements):
- `BufferView` deletion paired with `Window::render(buf, grid)` —
  P1.d.
- `PanelWidget` trait + dialog.rs panel multiplexing — P1.c.
- `Component` trait + remaining `WidgetEvent` — P1.d.
- `Placement` enum + `add_layer`/`register_split` plumbing — P1.b +
  P1.c.

**Note for next session:** puml + SVG are in sync. If the puml is
edited, regenerate via `plantuml -tsvg
refactor/tui-ui-architecture-target.puml`. Run `refactor/check.sh`
before declaring anything done.

## What's next

**Active phase:** P1.c — `Overlay` replacing `Float`. The
data + resolution + focus/hit-test layer is in place
(C.0–C.4 + tails); see "Where we are" above for the running
narrative and the open C.5 design point. Target shape from
`REFACTOR.md` § P1.c (for reference):

- `Overlay { layout: LayoutTree, anchor: Anchor, z: u16, modal: bool }`.
- `Anchor::{ ScreenCenter | ScreenAt { row, col, corner } |
  Cursor { corner, row_offset, col_offset } | Win { target, attach } }`.
- Drag = mutate the anchor.
- `Float` / `FloatId` / `FloatConfig` / `Placement` /
  `PanelWidget` trait / `dialog.rs` panel multiplexing — all
  deleted at C.6+ once C.5 lands.

## Deferred to P1.a-tail (after the transcript migration)

- Transcript-pipeline migration onto `BufferParser` (each `Block`
  kind becomes its own parser; `BlockArtifact` becomes per-block
  Buffer; `TranscriptSnapshot` composes from per-block Buffers).
- Then: `BufferParser::on_render` / `on_change` hooks gain
  consumers and stop being dead API.
- Then: `transcript_cache.rs` deletion.
- Edit history merge (`edit_buffer.rs` into `Buffer`): independent
  of the transcript track but multi-day on its own. Schedule
  alongside P1.d when vim state machine decomposes (the merge
  naturally pairs with the per-buffer history move described in
  P1.d).
- Drop `BufferView` `Arc::clone`: blocked on `BufferView` deletion
  in P1.d.

## Closed as "wrong fit" (don't revisit without restructuring)

- Hidden-thinking → `YankSubst::Empty`: requires re-rooting
  `TranscriptSnapshot` in a Buffer.
- Prompt attachment → `YankSubst::Static`: substitution happens at
  submit, not on copy.
- `WindowView` wrap → `Buffer::wrap_at`: rendering pre-wraps via
  `DisplayLine`s before the buffer.

Recently shipped: theme registry + plumbing + atomic-on-Theme
collapse + 42 call site migrations + Snapshot elimination + ColorRole
expansion + Buffer extmark + namespace model.
P0 orthogonal deletions
(`selection_style`/`set_selection_bg` shim, `handle_mouse_with_lua` +
`classify_widget_action`, `MouseAction::Yank`/`WidgetEvent::Yank`,
`BufferList`). `TESTING.md` (three-layer testing strategy). Test
harness + 5 baseline scenarios (`plain_turn`, `thinking_then_text`,
`incomplete_stream`, `provider_auth_error`, `streaming_concat_across_deltas`).
`TRACE.md` vertical-slice walk-through.

## Open questions / blocked

(Rows from `INVENTORY.md` that need a decision before their phase
begins. Move resolved items down to a "decisions" log here or in the
relevant `P<n>.md`.)

- **`commands.lua` shape** — P4.e. One master file vs one-per-command in
  `plugins/`. Decide while coding P4.e.
- **`render_ops.rs` per-language split** — P3.b. One `parse.rs` binding,
  or per-language (`diff.rs`/`syntax.rs`)? Decide while coding P3.b.
- **`prompt_picker.lua`** — P4.a. Merge into `widgets/picker.lua` or stay
  separate as `widgets/prompt_picker.lua`? Decide while coding P4.a.
- **`predict.lua` location** — P4. Stay in `plugins/` (it's a hook), or
  move under a new `hooks/` dir? Decide while coding P4.
- **`plan_mode.lua` split** — P4/P5. Tool half → `tools/exit_plan_mode.lua`,
  hook half → `modes.lua` or `plugins/`. Confirm split shape when P4/P5
  begin.

None of these block the start of P0 or P1.

## Recently landed

(Phases as they complete. Newest first. One line each, link to the
P-log.)

- **P1.b — `LayoutTree`.** `Vbox`/`Hbox`/`Leaf(WinId)` + 7
  `Constraint` variants + `Chrome` (gap/border/title/separator);
  `resolve_layout` keyed by `WinId`. See `P1.md` § P1.b.
- **P1.0 — Theme registry + first paired structural deletion.**
  `ui::Theme` (groups + links) replaces `crate::theme::*` atomic
  globals; populated each frame from `is_light` / `accent` / `slug`.
  See `P1.md` § P1.0.
- **P1.a foundation.** Buffer extmark + namespace model;
  `YankSubst`; `wrap_at` cache; `BufferParser` rename + `on_attach`
  hook. Structural completion deferred to P1.a-tail. See `P1.md`
  § P1.a.
- **P0 — Clear the deck (orthogonal half).** 4 of 9 deletions
  shipped; 5 structural deletions deferred to P1.0. Tree green; 901
  workspace tests + 6 baseline scenarios passing. See `P0.md`.

## How to keep this file current

- At session end: update "Where we are", "What's next", "Open
  questions" — even one line is enough.
- When a phase boundary lands: rotate the line into "Recently landed",
  reset "What's next."
- When a decision is made: log it in the active `P<n>.md`; if it
  changes scope, also update `REFACTOR.md` in the same commit.

(For meta-rules and the cold-start checklist, see `README.md`.)
