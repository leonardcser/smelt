# Session lifecycle architecture plan

Status: Implementation complete; Stages 1 through 8 validated

## Brief

Make the complete session lifecycle feel immediate without weakening durability or
hiding linear work behind another abstraction. The common paths are startup,
resume, send, search, rewind, fork, session switching, metadata mutation, delete,
and graceful exit.

The recommended end state is:

1. Keep SQLite, WAL, `synchronous = FULL`, durable commit receipts, bounded resume,
   content-addressed objects, sparse transcript projection, and byte-bounded caches.
2. Evaluate configuration and plugins once per process launch and transfer that
   runtime into the TUI instead of constructing a second launch generation.
3. Store each original session and all of its forks in one isolated SQLite
   **lineage database**.
4. Represent canonical history and transcript state with immutable, structurally
   shared sequence roots. A user-visible session is a lightweight head pointing at
   one immutable revision.
5. Make transcript search a disposable, versioned, per-lineage projection. Do not
   duplicate full searchable text and do not maintain a full-detail trigram index
   in every branch.
6. Search a small live or unindexed suffix directly, search ready immutable
   segments through a compact candidate index, and verify every result exactly
   against canonical transcript data.
7. Keep all lifecycle work proportional to the changed suffix, requested viewport,
   query candidates, or persistent-tree height. No common operation may
   materialize, copy, rewrite, or rescan a complete growing session.

This is a greenfield storage design. smelt recognizes only canonical lineage
storage and does not read, import, migrate, or mutate pre-lineage session
formats. There is one canonical write path and one canonical read model.

## Why this plan exists

Recent end-to-end measurements show that several existing foundations are already
strong, while three architectural defects dominate the remaining lifecycle:

- Startup evaluates two Lua/config/plugin generations before the first usable
  frame. Session size is not the dominant startup cost.
- Fork materializes the complete source and rewrites the complete destination.
  A realistic fork took 56.8 seconds and peaked at about 5.4 GiB RSS.
- Search queries are usually fast, but the full-detail FTS representation can
  consume most of a text-heavy database. On one realistic session, search used
  about 1.24 GB, 83 percent of the complete database and 3.12 times the logical
  searchable text. An absent two-character query still took about 292 ms.

The same review also found paths that should not be rewritten without new
contrary evidence:

- Durable Enter is already suffix-bounded and completes in about 30 to 45 ms on
  the reproduced large sessions.
- Ordinary rewind is about 13 ms.
- Current-schema resume loads a bounded tail and viewport instead of the complete
  session.
- Graceful shutdown is already tens of milliseconds.
- SQLite transaction durability, writer fencing, ambiguous-receipt recovery,
  content-addressed objects, and sparse transcript hydration solve real
  correctness and memory requirements.

The goal is therefore not a universal rewrite. It is to replace the data ownership
that makes startup duplicate work, forks copy prefixes, and search indexes belong
to individual mutable branches.

## Decision principles

1. **Fix algorithmic shape before tuning constants.** A faster full copy is still
   the wrong fork algorithm. A lazily built 1.24 GB index is still the wrong search
   representation.
2. **Preserve proven correctness boundaries.** Provider dispatch follows a durable
   canonical receipt. WAL remains the journal. Search and catalog projections may
   lag, but canonical state may not.
3. **Share immutable data, never mutable ownership.** Forks share revision roots,
   transcript segments, objects, and ready search segments. Each branch has its own
   identity and head.
4. **Keep bounded fallbacks correct.** A missing search segment may make results
   progressive, but may never produce a false claim that no result exists.
5. **Use one concrete implementation.** Do not introduce generic repository,
   database-backend, event-bus, persistent-collection, or search-provider layers.
6. **Keep interaction work cancelable and latest-value.** Search typing and derived
   index builds must not queue unbounded obsolete work.
7. **Measure p95 at the user boundary.** Means and store microbenchmarks are useful
   diagnostics, not acceptance evidence.
8. **Treat storage and memory as first-class performance dimensions.** An index is
   not acceptable merely because its warm query time is low.
9. **Delete superseded mechanisms.** A cutover removes the old FTS text copy,
   branch-copy fork path, duplicate Lua launch generation, and obsolete tests and
   metrics.
10. **Prefer a simpler terminal architecture over a smaller patch.** Development
    cost is not a reason to retain a weaker data model.

## Measured baseline

These values are investigation baselines, not promises made by the proposed
architecture. Release measurements must be rerun on the implementation machine
before each phase so hardware and build changes do not masquerade as improvement.

| User boundary | Current evidence |
|---|---:|
| `--version` / `--help` | about 8 ms |
| Empty launch to first frame | about 152 ms, later identified as a non-responsive PTY artifact |
| Current-schema launch to resumed session | about 180 to 194 ms, later identified as a non-responsive PTY artifact |
| Resume-specific storage increment | about 28 to 42 ms |
| Object-heavy Enter | 30.216 ms mean |
| Deep-context Enter | 44.702 ms mean |
| Ordinary rewind | 12.769 ms mean |
| Large fork to visible destination | 56,800.878 ms |
| Large fork peak RSS | 5,682,128 KiB |
| Graceful exit | about 27 to 36 ms |
| Common three-character indexed search | 4.877 ms warm mean |
| Common eight-character indexed search | 18.972 ms warm mean |
| Absent two-character search | 292.294 ms warm mean |

Search storage evidence:

| Workload | Searchable text | Search storage | Amplification | Share of database |
|---|---:|---:|---:|---:|
| Large mixed session | 15.3 MB | 55.2 MB | 3.62x | 33.0% |
| Deep text-heavy session | 397.6 MB | 1,240.0 MB | 3.12x | 83.0% |

A prototype contentless FTS5 trigram index using `detail=none` and
`columnsize=0` used 25.3 MB for 398.1 MB of searchable text, or 6.36 percent.
That result proves compact candidate indexes are possible. It does not by itself
prove the final query design: without positions, a multi-word query produced 1,858
candidate blocks for six exact matches and required 117.9 ms to verify the first
match in one whole-block experiment.

## Scope and non-goals

### In scope

- Process launch and first frame.
- Launch directly into a requested session.
- In-process resume and session switching.
- Sending a turn through durable commit and dispatch.
- Live search, submitted search, and `n` / `N` navigation.
- Rewind and branch creation.
- Fork, delete, rename, metadata changes, and import/export.
- Graceful and persistence-blocked exit.
- Canonical storage growth, derived search growth, peak RSS, and write
  amplification for those operations.
- A strict format boundary that ignores pre-lineage session storage.

### Out of scope

- Weakening `synchronous = FULL` or dispatching before a durable receipt.
- Replacing SQLite with a custom journal or distributed database.
- Rewriting transcript rendering, exact layout, or sparse hydration without a
  separate reproduced defect.
- A cloud synchronization protocol.
- Cross-machine concurrent writers.
- Semantic, fuzzy, or embedding search. This plan preserves literal,
  display-aware search.
- Guaranteeing that arbitrary provider context construction is sublinear in the
  bytes actually sent to a provider.

## Architecture alternatives

### Option A: Optimize the current per-session copy model

Stream the source during fork, copy objects without hydration, and avoid rewriting
unaffected rows.

This could reduce the 56-second failure substantially, but physical fork cost and
storage would still grow with the shared prefix. Search indexes would still be
copied or rebuilt per branch. It does not meet the scalability requirement.

### Option B: Keep one database per session and reference parent databases

A fork could store a parent session ID, revision, and local suffix. Common forks
would be immediate.

This creates cross-database canonical dependencies. Deleting, moving, exporting,
repairing, backing up, or evolving one session would require a dependency graph
across directories. A deep fork chain would also require path flattening or many
open databases. The fast path is attractive, but lifecycle and recovery become
more complicated than the operation being optimized.

### Option C: Put every session in one global canonical database

A global revision DAG makes fork and sharing straightforward. It also makes one
corruption, schema fault, writer lock, disk-full event, or maintenance operation a
root-wide failure domain. It discards the useful isolation of lineage storage.
The existing rebuildable global catalog is useful; a global canonical write domain
is not required.

### Option D: Use one canonical database per fork lineage

An original session creates a lineage. Every fork of any branch remains in that
lineage database. Branches share immutable roots and objects transactionally,
while unrelated lineages retain independent files, leases, backups, and failure
domains.

This is the recommended option. It provides the sharing properties of a global
revision graph without making unrelated sessions one canonical write domain.
Deletion is local: removing a branch removes one head, and the lineage is removed
only after its last branch is deleted.

### Option E: Rewrite storage from scratch

