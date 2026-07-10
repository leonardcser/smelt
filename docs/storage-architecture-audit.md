# Storage architecture audit and hardening plan

Status: proposed  
Audit target: `6ff5f188e fix(storage): preserve canonical history through compaction`, on top of `bc17a9cbe refactor(storage): consolidate session persistence spine`

## Executive decision

Keep the current direction:

- one SQLite database per session
- one authoritative writer per session
- concurrent read-only viewers
- the typed `SessionCommit` and `SaveReceipt` protocol
- sparse resume with a persisted history prefix and an in-memory live suffix
- canonical history kept distinct from the compacted model-visible projection
- SQLite as canonical state, with `meta.json` and `content.txt` treated only as derived caches

The storage-spine refactor is a real improvement. It makes history, session metadata,
rewindable snapshots, and transcript descriptors one optimistic, atomic commit. It
addresses the logical divergence failures seen in existing logs. The follow-up compaction
fix is also the right design: compacted model input is now an explicitly mapped projection
of canonical history rather than a replacement for it.

Do not replace this with event sourcing, a global sessions database, automatic
multi-writer merging, or a generic repository framework. The highest-value work is a
focused hardening pass around ownership, open modes, transaction behavior, filesystem
safety, error handling, attachments, and lifecycle cleanup.

The recommended end state is:

1. A concrete `SessionReader` for query-only access.
2. A concrete `OwnedSessionWriter` held only by the persistence worker.
3. A concrete maintenance path for migration, repair, import, fork, object GC,
   database vacuum, and deletion.
4. A lifetime-held per-session OS lock plus an in-database owner token for diagnostics
   and fencing.
5. Every session mutation, including request audits and maintenance, routed through the
   owned writer or an exclusive maintenance operation.
6. No writes, migrations, repairs, sidecar regeneration, or directory creation from an
   ordinary read path.
7. Structured errors and bounded retries for transient lock contention only.
8. Attachments stored transactionally in SQLite's existing content-addressed object
   store.

## Priority summary

| Priority | Change | Why |
| --- | --- | --- |
| P0 | Validate session IDs and confine all filesystem paths | Prevent path traversal and destructive deletion outside the session root |
| P0 | Make every ordinary read strictly read-only | Remove migrations and repair writes from readers and provider dispatch |
| P0 | Introduce atomic, lifetime writer ownership | Prevent two CLIs from both believing they own one session |
| P0 | Fence every mutation with ownership | Request audits, repair APIs, delete, and other writes currently do not share one ownership rule |
| P0 | Use RAII SQLite transactions | Guarantee rollback on body and commit errors and avoid poisoned cached connections |
| P0 | Preserve detailed storage errors | Do not turn corruption, unsupported schema, lock contention, and I/O failure into “missing session” |
| P0 | Secure state permissions | Sessions can contain prompts, tool output, request bodies, and secrets |
| P1 | Centralize lifecycle in the persistence backend | Make open, claim, commit, flush, release, and close one explicit state machine |
| P1 | Add bounded busy retry and reconnect policy | Recover from transient contention without retrying integrity failures |
| P1 | Make shutdown deterministic and observable | Remove arbitrary retry loops and release ownership cleanly |
| P1 | Make create, fork, and delete staged operations | Prevent partially published or partially deleted session directories |
| P1 | Move attachments into SQLite objects | Remove the external-file transaction gap and simplify fork, rewind, and recovery |
| P1 | Make derived files revisioned, atomic, and best-effort | Keep caches useful without confusing them with canonical state |
| P1 | Make grouped reads snapshot-consistent | Avoid headers, metadata, descriptor counts, and tails from different revisions |
| P1 | Make rewindable side tables use history-boundary semantics | Preserve final-boundary turn metadata across save and resume |
| P1 | Add object garbage collection and size limits | Bound growth from rewind and reject corrupt oversized objects safely |
| P2 | Tighten schema constraints and remove dead schema | Move invariant enforcement closer to the data and reduce maintenance surface |
| P2 | Add WAL size policy and explicit close-time checkpointing | Bound retained WAL files without adding hot-path checkpoints |
| P2 | Add a storage doctor and consistent backup/export | Make corruption and recovery actionable rather than silent |
| P2 | Add real multi-process and crash-injection tests | Validate the behavior that thread-only tests cannot prove |
| Later | Consider a global read-only catalog only if measured | Per-session sidecars should remain the simple scaling mechanism until proven insufficient |

## Latest commit assessment

`6ff5f188e` should be kept. It corrects a domain-boundary bug without changing the storage
schema or weakening sparse resume:

- model-visible history now carries `ModelHistoryCoordinates`
- engine history events carry `CanonicalHistoryDelta`
- synthetic checkpoint summaries are stripped by coordinate mapping rather than by
  content-prefix heuristics
- TUI history replacement applies a canonical suffix instead of replacing a projected
  snapshot
- rewind and generic truncation prune rewindable state in the same document mutation
- side-table serialization validates every staged key against final canonical history
- the end-to-end compaction test proves the old canonical prefix survives save and resume

This is a stronger and simpler separation of concerns than teaching persistence about
compacted model messages. Storage continues to own canonical semantic history; compaction
owns only checkpoint metadata and provider projection.

The commit does not materially change the remaining multi-process, read/write open,
transaction cleanup, path confinement, attachment, derived-cache, or error-propagation
findings. One follow-up is warranted: make `turn_metas` persist the valid final history
boundary instead of staging it only until another history row exists.

## Current architecture

### On-disk layout

Each session lives under the state directory:

```text
$XDG_STATE_HOME/smelt/sessions/<64-hex-session-id>/
  session.db       canonical session state
  session.db-wal   SQLite WAL while present
  session.db-shm   SQLite shared-memory index while present
  meta.json        derived list and preview metadata
  content.txt      derived searchable text
  blobs/           external attachment data URLs
```

`session.db` is canonical. The two sidecars and the blob directory are not protected by
the SQLite transaction.

Read-write connections currently use WAL, `synchronous=NORMAL`, foreign keys, a five
second busy timeout, and in-memory temporary storage. Read-only connections set
`query_only=ON`. See `crates/store/src/db.rs:1150`.

### Canonical schema

Schema version 2 is defined in `crates/store/src/schema.rs:238`.

