-- Root transcript renderer helpers. Rust owns only the single renderer handle;
-- middleware composition is ordinary Lua around that handle.

smelt.transcript = smelt.transcript or {}

--- Bounded tool output metadata passed to transcript renderers.
---@class smelt.transcript.ToolOutput
---@field content_id integer Stable shared-content id accepted by `smelt.layout.content`.
---@field content_revision integer Monotonic content revision.
---@field content_bytes integer Current output size in bytes.
---@field content_lines integer Current logical line count.
---@field content_preview string Bounded text preview for labels and fallback UI, not complete output.
---@field is_error boolean True when the tool result is an error.
---@field metadata? table Bounded tool-specific structured metadata.
---@field content_fields? table<string, smelt.transcript.ContentMetadata> Named retained payloads referenced by opaque content IDs.

--- Metadata for a retained payload whose complete content remains in Rust and is
--- available to renderers only through retained layout leaves.
---@class smelt.transcript.ContentMetadata
---@field content_id integer Stable shared-content id accepted by retained layout leaves.
---@field content_revision integer Monotonic content revision.
---@field content_bytes integer Current content size in bytes.
---@field content_lines integer Current logical line count.
---@field content_preview string Strictly bounded preview for labels, never the complete retained payload.

--- Opaque top-level string argument passed to transcript renderers. Complete field
--- content remains in Rust and is available only through `smelt.layout.content`.
---@class smelt.transcript.ArgumentField
---@field name string Top-level argument name.
---@field content_id integer Stable shared-content id accepted by `smelt.layout.content`.
---@field content_revision integer Monotonic content revision.
---@field content_bytes integer Current field size in bytes.
---@field content_lines integer Current logical line count.
---@field content_preview string Bounded text preview for labels and fallback UI, not complete content.
---@field complete boolean True when the JSON parser has consumed the complete field value.

--- Renderer context. Width, theme, and scroll state are intentionally absent.
---@class smelt.transcript.Context
---@field view_state "collapsed"|"peek"|"expanded"|"trimmed_head"|"trimmed_tail" Effective view state for the node currently being rendered.
---@field renderer_generation integer Current renderer generation used for cache invalidation.
---@field surface string Rendering surface name, currently `"transcript"`.
---@field limits table Numeric product row budgets such as `tool_output_rows`.
---@field now_ms integer Unix epoch milliseconds shared by the complete top-level render pass.
---@field render fun(node: smelt.transcript.Block, overrides?: { view_state?: string }): smelt.layout.Node Re-enter the composed root renderer for a semantic child.

--- Bounded semantic metadata for one retained group child. Growing content and
--- complete child payloads are never embedded in group renderer input.
---@class smelt.transcript.GroupChild
---@field id integer Stable child block id.
---@field kind string Semantic block kind.
---@field name? string Tool name.
---@field status? "pending"|"confirm"|"ok"|"err"|"denied" Tool status.
---@field summary_text? string Bounded plain-text summary.
---@field called_at_ms? integer Invocation start as Unix epoch milliseconds.
---@field args? table Bounded argument previews used by collapsed labels.
---@field output? { content_lines?: integer, is_error?: boolean } Bounded output metadata.
---@field event? string Process status event type.
---@field event_data? { process_id?: string, exit_code?: integer } Bounded process metadata.
---@field process_id? string Background process id.
---@field exit_code? integer Background process exit code.

