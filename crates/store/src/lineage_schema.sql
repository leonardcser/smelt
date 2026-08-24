CREATE TABLE IF NOT EXISTS store_meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL,
    updated_at INTEGER NOT NULL DEFAULT (unixepoch()) CHECK (updated_at >= 0)
) STRICT;

CREATE TABLE IF NOT EXISTS objects (
    hash TEXT PRIMARY KEY CHECK (length(hash) = 64 AND hash NOT GLOB '*[^0-9a-f]*'),
    codec TEXT NOT NULL CHECK (codec IN ('none', 'zstd')),
    raw_size INTEGER NOT NULL CHECK (raw_size >= 0),
    stored_size INTEGER NOT NULL CHECK (stored_size >= 0 AND stored_size = length(bytes)),
    bytes BLOB NOT NULL
) STRICT;

CREATE TABLE IF NOT EXISTS request_attempts (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    request_id TEXT,
    turn_id TEXT,
    ask_id TEXT,
    started_at INTEGER NOT NULL CHECK (started_at >= 0),
    completed_at INTEGER CHECK (completed_at IS NULL OR completed_at >= 0),
    provider TEXT,
    model TEXT,
    history_len INTEGER CHECK (history_len IS NULL OR history_len >= 0),
    error_summary TEXT,
    background INTEGER NOT NULL DEFAULT 0 CHECK (background IN (0, 1)),
    raw_body_size INTEGER NOT NULL DEFAULT 0 CHECK (raw_body_size >= 0),
    kind TEXT,
    api_base TEXT,
    url TEXT,
    http_status INTEGER CHECK (http_status IS NULL OR http_status BETWEEN 100 AND 599),
    prompt_cache_key TEXT,
    stream INTEGER NOT NULL DEFAULT 0 CHECK (stream IN (0, 1)),
    attempt INTEGER NOT NULL DEFAULT 1 CHECK (attempt >= 1),
    response_summary TEXT
) STRICT;
CREATE INDEX IF NOT EXISTS request_attempts_started_at_idx
    ON request_attempts(started_at DESC, id DESC);
CREATE INDEX IF NOT EXISTS request_attempts_request_id_idx ON request_attempts(request_id);
CREATE INDEX IF NOT EXISTS request_attempts_turn_ask_idx ON request_attempts(turn_id, ask_id, id);
CREATE INDEX IF NOT EXISTS request_attempts_provider_model_idx
    ON request_attempts(provider, model, started_at DESC);
CREATE INDEX IF NOT EXISTS request_attempts_error_idx
    ON request_attempts(error_summary, started_at DESC);
CREATE INDEX IF NOT EXISTS request_attempts_background_idx
    ON request_attempts(background, started_at DESC);
CREATE INDEX IF NOT EXISTS request_attempts_body_size_idx
    ON request_attempts(raw_body_size DESC);
CREATE INDEX IF NOT EXISTS request_attempts_url_idx ON request_attempts(url);

CREATE TABLE IF NOT EXISTS request_object_refs (
    request_attempt_id INTEGER NOT NULL
        REFERENCES request_attempts(id) ON DELETE CASCADE CHECK (request_attempt_id > 0),
    object_hash TEXT NOT NULL REFERENCES objects(hash) ON DELETE RESTRICT,
    role TEXT NOT NULL CHECK (role IN (
        'body_json', 'body_manifest', 'body_top', 'body_item', 'body_parent', 'response', 'error'
    )),
    PRIMARY KEY (request_attempt_id, object_hash, role)
) STRICT;
CREATE UNIQUE INDEX IF NOT EXISTS request_object_refs_body_root_idx
    ON request_object_refs(request_attempt_id)
    WHERE role IN ('body_json', 'body_manifest');
CREATE UNIQUE INDEX IF NOT EXISTS request_object_refs_response_idx
    ON request_object_refs(request_attempt_id)
    WHERE role = 'response';
CREATE UNIQUE INDEX IF NOT EXISTS request_object_refs_error_idx
    ON request_object_refs(request_attempt_id)
    WHERE role = 'error';
CREATE INDEX IF NOT EXISTS request_object_refs_object_idx
    ON request_object_refs(object_hash, request_attempt_id);

CREATE TABLE IF NOT EXISTS request_stats (
    request_attempt_id INTEGER PRIMARY KEY
        REFERENCES request_attempts(id) ON DELETE CASCADE CHECK (request_attempt_id > 0),
    input_tokens INTEGER CHECK (input_tokens IS NULL OR input_tokens >= 0),
    output_tokens INTEGER CHECK (output_tokens IS NULL OR output_tokens >= 0),
    cached_input_tokens INTEGER CHECK (cached_input_tokens IS NULL OR cached_input_tokens >= 0),
    reasoning_tokens INTEGER CHECK (reasoning_tokens IS NULL OR reasoning_tokens >= 0),
    total_cost_micros INTEGER CHECK (total_cost_micros IS NULL OR total_cost_micros >= 0),
    stats_json TEXT,
    context_tokens INTEGER CHECK (context_tokens IS NULL OR context_tokens >= 0),
    cache_write_tokens INTEGER CHECK (cache_write_tokens IS NULL OR cache_write_tokens >= 0),
    tokens_per_sec REAL CHECK (tokens_per_sec IS NULL OR tokens_per_sec >= 0)
) STRICT;
CREATE INDEX IF NOT EXISTS request_stats_input_tokens_idx ON request_stats(input_tokens DESC);
CREATE INDEX IF NOT EXISTS request_stats_output_tokens_idx ON request_stats(output_tokens DESC);
CREATE INDEX IF NOT EXISTS request_stats_total_cost_idx ON request_stats(total_cost_micros DESC);