A custom append log, content-addressed page store, or self-indexed text format could
make sharing intrinsic. It would also require new transaction, crash recovery,
checksumming, locking, compaction, schema evolution, query, and inspection
machinery that SQLite already provides. No measured defect requires replacing the
SQLite substrate.

The from-scratch idea worth retaining is the immutable revision model, not a
custom durability engine.

## Canonical lineage model

### Physical layout

The conceptual layout is:

```text
<sessions-root>/
  catalog.db                         # disposable global listing projection
  .locks/<lineage-id>.lock           # stable canonical writer lease
  lineages/<lineage-id>/
    lineage.db                        # canonical branches, revisions, payloads
    search.db                         # disposable versioned search projection
```

Names may change, but canonical and derived ownership may not:

- `lineage.db` is sufficient to reconstruct every surviving branch in the lineage.
- `search.db` can be deleted while the application is stopped. The next search
  remains correct through direct/progressive scanning and background rebuild.
- `catalog.db` can be deleted. Lineage headers rebuild it.
- No branch owns a private copy of shared history, transcript, objects, or search
  postings.

A separate `search.db` is intentional. FTS rebuilds, corruption recovery, free-page
behavior, and background writes must not bloat or contend with the canonical
transaction file. It is a cache with an explicit format version, not a compatibility
export or second source of truth.

### Branches and revisions

A user-visible session is a branch row with immutable identity and one mutable head:

```text
SessionBranch
  session_id
  lineage_id
  fork_parent_session_id
  created_at
  head_revision_id
  lifecycle metadata
```

A canonical mutation creates one revision and atomically advances one branch head:

```text
Revision
  revision_id
  parent_revision_id
  history_root_id
  transcript_root_id
  small canonical state needed for exact recovery
  commit fingerprint and receipt coordinates
```

Branch identity is not rewound. Model history, transcript, and small canonical
state whose semantics require revision consistency are represented by the revision.
Mutable presentation and runtime-selection metadata deliberately independent of
rewind remains on the branch.

The field boundary is fixed as follows:

| Logical field | Owner | Rewind and fork semantics |
|---|---|---|
| `session_id`, `lineage_id` | Immutable branch identity | Never rewind. Fork allocates a new session ID in the same lineage. |
| `fork_parent_session_id` | Immutable branch identity | Never rewind or retarget. It records the source branch captured by fork. |
| `created_at` | Immutable branch identity | Never rewind. Fork gets its own creation time. |
| `head_revision_id` | Mutable branch pointer | Rewind moves it. Fork initializes it to the captured source revision. |
| `updated_at` | Mutable branch lifecycle metadata | Never rewind. It records the latest branch mutation or head move. |
| `cwd` | Mutable branch workspace metadata | Never rewind. Fork copies the current value, then each branch changes independently. |
| `mode`, `reasoning_effort`, `model`, `fast_mode` | Mutable branch runtime selections | Never rewind. Fork copies current selections. These select future turns rather than describe a historical prefix. |
| `session_cost_usd`, cumulative `session_usage` | Cumulative branch accounting | Never rewind. Fork copies the spent baseline, then each branch accumulates independently. Reclamation cannot reduce it. |
| title, slug, first user message, and their history-keyed snapshots | Immutable revision state | Rewind restores the latest snapshot at or before the retained history boundary. Fork shares the captured value and snapshot prefix. |
| history root | Immutable revision state | Rewind selects or cuts a root. Fork shares it. |
| transcript root and canonical transcript metadata | Immutable revision state | Move with the history-consistent revision. Fork shares them. |
| checkpoint | Immutable revision state | Rewind restores or clears it according to the retained history boundary. |
| provider and display context-token baselines, history lengths, and identities | Immutable revision state | Rewind restores the matching context snapshot or clears a stale baseline. |
| turn metadata snapshots and context snapshots | Immutable revision state | Rewind truncates to the retained history boundary. Fork shares the captured prefix. |
| persistence generation, dirty ranges, actor state, caches, hydration windows, search readiness, and viewport | Derived process or projection state | Never canonical revision payload. Recompute or restore through their dedicated lifecycle state. |

This preserves current user-visible semantics rather than making runtime selections
or already-spent usage unexpectedly travel backward. A revision stores scalar state
directly when it is small and references immutable roots for growing sequences.
Derived fields must not be added to revisions merely because the current in-memory
`Session` type contains them.

Fork inserts a new branch pointing at the captured revision and copies only the
small branch-local values listed above. Rewind points the current branch at a prior
or newly cut revision. Neither operation rewrites shared payload rows.

### Immutable sequence roots

History and transcript use concrete immutable persistent sequences stored in
SQLite. The required properties are:

- Leaves contain bounded item counts and bounded referenced bytes.
- Internal nodes contain child IDs and cumulative item/byte extents.
- Nodes are immutable after publication.
- Append path-copies only the right spine.
- Seeking a history index, transcript record, or tail is logarithmic in item count.
- Splitting at a rewind boundary path-copies only the search path.
- Fork copies root IDs, not nodes or payloads.
- Payloads and large objects remain content-addressed and shared within the
  lineage.

A 32- or 64-way tree is a likely implementation, but fanout and leaf bounds must be
selected from write-amplification and cache measurements. Do not build a generic
persistent collection crate. Implement the two concrete sequence uses over one
small node representation only if that actually removes duplicate invariants.

Revision rows make canonical commits and ambiguous receipt recovery auditable.
Sequence roots make reads independent of revision-chain depth. A reader must never
walk every historical commit to reconstruct the current tail.

### Object ownership

Objects remain content-addressed blobs with typed references. Moving forks into one
lineage naturally deduplicates all inherited objects without introducing a global
object store or cross-database reference counts.

Unrelated lineages do not deduplicate objects. That trade-off preserves independent
backup, deletion, corruption, and ownership domains and avoids a second global
canonical store.

### Reachability and reclamation

Canonical writes are append-oriented. Rewind, failed experiments, and deleted
branches can leave unreachable nodes and objects. Reclamation is never on an
interaction barrier:

1. Mark roots from all live branch heads, every immutable branch-creation
   revision, and any explicitly retained recovery or export revision.
2. Traverse parent revisions, sequence roots and nodes, payloads, direct objects,
   and nested objects from those roots.
3. Delete unreachable receipts, transitions, turns, revisions, sequence rows,
   payload references, and objects in dependency order and row-bounded
   transactions. Branch-lifetime request audit references remain object roots
   while they exist.
4. Let SQLite reuse free pages immediately.
5. Run incremental vacuum only as bounded maintenance, never during Enter, fork,
   rewind, resume, branch switch, or exit.
6. Remove a lineage directory only after its last branch is durably deleted and
   the directory is renamed to trash under the stable lineage lease.

A deleted source branch cannot invalidate a surviving fork because both heads
already root their own reachable views in the same lineage. A branch tombstone
retains its immutable creation revision and branch-local request audit until the
lineage is retired. This preserves fork identity, durable receipt interpretation,
and inspection evidence without making those rows interaction-path GC work. The
mutable head is cleared immediately, so revisions created after that branch origin
can still be reclaimed when no live head or explicit retention reaches them.

## Ownership and concurrency

### Lineage actor

Replace per-session canonical ownership with one concrete writer actor per open
lineage. It owns:

- The stable lineage lease and owner token.
- The canonical SQLite writer and current known branch heads.
- Canonical commit ordering and receipt reconciliation.
- Branch creation, head movement, deletion, and publication.
- Priority over every derived maintenance request.

The active `ConversationRuntime` still owns one branch's in-memory projection,
change generation, prompt, turn lifecycle, and transcript state. It sends concrete
branch commands to the lineage actor.

Canonical commands are prioritized over garbage collection and derived status
installation. No large search-index build runs on the lineage actor.

### Search projector

A bounded per-lineage projector:

1. Reads immutable canonical ranges through a read snapshot.
2. Builds one small search segment outside the canonical actor.
3. Installs that segment atomically in `search.db`.
4. Publishes readiness keyed by bounded groups of immutable canonical leaf IDs and
   the search format version.
5. Uses a latest-value or deduplicated work set, not an unbounded FIFO.

If an active writer exists, it is the only process allowed to schedule projection
mutation. Read-only processes may query ready projection data but do not race to
rebuild it.

Canonical commits do not wait for search projection. Search build failure does not
change commit success, dirty state, or provider dispatch.

### Catalog projector

Retain the rebuildable root catalog, but make a catalog row map a session ID to a
lineage and branch. Catalog updates remain post-receipt, bounded, and coalescing.
Exact open always validates identity and head against `lineage.db`.

## Lifecycle flows

### Startup

Before Stage 1, startup constructed and evaluated one Lua/config/plugin runtime in
`main`, then constructed and evaluated another launch candidate in the TUI. Stage 1
implements this flow:

