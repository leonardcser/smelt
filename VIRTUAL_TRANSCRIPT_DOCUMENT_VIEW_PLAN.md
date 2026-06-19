# Virtual Transcript Document and Document View Rewrite Plan

## Purpose

This plan is the second pass after `SESSION_STORE_TRANSCRIPT_REWRITE_PLAN.md`. The storage plan remains the foundation for per-session SQLite, request audit, object storage, import/export, and legacy compatibility. This plan supersedes the transcript/viewer portions of that first pass with one cohesive design:

1. Stop treating transcript as a big in-memory list of blocks.
2. Make transcript a virtual document backed by durable SQLite block records.
3. Load, layout, and render only the visible or semantically required ranges.
4. Promote `DisplayDocument` into the semantic viewer boundary for transcript, readonly buffers, and static row documents.
5. Route Vim motions, mouse selection, copy/yank, search navigation, actions, folds, anchors, and pin-to-bottom through document coordinates, not through a materialized buffer slice.

The target is not an optimization layer on top of the current model. The target is one document/view architecture where storage, logical document state, render cache, and window painting each have one owner.

## Principles

These follow the same principles as the session store rewrite plan.

1. **Greenfield posture**
   - We are not constrained by current internal transcript layouts, row-index caches, viewer mode flags, or host override APIs.
   - We may replace display caches, row caches, transcript projection APIs, and window interaction APIs without compatibility shims.
   - The compatibility promise remains loading old sessions through storage-boundary importers for the compatibility window.

2. **Right abstractions over small patches**
   - Do not add more `TRANSCRIPT_WIN` branches to paper over a missing document boundary.
   - Do not make `Window` the semantic owner of row-backed text.
   - Do not make `Buffer` both source truth and projected render cache.
   - Promote the existing `DisplayDocument` seam instead of inventing a sibling transcript trait.

3. **One owner per concept**
   - Durable transcript records live in `smelt-store` SQLite tables.
   - Logical transcript state lives in `TranscriptDocument`.
   - Viewer cursor, selection, anchor, and follow-tail state live in `DocumentViewState`.
   - Vim parsing emits semantic `DocumentCommand`s.
   - Rendering materializes rows into a bounded `RenderCache`.
   - `Window` paints cached rows and owns terminal geometry, not document semantics.

4. **Exact interaction correctness**
   - Visible rows are exact.
   - Cursor position, hit testing, visual selection, copy/yank, search match location, fold targeting, link/action hit testing, `gg`, `G`, page movement, and pin-to-bottom behavior are exact.
   - The only allowed approximation is global scrollbar extent and coarse scrollbar click/drag mapping before local refinement.
   - Approximate rows must never provide text, copy data, action targets, or semantic cursor positions.

5. **No duplicate half-abstractions**
   - Every introduced type must have an explicit replacement and deletion target.
   - Transitional adapters are allowed only at boundaries and must have removal criteria.
   - Do not leave `DisplayDocument`, `UiHost::display_rows_for_range`, materialized-row `Window` mode, and transcript-specific app snapping all claiming to own row-document behavior.

6. **Do worthwhile large work now**
   - If a rename, split, or deletion makes the final architecture easier to reason about, include it.
   - Avoid intermediary scaffolding whose only purpose is to make the old architecture survive longer.
   - Delete obsolete eager transcript rebuild and host-special-case paths once the replacement covers load, live append, render, search, copy, and tests.

7. **Plan is allowed to evolve**
   - Update this plan when implementation uncovers better facts.
   - Changes should be explicit and justified.

8. **Measure first, then delete obsolete optimizations**
   - Micro-optimizations added to make the old architecture tolerable are not sacred.
   - If a better owner or database query removes the need for a cache, fallback, or reuse branch, delete the old optimization instead of layering the new design on top.
   - Keep performance instrumentation, but remove duplicated runtime paths once measurements show the replacement covers the operation.
   - Prefer simple typed transactions and indexed SQL over in-memory mirrors of database state.

9. **No intra-plan compatibility debt**
   - This rewrite lands as one unreleased change. No agent is running against intermediate Phase 1, Phase 2, Phase 3, or Phase 4 database shapes.
   - Do not add legacy readers, schema migrations, format-version bumps, cache-version bumps, or compatibility shims for formats introduced earlier in this plan.
   - When schema, cache, index, or wire shapes introduced by this plan need to change, rewrite the original definition in place and keep its version at the plan baseline.
   - The only compatibility code allowed here is for data formats that existed before this plan started, and it must stay isolated at the storage boundary.

## Non-Negotiable Performance and Memory Invariants

These are design constraints, not optimization goals.

- Normal resume must not load the whole session into memory.
- Display-only resume and preview must load only the visible transcript range plus bounded overscan and semantic context.
- Request start must not clone full provider history.
- Saving at request start, engine history snapshots, tool-loop checkpoints, append, and rewind must be transactional suffix work against SQLite, not full-session serialization.
- Search must not layout or scan all display rows in large sessions.
- Copy cost must be proportional to the selected display content, not total transcript size.
- Resize must re-materialize the visible range only.
- Scrolling to a nearby row should be `O(visible + overscan)` after the local anchor is known. Arbitrary row or scrollbar jumps may use `O(log blocks + visible + overscan)` index work plus bounded local refinement.
- Opening, saving, appending, searching, copying, resizing, and painting must not be bounded by total session file size. They may be bounded by requested result size, selected content size, or intentionally configured prefetch limits.
- Scrollbar extent and coarse scrollbar landing are the only approximate operations. Every user-visible row, command result, selection, copy result, search target, fold target, and action target is exact.
- Explicit export, import, repair, and diagnostic commands may stream the whole session, but they must be named as such and stay out of hot resume/render/request paths.

## Current Branch Checkpoint

The branch was rebased onto `main` at `520e1325`, then added store, history, and transcript-document hardening commits through `a4dc0541`.

Relevant completed changes that shape the remaining plan:

- `777d8663 fix(session): preserve in-flight request history` made request-start history durable. User/process/command request items are appended before engine dispatch, transcript blocks are tagged with their future `History(idx)` origin, and engine dispatch receives pre-request history to avoid duplicating the current input.
- The same commit fixed history suffix persistence so truncating history by index no longer deletes transcript blocks by mismatched `block_idx`. Final storage APIs must preserve this rule: originated transcript rows are deleted by `history_idx`, while stale unoriginated descriptor tails are cleared only beyond the preserved originated range.
- `77323b61 feat(engine): load model history from store` and `17bbc0a0 refactor(history): share model history sources` moved normal interactive provider-history reads to store-backed sources. Explicit Lua, test, and debug callers can still request full materialization.
- `6f305b90 fix(store): save transcript descriptors transactionally` made history and descriptor suffix writes atomic.
- `bc4d6b2a perf(tui): bound model history message reads` bounded model history reads for runtime hooks.
- `41639f7c refactor(edit): narrow buffer display document adapter` clarified that buffer display adapters are not transcript ownership.
- `4c7fc1c4 refactor(transcript): promote document owner` made `TranscriptDocument` the long-lived TUI owner, renamed the borrowed adapter to `TranscriptDisplayDocument`, and moved transcript render cache ownership under the document.
- `c4304828 refactor(transcript): attach store identity to document`, `3df39b8d refactor(transcript): keep loaded store backing`, and `a4dc0541 refactor(transcript): own descriptor loading policy` attached `session_dir` and descriptor-window metadata to loaded transcript documents, added document-owned descriptor range reads, preserved store identity through full and tail SQLite transcript loads, and moved descriptor load policy out of `app/history.rs`.
- Resume/deferred load now can ask `TranscriptDocument` to reload descriptor windows from its store backing before falling back to rebuilding from the semantic session. That fallback is still a repair/import behavior that needs to leave hot paths.

Current seams that still violate the final constraints:

- Normal resume can still materialize full semantic session history in `load_session_snapshot` for non-preview paths.
- Some explicit Lua, test, and debug APIs still materialize `model_history()` as `Vec<HistoryItem>` when the caller asks for the whole model-visible history.
- `TranscriptDocument` is now the long-lived owner, but it still wraps a mostly eager `Transcript`/`BlockHistory` for rendering. It owns store identity, descriptor load policy, sparse descriptor ranges, and render cache, but not yet a virtual row index, folds, anchors, or payload hydration policy.
- Display-only preview and resume tail-load descriptors, but arbitrary scroll, copy, search refinement, folds, and anchors do not yet page sparse descriptor windows through the document owner.
- Search still has fallback paths that can scan display rows for non-transcript documents. Transcript search uses SQLite candidates first, then bounded refinement.
- `BufferDisplayDocument` remains as the non-transcript buffer fallback adapter. It should not become a transcript path.
- `save_session()` uses dirty suffix snapshots and a combined SQLite transaction for history plus descriptors, but full completion still requires typed append/rewind/checkpoint transactions that avoid constructing a `SessionSnapshot` for hot request paths.

