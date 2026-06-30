# Session persistence architecture plan

## Goal

Make session persistence simpler, correct by construction, and easier to modify.

The current design splits durable-state knowledge across `SessionPersistState`, `LiveSession`, SQLite, pending saves, dirty markers, and transcript descriptor state. This makes it possible for one component to claim an unchanged history prefix that SQLite does not actually contain.

The target design has one owner for durable state and one code path that computes save ranges.

## Core principles

1. There is exactly one durable cursor for a document.
2. Only the persistence state machine knows what is stored in SQLite.
3. History storage and persistence state are separate concerns.
4. Save planning always goes through one function.
5. Stale acks may advance durable metadata, but must not drop or compact newer unsaved work.
6. SQLite remains strict and rejects impossible deltas.

## Architectural decisions

These are the intended end-state choices:

1. `SessionDocument` becomes the real owner of mutable session state. `TuiApp.core.session` may remain as a temporary migration mirror, but it should not be the long-term source of truth.
2. Keep the request-append fast path because turn dispatch needs a store-backed `ModelHistorySource` including the submitted user request. Treat it as a validated specialization of the unified save planner.
3. Keep sparse history support. Do not make full materialization the normal implementation strategy.
4. Keep transcript descriptor cache state inside `TranscriptDocument`.
5. On save failure, use the conservative retry strategy: mark history dirty from `0`.
6. On stale save ack, update the durable cursor only. Do not compact or drop any overlay rows on stale acks.
7. Refactor in staged steps with tests after each phase, not as one large persistence rewrite.

## Codebase constraints this plan accounts for

Reviewing the current code changes the plan in four important ways:

1. `TuiApp` still owns `core.session`, and store-backed sessions currently use an empty `core.session.history` as a sparse-session sentinel. The end state should remove that sentinel by moving mutable session document state behind `SessionDocument`.
2. `LiveSession` currently combines sparse history access with persistence state. The end state should keep the sparse history behavior, but remove durable length, store revision, and dirty tracking from it.
3. `TranscriptDocument` already has a separate sparse descriptor model with loaded ranges, total descriptor count, and descriptor dirty generation. The plan should not move descriptor cache internals into `PersistenceState`.
4. Request-history append is a real fast path used before turn dispatch. It should remain, but as a validated specialization of the unified planner, not as a separate persistence model.

## Target model

The final shape should make `SessionDocument` the owner of the mutable session document. `TuiApp` should not need to keep a separate sparse `core.session.history` model that sometimes means "real history" and sometimes means "metadata template for store-backed history".

```rust
struct SessionDocument {
    session: SessionStateProjection,
    history: SessionHistory,
    transcript: TranscriptDocument,
    persistence: PersistenceState,
}

struct SessionStateProjection {
    meta: SessionMetaState,
    rewindable: RewindableState,
}

struct SessionHistory {
    storage: HistoryStorage,
    checkpoint: Option<ContextCheckpoint>,
}

enum HistoryStorage {
    StoreBacked {
        store: SessionStoreRef,
        overlay_start: usize,
        overlay: Vec<HistoryItem>,
    },
    Materialized(Vec<HistoryItem>),
}

struct PersistenceState {
    durable: DurableCursor,
    dirty: DirtyState,
    pending: Option<PendingSave>,
    save_queued: bool,
    writable: bool,
}

struct DurableCursor {
    /// Number of history rows currently stored in SQLite for this session, not
    /// the number of rows that are clean in the current in-memory document.
    store_history_len: usize,
    revision: u64,
}

struct DirtyState {
    history_from: Option<usize>,
    metadata: bool,
    side_tables_from: Option<usize>,
    blobs: bool,
}
```

Transcript descriptor persistence remains coordinated by `SessionDocument`, but the descriptor content, sparse descriptor cache, total descriptor count, and descriptor dirty generation stay inside `TranscriptDocument`. Do not put descriptor cache state into `PersistenceState`; it already has a coherent owner.

The key invariant is:

```rust
save_start <= persistence.durable.store_history_len
```

All save planning must enforce this through one helper:

