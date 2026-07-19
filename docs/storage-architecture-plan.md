# Storage architecture implementation plan

Status: Proposed, reviewed against the current implementation

## Purpose

Replace the current multi-owner persistence orchestration with one concrete,
per-session convergence actor. Preserve the proven SQLite data model, bounded
read path, canonical history semantics, and transaction invariants, but remove
special cases and compatibility-shaped runtime abstractions that only exist to
coordinate the current save pipeline.

The target is not a generic storage framework. It is one document change
tracker, one latest desired update, one session actor, one ownership lease, and
one canonical transaction path.

## How to use this plan

This plan sets the direction and desired end state. It is not a contract to
implement every proposed type, phase, or mechanism literally. If implementation
work, code evidence, or fault testing reveals a simpler and stronger design, use
that design and update this document to match it.

Judge every decision by the resulting system:

1. Prefer fewer moving parts, less state, and fewer abstractions.
2. Make responsibilities concrete and composable so the same small primitives
   solve runtime, import, repair, fork, and test needs without parallel paths.
3. Improve reliability, correctness, testability, debuggability, and ease of
   future change, even when doing so requires a larger refactor.
4. Fix root causes instead of adding retries, fallbacks, reconciliation state,
   compatibility wrappers, or UI policy that only masks a symptom.
5. Delete superseded mechanisms completely. Do not leave old paths dormant in
   case the replacement fails.
6. Introduce an abstraction only when it removes real duplication or makes an
   invariant structurally unavoidable. Do not generalize for hypothetical
   backends or future requirements.
7. Preserve proven components when they remain the simplest robust choice. A
   greenfield mindset does not require rewriting sound SQLite, WAL, reader, or
   content-addressing behavior.
8. Use end-to-end behavior, failure injection, and final code review as evidence.
   Passing a phase checklist is not evidence that an unnecessarily complex
   implementation should remain.

Large changes to schema, APIs, module ownership, and call sites are acceptable
when they reduce total system complexity. Development cost and patch size are
not reasons to retain a weaker architecture. Conversely, do not turn this into a
maximal rewrite when a smaller concrete design reaches the same end state.

Success means the final code is simpler, more reliable, more composable, easier
to test and work with, and contains fewer band-aid mechanisms. Literal adherence
to this document is not a success criterion.

## Reviewed decision

Adopt a **per-session convergence actor**:

```text
TuiSessionDocument
    |
    | latest SessionSaveIntent { generation, cumulative canonical suffixes }
    v
SessionPersistence actor
    |
    | SessionCommit { expected actor-owned StoreHead }
    v
OwnedSessionWriter { stable lease + replaceable SQLite connection }
    |
    v
session.db
```

There is no timed retry subsystem. The actor performs at most one structural
connection recovery and idempotency reconciliation after a
connection-invalidating failure. If that does not restore a known-safe state,
persistence becomes visibly blocked until an explicit user retry or a deliberate
lifecycle action.

### Alternatives rejected

- **Direct synchronous saves:** smallest concurrency model, but canonical writes,
  first publication, and filesystem faults would block the TUI at arbitrary save
  points. Keep synchronous use only for explicit offline commands.
- **Thin global FIFO worker:** preserves the current cross-session state machine,
  queues large obsolete snapshots, and still requires UI pending/retry
  coordination. It fixes enqueue symptoms rather than ownership.
- **Per-session FIFO actor:** improves ownership but retains multiple queued
  snapshots and barrier/coalescing rules. A latest-value convergence slot matches
  the actual desired-state semantics with less state.
- **Event sourcing or one global database actor:** adds replay, migration,
  compaction, and global contention without a product requirement.
- **Full storage rewrite:** replacing per-session SQLite, WAL, canonical history,
  content addressing, and bounded readers would discard the strongest parts of
  the current design. The correct greenfield rewrite boundary is the
  orchestration, ownership lease, canonical updater, and flawed semantic schema,
  while retaining the proven SQLite substrate.

## Corrections found by reviewing the plan against the code

The initial plan had several mismatches that this revision removes.

1. Current `DocumentGeneration` is a pair of session and descriptor generations
   with equality only at `crates/core/src/session_save.rs:8-20`. It has no valid
   total ordering, so `desired > durable` and newest-intent coalescing were not
   well-defined. Replace it with one TUI-owned monotonic
   `PersistenceGeneration(u64)` advanced once for every canonical mutation.
2. Store-backed `LiveSession` deliberately retains only the unsaved suffix at
   `crates/core/src/session_runtime.rs:10-17`. The document must therefore keep
   an acknowledged durable prefix for bounded intent construction. The actor is
   authoritative for commits, while the document keeps a read-only projection
   of acknowledged `StoreHead` for suffix planning and live-prefix compaction.
3. `PreparedSessionSave::RequestHistoryAppend` and
   `DescriptorAppendSubmission` are optimizations around pending-save state at
   `crates/tui/src/app/session_document.rs:177-223`. A normal cumulative intent
   can represent the same one-item history and descriptor suffix. Delete the
   specialized request-append save path.
4. Full and live saves currently have separate orchestration at
   `crates/tui/src/app/history.rs:1723-1871`, although they produce the same
   canonical suffix shape. Replace both with one intent builder over either a
   materialized or store-backed history source.
5. Derived files are rebuilt by reading canonical SQLite at
   `crates/core/src/session.rs:2382-2398`. They do not belong in save intents.
   The actor refreshes them from the committed database after durability.
6. `session.lock` currently lives inside the directory being staged, renamed,
   or deleted at `crates/store/src/access.rs:800-840`. First publication releases
   that lock before rename and reacquires after publication. A stable lease must
   instead live under the sessions root and remain held across database close,
   staging rename, publication reconciliation, and reopen.
7. Descriptor replacement occurs after revision calculation at
   `crates/store/src/db.rs:1407-1438`. A descriptor-only canonical change can
   therefore leave revision unchanged. One canonical apply function must decide
   revision after metadata, history, side tables, and descriptors have all
   contributed to the change result.
8. Save id exists mainly to match core pending state. The canonical fingerprint
   already identifies an exact retry at `crates/store/src/db.rs:1325-1330`.
   Remove both `SaveId` and the proposed replacement `OperationId`; actor status
   correlates durability with `PersistenceGeneration`.
9. Request audits can arrive from stale host work after a session boundary. A
   per-session actor must reject audits whose session epoch does not match the
   active actor rather than reopening an arbitrary old or current session.
10. Ephemeral, initially read-only, and ownership-lost sessions have different
    unsaved semantics. Do not fake durability for ephemeral work, and do not
    hide dirty work merely because ownership was lost.
11. `SessionPersistState` and all of its callers exist only for the TUI. Keeping
    that state machine in `smelt-core` creates a cross-crate abstraction without
    another consumer. Delete `crates/core/src/session_save.rs` and keep the much
    smaller change tracker beside `TuiSessionDocument`.
12. `SessionSnapshot` duplicates the canonical write path and shares the manual
    revision logic that caused the `fast_mode` omission. Remove the snapshot
    write protocol after fixtures, imports, and offline synthesis use the same
    canonical update path.
13. Request payload hashes are stored both on `request_attempts` and in
    `request_object_refs` at `crates/store/src/schema.rs:544-583`, leaving two
    sources that can disagree. Make typed reference rows authoritative and remove
    the duplicate body, response, and error hash columns.
14. `RestoreCwd` changes canonical `Session.cwd` but intentionally reports no
    dirty state at `crates/tui/src/app/session_document.rs:1471-1474`. A later
    unrelated save can therefore persist an ungenerated runtime fallback. Delete
    this mutation: restored/fallback process CWD stays in app runtime state;
    only an explicit user CWD change mutates canonical session metadata.
15. Moving to a root lock can race an already-running binary that still holds
    only legacy `session.lock`. Opening a pre-root-lock schema must acquire root
    lock first and legacy lock second, hold both through the schema bump, then
    use only the root lease. Otherwise the migration itself can create two
    writers.

## Architectural invariants

### Document changes

1. One `PersistenceGeneration(u64)` is scoped to one in-memory session document.
2. Every applied mutation that changes canonical metadata, history, side tables,
   or transcript descriptors increments it exactly once.
3. Generation increment uses checked arithmetic. Overflow is a fatal invariant
   failure, never saturation or wraparound.
4. The document tracks one last acknowledged durable generation and one
   acknowledged `StoreHead` projection.
5. For a writable durable session, equal current and durable generations means
   clean. Unequal generations mean unsaved work.
6. Intent submission never clears dirty ranges.
7. An older durable observation may update diagnostics, but cannot clear dirty
   state or discard newer live history.
8. Ephemeral documents intentionally do not participate in durable-generation
   accounting. Initially read-only documents do not claim writable durability.
9. Ownership loss after local mutation preserves an explicit unsaved state even
   though further writes are disabled.

### Desired update handoff

1. The actor handle contains one latest-value save slot, not a FIFO of large save
   payloads.
2. Submitting a newer intent atomically replaces the older queued intent and
   sends a lightweight wake signal.
3. Mailbox fullness cannot reject canonical desired state. A pending wake means
   the actor will inspect the latest slot.
4. Channel disconnection is returned synchronously and leaves the document
   visibly dirty.
5. The slot contains at most one queued intent in addition to one in-flight
   intent.
6. Request audits and lifecycle controls use a separate bounded control lane.
   They cannot displace the latest canonical intent.

### Actor ownership

1. One actor is bound to one session ID and one session epoch.
2. Actor save commands do not carry arbitrary session IDs.
3. The actor owns the authoritative database head and creates every expected
   commit head.
4. The document may use only acknowledged head coordinates as conservative
   suffix-planning hints.
5. The actor serializes canonical commits, publication, sidecar refresh, and
   request audits.
6. Flush targets a `PersistenceGeneration` and a deadline.
7. The actor never silently opens a different session to service a stale audit.