| Table | Responsibility |
| --- | --- |
| `store_meta` | Schema/app metadata and the serialized writer lease |
| `session_state` | Singleton identity, display metadata, checkpoint/accounting state, revision, history length, and timestamps |
| `history_items` | Dense semantic model history with normalized JSON and hashes |
| `transcript_blocks` | Sparse transcript projection and persisted descriptor records |
| `transcript_search` | Search text associated with transcript blocks |
| `transcript_search_fts` | FTS5 trigram index maintained by triggers |
| `objects` | Content-addressed large payloads, optionally zstd compressed |
| `history_object_refs` | Object reachability from history rows |
| `request_attempts` | Request timing, provider, model, status, and summary audit data |
| `request_stats` | Token, cost, context, cache, and throughput statistics |
| `request_object_refs` | Object reachability from request audits |
| `turn_metas` | Rewindable per-turn metadata keyed by a semantic history boundary, currently restricted to boundaries strictly before the final length |
| `metadata_snapshots` | Rewindable session metadata keyed by a history boundary at or before the final length |
| `accounting_snapshots` | Rewindable context/accounting state keyed by a history boundary at or before the final length |
| `turn_tool_elapsed` | Present in the schema but not used by production Rust code |

Objects are hashed over uncompressed bytes and verified when hydrated. Large request
payloads and selected history metadata use this object store. Attachments do not.

### Save flow

```text
TUI mutation
  -> SessionDocument records dirty generation and dirty suffixes
  -> core save planner chooses skip, metadata-only, or history save
  -> core constructs SessionCommit
  -> TUI sends it to the FIFO persistence worker
  -> worker writes external attachment blobs
  -> worker opens or reuses a read-write SessionDb
  -> BEGIN IMMEDIATE
  -> validate revision/history/descriptor bases and command shape
  -> replace history and side-table suffixes
  -> replace transcript descriptor suffix
  -> validate post-write invariants
  -> COMMIT
  -> refresh meta.json and content.txt
  -> return SaveReceipt
  -> core advances durable cursors or schedules reconciliation
```

The command contains independent typed bases for revision, history length, and
transcript descriptor length. See `crates/store/src/session_commit.rs:62`.

The transaction validates:

- session identity
- expected revision
- expected dense history length
- expected dense descriptor length
- exact suffix shape
- side-table index bounds
- descriptor-to-history bounds and kind compatibility
- dense history indices after the write
- dense descriptor indices after the write
- session-state history length against row count
- side-table and object-reference bounds
- object payload hashes in debug builds

The transaction boundary in `crates/store/src/db.rs:425` is the strongest part of the
current design and should remain.

### Request audit flow

Request audit commands share the same FIFO worker and cached read-write connection as
session commits. A request audit is its own immediate transaction. Summary mode stores
metadata and statistics. Full mode also stores request, response, and error payloads in
content-addressed objects. See `crates/tui/src/persist.rs:143` and
`crates/store/src/request_audit.rs:194`.

Request audits are intentionally not atomic with conversation commits. They are audit
telemetry and may survive a crash even when the latest conversation mutation does not,
or vice versa. That separation is acceptable, but both writes must obey the same owner
fence.

### Restore and resume flow

Normal resume does not materialize all history. It loads:

- the singleton session header
- history and revision extents
- a bounded transcript descriptor tail
- a `SessionStoreRef` for later reads

`LiveSession` then represents:

```text
persisted SQLite prefix + in-memory live suffix
```

Provider dispatch reads only the needed persisted range and appends the live suffix.
Transcript rendering and search use bounded SQLite reads and long-lived read-only
connections. Full materialization remains limited to explicit detail, export, debug,
and test paths.

This sparse model is the right scaling design. The provider path currently violates it
at the connection-mode boundary by opening the database read-write for a read at
`crates/engine/src/agent.rs:250`.

### Canonical history and context compaction

Context compaction is now a projection operation, not a canonical history rewrite:

```text
canonical history:  [old prefix] [live tail]
model projection:   [synthetic checkpoint summary] [live tail]
                                      |
                         ModelHistoryCoordinates
                                      |
engine updates:     canonical suffix deltas only
```

A `ContextCheckpoint` stores the summary and `first_live_index`. `ModelHistorySource`
loads the synthetic summary plus only the required canonical tail. The engine carries
`ModelHistoryCoordinates`, maps model-visible boundaries back to canonical boundaries,
and emits `CanonicalHistoryDelta` values. The TUI applies those deltas to the canonical
`LiveSession`; synthetic checkpoint items are never persisted as history rows. See
`crates/protocol/src/event.rs:200`, `crates/core/src/session_runtime.rs:218`, and
`crates/engine/src/agent.rs:214`.

This closes an important pre-audit failure mode: a compacted provider snapshot can no
longer replace the retained canonical prefix or make subsequent side-table indices refer
to projection coordinates. The new end-to-end test verifies compaction, append, save,
and resume while preserving the full canonical prefix at
`crates/tui/src/app/harness_tests/persistence.rs:965`.

Preserve these invariants:

- context compaction never deletes or replaces canonical history
- a checkpoint summary is synthetic model input, not a canonical `history_items` row
- all persistence dirty ranges and engine-to-TUI deltas use canonical coordinates
- a coordinate mapping is immutable for one model request
- rewind is the only normal user operation that truncates canonical history

Do not route context compaction through `SessionMaintenance`. Installing a checkpoint is
an ordinary session mutation committed through `SessionCommit`. Reserve “database
compaction” for object GC, `VACUUM`, and other exclusive physical maintenance.

### Rewind flow

Rewind finds the semantic history boundary, truncates live semantic history and
transcript state, restores rewindable metadata/accounting snapshots, and marks the
corresponding history and descriptor suffixes dirty. The next `SessionCommit` replaces
and truncates all canonical suffixes atomically.

Request audits are not rewound. They describe actual requests that occurred and should
remain immutable audit history. Unreferenced objects and external blobs currently remain
behind after rewind.

### Fork flow

There are currently two fork paths:

- a materialized path that clones the in-memory session and saves it as a new session
- a sparse path that flushes, creates a new database, attaches the source database,
  copies a history prefix and related tables, copies the entire blob directory, refreshes
  derived files, and resumes the destination

The sparse database copy is transactionally coherent. The surrounding destination
directory publication and blob copy are not atomic. A failed blob copy or derived refresh
can leave a partial fork directory. Request audits correctly remain with the parent.

### Shutdown flow

The app stops producers, asks for a save, flushes the worker, drains receipts, and repeats
up to 64 times while work remains. `Persister::drop` flushes and joins the worker. See
`crates/tui/src/app/history.rs:1829` and `crates/tui/src/persist.rs:114`.

