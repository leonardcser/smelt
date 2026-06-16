# Transcript Render Nodes, Folding, and Lua Grouping Plan

## Implementation principles

- Treat this as greenfield API design. There are no external consumers to preserve, so choose the public Lua API, Rust internal API, names, and data model that should exist at the end.
- Prefer the simpler final architecture over incremental compatibility. Do not keep shims, duplicate paths, or awkward names just to reduce the size of an intermediate diff.
- Take worthwhile refactors now: rename `block_*` concepts to `node_*` where they are render-node concepts, consolidate duplicated cache/index logic, and move state to the owner that makes the invariant easiest to reason about.
- Disk/session/cache formats may change. Use the normal release/migration path where needed, but do not contort the design around old serialized shapes.
- Do not defer a change just because it is larger. Defer only when the information is genuinely missing or when the smaller step produces the same clean end state.
- Optimize for the codebase after the work lands: clear ownership boundaries, one abstraction per concept, deterministic cache behavior, and easy-to-test planning/rendering seams.

## Goals

- Keep transcript history flat and semantic.
- Add display-only grouping for repetitive adjacent transcript items.
- Make grouping policy and group rendering live in Lua, not hard-coded Rust rules.
- Reuse one collapse/expand model for ordinary blocks and dynamic groups.
- Preserve transcript virtualization: render one display node at a time, not the whole transcript.
- Make the grouping API simple enough that built-in tools and user config can define new groups without Rust changes.

## Non-goals

- Do not add `Block::Group` to durable transcript history.
- Do not make Lua own viewport planning or full-transcript rendering.
- Do not implement tool-specific group behavior in Rust except for exposing enough semantic fields to Lua.
- Do not group different tool families by default: `read_file` groups with `read_file`; `grep` groups with `grep`; `glob` groups with `glob`; mixed read/search sequences stay separate unless user config opts in.
- Do not preserve old internal names, cache formats, or session shapes when replacing them produces a cleaner model. Handle format changes deliberately through release/migration mechanics instead.

## Current shape

- `BlockHistory` stores a flat ordered list of `BlockId`s.
- Rendering currently compiles one history block into one `LayoutIr`.
- `ViewState` already supports `Expanded`, `Collapsed`, `TrimmedHead`, and `TrimmedTail`, but it is keyed by `BlockId`.
- Lua already owns default transcript rendering and tool body rendering, so group rendering should follow the same pattern.

## Schema and API posture

Assume the transcript/rendering API is not constrained by existing consumers. When the plan says to introduce render nodes, node layout keys, typed process-status events, or separate presentation state, implement those as the canonical shapes rather than compatibility layers around old block-only names.

Acceptable breaking changes:

- serialized display layout caches;
- persisted row-index caches;
- session/history schema for typed process-status events;
- presentation-state schema for manual view overrides;
- internal Rust APIs under `content/display_layout.rs` and `content/transcript_buf.rs`;
- Lua transcript customization APIs before they are documented as stable.

Use migrations or cache invalidation at release boundaries where needed, but target a single clean representation after migration.

## Final architecture summary

- `BlockHistory` stays semantic and flat. It does not contain groups or presentation-only fold state.
- `TranscriptPresentationState` owns manual `ViewState` overrides keyed by `RenderNodeId`.
- Thinking is modeled as ordinary collapsible transcript content: thinking block nodes render their full content, default to collapsed through presentation policy, and expand/collapse with the same node APIs and bindings as every other block/group.
- A config-driven default-view policy map replaces one-off booleans such as `show_thinking`: users can set defaults by block kind, tool name, or group name without changing semantic history.
- Lua registers virtual group node types with `smelt.transcript.groups.register { name, selector, bucket?, default_view?, cache_key, render }`.
- Rust stores the declarative registry fields, builds a `RenderPlan` by maximal adjacent run batching, and never asks Lua to plan the viewport.
- Render/layout/projection caches are keyed by `RenderNodeId`; semantic transcript operations continue to use `BlockId`.
- Group renderers are ordinary Lua transcript renderers that receive a group snapshot and return a `smelt.layout` tree.
- Background process completion grouping uses typed process-status events, not text parsing or generic metadata.

## Target shape

Add a display planning layer between semantic transcript history and projection:

```text
BlockHistory
  + GroupRegistry from Lua registrations
  + TranscriptPresentationState
  -> RenderPlan Vec<RenderNode>
  -> DisplayModel cache keyed by RenderNodeId
  -> Lua render(node, ctx)
  -> LayoutIr
  -> transcript projection / viewport
```