```text
process entry
  -> parse static bootstrap options
  -> create one runtime environment and generation-zero VM
  -> evaluate bundled, user, and project early Lua
  -> register and parse dynamic flags with that same VM
  -> resolve a provisional interactive runtime
  -> construct the TUI around generation zero
  -> claim the terminal and lend the live frontend host
  -> evaluate full bootstrap, config, and plugins exactly once
  -> resolve final runtime and authentication state
  -> first frame
```

Requirements:

- `--version` and other fully static commands bypass Lua when their output cannot
  depend on plugins.
- Dynamic plugin flags continue to work. This is why the generation-zero runtime is
  transferred, not discarded after parsing.
- TUI construction accepts that same launch generation and does not call a second
  `bring_up_lua("launch", true)`.
- Full UI bootstrap and normal configuration run only while a live frontend host is
  available. Headless startup explicitly loads the full bootstrap without a TUI.
- Startup spans begin before runtime/config work so fixed bootstrap cost is
  attributable.
- Session resume is not made asynchronous merely to hide duplicate startup work.
  First remove the duplicate generation, then remeasure.
- A first frame may precede complete session hydration only if it is truthful and
  interactive. It may not display a shell that immediately blocks on the same
  synchronous work.

#### Stage 1 implementation evidence

Generation-zero startup now has a dedicated launch owner in both core and TUI code.
It shares the VM and synchronized registries with the runtime installed in the app,
but owns mutable load bookkeeping until launch completes. It intentionally has no
candidate commit handles: launch performs live startup effects, while `/reload`
retains its separate transactional candidate path. The ordered full bootstrap is
deferred until `TuiApp` and its scoped frontend host exist. Headless startup loads
that bootstrap explicitly.

Correctness evidence:

- The generation harness asserts that launch remains generation zero, every
  bootstrap, early, autoload, config, plugin, and project phase executes once, and a
  subsequent reload commits generation one with each reload phase executing once.
- A core regression test proves runtime construction evaluates no deferred
  bootstrap chunk and an explicit load evaluates it once.
- PTY tests prove a plugin-declared dynamic flag affects the first frame, a failing
  config is evaluated and reported once, and graceful exit invokes the shutdown
  hook once with final ephemeral and message state.
- Harness tests cover command-line versus Lua vim precedence and filter explicit
  mode cycles against the final Lua-registered catalog.
- The headless `plain_turn` scenario passes through the explicit full-bootstrap
  path.

The original 152 ms empty and 180 to 194 ms resumed measurements were not product
latency. The controlling-PTY benchmark did not answer smelt's OSC 11 background
query and DSR fence, so background detection deliberately waited for its 100 ms
no-response fallback. The permanent harness now emulates both terminal responses
and recognizes a probe even when it is split across PTY reads. Process spawn remains
inside the measured launch-to-frame boundary.

An instrumented distribution run reached the internal first-frame marker in about
48.4 ms. Its largest span was `startup:lua_launch` at about 22.1 ms, while the first
render took about 0.738 ms. A separate direct PTY observed the frame at 52.245 ms
from process launch. Corrected 20-run end-to-end results are:

| Workload | First-frame mean | First-frame p95 | Visible-ready p95 | Exit p95 | Peak RSS p95 |
|---|---:|---:|---:|---:|---:|
| Empty startup | 52.244 ms | 54.501 ms | 54.552 ms | 10.086 ms | 54,896 KiB |
| Current-schema resume | 52.988 ms | 55.984 ms | 56.021 ms | 16.798 ms | 70,376 KiB |

The resume run used a copied 160 MiB current-schema database with 1,934 history
rows, 4,416 transcript blocks, and 575 objects. Final Stage 1 validation passed
formatting, warnings-denied workspace Clippy, release-fast and distribution builds,
the five-test startup PTY suite, 79 focused TUI launch/reload tests and their matching
storybook regression, the core deferred-bootstrap regression, the headless
`plain_turn` scenario, and `git diff --check`. Stage 1 therefore meets the 100 ms
first-frame p95 gate for both empty launch and current-schema resume, with no
remaining startup blocker.

### Resume and session switching

```text
catalog lookup
  -> open and validate lineage + branch head
  -> read bounded history tail from history root
  -> read bounded transcript records around the saved viewport/tail
  -> install branch projection
  -> render usable session
  -> schedule optional prefetch and missing search projection
```

Requirements:

- Work is proportional to configured tail, viewport, and tree height, not lineage
  bytes, total branches, or history length.
- Scalar projections are used for counts, current context, model, title, and other
  header fields.
- No search index is built before the session is usable.
- A missing `search.db` does not delay resume.
- In-process switching closes or parks the prior branch through the same
  generation-targeted flush protocol. It does not materialize its complete state.
- Open validates the lineage schema without scanning or converting another
  storage format.

### Send

Preserve the proven durable barrier:

```text
Enter
  -> prepare changed canonical suffix
  -> one lineage.db transaction:
       payload and object references
       new immutable sequence nodes
       new revision
       branch head
       durable turn state
       commit fingerprint and receipt
  -> validate durable receipt
  -> dispatch provider request
  -> project catalog and search asynchronously
```

Requirements:

- One canonical transaction is attributable to the submitted turn.
- Provider dispatch never precedes the receipt.
- Node and row writes are proportional to changed payload plus persistent-tree
  height.
- Search text, FTS postings, catalog writes, checkpointing, vacuum, and garbage
  collection are outside the Enter barrier.
- Exact ambiguous-outcome replay returns the original receipt and revision.
- The existing append-only context semantics and bounded history-tail behavior are
  retained.

### Search

Search has two independent concerns:

1. The UI must stay responsive while the query changes.
2. Persistent candidate data must stay compact and share with forks.

The interaction flow is:

```text
query edit
  -> increment search generation
  -> scan current viewport/live suffix immediately
  -> replace pending persistent query with newest generation
  -> query ready immutable segments nearest the origin
  -> verify candidate text exactly against canonical records
  -> publish first current-generation result
  -> continue progressive pages while useful
```

UI requirements:

- Search begins as the query changes, not only after Enter.
- Empty queries cancel work and clear preview state.
- Every query has a generation. Stale worker results are discarded.
- Only one pending query generation is retained per search input.
- The current viewport and dirty in-memory suffix are searched synchronously within
  a frame budget; persistent work runs off the event handler.
- Live results preview the nearest match from the original search anchor. Esc
  restores the anchor and prior committed search. Enter confirms the preview.
- Results may become more complete while typing, but an older generation may never
  move the viewport.
- `n` and `N` reuse cached verified results and continue the same candidate cursor.
- The UI distinguishes `searching` from a proven `no matches`. Absence is shown
  only after every reachable ready segment and unindexed range has been checked.

Literal semantics remain authoritative in Rust:

- Matching is exact and display-aware.
- Newline-containing queries remain unsupported unless row-break semantics are
  deliberately implemented.
- Candidate indexes may return false positives, never false negatives.
- Exact verification happens before highlight, reveal, or absence.
- Unicode, punctuation, case, omission caps, hidden detail, and tool-output behavior
  are covered by differential tests against the direct scanner.

### Rewind

A normal rewind is a persistent-sequence split and branch-head update:

```text
resolve semantic target
  -> derive history/transcript roots at target boundary
  -> insert one revision
  -> atomically advance branch head
  -> install bounded in-memory projection
```

It does not delete or rewrite the old suffix synchronously. Unreachable data is
reclaimed later if no fork or retained head references it. Search immediately uses
only segments reachable from the new transcript root.

If the target lies inside a leaf, only the path and boundary leaf are copied. Large
payload objects remain shared.

### Fork

The common fork flow is:

```text
freeze source branch mutation
  -> flush captured source generation
  -> capture source revision ID
  -> insert destination branch pointing at that revision
  -> durable branch-creation receipt
  -> update catalog projection
  -> switch to destination branch
```

The operation writes branch identity and small metadata only. It reads no inherited
payload and builds no search index. The destination immediately shares canonical
roots, objects, and ready search segments.

Fork is idempotent under ambiguous receipt delivery. A source mutation after the
captured revision belongs only to the source. The destination's immutable
`fork_parent_session_id` records user-visible ancestry while the lineage root
provides physical sharing.

Forking unsaved work after canonical storage has become unavailable is an
exceptional recovery path. It may stream the readable canonical prefix plus live
suffix into a new lineage, because the original lineage cannot safely accept a new
head. This path must preserve data and report progress, but it is not allowed to
complicate or slow the normal O(1) fork.

### Graceful exit