```rust
fn bounded_history_save_start(
    dirty_from: Option<usize>,
    current_len: usize,
    durable_len: usize,
) -> usize {
    dirty_from.unwrap_or(current_len).min(current_len).min(durable_len)
}
```

## Session history overlay

Replace the persistence responsibilities of `LiveSession` with a pure history abstraction. The overlay may know how to read old rows from a store, but it must not decide what is durable or what needs saving.

```rust
struct SessionHistory {
    storage: HistoryStorage,
    checkpoint: Option<ContextCheckpoint>,
}

enum HistoryStorage {
    StoreBacked {
        store: SessionStoreRef,
        overlay_start: usize,
        overlay: Vec<HistoryItem>,
    },
    Materialized(Vec<HistoryItem>),
}
```

For store-backed history:

- rows below `overlay_start` are read from SQLite
- rows at or after `overlay_start` are read from `overlay`
- `len()` is `overlay_start + overlay.len()`
- after truncating below the stored prefix, set `overlay_start = truncate_index` and clear or replace the overlay
- after a clean persisted ack, set `overlay_start = ack.history_len` and clear the saved overlay prefix

Responsibilities:

- `len()`
- `is_empty()`
- `range(start..end) -> Result<Vec<HistoryItem>, String>`
- `tail(max_items, max_bytes)`
- `append(item) -> usize`
- `truncate_from(index)`
- `replace_from(index, items)`
- `model_history_source(...)`
- `first_live_history_index_for_model_message(...)`
- `effective_mode_at(...)`

Non-responsibilities:

- durable history length
- store revision
- pending save tracking
- dirty generation tracking
- save range selection
- ack/failure handling

Those move to `PersistenceState`.

`HistoryStorage` should not contain a `durable_len` field. If history storage and persistence both know the durable cursor, the bug class returns.

## Session state and side tables

The current `Session` type mixes metadata, history rows, and rewindable side tables. Store-backed resumes currently keep `core.session.history` empty while using the same `Session` as a metadata template. That is an abstraction leak.

The final design should split it:

```rust
struct SessionStateProjection {
    meta: SessionMetaState,
    rewindable: RewindableState,
}

struct RewindableState {
    turn_metas: HistorySnapshots<TurnMeta>,
    metadata_snapshots: HistorySnapshots<SessionMetadataSnapshot>,
    accounting_snapshots: HistorySnapshots<SessionAccountingState>,
    context_tokens: ContextTokenState,
}
```

`SessionStateProjection` is indexed by `SessionHistory::len()`, but it does not own the history rows. Save planning asks it to build:

- `SessionState`
- `SessionSideTableSuffixes`
- rewind/restore metadata at a history boundary

This prevents metadata-only saves, side-table suffix saves, and rewind cleanup from depending on whether history is materialized or sparse.

## Save planning

There should be one general save planner for store-backed and materialized sessions.

```rust
enum SavePlan {
    Skip(SessionSaveSkipReason),
    MetadataOnly {
        generation: DocumentGeneration,
        state: SessionState,
        side_tables: SessionSideTableSuffixes,
    },
    History {
        generation: DocumentGeneration,
        delta: PersistDelta,
    },
    RequestAppend {
        generation: DocumentGeneration,
        delta: PersistDelta,
        descriptor_append: DescriptorAppendSubmission,
    },
}
```

`RequestAppend` stays as an explicit fast path. It is not a second persistence model; it is the same planner proving stronger preconditions:

- `history_index == durable.store_history_len`
- no earlier history dirty state
- no conflicting pending save
- descriptor state can be appended without replacing earlier descriptors

This matters because turn dispatch wants a store-backed `ModelHistorySource` that includes the submitted user request without first materializing all history.

The general planner should do this:

```rust
let current_len = document.history.len();
let history_start = bounded_history_save_start(
    persistence.dirty.history_from,
    current_len,
    persistence.durable.store_history_len,
);
```

Then it builds a delta from `history_start..current_len` through `SessionHistory::range`.

This removes the separate live/full save range logic while preserving the request-append fast path as a validated specialization.

## Ack handling

