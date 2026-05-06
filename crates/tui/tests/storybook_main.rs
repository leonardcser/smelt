//! L3 storybook — visual snapshot tests over the real `Ui`.
//!
//! Each story builds a fresh `Ui`, drives it into a target visual
//! state, and snapshots the rendered frame via `insta`. Stories are
//! expressed as flat functions taking `&mut StoryCtx`; the
//! [`story!`] macro wraps each into a `#[test]` so
//! `cargo nextest run --workspace` runs them as part of the regular
//! gate. See `refactor/TESTING.md § L3` for the broader plan.

#[macro_use]
mod storybook;
