# Incremental transcript architecture objective

Status: complete

## Objective

Replace smelt's snapshot-oriented transcript pipeline with one incremental,
retained architecture whose work is proportional to new data and changed visible
rows.

The finished system must remain responsive while an agent works without user
input, regardless of accumulated transcript history, active tool output, group
size, or sparse session state. Tool output, local command output, streamed tool
arguments, persisted history, layout, and viewport projection must use the same
bounded data flow instead of separate clone, hash, serialization, and rendering
paths.

This is a replacement, not a compatibility retrofit. We will remove or redesign
APIs, data structures, Lua contracts, caches, and storage queries that obstruct
the target architecture. We will not preserve a costly abstraction solely to
minimize migration work.

## Definition of done

This objective is complete only when all of the following are true:

1. Live transcript mutations are append-oriented typed operations over stable
   node identities. No continuation update replaces or snapshots accumulated
   output.
2. The render path consumes retained node revisions and bounded source ranges.
   It never receives a JSON copy of complete live tool, exec, draft, or group
   state.
3. Layout and projection work is limited to dirty visible nodes and requested
   rows. A visual cap is also a computational cap.
4. All continuation events are merged at the frame boundary. First output,
   lifecycle transitions, errors, permissions, and user interaction remain
   urgent.
5. Draft parsing, ANSI decoding, line indexing, display hashing, and group
   aggregation are incremental. Their total work is linear in bytes appended or
   children changed.
6. Sparse extent and semantic navigation queries use retained or persisted
   metadata and never deserialize transcript payloads merely to compute a row
   estimate or nearest semantic neighbor.
7. Animation-only frames reuse transcript layout, extents, navigation, and
   visible rows.
8. Frame preparation performs no synchronous storage opens and no unbounded
   storage reads.
9. The superseded snapshot, replacement, full-payload hash, full-child group,
   synchronous navigation-scan, and payload-backed extent paths are deleted.
10. Correctness, allocation, timing, and scaling gates in this document pass in
    release mode, and the final benchmark results are recorded.

## Architectural principles

### One source of truth

`Transcript` owns typed nodes and their content. Renderers, Lua, groups, storage,
and the scheduler hold IDs, revisions, and bounded views, not duplicate block or
tool snapshots.

### One mutation vocabulary

Provider events, local exec output, draft deltas, persistence hydration, rewind,
and lifecycle changes all become explicit `TranscriptMutation` values. A
mutation is applied once and returns a compact `TranscriptChangeSet` describing
which stable nodes and semantic properties changed.

### Ownership transfer instead of copying

Incoming strings and byte buffers are moved into chunked content storage. An
append reuses the incoming allocation when practical. Existing content is never
cloned to add a suffix.

### Retained computation instead of repeated derivation

Hashes, parser state, row checkpoints, extent summaries, group child layouts,
and semantic neighbors are retained next to the revision that makes them valid.
A frame looks up retained results; it does not rediscover unchanged facts.

### Bounded synchronous work

The UI thread may apply small mutations, select a viewport, and paint a row diff.
Payload hydration, large-chunk parsing, and persistence happen through bounded
work queues. No Lua callback or frame phase can synchronously request an
unbounded payload range.

### Explicit invalidation

Every change identifies its semantic effects: content append, lifecycle,
structure, presentation, extent, navigation, or animation. A spinner tick cannot
invalidate transcript content. Updating one group child cannot invalidate an
unchanged sibling.

### Delete the old path

There will be no long-lived dual renderer, compatibility mirror, or shadow model.
Each migration slice must remove the path it replaces. Temporary adapters are
allowed only inside an incomplete slice and must not survive the slice's final
commit.

## Target data flow

```text
provider / local exec / draft / persistence
                    |
                    v
          TranscriptMutation
                    |
                    v
        Transcript::apply_mutation
          |                  |
          v                  v
 retained typed nodes   TranscriptChangeSet
          |                  |
          |                  v
          |          semantic frame scheduler
          |                  |
          v                  v
      RenderStore <---- apply pending changes
          |
          v
 width-specific retained row and extent indexes
          |
          v
 bounded viewport row query
          |
          v
 terminal row diff
```

Persistence consumes the same typed node changes. It does not require the render
layer to materialize a second transcript representation.

## Core model

### Stable transcript nodes

Each transcript entry has a stable `BlockId` and a typed node payload. The node
contains compact semantic metadata and references content by `ContentId`.
Ordering is maintained separately so inserting, grouping, hydrating, or rewinding
does not change unrelated identities.

The core node kinds should describe domain meaning, not renderer implementation:

- user and assistant text;
- reasoning;
- tool invocation and lifecycle;
- local process invocation and lifecycle;
- streamed tool draft;
- status and system records;
- group references to child IDs.

A group never embeds child snapshots. It stores child IDs and group presentation
metadata.

### Chunked content

`ContentStore` owns append-only UTF-8 chunks. A live content entry retains:

- total byte and character lengths;
- chunk boundaries;
- incremental content hash;
- newline and display-cell checkpoints;
- ANSI/parser continuation state;
- a monotonic content revision.

Appending is amortized `O(new_bytes)` and does not touch prior chunks. Completed
content may be compacted or persisted in idle work without changing `ContentId`.
Readers request explicit byte or visual-row ranges through UTF-8-safe APIs.

There should be one chunked content implementation for tool output, exec output,
assistant text, reasoning, and large draft strings. Content-specific behavior is
parser state, not a different storage algorithm.

### Typed mutations

The mutation vocabulary should stay small and concrete. Expected operations are:

- insert or remove a node;
- append content to a node channel;
- update lifecycle or semantic metadata;
- update draft parser input;
- attach or detach group children;
- install a persisted node reference;
- hydrate or evict bounded content;
- apply rewind or compaction boundaries.

Mutation application returns revisions and invalidation flags. Callers do not
manually clear render caches.

### Revisions, not payload hashes

Stable monotonic revisions identify content, metadata, structure, and
presentation changes. Cache keys use IDs, revisions, width, and presentation
state. Full payload hashes are reserved for persistence integrity and are
maintained incrementally as bytes arrive. They are never recomputed to decide
whether to render.

## Incremental parsing and indexing

### Text and ANSI

Each content entry incrementally records logical line starts and parser
checkpoints at bounded byte intervals. A checkpoint contains enough ANSI and text
state to resume rendering without scanning from byte zero.

Width-specific visual-row indexes are built from these checkpoints and extended
only for appended content. Tail caps query the final retained rows directly.
Adjacent row requests are rendered as one range.

A single oversized provider chunk is processed in bounded slices. Parsing can
continue between frames, while already indexed rows remain renderable. No frame
must absorb work proportional to an arbitrary network chunk size.

### Markdown

Streaming Markdown keeps parser state at stable block boundaries. Completed
prefix blocks become immutable retained layout nodes. Appends may invalidate only
the unfinished suffix whose syntax can still change. Rendering a visible suffix
does not reparse a completed prefix.

### Tool-call drafts

A draft owns one incremental JSON parser and typed field state. Characters are
appended to field content storage without cloning the growing value or rebuilding
a map. The render store sees an immutable revisioned field view only when a frame
consumes pending changes.

Final arguments reuse parser results and content chunks. Finalization does not
replay all streamed characters through a second preview path.

## Retained rendering

### Rust-owned structural render tree

`RenderStore` retains one render node per transcript node and one lightweight
container per group. Applying a `TranscriptChangeSet` updates only referenced
nodes. Nodes expose measurement and row rendering over bounded ranges.

The render tree owns structural correctness, source mappings, copy ranges,
selection metadata, and row extents. Terminal drawing consumes retained rows and
produces a cell diff.

### Lua contract

Lua remains an extension and presentation layer, not a payload transport layer.
The transcript renderer API is replaced with a typed layout-template contract:

- Lua receives compact node metadata, IDs, statuses, revisions, and presentation
  state;
- text leaves contain opaque `ContentId` and channel references;
- Lua may compose chrome, labels, gutters, caps, and child references;
- Rust resolves content leaves and bounded row ranges;
- Lua cannot receive or synchronously materialize complete output as JSON;
- any preview exposed to Lua has a strict byte and row bound established before
  callback dispatch.

Built-in templates are compiled once per relevant metadata revision. Appending
content to an existing leaf does not rerun structural Lua layout unless metadata
that controls structure changed.

Delete full transcript block snapshots, group snapshots, source-view mirrors,
and payload-derived display hashes once this contract is active.

### Computational caps

A cap is part of the row query. Measuring a 20-row tail asks the child index for
at most those rows plus a bounded marker context. It must not measure the full
child first. Nested prefixes, gutters, and decorations operate on the same
bounded range rather than materializing scratch copies of complete children.

### Groups

A group retains child IDs, ordering, and per-child render entries. Its revision
changes structurally only when membership, order, or group presentation changes.
A child append updates that child's content and extent contribution. Unchanged
siblings are not serialized, hashed, measured, or rendered.

## Scheduling and frame pipeline

### Semantic frame scheduler

All continuation deltas enter one pending mutation queue. The scheduler permits
at most one ordinary continuation frame per display interval and drains all
pending mutations before that frame.

Urgent frames are limited to events with immediate semantic or interactive
value:

- first visible output;
- completion and error transitions;
- permission and confirmation requests;
- user input, navigation, and resize;
- explicit foreground status changes.

Tool output, local exec output, draft continuation, assistant text continuation,
and reasoning continuation share the same coalescing policy. Backpressure bounds
queued bytes and parsing work without dropping content.

### Frame stages

A frame performs these bounded stages once:

1. apply pending changes to retained render nodes;
2. update dirty extent and row-index entries;
3. resolve the viewport from retained extents;
4. render dirty intersecting row ranges;
5. update committed-view metadata from retained semantic neighbors;
6. paint a terminal cell diff.

Every stage reports changed IDs and ranges. There is no global transcript cache
clear.

### Animation isolation

Animation state lives outside transcript content and layout revisions. A spinner
or elapsed-time tick updates only the small visible decoration that displays it.
If no visible animated decoration changes cells, no compositor frame is needed.

## Sparse and persisted transcripts

### Payload-independent extent index

Persistence stores a compact `RecordProfile` beside each transcript record. It
contains semantic kind, role, logical line facts, byte and cell summaries, and
other payload-independent data needed for row estimation.

Profiles are aggregated in a persisted prefix tree or equivalent ordered index.
Range and prefix extent queries read aggregate profiles in `O(log records)` plus
a bounded number of profile rows. Partial boundaries never deserialize transcript
payloads.

The runtime retains one width-keyed extent view per immutable transcript root and
active loaded range. Loaded-prefix, scrollbar-total, viewport planning, and
scroll anchoring share that view.

### Semantic navigation index

Persistence maintains ordered indexes for role and semantic block kind. Nearest
previous and next queries return record offsets and compact metadata in
`O(log records)` without loading intervening records.

A committed transcript view includes bounded previous and next navigation targets
for built-in observers. Lua view watchers cannot initiate synchronous history
scans.

### Hydration service

Each open sparse session owns two purpose-specific persistent lineage readers or
equivalent pooled immutable store handles. The transcript document's metadata
reader answers payload-free index and extent queries. The hydration worker's
reader serves bounded payload reads off the UI thread, merging nearby requests and
prioritizing the viewport.

Frame preparation and committed-view callbacks may request hydration but cannot
open stores or wait on unbounded reads. Explicit ID sets are submitted as one
worker request, coalesced into bounded canonical ranges, and installed under the
same hydrated-content budget as viewport work. A request larger than cache
capacity succeeds when worker processing completes; LRU eviction may prevent all
requested blocks from remaining materialized simultaneously. Changing an
immutable store address explicitly replaces both retained readers and invalidates
only indexes tied to the old root.

## Simplicity and deletion plan

The new architecture should reduce concepts, not add a cache beside every old
abstraction. The final implementation has:

- one transcript node model;
- one chunked content store;
- one typed mutation/change-set path;
- one retained render store;
- one row/extent query abstraction for live and sparse content;
- one semantic frame scheduler;
- one hydration service per session.

Delete these obstructing mechanisms after their replacements land:

- complete `ToolState` display hashing for render invalidation;
- full block and group JSON snapshots for Lua layout;
- mirrored Lua source views containing complete live payloads;
- clone-and-replace local exec appends;
- per-character cloned draft preview values and final replay;
- group cache keys derived from complete child payloads;
- full-child measurement before visual capping;
- row-range rendering that rescans ANSI content from the start;
- urgent scheduling of continuation-only tool output;
- duplicate sparse prefix and total reconstruction;
- payload deserialization for extent boundaries;
- synchronous 256-record semantic navigation scans;
- per-operation lineage reader opens in live session paths;
- animation-driven transcript projection invalidation;
- manual broad cache-clear calls made unnecessary by change sets.

Do not retain deprecated aliases or compatibility branches for removed internal
APIs. Update built-in Lua, generated API documentation, tests, and examples in
the same slice that changes a contract.

## Implementation sequence

The sequence is architectural, but each phase must end with one production path
and passing tests.

### Phase 1: canonical mutations and content

- Introduce the shared chunked content store and typed transcript mutations.
- Move provider output and local exec to ownership-transferring append operations.
- Replace payload-derived render invalidation with stable revisions.
- Preserve all transcript semantics and persistence receipts.
- Delete clone-and-replace append paths as each producer migrates.

### Phase 2: drafts and groups

- Replace draft preview cloning with retained incremental parser state.
- Make final draft arguments reuse streamed parser/content state.
- Store group membership by child identity and revision.
- Remove whole-group snapshots and payload-derived group keys.

### Phase 3: retained renderer and Lua contract

- Introduce stable retained render nodes and typed content leaves.
- Replace full-payload Lua snapshots with compact metadata templates.
- Compile structural templates only on metadata revisions.
- Move source mapping, copy metadata, and bounded row resolution into Rust.
- Delete the old snapshot renderer and source-view mirrors.

### Phase 4: bounded row indexes and caps

- Incrementally retain line, ANSI, Markdown suffix, and visual-row checkpoints.
- Make all measurement and rendering APIs range-based.
- Make caps, prefixes, gutters, and nested layouts preserve bounded row work.
- Delete full-child cap measurement and prefix rescans.

