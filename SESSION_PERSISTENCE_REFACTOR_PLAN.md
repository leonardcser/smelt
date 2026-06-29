# Session persistence architecture rewrite plan

## Current status

Committed in this worktree before the larger rewrite:

- Context-token identity is persisted in the existing SQLite `accounting_json` envelope and restored through full and store-backed loads.
- Store-backed resume restores display-token identity, so the prompt bar does not mark an unchanged completed-turn token reading as stale.
- Visible token-usage updates mark the session dirty before persistence.
- Shutdown flushing now loops until session, live-history, and transcript-descriptor dirty work is clean, including descriptor-only transcript blocks.
- Regression coverage now covers queued-generation shutdown flushes, descriptor-only transcript persistence, and store-backed token identity resume.

Uncommitted work now implements the persistence architecture boundaries in this plan:

- `crates/tui/src/app/session_document.rs` introduces `SessionDocument`, typed `SessionMutation`s, document generations, save preparation, submit/ack handling, and document-level tests.
- `TuiApp` owns a single `TuiSessionDocument` for TUI persistence state. `transcript`, `live_session`, and `SessionPersistState` live under that document instead of as separate `TuiApp` fields.
- TUI call sites route persistence-affecting changes through semantic mutations: token usage, metadata, cwd, title, request input commit, history append/replace/truncate, transcript appends, streaming text/thinking, tool lifecycle, exec lifecycle, checkpoints, rewindable snapshots, turn finish state, and save acknowledgement.
- `TuiSessionDocument::apply(...)` is the production mutation seam. Callers submit intent; the document decides whether the mutation applies to full session state, store-backed live session state, transcript state, or parser plus transcript state.
- Full and store-backed load paths construct a document projection first, then install it into `TuiSessionDocument` with the matching persistence lifecycle state.
- Save planning is a one-call document operation from `TuiApp`: full/live saves call `TuiSessionDocument::prepare_save(...)`, and request-history appends call `TuiSessionDocument::prepare_request_history_append_save(...)`. The old production `PlanOnly` plus `BuildRequest` split is gone.
- Persistence lifecycle operations for submit, ack, failure, queued-save state, ephemeral clean marking, unflushed-work checks, loaded-session marking, materialization marking, history-resave marking, and pending-save test setup are routed through `TuiSessionDocument` methods instead of direct `TuiApp` manipulation of `SessionPersistState`.
- Targeted and workspace validation currently passes:
  - `cargo check -p smelt-tui --features harness`
  - `cargo clippy -p smelt-tui --all-targets --features harness -- -D warnings`
  - `cargo test -p smelt-tui --features harness session_document`
  - `cargo test -p smelt-tui --features harness persistence`
  - `cargo test -p smelt-tui --features harness normal_request_append_persists_touched_metadata_suffix`
  - `cargo test -p smelt-tui --features harness descriptorless_store_resume_backfills_bounded_tail_without_full_load`
  - `git diff --check`
  - `cargo fmt && cargo clippy --workspace --all-targets --features smelt-tui/harness -- -D warnings`
  - `cargo nextest run --workspace --features smelt-tui/harness`

The revised target keeps `core.session` as the core session projection. After completing the document boundary, moving `Session` itself into the TUI document did not reduce coordination enough to justify broadening the refactor into shared core/runtime ownership.

## Revised target architecture

The original greenfield target was a single `SessionDocument` owning all session state, including metadata, history, transcript, accounting, checkpoints, and dirty state. That direction is conceptually right, but moving `core.session` wholesale into a TUI document is not obviously the simplest next boundary because `smelt_core::Core` is shared by TUI and headless/runtime code.

The better near-term target is a TUI-owned document that owns all persistence-sensitive TUI state and uses `core.session` as the durable session projection:

```rust
struct TuiSessionDocument {
    transcript: TranscriptDocument,
    live_session: Option<LiveSession>,
    persist: SessionPersistState,
}
```

The production API should stay intent-based:

```rust
impl TuiSessionDocument {
    fn apply(
        &mut self,
        session: &mut Session,
        parser: &mut StreamParser,
        mutation: SessionMutation,
    ) -> MutationResult;

    fn plan_save(&self, session: &Session, blobs_pending: bool) -> SaveReadiness;
    fn mark_submitted(&mut self, submission: SaveSubmission) -> Option<u64>;
    fn mark_persisted(&mut self, session: &Session, ack: PersistAck) -> PersistAckResult;
    fn mark_failed(&mut self, failure: PersistFailure);
    fn has_unflushed_work(&self, session: &Session) -> bool;
}
```

