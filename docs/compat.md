# Compatibility debt

Compatibility code we intend to remove while Smelt is alpha. Mark matching code
with `COMPAT(<id>)`.

## session-v1-messages

- Remove after: two alpha releases after session schema v2 ships
- Why: load old `session.json` files that stored provider-style `messages`
- Code:
  - `crates/core/src/session.rs`: `SessionWire`, message-index remapping, legacy
    accounting snapshots, and `context_snapshots` field aliasing
  - `crates/protocol/src/history.rs`: `history_from_messages`, legacy
    note-prefix decoding
  - `crates/store/src/legacy.rs`: provider-message import when old
    monolithic `session.json` files lack native `history` rows
- Tests:
  - `legacy_messages_session_loads_with_no_user_display`
  - `legacy_session_with_orphan_tool_use_is_repaired_on_deserialize`
  - `legacy_session_with_token_snapshots_loads_without_error`

## session-json-monolith

- Remove after: two alpha releases after SQLite session storage ships
- Why: import old monolithic `session.json` files to canonical SQLite storage on open or background migration, then remove the monolith
- Code:
  - `crates/core/src/session.rs`: `read_legacy_json_session`,
    `migrate_legacy_json_session`, monolithic-session prefix matching, and
    migrated artifact cleanup
  - `crates/core/src/session_migration.rs`: `migrate_session_dir_to_db` and
    legacy-sidecar directory classification
  - `crates/store/src/legacy.rs`: `session.json` metadata/history import

## session-split-jsonl

- Remove after: two alpha releases after SQLite session storage ships
- Why: import pre-SQLite `meta.json` + `history.jsonl` session directories to
  canonical SQLite storage without requiring the user to open each session
- Code:
  - `crates/core/src/session.rs`: `read_jsonl_session`, split-session prefix
    matching, and migrated artifact cleanup
  - `crates/core/src/session_migration.rs`: `migrate_session_dir_to_db`,
    `migrate_all_sessions_once`, and legacy-sidecar directory classification
  - `crates/store/src/legacy.rs`: `meta.json` + `history.jsonl` import

## session-search-sidecar-missing

- Remove after: old session dirs without `meta.json` / `content.txt` no longer
  matter
- Why: rebuild list metadata, search text, and missing transcript descriptor rows from canonical SQLite, surfacing pending
  migration status for legacy inputs without loading them directly
- Code:
  - `crates/core/src/session.rs`: session listing/search blob fallbacks and SQLite
    session repair during background migration

## legacy-session-full-load-fallbacks

- Remove after: SQLite session metadata, history, and transcript descriptors are required for all supported saved sessions
- Why: explicit open/preview paths still need to display old or partially migrated sessions that lack sparse transcript records or history length metadata
- Code:
  - `crates/tui/src/app/lua_handlers.rs`: fallback in `load_session_by_id`, counted by `compat:session:load_full_fallback`
  - `crates/tui/src/lua/api/session.rs`: fallback in `smelt.session.render_preview_into`, counted by `compat:session:preview_full_fallback`
  - `crates/tui/src/app/history.rs`: fallback transcript rebuild in `rebuild_screen_from_history`, counted by `compat:session:rebuild_transcript_full_fallback`

## transcript-group-blocks-alias

- Remove after: two alpha releases after transcript group snapshots consistently use
  `children`
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