The Phase 4 implementation carries a retained `MeasuredLayout` through every
production viewport row query. Vboxes, Hboxes, panels, prefixes, gutters, and
style wrappers consume child measurements from that plan and cannot fall back to
complete child measurement. One-off complete previews use an explicit complete
render mode instead of the viewport API. Caps resolve retained text, Markdown,
code, file, and nested layout edges directly. Transient cap children are accepted
only within 64 KiB, 4,096-node, 4,096-span, and 32-level budgets; larger content
uses a bounded omission row.

Retained content diffs compare chunk streams, narrow compilation by common byte
prefix and suffix, expand only to complete lines plus three context rows, and
materialize only that changed window. On the 20,000-line and 80,000-line release
fixtures, first-render allocation was 3,948,287 and 3,948,438 bytes respectively,
while full retained rows remained zero. This validates allocation independence
from unchanged source size; detailed commands and timing are recorded in
`docs/transcript-layout-benchmarks.md`.

Provider output now transfers into one shared source allocation and exposes
UTF-8-safe ranged slices to the retained content store. The first slice is
available immediately; output beyond the 4 MiB ingestion budget advances one
slice per semantic render boundary. Tool completion, history replacement, and
terminal turn events remain ordered behind pending slices. Cancellation and
transcript clearing discard both pending slices and deferred lifecycle events.
Final provider output reuses the streamed content identity instead of replaying
its complete payload.

Text layouts retain line extents plus at most 64 visual tail rows per width and
ANSI mode. An append invalidates cached rows only from the changed logical line
onward. Cap selection metadata stays stack-bounded, non-truncated caps render
directly, and visible block-layout and row-identity capacities are reused across
projections. These bounds do not retain complete visual rows or eagerly measure
an appended payload.

The final optimized release measurements passed all three provider gates:

- The warmed 64 KiB, 2,048-chunk workload used 131 frames, allocated 16,273,090
  bytes in the measured hot path, and recorded 0.260/0.390/2.540 ms frame
  p95/p99/maximum. This is 504,126 bytes below the 16 MiB limit.
- The 8 MiB, 128-chunk workload used 11 frames, allocated 5,708,304 bytes, and
  recorded 2.107 ms frame p95, p99, and maximum.
- The single-event 96 MiB workload was indexed in 4 MiB slices over 27 frames.
  It allocated 31,772,080 bytes in total and recorded
  14.800/15.357/15.357 ms frame p95/p99/maximum. No ingestion, compositor, or
  transcript-projection frame approached the 33 ms limit.

The final Phase 4 correctness matrix passed 5,126 workspace tests with 2 skipped.
Strict workspace clippy, formatting, Lua API generation, the `release-fast` smelt
build, and `git diff --check` passed. Line coverage was 86.33 percent against the
80 percent gate, and no generated `.snap.new` files remained.

### Phase 5: semantic scheduler and compositor

- Route every continuation producer through one coalescing scheduler.
- Apply pending changes once per frame.
- Separate animation, content, metadata, structure, and viewport invalidation.
- Render dirty viewport ranges and paint terminal row/cell diffs.
- Delete continuation-specific urgent paths and broad cache clears.

Production autonomous work now enters one typed frame-boundary queue. Continued
provider text, reasoning, tool drafts, tool output, auxiliary engine asks, and
local exec output share that queue. The first visible output remains synchronous
and urgent. Auxiliary asks retain an explicit per-request continuation identity
because idle asks can update Lua UI without creating parser-owned transcript
blocks. Local exec start is likewise urgent because it creates the visible block;
append, finish, and finalize mutations stay ordered in the shared queue.

At compositor frame start, the queue is drained exactly once before animation,
signal, layout, and paint work. The release benchmark records
`transcript:pending_work:applied` once per traced frame and asserts the
counts match. Completion, errors, and other urgent lifecycle events apply pending
continuations before their visible effects, so completion cannot overtake output.
Cancellation, rewind cancellation, and transcript clearing use one pending-work
discard operation for continuation mutations, provider slices, and deferred
lifecycle events. The obsolete benchmark-only exec scheduler override was deleted.

Typed transcript patches keep invalidation semantic. Content appends update
retained byte ranges and changed nodes; metadata and structure revisions invalidate
only their affected retained state; viewport changes retain source and measurement
ownership; animation patches never enter transcript scene or sparse projection.
Broad clears remain only at renderer or policy contract changes, complete source
replacement, width changes, theme changes, and full reset boundaries. Timed
renderer refreshes now resolve dirty height nodes through the retained scene index
and clear the bounded complete-width snapshot set instead of scanning every prior
node. That removed the only measured streaming-frame cost proportional to a
10,000-block in-memory history.

Dirty viewport rows continue through the retained projection and terminal's
existing double-buffered compositor. `Ui::flush_prepared_frame` forwards the grid
to `Compositor::flush_frame`; `Grid::diff` yields changed cells and `flush_diff`
emits only those cells. No parallel transcript compositor or full-grid terminal
paint path was introduced.

The final Phase 5 release scheduler measurements were:

- A 64 KiB provider response in 2,048 chunks used 131 frames. The final narrow
  typed-queue sample allocated 16,321,104 bytes, 456,112 bytes below 16 MiB, with
  0.378/0.430/2.199 ms frame p95/p99/maximum. Every frame recorded one queue
  application pass and 130 frames performed transcript projection.
- The equivalent local exec stream used 130 frames, allocated 10,674,526 bytes,
  and recorded 0.219/0.343/0.568 ms frame p95/p99/maximum.
- Adding 120 spinner-only frames increased compositor frames from 131 to 251 while
  transcript projection, viewport planning, sparse planning, and scene refresh
  remained at 130. The idle segment performed no transcript work.
- The 8 MiB, 128-chunk provider workload used 11 frames and recorded
  4.663/4.663/4.663 ms frame p95/p99/maximum. The single-event 96 MiB workload
  advanced over 28 bounded frames and recorded 13.158/15.528/15.528 ms. Both
  remained below the frame gates.
- Across warmed samples, increasing prior history from 5 to 10,000 blocks changed
  mean frame cost from 0.233 to 0.222 ms. Mean frame p99 across three runs changed
  from 0.497 to 0.388 ms. Streaming frame cost therefore did not grow with prior
  transcript size.

The final Phase 5 correctness matrix passed 5,132 workspace tests with 2 skipped.
Strict workspace clippy, formatting, Lua API generation, the `release-fast` smelt
build, snapshot review, and `git diff --check` passed. Line coverage was 86.34
percent against the 80 percent gate, and no generated `.snap.new` files remained.

### Phase 6: sparse indexes and hydration

- Persist payload-independent extent profiles and aggregate indexes.
- Persist semantic role/kind navigation indexes.
- Share retained sparse extent state across viewport consumers.
- Add persistent readers and merged asynchronous hydration.
- Remove payload-backed boundary estimation and synchronous navigation scans.

Schema version 3 stores one immutable transcript profile per payload and one
aggregate profile per persistent sequence node. Profiles contain bounded preview,
kind, broad semantic role, block bounds, estimated text bytes, and row extents at
the supported widths. Sequence aggregates retain record count, block and
canonical history-index bounds, kind/role masks, and summed extents. Range extent,
row lookup, previous/next kind or role, block lookup, and transcript-boundary
lookup descend the content-addressed sequence tree. They do not deserialize
transcript payloads or construct root-sized side indexes. Existing version 1 and
2 databases migrate transactionally: the migration validates the exact old
shape, backfills payload and node profiles, removes the old root-scoped extent
table when present, and advances both schema version markers. A corrupt payload
rolls the entire migration back after partial backfill work.

The transcript document retains one metadata reader for the immutable session
store address, while the persistent per-session hydration worker retains a second
reader for payload reads off the UI thread. Changing the store address explicitly
replaces both retained readers. Viewport,
semantic navigation, search, persistence, and rewind planning enqueue typed record
or block requests without opening a store or waiting for payloads during frame
preparation. The request queue merges adjacent windows and gives explicit semantic
requests priority. Worker results carry their document context and revision, so
stale results are rejected; missing or unreadable storage returns a typed terminal
failure instead of redispatching forever. Successful installation requests redraw
and consumes a pending projection restore only after planning succeeds. Sparse
prefix, scrollbar total, and row lookup share one retained width/root extent
result. Its ad hoc prefix and row-location query caches each evict after 256
entries, while exact observations remain limited to loaded record windows.

Navigation first locates one payload-free metadata target and then hydrates only
that target when the Lua contract requires a complete `StoredBlockWithId`.
Committed-view publication itself contains bounded metadata and never opens a
store. The former 64-record root extent chunks, root-sized prefix vectors,
payload-backed extent boundaries, and 256-record navigation scan are deleted from
the production schema and TUI path.

The optimized Phase 6 sparse-index matrix completed every lookup with zero
payload hydration:

| Records | Previous kind p99 | Next role p99 | Block lookup p99 | Row lookup p99 | 129-record extent p99 | Total extent p99 |
|---:|---:|---:|---:|---:|---:|---:|
| 10,000 | 391 us | 419 us | 921 us | 997 us | 909 us | 18 us |
| 100,000 | 222 us | 200 us | 553 us | 542 us | 403 us | 10 us |
| 1,000,000 | 449 us | 251 us | 953 us | 904 us | 718 us | 14 us |

A 5,000-block committed-view watcher ran 1,001 dispatches at 450 us p99
and 1,725 us maximum with one metadata reader, one hydration reader, and no
additional store open. Over
600 warmed autonomous frames with equivalent 5 MiB visible history, hydrated
frames used 35,340 us of compositor time and allocated 22,936,808 thread bytes;
sparse frames used 35,792 us and 23,336,589 bytes. Sparse/hydrated ratios were
1.013 for compositor time, 1.005 for allocation count, and 1.017 for allocation
bytes, all within the 1.25 gate. The warmed sparse interval loaded zero transcript
payloads and did not replace or reopen either reader. A separate resumed 64 KiB
provider turn applied 2,049 events over 252 scheduled frames, including 120 idle
frames, while retaining both readers and making zero additional open attempts.