```text
capture active persistence generation
  -> submit latest canonical intent
  -> wait for that generation's durable result
  -> drain already accepted required lifecycle acknowledgements
  -> stop provider/process resources
  -> cancel derived search/catalog work
  -> release lineage lease and terminal
```

Requirements:

- Exit uses the same generation-targeted flush and receipt path as other lifecycle
  transitions.
- Search projection, catalog catch-up, WAL checkpoint, vacuum, and garbage
  collection are not required for canonical success.
- A blocked canonical flush presents explicit retry, fork/save-as, or discard
  choices. It does not spin through a hidden retry policy.
- Canceling a search build leaves either a complete versioned segment or no segment.
- Process and terminal cleanup remains idempotent.

### Delete, rename, and metadata mutation

- Rename and ordinary metadata changes create one small revision or branch-row
  update according to the documented rewind boundary. They never rewrite history.
- Deleting one branch removes or tombstones one head and updates the catalog after
  commit.
- Shared nodes remain while any branch root reaches them.
- Deleting the final branch renames the complete lineage to trash under the stable
  lease, fsyncs the sessions root, then performs best-effort physical cleanup.
- Branch listing and deletion do not open or scan payload tables.

### Import and export

- Export reads one revision-pinned root and streams canonical items in order.
- Export memory is bounded by one sequence leaf, object chunk, and output buffer.
- Import uses the same canonical node/revision transaction primitives as runtime
  commits, in bounded batches.
- Import does not build search synchronously. It schedules immutable segments after
  canonical publication.
- A branch-only export contains no hidden dependency on its original lineage.

## Search storage architecture

### What changes

The current schema stores a full searchable-text copy and a full-detail trigram
index per session. That design is replaced. Search becomes derived per lineage:

- No full `transcript_search.indexed_text` copy.
- No branch-private FTS index.
- No search write in `SubmitTurn`.
- Ready index data is keyed to immutable canonical transcript nodes or sealed
  corpus segments.
- Forks reuse those keys automatically.
- Rewind changes the reachable segment set rather than deleting index rows.

The logical searchable-text policy, including any deliberate per-block cap, must be
specified independently of the index. Storage ratios use those logical searchable
UTF-8 bytes as the denominator.

### Segment shape

The transcript root is decomposed into:

- Immutable sealed corpus segments containing 2 MiB of logical searchable text.
- 32 KiB search documents that pack consecutive records; oversized records use 1 KiB
  record-local overlap.
- A small right-edge frontier and dirty in-memory suffix searched directly.

Stage 2 selected these sizes from the complete 1 to 8 MiB segment and 4 to 32 KiB
document matrices. The 2 MiB segment keeps projector work and peak build memory
small while retaining physical-storage headroom. Segment construction and query
memory are bounded by segment size, never total transcript size.

For long queries, candidate generation uses up to eight evenly spaced trigrams
from the first scalar-safe 512-byte anchor. The 1 KiB overlap guarantees that this
anchor fits in the document whose core owns a possible match start. Exact
verification then reads enough adjacent canonical text to check the complete
query. Candidate optimization may not require query trigrams outside that anchor
to appear in one document.

### Stage 2 representation selection

#### Selected: compact FTS5 trigram segments

Use contentless FTS5 with the trigram tokenizer, `detail=none`, and
`columnsize=0`. Generate a bounded conjunction of query trigrams as a candidate
filter, then verify against canonical records. Keep no full transcript-text copy
in the projection.

Candidate traversal batches reachable segment IDs, orders candidates by explicit
block index, and stops when no remaining segment range can improve the bounded page.
It does not infer block order from transcript sequence order because compacted
records can introduce block-index inversions. Every candidate is verified against
canonical record bytes. Document cores and overlap never cross canonical record
boundaries, and a match is reported only from the document that owns its start.
These rules prevent false negatives without synthesizing matches across records.

#### Rejected: chunk filters

The prototype used fixed 512-byte probabilistic filters per search document: 32
bytes for Unicode scalar presence, 128 bytes for bigrams, and 352 bytes for
trigrams. This representation is simple, bounded, and smaller on realistic text,
but every query scans every search document. Its 43.562 ms maximum cold p95 at
only 64 MiB leaves less interaction headroom and grows linearly with transcript
size. Keep chunk filters as a rejected fallback, not the production projection.

#### Rejected: run-length FM-index

The bounded run-length FM-index experiment was exact, but on a 1 MiB corpus it
used 30.83 percent physical storage, built at 3.88 MiB/s, consumed about 17.19 MiB
of incremental peak live allocation, and took about 9,277 ms warm p95 for a common
one-character query. It fails the storage, latency, memory-scaling, and stable
production-format gates.

### Stage 2 implementation evidence

The prototype evaluated all combinations of 4, 8, 16, and 32 KiB search documents
with 1, 2, 4, and 8 MiB canonical segments over both deterministic synthetic data
and read-only copied realistic fixtures. Every query class was differentially
checked against the direct canonical scanner. Coverage included one- and
two-character queries, common and rare text, punctuation, Unicode, long queries,
document boundaries, absence, and full enumeration. Long Unicode query and
boundary tests additionally exercise scalar-safe anchor and overlap logic.

The 64 MiB synthetic matrix produced these ranges:

| Representation | Passing | Physical projection | Build peak | Cold first-result p95 max | Warm first-result p95 max | Next-result p95 max | Gated absence p95 max |
|---|---:|---:|---:|---:|---:|---:|---:|
| Compact FTS | 16/16 | 7.05-15.42% | 1.37-12.02 MiB | 2.280 ms | 0.711 ms | 0.005 ms | 0.090 ms |
| Chunk filters | 16/16 | 10.83-21.64% | 1.01-8.16 MiB | 14.473 ms | 10.563 ms | 0.006 ms | 7.558 ms |

The 64 MiB realistic copied-fixture matrix produced these ranges:

| Representation | Passing | Physical projection | Build peak | Cold first-result p95 max | Warm first-result p95 max | Next-result p95 max | Gated absence p95 max |
|---|---:|---:|---:|---:|---:|---:|---:|
| Compact FTS | 4/16 | 21.60-64.25% | 3.11-24.97 MiB | 4.153 ms | 0.519 ms | 0.028 ms | 0.537 ms |
| Chunk filters | 16/16 | 6.97-18.67% | 1.04-8.16 MiB | 43.562 ms | 10.496 ms | 0.038 ms | 9.283 ms |

Only the 32 KiB compact FTS documents passed the 25 percent realistic-storage
gate, independently of segment size. The selected 32 KiB document and 2 MiB
segment configuration had:

- 21.91 percent physical projection size.
- 15.50 MiB/s build throughput.
- 5.46 MiB incremental live-allocation build peak.
- 22 MiB process RSS high-water growth above build start.
- 1.372 ms maximum cold first-result p95.
- 0.382 ms maximum warm first-result p95.
- 0.010 ms maximum next-result p95.
- 0.356 ms maximum gated absence p95.

At the same sizing, the synthetic projection used 7.23 percent of logical corpus
bytes, built at 28.28 MiB/s, and had about 2.70 MiB incremental peak live
allocation. Physical projection measurements include the SQLite database and
known sidecars, use physical searchable corpus bytes as the denominator, and do
not charge canonical record metadata to the disposable projection.

Every required cold-cache eviction succeeded before the corresponding sample, so
no warm sample was mislabeled cold. The realistic matrix exercised 220 successful
cold attempts per configuration. All configurations passed exactness. A
proven-absent query composed entirely of common corpus trigrams forced nonempty
candidate traversal before exact verification rejected every candidate. This
guards against absence measurements that only exercise an index miss. Streaming
one- and two-character searches also visited fewer candidates than full
enumeration when a bounded result was requested. The FTS query-plan regression
requires `ORDER BY f.rowid`, avoiding an eager temporary sort before the first
result.

### One- and two-character strategy

- Store compressed delta-varint document postings for hashed Unicode scalars and
  scalar bigrams beside the compact FTS projection.
- Stream posting candidates in document order and verify against canonical bytes.
- Never use byte-only filters that can exclude an exact Unicode scalar match.
- Preserve full-enumeration differential tests for common, rare, and absent queries
  even though interactive search stops after its bounded result count.
- Use direct scanning only for the bounded live frontier, not the complete growing
  transcript.

### Projection correctness

A search segment is visible only after one atomic install records:

- Search format version.
- The ordered immutable source-leaf IDs and their derived group identity.
- Logical searchable-byte extent.
- Complete document/filter/FTS rows.
- A completion marker and checksum.

Incomplete builds are ignored. A source-hash mismatch invalidates only that derived
segment. Missing, stale, corrupt, or incompatible `search.db` data schedules a
rebuild and falls back to progressive exact scan.