### Transactions and recovery

1. The store executes one transaction body once per attempt.
2. No store-level elapsed retry loop exists.
3. SQLite uses only a short primitive busy timeout.
4. Busy under the stable exclusive lease is an invariant violation or
   unsupported external writer, not a reason for exponential retry.
5. After a connection-invalidating failure, the actor retains the lease,
   replaces the connection once, and reconciles the exact commit fingerprint.
6. The exact commit may be repeated once only when reconciliation proves it did
   not commit and the store head still matches its expected head.
7. Otherwise the actor preserves the latest intent and becomes blocked.
8. Disk full, permissions, unavailable paths, integrity failures, unsupported
   schema, and ownership failures do not loop automatically.
9. Explicit user retry performs one new recovery attempt against the latest
   intent.
10. Shutdown uses the same flush path and never starts another retry policy.

### Ownership and publication

1. A stable lock at `sessions/.locks/<session-id>.lock` is authoritative for
   create, open, migrate, publish, maintain, fork, and delete.
2. Stable lock files are never deleted, avoiding inode replacement races.
3. The database owner token fences canonical and request-audit transactions.
4. Closing or invalidating SQLite does not release the stable lock.
5. Staged publication closes SQLite as required by the platform, renames under
   the stable lease, fsyncs the root, reopens at the destination, and verifies
   the same owner token.
6. Publication is idempotently reconcilable if rename succeeded but a later
   fsync or reopen failed.
7. The lease is released only on explicit close, completed maintenance, or
   verified ownership loss.
8. Best-effort audit failure cannot release or poison canonical ownership.

### Canonical data

1. `session.db` is the only canonical representation.
2. Immutable identity, mutable metadata, history, side tables, descriptors,
   fingerprint, receipt, and revision are handled by one transaction path.
3. Session ID, creation time, and fork parent are immutable after first insert.
4. Revision advances once if any canonical section changes, including
   descriptor-only replacement, and never advances for request audits or
   sidecars.
5. Revision increment is checked against SQLite integer limits.
6. Request audits remain separate best-effort transactions.
7. Sidecars are regenerated from SQLite only after canonical success.
8. Object bytes are content-addressed and semantically untyped. Interpretation
   lives on each reference.

## Target responsibilities

### TUI session document

`TuiSessionDocument` owns:

- Current materialized or store-backed session projection.
- Current transcript projection.
- One total persistence generation.
- Generation inequality for metadata-only changes and the earliest dirty history
  index for suffix construction.
- Transcript descriptor dirty range already owned by the transcript model.
- The last acknowledged durable generation and `StoreHead` projection.
- Construction of one cumulative, store-ready `SessionSaveIntent`.
- Clearing dirty state only after current-generation durability.
- Compacting the `LiveSession` prefix after a safe matching receipt.

It does not own:

- An in-flight or pending save object.
- Save IDs.
- Retry or reopen state.
- The authoritative current database head.
- Separate full, live, metadata-only, and request-append submission protocols.

`smelt-core` continues to own `Session`, `LiveSession`, transcript domain types,
and conversion helpers that have non-TUI callers. It does not own an
interactive persistence state machine. Delete `crates/core/src/session_save.rs`.

### Session persistence actor

The actor owns:

- The fixed session ID and session epoch.
- Existing-session open or lazy first-session creation.
- The stable session lease and replaceable SQLite connection.
- The authoritative durable `StoreHead`.
- One latest desired intent and at most one in-flight commit.
- Exact-fingerprint reconciliation after ambiguous commit failure.
- Staged publication and publication recovery.
- Sidecar refresh after canonical commit.
- Best-effort request-audit serialization.
- Explicit blocked recovery, generation-targeted flush, and close.
- Typed latest-status publication and coalesced wake notification.

It does not own a general task scheduler, timer wheel, exponential backoff, or
cross-session writer cache.

For an existing writable session, construction receives the document's loaded
head and acknowledged generation. The actor acquires the lease, reads the actual
head, and must match the loaded head before edits are enabled. A mismatch forces
a document reload or a visible read-only/conflict result; startup never silently
rebases a projection loaded before ownership. A replacement actor may start from
a document that is already dirty: it keeps the document's acknowledged
generation, adopts the verified actual store head as authority, and applies the
new cumulative intent from its conservative boundary.

### Store

The store owns:

- Stable session lease acquisition and path safety.
- Existing and staged writer lifecycle.
- SQLite connection configuration and schema migration.
- One canonical apply transaction.
- Owner-token verification inside every write transaction.
- Commit fingerprint and receipt idempotency.
- Revision calculation across all canonical sections.
- Structured operation failures.
- Integrity constraints, reader capabilities, and maintenance capabilities.

The store does not own:

- User-visible actions.
- Backoff schedules or retry budgets.
- TUI generations.
- Runtime dispositions such as `Retry`, `Reopen`, or `ReadOnly`.

### UI application

The application owns:

- Starting, transferring, and closing the actor at session boundaries.
- Rendering unpublished, intentionally ephemeral, clean, saving, read-only,
  blocked, ownership-lost, and unavailable states.
- Explicit retry after the user changes external conditions.
- User decisions when switch, fork, reset, or shutdown cannot make the target
  generation durable.
- Rejecting stale request-audit host calls by session epoch.

It does not own SQLite recovery, storage backoff, pending-save reconciliation,
or shutdown-specific retry loops.

## Target domain types

These types make the intended responsibility boundaries concrete; they are a
starting design, not a mandatory public API. If implementation exposes a smaller
representation with the same or stronger invariants, use it and update the plan.
Do not leave old and new protocols in parallel.

### Persistence generation and actor epoch

```rust
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct PersistenceGeneration(u64);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SessionEpoch(u64);
```

`PersistenceGeneration` replaces the current two-field `DocumentGeneration`. It
belongs beside `TuiSessionDocument`, because no non-TUI code consumes the
current persistence state machine. It advances once after an applied canonical
mutation. Transcript subsystems may retain local counters for their own no-op
detection, but those counters are not persistence identities.

`SessionEpoch` is a checked, process-local actor-instance sequence allocated by
the UI. It changes whenever a writable actor is replaced, even for the same
session ID. It is never persisted or used for commit idempotency.

### Session identity, metadata, and head

Replace `SessionState`; do not add another type beside it:

```rust
pub struct SessionIdentity {
    pub id: String,
    pub created_at: i64,
    pub parent_id: Option<String>,
}

pub struct SessionMetadata {
    // All mutable canonical metadata currently stored in SessionState,
    // including fast_mode, accounting, checkpoint, cost, and updated_at.
}

pub struct SessionCostUsd(f64);

pub struct StoreHead {
    pub revision: Revision,
    pub history_len: HistoryLen,
    pub descriptor_len: DescriptorLen,
}
```

Rules:

- Session ID, creation time, and fork parent are inserted once and validated
  unchanged on every later commit.
- `SessionMetadata` derives full `PartialEq`; no hand-maintained field list is
  allowed.
- Revision and canonical lengths never appear in metadata input.
- Keep current cost precision without inventing a fixed-point unit. Wrap it in
  `SessionCostUsd`, reject negative and non-finite values, normalize negative
  zero, and use the normalized IEEE-754 bits for fingerprint serialization. The
  wrapper and metadata implement `PartialEq`, not `Eq`.
- Existing `SessionMeta` summary projections may remain only where their reduced
  read model is useful. Do not create duplicate full metadata structures in
  core and store.

### Document change state

The replacement for `SessionPersistState` is intentionally small:

```rust
struct DocumentChanges {
    current: PersistenceGeneration,
    durable: PersistenceGeneration,
    acknowledged_head: StoreHead,
    history_dirty_from: Option<usize>,
}
```

Generation inequality alone represents a metadata-only change, so it needs no
second dirty flag. A mutation to a history-indexed side table lowers
`history_dirty_from` to that row's history coordinate, using one conservative
boundary for history and its side tables. Transcript descriptor dirtiness
remains in the transcript model, which already owns `descriptor_dirty_from`.
Remove duplicate `descriptors_persisted` state. `acknowledged_head` is a read
projection for bounded suffix construction, not authority to choose a commit
base.

### Save intent

```rust
struct SessionSaveIntent {
    generation: PersistenceGeneration,
    identity: SessionIdentity,
    metadata: SessionMetadata,
    history: HistorySuffix,
    side_tables: SideTableSuffixes,
    descriptors: Option<TranscriptDescriptorSuffix>,
}
```

The intent is already converted to store protocol rows by the document builder.
The actor only adds its authoritative expected `StoreHead`.

There are no metadata-only, live, full, or request-append intent variants. A
metadata-only update is represented by an empty history suffix at the final
history length and no descriptor suffix. A live or request append is an ordinary
cumulative suffix from the conservative dirty boundary.

The latest intent must contain all work not acknowledged by the document. Dirty
ranges are not cleared on submission, making replacement of an older queued
intent safe.

### Actor handle and control surface

Canonical desired state uses a latest-value slot plus a lightweight wake:

```rust
struct SessionPersistence {
    latest: Arc<Mutex<LatestIntentState>>,
    control: SyncSender<PersistenceControl>,
    status: Arc<Mutex<SessionPersistenceStatus>>,
    status_wake: Receiver<()>,
    thread: JoinHandle<()>,
}

struct LatestIntentState {
    accepting: bool,
    intent: Option<Arc<SessionSaveIntent>>,
}

enum PersistenceControl {
    WakeDesired,
    AppendRequestAudit(RequestAuditIntent),
    RetryBlocked,
    Flush {
        target: PersistenceGeneration,
        deadline: Instant,
        reply: Sender<PersistenceFlushOutcome>,
    },
    Close {
        target: PersistenceGeneration,
        deadline: Instant,
        policy: ClosePolicy,
        reply: Sender<PersistenceCloseOutcome>,
    },
}

enum ClosePolicy {
    RequireDurable,
    AllowUnsaved,
}
```

