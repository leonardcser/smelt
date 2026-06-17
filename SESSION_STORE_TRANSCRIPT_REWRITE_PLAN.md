# Session Store and Progressive Transcript Rewrite Plan

## Purpose

This plan describes the greenfield rewrite of Smelt's session persistence, large-object handling, transcript source, and transcript projection architecture. The goal is to make resume, save, memory use, scrolling, search, selection, request inspection, and rendering scale to very large sessions while leaving the code simpler and less error-prone than the current design.

The immediate motivating profile is a resumed session whose `history.jsonl` is ~165 MB while the model-visible/user-visible content is closer to ~10-15 MB. The dominant cause is large tool/UI metadata, especially `edit_file` before/after file snapshots, stored inline inside durable conversation history.

## Principles

1. **Greenfield posture**
   - We are not constrained by deployed cache formats or current internal layouts.
   - We may break and replace intermediate representation caches, display caches, row-index caches, and internal APIs without compatibility shims.
   - The only compatibility promise is loading existing sessions for one or two releases.

2. **Session loading compatibility only**
   - Existing `meta.json` + `history.jsonl` and legacy `session.json` sessions must load through a compatibility importer.
   - Once imported, sessions should be saved in the new format.
   - Compatibility code should be isolated at the storage boundary and marked/removable according to the repo compatibility convention.

3. **Right abstractions over small patches**
   - Do not preserve an abstraction just because code already exists.
   - Split durable history, object storage, request audit, display projection, and UI selection into explicit concepts.
   - Prefer simple concrete types and direct ownership over clever generic layers.

4. **Do worthwhile large work now**
   - Do not defer a refactor only because it is large.
   - If a rename, split, or deletion makes the final architecture easier to reason about, include it.
   - Delete obsolete compatibility or cache paths as soon as the replacement is proven.

5. **Preserve interaction correctness**
   - Visible transcript rows must be exact.
   - Selection, copy, mouse drag, visual mode, `gg`/`G`, click-to-position, and autoscroll behavior must remain correct.
   - The allowed approximation is only the global scrollbar/total-height estimate in large transcripts.
   - Even for approximate scrollbars, clicking/dragging the bar must land near the intended transcript region and then refine locally.

6. **Plan is allowed to evolve**
   - This plan should be updated when implementation uncovers better facts.
   - Changes should be explicit and justified, not accidental drift.

7. **No duplicate half-abstractions**
   - Every new abstraction must have an explicit replacement target and deletion target.
   - Do not keep two concepts that both mean “display document”, “transcript source”, “row index”, or “session history”.
   - If an existing abstraction is close to the right one, promote and finish it instead of introducing a sibling.
   - If an abstraction is introduced in this plan, the plan must wire it through load, save, render, search, copy, tests, and deletion of the old path.
   - Transitional adapters are allowed only at boundaries and must have removal criteria.

## Current Implementation Checkpoint

Phase A is complete and implementation now happens in a real clean git worktree:

```text
/home/dev/dev/smelt/.worktrees/session-store-plan
branch: session-store-sqlite
base: current main
```

The restored Mac WIP snapshot is preserved for archaeology only:

```text
/home/dev/dev/smelt/.worktrees/session-store-reference-snapshot
```

Current implementation-branch state:

- The clean worktree builds with `cargo build`.
- The abandoned manifest/segment files are not present in the implementation branch.
- The durable target is one SQLite DB per session, with `meta.json` as a generated sidecar and old `session.json`, `history.jsonl`, and `requests.jsonl` as import-only inputs.
- The WIP snapshot remains useful only as a source of ideas and small portable pieces: storage-shape counters, benchmark cases, metadata externalization logic as importer logic, descriptor-record ideas, transcript-index tests, and persister coalescing ideas.
- The WIP snapshot does not compile as restored because `crates/tui/src/content/transcript_index.rs` uses `sha2`, but `crates/tui/Cargo.toml` does not declare `sha2`. Do not use that snapshot as an implementation base.

Architecture decision:

- The manifest/segment store and file artifact store are **not** the final direction. Treat them as implementation debt unless small pieces are cherry-picked into the SQLite design.
- Use the clean `session-store-sqlite` worktree for all new implementation work.
- Do not carry manifest/segment code forward as a second storage engine.

## Current Architecture Findings

### Persistence

Current durable state centers on `Session`:

- `Session.history: Vec<HistoryItem>` lives at `crates/core/src/session.rs:105`.
- Split session loading parses every JSONL line into memory at `crates/core/src/session.rs:1081`.
- Saving encodes the entire history into a single `Vec<u8>` before writing at `crates/core/src/session.rs:1011`.
- `save_with_blobs` may clone the whole session for blob externalization at `crates/core/src/session.rs:978`.
- Search sidecar generation walks all history at `crates/core/src/session.rs:1266`.

Problems:

- Full-history load is required before the UI can resume.
- Full-history rewrite is required for save.
- Full-history serialization is also used for change detection.
- Large non-model metadata is persisted inline in history.

### Request audit

Current request inspection uses append-only `requests.jsonl` written by the engine. The current log shape stores exact/near-exact provider `body` and also decomposed `messages`, `tools`, and `system_prompt`, which the measured report showed are near-duplicate large fields. This is why SQLite request audit is a first-class part of the storage rewrite, not an optional inspector cleanup.

Problems:

- Request logs were the largest measured state component.
- Listing requests requires scanning JSONL instead of querying indexed rows.
- The audit body is important and must be preserved, not dropped.

