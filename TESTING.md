# Testing Guide

This project uses several test layers. Pick the lowest layer that exercises the behavior faithfully.

## 1. Unit tests

Use unit tests for pure functions and small modules. They should not construct a full `TuiApp`.

Good fits:

- permission rule matching,
- provider request/stream parsing,
- text and UTF-8 boundary helpers,
- markdown/span parsing,
- small state machines.

## 2. Component tests

Use component tests when behavior needs real editor, buffer, window, layout, or grid machinery, but not a full app.

Good fits:

- `smelt_edit::Ui`, windows, buffers, cursor and selection behavior,
- vim motions/operators,
- terminal grid mutation/diff behavior,
- content rendering primitives.

## 3. App integration tests

Use `crate::app::test_harness::TestApp` for cross-cutting `TuiApp` behavior.

Good fits:

- prompt + engine + session interactions,
- Lua reload and TUI API integration,
- overlays, dialogs, pickers, pane focus,
- compaction and context-window behavior,
- event dispatch behavior that depends on app state.

Do not add concrete regressions directly to the harness implementation. Put them in topical app test modules and keep the harness focused on reusable driving/probing/invariant helpers.

## 4. Visual/storybook snapshots

Use storybook snapshots when the important assertion is what appears on screen.

Good fits:

- transcript block rendering,
- prompt rendering,
- dialogs and permissions UI,
- overlays,
- style/theme regressions,
- visible vim/editor sequences.

Avoid putting hidden business-logic assertions in storybook tests unless the visual output is the behavior being pinned.

## 5. Subprocess/headless integration tests

Use the binary + mocked provider/network harness when the seam under test crosses process, CLI, config, or provider boundaries.

Good fits:

- CLI argument/config resolution,
- provider registration from `init.lua`,
- mocked HTTP request/response behavior,
- headless JSON output shape.

## 6. Fuzz and fuzz regression replay

Use fuzzing for panic discovery, invariant checking, and broad state-space exploration. Fuzz targets should keep deterministic replay paths for found bugs.

Good fits:

- arbitrary TUI event/engine event sequences,
- Lua API lifecycle and resource handling,
- UTF-8 byte-offset mutation surfaces,
- provider parser robustness,
- permission rule combinations.

Every fuzz-found bug should get a committed seed under `fuzz/seeds/<target>/regression/`.

## Choosing a layer

- If a pure function can cover it, write a unit test.
- If real edit/layout/render components are enough, avoid `TestApp`.
- If behavior depends on app-level state, use `TestApp`.
- If the user-visible frame matters, prefer storybook.
- If config/CLI/provider process seams matter, use subprocess integration.
- If the state space is large or history-dependent, add fuzz coverage and commit regression seeds for bugs.