CREATE TABLE IF NOT EXISTS lineage_identity (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    lineage_id TEXT NOT NULL UNIQUE
        CHECK (length(lineage_id) = 32 AND lineage_id NOT GLOB '*[^0-9a-f]*'),
    created_at INTEGER NOT NULL CHECK (created_at >= 0)
) STRICT;

CREATE TABLE IF NOT EXISTS lineage_branches (
    lineage_id TEXT NOT NULL,
    session_id TEXT PRIMARY KEY
        CHECK (length(session_id) = 64 AND session_id NOT GLOB '*[^0-9a-f]*'),
    fork_parent_session_id TEXT,
    parent_session_id TEXT
        CHECK (parent_session_id IS NULL OR
            (length(parent_session_id) = 64 AND parent_session_id NOT GLOB '*[^0-9a-f]*')),
    initial_revision_id TEXT NOT NULL,
    head_revision_id TEXT,
    head_sequence INTEGER NOT NULL CHECK (head_sequence > 0),
    next_turn_id INTEGER NOT NULL CHECK (next_turn_id > 0),
    created_at INTEGER NOT NULL CHECK (created_at >= 0),
    updated_at INTEGER NOT NULL CHECK (updated_at >= created_at),
    deleted_at INTEGER CHECK (deleted_at IS NULL OR deleted_at >= created_at),
    cwd TEXT,
    mode TEXT,
    reasoning_effort TEXT,
    model TEXT,
    fast_mode INTEGER CHECK (fast_mode IS NULL OR fast_mode IN (0, 1)),
    session_cost_usd REAL NOT NULL CHECK (session_cost_usd >= 0.0),
    input_tokens INTEGER NOT NULL CHECK (input_tokens >= 0),
    cached_input_tokens INTEGER NOT NULL CHECK (cached_input_tokens >= 0),
    output_tokens INTEGER NOT NULL CHECK (output_tokens >= 0),
    reasoning_tokens INTEGER NOT NULL CHECK (reasoning_tokens >= 0),
    accounting_json TEXT NOT NULL DEFAULT '{}',
    UNIQUE (lineage_id, session_id),
    FOREIGN KEY (lineage_id) REFERENCES lineage_identity(lineage_id),
    FOREIGN KEY (lineage_id, fork_parent_session_id)
        REFERENCES lineage_branches(lineage_id, session_id),
    FOREIGN KEY (lineage_id, initial_revision_id)
        REFERENCES lineage_revisions(lineage_id, revision_id)
        DEFERRABLE INITIALLY DEFERRED,
    FOREIGN KEY (lineage_id, head_revision_id)
        REFERENCES lineage_revisions(lineage_id, revision_id)
        DEFERRABLE INITIALLY DEFERRED
) STRICT;

CREATE TABLE IF NOT EXISTS lineage_payload_object_refs (
    lineage_id TEXT NOT NULL,
    payload_id TEXT NOT NULL
        CHECK (length(payload_id) = 64 AND payload_id NOT GLOB '*[^0-9a-f]*'),
    payload_kind TEXT NOT NULL
        CHECK (payload_kind IN ('history', 'transcript', 'revision_state')),
    object_hash TEXT NOT NULL
        CHECK (length(object_hash) = 64 AND object_hash NOT GLOB '*[^0-9a-f]*'),
    byte_count INTEGER NOT NULL CHECK (byte_count >= 0),
    PRIMARY KEY (lineage_id, payload_id),
    FOREIGN KEY (lineage_id) REFERENCES lineage_identity(lineage_id),
    FOREIGN KEY (object_hash) REFERENCES objects(hash)
) STRICT;

CREATE TABLE IF NOT EXISTS lineage_payload_nested_object_refs (
    lineage_id TEXT NOT NULL,
    payload_id TEXT NOT NULL,
    object_hash TEXT NOT NULL
        CHECK (length(object_hash) = 64 AND object_hash NOT GLOB '*[^0-9a-f]*'),
    object_role TEXT NOT NULL CHECK (object_role IN ('attachment_image', 'metadata')),
    raw_size INTEGER NOT NULL CHECK (raw_size >= 0),
    PRIMARY KEY (lineage_id, payload_id, object_hash, object_role),
    FOREIGN KEY (lineage_id, payload_id)
        REFERENCES lineage_payload_object_refs(lineage_id, payload_id) ON DELETE CASCADE,
    FOREIGN KEY (object_hash) REFERENCES objects(hash)
) STRICT;