### Phase 7: consolidation

- Remove superseded types, APIs, metrics, feature flags, and dead tests.
- Rename remaining concepts around nodes, mutations, revisions, retained rows,
  extents, and hydration so ownership is obvious.
- Regenerate Lua stubs and reference documentation.
- Add architecture invariants and focused regression tests.
- Run the complete correctness and performance matrix.
- Record final measurements and mark this objective complete only after all
  obsolete paths are gone.

#### Final architecture and ownership

The consolidated production path has one owner for each kind of state:

- `Transcript` owns stable typed nodes, while `TranscriptContent` owns shared
  append-only chunks, revisions, parser state, logical lines, and bounded retained
  layout caches.
- Typed transcript operations and `TranscriptPatchOperation` form the canonical
  write and retained-change vocabulary. Provider, auxiliary ask, local exec, draft,
  lifecycle, persistence, rewind, and cancellation paths apply typed changes
  instead of replacing accumulated snapshots.
- The retained transcript scene owns structural layout, measurements, row identity,
  and bounded visible materialization. Lua receives compact typed metadata and
  opaque retained content references, not complete transcript payloads.
- One indexed `TranscriptWorkQueue` owns autonomous provider, auxiliary, tool,
  reasoning, and local-exec work in insertion order. The compositor drains it once
  per semantic frame before projection; incremental indexes provide pending-tool
  lookup and main-turn cancellation without repeated queue scans. Animation state
  is independent of transcript revisions.
- Schema version 3 owns immutable payload profiles and persistent sequence-node
  aggregates. Extent, row, block, canonical history-boundary, role, and kind
  queries descend that sequence without loading payloads or building a root-sized
  index.
- Derived search format 2 owns immutable FTS and short-posting segments plus an
  atomically published ordered manifest keyed by canonical transcript-root ID.
  Forks and rewinds reuse manifests for shared roots. Candidate queries validate
  root identity without walking canonical leaves, lazily reconstruct only selected
  source segments for exact literal verification, and fall back to canonical
  direct search when projection data is missing, stale, corrupt, or incompatible.
- Each active sparse document owns one persistent metadata reader, while its
  background hydration service owns one persistent payload reader and one bounded
  request queue. Both share one immutable store address and one width/root extent
  view. Prefix
  and row-location caches are independently capped at 256 entries, and retained
  text tails are capped at 64 visual rows per width and ANSI mode.
- Transcript ownership is split into focused internal modules for hydration memory,
  store and reader caching, sparse records, sparse extents, display adaptation,
  resume-preview caching, and application integration. Retained layout separately
  owns horizontal composition, cap-row selection, and style/panel chrome. The root
  modules coordinate those owners while each extracted subsystem retains its own
  state and invariants.

#### Post-reflection architecture closure

A final architecture reflection identified four consolidation gaps. All four are
closed in the accepted path:

1. Payload hydration no longer runs synchronously from compositor preparation.
   `TranscriptHydrationWorker` is a persistent per-session background service with
   coalesced requests, context/revision rejection, terminal failure results, and
   UI-thread result installation.
2. Harness settling no longer activates a `cfg(test)` direct-read service. It
   submits requests to the production worker, waits for production results, and
   enforces the unchanged 16-round convergence limit. Deferred hydration behavior
   in correctness tests is therefore production-identical.
3. Provider continuations, auxiliary continuations, tool-output appends, deferred
   reasoning summaries, ordered lifecycle events, and local-exec work use one
   insertion-ordered queue. Incrementally maintained invocation and main-turn
   indexes replace the former coordinated queues and repeated application scans.
4. The transcript and retained-layout implementations are divided by ownership:
   `transcript/{memory,storage,sparse_records,sparse_extent,adapters,app_integration}.rs`
   isolate memory policy, persistence, sparse planning, display consumers, and
   application lifecycle/view integration;
   `layout_ir/{hbox,cap_rows,chrome}.rs` isolate horizontal layout, bounded cap
   selection, and style/panel composition. This leaves document and layout
   orchestration explicit instead of mixing every subsystem in one root file.

Main transcript hydration and resume-preview hydration deliberately remain
separate workers. Preview cancellation cannot interfere with the active document,
and preview completion commits directly on the UI thread and requests redraw
without Lua polling.

#### Deleted superseded paths

The final implementation removes complete tool and group JSON transport to Lua,
payload-derived display hashes, source-view payload mirrors, clone-and-replace
provider and local-exec appends, per-character draft value cloning and final
replay, whole-child group cache keys, full-child cap measurement, ANSI prefix
rescans, continuation-specific urgent scheduling, duplicate sparse prefix/total
reconstruction, payload-backed extent boundaries, the 256-record navigation scan,
per-frame reader opens, and animation-driven transcript projection. The former
root-scoped 64-record extent table exists only as version 1 migration vocabulary;
the current migration drops it transactionally. A final symbol and metric scan
found no production prefix-index type, old extent chunk type, obsolete renderer mirror, or
obsolete navigation and extent metric.

