# Compatibility debt

Compatibility code we intend to remove while Smelt is alpha. Mark matching code
with `COMPAT(<id>)`.

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

## transcript-record-history-link-mismatch

- Remove after: sessions saved by early sparse transcript append builds no longer
  need to resume in supported versions
- Why: detach transcript record `history_idx` / `origin_json` values that point at
  a missing or wrong-kind history row, such as a user record linked to a context
  note after sparse resume
- Code:
  - `crates/store/src/access.rs`: `SessionMaintenance` exposes the repair only
    while holding exclusive session ownership
  - `crates/store/src/history.rs`: repair scans linked transcript records and
    clears only mismatched history links, leaving block content intact
- Tests:
  - `maintenance_repair_preserves_semantic_links_and_detaches_mismatches`

## transcript-preserve-order-content-hash

- Remove after: sessions written while transcript hashes depended on JSON object
  insertion order no longer need to open in supported versions
- Why: enabling `serde_json/preserve_order` made tool argument hashes depend on
  randomized `HashMap` iteration order, so valid persisted tool blocks could fail
  hydration after deserialization
- Code:
  - `crates/core/src/transcript_model.rs`: validates the legacy hash against the
    exact persisted block JSON, then upgrades it in memory to the canonical hash
- Tests:
  - `legacy_order_dependent_block_hash_hydrates_as_canonical`
  - `hydration_rejects_block_json_that_does_not_match_content_hash`
  - `resumed_turn_hydrates_legacy_multi_arg_tool_record_suffix_for_save`

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
- Why: a pre-v6 binary coordinates through `<session>/session.lock`. Any
  supported older schema may still be owned by such a binary, so migration
  conservatively acquires the stable root lock first and the legacy lock second,
  then holds both through migration and any writer-owner claim
- Code:
  - `crates/store/src/access.rs`: migration-only `LegacySessionLock`
- Tests:
  - `legacy_lock_holder_blocks_root_lease_migration`

## storage-v2-wide-transcript-search

- Remove after: schema v2 sessions created by early alpha builds no longer need
  to open in supported versions
- Why: one historical v2 shape added three byte-count columns to
  `transcript_search`. The canonical v2-to-v3 rebuild retains only `block_idx`,
  `history_idx`, and `indexed_text`, so migration must select those columns
  explicitly instead of copying all six by position
- Code:
  - `crates/store/src/schema.rs`: v2 `transcript_search` rebuild
- Tests:
  - `v2_wide_transcript_search_migration_preserves_data_and_removes_dead_schema`

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
  - `v2_wide_transcript_search_migration_preserves_data_and_removes_dead_schema`