--- Bounded semantic transcript metadata passed to the root renderer.
---@class smelt.transcript.Block
---@field id integer Stable block id within the session.
---@field index integer Zero-based block index in transcript order.
---@field kind "user"|"assistant"|"thinking"|"tool"|"group"|"code"|"exec"|"mode"|"process_status"|"compacted"|"compaction_preview" Block kind.
---@field text? string User/mode/process text.
---@field user_lines? table User text as styled span lines, including slash/ref/image accents.
---@field sent_at_ms? integer User-message submission time as Unix epoch milliseconds; absent for undated history.
---@field content? string Code content.
---@field content_id? integer Stable shared-content id for assistant and thinking blocks.
---@field content_revision? integer Monotonic shared-content revision.
---@field content_bytes? integer Shared content size in bytes.
---@field content_lines? integer Shared content logical line count.
---@field content_preview? string Bounded preview for labels and fallback UI, not complete content.
---@field title? string Latest structured reasoning-summary title.
---@field summary_titles? string[] Ordered structured reasoning-summary title history.
---@field reasoning_kind? "summary"|"raw" Reasoning source for thinking blocks.
---@field image_labels? string[] User image labels.
---@field icon? string Mode icon.
---@field hl_group? string Mode/process highlight group.
---@field lang? string Code language.
---@field call_id? string Tool call id.
---@field name? string Tool name.
---@field args? table Bounded tool-argument previews and complete non-string structured values.
---@field argument_fields? smelt.transcript.ArgumentField[] Opaque top-level string arguments. Complete field content is never included in renderer metadata.
---@field summary? any Tool styled summary lines or compacted summary text. Header parts receive the resolved presentation title here; other parts keep the original summary.
---@field summary_text? string Original tool summary flattened to plain text.
---@field kept? smelt.transcript.Block[] Compacted marker: user, assistant, and tool entries the model still receives verbatim, oldest first.
---@field status? "drafting"|"pending"|"confirm"|"ok"|"err"|"denied" Tool status. `"drafting"` while the model still streams the call.
---@field draft? boolean True while the model still streams the tool call.
---@field draft_finished? boolean Draft whose arguments finished streaming.
---@field native? boolean Draft the provider runs itself, outside smelt's tool preflight.
---@field called_at_ms? integer Invocation start as Unix epoch milliseconds.
---@field elapsed_ms? integer Best-known execution duration in milliseconds.
---@field elapsed_active? boolean True only while elapsed time can continue advancing.
---@field thinking_summary? string Folded thinking summary text.
---@field user_message? string Tool user-facing status message.
---@field preview_output? smelt.transcript.ToolOutput Immutable pending output metadata for a promoted finished draft.
---@field output? smelt.transcript.ToolOutput Tool output metadata.
---@field event? string Process status event type, e.g. `"background_process_completed"`.
---@field event_type? string Alias for `event`.
---@field event_data? table Full typed process status event payload.
---@field process_id? string Background process id for process status events.
---@field exit_code? integer Background process exit code when known.
---@field output_id? integer Stable shared-content id for exec or process-status output.
---@field output_revision? integer Shared output revision.
---@field output_bytes? integer Shared output size in bytes.
---@field output_lines? integer Shared output logical line count.
---@field output_preview? string Bounded output preview, not complete content.
---@field command? string Exec command.
---@field command_spans? table Exec command as one styled span line, including the `!` accent.
---@field group_kind? string Registered semantic group name.
---@field bucket? string Stable planner bucket for a group.
---@field view_state? "collapsed"|"peek"|"expanded" Effective group or child view state.
---@field children? smelt.transcript.GroupChild[] Ordered bounded child presentation metadata for a group.
---@field child_ids? integer[] Ordered stable block ids for a group.
---@field child_count? integer Number of semantic children in a group.

--- Group selector declared through `smelt.transcript.groups.register`.
---@class smelt.transcript.GroupSelector
---@field kind? string Match block kind.
---@field name? string Match one tool name for tool blocks.
---@field names? string[] Match any listed tool name for tool blocks. Cannot be combined with `name`.
---@field terminal? boolean Match terminal/non-terminal blocks.
---@field event? string Match typed process status event type.
---@field event_type? string Alias for `event`.
---@field process_id? string Match typed background process id.
---@field exit_code? integer|string Match typed background process exit code.
---@field fields? table<string,string|integer> Exact block-field matches such as `{ event = "background_process_completed" }`.

--- Declarative transcript group registration. The host owns planning; the root
--- transcript renderer owns presentation for the resulting semantic group node.
---@class smelt.transcript.GroupSpec
---@field name string Unique group name. Registering the same name replaces it.
---@field cache_key? string Persisted layout cache key; omit to opt out while active.
---@field priority? integer Higher priority plans first. Defaults to 0.
---@field min? integer Minimum adjacent matching blocks required. Defaults to 2.
---@field default_view? "collapsed"|"peek"|"expanded" Initial presentation when the group first appears.
---@field selector smelt.transcript.GroupSelector Declarative block matcher.
---@field bucket? string|string[] Stable field names used to split adjacent matching runs.

--- Style attributes accepted on one styled title span.
---@class smelt.transcript.StyledSpanStyle
---@field hl? string Theme highlight group.
---@field fg? string Foreground color.
---@field bg? string Background color.
---@field dim? boolean Whether to dim the text.
---@field bold? boolean Whether to render bold text.
---@field italic? boolean Whether to render italic text.