The latest-generation flush tests pass, but ownership is not released, generic failures
are not classified, and the fixed iteration count is not a meaningful durability policy.

## What is already good and should be preserved

1. **Per-session databases**
   - Sessions can be opened, copied, deleted, exported, and repaired independently.
   - Contention and corruption blast radius stay bounded to one session.
   - Different CLIs can safely own different sessions concurrently.

2. **Typed commit coordinates**
   - `Revision`, `HistoryLen`, and `DescriptorLen` cannot be accidentally compared across
     domains.
   - Stale state becomes explicit rather than silently overwriting newer state.

3. **One logical session transaction**
   - Session state, semantic history, side tables, object references, transcript
     descriptors, and search projection advance together.

4. **Sparse resume**
   - Runtime memory and startup cost depend on the visible tail and current request range,
     not total session size.

5. **Canonical model-history coordinates**
   - Compaction can inject a synthetic summary and omit old model input without rewriting
     canonical session history.
   - Engine deltas carry typed canonical boundaries, so projection indices cannot silently
     become persistence indices.

6. **Content-addressed objects**
   - Hashing, compression, deduplication, references, and hydration verification already
     provide the right foundation for all large payloads.

7. **Derived sidecars**
   - A small metadata file remains a simple way to list many sessions without a global
     coordination database.
   - Sidecars are safe only if all code treats them as disposable and potentially stale.

8. **SQLite WAL and foreign keys**
   - WAL is appropriate for one writer plus concurrent readers on a local filesystem.
   - Foreign-key enforcement and post-commit validation catch important divergence.

## Findings and concrete changes

## P0: Filesystem confinement and destructive-operation safety

### Problem

Session IDs cross filesystem boundaries as plain strings. `delete(id)` currently joins
an unchecked string under the sessions directory, calls `remove_dir_all`, ignores the
result, and is callable from Lua. See `crates/core/src/session.rs:1859` and
`crates/tui/src/lua/api/session.rs:921`.

An absolute path or parent traversal must never be able to select a path outside the
session root. Symlinked session directories and database files also make write and delete
behavior ambiguous.

### Change

- Add one parser for persisted session IDs.
- Accept exact IDs only when they are exactly 64 lowercase hexadecimal characters.
- Accept prefixes only through a separate resolver that rejects empty strings, path
  separators, absolute paths, `.`/`..`, non-hex characters, and overly short prefixes.
- At every filesystem boundary, derive the path from a validated ID rather than accepting
  a caller-supplied `PathBuf`.
- Keep `String` in serialized DTOs if that avoids churn, but parse before path access.
- Reject symlinked session directories, databases, lock files, and staging destinations
  for mutation and deletion.
- Change delete to return a typed `Result` and propagate the error through Lua and UI.
- Add a final confinement assertion before recursive rename or removal.

### Acceptance tests

- Delete rejects `/tmp/x`, `../x`, `a/b`, empty input, uppercase input, malformed hex, and
  ambiguous prefixes.
- A symlinked session directory cannot be deleted or opened writable.
- A valid exact ID and valid unique prefix still work.
- Lua receives an actionable error instead of silent success.

## P0: Strict read-only query paths

### Problem

`SessionDb::open` combines create, read-write open, write pragmas, and migration. It is
therefore too easy for a read path to become a writer.

Concrete violations:

- provider model-history loading uses `SessionDb::open` at
  `crates/engine/src/agent.rs:250`
- header loading opens read-write and runs repairs before reopening read-only at
  `crates/core/src/session.rs:1391`
- read-write open always runs migration at `crates/store/src/db.rs:96`
- sidecar and search reads can regenerate files as a side effect

This creates avoidable lock contention and allows a second CLI to migrate or repair a
session before ownership is decided.

### Change

Create concrete, non-trait types:

```text
SessionReader
  open_existing_read_only
  header/read/query/export methods only

OwnedSessionWriter
  created only after exclusive ownership
  commit/request-audit/derived-refresh methods

SessionMaintenance
  migration/repair/import/fork/object-gc/vacuum/delete under exclusive ownership
```

Then:

- make `SessionDb` and the raw SQLite connection internal implementation details
- remove public write methods from reader-capable types
- change provider model-history loading to a read-only range query
- make all transcript/search/detail/export paths use `SessionReader`
- do not create directories or databases from any read function
- do not migrate, repair, checkpoint, or regenerate sidecars during an ordinary read
- make grouped resume reads use one read snapshot rather than several independent opens

A missing sidecar may be rebuilt only by the owner after commit or by explicit
maintenance. A reader can fall back to canonical SQLite without caching the result.

## P0: Atomic lifetime writer ownership

### Problem

The current database lease is advisory metadata. It uses `hostname:pid`, checks and sets
outside an explicit claim transaction, and is not released in production. Initial session
creation is unleased until the first commit. PID reuse can make a new process look like
the old owner. Cross-host leases expire after 30 minutes even if an idle owner remains
alive. See `crates/store/src/meta.rs:52` and `crates/store/src/meta.rs:92`.

There is also a migration race: read-only open rejects schema v1, so lease probing can
fail, after which read-write open migrates before claiming ownership.

### Change

Use two simple, complementary mechanisms:

1. **Lifetime OS lock**
   - Create `session.lock` inside the validated session directory.
   - Acquire an exclusive, nonblocking advisory lock before writable open.
   - Hold the file descriptor for the entire `OwnedSessionWriter` lifetime.
   - Let the OS release it automatically on process death.
   - If unavailable, open the session read-only and report the recorded owner.

2. **In-database fencing token**
   - Generate a random owner token for each successful claim.
   - Store token, hostname, PID, process-start identity, app version, and claim time in
     `store_meta`.
   - In one `BEGIN IMMEDIATE` transaction, inspect compatibility metadata, migrate if
     allowed, run pending one-time repairs, write the token, and commit.
   - Check the token at the start of every mutating transaction.
   - Clear it on clean close only when it still matches the handle token.

The OS lock is the authoritative lifetime lock. The database token provides diagnostics
and fences stale handles or accidental mutation paths. Once the OS lock is acquired,
old serialized lease metadata is stale and can be replaced. This removes PID liveness,
heartbeat expiry, and check-then-set races from ownership decisions.

