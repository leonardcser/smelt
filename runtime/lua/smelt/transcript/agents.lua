-- Transcript presentation of the calls through which Claude Code's agents work
-- together: launching an agent, and sending another agent a message.

local defaults = require("smelt.transcript.defaults")
local detach = require("smelt._detach")
local layout = smelt.layout

local M = {}

local function text_arg(block, name)
  local value = (block.args or {})[name]
  if type(value) ~= "string" then return nil end
  value = value:gsub("^%s+", ""):gsub("%s+$", "")
  if value == "" then return nil end
  return value
end

local function metadata(block)
  local output = block.output
  return output and type(output.metadata) == "table" and output.metadata or {}
end

local function agent_title(block)
  local spans = { { text = text_arg(block, "description") or "agent" } }
  local kind = text_arg(block, "subagent_type")
  if kind and kind ~= "general-purpose" then
    spans[#spans + 1] = { text = "  " .. kind, dim = true }
  end
  if (block.args or {}).run_in_background == true then
    -- The same tag the transcript shows on any call that went to the background.
    spans[#spans + 1] = { text = "background", fg = "SmeltAgent", title_suffix = true }
  else
    -- Shown while the call waits: the agent that launched it works on once it reports.
    spans[#spans + 1] = { text = "waiting", fg = "SmeltAgent", title_suffix = true }
  end
  return { spans }
end

local function agent_usage(block)
  local data = metadata(block)
  local parts = {}
  local tokens = tonumber(data.tokens)
  if tokens then parts[#parts + 1] = smelt.text.format_tokens(tokens) .. " tokens" end
  local uses = tonumber(data.tool_uses)
  if uses then parts[#parts + 1] = tostring(uses) .. (uses == 1 and " tool use" or " tool uses") end
  if #parts == 0 then return nil end
  return table.concat(parts, " · ")
end

local function agent_report(block)
  local fields = block.output and block.output.content_fields
  return fields and fields.report
end

local function agent_body(block, ctx)
  if block.status == "pending" or block.status == "confirm" or block.status == "drafting" then
    -- What the agent works on, as Claude Code reports it.
    local items = {}
    if block.output then
      items[#items + 1] = defaults.render_tool_output_tail(block.output, ctx, { rows = 4 })
    end
    items[#items + 1] = detach.waiting_hint(block)
    if #items == 0 then return nil end
    return layout.vbox(items)
  end
  if not block.output or block.output.is_error then
    return block.output and defaults.render_tool_output_tail(block.output, ctx)
  end
  local report = agent_report(block)
  if not report then return defaults.render_tool_output_tail(block.output, ctx) end
  local items = {
    layout.cap(layout.content(report.content_id, { format = "markdown" }), {
      rows = (ctx and ctx.limits and ctx.limits.tool_output_rows) or 20,
      keep = "head",
      marker = "below",
    }),
  }
  local usage = agent_usage(block)
  if usage then items[#items + 1] = layout.runs({ { { text = usage, dim = true } } }) end
  return layout.vbox(items)
end

local function agent_compact(block)
  if block.status == "pending" then return detach.waiting_hint(block) end
  if block.status ~= "ok" then return nil end
  return agent_usage(block)
end

local function message_title(block)
  local spans = { { text = "→ ", fg = "SmeltAgent" }, { text = text_arg(block, "to") or "agent", bold = true } }
  local summary = text_arg(block, "summary")
  if summary then spans[#spans + 1] = { text = "  " .. summary, dim = true } end
  return { spans }
end

local function message_body(block, ctx)
  local items = {}
  local message = text_arg(block, "message")
  if message then items[#items + 1] = layout.markdown(message, {}) end
  local output = block.output
  if output and output.is_error then
    items[#items + 1] = defaults.render_tool_output_tail(output, ctx)
  end
  if #items == 0 then return nil end
  return layout.vbox(items)
end

--- Register the presentations for the native tools of a provider named `provider`.
---@type fun(provider: string)
function M.register(provider)
  -- Claude Code names the tool `Agent`; older versions call it `Task`.
  for _, name in ipairs({ "Agent", "Task" }) do
    smelt.transcript.register_tool(provider .. "/" .. name, {
      cache_key = "smelt.transcript.agents.agent:v3",
      background_hl = "SmeltAgent",
      title = agent_title,
      body = agent_body,
      compact = agent_compact,
    })
  end
  smelt.transcript.register_tool(provider .. "/SendMessage", {
    cache_key = "smelt.transcript.agents.send_message:v1",
    title = message_title,
    body = message_body,
  })
end

package.loaded["smelt.transcript.agents"] = M

return M
