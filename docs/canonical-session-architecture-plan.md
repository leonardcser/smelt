# Canonical session architecture implementation plan

Status: Implemented and validated

## Purpose

Complete the large-session architecture around one canonical SQLite database per
session. Pressing Enter must wait for one canonical transaction that records the
submitted turn and all state needed to recover it, then dispatch the model
request. Everything reconstructible from that transaction must move outside the
Enter barrier.

The target flow is:

```text
Enter
  -> one canonical session.db transaction:
       session revision
       submitted history suffix
       transcript record suffix
       deterministic turn state = ready
       transactional search rows
  -> durable CommitReceipt
  -> dispatch model request

After commit, asynchronously:
  -> catalog.db projection
  -> active transcript dematerialization
```

This plan builds on the per-session convergence actor and stable writer lease in
`docs/storage-architecture-plan.md`. It preserves the existing SQLite, WAL,
content-addressed object, bounded resume, and transactional search foundations.
Where the plans differ, this plan supersedes the older plan's derived-file,
session-listing, Enter-barrier, and active-transcript-memory sections.

## How to use this plan

1. Approve the architecture and invariants before implementation.
2. Implement one phase at a time, deleting the superseded path at each cutover.
3. Do not keep old and new canonical write paths behind long-lived feature flags.
4. Update this document when code or fault-testing evidence proves that a simpler
   design has the same or stronger invariants.
5. Do not commit, merge, push, or publish the worktree unless explicitly asked.

The final result matters more than literal type names in this document. Concrete
types are shown to make ownership and ordering auditable, not to require an
abstraction layer.

## Engineering decision principles

This plan inherits the decision principles from
`docs/storage-architecture-plan.md`. They govern implementation choices and take
priority over literal adherence to a proposed type, phase, queue, or mechanism.

1. **Prefer fewer moving parts, less state, and fewer abstractions.** Every actor,
   queue, cache, state transition, and recovery branch must justify its continued
   existence. Merge or delete components when one concrete owner can preserve the
   same invariants.
2. **Make responsibilities concrete and composable.** The same small canonical
   update, read, lease, and projection primitives should serve runtime, import,
   repair, fork, maintenance, fixtures, and tests. Do not create parallel paths
   with subtly different revision or durability behavior.
3. **Optimize for reliability and future change, not patch size.** Prefer the
   result that is easier to reason about, test, debug, extend, and operate, even
   when it requires a larger schema, API, module-ownership, or call-site refactor.
   Development cost is not a reason to retain a weaker architecture.
4. **Fix root causes instead of masking symptoms.** Do not add retries, fallback
   readers, compatibility wrappers, reconciliation state, or UI policy merely to
   hide an ownership, ordering, durability, or data-model flaw. Catalog
   reconciliation is justified only because the catalog is explicitly disposable
   derived state; it must never conceal canonical inconsistency. No per-session
   compatibility projection is retained.
5. **Delete superseded mechanisms completely.** A successful cutover removes old
   writers, readers, workers, state variants, tests, metrics, comments, and
   adapters. Do not leave dormant implementations in case the replacement fails.
6. **Introduce an abstraction only when it removes real duplication or makes an
   invariant structurally unavoidable.** Do not generalize for hypothetical
   storage backends, event buses, databases, providers, or future requirements.
7. **Preserve proven components when they remain the simplest robust choice.** A
   greenfield mindset does not require rewriting sound SQLite, WAL, stable lease,
   content-addressing, reader, search, or sparse-resume behavior.
8. **Use end-to-end behavior, fault injection, measurements, and final code review
   as evidence.** Passing a phase checklist is not evidence that unnecessary
   complexity should remain. Reproduce behavior at the same boundaries users hit,
   inspect failure outcomes, and simplify again after tests pass.
9. **Prefer the smallest design that reaches the strong end state.** Large changes
   are acceptable when they reduce total complexity, but this is not permission
   for a maximal rewrite. Keep a smaller concrete design when it provides equal or
   stronger correctness, scalability, and maintainability.
10. **Treat the plan as a maintained architectural model, not ceremony.** If code,
    benchmarks, or fault tests reveal a simpler and stronger design, implement it
    and update this document. Do not preserve a stale requirement merely because
    it was written first.

Success means the resulting code is simpler, more reliable, more composable,
easier to test and work with, and contains fewer band-aid mechanisms. Literal
adherence to this document is not a success criterion.

## Pre-implementation reviewed state

At approval, the worktree provided the following foundation and remaining gaps.
The implementation phases below supersede this historical inventory:

- `crates/tui/src/persist.rs:1220` has one persistence actor per writable
  session, with the authoritative store head and writer lease.
- `crates/tui/src/persist.rs:1734` converts a cumulative save intent into one
  `SessionCommit` and reconciles ambiguous outcomes by fingerprint.
- `crates/store/src/db.rs:1650` applies canonical data through one SQLite
  transaction body.
- `crates/store/src/db.rs:2676` uses WAL with `synchronous = FULL`; SQLite remains
  the journal and durability mechanism.
- `crates/store/src/schema.rs:682` stores canonical session state and revision.
- `crates/store/src/schema.rs:707` and `crates/store/src/schema.rs:718` store
  history and transcript records.
- `crates/store/src/schema.rs:844` keeps transcript search and FTS updates in the
  session database.
- `crates/tui/src/app/agent.rs:181` appends the user message before dispatch, and
  `crates/tui/src/app/agent.rs:214` currently forces pending history durability.
- `crates/tui/src/persist.rs:1885` still writes `meta.json` synchronously after a
  canonical commit. This remains part of Enter latency.
- `crates/tui/src/persist.rs:1142` already coalesces `content.txt` generation off
  the Enter path, but creates one worker per active persistence actor.
- `crates/core/src/session.rs:2168` lists sessions by scanning every session
  directory and reading `meta.json` or opening each `session.db`.
- `crates/core/src/session.rs:2257` revision-stamps derived metadata, and
  `crates/core/src/session.rs:2286` streams revision-stamped derived content.
- `crates/core/src/transcript_model.rs:1280` lazily materializes record-backed
  blocks through `OnceLock`, but hydration cannot be evicted.
- `crates/core/src/transcript_model.rs:1405` keeps every active block entry and
  coupled maps resident for the life of the active session.
- `crates/tui/src/app/transcript.rs:264` has bounded sparse record windows for
  resumed display-only sessions. The same bounded discipline does not yet apply
  to a long-lived active session.

The completed benchmark pass in `docs/transcript-layout-benchmarks.md:149`
provides the pre-architecture baseline. Its performance changes are retained.

## Architectural decision

Adopt the following design:

1. One canonical `session.db` per session.
2. One mutation owner per writable session.
3. One dedicated `SubmitTurn` command and one SQLite transaction attributable to
   each Enter submission.
4. Explicit persisted turn states, not an automatically retried provider outbox.
5. One rebuildable root `catalog.db` used for session listing.
6. One bounded, coalescing worker for the rebuildable catalog projection.
7. Transactional per-session search indexes.
8. Compact durable block references plus byte-budgeted hydration and render
   caches for committed old transcript blocks.
9. SQLite WAL as the only journal. No custom event log is introduced.