If a supported cross-platform OS lock cannot be provided, the fallback is a fully
transactional random-token lease with periodic heartbeat. Do not keep the current
non-transactional check-then-set behavior.

### Ownership rules

- One CLI can own one session for writes.
- Any number of CLIs can open that session read-only under WAL.
- A second CLI may explicitly fork from its read snapshot.
- A second CLI may not “force” takeover while the OS lock is held.
- A reader losing access to the file reports an error and never silently promotes itself.
- Every write, including request audits, repairs, imports, sidecars, fork destination
  creation, database vacuum/GC, and deletion, requires an owned or exclusive handle.
- Context compaction is different: it installs checkpoint metadata through the ordinary
  owned `SessionCommit` path and does not rewrite canonical history.

### Filesystem support boundary

SQLite WAL does not support a database shared by processes on different hosts over a
network filesystem. Define the session state directory as local-filesystem storage.
Fail closed for writable open when reliable advisory locking is unavailable. Do not imply
that the serialized hostname lease makes cross-host WAL safe.

## P0: Transaction correctness and connection recovery

### Problem

The manual transaction helpers issue `BEGIN IMMEDIATE`, call a closure, and issue
`COMMIT` or `ROLLBACK`. If `COMMIT` fails, rollback is not guaranteed. A cached connection
can remain inside a transaction and retain locks. Similar manual transaction code exists
in commit, migration, snapshot save, and descriptor repair. See
`crates/store/src/db.rs:125`, `crates/store/src/db.rs:425`, and
`crates/store/src/schema.rs:8`.

### Change

- Use rusqlite transaction objects with RAII rollback.
- Make the writer connection mutable and owned by the worker as required by the safe
  transaction API.
- Consolidate transaction setup in one internal helper that supports `Immediate` and
  records the operation stage.
- On rollback failure, I/O error, corruption, or a transaction-state error, discard the
  cached connection.
- Reopen only while the OS lock is still held and the fencing token can be revalidated.
- Never retry a transaction body after an ambiguous write outcome.
- Retry only acquisition of the write transaction before the first mutation.

Persisted revision and history bases still make an accidentally repeated command safe to
reconcile, but they should not substitute for correct transaction cleanup.

## P0: Structured errors instead of silent disappearance

### Problem

Many list, restore, header, search, and full-load paths use `Option` and `.ok()?`. Missing,
ambiguous, corrupt, unsupported, malformed, locked, and inaccessible sessions collapse
into the same result. Repair failures are ignored. Search silently drops failed sessions.
Commit errors other than stale bases are flattened into `Integrity` at
`crates/store/src/db.rs:694`.

### Change

Use a small concrete taxonomy at the core boundary:

```text
InvalidSessionId
NotFound
AmbiguousPrefix
ReadOnlyOwnerConflict { owner }
Busy { operation }
OwnershipLost
UnsupportedSchema { found, supported }
Corrupt { context }
Integrity { invariant }
Io { operation, path }
Sqlite { operation, code }
```

For commit results, preserve:

```text
StaleBase { expected: StoreHead, current: StoreHead }
OwnershipLost
Busy { operation }
InvalidCommand
Integrity
Io
Sqlite
```

`StoreHead` contains revision, history length, and descriptor length together. On any
stale base, return the whole current head so core can replace all durable cursors and
replan once instead of discovering one mismatch per retry.

Exact loads should return `Result<Option<T>, SessionOpenError>`. Session listing should
return valid and invalid entries together, so one corrupt session does not fail the list
and also does not disappear:

```text
SessionListEntry { id, status: Available(meta) | Unavailable(error) }
```

The UI should show unavailable sessions with a concise reason and offer doctor/export or
remove actions. Lua can retain a convenience list of available sessions but should expose
errors through a companion field or API.

## P0: Storage privacy and permissions

### Problem

Session databases, request payloads, tool output, sidecars, and attachments may contain
sensitive data. Creation currently relies primarily on the caller's umask.

### Change

- Create the smelt state directory and session directories as `0700` on Unix.
- Create databases, WAL/SHM companions where controllable, lock files, sidecars,
  attachments, staging files, and exports containing payloads as `0600`.
- Repair overly broad permissions on owned writable open, with an actionable warning if
  repair fails.
- Keep platform-specific behavior best effort where exact modes do not exist.
- Never include request bodies, history JSON, attachment content, or API credentials in
  storage errors or lock-owner diagnostics.
- Keep full request-audit payload capture explicitly opt-in and document its privacy and
  disk implications.

## P1: One persistence backend owns the complete write lifecycle

### Problem

The worker serializes normal commits and request audits well, but writable open, claim,
repair, fork, delete, and sidecar work remain spread across core, TUI, and store APIs.
Commands carry arbitrary session paths even though the backend should know the active
owned session.

### Change

Keep one concrete worker and command enum, but give it an explicit state:

```text
Closed
ReadOnly { session_id }
Owned { session_id, lock_guard, writer, owner_token }
Closing
```

Backend operations:

```text
OpenOwned(session_id)
Commit(SessionCommit)
AppendRequestAudit(entry)
RefreshDerived
Flush
Release
Fork { destination_id, history_len }
Delete(session_id)
RunMaintenance(operation)
Shutdown
```

Important constraints:

- Commands use validated session IDs, not caller-provided paths.
- A commit's session ID and state ID must match the backend's owned session.
- Switching sessions flushes the old queue, closes session readers, releases the old
  token and lock, then opens the new session.
- Request audits refresh and verify ownership just like session commits.
- Test-only repair methods do not remain generally callable production mutation APIs.
- Do not add an async trait, repository trait, or pluggable backend abstraction. The
  concrete command worker is enough.

## P1: Bounded transient retry policy

### Problem

`BUSY` and `LOCKED` are identifiable in `StoreError`, but commit conversion erases that
information. Normal saves do not automatically retry generic failures. Shutdown can
repeat failed saves through an arbitrary 64-iteration loop.

### Change

- Classify SQLite `BUSY` and `LOCKED` separately from integrity failures.
- Retry only `BEGIN IMMEDIATE` acquisition before transaction-body mutation.
- Use exponential backoff with jitter, a short per-attempt busy timeout, and a total
  elapsed budget around five seconds.
- Keep retry policy in the worker, not in UI state.
- Never retry `OwnershipLost`, `UnsupportedSchema`, malformed commands, corruption,
  foreign-key failures, or failed invariant checks.