Search projection status is never stored as canonical branch truth. Readiness is a
claim made and verified by the derived database itself.

### Stage 3 implementation evidence

Lineage schema version 1 defines lineage identity, branch creation metadata,
immutable revisions, history and transcript roots, 32-way sequence nodes,
lineage-owned payload references, durable commit receipts, and explicit recovery
retention. It is an independent schema, not a continuation of any earlier
per-session version track.

The concrete persistent sequence uses cumulative item and byte extents, a 2 MiB
multi-item leaf bound, right-spine append, and boundary-path split. Exact loading
recomputes canonical hashes and validates kinds, levels, extents, completeness,
payload bytes, cycles, and root metadata. Common fork writes one branch row and
one receipt while sharing the captured immutable revision and all sequence data.
Branch creation revisions are immutable metadata, so fork receipts remain valid
after either branch moves and after the source is soft deleted.

Store validation covers:

- Differential reconstruction, seek, tail, append, split, fork, rewind, branch
  deletion, and retained-revision reachability against flat models.
- Structural sharing and operation counters that bind common work to tree height
  and changed suffix size rather than complete prefix size.
- Exact idempotent replay and full transaction rollback at object, payload, node,
  entry, root, revision, branch-head, and receipt boundaries.
- File-backed process crashes at node, revision, branch-head, and receipt
  boundaries, followed by `quick_check`, `foreign_key_check`, absent-or-complete
  state checks, and durable retry.
- Immutable sequence, revision, branch-creation, and receipt rows, including exact
  schema-shape validation for required indexes and triggers.

The complete store suite passes with 207 tests, one intentionally ignored test,
and no failures. Workspace clippy passes with warnings denied. The lineage module
remains test-only, and no TUI production path references it before Stage 4.

### Stage 4 implementation evidence

Canonical lifecycle ownership now uses one fenced `OwnedLineageWriter` for the
active lineage and branch-aware `LineageSessionReader` snapshots. Ordinary save,
`SubmitTurn`, turn transition and recovery, resume, branch switch, rewind, fork,
delete, import, export, request inspection, backup, doctor, statistics, and vacuum
all resolve the branch through its lineage. A submitted turn publishes its immutable
revision, branch sequence and head, turn row, and durable receipt in one canonical
transaction before provider dispatch. Request-audit ownership and cumulative runtime
accounting remain branch-local.

New lineage databases are built at a staged private path and published only after
a complete validated commit. Discovery considers only the canonical
`lineages/<lineage-id>/lineage.db` layout. Pre-lineage session directories are
ignored and no runtime or maintenance command converts them.

Canonical history and transcript payloads are normalized into content-addressed
objects. A separate nested-reference table records attachment and metadata objects
reachable from those payloads. Exact reads hydrate those references, sparse
transcript slices keep them deferred, history-tail byte budgets account for them
before hydration, and reachability includes both direct and nested objects. This
keeps canonical correctness independent of the temporary Stage 4 search scan while
preserving sparse-resume memory bounds.

Common fork now inserts branch identity and a durable fork receipt while reusing the
captured immutable revision. The 100-fork store gate passed with p95 below 100 ms,
physical growth at or below 64 KiB per fork, and no additional inherited payload,
object, sequence-node, or root rows. The large visible TUI gate also completed below
100 ms with at most 4 MiB of UI-thread allocation, preserved every canonical row,
and kept 20 alternating branch switches below the 100 ms p95 ceiling. Fork receipts
remain replayable after source rewind and deletion and after target movement, and a
user-facing deletion regression proves a surviving fork retains its shared history.

Rewind now selects or cuts immutable roots and advances the branch head without
synchronously deleting the abandoned suffix. An end-to-end TUI regression restarts
from the rewound roots while the old rows remain available for later bounded
reclamation. A separate graceful-exit regression restarts from lineage storage and
proves that shutdown does not create a second canonical storage layout.

Launch into a canonical session resolves and validates the persisted workspace before
generation-zero full Lua bootstrap. Project config and plugins therefore load once in
the restored directory, while the ready hook still performs the canonical bounded
session install. Header-only lineage reads now open a snapshot without reading or
hydrating a transcript record. The sparse transcript keeps its visible active
projection separate from its wider cache guard, preserves the loaded resume tail
through repeated tail activation, and guarantees every centered reveal range
contains the requested record.

A final ten-run release-fast PTY measurement over a copied lineage fixture produced:

| User boundary | p95 |
|---|---:|
| First interactive frame | 90.796 ms |
| Requested session visibly ready | 90.855 ms |
| Graceful shutdown | 13.439 ms |
| Peak RSS | 88,012 KiB |

The copied source remained byte-identical. The expensive pre-frame cwd candidate
reload is absent, and transcript compaction runs once for the installed sparse
projection. The complete validation gate passes 5,000 workspace tests with two
skipped, including 1,333 TUI library tests and 193 storybook tests. The focused store
suite passes 218 tests with one ignored, core passes 1,320 with one ignored, session
command integration passes nine, and startup PTY integration passes six. Formatting,
`git diff --check`, and warnings-denied workspace Clippy pass. Stage 4 therefore
meets its lifecycle, structural-sharing, durability, resume, fork, rewind, delete,
and single-format storage gates.

### Stage 5 implementation evidence

Each lineage owns a disposable `search.db` with independently validated format
version 4. The canonical lineage schema is version 1 and contains no transcript
search tables, FTS virtual tables, searchable-text copies, or search-maintenance
triggers. A dormant per-lineage projector does no work until search is activated, and
canonical submission only coalesces an asynchronous wake after activation. An
end-to-end persistence regression submits and closes a session without activating
search and proves that Enter creates no `search.db`.

Projection groups consecutive immutable transcript leaves into deterministic source
segments bounded by 2 MiB and 32 leaves. The group identity hashes the ordered leaf
IDs, so forks reuse complete shared groups and appends rebuild at most the bounded
open suffix. Each installed segment packs consecutive short records into 32 KiB
documents and splits oversized records with 1 KiB overlap. It uses contentless trigram
FTS5 with `detail=none` and `columnsize=0`, compressed Unicode scalar and bigram
postings, integer internal segment keys, a checksum, and an atomic completion marker.
Boundary regressions cover exactly 2 MiB, crossing 2 MiB,
32 leaves, and 33 leaves and prove sealed group identities remain stable.

Queries combine ready reachable segments with exact canonical scans for missing or
unindexed sources; the TUI adds its dirty in-memory suffix. Long-query FTS and short
posting traversal batch all reachable segment IDs and order by explicit block index,
including real lineage histories where compacted records cause block-index
inversions. Every candidate is verified against canonical `indexed_text`. Short
queries hydrate only candidate canonical records rather than complete 2 MiB source
groups. Derived failures can fall back to canonical scanning, but canonical read
errors remain visible.

Differential coverage includes common and Unicode one-character queries, common,
punctuation, and Unicode two-character queries, long Unicode and three-plus-character
queries, document overlap, record boundaries, deliberate FTS false positives, both
directions and origins, and bounded pages. Missing files, non-SQLite files, old format
versions, incomplete installs, malformed postings, and structurally invalid segment
metadata all preserve exact results and rebuild on a later projection request. A
cancellation regression interrupts an open segment transaction, proves that no
segment, document, posting, or FTS row becomes visible, and then completes a clean
retry. Doctor reports missing, lagging, incompatible, or corrupt derived state without
classifying canonical lineage storage as corrupt.

A fork-and-rewind regression projects a source, reuses its segment in a fork without
duplication, adds only a target suffix segment, filters that suffix immediately after
rewind, and retains the now-unreachable segment for later bounded reclamation. Search
correctness does not depend on physical deletion.

The production benchmark ran 20 samples per query class over four copied realistic
lineage fixtures. It measured 419,816,739 logical searchable bytes and
94,539,776 physical derived bytes, including SQLite sidecars, for an aggregate storage
ratio of 22.52 percent. Worst p95 by query class was:

| Query class | Worst p95 |
|---|---:|
| Common one-character | 22.057 ms |
| Unicode one-character | 42.042 ms |
| Common two-character | 26.582 ms |
| Punctuation two-character | 25.599 ms |
| Common three-plus-character | 31.154 ms |
| Proven absent | 46.857 ms |

The complete store suite passes 232 tests with one skipped. The workspace gate passes
5,008 tests with two skipped, plus formatting, `git diff --check`, and warnings-denied
workspace Clippy. Stage 5 therefore meets its exactness, latency, physical-storage,
structural-sharing, corruption-recovery, cancellation, and zero-Enter-work gates.

### Stage 6 implementation evidence