## Measured Baseline After Rebase

Benchmarks were run on this rebased worktree with:

```text
cargo xtask bench-transcript-session --runs 1 --bytes 10485760
cargo xtask bench-transcript-session --runs 1 --bytes 52428800
```

Observed results:

| Target bytes | History items | Rows | Save | Descriptor load | First tail paint | Preview | Width change | Allocated bytes |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 10 MiB | 5,644 | 169,320 | 487.7 ms | 42.2 ms | 21.1 ms | 8.2 ms | 18.9 ms | 862 MB |
| 50 MiB | 28,206 | 846,180 | 2,314.5 ms | 395.2 ms | 115.3 ms | 31.2 ms | 102.9 ms | 4.2 GB |

Interpretation:

- Save is roughly linear in session size and already too slow at 50 MiB. At 500 MiB or 1 GiB it would be unusable.
- Allocation volume is roughly 80x the target content size. That means memory pressure will dominate before database size does.
- Descriptor load is still scaling with the total descriptor set for full resume paths.
- Width change is still scaling with too much transcript state.
- Preview is comparatively better because it tail-loads a bounded descriptor window. That is the direction all viewing paths should follow.
- The current benchmark does not yet measure search latency, provider-history rows read, SQLite row counts, or bytes hydrated per operation. The benchmark itself must be extended before implementation claims success.

## 500 MiB Benchmark Checkpoints

These runs use the dedicated large-session benchmark knobs with `TMPDIR=/home/dev/tmp` so SQLite and temp files stay off the small `/tmp` tmpfs.

### Descriptor-backed resume

Command:

```text
TMPDIR=/home/dev/tmp SMELT_TRANSCRIPT_RESUME_BENCH_BYTES=524288000 \
  cargo test -p smelt-tui --release --lib transcript_true_resume_benchmark_suite -- \
  --ignored --nocapture --test-threads=1
```

Result:

```text
TRANSCRIPT_TRUE_RESUME_SAMPLE mode=descriptor_backed target_bytes=524288000 generated_bytes=524288000 descriptors=128000 rows=7551997 setup_ms=15164.961 tail_load_ms=0.860 tail_render_ms=1.557
TRANSCRIPT_TRUE_RESUME_JSON {"type":"resume_summary","mode":"descriptor_backed","target_bytes":524288000,"generated_bytes":524288000,"descriptors":128000,"rows":7551997,"setup_ms":15164.961,"tail_load_ms":0.860,"tail_render_ms":1.557}
```

The fixture now writes the descriptor-backed SQLite state directly. The timed resume path loaded only the tail descriptor window (`descriptor_slice_requested=80`, `descriptors_loaded=80`) and did not rebuild full in-memory history.

### Transcript search before append-path render-plan reuse

Command:

```text
TMPDIR=/home/dev/tmp SMELT_TRANSCRIPT_BENCH_SEARCH=1 \
  SMELT_TRANSCRIPT_BENCH_SEARCH_BYTES=524288000 \
  SMELT_TRANSCRIPT_BENCH_NO_WARMUP=1 SMELT_TRANSCRIPT_BENCH_RUNS=1 \
  cargo test -p smelt-tui --release --lib transcript_layout_search_benchmark_suite -- \
  --ignored --nocapture --test-threads=1
```

Baseline after store-backed candidate paging:

```text
TRANSCRIPT_SEARCH_BENCH_SAMPLE run=1 bytes=524290206 rows=6413965 rare_ms=525.031 common_submit_ms=179.758 next100_ms=615.217 after_append_ms=604.984
TRANSCRIPT_SEARCH_BENCH_JSON {"type":"search_summary","runs":1,"bytes":524290206,"rows":6413965,"rare_mean_ms":525.031,"rare_stddev_ms":0.000,"common_submit_mean_ms":179.758,"common_submit_stddev_ms":0.000,"next100_mean_ms":615.217,"next100_stddev_ms":0.000,"after_append_mean_ms":604.984,"after_append_stddev_ms":0.000}
```

Store candidate scans were bounded (`search_candidate_rows_scanned=512` for rare/common searches), but after appending one matching row the search path still rebuilt full render-plan nodes and row-index state:

```text
TRANSCRIPT_SEARCH_PERF_DURATION label=after_append metric=transcript:render_plan count=1 last_us=365199
TRANSCRIPT_SEARCH_PERF_DURATION label=after_append metric=transcript:render_plan:build_nodes count=1 last_us=306621
TRANSCRIPT_SEARCH_PERF_DURATION label=after_append metric=transcript:prepare_row_index count=6 total_us=424167 p95_us=424137
TRANSCRIPT_SEARCH_PERF_DURATION label=after_append metric=search:transcript:candidate_layout count=1 last_us=135052
```

### Transcript search after append-path render-plan reuse

Render plans now distinguish content generation from transcript order generation and extend append-only plans in place. Grouped plans rebuild only the suffix that could merge with the appended run; pure appends update the plan fingerprint incrementally instead of hashing every node.

Result:

```text
TRANSCRIPT_SEARCH_BENCH_SAMPLE run=1 bytes=524290206 rows=6413965 rare_ms=144.308 common_submit_ms=156.722 next100_ms=331.318 after_append_ms=170.674
TRANSCRIPT_SEARCH_BENCH_JSON {"type":"search_summary","runs":1,"bytes":524290206,"rows":6413965,"rare_mean_ms":144.308,"rare_stddev_ms":0.000,"common_submit_mean_ms":156.722,"common_submit_stddev_ms":0.000,"next100_mean_ms":331.318,"next100_stddev_ms":0.000,"after_append_mean_ms":170.674,"after_append_stddev_ms":0.000}
```

The measured append search bottleneck moved out of full render-plan construction:

```text
TRANSCRIPT_SEARCH_PERF_DURATION label=after_append metric=transcript:render_plan count=1 last_us=2
TRANSCRIPT_SEARCH_PERF_DURATION label=after_append metric=transcript:render_plan:append_nodes count=1 last_us=1
TRANSCRIPT_SEARCH_PERF_DURATION label=after_append metric=transcript:render_plan:fingerprint_append count=1 last_us=0
TRANSCRIPT_SEARCH_PERF_VALUE label=after_append metric=transcript:render_plan:reused count=1 last=1
TRANSCRIPT_SEARCH_PERF_DURATION label=after_append metric=transcript:prepare_row_index count=6 total_us=40075 p95_us=40049
TRANSCRIPT_SEARCH_PERF_DURATION label=after_append metric=search:transcript:candidate_layout count=1 last_us=99766
```

Remaining work: candidate layout still prepares an index for the full loaded render plan and should move toward a sparse block-to-row lookup owned by the transcript document, not by a full in-memory projection.

## Current Violation Map

| Area | Current code | Violation | Final direction |
| --- | --- | --- | --- |
| Session load | `load_session_snapshot` reads all `history_items` into `Vec<HistoryItem>` | resume memory and latency scale with total session size | load metadata, descriptor windows, and bounded model-history cursors separately |
| Save decision | Dirty suffix markers and DB row hashes avoid unchanged-prefix writes, but hot paths still build `SessionSnapshot` suffix payloads | request-start/tool-loop durability can still allocate more than the exact typed delta | typed transactions for appended history, descriptor suffix, title/meta, checkpoint, turn meta, and accounting deltas |
| Save payload | History and transcript descriptor suffixes are saved together in one SQLite transaction | blob externalization and snapshot construction are still snapshot-shaped | explicit append/replace transactions over dirty rows and objects |
| Provider dispatch | Interactive `StartTurnPayload.history` uses `ModelHistorySource::Store`; engine reads the requested range from `session.db` | explicit Lua/test/debug callers can still request full model-visible history | keep materialization only for explicit APIs, and prefer store-backed message reads for runtime hooks |
| Transcript document owner | `TranscriptDocument` owns store identity, descriptor loading policy, sparse descriptor ranges, and render cache, but still delegates most row/index state to eager `Transcript`/`TranscriptProjection` | document ownership is real but not yet sparse enough for arbitrary large-session navigation | move virtual row index, folds, anchors, and payload hydration under `TranscriptDocument` |
| Transcript resume | tail resume loads bounded descriptor windows, while full resume still can load every descriptor and full session history | normal display resume can still scale with total transcript/session | open `TranscriptDocument` from metadata and sparse descriptor windows without full history or full descriptor hydration |
| Search storage | transcript search uses indexed SQLite candidate terms plus exact refinement | generic non-transcript document search can still scan display rows | keep transcript candidate paging; add document-level indexed search APIs for other document kinds |
| Search runtime | transcript search asks SQLite for candidate blocks before local display refinement | fallback scan paths remain for buffer documents | document-level search API with indexed implementations and bounded refinement |
| Generic document search | buffer fallback search can materialize row windows from row zero to total rows | full display-row scan for non-transcript documents | `BufferDisplayDocument` remains bounded to buffer fallback; indexed document implementations replace broader host scans |
| Projection cache | `build_rows`, `full_rows`, and projection-owned row vectors remain reachable for full-text consumers | easy accidental full materialization | remove from hot APIs; explicit export/debug command can stream instead |
| Schema | migration version exists but DB format has not shipped | carrying compatibility migrations would add complexity | reset/reshape schema freely before release; optimize for final query/write patterns |