CREATE TABLE IF NOT EXISTS lineage_sequence_nodes (
    lineage_id TEXT NOT NULL,
    node_id TEXT NOT NULL
        CHECK (length(node_id) = 64 AND node_id NOT GLOB '*[^0-9a-f]*'),
    sequence_kind TEXT NOT NULL CHECK (sequence_kind IN ('history', 'transcript')),
    node_kind TEXT NOT NULL CHECK (node_kind IN ('leaf', 'internal')),
    level INTEGER NOT NULL CHECK (level >= 0),
    entry_count INTEGER NOT NULL CHECK (entry_count BETWEEN 1 AND 32),
    item_count INTEGER NOT NULL CHECK (item_count > 0),
    byte_count INTEGER NOT NULL CHECK (byte_count >= 0),
    CHECK (
        (node_kind = 'leaf' AND level = 0)
        OR (node_kind = 'internal' AND level > 0)
    ),
    PRIMARY KEY (lineage_id, node_id),
    FOREIGN KEY (lineage_id) REFERENCES lineage_identity(lineage_id)
) STRICT;

CREATE TABLE IF NOT EXISTS lineage_sequence_entries (
    lineage_id TEXT NOT NULL,
    node_id TEXT NOT NULL,
    entry_index INTEGER NOT NULL CHECK (entry_index BETWEEN 0 AND 31),
    entry_kind TEXT NOT NULL CHECK (entry_kind IN ('item', 'child')),
    payload_id TEXT,
    child_node_id TEXT,
    item_count INTEGER NOT NULL CHECK (item_count > 0),
    byte_count INTEGER NOT NULL CHECK (byte_count >= 0),
    cumulative_item_count INTEGER NOT NULL CHECK (cumulative_item_count >= item_count),
    cumulative_byte_count INTEGER NOT NULL CHECK (cumulative_byte_count >= byte_count),
    PRIMARY KEY (lineage_id, node_id, entry_index),
    CHECK (
        (entry_kind = 'item' AND payload_id IS NOT NULL AND child_node_id IS NULL
            AND item_count = 1)
        OR
        (entry_kind = 'child' AND payload_id IS NULL AND child_node_id IS NOT NULL)
    ),
    FOREIGN KEY (lineage_id, node_id)
        REFERENCES lineage_sequence_nodes(lineage_id, node_id) ON DELETE CASCADE,
    FOREIGN KEY (lineage_id, payload_id)
        REFERENCES lineage_payload_object_refs(lineage_id, payload_id),
    FOREIGN KEY (lineage_id, child_node_id)
        REFERENCES lineage_sequence_nodes(lineage_id, node_id)
) STRICT;

CREATE TABLE IF NOT EXISTS lineage_sequence_roots (
    lineage_id TEXT NOT NULL,
    root_id TEXT NOT NULL
        CHECK (length(root_id) = 64 AND root_id NOT GLOB '*[^0-9a-f]*'),
    root_kind TEXT NOT NULL CHECK (root_kind IN ('history', 'transcript')),
    root_node_id TEXT
        CHECK (
            root_node_id IS NULL
            OR (length(root_node_id) = 64 AND root_node_id NOT GLOB '*[^0-9a-f]*')
        ),
    depth INTEGER NOT NULL CHECK (depth >= 0),
    item_count INTEGER NOT NULL CHECK (item_count >= 0),
    byte_count INTEGER NOT NULL CHECK (byte_count >= 0),
    PRIMARY KEY (lineage_id, root_id),
    CHECK (
        (item_count = 0 AND byte_count = 0 AND root_node_id IS NULL AND depth = 0)
        OR
        (item_count > 0 AND root_node_id IS NOT NULL AND depth > 0)
    ),
    FOREIGN KEY (lineage_id) REFERENCES lineage_identity(lineage_id),
    FOREIGN KEY (lineage_id, root_node_id)
        REFERENCES lineage_sequence_nodes(lineage_id, node_id)
) STRICT;

CREATE TABLE IF NOT EXISTS lineage_transcript_record_profiles (
    lineage_id TEXT NOT NULL,
    payload_id TEXT NOT NULL,
    block_idx INTEGER NOT NULL CHECK (block_idx >= 0),
    history_idx INTEGER CHECK (history_idx IS NULL OR history_idx >= 0),
    kind TEXT NOT NULL CHECK (kind IN (
        'user', 'mode', 'process_status', 'thinking', 'assistant',
        'code', 'tool', 'exec', 'compacted', 'compaction_preview'
    )),
    role TEXT NOT NULL CHECK (role IN ('user', 'assistant', 'mode', 'process_status')),
    first_line TEXT NOT NULL CHECK (length(first_line) <= 512),
    estimated_text_bytes INTEGER NOT NULL CHECK (estimated_text_bytes >= 0),
    rows_20 INTEGER NOT NULL CHECK (rows_20 >= 1),
    rows_40 INTEGER NOT NULL CHECK (rows_40 >= 1 AND rows_40 <= rows_20),
    rows_80 INTEGER NOT NULL CHECK (rows_80 >= 1 AND rows_80 <= rows_40),
    rows_120 INTEGER NOT NULL CHECK (rows_120 >= 1 AND rows_120 <= rows_80),
    rows_160 INTEGER NOT NULL CHECK (rows_160 >= 1 AND rows_160 <= rows_120),
    rows_240 INTEGER NOT NULL CHECK (rows_240 >= 1 AND rows_240 <= rows_160),
    PRIMARY KEY (lineage_id, payload_id),
    FOREIGN KEY (lineage_id, payload_id)
        REFERENCES lineage_payload_object_refs(lineage_id, payload_id) ON DELETE CASCADE
) STRICT;