Pending saves should be immutable snapshots.

```rust
struct PendingSave {
    id: u64,
    session_id: String,
    generation: DocumentGeneration,
    kind: PersistSaveKind,
    history_start: usize,
    history_len: usize,
    descriptor_delta: Option<PendingDescriptorDelta>,
}
```

`PersistAck` should include:

```rust
struct PersistAck {
    save_id: u64,
    session_id: String,
    kind: PersistSaveKind,
    history_len: usize,
    revision: u64,
}
```

Ack algorithm:

```rust
fn ack_save(&mut self, ack: PersistAck, current_generation: DocumentGeneration) {
    let Some(pending) = self.pending.take_matching(&ack) else {
        return;
    };

    self.durable.store_history_len = ack.history_len;
    self.durable.revision = ack.revision;

    if pending.generation == current_generation {
        self.clear_saved_dirty(pending.kind);
        self.history.compact_persisted_prefix(ack.history_len);
        self.transcript.note_persisted(pending.descriptor_delta, &ack);
    } else {
        self.save_queued = true;
        self.transcript.note_stale_persisted(pending.descriptor_delta, &ack);
    }
}
```

Important behavior:

- Matching ack can clear dirty state and compact the saved overlay prefix.
- Stale ack updates `durable.store_history_len` and `durable.revision` only.
- Stale ack must not compact history overlay rows.
- Stale ack must not move `overlay_start`, especially after rewind.
- Stale ack must not clear newer dirty markers.
- Stale ack must not drop newer overlay rows or descriptor records.

## Transcript descriptor persistence

`TranscriptDocument` already owns sparse descriptor state:

- total descriptor count when known
- loaded descriptor ranges
- active descriptor range
- local descriptor records
- descriptor dirty generation

Keep that ownership. The persistence state machine should not duplicate the descriptor cache. It should only remember enough pending-save metadata to acknowledge or reject a descriptor suffix.

Descriptor save planning should stay behind a `TranscriptDocument` method:

```rust
fn descriptor_save_suffix(
    &self,
    descriptors_persisted: bool,
    dirty_history_from: Option<usize>,
) -> TranscriptDescriptorSaveSuffix;
```

The long-term cleanup is to replace the external `descriptors_persisted: bool` with a descriptor persistence cursor owned by `TranscriptDocument`, because descriptor total count and sparse tail loading already live there.

Request append descriptor handling remains special only because it can update `descriptor_total_count` by a known append count without reloading the store.

## Failure handling

On save failure, do not keep retrying the same suffix.

Conservative behavior:

```rust
pending = None;
dirty.history_from = Some(0);
save_queued = true;
```

Optional optimized behavior, only when the durable cursor is trusted:

```rust
dirty.history_from = Some(durable.store_history_len.min(history.len()));
```

The conservative `0` path is simpler and safer.

## Migration plan

### Phase 1: Make current invariants explicit

- Keep the existing structures.
- Add assertions around save planning:
  - `history_start_idx <= durable_history_len`
  - `history_len == history_start_idx + suffix.len()`
- Keep the current prefix clamp.
- Keep the regression tests for interrupt, rewind, stale ack, and prefix errors.

### Phase 2: Introduce `DurableCursor`

- Add `DurableCursor` to `SessionPersistState`.
- Move `persisted_history_len` and current store revision into it.
- Make all save planning read durability from this cursor.
- Stop reading durable length from `LiveSession` in new code.
- Keep `LiveSession` fields temporarily as mirrors only until call sites are converted.

### Phase 3: Introduce `SessionHistory`

- Add `SessionHistory` as the façade for materialized and store-backed history.
- Route `session_history_len`, `session_history_range`, append, truncate, and replace through it.
- Keep existing `LiveSession` as the first implementation if necessary, but expose only history operations through `SessionHistory`.
- Move checkpoint storage into `SessionHistory` once model-history and rewind callers are routed through it.

### Phase 4: Split session metadata from history rows