## Current Seams to Promote or Delete

### Promote

- `DisplayDocument` in `crates/edit/src/row.rs` is the UI-facing document trait to finish.
- `DisplayRows`, `DisplayRow`, `DisplaySnapshot`, `DocPosition`, `DocRange`, and `TextRange` are the current basis for row-document coordinates.
- `RowTextState` in `crates/edit/src/window/row_text.rs` already keeps cursor and selection in document row coordinates. This should become `DocumentViewState`, not remain a window sub-mode.
- `ViewerCommand` and `resolve_row_document_viewer_command` in `crates/edit/src/window/row_text.rs` are close to the semantic command executor. They should be renamed, completed, and moved behind a document-view boundary.
- `TranscriptDocument` in `crates/tui/src/app/transcript.rs` is now the long-lived transcript owner. Promote it further from store-aware eager owner to sparse virtual document owner.
- `TranscriptDisplayDocument<'_>` is the intentionally borrowed per-render `DisplayDocument` adapter. Keep it only while `DisplayDocument` cannot be implemented directly on the long-lived owner without borrow conflicts.
- The current SQLite transcript descriptor and payload APIs are the storage seed for durable block records.
- The request-start durability path is the seed for the final transaction model, but its save/request payload implementation must become store-backed and bounded.

### Delete or demote

- `UiHost::display_rows_for_range` as the general document lookup path.
- Transcript-specific `TRANSCRIPT_WIN` branches in `crates/tui/src/app/ui_host.rs`.
- Transcript-specific cursor snapping in app event code.
- Any `Window` mode bit that means "this buffer is a materialized slice of a larger document".
- Readonly viewer branches that derive semantic motions and copy from backing buffer bytes when the active thing is a display document.
- Eager resumed `Session -> Transcript -> BlockHistory` reconstruction as the normal load path.
- Full transcript row vectors as the normal render/cache shape.
- Full-session fingerprints and full-history request payloads in hot save/dispatch paths.
- Full display-row scans for search in large sessions.

## Target Architecture

```text
Vim parser
    -> DocumentCommand
        -> DocumentViewExecutor
            -> DocumentViewState
            -> dyn DisplayDocument
                -> TranscriptDocument
                -> BufferDocument
                -> StaticRowsDocument
            -> RenderCache
                -> Window paint/layout
```

The pipeline is:

1. Resolve the document attached to the window through a document handle or document registry.
2. Parse key/mouse/search/fold actions into `DocumentCommand` values.
3. Execute the command against `DocumentViewState` and the document.
4. Ask view state for the row range needed for the current viewport, selection, search target, or anchor.
5. Materialize that exact row range from the document into `RenderCache`.
6. Paint from `RenderCache` into the window backing buffer or directly into the terminal layout layer.
7. Keep cursor, selection, yank flash, anchors, search match, and fold targets in document coordinates.

`Window` should know:

- focused or not
- terminal rectangle
- scroll gutter and content width
- backing buffer used by renderer/layout code
- document handle if the window displays a document

`Window` should not know:

- how to execute Vim word motions over a transcript
- how to exactify a transcript row range
- how to copy a row-document selection
- how transcript block ids map to SQLite rows
- whether a transcript block is loaded, estimated, folded, or hydrated

## Final Ownership Map

| Concept | Current owner | Final owner | Delete or demote |
| --- | --- | --- | --- |
| Durable transcript data | `Session.history`, eager `BlockHistory`, descriptor helpers | SQLite `transcript_blocks`, payload/object rows, and append/delete suffix APIs | Direct history-to-transcript rebuild as normal resume path |
| Runtime transcript document | `Transcript` plus TUI projection | `TranscriptDocument` | Host-side transcript row methods |
| Viewer cursor and selection | `Window::row_text` state | `DocumentViewState` | Window materialized-row semantic mode |
| Vim interpretation | mixed buffer and row-text commands | Vim parser emits `DocumentCommand`; executor handles document semantics | Viewer-command special cases tied to materialized buffers |
| Row materialization | backing `Buffer` source lines or transcript projection | `DisplayDocument::materialize` into `RenderCache` | `UiHost::display_rows_for_range` as lookup mechanism |
| Rendering cache | mixed transcript projection and buffer contents | bounded `RenderCache` keyed by document generation, width, style, and row range | full transcript row vectors |
| Readonly viewers | buffer text as source and semantic model | `BufferDocument` over a buffer, using document commands | byte-backed viewer semantics for display documents |
| Tests | app or window integration only | `StaticRowsDocument` plus focused executor tests | test-only host hacks |
| Provider context | `StartTurnPayload.history: Vec<HistoryItem>` and `model_history() -> Vec<HistoryItem>` | bounded store-backed model-history snapshot or cursor independent of display document | full-history clone seam |
| Incremental persistence | `save_session()` with dirty suffixes but full-session fingerprint and in-memory session history | SQLite transactions over dirty history/descriptor/object suffixes with DB revision checks | full-session serialization/diff/fingerprint in hot paths |

## Core Types

### `DocumentCommand`

Semantic commands produced after Vim parsing and mouse/action normalization.

```rust
enum DocumentCommand {
    MoveRows(isize),
    PageRows(isize),
    HalfPageRows(isize),
    ScrollRows(isize),
    Top,
    Bottom,
    GotoRow(RowIndex),
    GotoPosition(DocPosition),
    LineStart,
    LineEnd,
    WordForward(u64),
    WordBackward(u64),
    WordEnd(u64),
    StartVisual,
    StartVisualLine,
    ClearSelection,
    YankSelection,
    YankSelectionLinewise,
    YankLines(u64),
    Center,
    PanColumns(isize),
    MoveCursorCol(isize),
    OpenAction,
    ToggleFoldAtCursor,
    ExpandAtCursor,
    CollapseAtCursor,
    SearchNext,
    SearchPrev,
    FollowTail(bool),
}
```

The exact shape can differ, but the command must describe document intent, not buffer implementation details.

### `DocumentViewState`

The per-window state for a displayed document.

```rust
struct DocumentViewState {
    document: DocumentHandle,
    generation_seen: u64,
    cursor: DocumentCursor,
    selection: Option<DocumentSelection>,
    preferred_cell_col: Option<usize>,
    viewport_anchor: ViewportAnchor,
    follow_tail: bool,
    horizontal_offset: usize,
    search_state: DocumentSearchState,
    yank_flash: Option<DocumentYankFlash>,
}
```

Important rules:

- Cursor and selection are document coordinates.
- Viewport anchor is not just an absolute estimated row when a stronger anchor exists.
- Follow-tail is a view-state policy, not a transcript-specific app branch.
- Search state points at semantic matches and exact row positions after refinement.
- `generation_seen` lets the executor revalidate cursor, selection, and anchors after document mutation.

### `DocumentCursor` and anchors

```rust
enum DocumentCursor {
    Row(DocPosition),
    Block {
        key: BlockKey,
        row_offset: RowIndex,
        byte_col: usize,
    },
}

enum ViewportAnchor {
    Top,
    Bottom,
    Row(RowIndex),
    Block {
        key: BlockKey,
        intra_block_row: RowIndex,
    },
    SearchMatch(SearchMatchId),
}
```

`Row` is valid for simple documents. `Block` is preferred for transcripts because row estimates can shift as heights are exactified. The executor can convert between block and row positions by asking the document.

### `DisplayDocument`

The current trait should be promoted and extended only where semantics require it. Avoid making it a transcript-shaped trait.

Current core:

```rust
trait DisplayDocument {
    fn snapshot(&mut self) -> DisplaySnapshot;
    fn materialize(&mut self, range: Range<RowIndex>) -> DisplayRows;
    fn copy_range(&mut self, range: TextRange) -> Option<CopyOutput>;
    fn action_at(&mut self, pos: DocPosition) -> Option<SpanAction>;
}
```

Likely additions:

```rust
struct DisplaySnapshot {
    generation: u64,
    extent: DocumentExtent,
}

enum DocumentExtent {
    Exact { rows: RowIndex },
    Estimated { rows: RowIndex },
}

trait DisplayDocument {
    fn snapshot(&mut self) -> DisplaySnapshot;
    fn materialize(&mut self, range: Range<RowIndex>) -> DisplayRows;
    fn copy_range(&mut self, range: TextRange) -> Option<CopyOutput>;
    fn action_at(&mut self, pos: DocPosition) -> Option<SpanAction>;

    fn resolve_anchor(&mut self, anchor: ViewportAnchor) -> ResolvedAnchor;
    fn position_to_anchor(&mut self, pos: DocPosition) -> Option<ViewportAnchor>;
    fn command_capabilities(&self) -> DocumentCapabilities;
}
```