`TuiApp` should not decide dirty ranges, descriptor suffixes, side-table suffixes, live-vs-full routing, or acknowledgement clearing. It should provide user intent, submit IO requests, and render projections.

Only after this boundary is clean should we reconsider moving `Session` itself under the document. If `core.session` still causes complexity after `transcript`, `live_session`, and `persist` are no longer independently addressable, then move `Session` in a separate, deliberate core/runtime refactor.

## Non-goals

- Do not build full event sourcing. We need a consistent semantic mutation/save boundary, not an append-only durable event log with replay, compaction, and projection versioning.
- Do not keep adding target-specific production APIs such as `SessionMutationTarget::Session`, `SessionMutationTarget::Transcript`, or `SessionMutationTarget::LiveSession`. Those are useful for unit tests only. Production code should call one runtime document mutation API.
- Do not preserve direct dirty-state manipulation as a compatibility surface. If code must touch dirty state, it belongs in the document.

## Core invariants

1. A visible transcript block cannot exist without a document-owned durable representation path.
2. History, transcript descriptors, metadata, accounting, checkpoints, and side tables are updated in memory by the same typed mutation where they are semantically coupled.
3. Runtime call sites submit semantic intent. They do not choose storage target or dirty bookkeeping.
4. A save plan is produced from one consistent document generation.
5. A save acknowledgement can only clear the exact generation it submitted.
6. Shutdown flushes until the document has no dirty work and no submitted work in flight.
7. Resume constructs the same document shape used at runtime, not a partial `Session` plus separately interpreted transcript and metadata fragments.
8. Old split state is deleted or made private once routed through the document.

## Mutation model

The mutation enum should stay concrete and practical. Split variants only when callers need different semantics.

Current mutation coverage includes small metadata/accounting mutations, history mutations, transcript mutations, streaming/parser mutations, checkpoints, rewindable state, request input commits, and turn finish state.

The persistence-sensitive turn lifecycle is represented by concrete mutations rather than a full event log:

```rust
enum SessionMutation {
    CommitRequestHistoryItem { item: HistoryItem, block: Option<Block>, first_user_message: Option<String> },
    FinishTurnState { history_len: usize, meta: TurnMeta, snapshot_context: bool, update_context_token_history_len: bool },
}
```

The important part is that the document atomically decides:

- which history items are committed,
- which visible blocks remain descriptor-only,
- which live blocks attach to history,
- which context snapshots and turn metadata are written,
- which document generation is dirty.

## Save plan model

The bridge had `prepare_save(request, PlanOnly)` and `prepare_save(request, BuildRequest)`. The implemented end state uses one document save-preparation call per save request so preflight and build cannot diverge.

Preferred end state:

```rust
enum SaveReadiness {
    Skip(SessionSaveSkipReason),
    Blocked(SaveBlockedReason),
    Ready(SessionSavePlan),
}

struct SessionSavePlan {
    generation: DocumentGeneration,
    kind: PersistSaveKind,
    state: Option<SessionState>,
    history_suffix: Option<HistorySuffix>,
    descriptor_suffix: Option<DescriptorSuffix>,
    side_table_suffixes: Option<SideTableSuffixes>,
    blobs: Vec<Blob>,
}
```

`TuiApp` should ask once, then submit the plan. It should not do a preflight call and then a second build call that can diverge.

## Phased implementation plan

### Phase 0: Bridge baseline

Status: completed and superseded by later phases.

Completed:

- Added `SessionDocument` and document-level mutation tests.
- Moved `SessionPersistState` out of `app.rs` into the document module.
- Added generation-aware save submit and ack handling.
- Moved save planning behind document methods.
- Routed token usage, usage accounting, metadata, title, cwd, checkpoints, history append/replace/truncate, transcript mutations, streaming/tool/exec parser mutations, and rewindable snapshot changes through typed mutations.
- Added `SessionDocument::apply_runtime(...)` so production callers no longer choose target-specific mutation APIs.
- Added regression coverage for metadata-only persistence and stale/in-flight save generations.

