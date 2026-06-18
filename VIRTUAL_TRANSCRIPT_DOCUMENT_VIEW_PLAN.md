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

## Current Mainline Checkpoint After Rebase

The branch was rebased onto `main` at `520e1325`.

Relevant mainline changes that shape this plan:

- `777d8663 fix(session): preserve in-flight request history` made request-start history durable. User/process/command request items are appended before engine dispatch, transcript blocks are tagged with their future `History(idx)` origin, and engine dispatch receives pre-request history to avoid duplicating the current input.
- The same commit fixed history suffix persistence so truncating history by index no longer deletes transcript blocks by mismatched `block_idx`. Final storage APIs must preserve this rule: originated transcript rows are deleted by `history_idx`, while stale unoriginated descriptor tails are cleared only beyond the preserved originated range.
- Engine `HistoryUpdated` now triggers `save_session()` for per-request/tool-loop durability. The final design keeps that durability but replaces any full fingerprint, full clone, or full rewrite underneath it.
- Resume/deferred load now validates transcript descriptor coverage and falls back to rebuilding when descriptors are incomplete. The final architecture should turn descriptor coverage into an invariant. Fallback rebuild is acceptable only as importer/repair behavior, not as a normal hot path.
- The title plugin now avoids double-counting a request already committed to history. Lua session/history APIs must continue to distinguish committed history from the current input without forcing full history materialization.
- Mainline added and then simplified diff/document foundations. The current code does not have `crates/tui/src/app/document.rs`; the actual baseline is still `DisplayDocument`, `HostDisplayDocument`, `UiHost` row callbacks, and `TranscriptDocument<'_>` in `crates/tui/src/app/transcript.rs` borrowing `TranscriptView` per call.

Current seams that still violate the final constraints:

- Normal resume can still materialize full semantic session history in `load_session_snapshot` for non-preview paths.
- Some explicit Lua/test/debug APIs still materialize `model_history()` as `Vec<HistoryItem>` when the caller asks for the whole model-visible history. Normal interactive engine dispatch and Lua/host model-message fallback now use `ModelHistorySource` or bounded store reads when the store is current.
- Search still has fallback paths that can scan display rows for non-transcript documents. Transcript search uses SQLite candidates first, then bounded refinement.
- `BufferDisplayDocument` remains as the non-transcript buffer fallback adapter. It should not become a transcript path.
- `TranscriptDocument<'_>` is currently an adapter over `TranscriptView`; it is not yet the long-lived virtual document that owns store access, sparse ranges, virtual row index, and render cache.
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

## Current Violation Map

| Area | Current code | Violation | Final direction |
| --- | --- | --- | --- |
| Session load | `load_session_snapshot` reads all `history_items` into `Vec<HistoryItem>` | resume memory and latency scale with total session size | load metadata, descriptor windows, and bounded model-history cursors separately |
| Save decision | Dirty suffix markers and DB row hashes avoid unchanged-prefix writes, but hot paths still build `SessionSnapshot` suffix payloads | request-start/tool-loop durability can still allocate more than the exact typed delta | typed transactions for appended history, descriptor suffix, title/meta, checkpoint, turn meta, and accounting deltas |
| Save payload | History and transcript descriptor suffixes are saved together in one SQLite transaction | blob externalization and snapshot construction are still snapshot-shaped | explicit append/replace transactions over dirty rows and objects |
| Provider dispatch | Interactive `StartTurnPayload.history` uses `ModelHistorySource::Store`; engine reads the requested range from `session.db` | explicit Lua/test/debug callers can still request full model-visible history | keep materialization only for explicit APIs, and prefer store-backed message reads for runtime hooks |
| Transcript resume | full descriptor load path can still rehydrate descriptor JSON and tool state for all blocks | display resume can scale with total transcript | `TranscriptDocument` loads sparse descriptor ranges and hydrates payloads only on demand |
| Search storage | transcript search uses indexed SQLite candidate terms plus exact refinement | generic non-transcript document search can still scan display rows | keep transcript candidate paging; add document-level indexed search APIs for other document kinds |
| Search runtime | transcript search asks SQLite for candidate blocks before local display refinement | fallback scan paths remain for buffer documents | document-level search API with indexed implementations and bounded refinement |
| Generic document search | buffer fallback search can materialize row windows from row zero to total rows | full display-row scan for non-transcript documents | `BufferDisplayDocument` remains bounded to buffer fallback; indexed document implementations replace broader host scans |
| Projection cache | `full_rows` and `build_rows` remain for full-text consumers | easy accidental full materialization | remove from hot APIs; explicit export/debug command can stream instead |
| Schema | migration version exists but DB format has not shipped | carrying compatibility migrations would add complexity | reset/reshape schema freely before release; optimize for final query/write patterns |
## Current Seams to Promote or Delete