CREATE TABLE IF NOT EXISTS lineage_transcript_extent_nodes (
    lineage_id TEXT NOT NULL,
    node_id TEXT NOT NULL,
    record_count INTEGER NOT NULL CHECK (record_count > 0),
    first_block_idx INTEGER NOT NULL CHECK (first_block_idx >= 0),
    last_block_idx INTEGER NOT NULL CHECK (last_block_idx >= first_block_idx),
    kind_mask INTEGER NOT NULL CHECK (kind_mask > 0),
    role_mask INTEGER NOT NULL CHECK (role_mask > 0),
    rows_20 INTEGER NOT NULL CHECK (rows_20 >= record_count),
    rows_40 INTEGER NOT NULL CHECK (rows_40 >= record_count AND rows_40 <= rows_20),
    rows_80 INTEGER NOT NULL CHECK (rows_80 >= record_count AND rows_80 <= rows_40),
    rows_120 INTEGER NOT NULL CHECK (rows_120 >= record_count AND rows_120 <= rows_80),
    rows_160 INTEGER NOT NULL CHECK (rows_160 >= record_count AND rows_160 <= rows_120),
    rows_240 INTEGER NOT NULL CHECK (rows_240 >= record_count AND rows_240 <= rows_160),
    PRIMARY KEY (lineage_id, node_id),
    FOREIGN KEY (lineage_id, node_id)
        REFERENCES lineage_sequence_nodes(lineage_id, node_id) ON DELETE CASCADE
) STRICT;

CREATE TABLE IF NOT EXISTS lineage_revisions (
    lineage_id TEXT NOT NULL,
    revision_id TEXT NOT NULL
        CHECK (length(revision_id) = 64 AND revision_id NOT GLOB '*[^0-9a-f]*'),
    created_by_session_id TEXT NOT NULL,
    parent_revision_id TEXT,
    operation_kind TEXT NOT NULL
        CHECK (operation_kind IN ('initial', 'append', 'split', 'rewind')),
    history_root_id TEXT NOT NULL,
    transcript_root_id TEXT NOT NULL,
    state_payload_id TEXT NOT NULL,
    history_len INTEGER NOT NULL CHECK (history_len >= 0),
    transcript_record_count INTEGER NOT NULL CHECK (transcript_record_count >= 0),
    transcript_byte_count INTEGER NOT NULL CHECK (transcript_byte_count >= 0),
    commit_fingerprint TEXT
        CHECK (commit_fingerprint IS NULL OR
            (length(commit_fingerprint) = 64 AND commit_fingerprint NOT GLOB '*[^0-9a-f]*')),
    history_start_idx INTEGER CHECK (history_start_idx IS NULL OR history_start_idx >= 0),
    transcript_start_idx INTEGER
        CHECK (transcript_start_idx IS NULL OR transcript_start_idx >= 0),
    turn_id INTEGER CHECK (turn_id IS NULL OR turn_id > 0),
    created_at INTEGER NOT NULL CHECK (created_at >= 0),
    PRIMARY KEY (lineage_id, revision_id),
    UNIQUE (lineage_id, created_by_session_id, commit_fingerprint),
    CHECK (
        (operation_kind = 'initial' AND parent_revision_id IS NULL
            AND commit_fingerprint IS NULL)
        OR
        (operation_kind != 'initial' AND parent_revision_id IS NOT NULL
            AND commit_fingerprint IS NOT NULL)
    ),
    FOREIGN KEY (lineage_id, created_by_session_id)
        REFERENCES lineage_branches(lineage_id, session_id),
    FOREIGN KEY (lineage_id, parent_revision_id)
        REFERENCES lineage_revisions(lineage_id, revision_id),
    FOREIGN KEY (lineage_id, history_root_id)
        REFERENCES lineage_sequence_roots(lineage_id, root_id),
    FOREIGN KEY (lineage_id, transcript_root_id)
        REFERENCES lineage_sequence_roots(lineage_id, root_id),
    FOREIGN KEY (lineage_id, state_payload_id)
        REFERENCES lineage_payload_object_refs(lineage_id, payload_id)
) STRICT;

CREATE TABLE IF NOT EXISTS lineage_branch_revisions (
    lineage_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    branch_sequence INTEGER NOT NULL CHECK (branch_sequence > 0),
    revision_id TEXT NOT NULL,
    PRIMARY KEY (lineage_id, session_id, branch_sequence),
    FOREIGN KEY (lineage_id, session_id)
        REFERENCES lineage_branches(lineage_id, session_id),
    FOREIGN KEY (lineage_id, revision_id)
        REFERENCES lineage_revisions(lineage_id, revision_id)
        DEFERRABLE INITIALLY DEFERRED
) STRICT;

