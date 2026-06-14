-- Root transcript renderer helpers. Rust owns only the single renderer handle;
-- middleware composition is ordinary Lua around that handle.

smelt.transcript = smelt.transcript or {}

--- Tool output snapshot passed to transcript renderers.
---@class smelt.transcript.ToolOutput
---@field content string Captured output text.
---@field is_error boolean True when the tool result is an error.
---@field metadata? table Tool-specific structured metadata.

--- Renderer context. Width, theme, and scroll state are intentionally absent.
---@class smelt.transcript.Context
---@field show_thinking boolean Whether thinking blocks should render expanded.
---@field renderer_generation integer Current renderer generation used for cache invalidation.
---@field surface string Rendering surface name, currently `"transcript"`.
---@field limits table Numeric product row budgets such as `tool_output_rows`.

--- Semantic transcript block snapshot passed to the root renderer.
---@class smelt.transcript.Block
---@field id integer Stable block id within the session.
---@field index integer Zero-based block index in transcript order.
---@field kind "user"|"assistant"|"thinking"|"tool"|"code"|"exec"|"mode"|"process_status"|"compacted" Block kind.
---@field text? string User/mode/process text.
---@field content? string Assistant/thinking/code content.
---@field image_labels? string[] User image labels.
---@field icon? string Mode icon.
---@field hl_group? string Mode/process highlight group.
---@field lang? string Code language.
---@field call_id? string Tool call id.
---@field name? string Tool name.
---@field args? table Tool arguments.
---@field summary? any Tool styled summary lines or compacted summary text.
---@field summary_text? string Tool summary flattened to plain text.
---@field status? "pending"|"confirm"|"ok"|"err"|"denied" Tool status.
---@field elapsed_secs? integer Terminal/static tool elapsed seconds.
---@field user_message? string Tool user-facing status message.
---@field output? smelt.transcript.ToolOutput Tool output snapshot.
---@field command? string Exec command.

local transcript = smelt.transcript
local base_renderer = transcript.__get_renderer and transcript.__get_renderer() or nil
local extensions = {}
local order = {}
local next_token = 0

local function require_function(name, value)
  if type(value) ~= "function" then
    error("smelt.transcript." .. name .. ": expected function", 3)
  end
end

local function rebuild_renderer()
  local renderer = base_renderer
  for i = 1, #order do
    local entry = extensions[order[i]]
    if entry then
      local next_renderer = renderer
      local fn = entry.fn
      renderer = function(block, ctx)
        return fn(next_renderer, block, ctx)
      end
    end
  end
  if renderer then transcript.__set_renderer(renderer) end
end

--- Replace the base transcript renderer. Existing middleware registered with
--- `extend_renderer` remains wrapped around the new base. The renderer must
--- return a `smelt.layout` value; return `smelt.layout.empty()` to hide a block.
---@type fun(renderer: fun(block: smelt.transcript.Block, ctx: smelt.transcript.Context): table?): nil
function smelt.transcript.set_renderer(renderer)
  require_function("set_renderer", renderer)
  base_renderer = renderer
  rebuild_renderer()
end

--- Return the current composed root transcript renderer, or nil before the
--- default renderer has been installed.
---@type fun(): (fun(block: smelt.transcript.Block, ctx: smelt.transcript.Context): table?)?
function smelt.transcript.get_renderer()
  return transcript.__get_renderer()
end

--- Add or replace named middleware around the root renderer. Later extensions
--- run first. The callback receives `(next, block, ctx)` and may return its own
--- layout or delegate with `next(block, ctx)`. The returned `Reg` removes only
--- this registration instance.
---@type fun(name: string, renderer: fun(next: fun(block: smelt.transcript.Block, ctx: smelt.transcript.Context): table?, block: smelt.transcript.Block, ctx: smelt.transcript.Context): table?): smelt.Reg
function smelt.transcript.extend_renderer(name, renderer)
  if type(name) ~= "string" or name == "" then
    error("smelt.transcript.extend_renderer: name must be a non-empty string", 2)
  end
  require_function("extend_renderer", renderer)

  next_token = next_token + 1
  local token = next_token
  if not extensions[name] then order[#order + 1] = name end
  extensions[name] = { fn = renderer, token = token }
  rebuild_renderer()

  return smelt.reg.new(function()
    local entry = extensions[name]
    if not entry or entry.token ~= token then return end
    extensions[name] = nil
    for i = #order, 1, -1 do
      if order[i] == name then
        table.remove(order, i)
        break
      end
    end
    rebuild_renderer()
  end)
end

--- Bump the renderer generation after changing closed-over state that affects
--- renderer output without calling `set_renderer`, `extend_renderer`, or a
--- registration's `:remove()`.
---@type fun(): integer
function smelt.transcript.invalidate_renderer()
  return transcript.__invalidate_renderer()
end

local defaults = require("smelt.transcript.defaults")

smelt.transcript.set_renderer(function(block, ctx)
  return defaults.render(block, ctx)
end)

package.loaded["smelt.transcript"] = smelt.transcript

return smelt.transcript