### Promote

- `DisplayDocument` in `crates/edit/src/row.rs` is the UI-facing document trait to finish.
- `DisplayRows`, `DisplayRow`, `DisplaySnapshot`, `DocPosition`, `DocRange`, and `TextRange` are the current basis for row-document coordinates.
- `RowTextState` in `crates/edit/src/window/row_text.rs` already keeps cursor and selection in document row coordinates. This should become `DocumentViewState`, not remain a window sub-mode.
- `ViewerCommand` and `resolve_row_document_viewer_command` in `crates/edit/src/window/row_text.rs` are close to the semantic command executor. They should be renamed, completed, and moved behind a document-view boundary.
- `TranscriptDocument<'_>` in `crates/tui/src/app/transcript.rs` is a useful proof that transcript can implement `DisplayDocument`, but the final type must own virtual document state instead of borrowing `TranscriptView` for one call.
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

## Big Implementation Phases

### Phase 1: Store-backed runtime and document-view foundation

Goal: establish the two non-negotiable foundations before more transcript tuning: runtime session state is store-backed and incremental, and viewer semantics are document-backed. This phase removes the architecture that forces full-session load/save or uses `Window`/`Buffer` as semantic owner.

Deliverables:

- Replace hot runtime dependence on `Session { history: Vec<HistoryItem> }` with a store-backed session runtime:
  - small mutable session metadata in memory
  - append/update/delete suffix APIs over SQLite `history_items`
  - bounded model-history cursor/snapshot for provider request building
  - no full-history clone for request start, save, fingerprint, title generation, Lua conversation views, or display resume
- Replace full-session fingerprinting with DB revision, dirty suffix markers, row hashes, and expected-history-length checks.
- Make request start a transaction boundary:
  - append the user/process/command request item to `history_items`
  - append the corresponding transcript descriptor with `History(idx)` origin
  - update title/metadata/accounting snapshots as needed
  - commit before engine dispatch
  - dispatch the engine with prior provider history plus typed current input, without duplicating the request in the provider payload
- Make engine `HistoryUpdated` and tool-loop snapshots transactional suffix updates, not full session rewrites.
- Extend the benchmark before claiming improvement:
  - request-start transaction latency
  - engine `HistoryUpdated` save latency
  - provider history rows read
  - snapshot table rows touched
  - SQLite rows inserted/updated/deleted
  - bytes hydrated from objects
- Preserve the latest main fix in the final model: history suffix truncation must delete transcript rows by `history_idx`, while stale unoriginated descriptor tails are cleared only after the preserved originated block range.
- Rename or replace row-text viewer types with document-view types:
  - `ViewerCommand` -> `DocumentCommand`
  - `RowTextState` -> `DocumentViewState`
  - row-text executor -> `DocumentViewExecutor`
- Extend `DisplayDocument` only as needed for exact anchors, estimated extent, capabilities, and generation revalidation.
- Add `StaticRowsDocument` and focused executor tests.
- Add `BufferDocument` so readonly viewers use the same semantic path as transcript.
- Add a document handle or registry owned by the edit/UI layer.
- Move Vim motions, selection, copy/yank, action opening, and mouse drag semantics to the executor.
- Keep renderer/layout reads of backing buffers, but stop using backing buffers as the semantic source for document windows.