CREATE TABLE IF NOT EXISTS lineage_turns (
    lineage_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    turn_id INTEGER NOT NULL CHECK (turn_id > 0),
    submitted_history_idx INTEGER NOT NULL CHECK (submitted_history_idx >= 0),
    submitted_history_hash TEXT NOT NULL
        CHECK (length(submitted_history_hash) = 64
            AND submitted_history_hash NOT GLOB '*[^0-9a-f]*'),
    submitted_revision_id TEXT NOT NULL,
    submitted_sequence INTEGER NOT NULL CHECK (submitted_sequence > 0),
    turn_kind TEXT NOT NULL CHECK (turn_kind IN ('user', 'command', 'continuation', 'note')),
    turn_state TEXT NOT NULL
        CHECK (turn_state IN ('ready', 'running', 'completed', 'interrupted', 'failed', 'cancelled')),
    continuation_of INTEGER CHECK (continuation_of IS NULL OR continuation_of > 0),
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms >= 0),
    started_at_ms INTEGER CHECK (started_at_ms IS NULL OR started_at_ms >= created_at_ms),
    finished_at_ms INTEGER CHECK (finished_at_ms IS NULL OR finished_at_ms >= created_at_ms),
    terminal_reason TEXT,
    PRIMARY KEY (lineage_id, session_id, turn_id),
    FOREIGN KEY (lineage_id, session_id)
        REFERENCES lineage_branches(lineage_id, session_id),
    FOREIGN KEY (lineage_id, submitted_revision_id)
        REFERENCES lineage_revisions(lineage_id, revision_id),
    FOREIGN KEY (lineage_id, session_id, continuation_of)
        REFERENCES lineage_turns(lineage_id, session_id, turn_id)
) STRICT;

CREATE TABLE IF NOT EXISTS lineage_commit_receipts (
    lineage_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    fingerprint TEXT NOT NULL
        CHECK (length(fingerprint) = 64 AND fingerprint NOT GLOB '*[^0-9a-f]*'),
    operation_kind TEXT NOT NULL
        CHECK (operation_kind IN ('create', 'append', 'split', 'rewind', 'fork')),
    prior_revision_id TEXT,
    result_revision_id TEXT NOT NULL,
    history_start_idx INTEGER CHECK (history_start_idx IS NULL OR history_start_idx >= 0),
    history_item_count INTEGER CHECK (history_item_count IS NULL OR history_item_count >= 0),
    transcript_start_idx INTEGER
        CHECK (transcript_start_idx IS NULL OR transcript_start_idx >= 0),
    transcript_record_count INTEGER
        CHECK (transcript_record_count IS NULL OR transcript_record_count >= 0),
    turn_id INTEGER CHECK (turn_id IS NULL OR turn_id > 0),
    created_at INTEGER NOT NULL CHECK (created_at >= 0),
    PRIMARY KEY (lineage_id, session_id, fingerprint),
    CHECK (
        ((operation_kind IN ('create', 'fork') AND prior_revision_id IS NULL)
            OR (operation_kind IN ('append', 'split', 'rewind')
                AND prior_revision_id IS NOT NULL))
        AND
        ((operation_kind = 'append'
            AND history_start_idx IS NOT NULL AND history_item_count IS NOT NULL
            AND transcript_start_idx IS NOT NULL
            AND transcript_record_count IS NOT NULL)
            OR (operation_kind IN ('create', 'split', 'rewind', 'fork')
                AND history_start_idx IS NULL AND history_item_count IS NULL
                AND transcript_start_idx IS NULL
                AND transcript_record_count IS NULL))
    ),
    FOREIGN KEY (lineage_id, session_id)
        REFERENCES lineage_branches(lineage_id, session_id),
    FOREIGN KEY (lineage_id, prior_revision_id)
        REFERENCES lineage_revisions(lineage_id, revision_id),
    FOREIGN KEY (lineage_id, result_revision_id)
        REFERENCES lineage_revisions(lineage_id, revision_id),
    FOREIGN KEY (lineage_id, session_id, turn_id)
        REFERENCES lineage_turns(lineage_id, session_id, turn_id)
        DEFERRABLE INITIALLY DEFERRED
) STRICT;

CREATE TABLE IF NOT EXISTS lineage_request_attempts (
    lineage_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    request_attempt_id INTEGER NOT NULL,
    PRIMARY KEY (request_attempt_id),
    FOREIGN KEY (lineage_id, session_id)
        REFERENCES lineage_branches(lineage_id, session_id),
    FOREIGN KEY (request_attempt_id)
        REFERENCES request_attempts(id) ON DELETE CASCADE
) STRICT;

CREATE TABLE IF NOT EXISTS lineage_session_receipts (
    lineage_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    fingerprint TEXT NOT NULL
        CHECK (length(fingerprint) = 64 AND fingerprint NOT GLOB '*[^0-9a-f]*'),
    command_kind TEXT NOT NULL
        CHECK (command_kind IN ('save', 'submit_turn', 'turn_transition', 'startup_recovery')),
    save_receipt_json TEXT NOT NULL,
    turn_id INTEGER CHECK (turn_id IS NULL OR turn_id > 0),
    turn_state TEXT CHECK (turn_state IS NULL OR turn_state IN
        ('ready', 'running', 'completed', 'interrupted', 'failed', 'cancelled')),
    turn_payload_json TEXT,
    created_at INTEGER NOT NULL CHECK (created_at >= 0),
    PRIMARY KEY (lineage_id, session_id, fingerprint),
    FOREIGN KEY (lineage_id, session_id)
        REFERENCES lineage_branches(lineage_id, session_id)
) STRICT;

