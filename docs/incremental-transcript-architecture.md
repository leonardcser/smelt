# Incremental transcript architecture

The transcript pipeline is incremental and retained: work is proportional to new
data and changed visible rows, not to accumulated history, active tool output,
group size, or sparse session state. Tool output, local command output, streamed
tool arguments, persisted history, layout, and viewport projection all share one
bounded data flow.

Invariants:

- Live mutations are append-oriented typed operations over stable node
  identities; no continuation update replaces or snapshots accumulated output.
- The render path consumes retained node revisions and bounded source ranges,
  never a JSON copy of complete tool, exec, draft, or group state.
- Layout and projection touch only dirty visible nodes and requested rows. A
  visual cap is also a computational cap.
- Continuation events merge at the frame boundary. First output, lifecycle
  transitions, errors, permissions, and user interaction stay urgent.
- Draft parsing, ANSI decoding, line indexing, display hashing, and group
  aggregation are linear in bytes appended or children changed.
- Sparse extent and semantic navigation queries use retained or persisted
  metadata and never deserialize payloads to estimate rows or find a neighbor.
- Animation-only frames reuse transcript layout, extents, navigation, and rows.
- Frame preparation performs no synchronous storage opens or unbounded reads.

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

### No parallel paths

There is one renderer and one model: no compatibility mirror or shadow model. A
change that replaces a path removes the old one in the same commit.

## Data flow

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

## Ownership


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
- The lineage store owns immutable payload profiles and persistent sequence-node
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
