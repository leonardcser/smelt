# Compatibility debt

Compatibility code we intend to remove while Smelt is alpha. Mark matching code
with `COMPAT(<id>)`.

## branch-sqlite-schema-shape-repair

- Remove after: transcript virtualization branch-local databases have either been repaired/reimported or the branch lands with a real schema migration boundary
- Why: recover local `session.db` files created by earlier iterations of the unreleased transcript virtualization branch that used `user_version = 1` with older table shapes
- Code:
  - `crates/store/src/schema.rs`: same-version schema shape repair before running canonical `SCHEMA`
- Tests:
  - `migrate_repairs_in_place_version_one_session_state_schema`
  - `read_only_validation_rejects_same_version_wrong_shape`

## session-v1-messages

- Remove after: two alpha releases after session schema v2 ships
- Why: load old `session.json` files that stored provider-style `messages`
- Code:
  - `crates/core/src/session.rs`: `SessionWire`, message-index remapping, legacy
    accounting snapshots
  - `crates/protocol/src/history.rs`: `history_from_messages`, legacy
    note-prefix decoding
- Tests:
  - `legacy_messages_session_loads_with_no_user_display`
  - `legacy_session_with_orphan_tool_use_is_repaired_on_deserialize`
  - `legacy_session_with_token_snapshots_loads_without_error`

## session-json-monolith

- Remove after: two alpha releases after SQLite session storage ships
- Why: import old monolithic `session.json` files to canonical SQLite storage on
  open or background migration, then remove the monolith
- Code:
  - `crates/core/src/session.rs`: `read_legacy_json_session`,
    `migrate_legacy_json_session`
  - `crates/core/src/session_migration.rs`: `migrate_session_dir_to_db`

## session-split-jsonl

- Remove after: two alpha releases after SQLite session storage ships
- Why: import pre-SQLite `meta.json` + `history.jsonl` session directories to
  canonical SQLite storage without requiring the user to open each session
- Code:
  - `crates/core/src/session.rs`: `read_jsonl_session`
  - `crates/core/src/session_migration.rs`: `migrate_session_dir_to_db`,
    `migrate_all_sessions_once`

## session-search-sidecar-missing

- Remove after: old session dirs without `meta.json` / `content.txt` no longer
  matter
- Why: rebuild list metadata and search text from canonical SQLite, surfacing
  pending migration status for legacy inputs without loading them directly
- Code:
  - `crates/core/src/session.rs`: session listing/search blob fallbacks

## legacy-session-full-load-fallbacks

- Remove after: SQLite session metadata, history, and transcript descriptors are required for all supported saved sessions
- Why: explicit open/preview paths still need to display old or partially migrated sessions that lack sparse transcript records or history length metadata
- Code:
  - `crates/tui/src/app/lua_handlers.rs`: fallback in `load_session_by_id`, counted by `compat:session:load_full_fallback`
  - `crates/tui/src/lua/api/session.rs`: fallback in `smelt.session.render_preview_into`, counted by `compat:session:preview_full_fallback`
  - `crates/tui/src/app/history.rs`: fallback transcript rebuild in `rebuild_screen_from_history`, counted by `compat:session:rebuild_transcript_full_fallback`

## transcript-deferred-full-descriptor-bridge

- Remove after: normal session resume opens metadata and sparse transcript windows without `load_full` / `load_full_session_snapshot`
- Why: deferred load currently validates the display transcript against a fully materialized semantic session, so it may need a full descriptor merge as a temporary repair bridge
- Code:
  - `crates/tui/src/app/transcript.rs`: `legacy_merge_full_descriptor_slice_for_deferred_load`
  - `crates/tui/src/app/history.rs`: deferred session load fallback, counted by `compat:session:deferred_load_full`

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