CREATE TABLE IF NOT EXISTS lineage_turn_transitions (
    lineage_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    fingerprint TEXT NOT NULL,
    turn_id INTEGER NOT NULL CHECK (turn_id > 0),
    from_state TEXT NOT NULL
        CHECK (from_state IN ('ready', 'running', 'completed', 'interrupted', 'failed', 'cancelled')),
    to_state TEXT NOT NULL
        CHECK (to_state IN ('ready', 'running', 'completed', 'interrupted', 'failed', 'cancelled')),
    transitioned_at_ms INTEGER NOT NULL CHECK (transitioned_at_ms >= 0),
    terminal_reason TEXT,
    PRIMARY KEY (lineage_id, session_id, fingerprint),
    FOREIGN KEY (lineage_id, session_id, fingerprint)
        REFERENCES lineage_session_receipts(lineage_id, session_id, fingerprint),
    FOREIGN KEY (lineage_id, session_id, turn_id)
        REFERENCES lineage_turns(lineage_id, session_id, turn_id)
) STRICT;

CREATE TABLE IF NOT EXISTS lineage_retained_revisions (
    lineage_id TEXT NOT NULL,
    revision_id TEXT NOT NULL,
    retention_kind TEXT NOT NULL CHECK (retention_kind IN ('recovery', 'export')),
    retained_at INTEGER NOT NULL CHECK (retained_at >= 0),
    PRIMARY KEY (lineage_id, revision_id, retention_kind),
    FOREIGN KEY (lineage_id, revision_id)
        REFERENCES lineage_revisions(lineage_id, revision_id) ON DELETE CASCADE
) STRICT;

CREATE INDEX IF NOT EXISTS lineage_branches_updated_idx
    ON lineage_branches(lineage_id, updated_at DESC, session_id);
CREATE INDEX IF NOT EXISTS lineage_branches_fork_parent_idx
    ON lineage_branches(lineage_id, fork_parent_session_id);
CREATE INDEX IF NOT EXISTS lineage_revisions_parent_idx
    ON lineage_revisions(lineage_id, parent_revision_id);
CREATE INDEX IF NOT EXISTS lineage_revisions_creator_idx
    ON lineage_revisions(lineage_id, created_by_session_id, created_at);
CREATE INDEX IF NOT EXISTS lineage_roots_node_idx
    ON lineage_sequence_roots(lineage_id, root_node_id);
CREATE INDEX IF NOT EXISTS lineage_entries_child_idx
    ON lineage_sequence_entries(lineage_id, child_node_id);
CREATE INDEX IF NOT EXISTS lineage_entries_payload_idx
    ON lineage_sequence_entries(lineage_id, payload_id);
CREATE INDEX IF NOT EXISTS lineage_payload_objects_idx
    ON lineage_payload_object_refs(lineage_id, object_hash);
CREATE INDEX IF NOT EXISTS lineage_payload_nested_objects_idx
    ON lineage_payload_nested_object_refs(lineage_id, object_hash);
CREATE INDEX IF NOT EXISTS lineage_transcript_profiles_block_idx
    ON lineage_transcript_record_profiles(lineage_id, block_idx, payload_id);
CREATE INDEX IF NOT EXISTS lineage_receipts_result_idx
    ON lineage_commit_receipts(lineage_id, result_revision_id);
CREATE INDEX IF NOT EXISTS lineage_branch_revisions_revision_idx
    ON lineage_branch_revisions(lineage_id, revision_id);
CREATE INDEX IF NOT EXISTS lineage_turns_state_idx
    ON lineage_turns(lineage_id, session_id, turn_state, turn_id DESC);
CREATE INDEX IF NOT EXISTS lineage_turns_revision_idx
    ON lineage_turns(lineage_id, submitted_revision_id);
CREATE INDEX IF NOT EXISTS lineage_request_attempts_branch_idx
    ON lineage_request_attempts(lineage_id, session_id, request_attempt_id);
CREATE INDEX IF NOT EXISTS lineage_session_receipts_created_idx
    ON lineage_session_receipts(lineage_id, session_id, created_at DESC);
CREATE INDEX IF NOT EXISTS lineage_turn_transitions_turn_idx
    ON lineage_turn_transitions(lineage_id, session_id, turn_id, transitioned_at_ms);
CREATE INDEX IF NOT EXISTS lineage_retained_kind_idx
    ON lineage_retained_revisions(lineage_id, retention_kind, retained_at);

