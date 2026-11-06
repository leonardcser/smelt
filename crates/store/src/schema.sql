-- One database holds every session. Rows are keyed by session and position, and
-- a save only replaces the changed suffix of each sequence.

CREATE TABLE sessions (
    sid INTEGER PRIMARY KEY,
    id TEXT NOT NULL UNIQUE,
    parent_id TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    title TEXT,
    slug TEXT,
    cwd TEXT,
    mode TEXT,
    reasoning_effort TEXT,
    model TEXT,
    fast_mode INTEGER,
    accounting TEXT,
    context_tokens INTEGER,
    context_tokens_history_len INTEGER,
    display_context_tokens INTEGER,
    cost_usd REAL NOT NULL DEFAULT 0,
    last_turn_id INTEGER NOT NULL DEFAULT 0,
    history_len INTEGER NOT NULL DEFAULT 0,
    record_count INTEGER NOT NULL DEFAULT 0,
    text_bytes INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX sessions_by_updated ON sessions (updated_at DESC, id DESC);
CREATE INDEX sessions_by_cwd ON sessions (cwd, updated_at DESC, id DESC);
CREATE INDEX sessions_by_parent ON sessions (parent_id) WHERE parent_id IS NOT NULL;

-- Session values that are large and change rarely, kept out of the session row so
-- that ordinary saves do not rewrite them.
CREATE TABLE session_values (
    sid INTEGER NOT NULL REFERENCES sessions (sid) ON DELETE CASCADE,
    name TEXT NOT NULL,
    value TEXT NOT NULL,
    PRIMARY KEY (sid, name)
) WITHOUT ROWID;

-- Model history. The leading columns describe the item so navigation queries
-- never decode bodies.
CREATE TABLE history (
    sid INTEGER NOT NULL REFERENCES sessions (sid) ON DELETE CASCADE,
    idx INTEGER NOT NULL,
    -- Plain user turns in [0, idx].
    users INTEGER NOT NULL,
    user_turn INTEGER NOT NULL,
    visible INTEGER NOT NULL,
    note_kind TEXT,
    context TEXT,
    mode TEXT,
    base_mode TEXT,
    text_bytes INTEGER NOT NULL,
    body BLOB NOT NULL,
    PRIMARY KEY (sid, idx)
);
CREATE INDEX history_user_turns ON history (sid, idx) WHERE user_turn;
CREATE INDEX history_visible ON history (sid, idx) WHERE visible;
CREATE INDEX history_contexts ON history (sid, context, idx) WHERE context IS NOT NULL;
CREATE INDEX history_modes ON history (sid, idx) WHERE mode IS NOT NULL;
CREATE INDEX history_base_modes ON history (sid, idx) WHERE base_mode IS NOT NULL;

-- Rows keyed by history position (turn metadata, title and context snapshots)
-- or by ordinal (checkpoint events).
CREATE TABLE side_rows (
    sid INTEGER NOT NULL REFERENCES sessions (sid) ON DELETE CASCADE,
    tbl INTEGER NOT NULL,
    key INTEGER NOT NULL,
    value TEXT NOT NULL,
    PRIMARY KEY (sid, tbl, key)
) WITHOUT ROWID;

-- Transcript records. `rows_*` hold the cumulative estimated row count through
-- this record at each profile width, so any range's extent is one subtraction.
CREATE TABLE transcript (
    sid INTEGER NOT NULL REFERENCES sessions (sid) ON DELETE CASCADE,
    idx INTEGER NOT NULL,
    block_idx INTEGER NOT NULL,
    history_idx INTEGER,
    kind TEXT NOT NULL,
    role TEXT NOT NULL,
    rows_20 INTEGER NOT NULL,
    rows_40 INTEGER NOT NULL,
    rows_80 INTEGER NOT NULL,
    rows_120 INTEGER NOT NULL,
    rows_160 INTEGER NOT NULL,
    rows_240 INTEGER NOT NULL,
    text_bytes INTEGER NOT NULL,
    tool_render_revision INTEGER NOT NULL,
    tool_call_id TEXT,
    tool_name TEXT,
    content_hash TEXT NOT NULL,
    first_line TEXT NOT NULL,
    preview TEXT NOT NULL,
    origin TEXT,
    tool_state TEXT,
    indexed_text TEXT NOT NULL,
    block BLOB NOT NULL,
    PRIMARY KEY (sid, idx)
);
CREATE INDEX transcript_blocks ON transcript (sid, block_idx, idx);
CREATE INDEX transcript_history ON transcript (sid, history_idx, idx) WHERE history_idx IS NOT NULL;
CREATE INDEX transcript_kinds ON transcript (sid, kind, idx);
CREATE INDEX transcript_roles ON transcript (sid, role, idx);

CREATE TABLE requests (
    id INTEGER PRIMARY KEY,
    sid INTEGER NOT NULL REFERENCES sessions (sid) ON DELETE CASCADE,
    request_id TEXT,
    kind TEXT,
    turn_id TEXT,
    ask_id TEXT,
    started_at INTEGER NOT NULL,
    completed_at INTEGER,
    provider TEXT,
    model TEXT,
    api_base TEXT,
    url TEXT,
    http_status INTEGER,
    history_len INTEGER,
    attempt INTEGER NOT NULL,
    stream INTEGER NOT NULL,
    prompt_cache_key TEXT,
    background INTEGER NOT NULL,
    raw_body_size INTEGER NOT NULL,
    response_kind TEXT,
    response_summary TEXT,
    error_summary TEXT,
    input_tokens INTEGER,
    output_tokens INTEGER,
    cache_read_tokens INTEGER,
    cache_write_tokens INTEGER,
    reasoning_tokens INTEGER,
    context_tokens INTEGER,
    cost_micros INTEGER,
    tokens_per_sec REAL,
    usage TEXT,
    body BLOB,
    response BLOB,
    error BLOB
);
CREATE INDEX requests_by_session ON requests (sid, started_at, id);