Search-mode cmdline edits, paste, and history navigation now create monotonically
numbered search generations. Persisted candidate lookup runs on one concrete
latest-value worker: a new request replaces pending work, cancellation invalidates the
running generation, and completion returns through the existing application event
channel. Keystroke handling never opens SQLite or hydrates canonical transcript
records. The UI validates the generation, session identity, target window, target
buffer, query, and direction before applying a result, so stale work cannot move a
viewport after rapid typing, branch movement, rewind, session switch, or reset.

Opening `/` or `?` captures the original cursor, scroll position, viewport-relative
row, width, semantic transcript anchor, and canonical block. Current and unloaded
persisted matches preview while the cmdline remains open. Sparse candidate activation
and continuation retain that canonical origin instead of deriving it from a preview's
replacement window. Empty input, Esc, and a current no-match result restore the
original semantic anchor. Enter confirms the installed preview without rerunning the
query; if lookup is still pending, it confirms the eventual current generation.
Confirmed `n` and `N` continue in the original and reverse directions using the same
search session.

End-to-end harness coverage exercises edits, paste, history replacement, preview before
Enter, Enter without a second move, empty-query and Esc restoration, an unloaded early
match, a matching-to-absent transition, slow-worker rapid typing, stale generations,
session switch, rewind, reset, and forward and reverse sparse continuation. With an
80 ms artificial worker delay, optimized per-keystroke handling plus redraw measured
0.463 ms p95 against the 16 ms one-frame budget. The `transcript_live_search_preview`
storybook snapshot verifies visible match highlighting, stable transcript placement,
and the active search cmdline.

The focused search suite passes 27 tests. The workspace gate passes 5,018 tests with
two skipped, plus formatting, `git diff --check`, warnings-denied workspace Clippy, and
the focused storybook snapshot. Stage 6 therefore meets its non-blocking typing,
stale-result rejection, loaded and unloaded preview, deterministic cancellation,
confirmation, and continuation gates.

### Stage 7 implementation evidence

`smelt session gc` now runs explicit maintenance outside Enter, fork, rewind,
resume, branch switch, deletion, and exit. Each canonical step builds temporary
reachability marks from live heads, every immutable branch-creation revision, and
explicit recovery or export retention, then traverses parent revisions, sequence
roots and nodes, and payloads. It performs at most one dependency-ordered deletion
phase and honors the requested row budget. The production command uses 256 rows per
transaction and reports cleared heads, deleted canonical rows, deleted objects,
deleted search segments, transaction count, free pages before and after maintenance,
and pages reclaimed by bounded incremental vacuum.

Deletion proceeds through turn transitions, session and commit receipts, branch
history, continuation turns from leaf to parent, revisions from child to parent,
roots, entries, nodes, nested references, payload references, and objects. The three
normal-operation receipt deletion guards are suspended and restored inside each
transaction, so rollback after an injected process abort restores both data and
schema guards. Reachable shared fork data, explicit retained revisions, branch
origins, nested attachment and metadata objects, and request-audit references
survive. Once their final reference is removed, unreachable objects are reclaimed.
Branch-local request audit and immutable branch
origins intentionally remain for the lifetime of a branch tombstone and disappear
with final lineage retirement.

Derived cleanup deletes one bounded source segment before each canonical step. The
projection records only the segment's immutable source leaf IDs and extents. GC
reconstructs the original indexed terms from canonical source leaves, issues exact
contentless FTS5 delete commands, and then cascades documents and compact postings.
Shared source segments remain available across forks, while a rewound target-only
segment and its trigram matches disappear. Missing projection is a no-op, and
incompatible, incomplete, malformed, corrupt, or unreconstructable projection state
is discarded rather than blocking canonical reclamation. Search remains disposable
and canonical correctness never depends on it.

Deleting a final live branch durably clears and tombstones its head, closes SQLite,
syncs the lineage directory, and renames the complete lineage into `lineages/.trash`
under the stable lineage lease before best-effort physical removal. Startup cleanup
handles both interrupted states: a published lineage with no live branch and a
post-rename trash tombstone. It skips an active stable lease and rejects symlinked
lineage or trash roots. Deleting a source while a fork survives keeps the shared
lineage readable; deleting the final fork retires it.

Stale private staging directories are discarded before retrying, while publication
remains rename-based and atomic. Regressions cover interrupted staging, pre-rename
and post-rename final deletion, SQLite full during lineage revision publication,
ownership conflicts, canonical reclamation crashes, corrupt and incomplete derived
search, corrupt and crash-interrupted catalog projection, schema corruption, and
retry after failure. Static production-path review confirms one lineage mutation
path and one derived search path. No production fork copies a prefix, rewind deletes
a suffix, or canonical lineage database maintains transcript FTS.

The simplification pass made derived cleanup's one-segment contract explicit,
centralized its disposable-failure reset behavior, and aligned test reachability with
the production branch-origin root set without adding a generic GC abstraction. The
store suite passes 235 tests with one intentionally ignored test, storage failure
boundaries pass seven tests, core ownership boundaries pass three tests, and the real
session command suite passes ten tests. The workspace gate passes 5,029 tests with
two skipped in 83.668 seconds, plus formatting, `git diff --check`, and
warnings-denied workspace Clippy. Stage 7 therefore meets its bounded reclamation,
sharing safety, crash recovery, final retirement, and single-path exit gates.

### Stage 8 implementation evidence

Final validation reran the user-boundary gates in isolated state with the current
release-fast binary. A ten-run empty-startup matrix and a ten-run copied realistic
session matrix produced:

| Workload | First-frame p95 | Visible-ready p95 | Exit p95 | Peak RSS p95 |
|---|---:|---:|---:|---:|
| Empty startup | 55.875 ms | 55.941 ms | 11.132 ms | 76,520 KiB |
| Realistic lineage session | 88.799 ms | 88.829 ms | 12.528 ms | 87,120 KiB |

The realistic source fixture remained byte-identical. Compared with the Stage 4
realistic result, first frame improved from 90.796 ms, visible ready improved from
90.855 ms, exit improved from 13.439 ms, and peak RSS decreased from 88,012 KiB.
Fresh common-fork and large sparse visible-fork gates also pass, retaining the 100 ms,
64 KiB per-fork, 4 MiB UI-allocation, exact canonical-row preservation, and branch-
switch ceilings documented in Stage 4.

A fresh optimized realistic search matrix ran 20 samples for each query class over
four copied lineages. It measured 419,816,739 canonical searchable bytes and
94,744,576 physical derived bytes, including SQLite sidecars, for a 22.568 percent
aggregate ratio. The worst query-class p95 was 47.252 ms. The fresh optimized rapid-
typing and redraw gate measured 0.471 ms p95 with an artificial 80 ms worker delay;
the unoptimized validation build measured 2.725 ms. Both remain below the 16 ms frame
budget, and the persisted search result remains below the 50 ms warm-result ceiling.

A post-review short-record stress fixture with 10,000 transcript records measured
3,174,608 logical searchable bytes and 413,696 physical derived bytes, a 13.031
percent ratio. Its worst 20-run query-class p95 was 36.880 ms. This fixture guards
against per-record document overhead silently violating the same storage ceiling.

The complete quality matrix passes:

- 5,029 workspace tests with one slow test and two skipped, including 235 store tests
  with one intentionally ignored test, seven storage failure-boundary tests, three
  core ownership-boundary tests, and ten session command integration tests.
- Warnings-denied workspace Clippy, formatting, `git diff --check`, and the focused
  live-search storybook snapshot.
- 85.13 percent total region coverage across 360,453 regions. The lineage storage,
  access, and search modules have 88.38, 87.48, and 84.39 percent region coverage.
- Generated Lua documentation for 85 modules, 461 functions, 92 classes, and 13
  aliases, with generated navigation, customization references, and plugin inventory
  synchronized.
- All 97 retained fuzz regression seeds: 81 main-loop, two Lua-loop, one cache-
  invariance, two provider-body, three transcript-render, six transcript-scroll, and
  two permission-rule seeds.

The final static architecture audit confirms that production canonical mutation
uses `OwnedLineageWriter` and reads use `LineageSessionReader`. The deleted
per-session writer, reader, maintenance, search-table, and suffix-deleting rewind
implementations have no runtime, fixture, test, benchmark, fuzz, or documentation
surface. The lineage schema contains no canonical search tables, normal fork inserts
one branch row, and root-based rewind performs no synchronous reclamation.