CREATE TRIGGER IF NOT EXISTS lineage_sequence_entry_insert
BEFORE INSERT ON lineage_sequence_entries
BEGIN
    SELECT CASE WHEN NEW.entry_index >= (
        SELECT entry_count FROM lineage_sequence_nodes
        WHERE lineage_id = NEW.lineage_id AND node_id = NEW.node_id
    ) THEN RAISE(ABORT, 'sequence entry index exceeds node entry count') END;
    SELECT CASE WHEN NEW.entry_index = 0 AND (
        NEW.cumulative_item_count != NEW.item_count
        OR NEW.cumulative_byte_count != NEW.byte_count
    ) THEN RAISE(ABORT, 'first sequence entry has invalid cumulative extent') END;
    SELECT CASE WHEN NEW.entry_index > 0 AND NOT EXISTS (
        SELECT 1 FROM lineage_sequence_entries previous
        WHERE previous.lineage_id = NEW.lineage_id
          AND previous.node_id = NEW.node_id
          AND previous.entry_index = NEW.entry_index - 1
          AND NEW.cumulative_item_count = previous.cumulative_item_count + NEW.item_count
          AND NEW.cumulative_byte_count = previous.cumulative_byte_count + NEW.byte_count
    ) THEN RAISE(ABORT, 'sequence entries are not contiguous') END;
    SELECT CASE WHEN NEW.entry_kind = 'item' AND NOT EXISTS (
        SELECT 1
        FROM lineage_sequence_nodes node
        JOIN lineage_payload_object_refs payload
          ON payload.lineage_id = NEW.lineage_id AND payload.payload_id = NEW.payload_id
        WHERE node.lineage_id = NEW.lineage_id AND node.node_id = NEW.node_id
          AND node.node_kind = 'leaf' AND node.level = 0
          AND node.sequence_kind = payload.payload_kind
          AND payload.payload_kind IN ('history', 'transcript')
          AND payload.byte_count = NEW.byte_count
    ) THEN RAISE(ABORT, 'sequence item does not match leaf payload') END;
    SELECT CASE WHEN NEW.entry_kind = 'child' AND NOT EXISTS (
        SELECT 1
        FROM lineage_sequence_nodes parent
        JOIN lineage_sequence_nodes child
          ON child.lineage_id = NEW.lineage_id AND child.node_id = NEW.child_node_id
        WHERE parent.lineage_id = NEW.lineage_id AND parent.node_id = NEW.node_id
          AND parent.node_kind = 'internal' AND parent.level = child.level + 1
          AND parent.sequence_kind = child.sequence_kind
          AND child.item_count = NEW.item_count
          AND child.byte_count = NEW.byte_count
    ) THEN RAISE(ABORT, 'sequence child does not match parent entry') END;
    SELECT CASE WHEN NEW.entry_index + 1 = (
        SELECT entry_count FROM lineage_sequence_nodes
        WHERE lineage_id = NEW.lineage_id AND node_id = NEW.node_id
    ) AND NOT EXISTS (
        SELECT 1 FROM lineage_sequence_nodes node
        WHERE node.lineage_id = NEW.lineage_id AND node.node_id = NEW.node_id
          AND node.item_count = NEW.cumulative_item_count
          AND node.byte_count = NEW.cumulative_byte_count
    ) THEN RAISE(ABORT, 'final sequence extent does not match node') END;
END;

CREATE TRIGGER IF NOT EXISTS lineage_sequence_root_insert
BEFORE INSERT ON lineage_sequence_roots
WHEN NEW.root_node_id IS NOT NULL
BEGIN
    SELECT CASE WHEN NOT EXISTS (
        SELECT 1
        FROM lineage_sequence_nodes node
        WHERE node.lineage_id = NEW.lineage_id AND node.node_id = NEW.root_node_id
          AND node.sequence_kind = NEW.root_kind
          AND node.level + 1 = NEW.depth
          AND node.item_count = NEW.item_count
          AND node.byte_count = NEW.byte_count
    ) THEN RAISE(ABORT, 'sequence root does not match root node') END;
    SELECT CASE WHEN EXISTS (
        WITH RECURSIVE reachable(node_id) AS (
            SELECT NEW.root_node_id
            UNION
            SELECT entry.child_node_id
            FROM reachable
            JOIN lineage_sequence_entries entry
              ON entry.lineage_id = NEW.lineage_id
             AND entry.node_id = reachable.node_id
            WHERE entry.entry_kind = 'child'
        )
        SELECT 1
        FROM reachable
        JOIN lineage_sequence_nodes node
          ON node.lineage_id = NEW.lineage_id AND node.node_id = reachable.node_id
        WHERE node.entry_count != (
            SELECT count(*)
            FROM lineage_sequence_entries entry
            WHERE entry.lineage_id = node.lineage_id AND entry.node_id = node.node_id
        )
    ) THEN RAISE(ABORT, 'sequence root reaches an incomplete node') END;
END;

CREATE TRIGGER IF NOT EXISTS lineage_branch_identity_update
BEFORE UPDATE OF lineage_id, session_id, fork_parent_session_id, parent_session_id,
    initial_revision_id
ON lineage_branches
BEGIN
    SELECT RAISE(ABORT, 'lineage branch identity is immutable');
END;

CREATE TRIGGER IF NOT EXISTS lineage_payload_object_ref_update
BEFORE UPDATE ON lineage_payload_object_refs
BEGIN
    SELECT RAISE(ABORT, 'lineage payload references are immutable');
END;

CREATE TRIGGER IF NOT EXISTS lineage_payload_nested_object_ref_update
BEFORE UPDATE ON lineage_payload_nested_object_refs
BEGIN
    SELECT RAISE(ABORT, 'lineage nested payload references are immutable');