### Alternatives rejected

#### Keep synchronous sidecars and optimize them

This preserves correctness only by making reconstructible files part of the
critical path. It scales Enter latency with filesystem behavior and duplicates
canonical state. It is rejected.

#### Persist optimistically after provider dispatch

This reduces apparent latency but allows a provider request to run without a
durable user turn. A crash can lose the input that caused a billed request. It
is rejected as the default behavior.

#### Add a traditional auto-retrying outbox

Most model providers do not offer an end-to-end idempotency guarantee. Retrying
an uncertain request can duplicate output or billing. Persisted turn state is
required, but automatic provider resend is rejected.

#### Make search an asynchronous projection

This would require a search watermark, an unindexed live suffix, query merging,
and additional recovery semantics. The measured transactional FTS and compact
character-mask indexes are fast enough. Asynchronous search is rejected unless
future measurements prove the transaction cost dominates Enter.

#### Use one global canonical database

A global database would simplify cross-session listing but create a global write
domain and weaken failure isolation, fork, export, and delete semantics. A global
catalog is useful only as a rebuildable projection. Global canonical storage is
rejected.

#### Introduce event sourcing or a custom append-only log

SQLite WAL already supplies atomicity, ordering, crash recovery, checksums,
migrations, and indexed reads. A second journal would create replay and
compaction obligations without adding a product capability. It is rejected.

#### Rewrite the storage layer from scratch

A rewrite would keep the same hard requirements while discarding the proven
session database, writer lease, transaction, content-addressing, and sparse read
paths. The correct greenfield boundary is the turn command, projections, and
active-memory model, not the SQLite substrate.

## State ownership

### Canonical state

Canonical state is information that cannot be discarded and rebuilt without
changing user-visible session semantics. It lives only in `session.db`:

- Immutable session identity and fork parent.
- Mutable session metadata and canonical revision.
- Model history and history-indexed side tables.
- Transcript records, origins, durable tool state, and record extents.
- Persisted turn IDs, turn states, and terminal turn outcomes.
- Content-addressed objects and typed references.
- Transactional transcript search text, character masks, and FTS rows.
- Commit fingerprint and receipt used for ambiguous-outcome reconciliation.

A change to canonical user/session state advances the session revision exactly
once. Request audit append, catalog projection, cache activity, and compatibility
export do not advance it.

### Durable auxiliary state

Request audit rows that were successfully captured live in `session.db` for
failure isolation and inspection, but they are best-effort diagnostics rather
than canonical session semantics. They do not participate in SubmitTurn
durability, do not advance session revision, and are never used to reconstruct
history, transcript, turn state, or search.

### Derived state

Derived state may lag, fail, be deleted, or be rebuilt without changing session
semantics:

- Root `catalog.db`.
- In-memory session-list overlays.
- Loaded record windows.
- Hydrated transcript blocks.
- Render plans, exact-height measurements, rows, and layout caches.
- Metrics and diagnostic summaries.

No derived-state failure may roll back a canonical commit or prevent model
dispatch after a successful submit receipt.

### Ephemeral state

Ephemeral state is process-local and is never treated as a recovery source:

- TUI focus, cursor, and transient notification state.
- Actor epoch and channel state.
- Provider stream handles and cancellation handles.
- Cache LRU order and pin counts.
- In-flight projector scheduling state.

## Core invariants

### Canonical database

1. `session.db` is the only source of truth for a session.
2. Exactly one writable owner holds the stable session lease.
3. Every canonical transaction verifies the fenced owner token.
4. Revision is checked, never wrapped or saturated.
5. A canonical transaction either commits all supplied metadata, history,
   records, turn state, and search changes, or none of them.
6. The session commit fingerprint includes every supplied canonical field and
   turn mutation.
7. Exact replay of an ambiguously acknowledged transaction returns the persisted
   receipt rather than applying the mutation twice.
8. WAL checkpoints and catalog writes are not part of the Enter barrier.

### Enter

1. Enter creates one `SubmitTurn` command containing all unacknowledged canonical
   changes needed by that request.
2. Queued but not yet started ordinary save intent is folded into the
   `SubmitTurn` command instead of being committed first.
3. The `SubmitTurn` command executes one canonical SQLite transaction.
4. Enter may wait behind a canonical transaction that was already in flight, but
   it does not create a preliminary flush transaction.
5. The provider request is not dispatched until a durable submit receipt is
   validated and applied to the document.
6. Catalog and compatibility projections are scheduled only after durability and
   never awaited by Enter.
7. A failed canonical submit does not dispatch the provider request.
8. The exact submitted input remains retryable in memory after a failed commit.

### Turn state

1. Every dispatched provider request has one durable turn row.
2. A new submitted turn is committed in `ready` state.
3. Dispatch acceptance advances it to `running` in a later revisioned mutation.
4. Final history, turn metadata, and terminal state are committed atomically.
5. Terminal states are immutable except through an explicit maintenance repair.
6. On writable startup, every `ready` or `running` turn becomes `interrupted`.
7. Recovery never automatically resends a provider request.
8. Retrying an interrupted turn is an explicit user action that creates a new
   turn linked to the interrupted turn.

### Projections

1. Projection requests carry only session identity and revision, or a deletion
   tombstone. They never carry history or transcript snapshots.
2. Workers reopen canonical SQLite and read one revision-pinned snapshot.
3. Requests coalesce by session ID to the highest requested revision.
4. Queues and pending maps have explicit bounds.
5. Queue overflow degrades to a bounded full reconciliation request, not silent
   permanent staleness or unbounded memory.
6. Projection output records the source revision.
7. Normal projection never overwrites a newer projection with an older revision.
8. Full reconciliation treats canonical SQLite as authoritative and repairs an
   impossible catalog revision that is ahead of the database.

### Active transcript memory

1. Mutable or uncommitted blocks remain materialized.
2. A committed block becomes evictable only after the document applies a receipt
   proving its record is durable.
3. Visible blocks, overscan, active streaming blocks, pending tool blocks, and
   explicit operation targets are pinned.
4. Unpinned hydrated blocks are evicted by byte budget, not entry count.
5. Hydration reads bounded record ranges from SQLite.
6. Search does not hydrate every matching block.
7. A single required block larger than a budget may temporarily exceed it, but
   all unpinned entries are evicted and the oversize debt is measured.
8. Cache eviction never changes canonical dirtiness, record generations,
   block IDs, navigation semantics, or scroll anchors.

## Canonical submit transaction

### Concrete command surface

Use a dedicated command rather than a generic repository or event bus. Names may
change during implementation, but the information boundary should remain:

```rust
struct SubmitTurn {
    generation: PersistenceGeneration,
    acknowledged_head: StoreHead,
    identity: SessionIdentity,
    metadata: SessionMetadata,
    history: HistorySuffix,
    side_tables: SideTableSuffixes,
    transcript_records: Option<TranscriptRecordSuffix>,
    turn: NewTurn,
}

struct NewTurn {
    kind: TurnKind,
    submitted_history_idx: HistoryIndex,
    continuation_of: Option<TurnId>,
    created_at_ms: u64,
}

struct CommitReceipt {
    session_id: SessionId,
    generation: PersistenceGeneration,
    previous: StoreHead,
    current: StoreHead,
    turn_id: TurnId,
}
```