This is illustrative, not a requirement to expose the mutex. `submit(intent)`
atomically replaces an older generation and treats a full wake channel as
success because the actor checks the slot after every queued control message. It
returns failure only when the actor is disconnected or stopped. Drop replaced
large intents after releasing the slot lock.

The slot state also carries an `accepting` flag. Submission and the start of
close update it under the same lock, so close cannot capture a target and then
accept a newer unreported intent.

Audits remain best effort and are bounded by both message count and total
accepted payload bytes. Oversized/full-payload admission uses the existing
summary compaction and preserves the skipped-byte count; queue saturation
returns an audit warning. Flush and close are explicit blocking operations
already expected to wait for durability; enqueueing either waits only until its
own deadline and returns a typed failure on timeout.

### Actor status handoff

Do not let a bounded event queue deadlock the actor while the TUI synchronously
waits for flush, and do not make status memory unbounded. Publish one latest
status snapshot plus a coalesced wake:

```rust
struct SessionPersistenceStatus {
    epoch: SessionEpoch,
    state: PersistenceState,
    latest_audit_warning: Option<PersistenceCause>,
    latest_sidecar_warning: Option<PersistenceCause>,
}

enum PersistenceState {
    Idle {
        durable: PersistenceGeneration,
        head: StoreHead,
    },
    Saving {
        generation: PersistenceGeneration,
    },
    Durable {
        generation: PersistenceGeneration,
        receipt: SaveReceipt,
    },
    Blocked {
        desired: PersistenceGeneration,
        durable: PersistenceGeneration,
        cause: PersistenceCause,
    },
    OwnershipLost {
        desired: PersistenceGeneration,
        durable: PersistenceGeneration,
        cause: PersistenceCause,
    },
    Stopped {
        outcome: PersistenceStopOutcome,
    },
}
```

The actor replaces status under a short lock, then `try_send`s a unit wake. A
full wake is success because the consumer will read the latest status; a
disconnected wake receiver never changes durability or ownership. Durable status
retains the newest receipt until another state replaces it, and flush/close also
return receipts directly. The UI atomically takes audit/sidecar warnings while
reading status; replacing an unread warning increments an aggregate dropped
warning metric. Warning fields cannot overwrite canonical state.

There is no `Retrying { next_attempt }` state because there is no timed retry
scheduler. Each failure carries one structured cause, not a cause plus an
independent disposition.

### Store commit and receipt

```rust
pub struct SessionCommit {
    pub expected: StoreHead,
    pub identity: SessionIdentity,
    pub metadata: SessionMetadata,
    pub history: HistorySuffix,
    pub side_tables: SideTableSuffixes,
    pub descriptors: Option<TranscriptDescriptorSuffix>,
}

pub struct SaveReceipt {
    pub previous: StoreHead,
    pub current: StoreHead,
}
```

Remove `SaveId` from commit and receipt. The hash of the exact serialized commit
is its idempotency identity. The actor retains the exact commit while resolving
an ambiguous result, and the store persists only the latest commit fingerprint
and receipt because actor writes are serial.

## Save lifecycle

### 1. Apply a canonical mutation

Every canonical mutation entry point uses one outer post-mutation hook:

1. Compute `next = current.checked_add(1)` before changing data; reject the
   mutation if no next generation exists.
2. Capture conservative history/descriptor boundaries before the mutation can
   invalidate old coordinates.
3. Apply the domain mutation once.
4. If it was a semantic no-op, leave generation and dirty state unchanged.
5. Otherwise install the expanded dirty boundaries, set `current = next`, leave
   `durable` and `acknowledged_head` unchanged, and mark the document unsaved.

A mutation that changes metadata, history, side tables, and descriptors together
still advances exactly once. Never wrap, saturate, or reuse a generation, and do
not allow direct canonical field mutation outside this hook.

### 2. Build one cumulative intent

One builder handles both materialized and store-backed sessions. It:

1. Sets history replacement start to the minimum of the local dirty/indexed-side-
   table boundary, final history length, and acknowledged durable history length.
   This clamps stale or missing local markers to a store-safe prefix.
2. Reads the replacement suffix from `LiveSession::history_range`, allowing the
   range to span SQLite-backed and in-memory history.
3. Builds replacement/truncation suffixes for every history-indexed side table
   from the same conservative boundary through its final sparse rows.
4. Clamps the transcript model's descriptor dirty boundary to the acknowledged
   descriptor extent. Installing synthesized descriptors not proven present in
   SQLite marks them dirty from zero and advances the document generation once;
   ordinary materialization of already stored descriptors does neither.
5. Copies immutable identity and current mutable metadata.
6. Tags the result with `current`.

The history suffix carries a replacement start and final length, so truncation
is represented without a special command. The builder must not assume an
append-only session.

This design is deliberately cumulative from the last document acknowledgement.
If generation G1 is in flight and a rewind creates G2, the G2 intent includes the
rewritten or truncated range even if G1 later commits first.

For a store-backed session, the document retains enough acknowledged durable
coordinates to read the SQLite prefix and enough in-memory suffix to represent
all unacknowledged work. No actor-owned snapshot is copied back into the model.

### 3. Publish desired state

`submit(intent)` atomically replaces an older queued intent if the generation is
newer, then attempts a nonblocking wake. Submission rules are:

- A wake already queued is success.
- A stopped or disconnected actor is failure.
- A generation older than the slot or actor's durable generation is rejected as
  a programming error.
- Equal generations are accepted only when the canonical intent fingerprint is
  identical; otherwise they expose a missing generation advance. If that
  generation is already durable, the actor republishes its cached receipt without
  another transaction.

Submission never clears dirty state. If submission fails, local state remains
visibly unsaved and the UI reports that persistence is unavailable.

### 4. Converge in the actor

After a wake or successful canonical commit, the actor repeatedly takes the
newest desired intent until no newer intent remains. For each intent it:

1. Uses its authoritative `StoreHead` as the expected base.
2. Builds and retains one exact `SessionCommit` and fingerprint.
3. Executes the single canonical transaction.
4. Performs the one structural recovery sequence if the outcome is ambiguous.
5. Updates its head only from a validated receipt.
6. Publishes `Durable` status and resolves eligible flushes immediately.
7. Rebuilds derived sidecars from SQLite, updating the independent warning field
   without changing durable status.
8. Rechecks the latest slot before sleeping.

An in-flight commit is immutable. A newer intent can replace the queued slot but
cannot mutate the command being executed. If G1 commits after G2 has replaced
the slot, the actor applies G2 against the G1 head. Because G2 was built from a
conservative dirty boundary, overlap is expected and safe.

The actor may skip a queued generation that never began. Generations order
states; they do not promise one database revision per local edit.

### 5. Acknowledge only a current generation

The document applies a `Durable` status or successful flush receipt only when
its actor epoch matches and its generation equals the document's current
generation. This single rule removes the need to retain submitted snapshots in a
pending-save state machine.

On a matching durable observation, the document:

1. Validates the receipt's final history and descriptor lengths against the
   current intent shape.
2. Sets `durable = current`.
3. Replaces `acknowledged_head` with the receipt head.
4. Clears all covered dirty state.
5. Compacts `LiveSession` through the acknowledged history length.
6. Installs the receipt's descriptor extent as the transcript's acknowledged
   total and clears only the matching dirty range. The extent is not inferred
   from the number of sparse descriptor rows.

An older durable observation is valid actor progress but is not applied to the
live document. In particular, never compact an old saved prefix after a newer
edit has rewritten that prefix. The actor can still apply the newer cumulative
intent and eventually emit a current-generation receipt.

A future generation, mismatched actor epoch, decreasing head, or structurally
impossible receipt is an invariant failure and must not clear dirty state.

### 6. Handle actor loss conservatively

If the actor thread or control channel disconnects, a commit may or may not have
completed. Keep all unacknowledged dirty state. A missed/coalesced status wake is
not a failure because the UI reads the shared latest status on every persistence
poll and before lifecycle decisions. A replacement actor opens under a new
epoch, reads the canonical head, and receives a newly built cumulative intent.
Overlap or an exact no-op resolves uncertainty without inventing an
acknowledgement.

### 7. Keep publication lazy

A never-published session with no canonical history or transcript descriptors
needs no actor, lease, directory, or database. Metadata edits may accumulate in
that in-memory draft, but do not create an otherwise empty session. Start its
actor when the first content-bearing history or descriptor intent is submitted;
that intent includes all accumulated metadata. Once published, truncating
content back to zero is an ordinary durable canonical update and does not delete
the session. The unpublished draft is an explicit intentionally nondurable
state, not a blocked save.

Ephemeral sessions never start a persistence actor and are intentionally
nondurable, not dirty-with-an-error. An existing unsupported or initially
read-only session may be viewed but cannot start a writable actor; edits remain
unsaved until the user forks or saves to a writable destination.

### 8. Treat sidecars as post-commit caches

After canonical commit succeeds:

- Publish `Durable` and advance document durability before sidecar work.
- If sidecar refresh or reading fails, keep durable status and surface an
  independent warning, never `Blocked`.
- Attempt repair after a later successful save or explicit maintenance action.

Do not replay canonical state to repair a cache.

## Ordering and control semantics

There is no FIFO of large save payloads. Ordering comes from one monotonic
generation, one in-flight commit, and one latest-value slot.

Rules:

1. The actor serializes all SQLite writes for its session.
2. Replacing a queued intent is allowed; replacing an in-flight command is not.
3. After every commit and control message, the actor checks for newer desired
   state before waiting.
4. A bounded control queue carries small lifecycle messages and byte-budgeted
   audits. Saturation can compact or drop a best-effort audit with a warning.
   Flush, close, and explicit retry return a typed failure rather than being
   silently lost.
5. The actor handle refuses new commands while a close attempt owns the slot;
   `RequireDurable` failure restores acceptance before returning.