Rules:

- `materialize` returns exact rows for the requested range, or fewer rows if the range is outside the document.
- `copy_range` exactifies or streams the selected range. It never copies from estimated data.
- `action_at` uses exact materialized row text and action spans.
- `snapshot.extent` is the only place approximation is exposed, and only for scrollbar/scroll range decisions.
- Capabilities let the executor know whether folds, actions, search, and block anchors are supported without checking for transcript types.

### Document implementations

#### `TranscriptDocument`

```rust
struct TranscriptDocument {
    store: TranscriptStore,
    live_suffix: LiveTranscriptSuffix,
    loaded_blocks: LoadedRanges<BlockRecord>,
    row_index: VirtualRowIndex,
    block_view_state: BlockViewStateMap,
    render_cache: RenderCache,
    dirty_persistence: DirtyTranscriptSuffix,
}
```

Responsibilities:

- Load durable block descriptors and payloads from SQLite ranges and tails.
- Append live streaming blocks without forcing full transcript materialization.
- Persist only new or changed suffix records.
- Maintain folded/expanded state by stable block key.
- Maintain a virtual row index with exact rows for materialized regions and estimates elsewhere.
- Materialize visible, selected, searched, or action-target rows exactly.
- Support `Top`, `Bottom`, arbitrary row jumps, block-anchor resolution, and follow-tail.
- Stream large copy ranges by blocks/chunks instead of building a full transcript buffer.

#### `BufferDocument`

A document implementation over a real `Buffer`, used for readonly viewers that should still use the document command pipeline.

Responsibilities:

- Expose exact rows from buffer lines.
- Copy ranges using buffer text and existing copy semantics.
- Support Vim motions through the same executor.
- Prove that the document-view abstraction is not transcript-only.

#### `StaticRowsDocument`

A small pure test implementation.

Responsibilities:

- Test `DocumentViewExecutor` without TUI, SQLite, Lua renderers, or app state.
- Cover word motions, line motions, visual selection, yank ranges, page movement, anchors, and generation changes.
- Make row-document behavior easy to reason about before transcript integration.

## Durable Transcript Model

`TranscriptDocument` is backed by durable block records, not a rebuilt `Vec<Block>`.

```rust
trait TranscriptStore {
    fn descriptor_count(&self) -> usize;
    fn load_descriptors(&self, range: Range<usize>) -> Vec<BlockDescriptorRecord>;
    fn load_payloads(&self, keys: &[BlockKey]) -> Vec<BlockPayloadRecord>;
    fn load_tail(&self, count: usize) -> LoadedBlockRange;
    fn search_candidates(&self, query: &SearchQuery, page: SearchPage) -> Vec<SearchCandidate>;
    fn refine_candidate(&self, candidate: SearchCandidate, width: u16) -> RefinedSearchMatch;
    fn append_history_and_descriptors(&self, tx: AppendTurnRecords) -> StoreRevision;
    fn replace_suffix(&self, tx: ReplaceSuffixRecords) -> StoreRevision;
}
```

Storage rules:

- Durable identity is a stable block key or descriptor index, not a transient UI `BlockId`.
- Descriptor rows contain enough information to estimate height, list blocks, search coarse text, and decide whether payload hydration is needed.
- Large tool metadata and request bodies remain object-backed until visible render, exact copy, or explicit inspection requires them.
- Live draft blocks may exist in `TranscriptDocument`, but only promoted blocks become durable records.
- Rewind deletes or replaces suffix records transactionally.
- Save after one turn writes only new or changed suffix rows and objects.
- Request-start history and transcript descriptor insertion happen in one SQLite transaction before engine dispatch.
- Engine history snapshots and tool-loop durability are suffix transactions, not whole-session snapshots.
- Searchable text is indexed in SQLite. `instr(text, ?)` over all transcript rows is not acceptable for large sessions.
- The DB format has not shipped, so schema migrations can be reset or reshaped. Prefer the simplest final schema over compatibility migrations for unshipped layouts.

Schema/index requirements:

- `history_items(idx)` remains the provider history order.
- `transcript_blocks(block_idx)` remains the transcript order, with indexed `history_idx`, `kind`, tool fields, and stable block keys if they differ from descriptor index.
- Search uses either SQLite FTS5 or explicit trigram/posting tables keyed by block/descriptor id. The chosen shape must support candidate lookup without scanning all searchable text.
- Candidate search returns block ids and enough rank/order metadata to page through results around the current origin.
- Exact display match positions are computed only by refining candidate neighborhoods through `TranscriptDocument` rendering.
- Snapshot tables should be append/update/delete by touched index. Do not compare full snapshot tables on every save.
- Sidecar files such as `content.txt` are generated export/debug artifacts only, not hot search or save inputs.

Runtime cache rules:

- `loaded_blocks` is sparse by descriptor range or block key.
- `render_cache` is bounded by viewport, overscan, active selection, active search candidates, and recent anchors.
- Exact row heights are remembered by block/render key.
- Estimates are used only to map coarse scrollbar and arbitrary row targets to nearby blocks before exact local refinement.

## Virtual Row Index

The transcript row index is a virtual index over block descriptors.

```rust
struct VirtualRowIndex {
    generation: u64,
    block_order: SparseBlockOrder,
    heights: FenwickOrSegmentTree<NodeHeight>,
    exact_ranges: LoadedRanges<RowIndex>,
    total: DocumentExtent,
}

struct NodeHeight {
    key: BlockKey,
    estimate: RowIndex,
    exact: Option<RowIndex>,
    source: HeightSource,
}
```

Rules:

- Visible materialization exactifies the blocks it uses.
- Copy exactifies or streams the selected range.
- Search exactifies candidate neighborhoods only.
- `gg` resolves to first block and row zero exactly.
- `G` resolves by tail loading and backward fill, not by requiring a globally exact total row count.
- Scrollbar drag can land approximately, then re-anchor to an exact block and row once materialized.
- Re-estimation must not move visible content once a concrete anchor is established.

## Rendering Pipeline

The window render path becomes document-driven.

```text
WindowRenderRequest
    -> DocumentRegistry::get(handle)
    -> DocumentViewState::desired_range(rect, width)
    -> DisplayDocument::materialize(range)
    -> RenderCache::install(range, rows, generation)
    -> Window paint/layout reads RenderCache or backing Buffer
```

Rules:

- Only renderer/layout code reads the backing `Buffer` used to paint the window.
- Motions, copy, actions, selection, and document cursor use `DisplayDocument`.
- The backing buffer may be rebuilt from `RenderCache` every frame or only when rows change. That is a renderer implementation detail.
- Width changes invalidate layout/render cache keys and force exact re-materialization of the visible range only.
- Lua renderer generation and cache keys participate in render cache keys.

## Interaction Semantics

### Vim motions

- Vim parsing is independent of document implementation.
- Motions execute through `DocumentViewExecutor` using exact row text for rows they inspect.
- Word motions may materialize one row at a time and cross row boundaries by asking the document for adjacent rows.
- Counts, visual mode, visual-line mode, yank lines, line start/end, and horizontal movement use `DocPosition` and UTF-8-safe text helpers.
- Motions never inspect the paint buffer for transcript semantics.

### Mouse selection and drag

- Hit testing uses exact materialized visible rows.
- Drag anchor is a document coordinate. For transcript, prefer block key plus intra-block row when available.
- If drag autoscroll leaves the materialized range, the executor materializes the next range before updating the endpoint.
- Word/line selection uses break data from the same exact rows as hit testing.
- Copy exactifies the full selected range before producing output.

### Copy and yank

- Small visible ranges copy from exact materialized rows.
- Large transcript ranges stream block-by-block and render chunks as needed.
- Non-selectable spans, soft-wrap merging, hard breaks, `copy_as`, and display chrome rules remain exact.
- Yank flash is a document-range visual effect, not a buffer byte effect.

### Search

- Search starts from SQLite FTS/trigram candidates, not from display-row scans.
- Candidate lookup is indexed and paged. It must not scan all transcript text for every query.
- The active match exactifies the local display rows needed to compute visible row and byte columns.
- `n` and `N` move between semantic candidates, refining display locations lazily.
- Search keeps a small result window around the current match plus prefetch. It does not store every display match for a huge session unless the user explicitly asks for an exhaustive export/report.
- Short queries that are poor FTS/trigram filters need a bounded strategy: prefix index, exact phrase FTS, or a deliberately paged scan with visible progress. They must not freeze the UI.
- Search never layouts the entire transcript to find a match.

### Folds and expand/collapse