- Reopen a connection only for errors classified as connection-invalidating.
- Report attempts, total wait, and final operation in structured diagnostics.
- On stale base, install the returned `StoreHead` and replan from current in-memory state.

Do not combine a five-second SQLite busy timeout with several five-second retries. One
bounded policy should own the total latency budget.

## P1: Deterministic shutdown and clean release

### Change

Replace the fixed loop with explicit completion:

1. Stop engine, Lua, and background producers for the session.
2. Apply pending history mutations.
3. Submit the latest document generation.
4. Drain commits and audits in FIFO order.
5. Replan after receipts or stale-head reconciliation until the document is clean.
6. Retry transient transaction acquisition within the bounded policy.
7. If a permanent error remains, report the exact unsaved generation and require an
   explicit interactive quit decision where possible.
8. Drop all session read handles.
9. Best-effort checkpoint/truncate the WAL.
10. Clear the matching database owner token.
11. Close the writer and release the OS lock.
12. Return a structured shutdown outcome.

`Persister::flush` should return success, permanent failure, worker-exited, or timeout. It
must not silently become a no-op when the worker has died.

Do not add a second ad hoc recovery journal. If SQLite and the session directory cannot
be written, a side journal on the same storage is not a reliable durability mechanism and
would create another source of truth. Preserve the last valid SQLite commit and make the
failure visible.

## P1: Staged create, fork, and delete

### Session creation

- Create a new session in a private staging directory under the sessions root.
- Acquire its lock, initialize schema, install ownership, and make the first canonical
  commit.
- Validate the database and derived-cache inputs.
- Atomically rename the staging directory to the final validated ID.
- Clean abandoned staging directories during explicit maintenance.

This prevents an empty or half-created database from appearing as a real session.

### Fork

Unify materialized and sparse forks after flushing the source:

- create a private destination staging directory
- acquire destination ownership
- use SQLite `ATTACH` or backup APIs to copy the exact history prefix, required objects,
  transcript projection, and rewindable side tables in one destination transaction
- assign a fresh ID, `parent_id`, revision baseline, and owner token
- do not copy writer metadata or request audits
- copy only referenced compatibility blobs while external blobs remain
- validate logical invariants and `quick_check`
- generate derived files
- atomically publish the destination directory
- remove staging on handled failure and leave it recognizable after a crash

The current whole-directory blob copy can copy unreferenced or post-boundary private data
into a fork and should be removed.

### Delete

- Resolve and validate the exact target ID.
- Refuse deletion while another process holds its session lock.
- Flush and close all local handles if deleting a locally owned session through a future
  explicit flow.
- Atomically rename the directory into a private `.trash` directory under the same
  filesystem.
- Remove the renamed tree after publication, synchronously or by bounded cleanup.
- Return and display all errors.

Rename-first deletion prevents half-deleted session directories from appearing in normal
listing and gives crash cleanup a clear target.

## P1: Put attachments in the existing object store

### Problem

Attachments are currently written before the SQLite commit. Failed commits leave orphan
files. Existing files are trusted without verification. One helper ignores directory and
write failures entirely at `crates/buffer/src/attachment.rs:130`. Missing or unreadable
blobs are silently skipped on load. Fork copies all blobs.

### Change

Use `objects` and `history_object_refs` for new attachments:

- normalize an attachment data URL into a content-addressed object inside the same session
  transaction as the history row
- retain the original data URL bytes initially for a low-risk migration
- store an internal object reference in normalized history and rehydrate it for runtime
  protocol values
- use an explicit reference role such as `attachment_image`
- verify object hashes during hydration
- enforce a maximum attachment and object size before allocation
- make missing objects a degraded-session warning and a request-time error, not a silently
  missing image
- let fork's existing referenced-object copy carry only reachable attachments
- let rewind remove references and later object GC reclaim payloads

Do not optimize by decoding base64 into a new attachment schema until measurements show
that the simpler exact-data-URL representation is too large.

Compatibility plan:

- continue reading `blob:<filename>` references from old databases
- verify the compatibility filename and content hash where possible
- import legacy blobs into objects on an exclusive owned save or explicit migration
- mark compatibility with a `COMPAT(...)` entry and remove it after a defined support
  window
- stop writing new external blobs immediately after the new schema is active

## P1: Derived cache contract

### Problem

`meta.json` and `content.txt` can lag a successful database commit, be partially written,
or be regenerated by readers. Some write helpers ignore errors. `meta.json` does not
identify the canonical revision it represents.

### Change

- Keep both files derived and disposable.
- Generate them only after a successful canonical commit, fork publication preparation,
  or explicit maintenance.
- Add `source_revision` and a cache format version to `meta.json`.
- Generate `content.txt` in the same derived refresh pass.
- Make all cache writes atomic with a unique temporary file, `create_new`, file flush,
  rename, cleanup, and a returned `Result`.
- Use secure permissions.
- A cache failure does not roll back or mark the canonical commit failed.
- Record and expose cache refresh failure.
- Listing can show a valid cached summary immediately. If the cache is missing or invalid,
  read canonical SQLite without writing from the reader.
- Exact resume and provider dispatch never trust sidecars.
- Provide explicit `doctor --rebuild-derived` for bulk repair.

A tiny crash window can still leave a stale cache after a committed database transaction.
That is acceptable for a derived list preview. It must never affect resumed history or
model requests.

## P1: Snapshot-consistent grouped reads

### Problem

Header, metadata, history count, descriptor count, and descriptor tail can be loaded by
separate connections or statements while the background writer commits concurrently.
A read-only viewer can likewise observe mixed revisions across a multi-step operation.

### Change

Add operation-shaped store methods, not generic repositories:

- `read_resume_snapshot(tail_limit)` returns session state, `StoreHead`, and descriptor
  tail from one short deferred read transaction
- `read_session_summary()` returns all list fallback fields from one snapshot
- `read_history_range(range)` returns and verifies one bounded range
- `read_transcript_slice(range)` returns total and rows from one snapshot
- fork uses one attached-database snapshot

Never hold a read transaction across frames, user input, provider network calls, or worker
flushes. Long-lived read-only connections are acceptable; long-lived read snapshots are
not.

## P1: Rewindable side tables need one boundary model

### Problem

The latest compaction fix correctly validates side-table coordinates against the final
canonical history length, but it also makes an older semantic mismatch explicit:

- `metadata_snapshots` and `accounting_snapshots` are history-boundary values and allow an
  index equal to `history_len`
- `Session::finish_turn_state` records `turn_metas` at the completed boundary, including
  the boundary equal to `history_len`
