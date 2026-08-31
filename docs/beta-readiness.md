# Beta and 1.0 Readiness

This list tracks the work that remains after P0 release hardening. `0.x` releases
use normal versions and tags such as `0.6.0` and `v0.6.0`; beta maturity is not
encoded with a `-beta` suffix.

## Beta blockers

- Refine the capability-oriented Supported facade and keep implementation-only
  hooks Internal. Supported and Advanced are provisional guidance during alpha,
  not a compatibility freeze of the broad callable surface.
- Complete the [Lua API boundary](lua-api-boundary.md), generated reference, and
  [plugin guide](docs/guide/plugins.md) so authors do not need Rust or bundled Lua
  source to understand normal usage, failures, ownership, or runtime tiers.
- Add external-style coverage for major Supported capabilities across cold start,
  successful reload, failed candidate rollback, removal, and repeated cycles.
- Verify deterministic cleanup and cancellation for generation-owned commands,
  tools, keymaps, hooks, providers, timers, tasks, windows, and overlays.
- Keep headless, provider, tool, reload, shutdown, and engine-disconnect outcomes
  consistent and covered end to end.
- Build documentation in strict mode and resolve warnings and broken links.

`smelt.api_version == "1"` remains the alpha API identifier. Breaking alpha
cleanup updates declarations and generated artifacts in place rather than
incrementing the API version or maintaining a cross-base inventory.

## Before 1.0

- Define a narrow compatibility and release-note policy for the intentional
  public surface.
- Normalize errors, cancellation, indexing, optional values, and resource
  ownership across that surface.
- Declare supported operating systems, architectures, Rust toolchain policy,
  terminal assumptions, and Lua version, with CI coverage where practical.
- Align crate and module boundaries with runtime ownership without preserving
  accidental seams through compatibility shims.
- Add performance budgets and fuzz coverage for startup, long transcripts,
  reload, persistence, provider streams, text mutation, and Lua metadata.
- Extend the immutable release process in [RELEASE.md](../RELEASE.md) with stronger
  provenance, signing or attestations, and compromised-release procedures.
- Keep the trust and reporting model in [SECURITY.md](../SECURITY.md) current.