6. Canonical submission failure never marks the document saved.
7. Failure to deliver a status wake never rolls back a commit or releases
   ownership; a still-live consumer reads the shared status directly, otherwise
   the document retains conservative unacknowledged state.
8. If all control senders disappear without close, the actor finishes an
   in-flight command and consumes the already queued latest intent only if it is
   not blocked, then performs normal lease cleanup and exits. It does not start
   a blocked retry or wait forever.

Do not build a generic actor framework. A private state struct, one thread, one
latest-intent slot, one latest-status snapshot, and bounded wake/control channels
are sufficient.

### Request-audit ordering

Each audit intent contains the fixed session epoch and the persistence
generation after which it is valid. Host dispatch rejects an audit if the active
document's session ID or epoch changed before submission. The actor checks the
same values again.

The actor writes an audit only after its canonical durable generation reaches
the audit's required generation. This preserves the existing request-history
boundary without a specialized request-append save protocol. If canonical
persistence blocks, the bounded audit remains best effort and may be rejected
with a latest audit warning; it never changes the canonical save outcome.

Request-audit transactions remain separate from canonical commit. Close first
resolves its canonical target, then drains already accepted eligible audits
until the same deadline. Audit failure does not turn a successful canonical
flush into failure.

An audit transaction error is never blindly replayed because its commit may be
ambiguous and audits have no separate idempotency protocol. Publish the latest
audit warning. If the error invalidates the connection, retain the lease, reopen
once, and verify the owner token for future canonical work. Reopen failure moves
the actor
to blocked; an already clean document stays clean, but future saves are
unavailable until explicit retry. The failure never discards the lease or makes
the lock acquirable.

### Generation-targeted flush and close

`flush` captures the document's current generation after successfully building
and submitting its latest intent. It succeeds when the actor's durable
generation is at least that target, even if an intermediate generation was
coalesced. Success returns the durable generation, epoch, and receipt; the caller
routes it through the same acknowledgement validator as durable status. It fails
on local intent construction/submission, deadline, ownership loss, actor exit, or
when a blocked state prevents the target.

The actor begins `close` by taking the slot lock, refusing new submissions, and
including any intent accepted before that lock in the effective target. An
expired or unadmitted close command never flips acceptance. Once a close command
is admitted, its caller waits for a definitive reply so a caller-side timeout
cannot create a ghost close after the UI moves on.

`RequireDurable` joins only after satisfying the effective target; on block or
deadline it leaves the actor alive and restores acceptance so the UI can retry,
fork, or cancel the lifecycle action. `AllowUnsaved` is used only after an
explicit user/lifecycle decision, returns the omitted target and durable
generations, releases the lease, and joins. For an unpublished staged session it
removes the unacknowledged stage under the lease; cleanup failure leaves the
root-lease-guarded orphan for startup maintenance. Eligible audits drain only
within the same deadline and never control canonical success. There is no
shutdown-only save loop.

## Structural recovery, not a retry subsystem

Delete all three current durability retry policies:

- The store's five-second `BEGIN` loop.
- Worker-level repeated save attempts and sleeps.
- Shutdown retry counters and delays.

Set one `SESSION_BUSY_TIMEOUT` constant to 100 ms, used only to absorb a
momentary SQLite/WAL lock handoff inside the library. Do not add exponential
delays, retry budgets, timers, attempt counters, or a `Retrying` state. With one
exclusive session writer, `BUSY` beyond that primitive wait means an invariant
or external interference, not ordinary load. Fault tests may justify changing
this one constant, never adding another loop.

### One ambiguous-outcome recovery sequence

For an I/O or SQLite failure that may invalidate the connection or leave commit
outcome unknown, retain the stable lease and exact command, then:

1. Close only the SQLite connection.
2. Reopen once while still holding the lease.
3. Verify the database owner token still matches the lease token.
4. Query the persisted commit fingerprint and receipt.
5. If the exact fingerprint is present, validate the receipt and report success.
6. If the fingerprint is absent and the store head still equals the command's
   expected head, execute that exact command one more time.
7. For every other result, or if the repeated command fails, retain the latest
   intent and become visibly blocked.

Do not rebuild the uncertain command against a different head. Do not infer
success from revision or length alone. An ambiguous second attempt is resolved
only by an explicit retry, which starts by checking its retained fingerprint.

### Explicit retry

`RetryBlocked` performs one immediate recovery pass after the user or lifecycle
code has reason to believe the environment changed. It has no sleep loop. It:

1. Reopens the connection if needed without releasing the lease.
2. Verifies ownership and reconciles any retained uncertain fingerprint.
3. Reads and validates the current head.
4. Processes the newest cumulative intent once.
5. Returns to blocked on another environmental failure.

A new save intent updates desired state but does not by itself create a retry
loop. The UI offers explicit retry, fork/save-as, or continue without durability
as appropriate.

### Failure classification

Use one typed failure cause and classify at the actor boundary:

- **Invalid command or invariant:** malformed suffix, decreasing coordinates,
  non-finite metadata, persistent `BUSY`, unexpected stale head, revision or
  generation overflow. Block and expose the defect.
- **Environment blocked:** permissions, missing parent, disk full, transient
  device or connection failure after structural recovery. Keep the lease and
  latest intent where possible; allow explicit retry.
- **Ownership conflict or loss:** root lease contention, token mismatch, or
  evidence of another writer. Stop canonical writes. Dirty local state remains
  available for fork/save-as.
- **Unsupported or corrupt store:** unsupported schema or integrity failure.
  Keep the session readable where safe, prohibit writes, and preserve local
  changes separately.

Remove `SessionPersistenceDisposition`, broad error wrappers carrying policy,
and duplicated string plus structured failure fields. Errors describe facts;
the actor chooses the response from concrete variants.

## Stable ownership lease and publication

The current `session.lock` sits inside the directory that staging, deletion, and
publication rename. It cannot provide continuous ownership. Replace it with one
stable lock namespace:

```text
<sessions-root>/.locks/<validated-session-id>.lock
```

Root lock files are small, permission-restricted, and never deleted. Their inode
and path therefore remain stable while a session directory is absent, staged,
renamed, quarantined, or deleted.

For a database whose schema predates the root lease, migration acquires locks in
one order: root lock, then existing in-directory `session.lock`. It holds both
through owner-token claim and the schema version bump. Contention on the legacy
lock releases the new root lock and reports ownership conflict. After the bump,
new code retains only the root lease and old binaries reject the newer schema
before claiming ownership. This is a migration guard, not a second normal writer
path. Mark it `COMPAT(storage-root-lease)` with removal tied exactly to dropping
all pre-root-lock schema versions; if those versions are dropped during this
implementation, remove the guard and compatibility entry in the same phase.

### Concrete capability

Keep ownership concrete in `crates/store/src/access.rs`:

```rust
struct SessionLease {
    root: PathBuf,
    session_id: String,
    token: OwnerToken,
    lock: File,
}

pub struct OwnedSessionWriter {
    lease: SessionLease,
    location: SessionLocation,
    db: Option<SessionDb>,
}
```

`SessionLocation` is a concrete staged or published path, not a general storage
backend. All lock and session paths come from one layout helper after strict
session-ID validation. Callers must not join arbitrary IDs into `.locks`.

The lease is acquired before inspecting or changing a destination and spans:

- Initial create and migration.
- All canonical and audit writes.
- SQLite connection invalidation and reopen.
- Staged publication and its reconciliation.
- Maintenance, repair, and import.
- A fork destination.
- Rename-to-trash and deletion.

`invalidate_connection` drops only `SessionDb`. `reopen_connection` uses the
same lease token and rejects a missing or mismatched database token as ownership
loss. Initial open under a newly acquired OS lease may replace stale database
owner metadata left by a crashed process; reconnect within one lease may not.

Normal close attempts owner-token cleanup and SQLite hygiene before releasing
the OS lock. Cleanup failure is reported but cannot leave an in-process lock
held forever. The next owner can distinguish stale metadata by first acquiring
the root lock.

### First publication protocol

A new session is not durable merely because its staging database committed. Its
actor performs this sequence under one root lease:

1. Confirm the published destination does not exist.
2. Create a uniquely named staging directory under the same filesystem.
3. Create the database, install the lease token, and apply the canonical commit.
4. Perform required SQLite close/checkpoint hygiene while retaining the lease.
5. Atomically rename staging to the final session directory.
6. Fsync the sessions root so the directory entry is durable.
7. Reopen the published database and verify session identity, owner token, head,
   fingerprint, and receipt.
8. Only then publish `Durable`.

If rename, root fsync, or reopen reports failure, keep the lease and reconcile
paths before retrying any transaction:

- Final path with the expected database and token means rename happened; verify
  it, successfully fsync the sessions root, then finish.
- Staging path only means publication did not happen; an explicit retry may
  continue it.
- Both paths, neither path after an ambiguous rename, an unexpected destination,
  or mismatched identity/token is a visible conflict. Never overwrite, merge, or
  delete either path automatically.

This publication state belongs to the actor's blocked state. Recreating a fresh
staging database could discard a transaction that already committed.

A process crash before rename leaves an unacknowledged staging directory, not a
durable session. Staging names encode the validated session ID so later cleanup
can acquire its root lease. New-session open checks matching stages directly;
a bounded maintenance batch handles other orphans. It removes a structurally
valid orphan only after proving the final path is absent and no actor owns the
lease; malformed or identity-mismatched staging is quarantined for doctor
output. It never silently publishes an actor intent whose document lifecycle was
lost. A crash after rename is reconciled from the final database on normal open.

### Delete, maintenance, and fork

Deletion first acquires the stable lease, validates identity, closes SQLite,
renames the directory to a unique trash path, and fsyncs the sessions root. That
rename plus fsync is the logical deletion. Trash-tree removal failure is reported
as a cleanup warning and leaves the uniquely identified tree for explicit
maintenance; it never restores or overwrites the original path. An ambiguous
rename/fsync is reconciled under the lease before either path is touched. The
stable lock file remains. An active actor therefore prevents delete even while
its SQLite
connection is closed.