### Runtime duplication

The TUI duplicates large history into a transcript model:

- `build_transcript_from_session` walks the session and constructs `Transcript` at `crates/tui/src/app/history.rs:32`.
- Tool outputs are cloned into `ToolState` at `crates/tui/src/app/history.rs:246`.
- `sync_session_snapshot` clones the full session into `shared_session` at `crates/tui/src/app/history.rs:393`.
- `session_snapshot_for_persist` clones the full session at `crates/tui/src/app/history.rs:668`.
- `session_persist_fingerprint` clones and serializes the full session at `crates/tui/src/app/history.rs:816`.

Problems:

- Resume memory is multiple copies of the same logical session.
- Huge tool metadata is copied into transcript tool state even if not needed for rendering.
- Change detection allocates proportional to full session size.

### Provider context boundary

`history_to_messages` builds provider wire messages at `crates/protocol/src/history.rs:634`. For tool results, it uses only:

- `inv.result.content`
- `inv.result.is_error`

at `crates/protocol/src/history.rs:669`.

It does **not** send `ToolOutcome.metadata` to the provider. Therefore large metadata is durable/UI state, not model-visible conversation state.

### Transcript projection

Current transcript projection is partly lazy but still globally exact:

- `plan_projection_measured` calls `rebuild_row_index_with_env` before viewport planning at `crates/tui/src/content/transcript_buf.rs:1705`.
- `rebuild_row_index_with_env` collects all missing nodes and measures every missing height at `crates/tui/src/content/transcript_buf.rs:1310`.
- `plan_projection_from_prepared` uses exact `total_rows` and exact prefix rows at `crates/tui/src/content/transcript_buf.rs:1783`.
- Visible projection itself is bounded and materializes a node range at `crates/tui/src/content/transcript_buf.rs:1944`.

Problems:

- Visible rendering is lazy, but exact height calculation is global.
- Large sessions block on exact measurement even when only the tail is visible.
- Persisted display caches help only when keys match and the full exact row index is accepted.

### Existing UI seams that we should keep

The edit/window layer already has useful row-document primitives:

- `DisplayDocument` exists at `crates/edit/src/row.rs:83`.
- `MaterializedRows` captures `row_base`, `total_rows`, and `materialized_rows` at `crates/edit/src/row.rs:131`.
- `TuiApp` overrides transcript document operations through `UiHost` at `crates/tui/src/app/ui_host.rs:45`.
- Transcript copy delegates to `copy_document_range` at `crates/tui/src/app/ui_host.rs:73`.
- Row-based selection uses absolute `DocPosition` in `RowTextState` at `crates/edit/src/window/row_text.rs:38`.
- Mouse selection in materialized row mode uses absolute rows, maps them into the materialized slice, and copies via host `copy_document_range` at `crates/tui/src/app/mouse.rs:541`.

These are good seams. The rewrite should strengthen them rather than bypass them.

## Abstraction Ownership Map

The final architecture should have one owner for each concept. Each replacement must delete or demote the old abstraction before the rewrite is considered complete.

| Concept | Current/WIP abstraction | Final abstraction | Plan |
| --- | --- | --- | --- |
| Durable session state | `Session { history: Vec<HistoryItem> }`; WIP `SessionStore + HistoryLog` manifest segments | `SessionDb` in `smelt-store`, backed by `sessions/<id>/session.db` | Revert/remove manifest segments. `Session` becomes import/export compatibility, then runtime stops cloning full history. |
| Fast session list metadata | `meta.json` | `meta.json` sidecar generated from `session_state` | Keep as a small readable cache so resume lists do not open every DB. Regenerate if missing/stale. |
| Provider conversation | `HistoryItem` in memory / JSONL / manifest records | `history_items` rows containing canonical `HistoryItem` JSON | Keep provider-visible fields inline. Large non-provider metadata goes through object refs. |
| Request audit | append-only `requests.jsonl` duplicating `body` and `messages` | `request_attempts` rows + exact request/response objects | Preserve exact request body audit, store payload once, query metadata through indexes. |
| Large durable payloads | WIP file `ArtifactStore`; inline `ToolOutcome.metadata`; raw request JSON | SQLite `objects` table keyed by SHA-256 raw bytes | Port normalization ideas, not file layout. Compression is enabled in first implementation only if benchmark gate passes. |
| Renderable transcript blocks | `Transcript` + eager `BlockHistory`; WIP descriptor records/index | `transcript_blocks` rows + `TranscriptDocument` runtime cache | Salvage descriptor-record ideas, but store canonical descriptors in SQLite. |
| UI row document | `DisplayDocument` plus `UiHost` row methods | `DisplayDocument` as the explicit row-document contract, with `UiHost` as adapter | Promote existing seam. Do not add a second UI-facing transcript trait. |
| Transcript viewport/rendering | `TranscriptView` + `TranscriptProjection`; WIP `TranscriptDocument` wrapper | `TranscriptDocument` implementing/providing `DisplayDocument` | Keep useful wrapper work, but finish deletion path for old projection entry points. |
| Search | `content.txt` sidecar and full display scan | SQLite FTS/candidate search + local display refinement | Search returns block/history candidates first; active match materializes exact rows locally. |
| Copy/selection | row materialization + `copy_document_range` | same UI seam, backed by exact-on-demand ranges | Never copy from approximate rows. Exactify only selected range. |
| Display/layout cache | `session.ir.bin`, display cache, WIP `transcript.idx.bin` | disposable cache tables, new cache file, or in-memory cache | Preserve the performance role, not the old file format. Cache formats have no compatibility promise. Delete/rebuild freely. |