- storage requires `turn_metas.turn_idx < history_len` because it has a foreign key to a
  history row
- serialization therefore omits the final-boundary turn meta and leaves it only in memory
  “until a subsequent history item” at `crates/core/src/session.rs:1294`

If the process exits before another history item, that final-boundary value is lost. After
resume and a later append, rewind metadata for that boundary cannot be recovered. This is
not canonical history corruption, but it violates the expected persistence semantics of a
rewindable side table.

### Change

Use one explicit history-boundary model for all rewindable side tables:

- valid keys are in `[0, history_len]`
- values at the final boundary are persisted immediately
- rewind to boundary `N` retains the latest value at or before `N`
- truncation to `N` deletes values strictly after `N`
- fork at boundary `N` copies values through `N`
- a schema migration removes the inappropriate row foreign key from `turn_metas` and
  validates the boundary against final session state in the commit invariant pass
- use a typed `HistoryBoundary` in the core/store DTO if needed to prevent row-index and
  boundary-index confusion; do not add a generic index hierarchy

Add an end-to-end test that finishes a turn at the current final boundary, saves, exits,
resumes, appends another turn, rewinds to the old boundary, and verifies the persisted
`TurnMeta` and context snapshot are restored.

## P1: Object integrity, limits, and garbage collection

### Change

- Verify each history row's stored hash against its normalized JSON when it is read.
- Verify stored `kind` against the decoded history variant.
- Keep object hash verification on hydration in release builds.
- Reject negative sizes and impossible codec metadata before allocation.
- Decode compressed objects through a bounded path so a corrupt `raw_size` cannot force
  unbounded allocation.
- Define a conservative maximum single-object size and return `ObjectTooLarge`.
- Treat `objects.kind` as advisory first-seen metadata; reference roles remain the
  semantic authority. Do not branch correctness logic on the singular object kind.
- Add an exclusive maintenance GC that deletes objects unreachable from both
  `history_object_refs` and `request_object_refs`, also checking direct request hash
  columns until their reference invariant is guaranteed.
- Run lightweight orphan deletion after large rewinds or explicit object-GC maintenance,
  not after every commit.
- Do not automatically prune request audit summaries.
- For full audit payload growth, provide stats and an explicit command to remove old full
  payload objects while preserving request summaries.

Deleting unreachable objects lets SQLite reuse pages. Physical file shrinking should
remain an explicit database-vacuum operation because `VACUUM` is expensive and requires
exclusive access. This is unrelated to context-window compaction.

## P2: Schema hardening and cleanup

### Constraints to add in the next planned schema rebuild

- nonnegative checks for indices, revisions, history lengths, sizes, costs where
  applicable, and timestamps where required
- boolean checks for integer booleans
- history-boundary checks for rewindable side tables; do not add a row foreign key to
  `turn_metas` because the valid final boundary has no corresponding history row
- explicit consistency checks for descriptor and search history links
- required indexes, FTS triggers, and foreign-key declarations in read-only schema-shape
  validation

Keep post-commit semantic validation even after adding SQL constraints. Some invariants,
such as dense indices and descriptor kind matching, are not cleanly expressible as row
constraints.

### Dead schema to remove after compatibility verification

Current workspace references indicate these are unused by production storage behavior:

- `history_items.model_visible_hash`
- `transcript_blocks.sidecar_hash`
- `turn_tool_elapsed`

Remove them in one versioned migration rather than carrying misleading schema. Do not
rename `accounting_snapshots` only for naming aesthetics; its current meaning can be
documented without a risky table rewrite.

### Schema validation

Current shape validation checks tables and columns but not all critical indexes and
triggers. Add cheap open-time validation for required schema objects. Keep expensive
checks for doctor or suspected corruption:

- `PRAGMA quick_check`
- `PRAGMA foreign_key_check`
- dense history and descriptor checks
- state length versus row count
- object reachability and payload hashes
- FTS integrity/rebuild checks

### Migration and repair discipline

- All migrations run under exclusive session ownership.
- Detect active ownership before changing a supported old schema.
- Make each migration transactional and test upgrade from every supported version.
- Record one-time data-repair IDs in `store_meta`.
- Run compatibility repairs once under the owner, not on every header read.
- A read-only viewer of a repair-needed session either applies a bounded in-memory
  tolerance or reports that the owner must reopen it with a newer version.
- Remove compatibility code according to `docs/compat.md` rather than allowing permanent
  repair-on-read behavior.

## P2: SQLite durability and WAL hygiene

### Change

- Change writable connections to `synchronous=FULL` unless a focused benchmark shows an
  unacceptable regression. Saves already occur on a background worker, and FULL gives
  stronger power-loss durability than NORMAL.
- Set an explicit `journal_size_limit`, initially in the 16 to 32 MiB range.
- Keep WAL auto-checkpointing enabled with an explicit documented threshold.
- After all local readers are dropped on session release or shutdown, best-effort
  `wal_checkpoint(TRUNCATE)` before closing the writer.
- Treat a busy checkpoint as non-fatal and leave the WAL valid for later cleanup.
- Never checkpoint or vacuum on every save.
- Run `PRAGMA optimize` only at an appropriate exclusive close or maintenance boundary.
- Expose database, WAL, object, history, descriptor, and request-audit sizes in storage
  diagnostics.

Observed large WAL files had fully checkpointed frames and truncated successfully. This
is a disk-hygiene problem, not evidence of canonical corruption.

## P2: Doctor, backup, export, and recovery

Add a concrete command surface such as:

```text
smelt session doctor <id>
smelt session doctor --all
smelt session vacuum <id>
smelt session rebuild-derived <id>
smelt session backup <id> <path>
```

Doctor should report without mutating by default. `--repair` must acquire exclusive
maintenance ownership and list every action before applying it.

A consistent backup should use SQLite's backup API or `VACUUM INTO`, not copy
`session.db` while a WAL may contain committed frames. Once attachments are objects, a
single backed-up database plus a small manifest is a complete portable session. Until
then, backup must include verified referenced blobs from the same source snapshot.

Existing JSONL history and request exports should remain. Add a portable manifest only if
users need round-trip import; do not invent another canonical session format for normal
runtime persistence.

On corruption:

- keep the session visible as unavailable
- preserve the original files
- allow read-only JSONL salvage of independently decodable rows
- write repaired output to a new staged session, never destructively rewrite the only
  copy during salvage