Deletion targets:

- Full-session `persist_fingerprint` over `Session.history`.
- `model_history() -> Vec<HistoryItem>` as the request-start hot path.
- Save paths that serialize, diff, or clone all history to decide whether one request changed.
- Materialized-row semantic mode on `Window`.
- Viewer command resolution that wraps a `Buffer` slice as the semantic document.
- New transcript-specific app cursor snapping.

Acceptance:

- Starting a request commits only the new request row, descriptor row, and touched metadata rows before dispatch.
- Engine request dispatch does not require a full history vector in memory.
- Save after request start, a tool-loop snapshot, or one assistant update writes only dirty suffix rows and objects.
- No-op save and one-row append save do not call full-session fingerprint, full history serialization, or full snapshot-table comparison.
- Replacing a later user request preserves prior multi-block assistant descriptors and deletes only the correct originated or stale-unoriginated suffix.
- Unit tests prove Vim row motions, word motions, visual selection, linewise selection, yank ranges, page movement, top/bottom, and action hit testing through `StaticRowsDocument`.
- Readonly buffer viewers and transcript windows execute the same document command path.
- `Window` no longer decides document text semantics.
- Only scrollbar extent can be estimated. All command results are exact against the document rows they consume.

### Phase 2: Make `TranscriptDocument` the real transcript runtime

Goal: replace eager in-memory transcript blocks with a virtual document backed by SQLite descriptors and sparse payload loading.

Deliverables:

- Define `TranscriptStore` over existing `SessionDb` descriptor/payload APIs.
- Split durable descriptor identity from transient UI ids everywhere.
- Make descriptor coverage a storage/runtime invariant, not a normal fallback rebuild path.
- Make `TranscriptDocument` own sparse loaded descriptor/payload ranges, live suffix blocks, block fold state, virtual row index, and render cache.
- Tail resume opens a `TranscriptDocument` from SQLite and loads only enough tail records to fill visible rows plus overscan.
- Live streaming appends records to the live suffix and updates dirty persistence state without rebuilding all prior blocks.
- Save persists only new/changed descriptor and payload suffixes.
- Rewind/delete suffix updates SQLite and invalidates only affected document ranges.
- Keep large tool metadata and sidecars object-backed until visible rendering, exact copy, explicit inspection, or exact search refinement requires hydration.

Deletion targets:

- Normal resumed-session `build_transcript_from_session` path.
- Descriptor coverage fallback rebuild as a hot path. It may remain only as an importer/repair diagnostic with explicit logging.
- Full `BlockHistory` construction for display-only resume.
- Host transcript row methods as the primary row-document API.
- Full transcript row vector used as the render source.

Acceptance:

- Opening a large session for preview or resume does not load every block payload or provider history row.
- First tail paint exactifies only visible plus overscan rows.
- Jump top, jump bottom, explicit row jump, scrollbar drag, and manual scroll all route through `TranscriptDocument` anchors and exact local materialization.
- Copy/yank/search/fold/action paths never use estimated rows as semantic data.
- Descriptor indices, history indices, and transient UI ids cannot be confused by type or API shape.

### Phase 3: Replace host lookups with document registry and render cache

Goal: remove the old host-special-case document lookup and make rendering a document pipeline.

Deliverables:

- Attach a `DocumentHandle` to windows that display row documents.
- Resolve documents through an explicit registry or direct handle, not `WinId` comparisons.
- Replace `UiHost::display_rows_for_range`, `document_total_rows`, and `copy_document_range` as general lookup paths with document-handle calls.
- Add `RenderCache` keyed by document handle, generation, width, style/theme, renderer generation, and row range.
- Have window painting consume cached/materialized rows while document commands continue to use the document directly.
- Make resize re-materialize visible rows only and preserve anchors.
- Keep the current `TranscriptDocument<'_>` adapter in `crates/tui/src/app/transcript.rs` only as the seed to replace, not the final owner. The final `TranscriptDocument` owns store/cache/view state instead of borrowing `TranscriptView` per call.