Queue, actor, cache, fallback, and transition review found no second canonical writer
or search request queue. The persistence actor has a 64-entry control lane, one
latest desired save, at most 64 request audits, and a 16 MiB full-audit payload cap;
canonical controls run before queued audits. Live search retains one pending request
and rejects stale generations before and after persisted lookup. The derived
projector coalesces requests through generation counters and installs only complete
segments. Search segments are bounded by 2 MiB and 32 leaves, the read-only search
connection uses a 2 MiB SQLite page cache, and transcript hydration retains explicit
32 MiB hydrated-block, 16 MiB record-window, and 16 MiB rendered-payload budgets.
Reclamation remains reachable only through explicit maintenance commands.

The privacy audit found no copied transcript text, fixture identifier, temporary
artifact name, user home path, or credential in this document or generated output.
Only aggregate measurements are retained.

Final deliberate deviations and implementation refinements are:

- Contentless FTS5 with `columnsize=0` cannot use `contentless_delete=1`. Derived
  cleanup therefore reconstructs exact terms from canonical source leaves and uses
  the FTS5 delete command while retaining the smaller selected index format.
- Immutable branch initial revisions remain reachability roots until final lineage
  retirement. This preserves branch-origin foreign keys and audit history after a
  branch tombstone.
- Empty-startup peak RSS is 76,520 KiB, above the frozen Stage 1 empty baseline of
  54,896 KiB. The increase is recorded rather than hidden; it remains below the
  87,120 KiB current realistic-resume result, and the bounded-cache and queue audit
  found no session-size-dependent startup allocation.

Stage 8 therefore passes the latency, storage, structural complexity, durability,
coverage, privacy, and quality gates. The terminal architecture has
one canonical lineage mutation path, one disposable derived search path, and no
interaction-path reclamation.

## Performance and resource contracts

The provisional interaction ceiling is 100 ms p95. A lower operation-specific
budget is used where current evidence proves it is practical.

### Latency targets

| Operation | Acceptance target |
|---|---:|
| Static CLI command | p95 <= 15 ms |
| Process launch to first interactive frame | p95 <= 100 ms |
| Process launch to requested session visibly usable | initial gate p95 <= 150 ms, target <= 100 ms |
| In-process resume or branch switch | p95 <= 100 ms |
| Enter to durable dispatch | p95 <= 75 ms and never above 100 ms in the standard matrix |
| Search keystroke handling/redraw | p95 <= 16 ms |
| Warm first verified search result | p95 <= 50 ms |
| Cold first verified search result | p95 <= 100 ms |
| Proven absent search, including two characters | p95 <= 100 ms |
| Cached `n` / `N` reveal | p95 <= 50 ms |
| Ordinary rewind | p95 <= 50 ms |
| Common fork to usable branch | p95 <= 100 ms |
| Graceful clean exit | p95 <= 100 ms |
| Session-list first page and filter | p95 <= 50 ms |

The 150 ms initial launch-to-resume gate acknowledges process setup plus bounded
session loading as two visible phases. It is not permission to stop there. After
single-generation startup lands, the target is re-evaluated against 100 ms.

### Complexity targets

| Operation | Required complexity |
|---|---|
| Startup | independent of transcript/history bytes |
| Resume | `O(tree height + configured tail + viewport)` |
| Send | `O(changed bytes + tree height)` |
| Search | `O(candidate postings + verified candidate bytes + bounded frontier)` |
| Rewind | `O(tree height + boundary leaf)` |
| Fork | `O(1)` branch metadata, excluding SQLite commit constants |
| Delete branch | `O(1)` head mutation; reclamation asynchronous |
| Exit | `O(uncommitted suffix + owned resource count)` |

### Storage targets

- A normal fork adds no inherited payload, object, transcript, or search bytes.
  Database growth should remain below 1 MiB excluding temporary WAL high-water
  behavior and should be reported exactly.
- Aggregate search projection bytes must be at most 25 percent of logical searchable
  UTF-8 bytes across the realistic text-heavy acceptance corpus. Per-fixture sizes
  are also reported so fixed SQLite overhead remains visible. The stretch target is
  15 percent.
- No uncompressed or full-size duplicate searchable-text table is permitted.
- Search storage is reported separately for FTS/postings, filters, mappings,
  checksums, free pages, and any exact-text cache.
- Canonical write amplification for Enter, rewind, and fork is reported as SQLite
  bytes written per changed canonical byte and as rows/nodes touched.
- Repeated forks of one prefix must grow with branch metadata and unique suffixes,
  not `fork count * prefix bytes`.

### Memory targets

- Fork must not hydrate inherited history, transcript payload, or objects.
- Resume retained memory remains bounded by existing tail, viewport, hydration,
  record-window, and render budgets.
- Search query memory is bounded by one candidate page, one verification window,
  and result cache limits.
- Search build peak memory is bounded by one corpus segment and must not grow with
  total lineage bytes.
- The implementation must report incremental RSS and allocator churn per operation,
  not only process high-water marks after fixture construction.

## Benchmark matrix

Every acceptance run uses a release or distribution binary and reports p50, p95,
p99, maximum, peak RSS, allocation churn, bytes read/written, SQLite rows/nodes,
and final physical database sizes. Cold and warm cache states are separate.

### Data shapes

1. Empty first launch.
2. Small ordinary session.
3. 10,000 and 50,000 short history rows.
4. 50 MiB and 500 MiB mixed text transcripts.
5. Deep text-heavy session with common repeated language.
6. Object-heavy session with hundreds of MiB of compressed object payload.
7. Sparse session with a huge database and tiny visible tail.
8. A lineage with 100 forks sharing a large prefix and varied small suffixes.
9. A rewind-heavy lineage with substantial unreachable data before maintenance.
10. Missing, stale, partially built, and corrupt search projections.

### Search queries

- One-character common, rare, punctuation, and Unicode.
- Two-character common, absent, punctuation, and Unicode.
- Three-character common and absent.
- Long rare identifiers.
- Repeated common words.
- Multi-word phrases with few exact matches and many trigram false positives.
- Queries crossing search-document, transcript-record, and segment boundaries.
- Queries longer than the overlap window.
- Matches in loaded, unloaded, dirty, committed-unindexed, and indexed regions.
- Forward and backward search from beginning, middle, and end.
- Rapid typing and deletion with stale generations completing out of order.

### End-to-end boundaries

- Real PTY launch to first frame, requested-session marker, and clean process exit.
- Real prompt Enter through Lua, canonical SQLite, receipt, and captured provider
  dispatch.
- Real `/` or `?` input edits, preview, Enter, Esc, `n`, and `N` through rendered
  transcript behavior.
- Real fork command through durable destination publication and visible switch.
- Real rewind command through head commit and rendered result.
- Real delete and session switch through catalog and lineage validation.

Microbenchmarks are retained to localize tree, FTS, filter, and SQLite costs, but
cannot satisfy a phase gate by themselves.

## Failure and recovery contracts

| Failure boundary | Required result |
|---|---|
| Canonical commit before WAL commit | No head movement and no dispatch |
| WAL commit before receipt delivery | Exact fingerprint returns original receipt |
| Fork branch insert before commit | No destination branch |
| Fork commit before receipt delivery | Exact retry returns the same destination |
| Search build crash | Old complete segments remain usable; partial segment ignored |
| Search database missing or corrupt | Canonical session usable; progressive scan and rebuild |
| Search projector disk full | Canonical writes continue; visible bounded warning |
| Catalog missing or stale | Exact branch open validates lineage; catalog rebuilds |
| Source branch deleted after fork | Fork remains complete and searchable |
| Rewind followed by crash | Either old or new head, never a mixed root |
| GC crash | Reachable roots and objects remain; batch is repeatable |
| Lineage database corrupt | Failure isolated to that lineage |
| Persistence blocked on exit | Explicit retry, fork/save-as, or discard decision |
| Runtime/config evaluation fails | One reported generation failure, no second evaluation |

Search may degrade in completion time after projection loss, but it may not return
incorrect matches or incorrect absence. Canonical recovery is never coupled to
search repair.

## Format boundary

The canonical lineage format starts at schema version 1 and has its own version
track. It does not inherit version numbers, tables, locks, receipts, or migration
semantics from pre-lineage storage.

Discovery scans only `sessions/lineages/<lineage-id>/lineage.db` and validates live
branch rows before exposing a session. Other layouts are ignored without being
opened, classified, quarantined, imported, or rewritten. There is no migration
command and no dual-format runtime path.

## What is retained and what is rewritten