## P2: Queue bounds and long-running scale

The current unbounded worker channel is unlikely to fail under normal request rates, but
full request audits can contain large payloads and a locked disk can allow memory growth.
After ownership and retry behavior are fixed:

- instrument queue command count and estimated payload bytes
- keep session commits lossless and prioritized
- place a bounded byte budget on pending full audit payloads
- when the optional audit budget is exceeded, retain a summary and report that full
  payload capture was skipped rather than blocking the TUI indefinitely
- flush all accepted audits before clean release

Do not split normal session writes across multiple writer threads. One FIFO writer remains
the simplest ordering guarantee.

## Concurrency model after the changes

### One CLI

- The TUI mutates memory without blocking on disk.
- One worker owns the SQLite writer, lock guard, and owner token.
- At most one session commit is in flight from the document state machine.
- Request audits and session commits are serialized FIFO.
- Reader connections may query concurrently under WAL.
- Read operations use short snapshots and cannot migrate or repair.

### Multiple CLIs, different sessions

- Each session has an independent database and lock.
- Each CLI can own a different session with no global writer bottleneck.
- Directory listing and sidecar reads remain concurrent.

### Multiple CLIs, same session

- The first exclusive lock holder is the only writer.
- Later CLIs open read-only and continue to see committed WAL snapshots.
- A read-only CLI cannot save, repair, delete, vacuum/GC, or append request audits.
- It may fork a consistent source snapshot into a newly owned destination.
- Writer crash releases the OS lock. The next writer replaces stale owner metadata in one
  transaction.
- A stale writer handle cannot mutate because it no longer has the lock/token pair.

### Unsupported case

- Two hosts writing or reading the same WAL database over a shared network filesystem are
  not supported.
- If cross-host collaborative access becomes a requirement, introduce a real storage
  service with one server-side SQLite owner. Do not stretch file locking or lease expiry
  into a distributed database protocol.

## Canonical invariants for the target system

### Filesystem

- Every session path is derived from a validated ID beneath the canonical sessions root.
- A published session directory contains a valid `session.db`.
- Staging and trash directories are never listed as sessions.
- Canonical state never depends on derived sidecars.
- New payload files are unnecessary after attachment object migration.

### Ownership

- At most one `OwnedSessionWriter` exists per session.
- It holds the OS lock for its lifetime.
- Every mutating transaction verifies its fencing token.
- Read-only handles expose no mutation methods.
- Maintenance requires the same exclusivity as runtime writing.

### Transaction

- Revision increases exactly once per successful session commit.
- `session_state.history_len == COUNT(history_items)`.
- History indices are dense in `[0, history_len)`.
- Descriptor indices are dense in `[0, descriptor_len)`.
- Descriptor history origins are null or point within history and match semantic kind.
- Rewindable side-table keys are canonical history boundaries in `[0, history_len]`.
- Context checkpoints point within canonical history, and synthetic summary items never
  appear in `history_items`.
- Engine and persistence deltas are expressed only in canonical coordinates.
- Every object reference points to an existing object.
- Every stored object hashes to its key after decoding.
- A commit receipt describes the exact committed head.
- Derived refresh never changes the commit result.

### Restore

- Resume reads one coherent header and transcript-tail snapshot.
- Sparse reads return the requested canonical range or a typed corruption error.
- Missing attachments produce explicit degraded state.
- Unsupported or corrupt sessions remain visible.

### Lifecycle

- Session switch and shutdown flush accepted work before release.
- Permanent save failure is visible and never reported as success.
- Create and fork publish atomically from staging.
- Delete first acquires exclusivity and renames to trash.

## Implementation sequence

### Phase 0: Lock in end-to-end failure tests

Before changing implementation, reproduce the current behavior through real application
boundaries:

- two processes simultaneously resume the same session
- provider history loading while a writer is committing
- repair-needed session while another CLI owns it
- lock contention shorter and longer than the retry budget
- delete with malicious and valid IDs
- crash and restart during create, fork, attachment write, canonical commit, derived
  refresh, and delete
- shutdown with a transient and permanent storage failure

### Phase 1: Remove immediate safety hazards

1. Validate IDs and confine filesystem access.
2. Make delete typed, exclusive, and error-returning.
3. Fix provider history to use read-only open.
4. Remove repair and migration from read paths.
5. Enforce secure permissions.
6. Preserve errors through exact load and list boundaries.

This phase is small and should land independently.

### Phase 2: Install the owned writer boundary

1. Add the per-session OS lock guard.
2. Add random database owner tokens.
3. Create `SessionReader`, `OwnedSessionWriter`, and `SessionMaintenance`.
4. Move writable open, migration, repair, session commit, and request audit behind the
   worker-owned handle.
5. Remove or restrict broad public mutation methods and raw connection access.
6. Add clean release on switch and shutdown.

### Phase 3: Make transaction and retry behavior exact

1. Replace manual transactions with RAII.
2. Add structured store and commit errors.
3. Return full `StoreHead` on stale bases.
4. Add bounded begin-transaction retry.
5. Discard poisoned connections.
6. Make all rewindable side tables persist final history-boundary values.
7. Replace the 64-iteration shutdown loop with structured completion.

### Phase 4: Make directory lifecycle atomic

1. Stage and publish new sessions.
2. Unify fork through staged prefix copy.
3. Rename-first delete into trash.
4. Make derived writes atomic and revisioned.
5. Add cleanup for abandoned staging, trash, and temporary cache files.

### Phase 5: Integrate attachments and object maintenance

1. Store new attachments as objects in the session transaction.
2. Add compatibility reads/import for old blobs.
3. Add missing-object degraded-state behavior.
4. Add bounded hydration and object size limits.
5. Add unreachable-object GC and explicit database vacuum.

### Phase 6: Schema and operational hardening

1. Remove verified dead columns/table in one migration.
2. Add SQL checks and stronger schema-object validation.
3. Add WAL size and close-time checkpoint policy.
4. Add doctor, backup, and rebuild-derived commands.
5. Add queue and storage-size telemetry.

## Test plan

### Real multi-process tests

- Exactly one of two simultaneous claimers obtains write ownership.
- The loser resumes read-only without migrating or repairing.
- Two CLIs can own and write different sessions concurrently.
- A read-only CLI cannot commit, audit, repair, vacuum/GC, or delete.
- A read-only CLI can fork while the source writer continues committing, and the fork is a
  coherent snapshot.
