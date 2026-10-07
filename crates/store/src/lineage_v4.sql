CREATE TABLE lineage_archive_coordinates (
    lineage_id TEXT NOT NULL,
    header_payload_id TEXT NOT NULL,
    header_kind TEXT NOT NULL CHECK (header_kind IN ('metadata', 'checkpoint')),
    coordinates_json TEXT NOT NULL CHECK (
        length(coordinates_json) <= 512 AND json_valid(coordinates_json)
    ),
    coordinate_id TEXT NOT NULL CHECK (
        length(coordinate_id) = 64 AND coordinate_id NOT GLOB '*[^0-9a-f]*'
    ),
    PRIMARY KEY (lineage_id, header_payload_id),
    FOREIGN KEY (lineage_id, header_payload_id)
        REFERENCES lineage_payload_object_refs(lineage_id, payload_id) ON DELETE CASCADE
) STRICT;

CREATE TABLE lineage_branch_revisions (
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

CREATE TABLE lineage_branches (
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

CREATE TABLE lineage_checkpoint_summary_presence (
    lineage_id TEXT NOT NULL,
    header_payload_id TEXT NOT NULL,
    has_summary INTEGER NOT NULL CHECK (has_summary IN (0, 1)),
    presence_id TEXT NOT NULL CHECK (
        length(presence_id) = 64 AND presence_id NOT GLOB '*[^0-9a-f]*'
    ),
    PRIMARY KEY (lineage_id, header_payload_id),
    FOREIGN KEY (lineage_id, header_payload_id)
        REFERENCES lineage_payload_object_refs(lineage_id, payload_id) ON DELETE CASCADE
) STRICT;

CREATE TABLE lineage_commit_receipts (
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

CREATE TABLE lineage_completed_sequence_nodes (
    lineage_id TEXT NOT NULL,
    node_id TEXT NOT NULL,
    PRIMARY KEY (lineage_id, node_id),
    FOREIGN KEY (lineage_id, node_id)
        REFERENCES lineage_sequence_nodes(lineage_id, node_id) ON DELETE CASCADE
) STRICT;

CREATE TABLE lineage_history_index_nodes (
    lineage_id TEXT NOT NULL,
    node_id TEXT NOT NULL CHECK (length(node_id) = 64 AND node_id NOT GLOB '*[^0-9a-f]*'),
    min_key BLOB NOT NULL CHECK (length(min_key) = 40),
    max_key BLOB NOT NULL CHECK (length(max_key) = 40 AND min_key <= max_key),
    min_history_idx INTEGER NOT NULL CHECK (min_history_idx >= 0),
    max_history_idx INTEGER NOT NULL CHECK (max_history_idx >= min_history_idx),
    split_bit INTEGER NOT NULL CHECK (split_bit BETWEEN -1 AND 319),
    left_node_id TEXT,
    right_node_id TEXT,
    value TEXT,
    CHECK (
        (split_bit = -1 AND left_node_id IS NULL AND right_node_id IS NULL
            AND value IS NOT NULL AND min_key = max_key AND min_history_idx = max_history_idx)
        OR (split_bit >= 0 AND left_node_id IS NOT NULL AND right_node_id IS NOT NULL
            AND value IS NULL AND min_key < max_key)
    ),
    PRIMARY KEY (lineage_id, node_id),
    FOREIGN KEY (lineage_id) REFERENCES lineage_identity(lineage_id),
    FOREIGN KEY (lineage_id, left_node_id)
        REFERENCES lineage_history_index_nodes(lineage_id, node_id),
    FOREIGN KEY (lineage_id, right_node_id)
        REFERENCES lineage_history_index_nodes(lineage_id, node_id)
) STRICT;

CREATE TABLE lineage_history_indexes (
    lineage_id TEXT NOT NULL,
    history_root_id TEXT NOT NULL,
    index_node_id TEXT,
    PRIMARY KEY (lineage_id, history_root_id),
    FOREIGN KEY (lineage_id, history_root_id)
        REFERENCES lineage_sequence_roots(lineage_id, root_id),
    FOREIGN KEY (lineage_id, index_node_id)
        REFERENCES lineage_history_index_nodes(lineage_id, node_id)
) STRICT;

CREATE TABLE lineage_identity (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    lineage_id TEXT NOT NULL UNIQUE
        CHECK (length(lineage_id) = 32 AND lineage_id NOT GLOB '*[^0-9a-f]*'),
    created_at INTEGER NOT NULL CHECK (created_at >= 0)
) STRICT;

CREATE TABLE lineage_payload_nested_object_refs (
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

CREATE TABLE "lineage_payload_object_refs" (
    lineage_id TEXT NOT NULL,
    payload_id TEXT NOT NULL
        CHECK (length(payload_id) = 64 AND payload_id NOT GLOB '*[^0-9a-f]*'),
    payload_kind TEXT NOT NULL
        CHECK (payload_kind IN ('history', 'transcript', 'revision_state', 'data')),
    object_hash TEXT NOT NULL
        CHECK (length(object_hash) = 64 AND object_hash NOT GLOB '*[^0-9a-f]*'),
    byte_count INTEGER NOT NULL CHECK (byte_count >= 0),
    PRIMARY KEY (lineage_id, payload_id),
    FOREIGN KEY (lineage_id) REFERENCES lineage_identity(lineage_id),
    FOREIGN KEY (object_hash) REFERENCES objects(hash)
) STRICT;

CREATE TABLE lineage_request_attempts (
    lineage_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    request_attempt_id INTEGER NOT NULL,
    PRIMARY KEY (request_attempt_id),
    FOREIGN KEY (lineage_id, session_id)
        REFERENCES lineage_branches(lineage_id, session_id),
    FOREIGN KEY (request_attempt_id)
        REFERENCES request_attempts(id) ON DELETE CASCADE
) STRICT;

CREATE TABLE lineage_retained_revisions (
    lineage_id TEXT NOT NULL,
    revision_id TEXT NOT NULL,
    retention_kind TEXT NOT NULL CHECK (retention_kind IN ('recovery', 'export')),
    retained_at INTEGER NOT NULL CHECK (retained_at >= 0),
    PRIMARY KEY (lineage_id, revision_id, retention_kind),
    FOREIGN KEY (lineage_id, revision_id)
        REFERENCES lineage_revisions(lineage_id, revision_id) ON DELETE CASCADE
) STRICT;

CREATE TABLE lineage_revision_state_projections (
    lineage_id TEXT NOT NULL,
    original_payload_id TEXT NOT NULL,
    projected_payload_id TEXT NOT NULL,
    original_format_version INTEGER NOT NULL CHECK (original_format_version = 1),
    projection_id TEXT NOT NULL CHECK (
        length(projection_id) = 64 AND projection_id NOT GLOB '*[^0-9a-f]*'
    ),
    PRIMARY KEY (lineage_id, original_payload_id),
    CHECK (original_payload_id <> projected_payload_id),
    FOREIGN KEY (lineage_id, original_payload_id)
        REFERENCES lineage_payload_object_refs(lineage_id, payload_id) ON DELETE CASCADE,
    FOREIGN KEY (lineage_id, projected_payload_id)
        REFERENCES lineage_payload_object_refs(lineage_id, payload_id)
) STRICT;

CREATE TABLE "lineage_revision_state_roots" (
    lineage_id TEXT NOT NULL,
    state_payload_id TEXT NOT NULL,
    role TEXT NOT NULL CHECK (role IN (
        'checkpoint', 'checkpoint_events', 'turn_metas',
        'metadata_snapshots', 'context_snapshots', 'first_user_message'
    )),
    root_id TEXT NOT NULL,
    PRIMARY KEY (lineage_id, state_payload_id, role),
    FOREIGN KEY (lineage_id, state_payload_id)
        REFERENCES lineage_payload_object_refs(lineage_id, payload_id) ON DELETE CASCADE,
    FOREIGN KEY (lineage_id, root_id)
        REFERENCES lineage_sequence_roots(lineage_id, root_id)
) STRICT;

CREATE TABLE lineage_revisions (
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

CREATE TABLE lineage_sequence_entries (
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

CREATE TABLE "lineage_sequence_nodes" (
    lineage_id TEXT NOT NULL,
    node_id TEXT NOT NULL
        CHECK (length(node_id) = 64 AND node_id NOT GLOB '*[^0-9a-f]*'),
    sequence_kind TEXT NOT NULL CHECK (sequence_kind IN ('history', 'transcript', 'data')),
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

CREATE TABLE "lineage_sequence_roots" (
    lineage_id TEXT NOT NULL,
    root_id TEXT NOT NULL
        CHECK (length(root_id) = 64 AND root_id NOT GLOB '*[^0-9a-f]*'),
    root_kind TEXT NOT NULL CHECK (root_kind IN ('history', 'transcript', 'data')),
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

CREATE TABLE lineage_session_receipt_results (
    lineage_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    fingerprint TEXT NOT NULL,
    result_revision_id TEXT NOT NULL,
    result_id TEXT NOT NULL CHECK (
        length(result_id) = 64 AND result_id NOT GLOB '*[^0-9a-f]*'
    ),
    PRIMARY KEY (lineage_id, session_id, fingerprint),
    FOREIGN KEY (lineage_id, session_id, fingerprint)
        REFERENCES lineage_session_receipts(lineage_id, session_id, fingerprint) ON DELETE CASCADE,
    FOREIGN KEY (lineage_id, result_revision_id)
        REFERENCES lineage_revisions(lineage_id, revision_id)
) STRICT;

CREATE TABLE lineage_session_receipts (
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

CREATE TABLE lineage_transcript_extent_nodes (
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
    min_history_idx INTEGER CHECK (min_history_idx IS NULL OR min_history_idx >= 0),
    max_history_idx INTEGER CHECK (
        (min_history_idx IS NULL AND max_history_idx IS NULL)
        OR (min_history_idx IS NOT NULL AND max_history_idx IS NOT NULL
            AND max_history_idx >= min_history_idx)
    ),
    PRIMARY KEY (lineage_id, node_id),
    FOREIGN KEY (lineage_id, node_id)
        REFERENCES lineage_sequence_nodes(lineage_id, node_id) ON DELETE CASCADE
) STRICT;

CREATE TABLE lineage_transcript_record_profiles (
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

CREATE TABLE lineage_turn_transitions (
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

CREATE TABLE lineage_turns (
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

CREATE TABLE object_data_roots (
    object_hash TEXT PRIMARY KEY REFERENCES objects(hash) ON DELETE CASCADE,
    lineage_id TEXT NOT NULL,
    root_id TEXT NOT NULL,
    FOREIGN KEY (lineage_id, root_id)
        REFERENCES lineage_sequence_roots(lineage_id, root_id)
) STRICT;

CREATE TABLE objects (
    hash TEXT PRIMARY KEY CHECK (length(hash) = 64 AND hash NOT GLOB '*[^0-9a-f]*'),
    codec TEXT NOT NULL CHECK (codec IN ('none', 'zstd')),
    raw_size INTEGER NOT NULL CHECK (raw_size >= 0),
    stored_size INTEGER NOT NULL CHECK (stored_size >= 0 AND stored_size = length(bytes)),
    bytes BLOB NOT NULL
) STRICT;

CREATE TABLE request_attempts (
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

CREATE TABLE request_object_refs (
    request_attempt_id INTEGER NOT NULL
        REFERENCES request_attempts(id) ON DELETE CASCADE CHECK (request_attempt_id > 0),
    object_hash TEXT NOT NULL REFERENCES objects(hash) ON DELETE RESTRICT,
    role TEXT NOT NULL CHECK (role IN (
        'body_json', 'body_manifest', 'body_top', 'body_item', 'body_parent', 'response', 'error'
    )),
    PRIMARY KEY (request_attempt_id, object_hash, role)
) STRICT;

CREATE TABLE request_stats (
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

CREATE TABLE store_meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL,
    updated_at INTEGER NOT NULL DEFAULT (unixepoch()) CHECK (updated_at >= 0)
) STRICT;

CREATE INDEX lineage_branch_revisions_revision_idx
    ON lineage_branch_revisions(lineage_id, revision_id);

CREATE INDEX lineage_branches_fork_parent_idx
    ON lineage_branches(lineage_id, fork_parent_session_id);

CREATE INDEX lineage_branches_head_revision_idx
    ON lineage_branches(lineage_id, head_revision_id);

CREATE INDEX lineage_branches_initial_revision_idx
    ON lineage_branches(lineage_id, initial_revision_id);

CREATE INDEX lineage_branches_updated_idx
    ON lineage_branches(lineage_id, updated_at DESC, session_id);

CREATE INDEX lineage_entries_child_idx
    ON lineage_sequence_entries(lineage_id, child_node_id);

CREATE INDEX lineage_entries_payload_idx
    ON lineage_sequence_entries(lineage_id, payload_id);

CREATE INDEX lineage_history_index_left_idx
    ON lineage_history_index_nodes(lineage_id, left_node_id);

CREATE INDEX lineage_history_index_right_idx
    ON lineage_history_index_nodes(lineage_id, right_node_id);

CREATE INDEX lineage_history_indexes_node_idx
    ON lineage_history_indexes(lineage_id, index_node_id);

CREATE INDEX lineage_payload_nested_objects_global_idx
    ON lineage_payload_nested_object_refs(object_hash);

CREATE INDEX lineage_payload_nested_objects_idx
    ON lineage_payload_nested_object_refs(lineage_id, object_hash);

CREATE INDEX lineage_payload_objects_global_idx
    ON lineage_payload_object_refs(object_hash);

CREATE INDEX lineage_payload_objects_idx
    ON lineage_payload_object_refs(lineage_id, object_hash);

CREATE INDEX lineage_receipts_prior_idx
    ON lineage_commit_receipts(lineage_id, prior_revision_id);

CREATE INDEX lineage_receipts_result_idx
    ON lineage_commit_receipts(lineage_id, result_revision_id);

CREATE INDEX lineage_request_attempts_branch_idx
    ON lineage_request_attempts(lineage_id, session_id, request_attempt_id);

CREATE INDEX lineage_retained_kind_idx
    ON lineage_retained_revisions(lineage_id, retention_kind, retained_at);

CREATE INDEX lineage_revision_state_projections_projected_idx
    ON lineage_revision_state_projections(lineage_id, projected_payload_id);

CREATE INDEX lineage_revision_state_roots_root_idx
    ON lineage_revision_state_roots(lineage_id, root_id);

CREATE INDEX lineage_revisions_creator_idx
    ON lineage_revisions(lineage_id, created_by_session_id, created_at);

CREATE INDEX lineage_revisions_history_root_idx
    ON lineage_revisions(lineage_id, history_root_id);

CREATE INDEX lineage_revisions_parent_idx
    ON lineage_revisions(lineage_id, parent_revision_id);

CREATE INDEX lineage_revisions_state_payload_idx
    ON lineage_revisions(lineage_id, state_payload_id);

CREATE INDEX lineage_revisions_transcript_root_idx
    ON lineage_revisions(lineage_id, transcript_root_id);

CREATE INDEX lineage_roots_node_idx
    ON lineage_sequence_roots(lineage_id, root_node_id);

CREATE INDEX lineage_session_receipt_results_revision_idx
    ON lineage_session_receipt_results(lineage_id, result_revision_id);

CREATE INDEX lineage_session_receipts_created_idx
    ON lineage_session_receipts(lineage_id, session_id, created_at DESC);

CREATE INDEX lineage_transcript_profiles_block_idx
    ON lineage_transcript_record_profiles(lineage_id, block_idx, payload_id);

CREATE INDEX lineage_turn_transitions_turn_idx
    ON lineage_turn_transitions(lineage_id, session_id, turn_id, transitioned_at_ms);

CREATE INDEX lineage_turns_continuation_idx
    ON lineage_turns(lineage_id, session_id, continuation_of);

CREATE INDEX lineage_turns_revision_idx
    ON lineage_turns(lineage_id, submitted_revision_id);

CREATE INDEX lineage_turns_state_idx
    ON lineage_turns(lineage_id, session_id, turn_state, turn_id DESC);

CREATE INDEX object_data_roots_root_idx ON object_data_roots(lineage_id, root_id);

CREATE INDEX objects_sharing_idx ON objects(raw_size, hash) WHERE raw_size >= 131072;

CREATE INDEX request_attempts_background_idx
    ON request_attempts(background, started_at DESC);

CREATE INDEX request_attempts_body_size_idx
    ON request_attempts(raw_body_size DESC);

CREATE INDEX request_attempts_error_idx
    ON request_attempts(error_summary, started_at DESC);

CREATE INDEX request_attempts_provider_model_idx
    ON request_attempts(provider, model, started_at DESC);

CREATE INDEX request_attempts_request_id_idx ON request_attempts(request_id);

CREATE INDEX request_attempts_started_at_idx
    ON request_attempts(started_at DESC, id DESC);

CREATE INDEX request_attempts_turn_ask_idx ON request_attempts(turn_id, ask_id, id);

CREATE INDEX request_attempts_url_idx ON request_attempts(url);

CREATE UNIQUE INDEX request_object_refs_body_root_idx
    ON request_object_refs(request_attempt_id)
    WHERE role IN ('body_json', 'body_manifest');

CREATE UNIQUE INDEX request_object_refs_error_idx
    ON request_object_refs(request_attempt_id)
    WHERE role = 'error';

CREATE INDEX request_object_refs_object_idx
    ON request_object_refs(object_hash, request_attempt_id);

CREATE UNIQUE INDEX request_object_refs_response_idx
    ON request_object_refs(request_attempt_id)
    WHERE role = 'response';

CREATE INDEX request_stats_input_tokens_idx ON request_stats(input_tokens DESC);

CREATE INDEX request_stats_output_tokens_idx ON request_stats(output_tokens DESC);

CREATE INDEX request_stats_total_cost_idx ON request_stats(total_cost_micros DESC);

CREATE TRIGGER lineage_archive_coordinate_delete
BEFORE DELETE ON lineage_archive_coordinates
WHEN EXISTS (
    SELECT 1 FROM lineage_payload_object_refs
    WHERE lineage_id = OLD.lineage_id AND payload_id = OLD.header_payload_id
)
BEGIN
    SELECT RAISE(ABORT, 'archive coordinates belong to their header payload');
END;

CREATE TRIGGER lineage_archive_coordinate_insert
BEFORE INSERT ON lineage_archive_coordinates
BEGIN
    SELECT CASE WHEN NOT EXISTS (
        SELECT 1 FROM lineage_payload_object_refs
        WHERE lineage_id = NEW.lineage_id AND payload_id = NEW.header_payload_id
          AND payload_kind = 'data'
    ) THEN RAISE(ABORT, 'archive coordinates require a Data header payload') END;
END;

CREATE TRIGGER lineage_archive_coordinate_update
BEFORE UPDATE ON lineage_archive_coordinates
BEGIN
    SELECT RAISE(ABORT, 'archive coordinates are immutable');
END;

CREATE TRIGGER lineage_branch_identity_update
BEFORE UPDATE OF lineage_id, session_id, fork_parent_session_id, parent_session_id,
    initial_revision_id
ON lineage_branches
BEGIN
    SELECT RAISE(ABORT, 'lineage branch identity is immutable');
END;

CREATE TRIGGER lineage_branch_revision_update
BEFORE UPDATE ON lineage_branch_revisions
BEGIN
    SELECT RAISE(ABORT, 'lineage branch revisions are immutable');
END;

CREATE TRIGGER lineage_checkpoint_summary_presence_delete
BEFORE DELETE ON lineage_checkpoint_summary_presence
WHEN EXISTS (
    SELECT 1 FROM lineage_payload_object_refs
    WHERE lineage_id = OLD.lineage_id AND payload_id = OLD.header_payload_id
)
BEGIN
    SELECT RAISE(ABORT, 'checkpoint summary presence belongs to its header payload');
END;

CREATE TRIGGER lineage_checkpoint_summary_presence_insert
BEFORE INSERT ON lineage_checkpoint_summary_presence
BEGIN
    SELECT CASE WHEN NOT EXISTS (
        SELECT 1 FROM lineage_payload_object_refs
        WHERE lineage_id = NEW.lineage_id AND payload_id = NEW.header_payload_id
          AND payload_kind = 'data'
    ) THEN RAISE(ABORT, 'checkpoint summary presence requires a Data header payload') END;
END;

CREATE TRIGGER lineage_checkpoint_summary_presence_update
BEFORE UPDATE ON lineage_checkpoint_summary_presence
BEGIN
    SELECT RAISE(ABORT, 'checkpoint summary presence is immutable');
END;

CREATE TRIGGER lineage_commit_receipt_delete
BEFORE DELETE ON lineage_commit_receipts
BEGIN
    SELECT RAISE(ABORT, 'lineage commit receipts are immutable');
END;

CREATE TRIGGER lineage_commit_receipt_update
BEFORE UPDATE ON lineage_commit_receipts
BEGIN
    SELECT RAISE(ABORT, 'lineage commit receipts are immutable');
END;

CREATE TRIGGER lineage_completed_sequence_node_delete
BEFORE DELETE ON lineage_completed_sequence_nodes
WHEN EXISTS (
    SELECT 1 FROM lineage_sequence_nodes node
    WHERE node.lineage_id = OLD.lineage_id AND node.node_id = OLD.node_id
)
BEGIN
    SELECT RAISE(ABORT, 'completed sequence nodes are immutable');
END;

CREATE TRIGGER lineage_completed_sequence_node_insert
BEFORE INSERT ON lineage_completed_sequence_nodes
BEGIN
    SELECT CASE WHEN NOT EXISTS (
        SELECT 1 FROM lineage_sequence_nodes node
        WHERE node.lineage_id = NEW.lineage_id AND node.node_id = NEW.node_id
          AND node.entry_count = (
              SELECT count(*) FROM lineage_sequence_entries entry
              WHERE entry.lineage_id = node.lineage_id AND entry.node_id = node.node_id
          )
          AND EXISTS (
              SELECT 1 FROM lineage_sequence_entries final
              WHERE final.lineage_id = node.lineage_id AND final.node_id = node.node_id
                AND final.entry_index = node.entry_count - 1
                AND final.cumulative_item_count = node.item_count
                AND final.cumulative_byte_count = node.byte_count
          )
          AND NOT EXISTS (
              SELECT 1 FROM lineage_sequence_entries entry
              LEFT JOIN lineage_sequence_entries previous
                ON previous.lineage_id = entry.lineage_id AND previous.node_id = entry.node_id
               AND previous.entry_index = entry.entry_index - 1
              WHERE entry.lineage_id = node.lineage_id AND entry.node_id = node.node_id
                AND (
                    entry.entry_index >= node.entry_count
                    OR (entry.entry_index = 0 AND (
                        entry.cumulative_item_count != entry.item_count
                        OR entry.cumulative_byte_count != entry.byte_count
                    ))
                    OR (entry.entry_index > 0 AND (
                        previous.entry_index IS NULL
                        OR entry.cumulative_item_count != previous.cumulative_item_count + entry.item_count
                        OR entry.cumulative_byte_count != previous.cumulative_byte_count + entry.byte_count
                    ))
                    OR (entry.entry_kind = 'item' AND NOT EXISTS (
                        SELECT 1 FROM lineage_payload_object_refs payload
                        WHERE payload.lineage_id = entry.lineage_id AND payload.payload_id = entry.payload_id
                          AND node.node_kind = 'leaf' AND node.level = 0
                          AND payload.payload_kind = node.sequence_kind
                          AND payload.byte_count = entry.byte_count AND entry.item_count = 1
                    ))
                    OR (entry.entry_kind = 'child' AND NOT EXISTS (
                        SELECT 1 FROM lineage_sequence_nodes child
                        JOIN lineage_completed_sequence_nodes complete
                          ON complete.lineage_id = child.lineage_id AND complete.node_id = child.node_id
                        WHERE child.lineage_id = entry.lineage_id AND child.node_id = entry.child_node_id
                          AND node.node_kind = 'internal' AND node.level = child.level + 1
                          AND node.sequence_kind = child.sequence_kind
                          AND child.item_count = entry.item_count AND child.byte_count = entry.byte_count
                    ))
                )
          )
    ) THEN RAISE(ABORT, 'cannot complete an invalid sequence node') END;
END;

CREATE TRIGGER lineage_completed_sequence_node_update
BEFORE UPDATE ON lineage_completed_sequence_nodes
BEGIN
    SELECT RAISE(ABORT, 'completed sequence nodes are immutable');
END;

CREATE TRIGGER lineage_history_index_delete
BEFORE DELETE ON lineage_history_indexes
BEGIN
    SELECT RAISE(ABORT, 'history semantic indexes are immutable');
END;

CREATE TRIGGER lineage_history_index_insert
BEFORE INSERT ON lineage_history_indexes
BEGIN
    SELECT CASE WHEN NOT EXISTS (
        SELECT 1 FROM lineage_sequence_roots
        WHERE lineage_id = NEW.lineage_id AND root_id = NEW.history_root_id
          AND root_kind = 'history'
          AND (NEW.index_node_id IS NULL OR EXISTS (
              SELECT 1 FROM lineage_history_index_nodes node
              WHERE node.lineage_id = NEW.lineage_id AND node.node_id = NEW.index_node_id
                AND node.max_history_idx < item_count
          ))
    ) THEN RAISE(ABORT, 'semantic indexes require a history root') END;
END;

CREATE TRIGGER lineage_history_index_node_delete
BEFORE DELETE ON lineage_history_index_nodes
BEGIN
    SELECT RAISE(ABORT, 'history semantic index nodes are immutable');
END;

CREATE TRIGGER lineage_history_index_node_insert
BEFORE INSERT ON lineage_history_index_nodes
WHEN NEW.split_bit >= 0
BEGIN
    SELECT CASE WHEN NOT EXISTS (
        SELECT 1 FROM lineage_history_index_nodes left_child
        JOIN lineage_history_index_nodes right_child
          ON right_child.lineage_id = left_child.lineage_id
        WHERE left_child.lineage_id = NEW.lineage_id
          AND left_child.node_id = NEW.left_node_id AND right_child.node_id = NEW.right_node_id
          AND left_child.min_key = NEW.min_key AND right_child.max_key = NEW.max_key
          AND left_child.max_key < right_child.min_key
          AND NEW.min_history_idx = min(left_child.min_history_idx, right_child.min_history_idx)
          AND NEW.max_history_idx = max(left_child.max_history_idx, right_child.max_history_idx)
          AND (left_child.split_bit = -1 OR left_child.split_bit > NEW.split_bit)
          AND (right_child.split_bit = -1 OR right_child.split_bit > NEW.split_bit)
    ) THEN RAISE(ABORT, 'invalid history semantic children') END;
END;

CREATE TRIGGER lineage_history_index_node_replace
BEFORE INSERT ON lineage_history_index_nodes
WHEN EXISTS (
    SELECT 1 FROM lineage_history_index_nodes
    WHERE lineage_id = NEW.lineage_id AND node_id = NEW.node_id
)
BEGIN
    SELECT RAISE(IGNORE);
END;

CREATE TRIGGER lineage_history_index_node_update
BEFORE UPDATE ON lineage_history_index_nodes
BEGIN
    SELECT RAISE(ABORT, 'history semantic index nodes are immutable');
END;

CREATE TRIGGER lineage_history_index_replace
BEFORE INSERT ON lineage_history_indexes
WHEN EXISTS (
    SELECT 1 FROM lineage_history_indexes
    WHERE lineage_id = NEW.lineage_id AND history_root_id = NEW.history_root_id
)
BEGIN
    SELECT RAISE(IGNORE);
END;

CREATE TRIGGER lineage_history_index_update
BEFORE UPDATE ON lineage_history_indexes
BEGIN
    SELECT RAISE(ABORT, 'history semantic indexes are immutable');
END;

CREATE TRIGGER lineage_payload_nested_object_ref_update
BEFORE UPDATE ON lineage_payload_nested_object_refs
BEGIN
    SELECT RAISE(ABORT, 'lineage nested payload references are immutable');
END;

CREATE TRIGGER lineage_payload_object_ref_update
BEFORE UPDATE ON lineage_payload_object_refs
BEGIN
    SELECT RAISE(ABORT, 'lineage payload references are immutable');
END;

CREATE TRIGGER lineage_request_attempt_update
BEFORE UPDATE ON lineage_request_attempts
BEGIN
    SELECT RAISE(ABORT, 'lineage request attempts are immutable');
END;

CREATE TRIGGER lineage_revision_insert
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

CREATE TRIGGER lineage_revision_state_projection_delete
BEFORE DELETE ON lineage_revision_state_projections
WHEN EXISTS (
    SELECT 1 FROM lineage_payload_object_refs
    WHERE lineage_id = OLD.lineage_id AND payload_id = OLD.original_payload_id
)
BEGIN
    SELECT RAISE(ABORT, 'revision state projections belong to their original payload');
END;

CREATE TRIGGER lineage_revision_state_projection_insert
BEFORE INSERT ON lineage_revision_state_projections
BEGIN
    SELECT CASE WHEN NOT EXISTS (
        SELECT 1 FROM lineage_payload_object_refs original
        JOIN lineage_payload_object_refs projected
          ON projected.lineage_id = original.lineage_id
         AND projected.payload_id = NEW.projected_payload_id
        WHERE original.lineage_id = NEW.lineage_id
          AND original.payload_id = NEW.original_payload_id
          AND original.payload_kind = 'revision_state'
          AND projected.payload_kind = 'revision_state'
    ) OR EXISTS (
        SELECT 1 FROM lineage_revision_state_projections
        WHERE lineage_id = NEW.lineage_id
          AND (original_payload_id = NEW.projected_payload_id
               OR projected_payload_id = NEW.original_payload_id)
    ) THEN RAISE(ABORT, 'invalid revision state projection') END;
END;

CREATE TRIGGER lineage_revision_state_projection_update
BEFORE UPDATE ON lineage_revision_state_projections
BEGIN
    SELECT RAISE(ABORT, 'revision state projections are immutable');
END;

CREATE TRIGGER lineage_revision_state_root_delete
BEFORE DELETE ON lineage_revision_state_roots
WHEN EXISTS (
    SELECT 1 FROM lineage_payload_object_refs
    WHERE lineage_id = OLD.lineage_id AND payload_id = OLD.state_payload_id
)
BEGIN
    SELECT RAISE(ABORT, 'revision archive roots belong to their state payload');
END;

CREATE TRIGGER lineage_revision_state_root_insert
BEFORE INSERT ON lineage_revision_state_roots
BEGIN
    SELECT CASE WHEN NOT EXISTS (
        SELECT 1 FROM lineage_payload_object_refs payload
        JOIN lineage_sequence_roots root
          ON root.lineage_id = NEW.lineage_id AND root.root_id = NEW.root_id
        WHERE payload.lineage_id = NEW.lineage_id
          AND payload.payload_id = NEW.state_payload_id
          AND payload.payload_kind = 'revision_state'
          AND root.root_kind = 'data'
          AND (
              (NEW.role = 'first_user_message' AND root.item_count = 1)
              OR (NEW.role <> 'first_user_message' AND root.item_count % 2 = 0
                  AND (NEW.role <> 'checkpoint' OR root.item_count = 2))
          )
    ) THEN RAISE(ABORT, 'invalid revision archive root') END;
END;

CREATE TRIGGER lineage_revision_state_root_update
BEFORE UPDATE ON lineage_revision_state_roots
BEGIN
    SELECT RAISE(ABORT, 'revision archive roots are immutable');
END;

CREATE TRIGGER lineage_revision_update
BEFORE UPDATE ON lineage_revisions
BEGIN
    SELECT RAISE(ABORT, 'lineage revisions are immutable');
END;

CREATE TRIGGER lineage_sequence_entry_complete
AFTER INSERT ON lineage_sequence_entries
WHEN NEW.entry_index + 1 = (
    SELECT entry_count FROM lineage_sequence_nodes
    WHERE lineage_id = NEW.lineage_id AND node_id = NEW.node_id
)
BEGIN
    INSERT INTO lineage_completed_sequence_nodes (lineage_id, node_id)
    VALUES (NEW.lineage_id, NEW.node_id);
END;

CREATE TRIGGER lineage_sequence_entry_completion_guard
BEFORE INSERT ON lineage_sequence_entries
BEGIN
    SELECT CASE WHEN EXISTS (
        SELECT 1 FROM lineage_completed_sequence_nodes complete
        WHERE complete.lineage_id = NEW.lineage_id AND complete.node_id = NEW.node_id
    ) THEN RAISE(ABORT, 'completed sequence nodes are immutable') END;
    SELECT CASE WHEN NEW.entry_kind = 'child' AND NOT EXISTS (
        SELECT 1 FROM lineage_completed_sequence_nodes complete
        WHERE complete.lineage_id = NEW.lineage_id AND complete.node_id = NEW.child_node_id
    ) THEN RAISE(ABORT, 'sequence child is incomplete') END;
END;

CREATE TRIGGER lineage_sequence_entry_delete
BEFORE DELETE ON lineage_sequence_entries
WHEN EXISTS (
    SELECT 1 FROM lineage_completed_sequence_nodes complete
    JOIN lineage_sequence_nodes node
      ON node.lineage_id = complete.lineage_id AND node.node_id = complete.node_id
    WHERE complete.lineage_id = OLD.lineage_id AND complete.node_id = OLD.node_id
)
BEGIN
    SELECT RAISE(ABORT, 'completed sequence entries are immutable');
END;

CREATE TRIGGER lineage_sequence_entry_insert
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
          AND payload.payload_kind IN ('history', 'transcript', 'data')
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

CREATE TRIGGER lineage_sequence_entry_update
BEFORE UPDATE ON lineage_sequence_entries
BEGIN
    SELECT RAISE(ABORT, 'lineage sequence entries are immutable');
END;

CREATE TRIGGER lineage_sequence_node_insert_guard
BEFORE INSERT ON lineage_sequence_nodes
WHEN EXISTS (
    SELECT 1 FROM lineage_sequence_nodes node
    WHERE node.lineage_id = NEW.lineage_id AND node.node_id = NEW.node_id
)
BEGIN
    SELECT RAISE(ABORT, 'lineage sequence nodes are immutable');
END;

CREATE TRIGGER lineage_sequence_node_update
BEFORE UPDATE ON lineage_sequence_nodes
BEGIN
    SELECT RAISE(ABORT, 'lineage sequence nodes are immutable');
END;

CREATE TRIGGER lineage_sequence_root_insert
BEFORE INSERT ON lineage_sequence_roots
WHEN NEW.root_node_id IS NOT NULL
BEGIN
    SELECT CASE WHEN NOT EXISTS (
        SELECT 1 FROM lineage_sequence_nodes node
        WHERE node.lineage_id = NEW.lineage_id AND node.node_id = NEW.root_node_id
          AND node.sequence_kind = NEW.root_kind
          AND node.level + 1 = NEW.depth
          AND node.item_count = NEW.item_count
          AND node.byte_count = NEW.byte_count
    ) THEN RAISE(ABORT, 'sequence root does not match root node') END;
    SELECT CASE WHEN NOT EXISTS (
        SELECT 1 FROM lineage_completed_sequence_nodes complete
        WHERE complete.lineage_id = NEW.lineage_id AND complete.node_id = NEW.root_node_id
    ) THEN RAISE(ABORT, 'sequence root reaches an incomplete node') END;
END;

CREATE TRIGGER lineage_sequence_root_update
BEFORE UPDATE ON lineage_sequence_roots
BEGIN
    SELECT RAISE(ABORT, 'lineage sequence roots are immutable');
END;

CREATE TRIGGER lineage_session_receipt_delete
BEFORE DELETE ON lineage_session_receipts
BEGIN
    SELECT RAISE(ABORT, 'lineage session receipts are immutable');
END;

CREATE TRIGGER lineage_session_receipt_result_delete
BEFORE DELETE ON lineage_session_receipt_results
WHEN EXISTS (
    SELECT 1 FROM lineage_session_receipts
    WHERE lineage_id = OLD.lineage_id AND session_id = OLD.session_id
      AND fingerprint = OLD.fingerprint
)
BEGIN
    SELECT RAISE(ABORT, 'session receipt results belong to their retained receipts');
END;

CREATE TRIGGER lineage_session_receipt_result_insert
BEFORE INSERT ON lineage_session_receipt_results
BEGIN
    SELECT CASE WHEN NOT EXISTS (
        SELECT 1 FROM lineage_session_receipts receipt
        JOIN lineage_branch_revisions association
          ON association.lineage_id = receipt.lineage_id
         AND association.session_id = receipt.session_id
        JOIN lineage_revisions revision
          ON revision.lineage_id = association.lineage_id
         AND revision.revision_id = association.revision_id
        JOIN lineage_sequence_roots history
          ON history.lineage_id = revision.lineage_id
         AND history.root_id = revision.history_root_id
        WHERE receipt.lineage_id = NEW.lineage_id
          AND receipt.session_id = NEW.session_id
          AND receipt.fingerprint = NEW.fingerprint
          AND revision.revision_id = NEW.result_revision_id
          AND json_extract(receipt.save_receipt_json, '$.session_id') = NEW.session_id
          AND json_extract(receipt.save_receipt_json, '$.lineage_id') = NEW.lineage_id
          AND json_extract(receipt.save_receipt_json, '$.current.revision') = association.branch_sequence
          AND json_extract(receipt.save_receipt_json, '$.current.history_len') = revision.history_len
          AND json_extract(receipt.save_receipt_json, '$.current.transcript_record_count') = revision.transcript_record_count
          AND json_extract(receipt.save_receipt_json, '$.history_text_bytes') = history.byte_count
    ) THEN RAISE(ABORT, 'receipt result does not match its exact saved revision') END;
END;

CREATE TRIGGER lineage_session_receipt_result_update
BEFORE UPDATE ON lineage_session_receipt_results
BEGIN
    SELECT RAISE(ABORT, 'session receipt results are immutable');
END;

CREATE TRIGGER lineage_session_receipt_update
BEFORE UPDATE ON lineage_session_receipts
BEGIN
    SELECT RAISE(ABORT, 'lineage session receipts are immutable');
END;

CREATE TRIGGER lineage_transcript_extent_node_update
BEFORE UPDATE ON lineage_transcript_extent_nodes
BEGIN
    SELECT RAISE(ABORT, 'transcript extent nodes are immutable');
END;

CREATE TRIGGER lineage_transcript_record_profile_update
BEFORE UPDATE ON lineage_transcript_record_profiles
BEGIN
    SELECT RAISE(ABORT, 'transcript record profiles are immutable');
END;

CREATE TRIGGER lineage_turn_identity_update
BEFORE UPDATE OF lineage_id, session_id, turn_id, submitted_history_idx,
    submitted_history_hash, submitted_revision_id, submitted_sequence, turn_kind, continuation_of,
    created_at_ms
ON lineage_turns
BEGIN
    SELECT RAISE(ABORT, 'lineage turn identity is immutable');
END;

CREATE TRIGGER lineage_turn_transition_delete
BEFORE DELETE ON lineage_turn_transitions
BEGIN
    SELECT RAISE(ABORT, 'lineage turn transitions are immutable');
END;

CREATE TRIGGER lineage_turn_transition_update
BEFORE UPDATE ON lineage_turn_transitions
BEGIN
    SELECT RAISE(ABORT, 'lineage turn transitions are immutable');
END;

CREATE TRIGGER object_data_root_delete
BEFORE DELETE ON object_data_roots
WHEN EXISTS (SELECT 1 FROM objects WHERE hash = OLD.object_hash)
BEGIN
    SELECT RAISE(ABORT, 'object data roots belong to their logical object');
END;

CREATE TRIGGER object_data_root_insert
BEFORE INSERT ON object_data_roots
BEGIN
    SELECT CASE WHEN NOT EXISTS (
        SELECT 1 FROM objects object
        JOIN lineage_sequence_roots root
          ON root.lineage_id = NEW.lineage_id AND root.root_id = NEW.root_id
        WHERE object.hash = NEW.object_hash
          AND object.codec = 'none' AND object.stored_size = 0
          AND object.raw_size BETWEEN 131072 AND 67108864
          AND root.root_kind = 'data' AND root.byte_count = object.raw_size
          AND root.depth BETWEEN 1 AND 3 AND root.item_count BETWEEN 2 AND 8192
    ) THEN RAISE(ABORT, 'object data root does not match logical object') END;
END;

CREATE TRIGGER object_data_root_object_update
BEFORE UPDATE ON objects
WHEN EXISTS (SELECT 1 FROM object_data_roots WHERE object_hash = OLD.hash)
BEGIN
    SELECT RAISE(ABORT, 'shared logical objects are immutable');
END;

CREATE TRIGGER object_data_root_update
BEFORE UPDATE ON object_data_roots
BEGIN
    SELECT RAISE(ABORT, 'object data roots are immutable');
END;