| Area | Decision |
|---|---|
| SQLite, WAL, `synchronous = FULL` | Retain |
| Stable lease and owner fencing | Retain, move ownership to lineage |
| Durable `SubmitTurn` receipt before dispatch | Retain |
| Ambiguous commit fingerprint reconciliation | Retain |
| Bounded history tails and scalar projections | Retain |
| Sparse resume and transcript viewport hydration | Retain |
| Content-addressed objects and typed references | Retain within lineage |
| Byte-bounded transcript/render caches | Retain |
| Root catalog as rebuildable listing projection | Retain, map branches to lineages |
| Append-only context-change semantics | Retain |
| Duplicate launch Lua generation | Rewrite to one transferred generation |
| Per-session physical canonical database | Rewrite to per-lineage database |
| Mutable row-prefix fork copy | Rewrite to shared revision head insertion |
| Destructive rewind of canonical suffix | Rewrite to immutable root movement |
| Transactional full-detail per-session search | Rewrite as derived compact lineage search |
| Enter-only search UI | Rewrite as cancelable generation-based live search |
| Full searchable-text duplicate | Remove |
| Search work in Enter transaction | Remove |

## Implementation stages

Each stage must leave one production path for the behavior it changes. Startup and
lineage/search work should remain separate reviewable changes. Do not combine both
rewrites into one patch.

### Stage 0: approve contracts and close baseline gaps

- Review this document and decide the branch metadata versus revision-state
  boundary.
- Extend the permanent lifecycle harness to report p95 for startup, ready, fork,
  rewind, search input, first result, absence, branch switch, and exit.
- Add physical `dbstat` reporting and operation-level I/O/RSS attribution.
- Preserve copied-fixture methodology and privacy checks.
- Add the 100-fork and search-pathology fixtures.
- Record current release-fast and distribution baselines.

Exit gate:

- Every latency, memory, storage, and complexity contract has an executable
  measurement.
- No architecture implementation has started before the branch/revision semantics
  are approved.

### Stage 1: make startup one generation

- Start instrumentation before runtime creation.
- Separate static bootstrap parsing from dynamic Lua flag parsing.
- Transfer the evaluated Lua generation and runtime environment into command/TUI
  construction.
- Remove the second launch candidate evaluation.
- Keep reload semantics explicit and independent of launch.
- Re-run empty, resume, headless, config-error, plugin-flag, and graceful-exit PTY
  tests.
- Simplify before proceeding. Do not add a startup cache unless one-generation
  measurements expose a separate repeated cost.

Exit gate:

- Exactly one launch generation evaluates.
- Dynamic plugin flags and config errors behave correctly.
- First-frame p95 meets the target or the remaining spans identify a new concrete
  blocker.

### Stage 2: prototype search representations

- Build a Rust/SQLite-native prototype over safe copied fixtures.
- Test 4, 8, 16, and 32 KiB search documents and 1 to 8 MiB corpus segments.
- Compare compact FTS, chunk filters, and one bounded self-index experiment.
- Implement no-false-negative overlap and long-query anchor logic.
- Prototype one- and two-character filters.
- Verify against the direct Rust scanner for every query class.
- Measure build throughput, peak RSS, physical bytes, cold/warm first result,
  absence, full enumeration, and next-result latency.

Exit gate:

- One design passes exactness, <=25 percent storage, bounded build memory, and all
  search p95 targets.
- If none passes, revise the architecture before changing production search.

### Stage 3: introduce lineage schema and immutable roots

- Add lineage identity, branch, revision, sequence-node, and root schemas.
- Implement concrete history/transcript append, seek, tail, split, and root
  validation.
- Adapt content-addressed objects to lineage ownership.
- Add branch-aware commit fingerprints and receipts.
- Implement reachability inspection without deleting data yet.
- Add property and differential tests against flat vectors across random append,
  fork, rewind, and branch sequences.
- Add crash tests at node, revision, and head-update boundaries.

Exit gate:

- Store tests prove structural sharing and exact reconstruction.
- Append, tail, split, and fork complexity depends on tree height and delta only.
- No TUI production path has cut over yet.

### Stage 4: cut over canonical lifecycle and fork

- Move persistence ownership from session actor to lineage actor.
- Cut `SubmitTurn`, ordinary save, resume, branch switch, rewind, fork, delete,
  import, and export to lineage commands.
- Make catalog rows branch-aware.
- Delete the full-prefix fork import and destination rewrite path.
- Keep current search temporarily behind a bounded transitional test setup only if
  required during this stage; do not copy its data into forks.
- Implement staged lineage publication and ignore non-lineage layouts.
- Run the 100-fork storage and large-fork PTY gates.

Exit gate:

- Common fork is a branch-row commit and meets p95, RSS, and storage targets.
- Rewind moves roots without suffix deletion.
- Send and resume retain or improve their current latency and durability evidence.
- Deleting a source branch leaves forks intact.
- No runtime mutation writes any second canonical format.

### Stage 5: install compact derived search

- Add disposable per-lineage `search.db` with strict format versioning.
- Add bounded immutable segment scheduling, building, installation, cancellation,
  and corruption recovery.
- Merge indexed segments, committed-unindexed ranges, and dirty in-memory suffixes.
- Add branch-root reachability filtering and directional candidate cursors.
- Remove transactional `transcript_search`, `transcript_search_chars`, full-detail
  FTS, triggers, and searchable-text copies from canonical storage.
- Add doctor output for derived search readiness without treating lag as canonical
  corruption.

Exit gate:

- Search passes all literal differential tests and p95 targets.
- Search projection passes the <=25 percent physical storage gate.
- Fork and rewind reuse or filter search segments without rebuild or duplication.
- Missing and corrupt search projections preserve correct progressive results.
- Enter performs zero search projection work.

### Stage 6: add live-search interaction

- Trigger search generations from search-mode cmdline edits and paste/history
  changes.
- Add latest-value worker requests and stale-generation cancellation.
- Add original-anchor preview, Esc restore, Enter confirm, and stable `n` / `N`
  continuation.
- Keep event handling and redraw within one-frame p95.
- Add rapid-typing, slow-worker, branch-switch, rewind, and session-close race
  tests.
- Review visual behavior in PTY/storybook snapshots for stable cursor, scroll,
  highlight, and status presentation.

Exit gate:

- Typing never blocks on SQLite or canonical hydration.
- Stale results cannot move the viewport.
- Current and unloaded matches preview correctly.
- Empty query, Esc, Enter, `n`, and `N` have deterministic behavior.

### Stage 7: reclamation, format hardening, and cleanup

- Add bounded mark/sweep reclamation and free-page metrics.
- Add final-branch trash publication and interrupted-cleanup recovery.
- Test disk full, ownership loss, and corrupt lineage, search, and catalog databases.
- Remove every pre-lineage schema writer, reader, command, fixture, and fuzz target.
- Remove obsolete per-session lock, fork-copy, rewind-delete, and FTS code.
- Reconcile canonical storage, TUI runtime, Lua API, and benchmark documentation.
- Run the simplification skill and delete abstractions that no longer enforce an
  invariant.

Exit gate:

- There is one canonical lineage path and one derived search path.
- No dormant compatibility or full-copy implementation remains.
- Reclamation never blocks an interaction and never removes reachable shared data.

### Stage 8: complete validation

- Run all lifecycle and search matrices in isolated release processes.
- Run workspace tests, warnings-denied Clippy, formatting, coverage, generated Lua
  docs, storybook snapshots, fuzz regressions, static architecture searches, and
  privacy checks.
- Compare p95, maximum, RSS, allocator churn, I/O, database size, and complexity
  counters with the frozen baseline.
- Inspect every retained queue, actor, cache, fallback, and state transition.
- Update this document with final implementation evidence and deliberate deviations.

Exit gate:

- Every operation meets its contract or has an explicitly approved revised target
  backed by user-boundary evidence.
- The resulting implementation is simpler to explain than the current combination
  of per-session prefix copies and branch-private full-detail search.

## Decision record and remaining approval

The following core decisions are approved:

1. Use one SQLite database per fork lineage as the canonical isolation and sharing
   boundary.
2. Live search previews and temporarily moves to the nearest result. Esc restores
   the original anchor.
3. Derived search storage has a hard ceiling of 25 percent of logical searchable
   bytes and a 15 percent target.
4. Pre-lineage storage is unsupported and ignored; canonical lineage schema version
   1 has an independent version track.
5. Use 2 MiB canonical search segments, 32 KiB core documents with 1 KiB overlap,
   compact contentless trigram FTS, and compressed scalar/bigram postings. Verify
   every candidate against canonical bytes.

The branch/revision field boundary is recorded in `Branches and revisions` above.
The format boundary is final: runtime open, mutation, fork, rewind, search,
maintenance, tests, fuzzing, and benchmarks use lineage storage only.

The Stage 2 prototype gate has passed with the compact FTS design and selected
sizing documented above. Production search remains derived and disposable and is
introduced only after canonical lineage storage can provide immutable segment
roots. Single-generation startup remains the first completed production cutover.