The TUI supplies its acknowledged head so stale document state is rejected. The
actor remains authoritative for the current database head and verifies that the
document's acknowledged head matches before applying the command.

The store exposes a dedicated `submit_turn` operation. Internally it and ordinary
canonical saves call one private `apply_canonical_update_in_transaction`
primitive. This avoids duplicate SQL while preserving concrete public commands.

Do not add a generic `Repository`, `Event`, `Projection`, or storage backend
interface. The codebase has one SQLite backend and concrete commands are easier
to audit.

### Transaction body

The store performs this ordering inside one SQLite transaction:

1. Begin the transaction under the stable writer lease.
2. Verify owner token, session ID, immutable identity, and expected `StoreHead`.
3. Validate suffix coordinates and object references before destructive writes.
4. Insert referenced content-addressed objects.
5. Apply history and history-indexed side-table suffixes.
6. Apply transcript record suffixes.
7. Update `transcript_search`, `transcript_search_chars`, and FTS triggers in the
   same transaction.
8. Calculate the one next session revision after all canonical changes are known.
9. Allocate the next per-session turn ID and insert the turn in `ready` state,
   linked to the submitted history row and new revision.
10. Update `session_state`, lengths, revision, and timestamps.
11. Persist the exact command fingerprint and complete receipt.
12. Commit once.

A turn insert guarantees that a submit transaction is never a no-op and advances
revision exactly once.

### Turn ID allocation

Keep the current protocol-compatible integer turn ID, but make it durable and
per-session:

- Read `session_state.next_turn_id` inside the submit transaction.
- Use that value for the new row and advance the stored sequence with checked
  arithmetic in the same transaction.
- Check overflow against SQLite and Rust integer limits.
- Persist the allocated ID in the commit receipt used by ambiguous-outcome
  reconciliation.
- Initialize runtime turn sequencing from SQLite rather than resetting to `1` on
  every process start.
- Never decrement the sequence or reuse an ID after rewind, maintenance, or a
  committed turn.

Because one fenced owner serializes writes, allocation is deterministic and does
not scan the turn table. If a transaction rolls back, the sequence increment also
rolls back and retry sees the same next ID. If commit acknowledgement is
ambiguous, the persisted fingerprint and receipt recover the committed ID.

### Enter application flow

Replace the save-plus-flush sequence around `crates/tui/src/app/agent.rs:181` and
`crates/tui/src/app/agent.rs:214` with:

```text
validate input and model configuration
  -> apply the user input to the TUI document as dirty state
  -> build cumulative SubmitTurn from the acknowledged head
  -> send SubmitTurn to the session actor
  -> actor folds any not-started desired save into the command
  -> wait for CommitReceipt
  -> validate and apply receipt to the document
  -> create store-backed ModelHistorySource at receipt.current.history_len
  -> send StartTurn with receipt.turn_id and receipt.current.revision
  -> schedule running-state mutation and derived projections
```

The actor control lane needs a request/reply `SubmitTurn` variant. It must not
route Enter through the latest-value wake plus a separate `Flush`, because that
obscures how many transactions the barrier executed.

A normal autosave that is already executing is allowed to finish. A queued
latest-value save that has not started is superseded by the cumulative submit
command. Tests must distinguish serialization wait from transactions caused by
Enter.

### Submit failure semantics

- Validation failure: no transaction starts, no provider dispatch occurs, and the
  submitted input remains available for correction.
- SQLite or filesystem failure before commit: the transaction rolls back, no
  provider dispatch occurs, and persistence becomes visibly blocked.
- Ambiguous commit result: retain the lease, reopen once, compare the exact
  fingerprint, and either recover the persisted receipt or repeat the exact
  transaction once only when the store head proves it did not commit.
- Failure after durable receipt but before dispatch: the turn remains `ready` and
  recovery marks it `interrupted`.
- Engine channel rejection after receipt: enqueue a canonical `failed`
  transition. If that write also fails, recovery still converts `ready` to
  `interrupted`.
- No error path deletes the durable user message to make the transcript appear as
  if the request never existed.

## Persisted turn model

### Schema

Bump the per-session schema once, add a durable turn sequence to
`session_state`, and add a dedicated table:

```sql
ALTER TABLE session_state
ADD COLUMN next_turn_id INTEGER NOT NULL DEFAULT 1 CHECK (next_turn_id > 0);

CREATE TABLE turns (
    turn_id INTEGER PRIMARY KEY CHECK (turn_id > 0),
    submitted_history_idx INTEGER NOT NULL CHECK (submitted_history_idx >= 0),
    submitted_history_hash TEXT NOT NULL
        CHECK (length(submitted_history_hash) = 64
               AND submitted_history_hash NOT GLOB '*[^0-9a-f]*'),
    submitted_revision INTEGER NOT NULL UNIQUE CHECK (submitted_revision > 0),
    kind TEXT NOT NULL CHECK (kind IN ('user', 'command', 'continuation', 'note')),
    state TEXT NOT NULL CHECK (
        state IN ('ready', 'running', 'completed', 'interrupted', 'failed', 'cancelled')
    ),
    continuation_of INTEGER REFERENCES turns(turn_id) ON DELETE RESTRICT,
    created_at INTEGER NOT NULL CHECK (created_at >= 0),
    started_at INTEGER CHECK (started_at IS NULL OR started_at >= created_at),
    finished_at INTEGER CHECK (finished_at IS NULL OR finished_at >= created_at),
    terminal_reason TEXT,
    CHECK (
        (state = 'ready' AND started_at IS NULL AND finished_at IS NULL)
        OR (state = 'running' AND started_at IS NOT NULL AND finished_at IS NULL)
        OR (state IN ('completed', 'interrupted', 'failed', 'cancelled')
            AND finished_at IS NOT NULL)
    )
);
CREATE INDEX turns_state_idx ON turns(state, turn_id);
CREATE INDEX turns_history_idx ON turns(submitted_history_idx, turn_id);
```

The store validates the submitted history coordinate and hash when creating the
turn. The coordinate deliberately is not a foreign key: rewind may later remove
that history suffix, while the historical turn outcome and monotonic ID remain
valid. `submitted_history_hash` preserves a stable identity for diagnostics
without duplicating the submitted body.

If completion data needs structure beyond the existing `turn_metas`, store a
small typed terminal kind plus a bounded human-readable reason. Do not persist
provider response bodies again in this table.

### State machine

Allowed normal transitions are:

```text
ready -> running
ready -> failed
ready -> cancelled
ready -> interrupted

running -> completed
running -> failed
running -> cancelled
running -> interrupted
```

`running` is written after the engine accepts dispatch. The one mutation owner
serializes this transition before subsequent engine history or completion
mutations. Completion commits final history, records, turn metadata, and
`completed` in one transaction.

Cancellation records `cancelled` only after the cancellation outcome is known.
Provider errors record `failed` with a bounded category and summary. Process or
ownership loss records `interrupted` during the next writable open.

### Restart recovery

After acquiring the writer lease and before enabling new edits, execute one
recovery transaction:

```sql
UPDATE turns
SET state = 'interrupted',
    finished_at = :startup_time,
    terminal_reason = 'process_restart'
WHERE state IN ('ready', 'running');
```

The actor first verifies that the head loaded by the document matches the
pre-recovery database head. It then performs recovery and returns a recovery
receipt for the document to apply before edits are enabled. This prevents the
actor's own recovery revision from looking like an external writer conflict.

Advance the canonical revision once if any rows changed and schedule projections
for that revision. A read-only open reports these rows as interrupted in its
projection without mutating the database; the next writable owner performs the
canonical transition.

The UI shows interrupted turns and offers explicit actions such as copy input or
retry. Retry creates a new turn with `continuation_of` pointing to the old turn.
It never mutates the old row back to `ready` and never automatically resends.

## Root session catalog

### Role and location

Create a rebuildable SQLite database at:

```text
<smelt-state-dir>/catalog.db
```

It is a fast read model for session lists, filters, and resume metadata. It is
not authoritative for open, fork, delete, search, or recovery. All exact session
operations still validate the target `session.db`.

### Catalog schema

Use an independent catalog schema version. The initial shape should contain the
fields needed by current `SessionMeta` list consumers plus projection status:

```sql
CREATE TABLE catalog_meta (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    schema_version INTEGER NOT NULL,
    next_scan_id INTEGER NOT NULL,
    completed_scan_id INTEGER NOT NULL,
    reconciled_at INTEGER
);

CREATE TABLE sessions (
    id TEXT PRIMARY KEY,
    title TEXT,
    slug TEXT,
    first_user_message TEXT,
    cwd TEXT,
    mode TEXT,
    reasoning_effort TEXT,
    model TEXT,
    fast_mode INTEGER,
    parent_id TEXT,
    context_tokens INTEGER,
    history_len INTEGER,
    text_bytes INTEGER,
    created_at INTEGER,
    updated_at INTEGER,
    source_revision INTEGER,
    status TEXT NOT NULL CHECK (status IN ('available', 'unavailable')),
    error_kind TEXT,
    error_summary TEXT,
    last_seen_scan INTEGER NOT NULL
);

CREATE INDEX sessions_updated_idx ON sessions(updated_at DESC, id);
CREATE INDEX sessions_cwd_updated_idx ON sessions(cwd, updated_at DESC, id);
CREATE INDEX sessions_status_updated_idx ON sessions(status, updated_at DESC, id);
```

Unavailable rows may retain the last successful summary and revision so a corrupt
or temporarily unreadable session remains visible. `status` and the error fields
make that state explicit. Exact open still ignores cached metadata and reads the
canonical database.

### Projection request and queue bounds

Use concrete commands:

```rust
struct ProjectionRequest {
    session_id: SessionId,
    minimum_revision: Revision,
}

enum CatalogCommand {
    Project(ProjectionRequest),
    Remove(SessionId),
    ReconcileAll,
}
```

The scheduler contains:

- A capacity-1 wake channel.
- A map from session ID to the highest pending revision.
- A `reconcile_all` flag.
- A fixed maximum number of distinct pending sessions, initially 1,024.

A repeated request replaces the lower revision. If adding a distinct session
would exceed the map bound, clear the map, set `reconcile_all`, and wake once.
This bounds memory and preserves eventual correctness. The worker records the
overflow and performs one root reconciliation instead of retaining arbitrary
work.

The catalog worker always reopens `session.db` read-only. It reads a consistent
SQLite snapshot, derives the small summary, and commits one catalog upsert. It
never receives or clones transcript content from the session actor.

### Revision rules

- A normal upsert writes the actual canonical revision read from SQLite, not just
  the requested minimum.
- If actual revision is below the requested minimum, leave the request pending
  for one post-publication retry, then fall back to reconciliation rather than a
  timed retry loop.
- Normal upsert uses a revision guard so stale work cannot overwrite a newer row.
- An equal revision may repair status or metadata after a prior projection
  failure.
- A full reconciliation compares exact canonical revision and may replace a
  catalog row that is impossibly ahead, because the catalog is disposable.
- Delete commits the canonical session deletion first, then asynchronously writes
  a catalog tombstone/removal. In-process listing overlays hide the deleted ID
  immediately.

### Startup reconciliation

Run reconciliation asynchronously at startup and when catalog creation, schema
validation, or integrity checks require a rebuild.

All catalog mutations acquire a stable OS lock at
`<smelt-state-dir>/.catalog.lock`. Normal projection holds it only for a short
upsert/remove. Full reconciliation holds it across the scan so two smelt
processes cannot interleave scan IDs, stale-row deletion, and new-session
projection. This lock protects derived state only and is never awaited by Enter
or a canonical session transaction. Catalog readers continue through SQLite WAL.

Under that lock:

1. Allocate a new scan ID in `catalog_meta`.
2. Enumerate valid session directory IDs with the existing path and symlink
   safety rules.
3. For each session, compare and project canonical metadata, revision, history
   length, and text bytes. Record unavailable rows rather than hiding corrupt
   sessions.
4. Mark each observed row with the scan ID.
5. Only after a complete successful root scan, delete rows not seen in that scan
   and publish `completed_scan_id`.
6. If the process crashes or root enumeration fails, do not perform stale-row
   deletion. The next scan repairs it.

Do not hold one SQLite write transaction while opening every session database.
Each row can be projected independently under the catalog OS lock; only final
stale-row deletion and scan completion need one short SQLite transaction. A
canonical create/delete that races the scan schedules its projection after the
lock is released, so the scan cannot delete a concurrently inserted catalog row.

Catalog corruption is handled under the same lock by closing it, moving the
corrupt derived file aside for diagnostics when safe, creating a fresh catalog,
and reconciling. A catalog failure never modifies a session database.

### Listing cutover and live overlay

Replace the directory scan at `crates/core/src/session.rs:2168` with indexed
catalog reads. Migrate every caller, including Lua session APIs and the inspect
server.

The active process overlays its current in-memory session summary and deletion
tombstones on the catalog page before sorting. The overlay is small and contains
only list metadata plus canonical revision. It ensures a just-committed active
session appears immediately even when the projector is delayed.

Catalog APIs should return both rows and reconciliation status so the UI can show
that a first-run rebuild is in progress. Existing list APIs that cannot expose
status read currently available rows without falling back to a synchronous root
scan.

Pagination and filters must execute in SQLite. Do not load all catalog rows and
sort them in Rust for a bounded list view.

## Derived-state boundaries

The root catalog is the only derived persistent session projection. Session
listing reads `catalog.db`; search, resume, exact metadata, forks, and inspection
read canonical `session.db` files. No per-session metadata or transcript sidecars
are generated, read, diagnosed, or repaired.

## Transactional search

Keep search tables and triggers in each canonical session database.

1. Record/history mutation and corresponding search rows commit together.
2. `transcript_search_chars` remains a compact candidate filter for covered
   one-character queries.
3. FTS result ordering continues to use FTS rowid without a temporary sort.
4. Exact `instr` verification remains after character-mask filtering.
5. Catalog contains list/filter metadata only, not a second full transcript
   search index.