- Introduce `SessionStateProjection` for metadata and rewindable side tables.
- Stop using `core.session.history.is_empty()` to mean "store-backed sparse history".
- Make state and side-table suffix construction take `SessionHistory::len()` explicitly.
- Move rewind metadata restore/prune methods behind `SessionStateProjection`.

### Phase 5: Make `LiveSession` a pure overlay

Remove from `LiveSession`:

- `persisted_history_len`
- `store_revision`
- `dirty`

Keep only sparse history behavior:

- store reference
- overlay start
- overlay rows
- checkpoint until `SessionHistory` owns it

Rename it to `HistoryStorage::StoreBacked` once call sites are converted.

### Phase 6: Unify full and live save planning

- Replace `FullSessionSaveState` and `LiveSessionSaveState` with one save input.
- Use `SessionHistory::range(history_start..history_len)` for both materialized and sparse sessions.
- Delete separate live/full planner branches where possible.

### Phase 7: Move ack/failure into `PersistenceState`

- `SessionDocument::mark_persisted` should delegate to `PersistenceState::ack_save`.
- `SessionDocument::mark_persist_failed` should delegate to `PersistenceState::fail_save`.
- The ack/failure methods should be the only place that updates durable cursor, pending save, queued save, and dirty state.

### Phase 8: Simplify public mutation surface

Callers should only use:

```rust
document.apply(mutation)
document.prepare_save(blobs_pending)
document.prepare_request_append_save(request)
document.mark_submitted(...)
document.ack_save(ack)
document.fail_save(failure)
```

No caller should mutate:

- live dirty fields
- persisted history length
- descriptor persisted flags
- pending save internals

## Tests required

### Store-backed history

- resume store-backed session and append one row
- resume, append, save, resume again
- repeated resume cycles preserve all history
- rewind to a stored prefix truncates SQLite rows
- rewind to start deletes all rows

### In-flight save behavior

- append while save pending, stale ack does not drop later row
- append transcript block while save pending, stale ack does not clear descriptor dirty state
- interrupt with in-flight save, final flush has no prefix error
- rewind with in-flight save, final flush has no prefix error

### Failure behavior

- prefix error failure forces conservative retry
- retry writes a valid delta
- repeated failure does not spin on the same invalid suffix

### Request append fast path

- request append succeeds only when `history_index == durable.history_len`
- request append falls back to general history save when history or descriptor state is dirty earlier
- request append updates descriptor total count on ack
- request append with side tables persists side-table suffixes from the append index

### Side table and metadata behavior

- metadata-only save is allowed only when current history length equals durable history length
- rewind prunes turn metadata, metadata snapshots, accounting snapshots, and context tokens consistently
- side-table suffix start is bounded by the same save start as history

### Save planner invariants

- `history_start_idx` never exceeds durable history length
- dirty marker past durable length clamps to durable length
- dirty marker past current length clamps to current length then durable length
- metadata-only save requires current length equal durable length

## Desired deletion list

At the end, these should be gone or reduced:

- duplicate persisted history length fields
- duplicate live/full save planners
- direct writes to live dirty state from app code
- direct pending-save mutation outside persistence tests
- manual save-start calculations outside one helper
- `core.session.history` as a sparse-session sentinel
- `LiveSession` as an owner of persistence state

## Acceptance criteria

The refactor is complete when:

1. There is one durable cursor per session document.
2. There is one save range helper.
3. There is one save planner path for sparse and materialized history.
4. `LiveSession` no longer tracks persistence state, or has been replaced by `SessionHistory` / `HistoryStorage::StoreBacked`.
5. Stale acks cannot clear newer dirty work.
6. Save deltas cannot claim an unchanged prefix longer than the durable cursor.
7. SQLite store validation remains strict.
8. `core.session.history` is no longer used as a sparse-session sentinel.
9. Transcript descriptor cache state remains owned by `TranscriptDocument`.
10. Persistence tests cover resume, append, interrupt, rewind, stale ack, request append, side tables, and failure retry.

## Non-goals

- Do not relax SQLite integrity checks.
- Do not make the store silently accept invalid deltas.
- Do not add rewind-specific save hacks.
- Do not add more duplicated durability state to paper over mismatches.