### Phase 1: Make TUI session document the runtime owner

Status: completed in the current unstaged work.

Completed:

- Added `session_document: TuiSessionDocument` to `TuiApp`.
- Moved `TranscriptDocument`, `Option<LiveSession>`, and `SessionPersistState` under `TuiSessionDocument`.
- Updated production and harness call sites to access TUI persistence state through `session_document`.
- Kept `core.session` outside for now, matching the revised target boundary.
- Preserved runtime mutation routing through the document seam.

Acceptance criteria:

- Production code no longer accesses `self.session_persist` directly.
- Production code no longer accesses `self.live_session.dirty` directly.
- Most production code reads transcript through document projection methods.
- Tests that need internals use explicit test helpers.

### Phase 2: Replace save preflight/build with one save planner

Status: completed in the current unstaged work.

Completed:

- Replaced production `prepare_save(..., PlanOnly)` plus `prepare_save(..., BuildRequest)` calls with one document save-preparation call per save.
- Full and live saves now call `TuiSessionDocument::prepare_save(...)` once.
- Request-history appends now call `TuiSessionDocument::prepare_request_history_append_save(...)` once after applying the semantic append; the document returns either a request append plan or a full/live history plan.
- Metadata update timing for save requests moved into document save preparation, so `TuiApp::save_session` no longer does a separate preflight before deciding whether to update runtime save metadata.
- Metadata-only, history, live-history, descriptor-only, and request-history append behavior remains covered by document and persistence tests.

Acceptance criteria:

- `TuiApp::save_session` asks the document once, then submits the returned request.
- No duplicated save-preparation path exists for preflight vs build.
- Metadata-only, history, live-history, descriptor-only, and request-history append saves are all covered by document tests.

### Phase 3: Move submit, ack, failure, retry, and flush ownership fully into the document

Status: completed by this rewrite.

Completed:

- Routed save submission through `TuiSessionDocument::mark_submitted(...)`.
- Routed save acknowledgement through `TuiSessionDocument::mark_persisted(...)`.
- Routed persist failure handling through `TuiSessionDocument::mark_persist_failed(...)`.
- Routed queued-save state and pending-save checks through `TuiSessionDocument` methods.
- Routed ephemeral clean marking and shutdown unflushed-work checks through `TuiSessionDocument` methods.

- Made `SessionPersistState` private inside `TuiSessionDocument`; production and tests use explicit document helpers instead of direct field access.
- Removed `TuiApp::begin_pending_save`; `TuiApp` only submits async-worker IO after the document records a submitted save.
- Kept pending-save inspection behind document methods so acknowledgements, retries, and shutdown flushes use one lifecycle owner.

Acceptance criteria:

- `TuiApp` does not inspect pending save state directly.
- Stale acknowledgements cannot clear newer work.
- Persist failures mark the correct full/live dirty range and queue retry through document code only.
- Shutdown flush loops on `document.has_unflushed_work(&session)`.

### Phase 4: Move turn lifecycle into document mutations

Status: completed for persistence-sensitive turn state.

Completed:

- User-visible request input commits use `SessionMutation::CommitRequestHistoryItem`, which atomically appends durable history, attaches the visible transcript block origin, sets first-user metadata when needed, snapshots metadata at the committed history boundary, and marks the correct dirty generation.
- Turn completion, cancellation, interruption, and resumable-error paths converge through `SessionMutation::FinishTurnState`, which records turn metadata, context snapshots, and context-token history length in one document mutation.
- Streaming text, thinking, tool, and exec lifecycle mutations already route through the document parser/transcript path, so mid-stream and active tool blocks have a descriptor persistence path before shutdown.
- Queued replacement preserves durable request commits through the same request-history append path and then finishes interrupted turn state through the document.

Acceptance criteria:

- `agent.rs` no longer directly coordinates first-user metadata snapshots with request history and visible transcript appends; it submits a request-input commit mutation.
- Interrupt with queued replacement, quit, and resume preserves visible blocks through document-owned descriptor and live-history save state.
- Mid-stream text, thinking, and tool blocks persist after shutdown.
- Completed-turn token usage remains non-stale after quit and resume.

### Phase 5: Delete or privatize old mutation surfaces

Status: completed.

Completed:

- `SessionPersistState` is private inside `TuiSessionDocument`.
- Production callers use `TuiSessionDocument::apply(...)`; target-specific `SessionMutationTarget` and `SessionMutationPersistence` are test-only or private implementation details inside `session_document.rs`.
- Direct descriptor dirty clearing and direct parser/transcript mutation calls are confined to document internals and document tests.
- Grep-based checks for production code pass: no direct `session_document.persist`, no production `live_session.dirty`, no production target-specific mutation API calls, no production `PlanOnly` or `BuildRequest`, and no `TuiApp::begin_pending_save`.

Remove or make private from production paths:

- direct `SessionPersistState` field access,
- direct `LiveSession::dirty` access,
- direct descriptor dirty generation clearing,
- direct parser mutations that take `transcript.history_mut()`,
- direct transcript descriptor persistence decisions outside the document,
- target-specific production mutation routing.

Grep-based acceptance checks for production code:

```text
No production call sites of:
- transcript.history_mut().clear_descriptor_dirty()
- live_session.dirty
- session_persist.session_dirty
- session_persist.dirty_history_from
- parser.*(transcript.history_mut())
```

Unit tests may still use lower-level helpers where they are explicitly testing document internals.

### Phase 6: Resume and materialization cleanup

Status: completed.

Completed:

- Store-backed and full loads construct `SessionDocument` projections and install them through `TuiSessionDocument::install_loaded_store_session(...)` or `install_loaded_full_session(...)`.
- Full materialization remains an explicit `COMPAT(legacy-session-full-load-fallbacks)` path for legacy display-only promotion, inspect details, and preview fallback cases.
- Descriptor windows, live suffix state, checkpoint state, metadata/accounting, transcript projection, and persistence clean/dirty state are installed together under the runtime document.
- Store-backed resume, render, save, rewind, and fork paths keep using live/store-backed history instead of materializing full history on the normal path.

Acceptance criteria:

- Full load and store-backed resume both install a runtime document field.
- Load paths do not separately reconstruct session metadata and transcript state in TUI code.
- Store-backed sessions do not materialize full history for normal render, save, rewind, or fork paths.

### Phase 7: Reconsider `core.session` ownership only if needed

Status: decided - keep `core.session` outside the TUI document.

Decision:

- The completed TUI document boundary removes the persistence hazards without moving shared core/runtime ownership.
- `core.session` remains the durable core projection used by non-TUI code.
- Moving `Session` into the TUI document would broaden the refactor into shared core APIs without a clear simplification payoff.

Acceptance criteria for moving `Session`:

- The move reduces code and removes coordination points.
- Headless/runtime code gets a simpler or equal boundary.
- The change is not just architectural symmetry.

## Tests to add or keep as gates

Document-level tests:

- Applying mutations produces matching history and transcript origins.
- Descriptor-only blocks are included in save plans.
- History replacement truncates side tables and descriptors consistently.
- Stale acknowledgement cannot clear a newer document generation.
- Runtime routing sends live history appends to `LiveSession` and streaming mutations to parser plus transcript.

Harness/E2E-style tests:

- Store-backed session, stream assistant text, quit, resume.
- Store-backed session, tool start/output/finish, quit, resume.
- Interrupt with queued replacement, quit before idle, resume.
- Rewind after descriptor-only blocks, save, resume.
- Compaction/checkpoint in store-backed session, save, resume.
- Metadata-only update after clean history save, resume.
- Completed-turn token usage remains non-stale after quit and resume.

Validation before landing substantial phases:

```bash
set -o pipefail; cargo fmt && cargo clippy --workspace --all-targets --features smelt-tui/harness -- -D warnings 2>&1 | tail -120
set -o pipefail; cargo nextest run --workspace --features smelt-tui/harness 2>&1 | tail -120
```

## Preferred end state

The rewrite is successful when:

- production code has one document mutation entry point,
- `TuiApp` no longer owns transcript, live session, and persistence state as independent mutable fields,
- save planning has one public entry point,
- save acknowledgements clear one document generation,
- shutdown flush has one document clean predicate,
- direct dirty flag manipulation is private or removed,
- resume constructs the same document shape used at runtime,
- no user-visible transcript block can exist without a document-owned durable representation,
- the resulting code is smaller at the coordination boundaries, easier to test, and harder to misuse.