6. Cross-session search opens canonical databases in bounded parallel batches and
   issues indexed queries. It does not concatenate every full SQLite search blob
   in memory.
7. No search watermark or eventually indexed live suffix is introduced.

If future measurements show search-row work dominates SubmitTurn, first optimize
row preparation and SQL. Moving indexing asynchronous requires a separate design
review with explicit query-merging semantics.

## Active-session transcript dematerialization

### Problem

The sparse resume path is bounded, but the active `BlockHistory` eventually
retains every block and every `OnceLock` hydration. Record-backed blocks can
become fully materialized but never become light again. Coupled `tool_states`,
origin, content-hash, layout, and render caches can also retain old content.

The target is not to remove all O(block-count) metadata. It is to make retained
full-content memory proportional to the configured working-set budgets rather
than total committed transcript bytes.

### Representation

Replace permanent lazy hydration with three explicit states:

```rust
enum BlockEntry {
    Live(Block),
    Stored(StoredBlockRef),
    Hydrated {
        stored: StoredBlockRef,
        block: Arc<Block>,
        weight: usize,
    },
}

struct StoredBlockRef {
    block_id: BlockId,
    record_index: usize,
    kind: BlockKind,
    content_hash: u64,
    estimated_rows: Option<u32>,
    estimated_text_bytes: u64,
    preview: CompactPreview,
    origin: Option<BlockOrigin>,
    tool_call_id: Option<CompactString>,
}
```

Exact fields should match operations that can remain record-only. Do not put
full record JSON, raw tool output, or complete tool state in
`StoredBlockRef`. Load those from SQLite when an operation actually needs them.

`Live` is mutable and dirty-capable. `Stored` is a compact durable reference.
`Hydrated` is clean, reconstructible, and evictable. A mutation targeting a
stored or hydrated block first hydrates it, then promotes it to `Live` and lowers
the record dirty boundary.

### Ownership boundary

Keep SQLite access out of `smelt-core` transcript domain types:

- Core defines compact entry state, promotion, installation, and eviction
  primitives.
- `TranscriptDocument` in the TUI owns the session record reader and decides
  which ranges to hydrate.
- Rendering explicitly calls `ensure_hydrated(range)` before code that requires
  full blocks.
- Record-only methods continue to provide kind, preview, navigation target,
  extent estimate, tool identity, and content hash without hydration.

Replace APIs that assume every `block(id)` can return a permanently borrowed full
`&Block`. Prefer explicit prehydration followed by scoped access. Do not hide
SQLite I/O inside an innocent immutable accessor.

### Eviction eligibility

A block is evictable only when all conditions hold:

1. Its record index is below the durable record length in the latest
   applied receipt.
2. It is before every current record/history dirty boundary.
3. It is not streaming, a tool draft, pending confirmation, or otherwise mutable.
4. It is not part of the active turn's live suffix.
5. It is not in the visible viewport or configured overscan.
6. It is not pinned by copy/yank, inspection, Lua detail access, rewind, or an
   active semantic anchor operation.
7. Durable SQLite contains enough data to hydrate it exactly.

Checkpoint markers, compaction previews, and other non-persisted blocks remain
`Live` until removed. A completed tool block may be stored only after its durable
tool state and record are acknowledged.

### Hydration triggers

Hydrate only bounded targets for:

- Visible viewport plus overscan.
- Exact layout measurement for an imminent viewport.
- Current search-result reveal.
- Copy, yank, inspect, or Lua APIs that explicitly request full block content.
- Rewind/edit operations that target a committed block.
- Navigation operations whose compact record cannot answer the query.

Search candidate discovery, session listing, total extent estimation, and normal
scrolling over unloaded ranges must remain record-only.

### Byte budgets

Create one concrete transcript cache policy with independently measurable limits:

```rust
struct TranscriptMemoryBudget {
    hydrated_blocks: usize,
    record_windows: usize,
    rendered_rows: usize,
}
```

Start with a 64 MiB default total split as:

- 32 MiB hydrated block payloads.
- 16 MiB loaded record windows.
- 16 MiB rendered rows and layout payloads.

Treat these as measured defaults, not API guarantees. Keep them centralized and
configurable for tests. Tune only from the 50 MiB and 500 MiB benchmark matrix.

Weight entries by retained heap capacity, including strings, vectors, JSON
values, and uniquely owned `Arc` payloads. Do not use source text length alone.
The accounting need not match the allocator byte-for-byte, but tests must prove
that insertion, replacement, promotion, and eviction cannot silently bypass it.

Each cache maintains LRU order for unpinned entries and evicts until it is under
budget. Required pinned content may exceed a budget. Record `pinned_bytes` and
`oversize_debt_bytes`, evict every unpinned entry, and converge under budget when
pins are released.

### Post-commit dematerialization

After applying a canonical receipt, schedule a bounded idle-loop compaction pass
in the TUI:

1. Advance the known durable history and record prefixes.
2. Convert eligible old `Live` entries to compact `StoredBlockRef` values.
3. Remove duplicated old full tool state and origin payloads from side maps after
   their compact fields are installed.
4. Preserve block IDs, ordering, hashes, and navigation generations.
5. Process at most a configured number of blocks or bytes per UI tick.
6. Enforce cache budgets after each slice.

Do not mutate `BlockHistory` from a projection thread. Dematerialization is local
UI state maintenance and must occur at safe reducer/idle boundaries.

### Model-history boundary

This work bounds transcript presentation memory. Provider request construction
is still necessarily linear in the active model-history bytes sent to the
provider. Preserve the store-backed durable history prefix and small live suffix;
load full uncheckpointed provider history only for request preparation. Continue
to use checkpointing/compaction for truly large model context.

## Failure and recovery matrix

| Boundary | Required result |
|---|---|
| Before SubmitTurn transaction | No canonical change, no dispatch |
| During history/record/search writes | Full rollback, no dispatch |
| After turn insert but before commit | Full rollback, no turn visible |
| WAL commit before receipt delivery | Fingerprint reconciliation returns exact receipt |
| Receipt delivered before provider dispatch | Durable `ready`; restart marks `interrupted` |
| Dispatch accepted before `running` commit | `ready` or `running`; restart marks either `interrupted` |
| Provider stream during disk full | Stop further canonical progress, best-effort cancel, recover as interrupted |
| Final history before terminal commit | Nonterminal durable turn, restart marks interrupted |
| Terminal commit succeeds | Final history, records, metadata, search, and terminal state agree |
| Catalog write fails | Session remains usable; bounded warning and later reconciliation |
| Catalog is stale or missing | Read available rows, overlay active state, reconcile asynchronously |
| Catalog is corrupt | Recreate derived database and reconcile |
| Cache hydration fails | Surface unavailable content for that operation; never invent or discard canonical state |
| Cache budget is exceeded by pins | Record debt, evict unpinned entries, converge after unpin |

Disk-full behavior must be tested both in the deterministic store fault seam and,
where supported, with SQLite `max_page_count` or a quota-limited filesystem.
Never rely only on mocked error mapping.

## Migration and rollback strategy

### Per-session schema

1. Add `session_state.next_turn_id` and the `turns` table in one transactional
   migration from the current schema.