- Fold state is keyed by stable block key.
- Toggle/expand/collapse commands target the block under the exact cursor position.
- Changing fold state increments document generation, invalidates affected render/height ranges, and revalidates view anchors.
- Pin-to-bottom stays pinned if the view was following tail before the fold mutation.

### Pin to bottom and anchors

- Follow-tail lives in `DocumentViewState`.
- Appending live blocks keeps the viewport pinned only when `follow_tail` is true.
- Manual upward scroll clears follow-tail.
- `G`, explicit bottom commands, and accepting new user input can set follow-tail according to existing UX rules.
- Tail resume loads enough tail descriptors and payloads to fill the viewport exactly, without loading the whole transcript.

## Provider History Boundary

This plan does not make transcript display responsible for provider context. Provider history must be store-backed as part of the foundation, not deferred cleanup, because full-history clones violate the core memory invariant.

Current problematic shape:

```rust
pub struct StartTurnPayload {
    pub history: Vec<HistoryItem>,
}
```

Current improvement to preserve:

- Request-start history is now committed before engine dispatch.
- Engine dispatch receives prior history plus typed current input, avoiding duplicate current-user messages.
- Process and command request items follow the same durability pattern.

Target:

- Provider request building reads a bounded model-history cursor or snapshot from SQLite/runtime state.
- The current input remains typed in `StartTurnInput`; it is not duplicated in history payloads.
- Display transcript records and provider history rows can share durable indices, but neither owns the other.
- Starting a turn must not clone full session history only because the transcript viewer needs durable display records.
- Lua title/session APIs must expose committed history and current input consistently without forcing full history materialization.
- Tool metadata that is not model-visible remains object-backed and is not rehydrated into provider context.

The document split and provider-history split reinforce each other: one prevents display from loading everything, the other prevents request dispatch and save from loading everything.

## Execution Phases From Current Branch

The original phase list was intentionally broad. The current branch has already completed parts of the store, history, and document-owner foundation, so the remaining work should be tracked as smaller phases with explicit acceptance gates.

### Phase 0: Completed foundation now on this branch

Status: completed and validated through `a4dc0541`.

Completed work:

- Store-backed model history is used by normal interactive engine dispatch instead of cloning full provider history.
- Runtime hooks that need model messages use shared bounded `ModelHistorySource` paths when the store is current.
- Transcript descriptor suffix writes are saved transactionally with history suffix writes.
- Dirty suffix markers and row hashes reduce unchanged-prefix work, though hot saves still construct snapshot-shaped payloads.
- Large copy and fold preparation have bounded paths for current materialized transcript ranges.
- `BufferDisplayDocument` is narrowed to non-transcript buffer fallback use.
- The long-lived TUI transcript owner is now `TranscriptDocument`.
- The borrowed per-call adapter is now `TranscriptDisplayDocument<'_>`.
- Transcript row render cache ownership moved under `TranscriptDocument`.
- Loaded transcript documents retain descriptor-window metadata and `session_dir` store identity.
- `TranscriptDocument` can read descriptor windows from its backing `session.db`.
- Descriptor load policy for full and tail SQLite transcript reads lives in the transcript module instead of `app/history.rs`.
- Deferred display-only loads can ask the current `TranscriptDocument` to reload descriptor windows from its store backing before falling back to rebuilding from `Session.history`.

Validation already run after these slices:

