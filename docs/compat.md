# Compatibility debt

Compatibility code we intend to remove while Smelt is alpha. Mark matching code
with `COMPAT(<id>)`.

## session-search-sidecar-missing

- Remove after: old session dirs without `meta.json` / `content.txt` no longer
  matter
- Why: rebuild list metadata and search text from canonical SQLite when derived
  cache files are missing
- Code:
  - `crates/core/src/session.rs`: session listing/search blob fallbacks

## transcript-descriptor-history-link-mismatch

- Remove after: sessions saved by early sparse transcript append builds no longer
  need to resume in supported versions
- Why: detach transcript descriptor `history_idx` / `origin_json` values that
  point at a missing or wrong-kind history row, such as a user descriptor linked
  to a context note after sparse resume
- Code:
  - `crates/core/src/session.rs`: store-header load and SQLite session repair run
    the bounded repair before sparse transcript resume
  - `crates/store/src/history.rs`: repair scans linked descriptor rows and
    clears only mismatched history links, leaving descriptor content intact
- Tests:
  - `repair_mismatched_transcript_descriptor_history_links_detaches_bad_links`
  - `load_store_header_for_dir_repairs_mismatched_transcript_descriptor_history_links`

## session-checkpoint-live-index-past-history

- Remove after: sessions saved by early sparse resume builds with stale
  checkpoint state no longer need to resume in supported versions
- Why: repair `checkpoint_json.first_live_index` values that point past
  `session_state.history_len`, which made resumed model history contain only the
  checkpoint summary and omit all retained SQLite history rows
- Code:
  - `crates/core/src/session.rs`: store-header load and SQLite session repair run
    the bounded repair before sparse model-history resume; read-only metadata
    load also clamps impossible checkpoint coordinates in memory
  - `crates/store/src/meta.rs`: repair rewrites impossible checkpoint live starts
    to `0`, preserving the summary while replaying all retained rows, and state
    validation rejects new checkpoint writes past `history_len`
- Tests:
  - `repair_checkpoint_first_live_index_past_history_replays_retained_rows`
  - `repair_checkpoint_first_live_index_past_actual_history_rows`
  - `session_state_rejects_checkpoint_first_live_index_past_history`
  - `load_store_header_for_dir_repairs_checkpoint_first_live_index_past_history`
  - `store_backed_resume_repairs_checkpoint_that_points_past_retained_history`

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