2. Validate their columns, indexes, foreign keys, checks, and SQL shape using the
   existing exact schema validation convention.
3. Do not synthesize historical turns from old history. Existing sessions start
   with no turn rows and `next_turn_id = 1`.
4. There can be no legitimate running provider process across a binary restart,
   so no old in-flight state needs backfill.
5. Keep the migration additive. Do not rewrite history or record tables for
   this phase.

Migration failure rolls back the transaction and leaves the previous schema
readable by the previous binary. Once the schema bump commits, binary downgrade
is not supported by the current strict schema model. Do not add dual writes or a
shadow old schema solely for downgrade. Recovery is roll-forward with a fixed
binary or restore from the user's backup.

### Catalog schema

The catalog is disposable:

- On incompatible schema, failed migration, or integrity failure, rebuild it from
  canonical session databases.
- Never block session open on catalog migration.
- Reconstruct it only from canonical session databases.

### Runtime cutover

Perform atomic code-path cutovers within the implementation branch:

- Store submit API before TUI Enter cutover.
- New Enter command and old save-plus-flush path must not coexist after cutover.
- Catalog listing replaces directory scanning in one phase.
- Permanent `OnceLock` hydration is deleted when byte-budgeted hydration lands.

No release should depend on an intermediate phase that has two competing sources
of truth.

## Implementation phases

The phase order is dependency-aware guidance, not ceremony. Combine, split, or
reorder phases when doing so creates a cleaner cutover with fewer temporary
paths. Every completed phase must still leave one production implementation for
the behavior it touches. Do not merge a phase that leaves a dormant compatibility
module, dual canonical path, or unexplained state machine behind.

After each phase, reassess the implementation against the engineering principles:

- Can any owner, queue, state, fallback, or abstraction now be deleted?
- Do runtime, import, repair, fork, fixtures, and tests use the same concrete
  primitive?
- Is a recovery mechanism solving an unavoidable failure boundary or masking a
  design flaw?
- Does end-to-end and fault-injection evidence match the intended terminal state?
- Did measured latency, memory, and complexity justify every retained mechanism?

Change direction and update the plan when the answers support a simpler design
with equal or stronger invariants.

### Phase 0: Lock invariants and baselines

1. Add this plan and obtain approval.
2. Inventory every canonical and derived read/write path, test fixture, Lua
   exposure, inspect endpoint, and cleanup path.
3. Add test instrumentation for:
   - canonical transaction count by operation;
   - provider dispatch timestamp and turn ID;
   - catalog writes inside the Enter interval;
   - projection queue depth and revision;
   - hydrated, pinned, and evicted bytes.
4. Preserve the current benchmark logs and rerun one release smoke baseline before
   changing behavior.
5. Add an end-to-end test that presses Enter through the real TUI, persistence
   actor, SQLite store, and engine command capture.

Exit criteria:

- The current barrier and failure behavior are reproducible.
- Every canonical and derived path is identified.
- Metrics can prove the later one-transaction and no-derived-write claims.

### Phase 1: Add canonical turn state and store SubmitTurn

1. Add typed `TurnId`, `TurnState`, `TurnKind`, `NewTurn`, `TurnTransition`, and
   receipt types in `smelt-store`.
2. Add and validate the `turns` schema migration.
3. Refactor the private transaction body so ordinary save and SubmitTurn share
   validation and canonical apply logic.
4. Implement deterministic turn ID allocation inside the transaction.
5. Include turn mutation in fingerprinting and persisted receipt recovery.
6. Implement revisioned running and terminal transition commands.
7. Implement writable-open interruption recovery.
8. Add store unit/integration tests for every state transition, invalid
   transition, overflow, rollback, idempotent repeat, and foreign-key invariant.

Exit criteria:

- A store-level SubmitTurn commits history, records, search, and `ready` in one
  transaction.
- Ambiguous outcome recovery returns the original turn ID.
- No provider-facing code has changed yet.

### Phase 2: Cut Enter over and remove derived work from the barrier

1. Add request/reply SubmitTurn handling to the per-session actor.
2. Fold queued not-started desired state into SubmitTurn.
3. Replace `save_session` plus `flush_persist` before dispatch with the dedicated
   receipt wait.
4. Use the persisted turn ID for `StartTurnPayload`.
5. Enqueue `running` only after dispatch acceptance.
6. Commit terminal state with final canonical history and metadata.
7. Remove all per-session derived-file generation from
   `PersistenceActor::complete_commit`.
8. Ensure Enter schedules but never waits for catalog work.
9. Add E2E crash and dispatch-order tests before optimizing latency.

Exit criteria:

- A normal Enter causes exactly one SubmitTurn transaction and then dispatch.
- No catalog filesystem write occurs in the Enter interval.
- Canonical failure prevents dispatch and preserves retryable input.
- Restart never auto-resends a ready/running turn.

### Phase 3: Add catalog projection and migrate listing

1. Add focused catalog schema/access code in `smelt-store`, taking an explicit
   catalog path.
2. Add root-path resolution and catalog service orchestration in core/TUI code.
3. Implement bounded coalescing, revision guards, deletion commands, and status.
4. Implement startup/full reconciliation with scan IDs and crash-safe stale-row
   deletion.
5. Add active-session and deletion overlays.
6. Move Lua session list, resume picker, inspect server, and other list consumers
   to paged catalog queries.
7. Remove directory-scan behavior from normal listing.
8. Keep exact open/resume/fork/delete validation against `session.db`.

Exit criteria:

- Listing cost depends on requested page size and SQLite index work, not the number
  of session directories opened.
- Missing, stale, corrupt, and ahead-of-source catalog rows are repaired.
- A just-committed active session is immediately visible through overlay.
- Deleting `catalog.db` loses no canonical information.

### Phase 4: Remove compatibility exports

1. Delete per-session metadata and transcript sidecar generation.
2. Delete the export worker, snapshot API, status diagnostics, maintenance command,
   metrics, cleanup paths, and compatibility debt entry.
3. Update storybook and test fixtures to seed canonical SQLite directly.
4. Keep catalog projection as the only derived persistent session service.

Exit criteria:

- No per-session sidecar is generated or consumed.
- List, search, resume, fork, inspection, and maintenance use canonical SQLite or
  the rebuildable catalog according to their ownership boundaries.
- No exporter thread, queue, lock, cancellation seam, or compatibility API remains.

### Phase 5: Bound active transcript memory

1. Measure retained bytes by block/record/tool/render category in an active
   50 MiB and 500 MiB session.
2. Introduce compact stored block references without full record JSON.
3. Replace permanent `OnceLock` hydration with explicit install/promote/evict
   operations.
4. Refactor rendering and detail APIs to prehydrate bounded ranges.
5. Add byte accounting and LRU eviction for hydrated blocks, record windows,
   and rendered payloads.
6. Add pin scopes for viewport, active turn, tool mutation, copy/yank, inspect,
   Lua detail, and rewind operations.
7. Add receipt-driven incremental dematerialization on bounded idle slices.
8. Compact duplicated old tool-state and origin maps.
9. Verify search, navigation, anchors, rewinds, forks, and record suffix saves
   across eviction and rehydration.
