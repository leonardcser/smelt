# Compatibility debt

Compatibility code we intend to remove while Smelt is alpha. Mark matching code with `COMPAT(<id>)`.

## session-v1-messages

- Remove after: two alpha releases after session schema v2 ships
- Why: load old `session.json` files that stored provider-style `messages`
- Code:
  - `crates/core/src/session.rs` — `SessionWire`, message-index remapping, legacy accounting snapshots
  - `crates/protocol/src/history.rs` — `history_from_messages`, legacy note-prefix decoding
- Tests:
  - `legacy_messages_session_loads_with_no_user_display`
  - `legacy_session_with_orphan_tool_use_is_repaired_on_deserialize`
  - `legacy_session_with_token_snapshots_loads_without_error`

## session-search-sidecar-missing

- Remove after: old session dirs without `meta.json` / `content.txt` no longer matter
- Why: rebuild list metadata and search text from `session.json`
- Code:
  - `crates/core/src/session.rs` — session listing/search blob fallbacks

## legacy-process-status-notes

- Remove after: removing `session-v1-messages`
- Why: render old plain-text background-process notes as process blocks
- Code:
  - `crates/tui/src/app/history.rs` — `is_legacy_process_status_note`
  - `crates/tui/src/app/transcript.rs` — legacy mode-note replacement
- Tests:
  - `restore_screen_rebuilds_legacy_process_status_notes_as_process_blocks`

## lua-border-sides

- Remove after: next alpha unless known plugins use it
- Why: old border shape `border = { sides = ... }`
- Code:
  - `crates/tui/src/lua/parse.rs` — `apply_legacy_sides`
- Tests:
  - `border_table_with_partial_sides_legacy`

## lua-session-messages-write

- Remove after: one alpha release after deprecation
- Why: `smelt.session.messages(list)` writes history through legacy provider-message rows
- Code:
  - `crates/tui/src/lua/api/session.rs` — `messages(list)` write path

## lua-provider-middleware-messages

- Remove after: provider middleware grows a semantic history API and old plugins no longer matter
- Why: `smelt.provider.middleware{on_request=...}` still mutates provider-style `Vec<Message>` rows
- Code:
  - `crates/engine/src/agent.rs` — converts middleware replacements back into `HistoryItem`

## lua-buffer-mode-aliases

- Remove after: next alpha unless known plugins use the aliases
- Why: old `buf.create` mode aliases (`bash`, `sh`, `shell`, `file`, `diff`) predate `mode = "code"`
- Code:
  - `crates/tui/src/format.rs` — legacy Lua buffer mode aliases

## openai-reasoning-summary-shape

- Remove after: sessions with object/string OpenAI reasoning summaries no longer matter
- Why: normalize old saved Responses reasoning `summary` shapes to the current array shape
- Code:
  - `crates/engine/src/provider/openai.rs` — `normalize_openai_reasoning_item`
- Tests:
  - `build_body_wraps_legacy_reasoning_summary_object`
  - `parse_response_normalizes_legacy_reasoning_summary_object`

## kimi-anthropic-compatible-provider-kind

- Remove after: old configs using Kimi as `anthropic-compatible` no longer matter
- Why: map pre-`kimi-code` configs to the Kimi catalog key
- Code:
  - `crates/engine/src/catalog.rs` — Kimi compatibility branch