Explicit complete materialization remains only in read-only fallback and test or
old in-memory load seams that request a complete session. Normal store-backed
resume, render, save, rewind, and fork do not enter those seams, and focused
regressions fail if they do. They are not reachable from autonomous transcript
frames, committed-view callbacks, sparse planning, or hydration.

#### Final before and after results

The complete release results and raw reproduction commands are recorded in
`docs/transcript-layout-benchmarks.md`. The principal comparisons are:

| Workload | Investigation reference | Final retained result |
|---|---:|---:|
| Provider bash, 64 KiB | 512 chunks: 3.80-3.85 s, 3.99 GB, 14.58-14.80 ms frame p99 | 2,048 chunks: 32.805 ms, 16,325,277 bytes, 0.414 ms frame p99 |
| Provider bash, 8 MiB / 128 chunks | 153.37 ms frame p99, 7.85 GB allocated | 3.406 ms frame p99, 5,754,773 bytes allocated |
| Provider bash, 96 MiB / one chunk | 2.088 s worst frame, 981 MB worst-frame allocation | 29 bounded frames, 11.889 ms maximum frame, 31,951,620 bytes total allocation |
| Sparse autonomous, 600 frames | 1.573 s, 1.220 GB allocated | 35.737 ms, 23,336,589 bytes, zero payload loads |
| 5,000-block committed-view watcher | 142.0 ms, 534 MB, up to 256 payload records | 509 us p99, two retained readers, zero measured-interval opens, metadata-only dispatch |
| Sparse semantic lookup, 1,000,000 records | Payload-backed linear fallback | At most 747 us p99, zero payload loads |
| Indexed search, 500 MiB | Canonical source reconstruction per candidate page: 404.847 ms rare, 404.268 ms absent, 409.717 ms after append | Format-2 root manifest: 199.065 ms rare with 512 returned candidates, 45.224 ms absent, 15.312 ms after append |
| Active retained memory, 500 MiB | Committed content could remain proportional to transcript bytes | Zero live committed content, 33,533,570 hydrated bytes under budget, 55,967,090 allocator retained-byte delta |

The post-reflection sparse/hydrated ratios are 0.996 for compositor time, 1.005
for allocation count, and 1.017 for allocation bytes. In the final three-run
history comparison, growth from 5 to 10,000 prior blocks changed warmed mean frame
cost from about 0.218 to 0.238 ms, a 9.2 percent increase against the 10 percent
gate; mean p99 remained effectively flat at 0.426 and 0.427 ms. Adding 120
animation-only frames added no transcript projection, sparse planning, hydration
planning, structural compilation, payload hashing, or storage reads. Retained diff
first-render allocation was 3,985,838 bytes at 20,000 lines and 3,985,989 bytes at
80,000 lines, a 151-byte difference; warm renders were 0.070 and 0.071 ms and full
retained rows remained zero.

Search projection setup is explicitly outside measured interaction intervals.
Every indexed release sample proves that both the persisted candidate index and
current content-addressed root manifest were used. The 50 and 500 MiB
active-memory gates submit all 1,200 adversarial hydration IDs in one request,
settle in eight and seven canonical ranges respectively, preserve zero working-set
rereads, and keep hydrated content below the 32 MiB budget through normal LRU
eviction.

The 96 MiB provider event is transferred once and advanced in UTF-8-safe 4 MiB
slices at semantic render boundaries. This is the only payload-size ingestion
bound. It preserves urgent first output and lifecycle ordering while preventing an
arbitrary provider event from monopolizing one frame. It is retained as a
production safety bound, not as a compatibility path.

#### Final correctness and release validation

The post-reflection workspace matrix passed 5,167 tests with 3 skipped. Line
coverage is 86.25 percent against the 80 percent gate. Strict full-workspace clippy,
the explicit `harness,transcript-bench` clippy gate, formatting, Lua API generation,
permission and transcript/tool storybooks, the optimized `release-fast` smelt
build, and `git diff --check` passed. Changed storybooks were manually inspected,
and no generated `.snap.new` files remain. Sparse scale, watcher, autonomous
comparison, provider sizing, draft, group, history scaling, animation isolation,
retained-reader, and retained-diff release benchmarks comprise the final
performance matrix.

## Performance acceptance gates

All gates use optimized release builds, the production scheduler, a silent real
terminal renderer, deterministic workloads, and allocation counters. Setup is
outside the measured interval.

### Frame and scheduling gates