10. Tune default budgets only from release benchmarks.

Exit criteria:

- Retained full block content is bounded by configured budgets plus measured pins
  and one required oversize block.
- Active session behavior matches non-evicting behavior in differential tests.
- Scrolling and hydration remain viewport/range bounded.
- No permanent hydration primitive remains.

### Phase 6: Harden recovery, migration, observability, and cleanup

1. Run subprocess crash tests at every transaction, dispatch, and projection
   boundary.
2. Test disk full, permissions, ownership loss, catalog corruption, and stale
   revisions.
3. Add doctor output for nonterminal turns and catalog lag without treating
   derived lag as canonical corruption.
4. Add metrics listed below with bounded labels and no user content.
5. Remove superseded functions, worker state, tests, comments, and metrics.
6. Reconcile `docs/storage-architecture-plan.md`, compatibility docs, Lua docs,
   and benchmark docs with the final implementation.
7. Run a simplification review. Delete any interface or queue that has only one
   use and does not enforce an invariant.

Exit criteria:

- There is one canonical submit path and one catalog path.
- Recovery outcomes are deterministic and user-visible.
- No old path remains dormant.

#### Phase 6 recovery evidence

The hardened implementation uses production SQLite transactions and concrete
workers rather than a generic failure-injection framework:

- `subprocess_crashes_cover_submit_transaction_and_receipt_boundaries` aborts an
  actual `SubmitTurn` before begin, after history, record, search, and ready
  inserts, and after WAL commit. Pre-commit crashes reopen with no canonical
  change; post-commit exact replay returns turn 1 without duplication.
- Enter harness tests cover receipt publication before dispatch, engine rejection,
  dispatch before the `running` transition, final transition failure, writable
  restart, and real resume. They prove that canonical failure preserves input and
  prevents dispatch, while ready/running restart becomes visible `interrupted`
  state without automatic resend.
- Store rollback, terminal transition, streamed history, SQLite
  `max_page_count`, ownership, migration, and process-lock tests cover disk full,
  statement failure, final-history atomicity, owner death/loss, and stale schema
  boundaries.
- `subprocess_crashes_leave_catalog_projection_absent_or_complete`, interrupted
  scan tests, revision-guard tests, queue-overflow reconciliation, 100,000-row
  pagination, and corrupt-catalog rebuild cover projection boundaries and repair.
- `session doctor` integration coverage reports ready turns and catalog lag while
  remaining read-only and preserving canonical health.

### Phase 7: Final performance, memory, and quality validation

1. Run the complete release benchmark matrix with `TMPDIR=~/tmp`.
2. Run isolated peak-RSS tests for each large workload.
3. Run full workspace tests, Clippy, formatting, coverage, and diff checks.
4. Run language-server diagnostics on changed Rust files.
5. Run relevant storybook snapshots for any changed persistence or transcript UI.
6. Compare all acceptance criteria below against the preserved baseline.
7. Update benchmark documentation with methodology, raw log paths, results, and
   remaining limits.
8. Do not retain complexity that lacks material benefit or a clear invariant.

#### Phase 7 validation evidence

- The complete three-run projection/navigation matrix passed. Five-run release
  medians for Enter were 38.279 ms at 50,000 short rows and 27.938 ms at 2,000
  rows x 8 KiB, improving on the preserved baselines by 16.02 and 24.16 percent.
  Every sample attributed exactly one `submit_turn` transaction, two history rows,
  and one record row to Enter, with zero invariant-history or search-blob rows,
  one last-user block scanned, at most two record-rank entries scanned, and
  provider dispatch only after the durable receipt.
- Five-run 50 MiB and 500 MiB search measurements passed the fixed-cost and scaling
  limits. The 500 MiB means were 11.296 ms for absent one-character submit, 3.426
  ms for common FTS submit, 3.628 ms for sparse common FTS submit, and 10.914 ms
  after append. Candidate pages stayed bounded at 512 blocks and record
  hydration used known canonical extents without table recounts.
- Sparse resume retained 77,249 B at 50 MiB and 80,969 B at 500 MiB. The 240-frame
  resumed wheel took 324.922 and 335.140 ms respectively, with zero foreground
  record loads and two row-index rebuilds. Active 50 MiB and 500 MiB sessions
  both retained 915 hydrated blocks and about 32 MiB of hydrated content; the 500
  MiB process peak was only 15.7 MiB higher, and working-set rereads stayed zero.
- Indexed catalog-page medians changed by at most 5 us between 1,000 and 100,000
  rows while opening zero session databases. Streamed 50 MiB and 500 MiB exports
  completed in 86.260 and 396.789 ms with 21,868 and 22,620 KiB peak RSS. A held
  compatibility-export lock did not prevent bounded canonical shutdown.
- The final post-reflection workspace run passed 4,680 tests with 12 skipped.
  Clippy passed across all targets with warnings denied, and line coverage was
  82.67 percent. The standalone storybook run passed all 187 stories with
  app-story filesystem isolation held for each story's full lifetime. Formatting,
  language-server diagnostics on every changed Rust file, and `git diff --check`
  also passed.
- `docs/transcript-layout-benchmarks.md` records the complete commands, methodology,
  measurements, acceptance comparisons, remaining limits, and raw output paths.
  Cached record rank, hydration membership, and retained-byte accounting were
  kept because the 500 MiB measurements prove material scaling benefits. Final
  reflection centralized record-count updates with the entry-transition
  helpers, leaving full recounts only at bulk projection and external-mutation
  seams. No generic rank tree, LRU framework, repository, event log, or SQLite
  abstraction was added.

## Test plan

### Store tests

- SubmitTurn creates one history/record/search/turn revision atomically.
- Failure at each statement boundary rolls back all tables.
- Turn ID allocation starts at one, is monotonic, and rejects overflow.
- Command fingerprint changes for every canonical turn field.
- Exact ambiguous retry returns the same turn ID and revision.
- Stale head, identity mismatch, bad history link, and bad record link fail
  before commit.
- Every allowed state transition succeeds; every other transition fails.
- Terminal state and final history commit together.
- Startup recovery changes ready/running only and advances revision once.
- Search queries observe either the old complete revision or new complete
  revision, never mixed rows.
- WAL reopen after injected ambiguous failure recovers without a custom journal.

### Actor tests

- SubmitTurn supersedes queued not-started desired save.
- SubmitTurn waits behind but does not duplicate an already-running commit.
- One receipt maps to one engine dispatch.
- Catalog failure cannot change SubmitTurn result.
- Engine send rejection schedules failed state.
- Actor panic after receipt leaves ready for interruption recovery.
- Ownership loss prevents submit and preserves dirty input.
- Projection maps and channels stay within bounds under request floods.
- Projectors receive IDs/revisions only.

### Catalog tests

- Empty/missing catalog rebuilds from canonical databases.
- Stale lower revision updates to canonical revision.
- Impossible higher revision is repaired by full reconciliation.
- Missing session rows appear; deleted session rows disappear only after complete
  scans or explicit tombstones.