Maintenance and schema migration acquire the same lease. They do not bypass the
actor for an active session.

Forking first freezes source-document mutation. For a writable source it flushes
the captured generation, then reads one SQLite snapshot through `SessionReader`.
For a read-only, ownership-lost, or deliberately unsaved source, a concrete fork
builder overlays
the document's cumulative local metadata/history/side-table/descriptor state on
that reader snapshot so no local edit is lost. It may use
`LiveSession::history_range(0..len)` for the SQLite prefix plus live suffix; it
does not need a source writer lease.

The builder assigns fresh destination identity with the source as fork parent.
After the source snapshot/overlay is fixed, fork acquires the destination's
stable lease and produces its initial canonical commit. The destination follows
the same
staged publication protocol and never copies source lock, owner token, commit
receipt, or fingerprint.

Add subprocess tests that contend for the root lock during create, staged
rename, connection reopen, migration, fork destination creation, and delete.

## Error model

Keep only errors with distinct factual domains:

- `StoreError` for open, read, schema, migration, object, and maintenance facts.
- `SessionCommitFailure` for command validation, stale expected head, ownership
  token loss, transaction outcome, and integrity facts.
- `PersistenceCause` as the actor's small presentation-safe classification, with
  the original store/commit error retained as its source where useful.

Neither store error contains UI actions, retry budgets, or dispositions. Delete
`PersistFailure` if it becomes a forwarding wrapper, and delete
`SessionPersistenceDisposition` unconditionally. Keep root-lock
`OwnershipConflict` distinct from token-based `OwnershipLost`.

Preserve SQLite result codes and I/O kinds until the actor decides whether the
connection outcome is ambiguous. UI formatting happens once. Multi-step errors,
especially migration plus pragma restoration and close plus owner cleanup,
retain the primary error and every cleanup error instead of overwriting one
another.

Do not add a generic error trait or repository abstraction.

## One canonical transaction path

Replace the split snapshot and commit implementations with one concrete
`apply_session_commit` transaction used by the actor, imports, synthesis, and
test fixture builders.

Transaction order:

1. Serialize and validate every suffix, object, coordinate, and finite metadata
   value before opening the transaction where possible.
2. Compute the exact command fingerprint from a version-tagged canonical
   serialization with fixed field order, sorted JSON object keys, normalized
   cost bits, and explicit integer encodings. Do not fingerprint incidental
   `serde` map/debug output.
3. Begin one `IMMEDIATE` transaction and verify the owner token inside it.
4. If the stored fingerprint is exact, validate and return its stored receipt.
5. Load identity, mutable metadata, and `StoreHead`; require the command's
   expected head to match.
6. Insert identity for a new database, or require session ID, creation time, and
   fork parent to be unchanged.
7. Compare and apply metadata, history, side tables, and transcript descriptors
   using section helpers that return whether canonical content changed.
8. Set final history and descriptor extents from the applied rows.
9. If any canonical section changed, increment revision exactly once with checked
   arithmetic. Otherwise leave revision unchanged.
10. Persist the command fingerprint and full receipt in the same transaction.
11. Commit and return the receipt.

A helper may compare canonical row hashes or exact decoded rows, whichever is
simplest for that section, but it may not infer change from a manually selected
metadata field list. Replacing equal history, side-table, or descriptor rows is a
no-op. Truncation, descriptor-only replacement, and metadata-only mutation each
count as canonical change. Audit rows, owner metadata, commit receipts, and
sidecars do not.

`updated_at` changes when the document changes, not merely because the user
pressed save. Consequently, resubmitting identical desired state can remain a
true no-op. Normalize any retained floating-point representation, including
negative zero, before equality and fingerprinting. A schema migration that
changes fingerprint format clears the old latest fingerprint/receipt atomically
under the lease; no in-memory uncertain command survives across process restart.
Runtime never carries two fingerprint formats.

The transaction validates final cross-table invariants before commit:

- History suffix item count exactly reaches final history length.
- Side-table rows refer to history that exists in the resulting state.
- Descriptor extent and records agree with the resulting history links.
- Every object reference resolves and has the expected reference-level role.
- Final coordinates fit both Rust and SQLite representations.

Delete `save_session_snapshot_in_transaction`, the `save_session_snapshot*`
entry points, and their independent revision calculation once all callers use
this path. No maintenance entry point may write canonical tables by bypassing
these invariants.

## Content-addressed objects and reference semantics

An object is bytes, not a semantic value. Keep raw SHA-256 identity and only
intrinsic storage metadata:

```text
objects(
    hash PRIMARY KEY,
    codec CHECK (codec IN ('none', 'zstd')),
    raw_size,
    stored_size,
    bytes
)
```

Delete the `kind` and `created_at` columns, `objects_kind_idx`,
`ObjectMeta.kind`, and the `kind` argument to object insertion. Current code has
no retention policy that uses object creation time.

Semantics live at every reference:

- `history_object_refs.role` uses a checked Rust enum and matching SQL constraint
  for every supported attachment/history role.
- `request_object_refs.role` uses a checked Rust enum and matching SQL
  constraint: `body_json`, `body_manifest`, `body_top`, `body_item`,
  `body_parent`, `response`, or `error`.
- Partial unique indexes permit at most one primary body role (`body_json` or
  `body_manifest`) and at most one response and error role per request attempt.
- Remove `body_hash`, `response_hash`, and `error_hash` from `request_attempts`.
  Typed reference rows are the only object-link source; summaries obtain hashes
  from a concrete join/read helper.
- A decoded manifest requires `body_parent` for parent manifests and
  `body_top`/`body_item` for JSON components. Readers validate those edge roles
  rather than consulting object metadata.

The same hash may therefore be a JSON body in one request, a manifest in another
request, and an attachment in history without first-writer aliasing. On hash
conflict, object insertion validates the existing codec and stored/raw sizes
against the incoming hash and raw length before accepting the reference. Full
payload hash scans remain a doctor/read-integrity concern. First physical
compression is allowed; first semantic role is not.

### Clean schema migration

Build the final tables in one migration and copy validated rows. Backfill each
existing body reference from old behavior before dropping `objects.kind`:
`request_body_manifest` becomes `body_manifest`; every other old kind becomes
`body_json`. Strictly decode and validate all rows marked as manifests, including
referenced hashes. If old metadata and bytes disagree, fail migration with a
specific integrity error rather than guessing.

Rebuild all request reference rows, validate their singleton constraints, then
drop the direct request-attempt hash columns. Validate all history roles. Finally
drop old object/reference tables and indexes in the same migration transaction.
Runtime code reads only the new schema. Do not retain a dual read path or
permanent compatibility flag.

If pragma restoration or migration cleanup fails while reporting a primary
migration error, preserve both errors in one structured result. A cleanup error
must not mask the operation that caused it, or be silently replaced by a later
cleanup failure.

### Bounded manifest traversal

Replace both recursive manifest expansion and recursive reference installation
with one iterative walker shared by write validation, reads, and migration.

The walker:

1. Starts with an explicit `body_manifest` reference.
2. Validates each SHA-256 hash before lookup.
3. Maintains a visited set and rejects a repeated parent as a cycle.
4. Enforces one manifest-chain depth limit on reads and writes.
5. Enforces total manifest count, item count, decoded bytes, and reconstructed
   payload byte limits so a wide chain cannot exhaust memory.
6. Validates version, checkpoint rules, stable `input_key` and `top_hash`, and
   parent/item roles at every edge.
7. Collects the chain iteratively, then applies checkpoints and deltas oldest to
   newest.
8. Fails on missing objects or references instead of returning a partial body.

Keep all limits beside `MAX_OBJECT_RAW_SIZE` and cover exact-boundary and
one-over-boundary behavior. The writer's chain checkpoint policy may remain an
optimization, but reader correctness never relies on it.

## Remove the snapshot write protocol

`SessionCommit` and `apply_session_commit` are the only canonical write protocol
and implementation.

Migrate every `SessionSnapshot` caller:

- Import and synthesis build a full history suffix from index zero and apply it
  under `SessionMaintenance` with the correct expected head.
- Repair builds an explicit replacement commit after validating the existing
  identity and head.
- Fork reads one coherent source snapshot and builds the destination's initial
  commit.
- Tests and fixtures use a small commit builder, then exercise the production
  transaction.
- Offline synchronous saves open an `OwnedSessionWriter`, build the same commit,
  apply it, and release it. They do not retain another revision algorithm.

Move `load_session_snapshot`, search-blob generation, and shared row/suffix
helpers to the concrete reader/history/commit modules that own those behaviors.
Then delete `crates/store/src/session_snapshot.rs`, the `SessionSnapshot` public
export, snapshot apply methods in `SessionDb` and `SessionMaintenance`, and core
helpers whose only purpose was constructing that protocol. A complete reader
result may use an ordinary local struct, but must not be writable through a
second path.

Do not unify these callers with a repository trait. They differ only in how they
obtain concrete data and ownership; the transaction is already shared.

## Derived files

Sidecars remain noncanonical caches. Save intents never contain rendered
sidecars or sidecar input copies. After a successful canonical transaction, the
actor calls the existing concrete rebuild helper, which opens SQLite as a reader
and derives files from committed state.

For a new session, rebuild only after staged publication has been reconciled and
the final path is readable. Sidecar replacement is atomic per file. A missing,
stale, malformed, or unreadable sidecar never affects canonical reads.

- Canonical `Durable` status and flush success are published before rebuild.
- Rebuild failure updates an independent latest warning; it does not change
  revision, generation, durable status, or flush success.
- It never replays the canonical commit.
- A later successful save or explicit maintenance action repairs it.
- Resume, fork, import, doctor, and repair do not depend on sidecar content.