- Ordinary autonomous streaming: below 16.7 ms p95 and 33 ms p99.
- At most one ordinary continuation frame per display interval.
- First output, completion, permissions, errors, and interactions remain urgent.
- Prior history growth from 5 to 10,000 blocks changes warmed streaming frame cost
  by no more than 10 percent.
- Spinner-only frames perform zero transcript projection, sparse planning,
  structural Lua compilation, payload hashing, or storage reads.

### Provider and exec gates

- A 64 KiB response in 2,048 chunks allocates less than 16 MiB in the warmed
  mutation, render-store, layout, and projection hot path, excluding fixture and
  terminal capture setup.
- An 8 MiB response in 128 chunks stays below 16.7 ms frame p95 and 33 ms p99.
- A single 96 MiB provider chunk causes no compositor or transcript-projection
  frame above 33 ms. Ingestion and indexing proceed in bounded work slices.
- Retained allocation after completed output is no more than content bytes plus
  bounded indexes and metadata. Transient allocation is independent of event
  count times accumulated output.
- Local exec and provider tool output use the same append algorithm and scale
  linearly with newly received bytes.

### Draft and group gates

- Streamed draft parsing is linear in argument bytes. A 1 MiB string argument
  allocates no more than three times input size in its mutation/parser hot path
  and has no dispatch span above 10 ms.
- Finalizing a draft does not replay or clone the complete streamed argument.
- Updating one child in an eight-child, 1 MiB-per-child group has frame and
  allocation cost independent of unchanged sibling payload sizes.
- The eight-child grouped workload stays below 16.7 ms p95 and 33 ms p99 with
  less than 16 MiB transient frame allocation.

### Sparse gates

- After warmup, sparse autonomous frames are within 25 percent of the equivalent
  hydrated frame CPU and allocation totals.
- Sparse prefix and scrollbar-total consumers share one retained extent result.
- Extent planning performs zero transcript payload deserializations.
- Previous/next role or kind navigation completes below 2 ms p99 and loads zero
  intervening payload records for 10,000, 100,000, and 1,000,000-record fixtures.
- Committed-view watcher dispatch completes below 2 ms p99 and performs no store
  open.
- A resumed autonomous provider turn keeps one metadata reader and one hydration
  reader per immutable session store address. Neither reader's open-attempt count
  changes with provider event count or frame count.

### Scaling gates

The measured implementation must demonstrate these bounds:

- append mutation: `O(new_bytes)`;
- frame scheduling: `O(display_intervals)`, not `O(events)`;
- active node preparation: `O(changed_metadata + changed_visible_rows)`;
- draft parsing: `O(new_argument_bytes)`;
- grouped update: `O(changed_children)` plus changed visible rows;
- capped rendering: `O(requested_rows + bounded_checkpoint_span)`;
- sparse extent query: `O(log records)` plus bounded profile reads;
- semantic navigation: `O(log records)`;
- projected search root validation: `O(1)`, followed by work proportional to
  manifest segments, posting cardinality, and the bounded candidate page rather
  than canonical transcript leaves;
- animation update: `O(visible_animated_decorations)`.

## Correctness gates

- Existing transcript, provider, exec, draft, group, permission, copy, selection,
  rewind, persistence, resume, search, and navigation tests pass.
- Storybook snapshots are intentionally reviewed and updated for any renderer
  contract change.
- UTF-8 offsets use the shared `smelt_buffer::text` and attached-text APIs.
- ANSI styles, hyperlinks, source ranges, Markdown boundaries, collapsed views,
  and copy semantics remain correct across chunk boundaries.
- Scrolling does not jump when exact sparse extents replace estimates.
- Rewind, compaction, hydration, and eviction cannot leave stale render nodes or
  content references.
- Lua callbacks cannot retain invalid content handles or trigger unbounded reads.
- Memory budgets account for chunks, checkpoints, retained rows, sparse profiles,
  and hydrated payloads, and all caches have explicit eviction ownership.

## Validation commands

```bash
cargo fmt -- --check
cargo nextest run --workspace --features smelt-tui/harness
cargo clippy --workspace --all-targets --features smelt-tui/harness -- -D warnings
cargo build --profile release-fast --bin smelt
cargo llvm-cov nextest --workspace --features smelt-tui/harness --fail-under-lines 80
cargo xtask gen-lua-docs
```

Run transcript storybook snapshots whenever transcript scenes, renderers, Lua
layout templates, permissions, or committed-view UI change. Run the release
streaming and sparse matrix after every phase and the full acceptance matrix in
Phase 7.

## Required final report

The completion report must include:

- the final architecture and ownership map;
- all deleted legacy mechanisms and APIs;
- before/after CPU, frame, allocation, reader-open, and scaling results;
- acceptance-gate results and raw reproduction commands;
- correctness, lint, coverage, snapshot, and release-build validation;
- any remaining bound with a precise reason and follow-up objective.

No objective may be called complete while a known full-state, payload-backed,
unbounded synchronous, or frame-per-event path remains in autonomous transcript
work.