--- One span in a styled tool title. Style attributes may be supplied directly or through `style`.
---@class smelt.transcript.StyledSpan
---@field text? string Span text. The positional field `[1]` is also accepted.
---@field style? smelt.transcript.StyledSpanStyle Nested style attributes.
---@field syntax? string Syntax language used to highlight the span text.
---@field hl? string Theme highlight group.
---@field fg? string Foreground color.
---@field bg? string Background color.
---@field dim? boolean Whether to dim the text.
---@field bold? boolean Whether to render bold text.
---@field italic? boolean Whether to render italic text.
---@field selectable? boolean Whether copied transcript text includes this span.
---@field title_suffix? boolean Whether this span is transient pending-state title metadata.

--- Options passed to focused tool body and draft callbacks.
---@class smelt.transcript.ToolBodyOptions
---@field gutter? string Prefix rendered before each body line.

--- Public presentation policy for one tool name. A complete `render` callback
--- takes precedence; otherwise the default renderer composes the focused pieces.
--- Registrations are copied and immutable.
---@class smelt.transcript.ToolPresentation
---@field cache_key? string Stable persisted-layout key. Omit for dynamic presentation state.
---@field render? fun(tool: smelt.transcript.Block, ctx: smelt.transcript.Context, presentation: smelt.transcript.ToolPresentation): smelt.layout.Node Complete replacement renderer.
---@field title? fun(tool: smelt.transcript.Block, ctx: smelt.transcript.Context): string|smelt.transcript.StyledSpan[][]|nil Semantic title after the status marker. Return nil to use the tool summary.
---@field body? fun(tool: smelt.transcript.Block, ctx: smelt.transcript.Context, opts?: smelt.transcript.ToolBodyOptions): smelt.layout.Node|nil Expanded body renderer. Return nil to suppress the body.
---@field draft? fun(draft: smelt.transcript.Block, ctx: smelt.transcript.Context, opts?: smelt.transcript.ToolBodyOptions): smelt.layout.Node|nil Draft body renderer. Return nil to suppress the body.
---@field draft_error? fun(draft: smelt.transcript.Block): string|nil Reason smelt's tool preflight will reject the streamed call. When it returns a string the draft shows that error instead of the `draft` body. Not consulted for `native` drafts.
---@field compact? fun(tool: smelt.transcript.Block, ctx: smelt.transcript.Context): string|smelt.layout.Node|nil Collapsed detail renderer. Return nil to suppress the detail.

local DEFAULT_RENDERER_CACHE_KEY = "smelt.transcript.defaults:v6"
local internal_transcript = __smelt_internal.transcript
local parts = {}
local part_order = {
  "root", "group", "tool", "tool_header", "tool_header_content", "user", "user_content", "assistant",
  "thinking", "exec", "mode", "process_status", "compacted", "compaction_preview", "code", "unknown",
}
local tool_presentations = {}
local tool_order = {}
local next_token = 0
local tool_presentation_callbacks = { "render", "title", "body", "draft", "draft_error", "compact" }

local function copy_tool_presentation(presentation)
  local copy = {}
  copy.cache_key = presentation.cache_key
  for _, field in ipairs(tool_presentation_callbacks) do
    copy[field] = presentation[field]
  end
  return copy
end

local function require_function(name, value)
  if type(value) ~= "function" then
    error("smelt.transcript." .. name .. ": expected function", 3)
  end
end

local function parse_cache_key(name, opts)
  if opts == nil then return nil end
  if type(opts) ~= "table" then
    error("smelt.transcript." .. name .. ": opts must be a table", 3)
  end
  local key = opts.cache_key
  if key == nil or key == false then return nil end
  if type(key) ~= "string" or key == "" then
    error("smelt.transcript." .. name .. ": opts.cache_key must be a non-empty string", 3)
  end
  return key
end

local function renderer_part(api, name)
  if name == nil then name = "root" end
  if type(name) ~= "string" or not parts[name] then
    error("smelt.transcript." .. api .. ": unknown renderer part `" .. tostring(name) .. "`", 3)
  end
  return parts[name]
end