- Interrupted scan never deletes unseen rows.
- Corrupt/unreadable sessions remain visible as unavailable.
- Catalog corruption rebuild loses no sessions.
- Pagination ordering is stable for equal timestamps.
- Active overlay wins over stale catalog and deletion overlay hides stale rows.
- Queue overflow produces reconciliation and eventual convergence.

### Transcript cache tests

- Only acknowledged durable blocks become stored/evictable.
- Dirty, streaming, tool-draft, and pinned blocks never evict.
- LRU byte accounting covers insert, replacement, promotion, and removal.
- Tiny test budgets force deterministic eviction.
- Oversized pinned block records debt and converges after unpin.
- Hydration preserves exact content, tool state, origin, hash, and block ID.
- Mutation after hydration promotes to live and lowers dirty boundaries.
- Search result reveal hydrates only a bounded neighborhood.
- Rewind/truncate across stored blocks produces the same canonical suffix as a
  fully materialized transcript.
- Scroll anchors and exact heights survive eviction/rehydration.
- Differential randomized operations compare bounded and non-evicting models.

### End-to-end crash tests

Use subprocess kill points around:

1. Before transaction begin.
2. After history insert.
3. After record/search insert.
4. After ready-turn insert.
5. After WAL commit before receipt publication.
6. After receipt before engine send.
7. After engine send before running transition.
8. During streamed history persistence.
9. Before terminal commit.
10. After terminal commit.
11. During catalog upsert.

For every case, reopen through the real session UI path and assert transcript,
turn state, search, listing status, and absence of automatic provider resend.

## Performance and memory acceptance criteria

### Enter

- One `submit_turn` transaction is attributable to a normal Enter.
- Zero catalog writes, full-history invariant scans, full search blob builds, or
  full transcript clones occur in the barrier.
- Work is proportional to changed canonical suffix bytes/rows plus SQLite commit
  and transactional index work, not unchanged transcript size.
- Release medians for the existing 50K short-row and 2K x 8 KiB Enter workloads
  must not regress by more than 5 percent from 45.579 ms and 36.840 ms
  respectively. The expected result is an improvement after removing synchronous
  metadata export.
- Any retained complexity must show a material latency, memory, or correctness
  benefit.

### Catalog

- Reading the first page performs one indexed catalog query and opens zero
  session databases.
- Page latency scales with page size, not total session-directory count.
- Reconciliation remains asynchronous and bounded in memory.
- A stress fixture with at least 100,000 catalog rows verifies indexed ordering,
  filtering, and pagination without loading all rows into Rust.
- Projection lag converges to zero after the worker drains, absent a persistent
  external error.

### Active memory

- Hydrated, record-window, and render-cache retained bytes stay within their
  configured budgets except explicit measured pins/oversize debt.
- After idle dematerialization, full-content retained memory does not grow with
  total committed transcript bytes.
- Compare active 50 MiB and 500 MiB sessions in isolated processes. The 500 MiB
  case may add compact per-block references and SQLite mappings, but must not
  retain an additional 450 MiB of block content.
- First render, wheel scrolling, search, and `n` navigation must remain bounded to
  visible/range work and preserve or improve the current release measurements.
- Hydration churn must not cause repeated SQLite reads while a block remains in
  the working set.

### Search

- Existing 50 MiB and 500 MiB search benchmarks must preserve their current
  complexity. Operations at or above 5 ms must remain within 5 percent unless the
  new result is faster. For sub-5 ms operations, use the larger of 5 percent or a
  0.6 ms fixed-cost floor, and require the 500 MiB scaling case to remain within 5
  percent or improve.
- SubmitTurn search work scales with changed records only.
- Search results at a committed revision agree with transcript records at
  that revision.

## Observability

Add bounded metrics without session IDs, user text, prompts, or secrets:

### Submit and turn metrics

- `persist:submit_turn:queue_wait_ms`
- `persist:submit_turn:transaction_ms`
- `persist:submit_turn:transactions`
- `persist:submit_turn:history_rows`
- `persist:submit_turn:record_rows`
- `persist:submit_turn:index_rows`
- `persist:submit_turn:dispatch_after_receipt_ms`
- `persist:turn:ready_to_running_ms`
- `persist:turn:interrupted_on_startup`
- `persist:turn:transition_failures`

### Catalog metrics

- pending distinct sessions and overflow count
- projection duration and source revision lag
- reconciliation scanned/available/unavailable/removed counts
- catalog query duration and returned row count
- rebuild and integrity-failure counts

### Transcript memory metrics

- live, stored, and hydrated block counts
- retained bytes by cache category
- configured budget, pinned bytes, and oversize debt
- eviction count/bytes and hydration count/bytes/duration
- dematerialized count/bytes per idle slice
- SQLite record rows read per viewport/search/reveal

Existing perf instrumentation can host hot-path counters. Long-lived status should
use the existing metrics system. Do not add high-cardinality labels.

## Documentation updates during implementation

- This plan remains the architecture source until implementation completes.
- Update `docs/storage-architecture-plan.md` to mark superseded sections and the
  implemented foundation accurately.
- Update `docs/transcript-layout-benchmarks.md` with final Enter, catalog, search,
  hydration, and memory results.
- Update Lua session-list/search API docs if return status or pagination behavior
  changes.
- Document interrupted-turn behavior and explicit retry in user-facing session
  recovery docs.

## Definition of done

This checklist records the current best end state. If implementation evidence
supports a simpler design with equal or stronger correctness, update the relevant
item and architecture text rather than preserving a stale requirement. The
architecture is complete only when the resulting plan and code satisfy all of the
following:

1. One `session.db` remains the canonical source for each session.
2. Enter waits for one dedicated canonical SubmitTurn transaction and a durable
   receipt, then dispatches exactly once in process.
3. The submitted message, record, search rows, revision, and `ready` turn
   state commit atomically.
4. Ready/running recovery is deterministic, visible, and never auto-resends.
5. Session listing reads the rebuildable root catalog and overlays live state.
6. Catalog work is asynchronous, revisioned, bounded, and repairable.
7. No per-session derived metadata or transcript sidecar exists.
8. No catalog operation remains in the Enter barrier.
9. Search indexing remains transactionally consistent.
10. Committed old transcript content dematerializes under explicit byte budgets.
11. Hydration is bounded, evictable, and behaviorally equivalent to full
    materialization.
12. SQLite WAL is the only journal.
13. Crash, disk-full, stale-projection, migration, and interruption tests pass.
14. Performance and memory acceptance criteria pass in release mode.
15. Full tests, Clippy with warnings denied, formatting, coverage, diagnostics,
    storybook snapshots, and `git diff --check` pass.
16. Superseded code and compatibility-shaped internal fallbacks are deleted, not
    merely unused.
17. Runtime, import, repair, fork, maintenance, fixtures, and tests share the same
    concrete canonical update and read primitives rather than parallel protocols.
18. No generic repository, storage-backend, event-bus, actor-framework, or async
    SQLite abstraction exists without demonstrated duplication or an invariant
    that requires it.
19. End-to-end behavior, failure injection, release measurements, and final code
    review prove the terminal states and resource bounds described by the plan.
20. The final implementation is simpler to reason about, test, debug, extend, and
    operate than the paths it replaces.
