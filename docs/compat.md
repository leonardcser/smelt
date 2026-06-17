# Compatibility debt

Compatibility code we intend to remove while Smelt is alpha. Mark matching code
with `COMPAT(<id>)`.

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

- Remove after: two alpha releases after `meta.json` + `history.jsonl` session
  storage ships
- Why: load old monolithic `session.json` files, migrate them to split JSONL
  storage on open, then remove the monolith
- Code:
  - `crates/core/src/session.rs`: `load_session_files`,
    `load_legacy_json_session`, `migrate_legacy_json_session`

## session-search-sidecar-missing

- Remove after: old session dirs without `meta.json` / `content.txt` no longer
  matter
- Why: rebuild list metadata and search text from `session.json`
- Code:
  - `crates/core/src/session.rs`: session listing/search blob fallbacks

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