Keep three concepts separate:

- `BlockHistory`: semantic transcript blocks and mutable semantic sidecars.
- `GroupRegistry`: Lua-defined virtual node types, each with a selector, optional bucket, default view, and renderer.
- `TranscriptPresentationState`: manual fold/view overrides for render nodes.

A render node is either a normal block or a virtual group over an adjacent run of blocks:

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct GroupRuleId(u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct GroupBucketId(u32);

enum RenderNodeId {
    Block(BlockId),
    Group(GroupId),
}

struct GroupId {
    rule: GroupRuleId,
    bucket: GroupBucketId,
    first_child: BlockId,
}

struct RenderPlan {
    history_generation: u64,
    nodes: Vec<RenderNode>,
    fingerprint: u64,
    rules_generation: u64,
    rules_cache_key: Option<u64>,
}

enum RenderNode {
    Block {
        id: BlockId,
        block_index: usize,
    },
    Group {
        id: GroupId,
        rule: GroupRuleId,
        bucket: GroupBucketId,
        child_range: std::ops::Range<usize>,
    },
}
```

Groups are virtual and recomputed as a run-length projection over the flat transcript. `child_range` indexes into `BlockHistory.order`, and Lua child snapshots are materialized only when a group node is rendered. A `RenderPlan` is valid only for its recorded `history_generation`; child access should go through `RenderPlan` helpers that check or assert the generation before indexing into `BlockHistory.order`. Transcript history remains semantically flat. The block/session schema can still change where the cleaner model needs richer semantic data, such as typed process-status events.

Internally, group rules and buckets should be interned ids (`GroupRuleId`, `GroupBucketId`) for cache/index hot paths. Lua snapshots and debug output convert them back to registration names and bucket strings.

`GroupId` should be stable enough for manual folding while a group grows. `rule + bucket + first_child` is a good initial key because appending more matching children keeps the same manual override. The plan fingerprint/cache key still includes the full child range, child ids, and child hashes, so stale layouts are invalidated even when the id stays stable.

## Ownership split: Lua virtual node types, Rust run batching

Lua should own grouping policy and rendering. Rust should own deterministic run batching because projection, row indexing, cache hydration, and viewport materialization need cheap stable node ids and cannot call arbitrary Lua for every row operation.

That means:

- built-in grouping rules are registered by Lua modules, not hard-coded as Rust semantic cases;
- each group registration defines a virtual transcript node type: selector, optional bucket, default view, and renderer;
- Rust stores the registered declarative planner fields in a host-side registry;
- Lua stores the renderer keyed by group name and keeps it inside the normal transcript renderer pipeline;
- Rust builds `RenderPlan` by batching maximal adjacent runs that match the same rule and bucket;
- Lua renderers receive block/group snapshots and decide the visible summary/details;
- registration generations and cache keys invalidate row/layout caches when config changes.

The grouping API is declarative-only for this work. Do not add arbitrary Lua match callbacks in the initial design; they undermine deterministic planning and cacheability. If a future real use case needs callbacks, design it as a separate feature with an explicit cache contract.

A practical initial API:

```lua
local layout = smelt.layout
local defaults = require("smelt.transcript.defaults")

smelt.transcript.groups.register({
  name = "read_file_batch",
  cache_key = "v1",
  priority = 0,
  min = 2,
  default_view = "collapsed",
  selector = {
    kind = "tool",
    name = "read_file",
    terminal = true,
  },
  render = function(group, ctx)
    if group.view_state == "collapsed" then
      return layout.vbox({
        layout.text("read " .. group.child_count .. " files"),
        defaults.render_group_child_list(group, ctx, {
          field = { "args", "path" },
          max = 8,
        }),
      })
    end
    return defaults.render_group_expanded(group, ctx)
  end,
})
```

`selector` says which blocks can join the virtual node. `bucket` is optional and says which selector-matched blocks must stay separate inside adjacent runs. The default bucket is the rule name, which means a `read_file_batch` rule naturally groups only adjacent `read_file` blocks that match that rule.

Use `bucket` only when one rule intentionally matches multiple categories but wants separate adjacent runs:

```lua
smelt.transcript.groups.register({
  name = "tool_batches",
  cache_key = "v1",
  min = 2,
  default_view = "collapsed",
  selector = {
    kind = "tool",
    name = { "read_file", "grep", "glob" },
    terminal = true,
  },
  bucket = { "name" },
  render = function(group, ctx)
    return require("my.groups").render_tool_batch(group, ctx)
  end,
})
```

The grouping algorithm is intentionally small:

1. Walk transcript blocks in order.
2. For the current block, find the first registered selector that matches by deterministic priority/registration order.
3. Batch the maximal adjacent run matching the same rule and bucket.
4. Emit `RenderNode::Group` when run length is at least `min`; otherwise emit normal block nodes.
5. Never skip over non-matching blocks and never create nested groups.

Failure policy for grouped tool runs: preserve linearity and keep adjacent matching calls in the same group even if one child failed. Collapsed rendering should stay simple: list the same child identifiers (path/pattern/glob) in order and style failed children with an error highlight, optionally dimmed. The full error message remains in the expanded view. Do not split around failures and do not pull later successes across an error.

`render` is mandatory. It returns the same `smelt.layout` tree used by ordinary transcript renderers; Rust compiles that tree into `LayoutIr`. This keeps Rust out of group presentation entirely.

Possible declarative registration fields for the first pass:

- `name`: unique registration id;
- `cache_key`: stable key for persisted cache reuse;
- `priority`: deterministic conflict resolution, default `0`;
- `min`: minimum adjacent run length before a group is emitted;
- `default_view`: registration default for group nodes;
- `selector.kind`: block kind, e.g. `"tool"` or `"process_status"`;
- `selector.name`: tool name or list of tool names for `kind = "tool"`;
- `selector.status` / `selector.terminal`: tool status constraints;
- `selector.event`: typed process-status event name;
- `bucket`: literal string or a list of snapshot field paths.

Registry semantics:

- `name` is unique. Registering the same name replaces the previous registration and renderer.
- Built-in registrations use ordinary names and can be replaced or disabled by user config.
- Planning order is deterministic: higher `priority` first, then registration order. Built-ins use the default priority.
- If two rules match the same block, the winner is the first rule by that ordering.
- A registration's `cache_key` covers both planner fields and renderer output. Omit or change it to opt out of persisted layout/row cache or invalidate it across restarts.

## Built-in Lua grouping rules

### 1. Background process completion notifications

Problem: when many background commands finish around the same time, the transcript can fill with repeated status/tool messages.

Default renderer example:

```text
background processes finished: 10
  12345 ok
  12346 exited 1
  12347 ok
  ...
```

This is only the bundled Lua renderer's choice, not a Rust-baked view.

Plan:

- Replace plain process-completion text as the internal representation with a typed process-status event, while preserving readable text for rendering/copy:

  ```rust
  enum ProcessStatusEvent {
      BackgroundProcessCompleted {
          id: String,
          exit_code: Option<i32>,
      },
  }

  enum Block {
      ProcessStatus {
          text: String,
          event: Option<ProcessStatusEvent>,
      },
      // ...
  }
  ```

- Lua snapshots expose structured fields such as `event = "background_process_completed"`, `process_id`, and `exit_code`.
- Register a Lua group with `selector = { kind = "process_status", event = "background_process_completed" }`.
- The bundled renderer can summarize ids/statuses, but users can replace it by replacing the registration.
- Keep the group expandable so the user can inspect the individual original blocks when needed.

Important detail: this is a renderer/grouping concern. The underlying individual transcript blocks remain available and are used when expanded or copied as children.

### 2. Consecutive `read_file` tool calls

Group only adjacent `read_file` blocks with other `read_file` blocks.

Collapsed summary example:

```text
read 4 files
  crates/core/src/transcript_model.rs
  crates/tui/src/content/display_layout.rs
  ...
```

Expanded view can render each child using the existing tool renderer.

### 3. Consecutive `grep` tool calls

Group only adjacent `grep` blocks with other `grep` blocks.

Collapsed summary example:

```text
searched 3 patterns
  "RenderNode"
  "ViewState"
  "ToolCall"
```

### 4. Consecutive `glob` tool calls

Group only adjacent `glob` blocks with other `glob` blocks.

Collapsed summary example:

```text
matched 3 globs
  **/*.rs
  runtime/lua/**/*.lua
  docs/**/*.md
```

### Explicitly not grouped by default

These should remain separate by default:

```text
read_file, grep, read_file
read_file, glob
best-effort mixed read/search batches
```

Users can define their own mixed group in Lua later, but built-ins should avoid surprising semantic merging.

## Folding model

Keep presentation/fold state separate from semantic transcript history:

```rust
struct TranscriptPresentationState {
    view_overrides: HashMap<RenderNodeId, ViewState>,
}
```

`BlockHistory` should remain semantic transcript data. Existing `BlockHistory.view_states` should move into `TranscriptPresentationState` rather than becoming a permanent second source of truth. If manual block fold state must be migrated, represent old block ids as `RenderNodeId::Block(id)` and then remove the old storage path.

Default view state has one precedence chain. Group registrations provide the default for their group nodes. User/view configuration can override registration defaults explicitly. Thinking is the canonical built-in block default: it is rendered as full thinking content and starts collapsed, rather than being hidden/replaced by a global `show_thinking` toggle.

The durable settings shape should be a map/table, not one boolean per block family. A concrete Lua-facing target shape is:

```lua
smelt.settings.transcript_view = {
  blocks = {
    thinking = "collapsed",
    tool = "expanded",
  },
  tools = {
    read_file = "collapsed",
  },
  groups = {
    read_file_batch = "collapsed",
  },
}
```

Renderer APIs may still receive cache-context flags while this is being migrated, but the product model is fold defaults plus manual overrides. Do not add new show/hide toggles for transcript content.

```lua
smelt.transcript.groups.register({
  name = "read_file_batch",
  default_view = "collapsed",
  ...
})

smelt.transcript.view.set_default("tool/read_file", "collapsed")
smelt.transcript.view.set_default("group/read_file_batch", "expanded")
```

Resolution order:

1. Manual override for this `RenderNodeId` in `TranscriptPresentationState`.
2. Explicit user/view default from the settings-backed transcript view policy (`smelt.settings.transcript_view` / `smelt.transcript.view.set_default`).
3. Group registration `default_view` for group nodes.
4. Built-in block default for block nodes (`thinking = collapsed` initially).
5. `Expanded`.

Manual expand/collapse should apply to both block and group nodes. The initial product recommendation is session-local manual overrides plus config-driven defaults. Persist overrides only if that clearly improves the product after the node-id model is stable.

If `GroupId` remains `rule + bucket + first_child`, manual expansion survives appended siblings but resets if a new matching child appears before the first child or if the group is split. If that behavior feels wrong during implementation, change the id shape then rather than preserving a weak abstraction.

## Renderer model

Keep one root transcript renderer pipeline, but make group registrations install group-specific renderers. Rust still calls Lua once per visible/needed render node; Lua dispatches group nodes to the renderer registered with `smelt.transcript.groups.register`.

Normal block snapshot:

```lua
{
  kind = "tool",
  id = 42,
  name = "read_file",
  ...
}
```

Group snapshot:

```lua
{
  kind = "group",
  id = "read_file_batch:read_file_batch:42",
  index = 17,
  group_kind = "read_file_batch",
  bucket = "read_file_batch",
  view_state = "collapsed",
  children = { block1, block2, block3 },
  child_ids = { 42, 43, 44 },
  child_count = 3,
}
```

Default dispatch:

```lua
function defaults.render(node, ctx)
  if node.kind == "group" then
    return smelt.transcript.groups.render(node, ctx)
  end
  -- existing block dispatch
end

function smelt.transcript.groups.render(group, ctx)
  local entry = registered[group.group_kind]
  if not entry then return defaults.render_unknown_group(group, ctx) end
  return entry.render(group, ctx)
end
```

A group registration must provide `render`; reject registrations without it. Avoid hidden Rust defaults.

Each registered group renderer can choose between:

- summary-only layout for collapsed/default display;
- child rendering with existing `defaults.render(child, ctx)` for expanded details;
- a hybrid summary plus capped child list.

Rust should not know how to summarize `read_file`, `grep`, `glob`, or background-process groups. It should only provide snapshots and compile the returned layout.

## Cache and row-index strategy

Introduce a node-level layout key instead of reusing `LayoutKey` everywhere:

```rust
struct NodeLayoutKey {
    content_hash: u64,
    sidecar_hash: u64,
    renderer_version: u64,
    renderer_generation: u64,
    renderer_cache_key: Option<u64>,
    render_context_hash: u64,
    view_state: ViewState,
}
```

For a block node, `content_hash` and `sidecar_hash` are the existing block/tool hashes. For a group node, derive them from:

- group rule name and registration cache key, covering both planner fields and renderer output;
- bucket;
- child block ids from the adjacent child range;
- child content hashes;
- child sidecar/tool display hashes;
- effective view state.

The plan itself also needs a fingerprint derived from `history.generation()`, rule generation/cache key, node ids, child ids, child content hashes, and child sidecar hashes. `ExactRowIndex` and persisted row-index entries should compare against the render plan, not `history.order` directly.

Concrete code impact:

- `DisplayModel` moves from `HashMap<BlockId, CachedLayout>` to `HashMap<RenderNodeId, CachedLayout>`.
- `DisplayLayoutCacheEntry.id` becomes `RenderNodeId`; cache validation calls `render_plan.node_key(id)` instead of `display_layout_entry_matches_history` only.
- `CompileJob` carries a `RenderNode` snapshot and compiles either a block snapshot or group snapshot through Lua.
- `ExactBlockRow` becomes `ExactNodeRow { id: RenderNodeId, key: NodeLayoutKey, ... }`.
- `ExactRowIndex::{is_current,rebuild_if_stale,sync_stable_order_prefix,hydrate_from_cache,cache_entry}` operate on `RenderPlan.nodes` and `RenderPlan.fingerprint`.
- `DisplayRowIndexNode.id` becomes `RenderNodeId`; row-cache hydration compares cached nodes with the current render plan instead of `history.order[index]`.
- `gc_if_stale` retains layout cache entries for the current render-node ids, not only block ids.

This lets display groups update when a child finishes, fails, receives output, or when Lua group config changes.

## Projection and public API impact

`TranscriptProjection` currently assumes one display node per history block in row indexing and visible-layout APIs. Replace that assumption directly:

- rename internal helpers from `block_*` to `node_*` where they operate on rendered rows;
- replace public/internal APIs that are actually render/layout APIs so they expose `RenderNodeId`, not `BlockId`;
- expose semantic child `BlockId`s only from APIs that truly operate on transcript content, such as copy/search/session export;
- add a visible-node iterator for UI interactions: `(RenderNodeId, row_start, row_end)`;
- when a group is visible, copy/search should operate over the child blocks by default, while fold/toggle/mouse targeting should operate on the group node;
- expanded group renderers can call existing child renderers from Lua using child snapshots rather than receiving pre-rendered child layouts from Rust.

This avoids mixing two identities: `BlockId` remains the durable semantic id, and `RenderNodeId` is the viewport/rendering id.

## Phases

### Phase 1: Render-node refactor with no visible grouping yet

- Add `RenderNodeId`, `RenderNode`, `RenderPlan`, and `NodeLayoutKey` types.
- Build a trivial plan that maps every `BlockId` to `RenderNode::Block`.
- Introduce `TranscriptPresentationState` and move fold/view overrides out of semantic `BlockHistory`.
- Refactor display/projection caches from `BlockId` to `RenderNodeId`.
- Refactor row indexing to compare against `RenderPlan` instead of `history.order`.
- Rename projection/display APIs from block terminology to node terminology where that is the real concept.
- Keep Lua renderer input identical for block nodes.
- Preserve visible behavior and tests, not internal API compatibility.

Deliverable: internal architecture supports virtual display nodes, but no grouping is visible yet.

### Phase 2: Declarative Lua group registry

- Add a host API namespace for `smelt.transcript.groups`.
- Let Lua register named grouping rules with `name`, `cache_key`, optional `priority`, `min`, `default_view`, `selector`, optional `bucket`, and mandatory `render`.
- Store registered declarative planner fields in Rust in a cheap-to-apply form, while keeping each renderer in Lua keyed by group name.
- Define replacement/disable/priority semantics so user config can override built-ins predictably.
- Include group registrations in transcript renderer/rule generation and cache-key invalidation.
- Keep matcher callbacks out of the initial API.

Deliverable: Lua owns group policy declarations; Rust can build a deterministic render plan without per-block Lua calls.

### Phase 3: Run batching from Lua registrations

- Build `RenderPlan` by walking flat `BlockHistory` and applying Lua-registered declarative selectors.
- For each matching block, emit a maximal adjacent run for the same rule and bucket.
- Store group children internally as `child_range: Range<usize>` and materialize child snapshots only when rendering.
- Emit a group only when the run length is at least the rule's `min`; otherwise emit the original block nodes.
- Only group consecutive blocks; never skip over non-matching blocks.
- Do not group pending or confirm tool blocks by default; group terminal tool blocks first to avoid active-tool churn.
- If a rule no longer matches, the plan naturally falls back to individual block nodes.
- Add group snapshots passed to Lua renderers.

Deliverable: Lua-defined groups appear in the transcript.

### Phase 4: Generalized presentation state

- Replace block-only view-state lookup in projection with render-node view-state lookup.
- Support defaults from Lua group/block policy, including built-in `thinking = collapsed` and the planned settings-backed per-kind/per-tool/per-group default-view map.
- Keep manual overrides in `TranscriptPresentationState`, separate from semantic history and default policy.
- Retire `show_thinking` as the UX model: thinking blocks are rendered as full content and folded by presentation state, not replaced by a separate summary/hide path.
- Add toggle commands/APIs for the node at a display row.
- Use Vim-compatible fold bindings for transcript render nodes: `za` toggles, `zo` opens, `zc` closes, `zR` opens all, and `zM` closes all. `Enter` is contextual activation: it toggles only when the focused row is an explicit fold summary/affordance, not arbitrary expanded content.
- Mouse folding should be limited to explicit fold affordances and collapsed summaries. Fire on mouse-up only when down/up target the same node and movement stayed below drag threshold; drag selection always wins and never toggles.
- Expose enough node metadata to Lua/UI for keymaps and mouse handlers.

Deliverable: users can manually expand/collapse groups and blocks; defaults can collapse group types automatically.

### Phase 5: Typed background process status events

- Replace plain-text-only process completion notes with typed `ProcessStatusEvent::BackgroundProcessCompleted` data in transcript/history snapshots.
- Preserve the current user-visible text for readability and copy behavior, but do not preserve the old plain-text-only internal representation.
- Expose typed event fields to Lua block snapshots so the background-completion selector can match exactly.
- Add tests for multiple completions arriving in the same event-loop drain and rendering as one group.

Deliverable: background process completion grouping does not depend on parsing English status strings.

### Phase 6: Built-in Lua grouping rules and renderers

Register built-in Lua rules for:

1. background process completion/status notifications, after typed process-status events exist;
2. consecutive terminal `read_file` calls;
3. consecutive terminal `grep` calls;
4. consecutive terminal `glob` calls.

Implement renderers in Lua using existing `smelt.layout` primitives and existing default child renderers.

Built-ins should be conservative:

- no default mixed read/search grouping;
- no grouping across assistant text/user messages/thinking/mode/compacted/checkpoint marker blocks;
- no grouping across unrelated tool kinds;
- minimum group size of 2;
- group errors/denials with the same tool kind only if the summary visibly surfaces the error/denied count;
- keep pending/confirm tools separate unless a later explicit rule chooses otherwise.

Deliverable: the quality-of-life improvements are visible without Rust tool-specific grouping code.

### Phase 7: Polish, docs, and hardening

- Persist manual fold overrides if the node-id model is stable and persistence improves the product; otherwise keep them intentionally session-local and document that choice.
- Add documentation and examples for user-defined group rules.
- Regenerate Lua API stubs/docs after API stabilization.
- Add storybook cases for collapsed and expanded groups.
- Add regression tests for group planning, cache invalidation, and viewport row mapping.
- Remove obsolete block-only cache/index names and any temporary migration code once the release boundary allows it.

Deliverable: stable public customization surface and a cleaned-up internal model.

## Edge cases to design/test

- **Streaming tools:** default rules should group only terminal tools first. Pending and confirm states are volatile and can make row indexes churn.
- **Errors and denied calls:** same-tool groups may include errors/denials only when the collapsed summary exposes counts and highlights failure.
- **Boundaries:** grouping never crosses non-matching blocks, including user/assistant/thinking/mode/compacted/checkpoint markers.
- **Config reload:** rule generation/cache key must invalidate render plans, layout cache, and row-index cache.
- **Manual overrides:** group fold state should survive appended matching children, but it may reset when a group splits or prepends. Keep overrides in presentation state, not semantic history.
- **Search/copy:** semantic operations should continue to see child blocks, while UI targeting sees render nodes.
- **Background completions:** process-completion blocks need typed event data before robust grouping.

## Open questions

- Is declarative `selector` plus optional `bucket` enough for the desired customization surface, or is there a concrete rule that requires a separate future callback feature with a first-class cache contract?
- Should manual fold state persist across session reloads as presentation state, or is session-local state the cleaner product behavior?
- Should group renderers receive already-rendered child layouts, or only child snapshots? Initial recommendation: child snapshots only, so Lua composition stays explicit and cacheable.