## Read path

Keep `SessionReader` and store-backed bounded resume.

Reads do not need to pass through the actor. When a caller requires read-your-
writes consistency, it first flushes the required generation and then opens or
uses a reader. WAL permits readers while the actor retains writable ownership.

Do not add asynchronous reader traits or route all reads through the write actor.

## Proposed code organization

Prefer deletion and concrete ownership over adapters. The final tree has one
protocol and no dormant old implementation.

### Delete `crates/core/src/session_save.rs`

Its persistence state has only TUI consumers. Move the smaller
`PersistenceGeneration`, `DocumentChanges`, intent builder, and acknowledgement
logic beside `TuiSessionDocument`. Delete `PendingSessionSave`,
`PreparedSessionSave`, `PersistDelta`, `SessionPersistState`, paired generations,
save IDs, and all acknowledgement-by-pending-snapshot code.

### `crates/tui/src/app/session_document.rs`

Own document generation, conservative dirty boundaries, acknowledged head
projection, one materialized/store-backed intent builder, current-generation
receipt validation, and generation-safe live-prefix compaction. Remove separate
full/live/metadata/request-append plans and `DescriptorAppendSubmission`.

### `crates/tui/src/persist.rs`

Replace the global cross-session worker with one `SessionPersistence` handle and
one-session actor state. Implement the latest-intent slot, wake coalescing,
bounded control lane, fixed session epoch, stable lease, canonical convergence,
structural recovery, audits, flush, close, and latest typed status. Delete backend
`Closed/ReadOnly/Owned` cross-session states, queue payload variants, retry
scheduler/state, policy wrappers, and ignored send results.

Keep this in one file after deleting old code. Split only if the resulting file
still has more than one cohesive responsibility; do not create a reusable actor
framework.

### TUI orchestration files

`crates/tui/src/app/history.rs` keeps save triggers, status handling, explicit
retry UI, and actor close/start at session boundaries. It loses duplicate full
and live submission flows, pending-save suppression, specialized request-history
append saves, retry counters, and persistence sleeps. The request boundary keeps
an ordinary cumulative submit plus generation-targeted flush.

`crates/tui/src/app/host_dispatch.rs` captures and validates the fixed session
epoch for request audits. `crates/tui/src/app/transcript.rs` stops maintaining a
second submitted descriptor extent; only current model dirtiness and matching
durable acknowledgement remain. `transcript_search.rs` derives SQLite coverage
from the acknowledged descriptor extent and current dirty boundary instead of a
separate `descriptors_persisted` boolean.

### Core session files

`crates/core/src/session_runtime.rs` keeps the bounded SQLite-prefix plus live-
suffix representation, `history_range`, truncation, and acknowledged-prefix
compaction. It does not own transaction coordinates.

`crates/core/src/session.rs` keeps readers, conversion helpers, sidecar rebuild,
and explicit offline operations. Migrate `save`, `save_result`, staged save, and
snapshot builders to the canonical updater, then delete helpers that duplicate
runtime persistence.

### Store files

- `access.rs`: stable root lease, staged/published location, connection reopen,
  and concrete reader/writer/maintenance capabilities.
- `session_commit.rs`: typed coordinates, canonical commit input, receipt, and
  factual failures. Delete `SaveId` and `SessionPersistenceDisposition`.
- `meta.rs`: immutable `SessionIdentity`, mutable `SessionMetadata`, read
  summaries, and `StoreHead`; no revision or lengths in desired metadata.
- `db.rs`: one `apply_session_commit` facade, fingerprint reconciliation, and
  reader methods. Delete transaction retry state and snapshot application.
- `session_snapshot.rs`: delete after callers migrate.
- `object.rs`: intrinsic content-addressed storage only.
- `request_audit.rs`: explicit reference roles and bounded iterative manifests.
- `schema.rs`: one clean migration to final metadata, ownership, object, and
  request-reference schema.
- `lib.rs`: export only the concrete capabilities and final protocol.

Private commit helpers may move out of `db.rs` if that makes the transaction
easier to review. Do not introduce storage traits or split files merely to match
architectural nouns.

## Implementation phases

The phase order is dependency-aware guidance, not ceremony. Combine, split, or
reorder phases when doing so produces a cleaner cutover with fewer temporary
paths. Reassess the architecture after each phase against the principles above
and change direction when code evidence supports a better design.

Each completed phase still leaves one production implementation for the behavior
it touches. Do not merge a phase with dormant compatibility modules, dual schema
readers, or an adapter whose only purpose is keeping the replaced protocol alive.

### Phase 0: Reproduce behavior at real boundaries

Add end-to-end tests through the same document, actor/worker, filesystem, and
SQLite boundaries a user exercises. Use deterministic failpoints only at the
boundary being tested.

Required regressions:

- Canonical enqueue failure leaves the document visibly unsaved.
- A `fast_mode`-only mutation changes canonical revision.
- Runtime CWD fallback does not mutate canonical CWD; an explicit CWD change does.
- A descriptor-only mutation changes revision exactly once.
- An exact no-op changes no revision.
- Rewind or truncation while an older save is in flight preserves the newer
  state and never compacts it away.
- Worker exit or channel disconnect preserves dirty state.
- Disk full, unwritable root, unsupported schema, and ownership conflict are
  distinct visible outcomes.
- A stale host audit after session switch is rejected.
- A new empty session creates no directory.
- Rename-success plus fsync/reopen-failure is reconcilable without data loss.
- Identical object bytes can occupy different semantic roles.
- Cyclic, deep, wide, and oversized manifest inputs fail within fixed bounds.

Record large-session resume memory, cumulative intent size, canonical commit
latency, and current queue memory as a baseline. Existing success, crash,
restart, fork, delete, migration, and ownership tests must stay green.

#### Phase 0 baseline

Recorded on 2026-07-18 with:

```bash
cargo xtask bench-transcript-layout --runs 3 \
  --workloads tiny_blocks_1mib --skip-nav --resume \
  --resume-bytes 10485760 --save-request \
  --save-request-history 10000 --no-warmup
```

The 10,000-row hot path produced these wall-clock means:

| Operation | Mean | Standard deviation |
| --- | ---: | ---: |
| No-op save | 0.019 ms | 0.005 ms |
| Request append | 6.400 ms | 0.419 ms |
| History append | 3.600 ms | 0.118 ms |
| Turn complete | 0.142 ms | 0.007 ms |
| Rewind/delete suffix | 7.307 ms | 0.417 ms |
| Provider history read | 0.701 ms | 0.024 ms |

Request-append and history-append intents serialized to 1,415 and 1,037 bytes.
The observed queue depth peaked at 2 and queued serialized payload bytes peaked
at 1,415. Their SQLite transaction commits took 69-82 us and 78-88 us across
the three samples.

The descriptor-backed 10 MiB resume fixture contained 2,560 descriptors and
108,877 estimated rows. Tail load took 7.548 ms, tail render took 2.799 ms, and
the resume process peaked at 32,760 KiB RSS. The combined layout/hot-path
process peaked at 97,788 KiB RSS.

### Phase 1: Install one canonical store model and updater

- Replace `SessionState` input with immutable identity, mutable metadata, and
  store-derived head projections.
- Validate and normalize floating-point metadata centrally; use structural
  `PartialEq` and canonical fingerprint serialization.
- Implement one `apply_session_commit` transaction with exact no-op detection,
  descriptor-aware revision calculation, checked coordinates, immutable
  identity validation, fingerprint, and receipt.
- Route runtime commit through it and remove the five-second transaction retry.
- Migrate import, repair, synthesis, synchronous save, fork destination, tests,
  and fixtures to the same updater.
- Delete `session_snapshot.rs`, its exports, and all duplicate snapshot/revision
  helpers.
- Make debug validation inspect changed or newly inserted objects only. Keep full
  scans in doctor/maintenance.

Exit gate:

- `fast_mode`, metadata-only, descriptor-only, append, replacement, and
  truncation each produce exactly the intended revision.
- True no-op commit returns a receipt without advancing revision.
- Revision overflow and every coordinate conversion fail before partial change.
- No canonical table can be written through a second implementation.
- Store, core, xtask, and command tests pass.

### Phase 2: Remove object semantic aliasing

- Add checked request roles with distinct `body_json`/`body_manifest` roots and
  remove duplicate request-attempt payload hash columns.
- Migrate and validate all existing references in one clean schema migration.
- Remove object `kind`, unused object `created_at`, the kind index/API fields,
  duplicate request hash columns, and all kind-based readers.
- Replace recursive manifest code with the shared bounded iterative walker.
- Add exact limit, cycle, missing-object, role-mismatch, same-hash/different-role,
  and migration-corruption tests.
- Ensure cleanup reports preserve both migration and pragma-restoration errors.

Exit gate:

- The final schema has no object semantic kind and runtime has no dual reader.
- Manifest CPU and memory are bounded by explicit limits.
- Garbage collection retains every referenced object and removes only unreachable
  content.
- Full migration tests pass from every supported schema version.

### Phase 3: Establish stable ownership before actor cutover

- Add `.locks/<session-id>.lock` path derivation and strict ID validation.
- For supported pre-root-lock schemas, acquire root then legacy lock through
  migration and track the exact removal condition in `docs/compat.md`.
- Refactor the writer so the root lease and token outlive SQLite connections.
- Put create, open, migrate, maintenance, staged publication, fork destination,
  and delete under the same lease.
- Rewrite first publication to retain the lease across close, rename, root
  fsync, reopen, and verification.
- Add explicit reconciliation for ambiguous publication paths and root-lease-
  guarded cleanup/quarantine for pre-rename crash orphans.
- Update the current persistence path to use this capability before replacing
  its orchestration.
- Remove in-directory `session.lock` creation and all code that deletes or moves
  lock files.

Exit gate:

- Subprocess contention fails during every lease phase, including while SQLite
  is closed and while a legacy-lock binary overlaps migration.
- An active writer prevents delete and destination reuse.
- Rename success followed by any later failure preserves one recoverable staged
  or final database.
- Unexpected destination, token mismatch, and source/destination identity errors
  never overwrite data.
- No write or migration path bypasses the stable lease.

### Phase 4: Atomic cutover to document convergence

Implement the final document and actor protocol as one cutover:

- Move one checked `PersistenceGeneration` and conservative dirty tracking into
  `TuiSessionDocument`; inventory every canonical mutation site and route its
  applied/no-op result through one generation hook.
- Build one cumulative intent for materialized and store-backed sessions.
- Replace the global worker with a fixed-session, fixed-epoch actor, latest-value
  save slot, latest typed status snapshot, wake coalescing, and bounded control
  lane.
- Have the actor own the authoritative head, stable writer, exact in-flight
  command, publication state, audits, flush, and close.
- Implement the single reopen/fingerprint/repeat structural recovery sequence
  with no timers or automatic retry loop.
- Acknowledge and compact only a receipt matching the current document
  generation and actor epoch.
- Make request audits carry epoch and required generation; remove the specialized
  request-history append and descriptor submission paths.
- Delete `RestoreCwd`; keep restored/fallback process CWD outside canonical
  session mutation, while explicit user CWD changes advance generation.
- Replace UI retry state and shutdown loops with explicit retry and
  generation-targeted flush/close.
- Preserve separate semantics for unpublished drafts, ephemeral sessions,
  writable sessions, initially read-only sessions, blocked actors, and
  ownership-lost documents.

Delete in the same phase:

- `crates/core/src/session_save.rs`.
- `DocumentGeneration`, `SessionPersistState`, `PersistDelta`, all pending-save
  types and fields, `SaveId`, and descriptor-submission acknowledgement.
- Global backend session states and all full/live/metadata/request-append command
  variants.
- `PersistenceRetryState`, retry delays/counters, shutdown retry loops,
  `SessionPersistenceDisposition`, and duplicated persistence error wrappers.

Exit gate:

- There is at most one queued intent and one immutable in-flight commit.
- Submission disconnect, actor panic, and status-wake receiver disconnect cannot
  make an unacknowledged document appear saved or block canonical commit.
- A truncating generation can supersede an append generation in flight.
- Store-backed live history stays bounded after matching acknowledgement.
- Flush and close report exact target/durable generations without sleeping.
- Session switch closes the old epoch before audits or writes can target the new
  one.
- Searches confirm every deleted type and old orchestration path is absent.

### Phase 5: Validate lifecycle and failure boundaries

Run fault-injected end-to-end scenarios for:

- Crash before transaction, during transaction, and after commit before receipt.
- First publication before rename, after rename, after root fsync, and before
  verified reopen.
- Latest-slot replacement before and during an in-flight commit.
- Append followed by rewind, truncation, or descriptor replacement.
- Connection invalidation with another process contending for ownership.
- Explicit retry after disk-full, permission, and missing-root failures.
- Ownership token loss with dirty local state followed by fork/save-as.
- Existing unsupported/read-only session edits.
- Stale audit, audit queue saturation, and audit failure after canonical save.
- Sidecar read/rebuild failure after canonical commit.
- Session switch, delete, fork, actor exit, and shutdown deadline expiration.
- Generation and SQLite revision overflow.

Then simplify the changed code, remove unused exports and expired compatibility
entries, verify any legacy-lock migration guard against its documented schema
removal condition, run full format/lint/test/coverage gates, regenerate storage
documentation, and
repeat the large-session measurements.

Exit gate:

- Every failure ends durable, explicitly unsaved, visibly blocked, read-only, or
  ownership-lost. There is no invisible pending state.
- No retry scheduler, generic actor/repository abstraction, snapshot writer,
  object semantic kind, or parallel save path remains.
- Steady-state memory is bounded by one queued intent, one in-flight intent, one
  status snapshot, bounded control/audit queues, and the existing live session
  suffix.
- Performance has no unexplained regression from the Phase 0 baseline.

#### Phase 5 validation record

Recorded on 2026-07-19. The fault matrix uses real temporary SQLite databases,
subprocesses for operating-system lock ownership, and test-only failpoints at the
actor/store boundary. Each row names the deterministic regression that proves the
required terminal state.

| Boundary | Deterministic evidence | Result |
| --- | --- | --- |
| Before transaction, process crash during transaction, and commit before actor receipt | `environmental_commit_failure_uses_one_structural_repeat`, `process_crash_rolls_back_an_open_canonical_transaction`, `publication_failure_blocks_until_explicit_retry` | Uncommitted state rolls back; an ambiguous committed command is recovered by fingerprint and exact replay. |
| Publication before rename, after rename, after root fsync, and before verified reopen | `prepared_publication_is_preserved_if_the_rename_never_starts`, `unexpected_publication_destination_preserves_both_paths`, `token_mismatch_after_rename_preserves_the_published_database`, `publication_retries_after_rename_and_reopen_failure` | One staged or final database remains recoverable and unexpected data is never replaced. |
| Latest replacement before and during commit | `latest_slot_replaces_an_intent_before_consumption`, `truncation_supersedes_an_append_in_flight`, `acknowledgement_does_not_release_a_newer_latest_intent` | The latest cumulative generation becomes durable and an older acknowledgement cannot clear it. |
| Append followed by rewind, truncation, or descriptor replacement | `in_flight_live_save_then_rewind_flushes_without_bad_prefix`, `rewind_to_start_persists_empty_history_and_descriptor_delete`, `descriptor_append_replacement_and_truncation_each_advance_once` | The final history and descriptor suffix is exact, with one intended revision per mutation. |
| Connection invalidation under process contention | `root_lease_remains_exclusive_while_sqlite_is_closed`, `release_reopens_an_invalidated_connection_and_clears_ownership` | The stable lease excludes a second owner while SQLite is closed and reopen verifies the token. |
| Disk-full, permission, and missing-root retry | `environmental_failures_remain_dirty_until_explicit_retry` | Each failure is visible and dirty after both the initial attempt and single structural repeat; only explicit Lua retry makes it durable. |
| Ownership loss, dirty local state, then fork/save-as | `ownership_loss_with_dirty_state_can_fork_to_a_writable_session` | The source remains unchanged and read-only; the fork imports the durable prefix, applies the cumulative history/descriptor suffix without full materialization, and becomes durable under a new ID. |
| Unsupported or initially read-only edits | `blocked_save_requires_explicit_retry`, `resuming_session_with_active_writer_is_read_only`, `repeated_read_only_resumes_do_not_modify_writer_session` | Unsupported saves stay visibly blocked; initially read-only mutation is rejected without changing SQLite. |
| Stale, saturated, and failed audits | `stale_request_audit_after_session_switch_is_rejected`, `full_control_lane_cannot_lose_the_latest_intent`, `audit_failure_after_canonical_save_preserves_durability` | Stale and excess audits are rejected within fixed bounds; audit failure is a warning and cannot undo canonical durability. |
| Sidecar failure after canonical commit | `sidecar_rebuild_failure_does_not_undo_canonical_commit` | The canonical receipt is acknowledged and the derived-cache failure remains a visible warning. |
| Session switch, delete, fork, actor exit, and shutdown deadline | `stale_request_audit_after_session_switch_is_rejected`, `delete_refuses_session_owned_by_another_process`, `sparse_fork_publishes_a_complete_destination`, `actor_panic_stops_submission_without_advancing_durability`, `control_disconnect_stops_submission_without_advancing_durability`, `close_deadline_reports_exact_progress_without_a_delayed_close`, `shutdown_flushes_latest_generation_after_in_flight_save` | Every boundary either closes the exact epoch, refuses under active ownership, reports unsaved progress, or completes durably. An expired close cannot stop the actor later. |
| Generation and SQLite revision overflow | `persistence_generation_is_checked`, `revision_overflow_fails_without_partial_mutation` | Overflow is rejected before partial mutation. |

The exact Phase 0 benchmark command completed successfully after the descriptor
fixture was corrected to store its generated canonical session identity. The
10,000-row hot path compared with the Phase 0 baseline as follows:

| Operation | Phase 0 mean | Phase 5 mean | Change |
| --- | ---: | ---: | ---: |
| No-op save | 0.019 ms | 0.017 ms | -0.002 ms |
| Request append | 6.400 ms | 6.439 ms | +0.6% |
| History append | 3.600 ms | 4.279 ms | +18.9% |
| Turn complete | 0.142 ms | 0.150 ms | +0.008 ms |
| Rewind/delete suffix | 7.307 ms | 7.830 ms | +7.2% |
| Provider history read | 0.701 ms | 0.664 ms | -5.3% |

Request and history intents occupied 1,170 and 792 bytes, down from 1,415 and
1,037 bytes. The latest slot remained bounded to one intent. Request and history
SQLite commits took 77-87 us and 89-110 us; all remained at or below 0.110 ms.
The history-append wall time increased by 0.679 ms, but its store metrics still
show one dirty row, one inserted row, no deletion, and one cached writer
connection. The sub-millisecond change therefore has no scaling or full-history
write regression.

The valid 10 MiB fixture again contained 2,560 descriptors and 108,877 rows.
Tail load fell from 7.548 ms to 1.544 ms and tail render fell from 2.799 ms to
1.939 ms. The combined layout/hot-path process peaked at 98,612 KiB versus
97,788 KiB in Phase 0. The resume-only process peaked at 48,024 KiB versus
32,760 KiB; that setup peak is not directly comparable because the corrected
benchmark now performs canonical fixture initialization and cleanup under the
generated identity. The increase does not appear in the tail timing or the
combined process peak.