The most important correction is that the WIP `SessionStore`/`HistoryLog` is not the storage abstraction to finish. SQLite is the durable store. If a WIP type directly helps the runtime/display plan, port the idea under the SQLite design; otherwise remove it.

## Target Architecture

```text
sessions/<id>/
  meta.json                 # small generated sidecar for session lists/status
  session.db                # canonical durable store
  session.db-wal            # SQLite WAL, transient
  session.db-shm            # SQLite shared-memory, transient
  legacy/                   # optional import sources retained temporarily
    session.json
    history.jsonl
    requests.jsonl
```

```text
smelt-store crate
  ├─ SessionDb              opens/migrates one session DB
  ├─ SessionMeta            small metadata sidecar model
  ├─ HistoryStore           history_items + transcript_blocks
  ├─ ObjectStore            content-addressed objects, compression-gated
  ├─ RequestAuditStore      exact request audit + queryable metadata
  ├─ SearchStore            FTS/candidate indexes
  └─ LegacyImporter         session.json/history.jsonl/requests.jsonl import

TranscriptDocument (implements/provides DisplayDocument)
  ├─ TranscriptBlocks       lazy block descriptors loaded from SQLite
  ├─ RenderPlan             block/group ordering and presentation policy
  ├─ TranscriptHeightIndex  estimated global rows + exact measured visible rows
  ├─ RenderCache            bounded layout/render cache
  └─ ViewportMaterializer   exact visible rows + overscan
```

`DisplayDocument` is the UI-facing abstraction. `TranscriptDocument` is the transcript implementation of it. `TranscriptBlocks` is an internal concrete replacement for eager resumed `BlockHistory`, backed by `transcript_blocks` rows and on-demand history row materialization.

The key split is:

```text
model-visible history != UI/tool metadata != request audit payloads != rendered transcript rows
```

## Durable Storage Shape

The canonical storage format is one SQLite database per session:

```text
sessions/<id>/session.db
```

Use a small generated `meta.json` next to it for fast resume lists and migration/lock status. `meta.json` is not canonical; if missing or stale, regenerate it from `session_state`.

Recommended SQLite setup on open:

```sql
PRAGMA foreign_keys = ON;
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
PRAGMA busy_timeout = 5000;
PRAGMA temp_store = MEMORY;
```

### Library, typing, and migrations

Use `rusqlite` directly with bundled SQLite:

```toml
rusqlite = { version = "...", default-features = false, features = ["bundled"] }
```

Do not use an ORM or async SQL layer initially. The DB boundary is small and owned by `smelt-store`, so direct SQL and explicit row mapping is simpler than framework-generated models.

| Library | Type/story | Migration story | Dependency cost observed | Decision |
| --- | --- | --- | --- | --- |
| `rusqlite` | SQL written by hand, Rust structs mapped by hand | tiny custom runner or `rusqlite_migration` later | low, ~7 normal deps with bundled SQLite | **Use this** |
| `sqlx` | SQL-first compile-checked macros/generated row types possible | built-in migrations | high, ~80-90 normal deps | Too heavy |
| `diesel` | Rust-first typed query DSL/schema macros | Diesel migrations/CLI | medium, ~19 normal deps | ORM shape is more than needed |
| `sea-orm` | entity generation / entity-first ORM | migration DSL | very high, ~120+ deps | Overkill |
| `sqlite` crate | manual mapping, minimal wrapper | manual | very low, ~4 deps | weaker ecosystem than `rusqlite` |

Own a tiny migration runner:

1. Open DB and apply PRAGMAs.
2. `BEGIN IMMEDIATE`.
3. Read `PRAGMA user_version`.
4. Apply pending embedded SQL migrations in order.
5. Set `PRAGMA user_version = N`.
6. Update `store_meta`.
7. Commit.
8. Run `PRAGMA quick_check` where appropriate.

Use `store_meta` for app version, import source, migration status, and active writer lease metadata.

### Core schema summary

Core tables:

- `store_meta`: schema/app metadata, migration status, active writer lease fields.
- `session_state`: id, title, slug, cwd, mode/model, accounting, checkpoint, `revision`, `history_len`.
- `history_items`: ordered canonical `HistoryItem` JSON, hashes, model-visible/search metadata.
- `transcript_blocks`: lazy render descriptors by block/history index, kind, tool id/name, preview/search text.
- `objects`: content-addressed request bodies, raw responses, large metadata, blobs; `codec` supports `none` and optional `zstd`.
- `history_object_refs`, `request_object_refs`: reference tables for GC and diagnostics.
- `request_attempts`, `request_stats`: indexed request audit/inspector data.
- `turn_metas`, `turn_tool_elapsed`, `metadata_snapshots`, `accounting_snapshots`: resume/rewind snapshots.
- `transcript_search`: SQLite FTS where available.

Important indexes:

- history by `idx`, kind, and created timestamp
- transcript block by `block_idx`, `history_idx`, kind, and `tool_call_id`
- request attempts by timestamp, request id, turn/ask id, history length, provider/model, errors, background flag, and body size
- objects by kind and raw size

### Compression gate

Compression is not deferred by default. It is part of the first SQLite implementation **if the benchmark gate passes**.

Initial implementation work must benchmark:

- `codec = none`
- zstd low level, e.g. level 1 or 3
- representative request bodies and history metadata from the measured report
- write latency, resume/request-inspector latency, DB size, and CPU overhead

Enable zstd for large objects in the first implementation if it materially reduces storage without hurting interactive latency. The measured data suggests this is likely: large request/history samples compressed to roughly 22-37%, and request logs were ~62% of the large state directory.

Rules either way:

- Hash raw uncompressed bytes.
- Always support `codec = none` for small objects and compatibility.
- Store exact request bodies; compression must not change audit fidelity.
- If compression is enabled, threshold it by measured object size, not by guesswork.

### Request audit

The request audit store must preserve why `requests.jsonl` exists: exact debugging/audit. Store exact request body bytes once as an object, not both full `body` and full decomposed `messages`. Queryable fields in `request_attempts` are derived metadata, not a replacement for the exact payload.

### Multi-process session access

Do not support collaborative concurrent writing to the same session. Support many readers and one active writer.

If resuming a session already active elsewhere, show one concise warning-level notification. Match existing notification style by keeping the message lowercase and letting `MessageKind::Warning` provide the warning state:

```text
session open on {host} pid {pid}; opened read-only. fork or take over stale lock to continue.
```

User-facing options:

1. Open read-only for browsing/history/search/request inspection.
2. Fork session to continue safely from the same point. This should be the default write option.
3. Take over only when the writer lease is stale or the user explicitly confirms.

Internal rules:

- Use WAL mode, short transactions, `busy_timeout`, and one writer connection/actor per active session.
- Never hold a DB transaction while waiting on provider streaming or tool execution.
- Track active writer lease owner id, hostname, pid, app version, started time, and heartbeat time.
- Background migration skips active writer sessions.
- Request inspector and support commands open read-only when possible.
- Writes check `session_state.revision`/expected history length inside the transaction and abort on mismatch.

### Legacy migration policy

Startup launches a low-priority background migrator after first paint. It scans `sessions/*` and imports old formats into `session.db` without blocking UI startup. On-demand migration during resume is allowed, but not sufficient: dormant old sessions must migrate before legacy readers can be removed.

Import-only legacy inputs:

- legacy monolithic `session.json`
- current split `meta.json + history.jsonl`
- current `requests.jsonl`
- inline large `ToolOutcome.metadata`
- WIP manifest/segment code is reference-only; do not add an importer for it unless that format shipped to users.

Migration rules:

1. If `session.db` exists and passes schema/integrity checks, it is canonical.
2. If missing, import available legacy files.
3. Normalize large `ToolOutcome.metadata` into `objects`.
4. Import request audit into `request_attempts` and request/response objects.
5. Generate `transcript_blocks` and search rows.
6. Write/refresh `meta.json` from `session_state`.
7. Keep legacy files for at least one compatibility window.
8. Provide JSONL export before removing legacy readers. Do not add public migrate/doctor commands initially unless alpha feedback shows they are needed.

## Transcript Blocks and Display Document

Replace eager `Session -> Transcript -> BlockHistory` reconstruction with a concrete renderable block index used by both resumed sessions and live streaming.

```rust
struct TranscriptBlocks {
    generation: u64,
    order: Vec<RenderBlockId>,
    descriptors: Vec<TranscriptBlockDesc>,
    materializer: TranscriptMaterializer,
}

struct TranscriptBlockDesc {
    id: RenderBlockId,
    history_idx: Option<u64>,
    block_idx: u64,
    kind: BlockKind,
    content_hash: u64,
    sidecar_hash: u64,
    estimated_text_bytes: u32,
    estimated_rows: Option<u32>,
}
```

`TranscriptBlocks` is the replacement target for the resumed-session use of `BlockHistory`. It is loaded from SQLite `transcript_blocks` descriptor rows for resumed sessions and appended in memory for live streaming until the writer commits those descriptors. Those are construction details, not separate UI abstractions.

`TranscriptDocument` owns `TranscriptBlocks`, `RenderPlan`, `TranscriptHeightIndex`, `RenderCache`, and viewport materialization. It is the transcript implementation of the existing `DisplayDocument` contract:

```rust
impl DisplayDocument for TranscriptDocument {
    fn snapshot(&mut self) -> DisplaySnapshot;
    fn materialize(&mut self, range: Range<RowIndex>) -> DisplayRows;
    fn copy_range(&mut self, range: TextRange) -> Option<CopyOutput>;
}
```

Important: the transcript renderer should request full content only for:

- visible + overscan blocks
- active search candidates
- explicit copy ranges
- background indexing work

Do not leave both `BlockHistory` and `TranscriptBlocks` as long-term runtime models. During migration, `BlockHistory` may be an adapter for existing renderers; the completion criterion is that render planning, display layout, search, copy, and live streaming all use `TranscriptBlocks`/`TranscriptDocument` directly.

## Progressive Height Index

The current exact row index should be replaced by an approximate-first height index.

```rust
struct TranscriptHeightIndex {
    key: HeightIndexKey,
    nodes: Vec<NodeHeight>,
    prefix: FenwickTree<RowIndex>,
    exact_count: usize,
    total_estimated_rows: RowIndex,
}

struct NodeHeight {
    id: RenderNodeId,
    key: NodeLayoutKey,
    estimated_rows: RowIndex,
    exact_rows: Option<RowIndex>,
    source: HeightSource,
}

enum HeightSource {
    PersistedExact,
    MeasuredVisible,
    MeasuredIdle,
    CalibratedEstimate,
    Heuristic,
}
```

