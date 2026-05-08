//! Frontend-neutral style primitives.
//!
//! Pure-data leaf crate. No terminal deps, no async runtime. Document
//! models (e.g. `smelt-buffer`) carry `Style` / `HlGroup` on their
//! span payloads; renderers (e.g. `smelt-term`) resolve `HlGroup` to
//! a concrete `Style` at paint time via [`theme::Theme::resolve`].
//!
//! Host-specific role mappings (e.g. `role_hl("Accent") →
//! "SmeltAccent"`) live in higher-level crates; this crate only
//! carries the generic interner + Theme machinery.

pub mod style;
pub mod theme;