END;

CREATE TRIGGER IF NOT EXISTS lineage_transcript_record_profile_update
BEFORE UPDATE ON lineage_transcript_record_profiles
BEGIN
    SELECT RAISE(ABORT, 'transcript record profiles are immutable');
END;

CREATE TRIGGER IF NOT EXISTS lineage_transcript_extent_node_update
BEFORE UPDATE ON lineage_transcript_extent_nodes
BEGIN
    SELECT RAISE(ABORT, 'transcript extent nodes are immutable');
END;

CREATE TRIGGER IF NOT EXISTS lineage_sequence_node_update
BEFORE UPDATE ON lineage_sequence_nodes
BEGIN
    SELECT RAISE(ABORT, 'lineage sequence nodes are immutable');
END;

CREATE TRIGGER IF NOT EXISTS lineage_sequence_entry_update
BEFORE UPDATE ON lineage_sequence_entries
BEGIN
    SELECT RAISE(ABORT, 'lineage sequence entries are immutable');
END;

CREATE TRIGGER IF NOT EXISTS lineage_sequence_root_update
BEFORE UPDATE ON lineage_sequence_roots
BEGIN
    SELECT RAISE(ABORT, 'lineage sequence roots are immutable');
END;

CREATE TRIGGER IF NOT EXISTS lineage_revision_update
BEFORE UPDATE ON lineage_revisions
BEGIN
    SELECT RAISE(ABORT, 'lineage revisions are immutable');
END;

CREATE TRIGGER IF NOT EXISTS lineage_branch_revision_update
BEFORE UPDATE ON lineage_branch_revisions
BEGIN
    SELECT RAISE(ABORT, 'lineage branch revisions are immutable');
END;

CREATE TRIGGER IF NOT EXISTS lineage_turn_identity_update
BEFORE UPDATE OF lineage_id, session_id, turn_id, submitted_history_idx,
    submitted_history_hash, submitted_revision_id, submitted_sequence, turn_kind, continuation_of,
    created_at_ms
ON lineage_turns
BEGIN
    SELECT RAISE(ABORT, 'lineage turn identity is immutable');
END;

CREATE TRIGGER IF NOT EXISTS lineage_request_attempt_update
BEFORE UPDATE ON lineage_request_attempts
BEGIN
    SELECT RAISE(ABORT, 'lineage request attempts are immutable');
END;

CREATE TRIGGER IF NOT EXISTS lineage_session_receipt_update
BEFORE UPDATE ON lineage_session_receipts
BEGIN
    SELECT RAISE(ABORT, 'lineage session receipts are immutable');
END;

CREATE TRIGGER IF NOT EXISTS lineage_session_receipt_delete
BEFORE DELETE ON lineage_session_receipts
BEGIN
    SELECT RAISE(ABORT, 'lineage session receipts are immutable');
END;

CREATE TRIGGER IF NOT EXISTS lineage_turn_transition_update
BEFORE UPDATE ON lineage_turn_transitions
BEGIN
    SELECT RAISE(ABORT, 'lineage turn transitions are immutable');
END;

CREATE TRIGGER IF NOT EXISTS lineage_turn_transition_delete
BEFORE DELETE ON lineage_turn_transitions
BEGIN
    SELECT RAISE(ABORT, 'lineage turn transitions are immutable');
END;

CREATE TRIGGER IF NOT EXISTS lineage_commit_receipt_update
BEFORE UPDATE ON lineage_commit_receipts
BEGIN
    SELECT RAISE(ABORT, 'lineage commit receipts are immutable');
END;

CREATE TRIGGER IF NOT EXISTS lineage_commit_receipt_delete
BEFORE DELETE ON lineage_commit_receipts
BEGIN
    SELECT RAISE(ABORT, 'lineage commit receipts are immutable');
END;

CREATE TRIGGER IF NOT EXISTS lineage_revision_insert
BEFORE INSERT ON lineage_revisions
BEGIN
    SELECT CASE WHEN NOT EXISTS (
        SELECT 1 FROM lineage_sequence_roots root
        WHERE root.lineage_id = NEW.lineage_id AND root.root_id = NEW.history_root_id
          AND root.root_kind = 'history' AND root.item_count = NEW.history_len
    ) THEN RAISE(ABORT, 'revision history root does not match history length') END;
    SELECT CASE WHEN NOT EXISTS (
        SELECT 1 FROM lineage_sequence_roots root
        WHERE root.lineage_id = NEW.lineage_id AND root.root_id = NEW.transcript_root_id
          AND root.root_kind = 'transcript'
          AND root.item_count = NEW.transcript_record_count
          AND root.byte_count = NEW.transcript_byte_count
    ) THEN RAISE(ABORT, 'revision transcript root does not match transcript extents') END;
    SELECT CASE WHEN NOT EXISTS (
        SELECT 1 FROM lineage_payload_object_refs payload
        WHERE payload.lineage_id = NEW.lineage_id AND payload.payload_id = NEW.state_payload_id
          AND payload.payload_kind = 'revision_state'
    ) THEN RAISE(ABORT, 'revision state has the wrong payload kind') END;
END;
