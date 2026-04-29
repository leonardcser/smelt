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

**P1.c in progress** — eight foundation commits landed
(`40f0c82`, `702305a`, `8fa6760`, `d3c4a83`, `44fe779`,
`434eee8`/`16ca777`, `7cee24c`, `d94d12c`): `Corner` rename;
target `Anchor` + `Overlay` + `OverlayId` types in a new
`overlay` module; pure `resolve_anchor` function with 9 tests;
`Ui::overlay_open / close / overlay / overlay_mut /
overlays_in_z_order` API + storage with 4 tests; per-frame
resolution layer (`LayoutTree::natural_size((cap))` + 9 tests;
`Ui::resolve_overlays(cursor)` returning z-ordered
`Vec<(OverlayId, Rect, &Overlay)>` + 4 tests); `SeparatorStyle`
on `Chrome` (None/Solid/Dashed; `with_separator` auto-inflates
gap from 0→1) + dedup of the dialog-side enum (one definition
in `layout`, used everywhere); focus + hit-testing primitives
(`Ui::active_modal()`, `OverlayHitTarget::{Window | Chrome}`,
`Ui::overlay_hit_test(row, col, cursor)` — modal-aware,
topmost-first) + 9 tests; canonical Win-typed focus API
(`Ui::focus()`, `set_focus(WinId) -> bool`, `focus_history`
slice; pushes prior focus on switch, no-op on same-win) + 6
tests. The whole P1.c data + resolution + focus/hit layer is
in place — callers can ask "where does this overlay draw,
what's modal, what was clicked, who's focused?" without touching
the compositor.

**Next in P1.c (open design point):** C.5 — first float migration
to Overlay — has a render-shape question that needs deciding
before code lands. Current state:

- Overlay storage holds `LayoutTree<WinId>`. `resolve_overlays`
  yields `(OverlayId, Rect, &Overlay)` per frame. ✅
- `Window::render(buf, grid)` doesn't exist yet — paint flows
  through `BufferView::draw(rect, slice, ctx)` invoked from
  `Dialog::draw` for buffer panels and from compositor layers
  (`Component::draw`) for floats. P1.d is the phase that flips
  Window into the unified render unit.
- `Compositor` owns `current` / `previous` grids privately;
  external callers can't paint into the active frame.

Three plausible paths, none clean today:

1. **Pull a minimal `Window::render(buf, slice, ctx)` helper
   forward from P1.d** scoped to read-only viewer Windows.
   Lets text_modal land as Overlay { layout: vbox(border+title,
   leaf(WinId)), anchor: ScreenCenter }. Overlay paint walks
   `resolve_layout`, paints chrome on container nodes, calls the
   helper on each leaf. Still needs Compositor to expose its
   `current` grid (small API addition).
2. **Add a temporary `Overlay::Component` carrier variant** that
   stores `Box<dyn Component>` alongside the `LayoutTree`. text_modal
   passes a Dialog Component; Overlay holds the existing-shape
   wrapper. Forbidden by meta-rules (parallel implementations).
3. **Defer C.5 to land paired with P1.f** (`Ui` facade rewrite —
   when overlays paint as peers of splits in a unified render
   pass owned by Ui). Delays float deletion but avoids
   scaffolding.

Recommendation: **path 1**, with the Window-render helper extracted
deliberately as a P1.c → P1.d hand-off. The helper is small (sync
buffer view + paint visible lines + scrollbar — most of the work
is already in `BufferView::draw`); landing it in C.5 turns it into
the first concrete pin for P1.d.

Until that shape is decided, C.5 doesn't ship code. C.6+ (delete
`FloatConfig` / `PanelWidget` / `Placement`) is gated on C.5.

Phase log: see `P1.md` for closed-sub-phase summary, decisions
made while coding, and per-section file/type changes.

**Tree:** green. `cargo nextest run --workspace` — 989 passed
(6 new since C.4-tail₂: focus / set_focus / focus_history
covering fresh-Ui / unknown-win / registered-split / prior
push / same-win no-op / 4-step chain). `cargo clippy
--workspace --all-targets -- -D warnings` clean.

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