The prefix tree stores the best current height for each node: exact if known, otherwise estimate. Updating one exact height is `O(log n)`, and row-to-node lookup is `O(log n)`.

### Rules

- Small transcripts can still exact-measure everything immediately.
- Large transcripts render after measuring only visible + overscan blocks.
- Global `total_rows` may be estimated for large transcripts.
- Visible rows and copy/selection ranges are exact.
- Idle time can exactify more nodes and improve scrollbar accuracy.

### Estimation inputs

Use, in order:

1. Persisted exact height for matching content/render key.
2. Previous exact height for same block/content at nearby width, scaled conservatively.
3. Calibrated per-kind estimate from measured samples in this session.
4. Static heuristic by block kind.

Heuristics should be simple and explainable:

- `CodeLine`: close to exact from line count and width.
- `Text`/`Thinking`: newline count + cell-width approximation.
- `User`: text width approximation + image labels.
- `ToolCall`: summary/status + capped output estimate.
- `Group`: group renderer estimate or sum child estimates.
- Folded/trimmed states apply `ViewState::measured_height` to the expanded estimate.

## Scrollbar and Navigation Semantics

### What may be approximate

For large transcripts only:

- scrollbar thumb size
- scrollbar thumb position
- click/drag mapping from terminal row to approximate transcript region

### What must be exact

- Rendered visible rows.
- Cursor row and byte column for visible rows.
- Selection start/end once rows are materialized.
- Copy output for selected ranges.
- `gg`/top and `G`/bottom semantics.
- Page/mouse wheel movement once anchored in visible materialized rows.

### Scrollbar click/drag flow

1. Convert clicked thumb position to an estimated row using current estimated total.
2. Map estimated row to nearest node through `TranscriptHeightIndex`.
3. Materialize and exact-measure around that node with overscan.
4. Re-anchor to exact block id + row offset.
5. Render visible rows.
6. Update the height index with measured exact heights.

This means scrollbar jumps become self-correcting. The first landing may be approximate in very large sessions; the final visible rows are exact.

### `gg`, `G`, and explicit row jumps

- `gg` maps to node 0 / row 0 exactly. No approximation required.
- `G` maps to tail by measuring backward from the end until the viewport is filled. No need for exact total height first.
- `GotoRow(row)` maps through the estimated height index, then refines locally.

### Mouse selection and drag autoscroll

The current row-selection model is already compatible with materialized absolute rows:

- `RowTextState.cursor` and `selection_anchor` are absolute `DocPosition`s.
- `handle_row_mouse` maps visible mouse coordinates to absolute rows using `scroll_top` and the current materialized range.
- Copy delegates to `copy_document_range`, which can materialize the selected range exactly.

Required rewrite rules:

1. During drag selection, keep the anchor as a stable document coordinate:
   - block id + row offset + byte column when available
   - absolute estimated row only as fallback
2. If autoscroll moves outside the current materialized range, materialize the next range before updating the drag endpoint.
3. Never copy from approximate rows. `copy_range` must exact-materialize all selected rows or stream block content through a copy path.
4. Word/line selection break data must come from the same materialized rows as the hit-test, matching the current warning in `crates/tui/src/app/mouse.rs:594`.

## Search Semantics

Search should not require full transcript layout.

New model:

1. Search textual history/index first.
2. Produce candidate block/history ids.
3. Materialize display rows only around candidates as needed.
4. Use exact local row positions for the active match.

Current exact search layout lives at `crates/tui/src/content/transcript_buf.rs:2151`. Replace this with a search index plus local display refinement.

## Copy Semantics

Copy has two modes:

1. **Visible/small range copy**
   - Use exact materialized display rows.
   - Preserve `copy_as`, soft-wrap merging, non-selectable spans, and row breaks.

2. **Large range copy**
   - Do not build the whole transcript buffer.
   - Stream through block content and exact-render chunks as needed.
   - Fetch large tool data through object references only if the user explicitly copies the display that exposes it.

Current transcript copy forces row-index rebuild at `crates/tui/src/content/transcript_buf.rs:2385`. The new copy path must exactify the selected range only.

## Caches, Compatibility, Migration, and Worktree Hygiene

### Breakable caches

These can be replaced without version compatibility:

- `session.ir.bin`
- `content.txt`
- WIP `transcript.idx.bin`
- display layout cache
- row index cache
- renderer IR cache
- any future display/height cache tables

If a cache format changes, delete/rebuild it. Do not preserve cache readers as compatibility debt.

`session.ir.bin` deserves special handling in the plan because its purpose is still valid. It was added to avoid rerunning expensive Lua/tool rendering across the whole transcript: keep enough intermediate representation and measured layout information to compute block heights quickly, then render only the visible viewport. The SQLite rewrite keeps that performance goal, but does not keep the old file as durable storage.

New rule:

- Do not migrate `session.ir.bin`.
- Do not treat missing or stale `session.ir.bin` as data loss.
- Preserve the role as a disposable render/layout cache behind the new SQLite-backed transcript descriptors.
- Rebuild or replace it using `history_items`, `transcript_blocks`, `objects`, renderer version, width, theme, and Lua renderer generation.
- The replacement can be SQLite cache tables, a new cache file, or in-memory-only at first. The exact cache shape is an optimization choice, not a compatibility promise.

Losing this cache may make first render or first scroll through old regions slower, but it must never lose conversation content, tool metadata, or request audit data.

