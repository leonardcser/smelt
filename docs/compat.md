# Compatibility debt

Compatibility code we intend to remove while Smelt is alpha. Mark matching code
with `COMPAT(<id>)`.

## session-derived-sidecar-exports

- Remove after: all supported smelt versions and bundled Lua/UI consumers read
  session lists from `catalog.db` and session content/search from canonical
  SQLite, at least two alpha releases have shipped with revision-stamped exports
  deprecated, and no supported external import/export contract requires
  `meta.json` or `content.txt`
- Why: keep revision-stamped `meta.json` and `content.txt` as best-effort exports
  for alpha-era external tooling while canonical state lives only in
  `session.db`
- Code:
  - `crates/core/src/session_exports.rs`: bounded process exporter, revision-pinned
    format generation, cross-process locking, and atomic streaming writes
  - `crates/core/src/session.rs`: offline-save scheduling, explicit maintenance
    export, and stale temporary-file cleanup
  - `crates/tui/src/persist.rs`: post-commit compatibility export scheduling
  - `crates/tui/src/app/history.rs`: post-publication fork export scheduling
  - `src/main.rs`: explicit `session rebuild-derived` export command
  - `crates/store/src/db.rs` and `crates/store/src/history.rs`: revision-pinned
    snapshot and cancellable row streaming
- Tests:
  - compatibility exports are revision-stamped and rebuilt from canonical SQLite
  - stale or failed compatibility exports never affect list, search, resume, or
    model dispatch

## legacy-attachment-blobs

- Remove after: sessions written before attachment objects no longer need to be
  opened by supported versions
- Why: hydrate verified `blob:<hash>.<ext>` image references from the private
  `blobs/` directory while new saves store exact data URLs transactionally in
  SQLite objects
- Code:
  - `crates/store/src/access.rs`: bounded, hash-verified compatibility hydration
- Tests:
  - `legacy_attachment_blobs_remain_readable_and_missing_blobs_are_explicit`

## transcript-descriptor-history-link-mismatch

- Remove after: sessions saved by early sparse transcript append builds no longer
  need to resume in supported versions
- Why: detach transcript descriptor `history_idx` / `origin_json` values that
  point at a missing or wrong-kind history row, such as a user descriptor linked
  to a context note after sparse resume
- Code:
  - `crates/store/src/access.rs`: `SessionMaintenance` exposes the repair only
    while holding exclusive session ownership
  - `crates/store/src/history.rs`: repair scans linked descriptor rows and
    clears only mismatched history links, leaving descriptor content intact
- Tests:
  - `repair_mismatched_transcript_descriptor_history_links_detaches_bad_links`

## session-checkpoint-live-index-past-history

- Remove after: sessions saved by early sparse resume builds with stale
  checkpoint state no longer need to resume in supported versions
- Why: repair `checkpoint_json.first_live_index` values that point past
  `session_state.history_len`, which made resumed model history contain only the
  checkpoint summary and omit all retained SQLite history rows
- Code:
  - `crates/core/src/session.rs`: read-only metadata loads clamp impossible
    checkpoint coordinates in memory without changing SQLite
  - `crates/store/src/access.rs`: `SessionMaintenance` exposes the repair only
    while holding exclusive session ownership
  - `crates/store/src/meta.rs`: repair rewrites impossible checkpoint live starts
    to `0`, preserving the summary while replaying all retained rows, and state
    validation rejects new checkpoint writes past `history_len`
- Tests:
  - `repair_checkpoint_first_live_index_past_history_replays_retained_rows`
  - `repair_checkpoint_first_live_index_past_actual_history_rows`
  - `session_state_rejects_checkpoint_first_live_index_past_history`
  - `store_backed_resume_tolerates_bad_checkpoint_without_repairing_database`

## storage-root-lease

- Remove after: schema versions older than v6 are no longer supported for
  writable open or migration
- Why: a pre-v6 binary coordinates through `<session>/session.lock`, so migration
  must acquire the stable root lock first and the legacy lock second, then hold
  both through schema migration and owner-token claim
- Code:
  - `crates/store/src/access.rs`: migration-only `LegacySessionLock`
- Tests:
  - `legacy_lock_holder_blocks_root_lease_migration`

## session-writer-lease-metadata

- Remove after: sessions last written by pre-lock alpha builds no longer need to
  open in supported versions
- Why: remove the obsolete timed `writer_lease` metadata row when claiming the
  lifetime operating-system lock and fenced writer ownership
- Code:
  - `crates/store/src/meta.rs`: writer ownership claim removes the legacy row

## request-audit-zero-based-attempts

- Remove after: schema v2/v3 sessions written by zero-based request-audit
  producers no longer need to open in supported versions
- Why: shift each persisted request attempt from zero-based to one-based during
  the v4 schema rebuild, preserving retry order while satisfying the hardened
  `attempt >= 1` invariant
- Code:
  - `crates/store/src/schema.rs`: v2/v3 to v4 request-attempt migration
- Tests:
  - `v2_to_v6_migration_preserves_data_and_removes_dead_schema`

## allowed-session-full-materialization

- Scope: full session materialization is allowed only for explicit
  detail/export/debug-style operations and tests
- Why: normal resume, render, save, search, rewind, fork, and Lua lightweight
  APIs must remain store-backed and bounded
- Code:
  - `crates/tui/src/app/history.rs`: `FullSessionMaterializationReason`
    documents all TUI wrapper reasons and records per-reason counters
  - `crates/tui/src/inspect_server.rs`: inspect session detail endpoint
    intentionally materializes a single requested session, counted by
    `inspect:session:detail_load_full`

## transcript-group-blocks-alias

- Remove after: two alpha releases after transcript group snapshots consistently
  use `children`
- Why: keep older Lua transcript renderers and saved/custom group snapshots that
  still read or provide `blocks` working during the group schema rename
- Code:
  - `runtime/lua/smelt/transcript/defaults.lua`: `group_children` fallback from
    `children` to legacy `blocks`

## openai-reasoning-summary-shape

- Remove after: sessions with object/string OpenAI reasoning summaries no longer
  matter
- Why: normalize old saved Responses reasoning `summary` shapes to the current
  array shape
- Code:
  - `crates/engine/src/provider/openai.rs`: `normalize_openai_reasoning_item`
- Tests:
  - `build_body_wraps_legacy_reasoning_summary_object`
  - `parse_response_normalizes_legacy_reasoning_summary_object`