- Killing the owner releases the OS lock and allows immediate reclaim.
- PID reuse and stale serialized owner metadata do not grant ownership.
- A stale handle fails its owner-token check after ownership changes.

### Transaction tests

- Failure at each statement in session commit rolls back every table.
- Failure during `COMMIT` does not leave the cached connection in a transaction.
- Rollback failure discards the connection.
- Busy before mutation retries within budget.
- Busy beyond budget returns typed `Busy` and leaves the document dirty.
- Integrity and ownership failures are never blindly retried.
- A stale command returns all current head coordinates and replans once.

### Restore and read tests

- Provider history fetch opens query-only and never migrates or creates files.
- Resume header and descriptor tail come from one revision.
- Corrupt JSON, hash mismatch, missing object, unsupported schema, and malformed schema
  each produce distinct errors.
- One corrupt session remains visible while healthy sessions still list.
- Ordinary reads do not modify database, WAL, sidecars, or directory timestamps.
- Long-lived reader connections do not hold a read transaction across commits.

### Canonical history and rewind tests

- Pre-request and context-limit compaction preserve every canonical history row through
  append, save, shutdown, and sparse resume.
- Synthetic summaries never appear in `history_items` or shift transcript origins.
- Engine append, replacement, cancellation, and note deltas map projected model indices to
  the expected canonical boundary.
- Rewind below, at, and above a checkpoint updates canonical history and checkpoint state
  without leaving side-table keys past the final boundary.
- A `TurnMeta` recorded exactly at final `history_len` survives save/resume and is restored
  by a later rewind to that boundary.

### Attachment and object tests

- Crash before, during, and after attachment object insertion leaves no committed dangling
  history reference.
- Rewind plus GC removes unreachable attachment and metadata objects but preserves request
  objects.
- Fork copies only objects reachable from the selected prefix.
- Corrupt compressed size and excessive raw size return errors without large allocation.
- Legacy blobs import once and remain readable through the compatibility window.

### Filesystem lifecycle tests

- New session is invisible until the first valid database is published.
- Every fork failure point leaves either no published fork or a fully valid fork.
- Delete refuses an active owner and never follows a symlink or escapes the root.
- Crash after trash rename does not make the session reappear.
- Derived write failure leaves canonical SQLite valid and reports degraded cache state.
- State and payload file modes are private on Unix.

### Migration and doctor tests

- Upgrade fixtures from every supported schema version.
- Active older owner prevents migration by a second CLI.
- Unsupported newer schema is read-only unavailable, not modified.
- Every compatibility repair is idempotent and recorded.
- Doctor detects missing triggers, foreign-key violations, dense-index gaps, hash mismatch,
  orphan objects, and FTS inconsistency.
- Repair writes a staged replacement or runs under exclusive ownership as appropriate.

### Model and scale tests

- Property-test random sequences of append, metadata update, descriptor update,
  checkpoint compaction, canonical delta, rewind, save failure, stale receipt, retry,
  resume, and fork against an in-memory reference model.
- Benchmark sparse resume with very large history and descriptor counts.
- Benchmark listing and search with thousands of session sidecars.
- Benchmark FULL versus NORMAL synchronous mode on the background save path before making
  a performance exception.
- Exercise large request-audit histories and queue backpressure.

## Alternatives considered

### Minimal hardening only

Do only:

- read-only provider open
- remove repair-on-read
- transactional lease claim
- structured busy error and bounded retry
- safe delete validation

This is low churn and removes immediate bugs, but it leaves mutation authority spread
across APIs and keeps attachment, lifecycle, and shutdown gaps. It is acceptable as an
initial phase, not the desired final architecture.

### Recommended focused refactor

Implement the concrete reader/owned-writer/maintenance split, OS lock plus owner token,
RAII transactions, structured errors, staged lifecycle, and attachment objects. This
removes the known failure classes without changing the canonical session model or adding
hypothetical abstraction.

This is the recommended option.

### Clean-slate rewrite

A clean-slate storage design would still use:

- one SQLite database per session
- one lifetime writer
- read-only WAL viewers
- semantic history rows rather than an event replay log
- content-addressed objects for all large payloads
- transactionally maintained transcript/search projections
- staged directory publication

It might separate transcript descriptors and search projection into cleaner tables and
remove legacy DTO duplication from day one. It would not use event sourcing or a global
multi-session write database.

A clean-slate rewrite is not recommended. The current schema and `SessionCommit` already
contain the difficult domain logic, and replacing them would create migration and
correctness risk without addressing ownership or lifecycle automatically.

## Explicit non-goals

Do not add:

- a generic repository trait
- an async storage trait for hypothetical remote backends
- a global database that owns all sessions
- an event log plus snapshot replay architecture
- automatic same-session multi-writer merge
- lock expiry as a distributed consensus mechanism
- a second canonical JSON or recovery-journal representation
- automatic destructive repair during read
- automatic request-audit deletion
- vacuum or full integrity checks on every save

## Evidence behind the priorities

The audited local state contained 548 session databases: 253 schema v1 and 295 schema v2.
A lightweight audit found no current session-state/history-count mismatches and no open
failures. Focused store tests passed 72 of 72. After the latest compaction fix, focused
TUI persistence tests passed 23 of 23, including canonical-history preservation through
compaction, append, save, and resume.

Historical logs nevertheless contain real failures from the pre-spine architecture:

- foreign-key failure
- unchanged-prefix/history-row divergence
- metadata-only history-length divergence
- checkpoint past retained history
- transcript descriptor suffix past dense end
- request audit `database is locked`

The new atomic commit directly addresses the first five logical divergence classes. The
latest canonical-coordinate change additionally fixes projection/canonical confusion
during context compaction. The remaining proposal focuses on the parts those changes
cannot solve: process ownership, read/write discipline, filesystem operations, error
propagation, final-boundary side-table persistence, attachment atomicity, and operational
recovery.

## Final recommendation

Land the work in the implementation phases above, with Phases 1 through 5 considered the
robust-storage target. Phase 6 can follow without blocking the ownership and attachment
fixes.

The key architectural rule should be easy to state and enforce:

> A session has one canonical SQLite database, one lifetime owner for all mutations, and
> any number of strictly read-only viewers. Every canonical mutation is fenced and
> transactional; every filesystem artifact outside SQLite is either staged lifecycle
> metadata or a disposable derived cache.

That rule preserves the current design's strongest properties while removing the
remaining high-risk ambiguity.