- `cargo fmt --check`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo nextest run --workspace`

What this phase did not finish:

- `TranscriptDocument` is store-aware, but not yet a true sparse document. It still wraps eager `Transcript`/`BlockHistory` for loaded blocks.
- Full resume can still load full semantic history and full descriptor sets.
- Save paths are transactional but not yet typed append/rewind/checkpoint transactions.
- Viewer semantics still rely on existing row-text/window paths and transcript-specific app branches.

### Phase 1: Make `TranscriptDocument` a sparse transcript owner

Status: implemented for sparse descriptor-window ownership in the current worktree. `TranscriptDocument` now tracks total descriptor count, loaded descriptor ranges, descriptor records by durable descriptor index, and can merge newly loaded descriptor windows without discarding already loaded ranges. It still renders through an isolated compact `Transcript` bridge until Phase 3 adds virtual row gaps and exact viewport row indexing.

Goal: replace the eager transcript block owner inside `TranscriptDocument` with sparse descriptor and payload ownership while keeping live append behavior intact.

Deliverables:

- Define a small `TranscriptStore` or equivalent private store helper owned by `TranscriptDocument`, backed by `SessionDb`.
- Replace document fields that assume one fully loaded `Transcript` with:
  - total descriptor count
  - loaded descriptor ranges
  - loaded payload state per descriptor or block
  - live unsaved suffix blocks
  - cache invalidation hooks once the virtual row index exists
  - object hydration counters for instrumentation
- Keep durable descriptor indices, history indices, and transient UI ids distinct in API shape.
- Teach `TranscriptDocument` to load arbitrary descriptor ranges and merge them into its sparse store instead of returning isolated `LoadedTranscript` values for callers to install.
- Keep object-backed metadata and sidecars unhydrated until visible rendering, exact copy, explicit inspection, or exact search refinement needs them.
- Move descriptor coverage checks toward a storage invariant. Keep rebuild-from-session only as repair/import fallback with explicit logging.
- Add tests for range merge, overlapping range reads, empty ranges, tail ranges, range invalidation after append, and descriptor identity correctness.

Acceptance:

- A display-only document can hold total transcript extent plus a bounded loaded tail without owning all blocks.
- Loading a middle descriptor range does not replace the document or discard the tail unless the caller explicitly requests a reset.
- `TranscriptDocument` can answer whether a descriptor range is loaded, missing, stale, or live-suffix.
- The app no longer owns descriptor-window loading policy beyond choosing the session id and viewport parameters.
- Descriptor-window reads and merges are sparse; the remaining compact `Transcript` rebuild is isolated as a legacy render bridge to delete in Phase 3.

### Phase 2: Normal resume without full semantic session load

Status: implemented for normal display resume and request start in the current worktree. Display-only resume now keeps metadata-backed deferred session state with persisted history length and checkpoint cursor, `prepare_user_visible_turn` no longer forces `session::load`, request start uses the deferred store-backed model-history bounds, and the request row plus transcript descriptor are appended to SQLite without rewriting or deleting the persisted prefix. Explicit operations that need semantic history, such as rewind and fork, still call the deferred full-load bridge until later phases replace those semantics.

Goal: make normal resume follow the same bounded document path as display-only preview, while still providing provider history on demand.

Deliverables:

- Split session open into metadata/state load, transcript document open, and lazy provider-history access.
- Avoid reading all `history_items` into `Session.history` for display resume.
- Keep a bounded model-history cursor or source for request dispatch and runtime hooks.
- Preserve Lua/session APIs that explicitly ask for full history, but keep them out of hot resume/render/request paths.
- Make descriptor coverage a store invariant checked cheaply by metadata or row counts rather than by building a full transcript.

Acceptance:

- Opening a large session for normal resume does not hydrate full provider history or all transcript descriptors.
- First paint for normal resume and preview uses the same bounded document path.
- Request start can still build provider context from the store-backed model-history source.
- Full history materialization happens only for explicit APIs, export/import/repair, or tests that deliberately request it.

### Phase 3: Add virtual row index and viewport materialization

Status: implemented for viewport materialization in the current worktree. `TranscriptProjection::display_rows_for_range` exactifies only the requested row window instead of the range from row zero, viewport materialization records exact materialized row counts, render-cache hit/miss metrics are emitted, and tail-loaded sparse transcript documents report virtual row coordinates that include unloaded prefix/suffix gaps. Sparse documents keep one active descriptor window in the compact render bridge, so loading a middle range does not collapse gaps to an already loaded tail range, nearby scroll reuses the active window, and row/tail projection planning activates bounded descriptor windows from the document store. Remaining exact sparse semantic work for search, copy, folds, and actions stays in Phase 6.

Goal: make scrolling, resize, and viewport rendering operate over document coordinates and local exactification instead of full transcript rows.

Deliverables:

- Move row-estimate and exact-row state under `TranscriptDocument`.
- Represent row anchors by stable descriptor/block identity plus intra-block row or text position, not by materialized buffer offsets.
- Maintain a virtual row index that can:
  - estimate global extent
  - exactify visible plus overscan windows
  - map row ranges to descriptor ranges
  - map descriptor ranges back to exact row spans after local render
- Make nearby scroll load only missing descriptor/payload ranges needed for visible plus overscan rows.
- Make arbitrary jumps use estimated position plus bounded local refinement, not global layout.
- Make width changes invalidate only affected local row estimates and visible caches.
- Keep `TranscriptProjection` only as a local block/range renderer, or fold its responsibilities into `TranscriptDocument` if that is simpler.

Acceptance:

- First tail paint exactifies only visible rows plus configured overscan.
- Scrolling near the current viewport does not layout the whole transcript.
- `gg`, `G`, row jump, scrollbar landing, resize, and follow-tail all route through document anchors and local exactification.
- Render-cache keys include document generation, width, theme, renderer generation, and exact row range.
- Bench output reports descriptor rows loaded, payload bytes hydrated, exact rows materialized, and render-cache hit/miss counts per viewport operation.

### Phase 4: Replace snapshot-shaped hot persistence with typed transactions

Goal: finish the store rewrite side that is still worth doing because save and request-start durability remain major scaling risks.

Status: implemented in the current worktree. TUI hot persistence now uses typed `SessionHistorySuffix` writes paired transactionally with transcript descriptor suffix replacement for display-only deferred request append, normal request append, assistant/tool dirty suffix saves, checkpoint state, title/metadata/accounting table suffixes, and empty-history rewinds. These paths no longer construct `SessionSnapshot` or run snapshot-table/full-history comparison; remaining `SessionSnapshot` usage is limited to explicit import/export/legacy core save boundaries and tests.

Deliverables:

- Add typed transactions for:
  - request item append
  - transcript descriptor append
  - assistant/tool history suffix append
  - checkpoint update
  - title and metadata update
  - turn metadata/accounting update
  - rewind/delete suffix
- Avoid constructing `SessionSnapshot` for hot request start, engine `HistoryUpdated`, tool-loop checkpoint, append, and rewind paths.
- Replace full-session fingerprinting in hot paths with DB revision, expected-history-length checks, dirty suffix markers, and row hashes.
- Keep the existing history/transcript suffix delete rule: originated transcript rows are deleted by `history_idx`; stale unoriginated descriptor tails are cleared only after preserved originated block range.
- Keep explicit export/import/repair commands as the only full-session streaming paths.

Acceptance:

- Starting a request commits only the new request row, descriptor row, and touched metadata rows before dispatch.
- Engine request dispatch does not require a full history vector in memory.
- Save after request start, a tool-loop snapshot, or one assistant update writes only dirty suffix rows and objects.
- No-op save and one-row append save do not call full-session fingerprint, full history serialization, or full snapshot-table comparison.
- Replacing a later user request preserves prior multi-block assistant descriptors and deletes only the correct originated or stale-unoriginated suffix.

### Phase 5: Finish document-view semantics and remove host special cases

Goal: make transcript, readonly buffers, and static/test documents share the same viewer command and materialization path.

Status: implemented in the current worktree. `DocumentViewExecutor` now executes row motions, word motions, page movement, visual selection, linewise yank, text objects, action lookup, and document-view mouse word/row-group selection against `DisplayDocument`; focused `StaticRowsDocument` tests cover the shared executor. Transcript key/keymap commands, transcript mouse selection/action dispatch, and non-transcript readonly buffer viewers all route through the document registry and shared document-view executor path. Full-row transcript projections now preserve active document-view state instead of falling back to byte-backed semantics, and remaining `row_text`/`row_viewer` compatibility names have been removed from the Rust sources. Transcript-specific indexed search remains as the Phase 6 sparse-search backend rather than a Phase 5 viewer-command special case.

Deliverables:

- Rename or replace row-text viewer types with document-view types:
  - `ViewerCommand` to `DocumentCommand`
  - `RowTextState` to `DocumentViewState`
  - row-text executor to `DocumentViewExecutor`
- Add or finish `StaticRowsDocument` for focused command tests.
- Add `BufferDocument` so readonly buffer viewers use the same command path as transcript.
- Attach a `DocumentHandle` or direct document reference to windows that display row documents.
- Resolve documents through a registry or explicit handle, not `WinId` comparisons.
- Replace `UiHost::display_rows_for_range`, `document_total_rows`, and `copy_document_range` as general lookup paths with document calls.
- Move Vim motions, visual selection, linewise selection, copy/yank, page movement, mouse drag, action opening, and top/bottom to the executor.
- Keep backing buffers only as renderer/layout targets, not semantic sources.

Acceptance:

- Transcript windows and readonly buffer viewers execute the same document command path.
- `Window` no longer decides row-document text semantics.
- Existing user interactions pass without transcript-specific app cursor snapping.
- Focused executor tests cover Vim row motions, word motions, visual selection, linewise selection, yank ranges, page movement, top/bottom, and action hit testing through `StaticRowsDocument`.

### Phase 6: Complete exact sparse search, copy, folds, and actions

Goal: make semantic operations exact across unloaded transcript ranges without falling back to full layout or paint-buffer text.

Status: implemented in the current worktree. Transcript search uses SQLite candidate blocks plus bounded display-row exact refinement, and candidate scanning no longer falls back to descriptor search-text line matches when exact display rows have no match. Document search has a `DisplayDocument::search_next_match` API used by non-transcript search navigation so viewer search can advance through document-owned chunks without building a full match list. Streamed copy across unloaded middle transcript ranges is covered by regression tests that assert selected chunk materialization and no full row build. Fold changes capture stable transcript row anchors before presentation changes and restore scroll position, document cursor, selection endpoints, drag endpoints, follow-tail state, and the current transcript search match afterward. Transcript action hit testing is covered through `TranscriptDisplayDocument::action_at`, asserting exact target-row materialization from display spans without full row builds. Drag autoscroll refreshes document cursor and drag endpoints from the newly materialized row slice, including UTF-8 byte-column snapping after crossing unloaded boundaries. The remaining regression surface is covered by targeted document-row tests for UTF-8 stale action offsets, selectable ranges, non-selectable chrome, action spans, soft breaks, hard breaks, and visual paragraph behavior, plus existing transcript copy, search, fold, and action tests.

Deliverables:

- Keep transcript search candidate lookup in SQLite, and page candidates around the current origin.
- Replace any remaining `instr(text, ?)` or full display-row scans for transcript hot paths with indexed candidate queries plus bounded exact refinement.
- Add document-level indexed search APIs so non-transcript documents do not need broad host scans for large row documents.
- Stream large-range copy through descriptor records and exact chunk rendering.
- Ensure copy cost is proportional to selected content, not total transcript size.
- Implement fold/expand/collapse on stable descriptor/block keys with generation invalidation.
- Implement action hit testing from exact materialized spans, never estimated rows.
- Implement drag selection and autoscroll across unloaded range boundaries.
- Add regression tests for UTF-8 stale offsets, soft wraps, hard breaks, selectable ranges, non-selectable chrome, action spans, and visual paragraph behavior.

Acceptance:

- Search in huge sessions does not layout the whole transcript, scan every row, or build a full in-memory text index.
- `n` and `N` use bounded SQLite candidate paging and local display refinement.
- Copy/yank across unloaded ranges loads and renders only selected chunks.
- Fold changes preserve cursor, selection, follow-tail, and search anchors.
- No semantic operation uses estimated rows except scrollbar extent and coarse landing before exact local refinement.

### Phase 7: Cleanup, compatibility isolation, and hard performance gates

Status: complete. Store-level performance counters now cover full and ranged history reads, descriptor full/slice/tail loads, object payload hydration and storage, search candidate paging, snapshot/full-session loads, dirty suffix size, and DB row write/delete counts. The transcript search benchmark prints `store:*`, `session:*`, and `transcript:*` perf rows in addition to `search:transcript*` rows so benchmark output exposes the storage hot-path counters added in this phase. Transcript projection now renders clipped row windows for visible, resize, jump, and copy materialization, and the layout benchmark fails if these operations, benchmarked copy ranges, or live append repaint materialize more than the bounded requested row window. Display-only resume benchmark gates now fail if tail resume performs full history/session/descriptor reads or loads more than a bounded tail descriptor window, and save/request regression tests fail if no-op save, one-row request append, engine history update save, or provider request dispatch enqueue work proportional to total session history. Hot TUI persistence no longer rebuilds the full `content.txt` search sidecar; compatibility search blob reads prefer canonical SQLite and refresh the sidecar only on explicit search-blob load. Search benchmarks now fail if rare search, repeated next-match navigation, or incremental appended-block search perform full store/session/transcript reads or exactify/display-scan more than bounded local refinement rows. Remaining explicit session open/preview/rebuild full-load fallbacks for old or partially migrated sessions are `COMPAT(legacy-session-full-load-fallbacks)` tagged and perf-instrumented; the deferred semantic-session load bridge is also perf-instrumented under its existing compatibility id. The inspect server's full session detail endpoint is now separately instrumented as an explicit diagnostic load.

Goal: delete old paths after the correct abstractions cover hot operations, and prove the application scales to very large sessions.

Deliverables:

- Ensure provider-visible history, display transcript records, object-backed metadata, and request audit payloads remain distinct.
- Keep legacy JSON importers isolated at the storage boundary and `COMPAT`-tracked according to repo convention.
- Delete obsolete transcript compatibility adapters, duplicate display-document concepts, old row-cache paths, and direct legacy hot load/read paths.
- Add instrumentation that records rows loaded, descriptors loaded, payloads loaded, history rows read, bytes hydrated, DB writes, dirty suffix size, render cache hit/miss, and exactified row count per operation.
- Add large-session regression benchmarks that fail when resume, request start, save, search, copy, resize, or scrollbar drag scale with total session size instead of requested range size.

Acceptance:

- Resume memory is proportional to visible transcript, sparse loaded ranges, active search/copy context, and bounded model-history needs.
- Save after no-op or one turn is independent of full session size except for SQLite index maintenance.
- Request start, request snapshot save, and tool-loop durability are incremental transactions.
- There is one canonical load/save/render/audit path.
- Any remaining compatibility or repair path is explicitly named, instrumented, and excluded from normal hot paths.

### Worthwhile deferred work that remains in scope

These items are not distractions. They are deferred only because they depend on the document owner and store boundaries being stable first.

- Indexed transcript search beyond the current candidate paging if measurements show `instr` or existing indexed terms are insufficient.
- A complete `DocumentViewExecutor` and `StaticRowsDocument` test suite for viewer semantics.
- `BufferDocument` for readonly buffers, so buffer viewers do not become a second semantic path.
- Typed incremental persistence transactions that bypass `SessionSnapshot` construction in hot paths.
- Normal resume metadata-only open with lazy history and transcript document loading.
- Hard asymptotic benchmark gates, including 100 MiB, 500 MiB, and 1 GiB class smoke coverage.
- Deleting old host callback paths only after the document path covers transcript, readonly buffers, search, copy, folds, anchors, and actions.

## Validation Gates

### Scale target envelope

The goal is not merely to make 10 MiB sessions acceptable. The architecture should keep working as sessions grow to 100 MiB, 500 MiB, and 1 GiB class databases.

Hot operations must be measured by rows/bytes touched, not only wall time:

- Resume/preview first paint touches tail descriptors, visible payloads, visible rows, and bounded overscan only.
- Opening session metadata touches `session_state`, small sidecar/meta rows, and no history payload rows.
- Request start touches one history row, one or a few transcript descriptor rows, touched metadata rows, and the bounded model-history cursor needed by the provider.
- No-op save touches revision/dirty metadata only and writes nothing proportional to history size.
- One-turn save writes the new/changed suffix and objects only.
- Search touches indexed candidate rows plus bounded exact-refinement rows.
- Scrollbar drag touches logarithmic index state plus bounded local descriptor/payload rows.
- Width change touches visible rows plus overscan, not all blocks.

Initial benchmark gates:

| Scenario | Must demonstrate |
| --- | --- |
| 10 MiB | no hot path allocates hundreds of MiB or does full-session save/load work |
| 50 MiB | save, resume, search, and resize are not materially worse than 10 MiB except where selected/visible result size changes |
| 100 MiB | normal interactive operations stay bounded by visible/search/copy/request ranges |
| 500 MiB to 1 GiB | smoke benchmark proves memory stays bounded and operations stream/page through SQLite instead of loading all rows |

Wall-time thresholds should be hardware-calibrated, but asymptotic gates are mandatory. A benchmark that improves 10 MiB while still scaling linearly with total session size is a failure.

Each phase should include targeted unit tests, integration tests, and a bounded benchmark run.

Required correctness coverage:

- Vim `gg`, `G`, counts, page/half-page, `w`, `b`, `e`, `$`, `0`, visual, visual-line, yank, linewise yank.
- Mouse click, drag selection, drag autoscroll, double/triple click if supported.
- Copy across soft wraps, hard breaks, non-selectable spans, and display chrome.
- UTF-8 stale offset snapping through `smelt_buffer::text` helpers.
- Search next/previous across unloaded ranges.
- Fold toggle, expand, collapse, and anchor revalidation.
- Pin-to-bottom across append, resize, fold, search, and manual scroll.
- Preview and display-only resume from SQLite tail records.

Required performance evidence:

- Large session resume first paint loads only tail descriptors/payloads needed for visible rows plus overscan.
- Display-only resume does not hydrate full provider history.
- Request start does not clone or serialize full provider history.
- Request-start save and engine `HistoryUpdated` save write only dirty suffix rows and objects.
- Arbitrary scrollbar drag loads and exactifies only the local target region.
- Search candidate lookup avoids full transcript layout and full display-row scans.
- Copy cost is proportional to selected display content, not total transcript size.
- Width change re-materializes visible ranges only.
- Save after append writes only dirty suffix records and objects.

Suggested benchmark command for current coverage is the transcript layout/search suite, extended as needed:

```text
cargo xtask bench-transcript-layout --runs 3 --workloads mixed_10mib,mixed_50mib --search --search-bytes 10485760 --resume --resume-bytes 10485760
```

The benchmark should report at least:

- descriptor load count and payload load count
- provider history rows read and model-history bytes hydrated
- first paint block and row materialization counts
- tail resume latency
- request-start transaction latency and rows written
- engine-history-snapshot transaction latency and rows written
- arbitrary jump latency and hydrated range size
- search candidate and refinement latency
- copy selected range latency
- live append update latency
- width change latency
- save dirty suffix latency
- allocation counts and bytes allocated

## Non-Goals

- Do not build a second durable storage engine.
- Do not preserve old display cache formats.
- Do not make scrollbar estimates exact before first paint.
- Do not make transcript a special host callback instead of a document.
- Do not solve collaborative multi-writer sessions in this plan.
- Do not let provider history and display transcript become the same runtime object again.
- Do not introduce temporary scaffolding whose planned end state is deletion instead of promotion.
- Do not accept full-session load/save/request clones as an intermediate architecture for normal operations.

## Completion Definition

This rewrite is complete when:

1. Transcript resume, preview, render, scroll, search, copy, fold, and live append run through `TranscriptDocument`.
2. Vim and mouse interactions run through `DocumentViewExecutor` and `DocumentViewState` for transcript and readonly buffer documents.
3. `Window` no longer owns row-document semantics.
4. `Buffer` is only the renderer/layout backing store for document windows, not the semantic source.
5. `UiHost::display_rows_for_range` is gone or demoted to a narrow compatibility adapter with a deletion issue.
6. No hot path reconstructs a full in-memory transcript for display-only resume.
7. No hot path loads, clones, serializes, fingerprints, or rewrites full session history.
8. Request start, engine history snapshots, append, rewind, and save are SQLite transactions over dirty suffixes and touched metadata/object rows.
9. No semantic operation uses estimated rows except for scrollbar extent and coarse scrollbar landing before exact local refinement.
10. Provider request start no longer clones full history as the normal path.
11. Legacy session compatibility remains isolated at the storage boundary and documented with `COMPAT` ids.

## Post-Completion Performance Follow-Up Plan

The document rewrite is complete, but the final benchmark pass exposed follow-up work that is worth doing before treating large-session performance as fully hardened. These items are intentionally tracked after the completion definition because they refine and stabilize the finished architecture rather than block it.

### Follow-up 1: Harden the benchmark harness

Status: complete. The layout benchmark now chooses a copy range by probing for selectable rows that actually copy, live append validation no longer depends on total row growth, and the broad mixed 10 MiB/50 MiB layout/search/resume command completes on the current worktree. Layout, navigation, search, resume, and layout-counter summaries now emit JSON lines in addition to the existing human and key-value output.

Goal: make the broad transcript benchmark command continuously runnable and easy to compare over time.

Deliverables:

- Fix benchmark fixtures so `mixed_10mib,mixed_50mib` and multi-workload runs do not fail because a synthetic copy range lands on non-selectable content or because a live append assertion depends on row-count growth that may not occur after wrapping/folding changes.
- Choose benchmark copy ranges from known selectable materialized rows.
- Assert live append repaint through document generation, tail materialization, and bounded counters rather than only `total_rows` growth.
- Emit machine-readable JSON or CSV summaries in addition to the current human table.
- Keep the existing counter gates for full row builds, exactified rows, descriptor reads, store full reads, and dirty suffix writes.

Acceptance:

- `cargo xtask bench-transcript-layout --runs 1 --workloads mixed_10mib,mixed_50mib --search --search-bytes 10485760 --resume --resume-bytes 10485760` completes successfully on the current worktree.
- The benchmark output includes stable machine-readable rows for layout, search, resume, and key counter summaries. Save/request benchmark rows are added in Follow-up 6.

### Follow-up 2: Explain and reduce tail-load wall-time scaling

Status: complete. Tail resume now emits JSON/key-value duration and counter rows for read-only store open, descriptor count, tail slice probing, descriptor decode, and compact window construction. The measured bottleneck was repeated descriptor count plus `LIMIT/OFFSET` tail reads; the tail path now computes descriptor extent with `MAX(block_idx)+1`, carries the known total through tail probes, and reads the tail via descending primary-key order without offset scans. True resume tail-load time dropped to about 0.8 ms at 10 MiB and 0.9 ms at 50 MiB while still loading only 80 descriptor rows.

Goal: tail resume should stay bounded in rows/descriptors and should not have unexplained wall-time growth with total database size.

Deliverables:

- Add focused timing labels around read-only SQLite open, metadata reads, descriptor count reads, tail descriptor queries, descriptor decoding, and document construction.
- Compare 10 MiB and 50 MiB tail-load timing using the true resume benchmark.
- Optimize only the measured bottleneck, such as avoiding unnecessary count queries, using a better tail index/query shape, reusing metadata already loaded by resume, or reducing row decoding allocations.

Acceptance:

- Tail-load benchmark output shows where the time is spent.
- Any remaining scaling is explained by a named SQLite operation or reduced to noise relative to render time.

### Follow-up 3: Reduce first-paint allocation churn

Goal: first paint should allocate in proportion to the visible/rendered work, not to pathological block count.

Deliverables:

- Add allocation counter breakdowns for row materialization, render-node collection, Lua display-model construction, render-cache insertion, and display-row cloning.
- Profile the `tiny_blocks_1mib` workload, which currently allocates far more than the input size on first paint.
- Remove avoidable clones and temporary vectors on the bounded materialization path.
- Prefer reusing render buffers or borrowing existing row text where that keeps ownership simple.

Acceptance:

- First-paint allocation for `tiny_blocks_1mib` is materially lower without regressing the bounded row counters.
- The benchmark reports enough allocation labels to catch future allocation regressions.

Status: complete. Layout perf snapshots now expose per-label allocation rows for transcript first paint, including render-plan construction, row-index rebuilds, display-model compile/insert/render, bounded row materialization, buffer installation, and display-row cloning. The large avoidable churn was render-plan fingerprinting: the old path collected node id/key vectors and JSON-serialized them, and block node fingerprints also used JSON serialization per node. The render plan now hashes incrementally with no per-node temporary vectors, and row-height estimation borrows block text instead of cloning raw text. On `tiny_blocks_1mib`, first-paint allocations dropped from the follow-up baseline of 196,685 allocs / 133,944,399 bytes to 36,137 allocs / 101,409,850 bytes, while first-paint bounded counters stayed at 60 materialized rows and 26 materialized blocks.

### Follow-up 4: Make search indexing durable or truly incremental

Status: complete. The search benchmark now persists the generated transcript before timed searches, so rare cold search exercises the durable SQLite transcript search records instead of an all-dirty in-memory fixture. Candidate-backed transcript searches build only the candidate layout/index and skip full trigram construction; dirty in-memory suffix blocks are merged with SQLite candidates so an append after the persisted baseline searches only the appended suffix. The benchmark now gates that rare/common/after-append searches use candidate indexes and that after-append scans at most the dirty suffix. On the 10 MiB search benchmark, rare cold dropped from 54.479 ms with a 47.400 ms full index build to 5.120 ms with 11 candidate entries and no trigram build; after-append dropped from 53.114 ms with a 47.648 ms full index build to 8.659 ms with one candidate entry, one dirty block scanned, and no trigram build. Common `n` navigation reuses the active candidate page instead of requerying SQLite on every repeat and measured 152.868 ms for the next 100 matches.

Goal: cold and after-append transcript search should avoid rebuilding the same large in-memory index when SQLite descriptor/search records are current.

Deliverables:

- Measure whether current search time is dominated by index build, candidate lookup, or exact display refinement.
- Choose the simplest durable or incremental index that fits the existing store model. Prefer SQLite-backed indexed terms/FTS/trigram rows if they avoid duplicating another cache invalidation system.
- Update append and rewrite suffix transactions to maintain the chosen search index incrementally.
- Keep exact display refinement in `TranscriptDocument`; the index may only produce candidates.

Acceptance:

- Rare search and after-append search do not rebuild a full in-memory transcript search index on the normal path.
- Search benchmarks still prove bounded exactified/display-scanned rows.

### Follow-up 5: Remove the compact transcript render bridge

Status: complete. Descriptor window loads now keep `LoadedTranscript.transcript` empty until the document activates a sparse window, so the old `transcript:descriptor_window:build_compact` bridge and its compatibility naming are gone. Loaded descriptor rows carry `TranscriptBlockRecordWithId`, allowing active projections to preserve durable SQLite `block_idx` values as `BlockId`s across window activation and hit-testing state instead of renumbering each active window from zero. Tail activation reuses the already loaded tail window. The true resume benchmark for 10 MiB shows `transcript:resume_tail:build_loaded` at 58 us, `transcript:descriptor_window:active_records` at 80, no compact-build metric, and tail-load at 0.826 ms; `tiny_blocks_1mib` first paint stayed flat at 36,137 allocations / 101,409,850 bytes with bounded materialization counters.

Goal: make `TranscriptDocument` render directly from sparse descriptor windows instead of rebuilding a compact `Transcript` for the active window.

Deliverables:

- Replace `rebuild_legacy_compact_transcript_from_active_descriptors` with a direct sparse descriptor-to-projection path.
- Keep `BlockId`, descriptor index, and history index identity stable across window activation, folds, anchors, and action hit testing.
- Remove compatibility naming once the bridge is gone.
- Preserve all Phase 6 exactness tests for search, copy, folds, actions, UTF-8 offsets, and drag autoscroll.

Acceptance:

- Activating a descriptor window no longer rebuilds an intermediate compact transcript.
- Render/copy/search counters remain bounded and first-paint allocations decrease or stay flat.

### Follow-up 6: Add wall-time hot-path save/request benchmarks

Goal: keep the row-count hot-path guarantees while also catching fixed-cost latency regressions.

Deliverables:

- Add benchmark samples for no-op save, one-row request append, engine `HistoryUpdated`, rewind/delete suffix, and provider request dispatch history read.
- Report wall time plus the existing row-count/store-read counters.
- Gate only on robust asymptotic counters by default; make wall-time thresholds advisory unless calibrated for CI hardware.

Acceptance:

- Benchmark output includes save/request latency and dirty suffix row counts.
- No benchmarked hot save/request path performs full session history reads, clones, serialization, fingerprinting, or sidecar rewrites.

Status: complete. `cargo xtask bench-transcript-layout --runs 1 --workloads tiny_blocks_1mib --skip-nav --save-request --save-request-history 256` now emits `TRANSCRIPT_HOT_PATH_BENCH_SAMPLE` and JSON rows for no-op save, one-row request append, engine `HistoryUpdated`, rewind/delete suffix, and provider history read. The 256-row release sample measured no-op save at 0.016 ms with no dirty rows, request append at 0.969 ms with one dirty history row and one descriptor row, `HistoryUpdated` at 1.027 ms with one dirty history row, rewind/delete suffix at 0.999 ms with two deleted history rows and no rewritten suffix rows, and provider history read at 0.278 ms with 32 range-read rows. The benchmark asserts the robust asymptotic counters by default and leaves wall time advisory through the emitted summaries.

## 500 MB Session Optimization Plan

Principle: keep the document/view separation and storage-boundary compatibility from this plan. Optimize by removing full-session algorithms and wrong abstractions, not by shaving local instructions off paths that should not run at 500 MB scale.

Phases:

1. Add 500 MiB benchmark knobs for search and resume so large-session runs do not duplicate warmup data by default. Status: complete for search and resume fixture scale. `--scale-500mb` and `--no-warmup` now exercise 500 MiB search/resume targets without doubling data generation. A 500 MiB search run with an out-of-`/tmp` temp dir reached 524,290,206 bytes and 6,413,965 rows. The current measured search bottlenecks are candidate layout and exact display refinement, not full index construction; rare search measured 525.031 ms, common submit 179.758 ms, next 100 matches 615.217 ms, and after-append search 604.984 ms. The true resume benchmark now writes a descriptor-backed SQLite fixture directly instead of building a full in-memory session/transcript as setup. A 500 MiB descriptor-backed resume run generated 524,288,000 bytes across 128,000 descriptors in 15.165 s of setup, then tail-loaded 80 descriptors from the 128,000-descriptor store in 0.860 ms and rendered the tail in 1.557 ms. The tail path recorded no full-session history or descriptor reads.
2. Run the 500 MiB benchmarks and identify measured bottlenecks in store search, descriptor loading, document materialization, save/request paths, and provider history reads. Status: in progress. Search at 500 MiB still points at candidate layout, exact display refinement, common-term driver counts, and after-append render-plan/row-index rebuilds; descriptor-backed tail resume is already bounded by loaded descriptors rather than total session size.
3. Replace bottleneck algorithms with bounded or indexed operations. Text search should page through indexed candidates in document order instead of materializing all matching blocks before applying origin/limit.
4. Validate that hot paths stay bounded by rows/descriptors touched, and that wall-time changes are explained by named benchmark metrics.
5. Reflect and clean up abstractions that became unnecessary after the large-session path is correct.