The `COMPAT(storage-root-lease)` guard remains required. Writable migration from
schemas older than v6 is still supported, so its documented removal condition in
`docs/compat.md` has not expired. Searches also confirm that the deleted retry
scheduler, global save worker, snapshot protocol, semantic object kind, and
parallel persistence variants remain absent.

## Testing strategy

Use real temporary SQLite databases, deterministic failpoints, and subprocess
ownership tests. Do not add a repository trait solely for mocks.

### Pure document-state tests

Cover:

- Checked generation advance and overflow.
- Conservative dirty-boundary expansion for append, replacement, rewind,
  truncation, metadata, and descriptors.
- Cumulative intent construction across SQLite prefix and live suffix.
- Matching-generation acknowledgement and compaction.
- Older, future, malformed, and wrong-epoch durable statuses/flush receipts.
- Unpublished-draft, ephemeral, initially read-only, blocked, and ownership-lost
  durability semantics.

Use generated mutation sequences to compare each cumulative intent with a fully
materialized canonical session. This is the most important proof that latest-
value replacement is safe.

### Actor integration tests

Drive the concrete actor handle and latest slot:

- Wake coalescing with no lost desired state.
- Replacement before commit and while a command is in flight.
- Equal/older generation rejection.
- Control queue saturation and disconnected actor reporting.
- Audit required-generation and epoch checks.
- Exact fingerprint reconciliation after ambiguous commit.
- Single reopen and exact repeat when the head is unchanged.
- Block instead of rebase when the head differs.
- Explicit retry with no hidden timer.
- Flush target, deadline, blocked result, close/submit race, no ghost close, and
  thread join.
- Status overwrite/wake coalescing, flush while the status wake is full, wake
  receiver disconnect, and control sender disconnect.

Use synchronization barriers rather than sleeps to force interleavings.

### Store and subprocess tests

Cover:

- Structural metadata equality and all revision/no-op cases.
- Expected-head, immutable identity, suffix, cross-table, and overflow checks.
- Exact fingerprint receipt replay without `SaveId`.
- Root lease retention while SQLite is absent or closed.
- Owner-token verification after reopen and stale-token claim on fresh lease.
- Every staged publication reconciliation shape.
- Active-writer contention with delete, migration, repair, and fork destination.
- Migration error aggregation and final-schema validation.
- Same-hash/different-role objects, garbage collection, and manifest limits.

Use subprocesses, not two handles in one process, for OS lock claims.

### End-to-end harness tests

Drive the same app paths a user triggers and inspect rendered status/dialogs:

- Metadata-only empty draft, first history/descriptor content, and lazy first
  publication.
- Save during an agent turn and mutation while saving.
- Rewind/truncate while an append is in flight.
- Environmental block, sticky unsaved status, and explicit retry.
- Ownership loss followed by fork/save-as.
- Session switch with late durable status and audit host calls.
- Read-after-generation-targeted flush.
- Normal close, blocked close, and shutdown deadline.
- Sidecar warning after canonical durability.

Persistence UI must never report saved while the current generation lacks a
matching receipt.

## Validation commands

Run after each meaningful phase:

```bash
set -o pipefail; cargo fmt -- --check 2>&1 | tail -120
set -o pipefail; cargo nextest run -p smelt-store 2>&1 | tail -120
set -o pipefail; cargo nextest run -p smelt-core --test storage_failure_boundaries 2>&1 | tail -120
set -o pipefail; cargo nextest run -p smelt-tui --features harness persistence 2>&1 | tail -120
```

Run before declaring the architecture complete:

```bash
set -o pipefail; cargo nextest run --workspace --features smelt-tui/harness 2>&1 | tail -120
set -o pipefail; cargo clippy --workspace --all-targets --features smelt-tui/harness -- -D warnings 2>&1 | tail -120
set -o pipefail; cargo llvm-cov nextest --workspace --features smelt-tui/harness --fail-under-lines 80 2>&1 | tail -120
cargo build --profile release-fast --bin smelt
```

Run relevant storybook snapshots when persistence status or shutdown dialogs
change.

## Observability

Record bounded, non-sensitive metrics for:

- Current generation minus actor durable generation.
- Whether the latest slot is occupied and its approximate payload bytes.
- Number of intents replaced before execution.
- Canonical commit and staged publication latency.
- Structural reopen, fingerprint reconciliation, and explicit retry counts.
- Blocked and ownership-lost transitions by structured cause.
- Flush target lag and deadline expiration.
- Audit rejection/failure, overwritten-warning, and sidecar warning counts.

Generation values and session IDs may be included in local debug diagnostics,
but never log session content, request bodies, object bytes, secrets,
credentials, or owner tokens. Metrics must not retain unbounded per-session
labels.

## Explicit non-goals

- No event sourcing or global all-session database.
- No generic repository, storage-backend, or actor framework.
- No asynchronous SQLite wrapper.
- No FIFO of complete session snapshots.
- No separate request-audit writer thread.
- No timed automatic retry policy in store, actor, UI, or shutdown.
- No statement- or transaction-body retry loop.
- No `SaveId` or replacement operation-ID sequence.
- No snapshot write protocol.
- No canonical dependence on sidecars.
- No permanent compatibility layer or dual schema reader.

## Risks and mitigations

### A cumulative intent omits superseded work

Risk: G2 replaces queued G1 but does not contain a change that existed only in
G1.

Mitigation: never clear dirty ranges on submission, always build from the last
matching document acknowledgement, and property-test mutation sequences against
a fully materialized session.

### An older receipt destroys newer live history

Risk: G1 commits after G2 rewrites part of G1's prefix, and G1 acknowledgement
compacts away G2.

Mitigation: apply receipts to document state only when observed generation equals
current generation and actor epoch matches. Older status updates affect actor
progress only.

### Latest-slot wake is lost

Risk: producer replaces the slot while the actor transitions to waiting and no
wake remains queued.

Mitigation: define the intent slot/wake handoff as a small state machine, recheck
the slot after draining a wake and immediately before waiting, and treat a full
wake as success. Status publication writes the snapshot before its coalesced
wake. Test every barrier-controlled producer/consumer interleaving.

### Ambiguous commit or publication is repeated incorrectly

Risk: SQLite commit or directory rename succeeded but its caller observed an
error.

Mitigation: retain the exact command, fingerprint, lease, staging/final paths,
and token. Reconcile persisted evidence before one exact repeat. Never build a
new command or fresh staging directory while the outcome is unknown.

### A blocked actor monopolizes ownership

Risk: an environmental failure keeps the stable lease while another operation
wants to repair or delete the same session.

Mitigation: keeping ownership is required while commit/publication is ambiguous.
Expose explicit retry, close-with-unsaved-result, and fork/save-as. Only an
explicit actor close releases the lease; maintenance never steals it.

### Audit work targets the wrong session

Risk: a host response arrives after session switch and is appended to the new
session.

Mitigation: bind every audit to session ID, actor epoch, and required generation,
then validate at host dispatch and again inside the actor.

### Stable lock files accumulate

Risk: deleted session IDs leave small files under `.locks`.

Mitigation: never unlink lock files because unlinking permits two processes to
lock different inodes for one session ID. Keep contents minimal and permissions
restricted. If scale requires it, shard lock paths by validated ID prefix without
changing lease semantics.

### Migration cannot infer damaged old object semantics

Risk: first-writer `objects.kind` already disagrees with bytes or references.

Mitigation: preserve old decoding deterministically during backfill, validate
all resulting edges and manifests, and stop with an integrity error instead of
guessing or silently dropping audit data.

### Architectural transition leaves two paths

Risk: runtime, import, synthesis, or fixtures keep different revision semantics.

Mitigation: each phase inventories callers, routes all writers through the
concrete canonical updater, and requires deleted old exports as an exit gate.

## Definition of done

This checklist records the current best end state. If implementation evidence
supports a simpler design with equal or stronger correctness, update the relevant
item rather than preserving a stale requirement. The architecture is complete
only when the resulting plan and code satisfy all of the following:

- One actor exists per active writable session and fixed session epoch.
- New empty sessions publish lazily; ephemeral and read-only sessions have no
  writable actor.
- The document owns one checked generation, conservative dirty boundaries, and
  an acknowledged head projection. It has no pending-save snapshot.
- The actor owns one stable root lease, optional SQLite connection,
  authoritative head, latest intent, exact in-flight command, latest typed status,
  publication state, audits, flush, and close.
- At most one queued intent and one in-flight intent retain large payloads.
- No automatic timed retry, UI backoff, store transaction loop, or shutdown retry
  loop remains.
- Reopen retains the root OS lock and owner token; ambiguous outcomes reconcile
  exact fingerprints before one exact repeat.
- Queue/control rejection, actor exit, malformed receipt, and stale epoch always
  leave current local changes visibly unsaved.
- One canonical transaction handles runtime, import, repair, synthesis, fork,
  synchronous save, and fixtures.
- Metadata, history, side tables, and descriptors contribute to one checked
  revision decision; true no-ops do not increment it.
- Session ID, creation time, and fork parent are immutable after first insert.
- `objects.kind` is absent, all semantics are reference-level, and manifest
  traversal is iterative and bounded by depth, count, and bytes.
- Root lock covers staging publication, migration, maintenance, fork destination,
  and delete; lock files are never moved or unlinked.
- Request audits are epoch-checked, generation-ordered, best effort, and unable
  to release canonical ownership.
- Sidecar failure cannot change canonical durability or flush success.
- Shutdown reports the exact target and durable generations and joins the actor.
- `crates/core/src/session_save.rs`, `session_snapshot.rs`, save IDs, persistence
  dispositions, global worker states, specialized save variants, `RestoreCwd`,
  retry state, and runtime compatibility adapters are deleted, not merely unused.
  A legacy lock migration guard exists only while its documented old schema
  versions are supported.
- No generic repository, backend, actor, or async SQLite abstraction was added.
- Full workspace formatting, tests, clippy, coverage, release build, fault
  scenarios, and relevant rendered UI snapshots pass.