### Compatibility importers

Keep importers only at the storage boundary for:

- legacy monolithic `session.json`
- current split `meta.json` + `history.jsonl`
- current append-only `requests.jsonl`
- inline large `ToolOutcome.metadata`
- WIP manifest/segment code is reference-only; do not add an importer for it unless that format shipped to users.

Importer responsibilities:

1. Detect available legacy inputs without loading more than necessary.
2. Import provider-visible history into `history_items`.
3. Move large non-provider metadata into `objects` and store refs in history JSON/ref tables.
4. Import request audit into `request_attempts` and request/response objects while preserving exact request bodies when present.
5. Generate `transcript_blocks` descriptors and search text.
6. Write/refresh `session_state` and `meta.json`.
7. Leave legacy files untouched or move them to `legacy/` only after a verified import marker exists.
8. Keep compatibility code isolated and removable.

### Background migration

On startup, launch a low-priority migration worker after the UI can paint. It scans the session root and migrates sessions missing a valid `session.db`. On-demand migration during resume is not enough because old conversations may not be resumed until after legacy loaders have been removed.

No public migrate/doctor command is planned for the first pass. Because Smelt is still alpha, the normal path is automatic background migration plus keeping legacy readers for a short compatibility window. Add explicit migrate/doctor commands only if alpha feedback shows users need manual recovery.

Planned export commands:

- export canonical DB history as JSONL
- export request audit rows in the old inspector-friendly JSONL shape

### Worktree/rebase policy

The restored `.worktrees/session-store-plan` snapshot is useful for archaeology but is not a real worktree. Before implementation:

1. Preserve the snapshot or commit it on a disposable WIP branch for reference.
2. Create a clean real worktree from current `main`/`origin/main`.
3. Rebase/cherry-pick only useful WIP pieces onto that clean branch.
4. Do not carry manifest/segment code forward as a second storage engine.
5. Fix build before architectural work; the current snapshot fails because `smelt-tui` uses `sha2` without declaring it.

## Measured State Evidence

A real large state directory report from another machine strongly supports the SQLite pivot.

```text
state dir:        4.49 GB
sessions:         1,649
session files:    4,932

requests.jsonl:   2.77 GB  (~62% of state)
history.jsonl:    923 MB   (~21%)
session.json:     689 MB   (~15%)
session.ir.bin:   61 MB
content.txt:      18 MB
prompt history:   4.3 MB
recent.json:      86 B
```

Request-log field bytes:

```text
messages:       1.36 GB
body:           1.35 GB
tools:          34 MB
system_prompt:  21 MB
response:       2.6 MB
usage:          326 KB
error:          34 KB
```

History field bytes:

```text
tool_result_metadata_json_bytes: 661 MB
tool_result_content_bytes:       151 MB
assistant_reasoning_blocks:       63 MB
tool_arguments:                   19 MB
assistant_reasoning:               7.8 MB
assistant_text:                    3.4 MB
user_text:                         1.4 MB
```

Implications:

- Request audit is the largest storage consumer, so preserve it while removing duplicated `body`/`messages` bytes.
- History bloat is mostly non-provider tool metadata, so object normalization should target metadata first.
- Prompt recall and `recent.json` are not architectural drivers.
- Compression probably helps and must be measured as part of the first SQLite implementation; if the gate passes, enable it immediately for large objects.

## Implementation Phases

### Phase A: Recover, rebase, and triage WIP

Status: complete. The implementation branch is a real clean worktree, the preserved WIP snapshot is reference-only, the clean worktree builds, and the manifest/segment store is not present in the implementation branch.

Deliverables:

- Make implementation happen in a real git worktree rebased onto current `main`/`origin/main`.
- Preserve the rsynced Mac snapshot for reference.
- Fix or record the current build break (`sha2` missing from `smelt-tui`) before using any WIP branch as a base.
- Categorize WIP files:
  - **discard/revert**: manifest `SessionStore`, `HistoryLog`, `.history_segments`, file artifact store layout, segment compaction
  - **port/adapt**: history-shape counters, metadata externalization/import logic, transcript descriptor records, transcript cache tests, benchmark harness changes, persister coalescing ideas
  - **defer**: progressive height/selection/search pieces not needed for first SQLite storage cutover

Acceptance:

- Clean worktree builds.
- There is one planned durable-storage target: per-session SQLite.
- No custom manifest/segment store remains in the implementation branch except possibly as isolated importer code for real WIP data recovery.

### Phase B: `smelt-store` crate and SQLite foundation

Status: complete. The `smelt-store` crate exists with a concrete `SessionDb`, bundled `rusqlite`, a v1 migration runner, initial schema, object hash/read/write helpers, active writer lease metadata helpers, read-only open mode, singleton `session_state`, generated `meta.json` sidecar support, and unit coverage for create/reopen, read-only reopen, object dedup, lazy object metadata, no-compression roundtrip, zstd-compression roundtrip, compression gate behavior, sidecar generation, writer lease roundtrip, and corrupt DB errors. Compression uses an explicit `ObjectCompression` enum and is enabled for large objects with zstd level 1 only when the compressed size saves at least 15%. `cargo xtask bench-store-compression STATE_DIR` samples real `requests.jsonl` and `history.jsonl` payloads and reports zstd level 1/3 size and latency so the policy can be validated against local state before cutover.

Deliverables:

- Add concrete `smelt-store` crate using `rusqlite` with bundled SQLite.
- Add `SessionDb` open/create/migrate logic.
- Add tiny in-house migration runner using `PRAGMA user_version` plus `store_meta`.
- Add schema tables: `session_state`, `history_items`, `transcript_blocks`, `objects`, `request_attempts`, `request_stats`, snapshot tables, and search table.
- Add object hash/read/write helpers.
- Add compression benchmark gate and implement zstd for large objects if it passes.
- Add active-writer lease metadata and read-only open mode.
- Add sidecar `meta.json` generation from `session_state`.
- Add unit tests for schema creation, migrations, object roundtrip, compression/no-compression roundtrip, and broken/corrupt DB handling.

Acceptance:

- A new empty session DB can be created and reopened.
- Large objects are content-addressed by raw bytes.
- Compression decision is backed by measured data, not a guess.
- DB open failures are surfaced without falling back to a second write format.

### Phase C: Legacy import/export

Status: complete at the storage boundary. `smelt-store` imports current split `meta.json + history.jsonl`, legacy monolithic `session.json` with typed `history`, legacy monolithic `session.json` with provider `messages` converted through `protocol::history_from_messages`, and current `requests.jsonl`. Import streams split history rows, refuses to overwrite non-empty DBs, normalizes large inline `metadata` JSON values into content-addressed `objects`, stores refs in normalized history rows without duplicating payloads in snapshot tables, generates `transcript_blocks` and `transcript_search`, stores canonical request body/response/error objects, and exports history/request JSONL with metadata and request payloads rehydrated. Unit tests cover split import/export, monolithic import, legacy messages conversion, request metadata listing, request body export, non-empty import refusal, UTF-8-safe search text truncation, and lazy object payload retrieval. Provider-builder cutover to DB cursors remains Phase E.

Deliverables:

- Import legacy `session.json`.
- Import current `meta.json + history.jsonl`.
- Import current `requests.jsonl`.
- Normalize large inline `ToolOutcome.metadata` into `objects`.
- Generate `transcript_blocks` and search rows during import.
- Export DB history/request audit back to JSONL for inspection and support.

Acceptance:

- Existing local sessions roundtrip through the DB importer.
- Imported provider-visible history produces the same provider messages.
- Imported request audit preserves exact request bodies when `body` exists in the source log.
- Inspector can list imported request metadata and lazily fetch full bodies.

### Phase D: Request audit writer cutover

Status: complete. Live engine request audit now opens the per-session `session.db` and appends request attempts through `SessionDb::append_request_attempt` instead of appending `requests.jsonl`. Request bodies, parsed responses, and errors are stored as content-addressed objects with indexed request metadata, inline response/error summaries, token/cost stats, and lazy payload loading through `request_payloads`. `RequestAuditQuery` supports filtering by time, request id, turn/ask id, provider/model, errors, body size, token minimums, and cost minimums. The inspector reads SQLite metadata for session-list stats and uses DB JSONL export for request detail when `session.db` exists; legacy `requests.jsonl` remains an import/fallback input only.

Deliverables:

- Replace new `requests.jsonl` writes with `RequestAuditStore` writes to SQLite.
- Store exact serialized request body bytes once in `objects`.
- Store response/error summaries inline and raw response/error bodies as optional objects.
- Add query APIs for request inspector: by time, request id, turn/ask id, provider/model, errors, size, and token/cost stats.
- Keep optional debug export, not a second live audit log.

Acceptance:

- New sessions no longer append `requests.jsonl` by default.
- Inspector does not need to read a hundreds-of-MB JSONL file for listing.
- Full body/response expansion is lazy by row/object.
- Audit fidelity is preserved.

### Phase E: Session save/load cutover

Deliverables:

- Save new session history into `history_items` and `transcript_blocks`.
- Update session metadata/accounting snapshots in DB and generated `meta.json`.
- Replace full-history save rewrite with row append/update/delete transactions.
- Enforce one active writer per session through leases and revision checks.
- Route provider request building through ordered history rows or a bounded history cursor instead of cloned full `Session` snapshots.

Acceptance:

- New saves use `session.db` as canonical storage.
- No-op save does not rewrite history or request audit payloads.
- Save after one turn writes only new/changed rows and objects.
- Resume can read session metadata without parsing full history.
- A second process opening an active session is read-only, forked, or explicitly takes over a stale lease; it never silently writes concurrently.

### Phase F: Background all-session migration

Deliverables:

- Startup migration worker scans all sessions and imports missing DBs in the background.
- JSONL export commands exist for history and request audit.
- Resume-list UI can show migration status or warnings for failed sessions.
- Failed migrations are retryable and never delete legacy data.
- Batch migration has bounded logging and progress counters.

Acceptance:

- A state directory with old sessions gradually becomes DB-backed without opening each session manually.
- Removing legacy readers after the compatibility window will not strand dormant conversations that migrated successfully.
- No public migrate/doctor command is required for the first pass; add one only if alpha feedback shows a real recovery need.

### Phase G: Lazy transcript descriptors

Deliverables:

- Port useful WIP `TranscriptBlocks` descriptor-record ideas to DB-backed `transcript_blocks`.
- Define/finish `TranscriptDocument` as the transcript implementation of `DisplayDocument`.
- Replace resume path's eager `build_transcript_from_session` with descriptor loading from SQLite.
- Move render planning to consume descriptors instead of requiring fully materialized history.
- Materialize blocks/tool state on demand for visible rendering, copy, and search refinement.

Acceptance:

- Resume does not clone every history item into `BlockHistory`.
- Rendering the tail of a large session materializes only visible + overscan blocks.
- There is a clear deletion path for `build_transcript_from_session`, resumed `BlockHistory`, and old projection entry points.

### Phase H: Progressive height, navigation, copy, and search

Deliverables:

- Replace exact-only row indexing with `TranscriptHeightIndex`.
- Estimate missing heights and exact-measure visible + overscan only before first paint.
- Harden stable selection anchors, drag autoscroll, copy exactification, and word/line selection across materialized ranges.
- Search through SQLite candidates first, then local exact display refinement.

Acceptance:

- Large resume paints without measuring every block.
- Visible rows and copy output are exact.
- Scrollbar is approximate but stable and refines after navigation.
- Search in huge sessions does not layout the whole transcript.

### Phase I: Cleanup and compatibility removal

Deliverables:

- Delete manifest/segment store code and file artifact store code after SQLite import paths replace them.
- Delete old `session.ir.bin` writer/reader or keep it as a disposable cache only.
- Delete old full row-index cache paths.
- Delete eager resume transcript build path.
- Delete old full-history save path.
- Remove `requests.jsonl` live writer.
- Remove legacy JSON importers after the compatibility window and migration/export requirements are satisfied.

Acceptance:

- Architecture has one clear path for load/save/render/audit.
- Compatibility code is isolated, documented, and then removed on schedule.
- Code is simpler than before: fewer full clones, fewer global rebuilds, fewer cache validity branches, and no custom database implemented in files.


## Risks and Design Responses

### Risk: approximate height breaks selection

Response:

- Approximation never supplies text/copy data.
- It only maps coarse navigation targets to nearby nodes.
- Selection operates on exact materialized rows and stable anchors.
- Copy exactifies selected rows before producing output.

### Risk: scrollbar feels jumpy after refinement

Response:

- Use calibrated estimates and persisted exact samples.
- Keep local anchor stable by block id + row offset after measuring.
- Update scrollbar smoothly after render; do not move visible content unexpectedly once anchored.

### Risk: Lua renderers make persisted heights invalid

Response:

- Exact persisted heights require matching renderer cache key.
- Missing/unstable renderer cache keys fall back to estimates + visible exact measurement.
- Cache format itself is disposable.

### Risk: object storage complicates tools

Response:

- Keep provider `ToolOutcome` small and simple.
- Make object storage a UI/session-store concern.
- Provide helper APIs so tools can return large data without knowing DB layout.

### Risk: request audit loses fidelity

Response:

- Store exact serialized request body bytes as objects.
- Keep queryable request metadata derived from the body, not as a replacement for it.
- Provide export commands for JSONL-compatible audit workflows.

### Risk: compression costs more than it saves

Response:

- Compression is benchmark-gated in Phase B.
- Enable zstd only for measured large-object thresholds that materially reduce storage without hurting interactive latency.
- Always keep `codec = none` support.

### Risk: two processes resume the same session

Response:

- One active writer lease per session.
- Concurrent resumes open read-only by default and show a one-line notification.
- Continuing from an active session requires fork or explicit stale-lock takeover.
- Writes check revision/history length in the transaction and never silently merge divergent conversations.

### Risk: too many transitional adapters

Response:

- Each phase must include deletion criteria.
- Adapters are allowed only at boundaries and should not leak into new core types.

## First Concrete Milestone

The first milestone should prove the SQLite direction on the motivating session and the measured multi-session state directory:

1. Start from a clean rebased worktree; port only useful WIP pieces.
2. Import old session formats into `session.db`.
3. Preserve exact request audit bodies while eliminating duplicated stored `body`/`messages` payloads.
4. Move huge inline tool metadata into DB `objects`.
5. Benchmark object compression and enable it for large objects if the gate passes.
6. Store compact history rows and transcript descriptors.
7. Resume into DB-backed transcript descriptors.
8. Paint tail without global exact measurement.
9. Preserve selection/copy for visible rows.
10. Save without full-history rewrite.
11. Run background migration over dormant sessions.

Success target:

- Resume memory roughly proportional to visible transcript + compact descriptor/history window, not raw historical metadata or request-log size.
- Save after no-op or one turn is near-constant in relation to full session size.
- First paint does not require measuring every transcript block.
- Request inspector lists requests without scanning giant JSONL files and lazily loads full bodies/responses.
- Dormant legacy sessions are migrated before legacy compatibility is removed.

## Notes from Code Reconnaissance

- `DisplayDocument` in `crates/edit/src/row.rs:83` is the row-document abstraction to promote and finish. Do not introduce a separate UI-facing transcript document trait.
- `MaterializedRows` in `crates/edit/src/row.rs:131` is the right shape for visible row windows. It may need an explicit `total_rows_is_estimated` bit or a richer `DocumentExtent` type.
- `UiHost` transcript overrides in `crates/tui/src/app/ui_host.rs:45` are the current integration point for lazy row access, total rows, and copy.
- `RowTextState` in `crates/edit/src/window/row_text.rs:38` already distinguishes document rows from backing-buffer rows. This is important for cross-window selection correctness.
- `handle_content_mouse` in `crates/tui/src/app/mouse.rs:541` already delegates materialized row copy through `copy_document_range`, which is the correct seam for exactifying copy ranges.
- Current transcript `display_rows_for_range` and `copy_range` force full row-index rebuilds. These should become range-exactifying operations.
- Current search layout is exact/global. Replace it with candidate search + local display refinement.