local function effective_cache_key()
  local keys = {}
  for _, name in ipairs(part_order) do
    local part = parts[name]
    if not part.cache_key then return nil end
    keys[#keys + 1] = name
    keys[#keys + 1] = part.cache_key
    for _, extension_name in ipairs(part.order) do
      local entry = part.extensions[extension_name]
      if not entry.cache_key then return nil end
      keys[#keys + 1] = extension_name
      keys[#keys + 1] = entry.cache_key
    end
  end
  for _, name in ipairs(tool_order) do
    local entry = tool_presentations[name]
    if not entry.presentation.cache_key then return nil end
    keys[#keys + 1] = "tool:" .. name
    keys[#keys + 1] = entry.presentation.cache_key
  end
  return table.concat(keys, "\n")
end

local function rebuild_renderer()
  for _, name in ipairs(part_order) do
    local part = parts[name]
    local renderer = part.base
    for _, extension_name in ipairs(part.order) do
      local next_renderer = renderer
      local fn = part.extensions[extension_name].fn
      renderer = function(block, ctx)
        return fn(next_renderer, block, ctx)
      end
    end
    part.renderer = renderer
  end
  internal_transcript.__set_renderer(parts.root.renderer, effective_cache_key())
end

--- Semantic stages in the bundled transcript composition pipeline.
---@alias smelt.transcript.RendererPart "root"|"group"|"tool"|"tool_header"|"tool_header_content"|"user"|"user_content"|"assistant"|"thinking"|"exec"|"mode"|"process_status"|"compacted"|"compaction_preview"|"code"|"unknown"

--- Options shared by renderer replacements and middleware.
---@class smelt.transcript.RendererOptions
---@field part? smelt.transcript.RendererPart Named renderer part; defaults to `root`.
---@field cache_key? string Stable persisted-layout key. Omit for dynamic state; bump when output changes across restarts.

--- Replace a part's base renderer, keeping its middleware. The default part is
--- `root`, the whole block renderer. Return `smelt.layout.empty()` to hide content.
--- Omit `opts.cache_key` to disable persisted layouts while this renderer is active.
---@type fun(renderer: fun(block: smelt.transcript.Block, ctx: smelt.transcript.Context): smelt.layout.Node, opts?: smelt.transcript.RendererOptions): nil
function smelt.transcript.set_renderer(renderer, opts)
  require_function("set_renderer", renderer)
  local cache_key = parse_cache_key("set_renderer", opts)
  local part = renderer_part("set_renderer", opts and opts.part)
  part.base = renderer
  part.cache_key = cache_key
  rebuild_renderer()
end

--- Return a snapshot of a composed renderer. Omit `part` for `root`. Capture it
--- before replacement to wrap the previous implementation without recursion.
--- Use `render_part` for live dispatch. Unknown part names raise.
---@type fun(part?: smelt.transcript.RendererPart): (fun(block: smelt.transcript.Block, ctx: smelt.transcript.Context): smelt.layout.Node)
function smelt.transcript.get_renderer(part)
  return renderer_part("get_renderer", part).renderer
end

--- Render a part using its current composition, including later replacements and
--- middleware. To wrap the same part, delegate with `next` or capture `get_renderer`
--- before replacement instead of recursively calling this function.
---@type fun(part: smelt.transcript.RendererPart, block: smelt.transcript.Block, ctx: smelt.transcript.Context): smelt.layout.Node
function smelt.transcript.render_part(part, block, ctx)
  return renderer_part("render_part", part).renderer(block, ctx)
end

--- Add or replace named middleware around a renderer part (default `root`).
--- Later extensions run first and delegate with `next(block, ctx)`. Names are
--- scoped to the part. The returned `Reg` removes only this registration instance.
--- Omit `opts.cache_key` to disable persisted layouts while the middleware is active.
---@type fun(name: string, renderer: fun(next: fun(block: smelt.transcript.Block, ctx: smelt.transcript.Context): smelt.layout.Node, block: smelt.transcript.Block, ctx: smelt.transcript.Context): smelt.layout.Node, opts?: smelt.transcript.RendererOptions): smelt.Reg
function smelt.transcript.extend_renderer(name, renderer, opts)
  if type(name) ~= "string" or name == "" then
    error("smelt.transcript.extend_renderer: name must be a non-empty string", 2)
  end
  require_function("extend_renderer", renderer)

  local cache_key = parse_cache_key("extend_renderer", opts)
  local part = renderer_part("extend_renderer", opts and opts.part)
  next_token = next_token + 1
  local token = next_token
  if not part.extensions[name] then part.order[#part.order + 1] = name end
  part.extensions[name] = { fn = renderer, token = token, cache_key = cache_key }
  rebuild_renderer()

  return smelt.reg.new(function()
    local entry = part.extensions[name]
    if not entry or entry.token ~= token then return end
    part.extensions[name] = nil
    for i = #part.order, 1, -1 do
      if part.order[i] == name then
        table.remove(part.order, i)
        break
      end
    end
    rebuild_renderer()
  end)
end

--- Bump the renderer generation after changing closed-over state that affects
--- renderer output without calling `set_renderer`, `extend_renderer`, or a
--- registration's `:remove()`. This also opts out of persisted DisplayIR until
--- the renderer is installed again with a cache key.
---@type fun(): integer
function smelt.transcript.invalidate_renderer()
  return internal_transcript.__invalidate_renderer()
end

--- Register or replace presentation policy for a tool. The supported fields are
--- copied, so later table mutation has no effect; re-register to change behavior
--- or cache keys. The returned registration removes only this exact replacement.
--- Registering presentation changes rebuilds the composed root renderer so all
--- cached standalone and grouped nodes invalidate.
---@type fun(name: string, presentation: smelt.transcript.ToolPresentation): smelt.Reg
function smelt.transcript.register_tool(name, presentation)
  if type(name) ~= "string" or name == "" then
    error("smelt.transcript.register_tool: name must be a non-empty string", 2)
  end
  if type(presentation) ~= "table" then
    error("smelt.transcript.register_tool: presentation must be a table", 2)
  end
  for _, field in ipairs(tool_presentation_callbacks) do
    if presentation[field] ~= nil and type(presentation[field]) ~= "function" then
      error("smelt.transcript.register_tool: presentation." .. field .. " must be a function", 2)
    end
  end
  if presentation.cache_key ~= nil
    and (type(presentation.cache_key) ~= "string" or presentation.cache_key == "")
  then
    error("smelt.transcript.register_tool: presentation.cache_key must be a non-empty string", 2)
  end

  local retained = copy_tool_presentation(presentation)
  next_token = next_token + 1
  local token = next_token
  if not tool_presentations[name] then tool_order[#tool_order + 1] = name end
  tool_presentations[name] = { presentation = retained, token = token }
  rebuild_renderer()

  return smelt.reg.new(function()
    local entry = tool_presentations[name]
    if not entry or entry.token ~= token then return end
    tool_presentations[name] = nil
    for i = #tool_order, 1, -1 do
      if tool_order[i] == name then
        table.remove(tool_order, i)
        break
      end
    end
    rebuild_renderer()
  end)
end

--- Return a copy of the current presentation policy for `name`, or nil. Mutating
--- the returned table has no effect; re-register the tool to change its presentation.
---@type fun(name: string): smelt.transcript.ToolPresentation?
function smelt.transcript.get_tool_presentation(name)
  local entry = tool_presentations[name]
  return entry and copy_tool_presentation(entry.presentation) or nil
end

smelt.transcript.groups = smelt.transcript.groups or {}

local function require_table(name, value)
  if type(value) ~= "table" then
    error("smelt.transcript." .. name .. ": expected table", 3)
  end
end

--- Register or replace a declarative transcript group type. This only declares
--- planning metadata; Rust owns deterministic adjacent-run planning and the
--- composed root transcript renderer presents resulting group nodes.
---@type fun(spec: smelt.transcript.GroupSpec): smelt.Reg
function smelt.transcript.groups.register(spec)
  require_table("groups.register", spec)
  if type(spec.name) ~= "string" or spec.name == "" then
    error("smelt.transcript.groups.register: spec.name must be a non-empty string", 2)
  end
  if type(spec.selector) ~= "table" then
    error("smelt.transcript.groups.register: spec.selector must be a table", 2)
  end

  local token = internal_transcript.__register_group(spec)
  local name = spec.name
  return smelt.reg.new(function()
    internal_transcript.__unregister_group(name, token)
  end)
end

--- Return group specs in planner order: higher priority first, then registration order.
---@type fun(): smelt.transcript.GroupSpec[]
function smelt.transcript.groups.list()
  return internal_transcript.__groups()
end

--- Current group-registry generation. Rust render planning uses this to invalidate
--- plan/cache state once group planning is enabled.
---@type fun(): integer
function smelt.transcript.groups.generation()
  return internal_transcript.__groups_generation()
end

--- Current group-registry cache key, or nil when any active group opted out of
--- persisted planning/layout caches.
---@type fun(): integer?
function smelt.transcript.groups.cache_key()
  return internal_transcript.__groups_cache_key()
end

local defaults = require("smelt.transcript.defaults")

for _, name in ipairs(part_order) do
  parts[name] = {
    base = name == "root" and defaults.render or defaults["render_" .. name],
    cache_key = DEFAULT_RENDERER_CACHE_KEY,
    extensions = {},
    order = {},
  }
end
rebuild_renderer()

require("smelt.transcript.builtins").register()

package.loaded["smelt.transcript"] = smelt.transcript

return smelt.transcript