Deletion targets:

- `TRANSCRIPT_WIN` branches in `TuiApp`'s `UiHost` implementation.
- `HostDisplayDocument` as the normal transcript adapter.
- App-level transcript cursor snapping and transcript-specific row total plumbing.
- Search code that scans a `HostDisplayDocument` from row zero to total rows.

Acceptance:

- Transcript, buffer, and static/test documents all use the same document lookup path.
- Rendering and command execution share generation and materialization state without coupling through a backing buffer.
- Width changes do not trigger global transcript layout.
- Existing user interactions pass through the new path without transcript-specific app branches.

### Phase 4: Complete exact search, copy, folds, and anchor correctness at scale

Goal: harden all semantic operations so virtual loading is invisible to users.

Deliverables:

- Implement search candidate lookup through SQLite FTS5 or explicit trigram/posting tables and exact local display refinement.
- Page search candidates around the current origin and prefetch bounded neighborhoods only.
- Remove `instr(text, ?)` full-table transcript search and generic `scan_search_matches` full display-row scans for document windows.
- Remove full in-memory transcript search-index construction as a hot path. Keep only bounded per-query/per-window caches.
- Implement large-range copy streaming through block records and exact chunk rendering.
- Implement fold/expand/collapse on stable block keys with generation invalidation.
- Implement drag autoscroll across unloaded ranges.
- Implement top, bottom, row, block, and search-match anchors with revalidation after exactification, resize, append, and fold changes.
- Add regression tests for multi-byte UTF-8, stale byte columns, soft/hard row breaks, selectable ranges, action spans, non-selectable chrome, and transcript visual paragraph behavior.

Deletion targets:

- Any remaining fallback that layouts the whole transcript for search, copy, fold, or navigation.
- Any semantic selection/copy path reading transcript text from the paint buffer.
- Any remaining approximations outside scrollbar extent and coarse scrollbar landing.

Acceptance:

- `gg` and `G` are exact without global layout.
- Search in huge sessions does not layout the whole transcript, scan every row, or build a full in-memory text index.
- First search result and `n`/`N` feel instant for indexed queries because SQLite returns bounded candidates and display refinement is local.
- Copy of a large selected range is exact and bounded by selected content, not full transcript size.
- Drag selection can cross materialized range boundaries without losing anchor correctness.
- Fold changes preserve cursor/selection/follow-tail semantics.

### Phase 5: Cleanup, compatibility isolation, and hard performance gates

Goal: delete old paths after the correct abstractions cover all hot operations, and prove the application scales to very large sessions.

Deliverables:

- Ensure provider-visible history, display transcript records, object-backed metadata, and request audit payloads remain distinct.
- Keep legacy JSON importers isolated at the storage boundary and `COMPAT`-tracked according to repo convention.
- Delete obsolete transcript compatibility adapters, old row cache paths, duplicate display-document concepts, and direct legacy hot load/read paths.
- Add instrumentation that records rows loaded, descriptors loaded, payloads loaded, history rows read, bytes hydrated, DB writes, dirty suffix size, render cache hit/miss, and exactified row count per operation.
- Add large-session regression benchmarks that fail when resume, request start, save, search, copy, resize, or scrollbar drag scale with total session size instead of requested range size.

Deletion targets:

- Display resume path that requires hydrated provider history.
- Any compatibility reader used as a hot runtime load/render path after migrate-on-open covers it.
- Any remaining code path that loads the whole session in memory except explicit export/import/repair commands.

Acceptance:

- Resume memory is proportional to visible transcript, sparse loaded ranges, active search/copy context, and bounded model-history needs.
- Save after no-op or one turn is independent of full session size except for SQLite index maintenance.
- Request start, request snapshot save, and tool-loop durability are incremental transactions.
- There is one canonical load/save/render/audit path.

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

Suggested benchmark command remains the session lifecycle benchmark, extended as needed:

```text
cargo xtask bench-transcript-session --runs 3 --bytes 10485760
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
