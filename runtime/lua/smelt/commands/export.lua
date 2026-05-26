-- Built-in /export command. Copy conversation markdown to clipboard or write to a file.

local function format_timestamp(ms)
  if ms == nil or ms <= 0 then
    return os.date("%Y-%m-%dT%H:%M:%S")
  end
  return os.date("%Y-%m-%dT%H:%M:%S", math.floor(ms / 1000))
end

local function slugify(title)
  if not title or title == "" then
    return "conversation"
  end
  local out = title:lower():gsub("[^%w%-]+", "-"):gsub("^%-+", ""):gsub("%-+$", "")
  if #out > 40 then
    out = out:sub(1, 40):gsub("%-+$", "")
  end
  if out == "" then
    return "conversation"
  end
  return out
end

local function file_stamp(ms)
  local secs = (ms and ms > 0) and math.floor(ms / 1000) or os.time()
  return os.date("%Y%m%d-%H%M%S", secs)
end

local function default_export_path()
  local dir  = smelt.session.cwd() or "."
  local slug = slugify(smelt.session.title())
  local stamp = file_stamp(smelt.session.created_at_ms())
  local base = string.format("%s/smelt-%s-%s.md", dir, slug, stamp)
  local path = base
  local n = 2
  while true do
    local f = io.open(path, "r")
    if not f then break end
    f:close()
    path = base:gsub("%.md$", string.format("-%d.md", n))
    n = n + 1
  end
  return path
end

local function format_markdown()
  local parts = {}
  local title = smelt.session.title()
  if title and title ~= "" then
    table.insert(parts, "# " .. title .. "\n")
  end

  local meta = {}
  local model = smelt.model()
  if model and model ~= "" then
    table.insert(meta, "**Model:** " .. model)
  end
  local cwd = smelt.session.cwd()
  if cwd and cwd ~= "" then
    table.insert(meta, "**CWD:** `" .. cwd .. "`")
  end
  local created = smelt.session.created_at_ms()
  if created and created > 0 then
    table.insert(meta, "**Date:** " .. format_timestamp(created))
  end
  if #meta > 0 then
    table.insert(parts, table.concat(meta, " · ") .. "\n")
    table.insert(parts, "---\n")
  end

  local history = smelt.session.messages()
  local tool_results = {}
  for _, msg in ipairs(history) do
    if msg.role == "tool" and msg.tool_call_id and msg.content then
      tool_results[msg.tool_call_id] = { content = msg.content, is_error = msg.is_error }
    end
  end

  for _, msg in ipairs(history) do
    if msg.role == "system" then
      table.insert(parts, "## System\n")
      if msg.content then table.insert(parts, msg.content .. "\n") end
    elseif msg.role == "user" then
      table.insert(parts, "## User\n")
      if msg.content then table.insert(parts, msg.content .. "\n") end
    elseif msg.role == "assistant" then
      table.insert(parts, "## Assistant\n")
      if msg.content and msg.content ~= "" then
        table.insert(parts, msg.content .. "\n")
      end
      if msg.tool_calls then
        for _, call in ipairs(msg.tool_calls) do
          table.insert(parts, string.format("**Tool call:** `%s`\n", call["function"].name))
          table.insert(parts, "```json")
          table.insert(parts, call["function"].arguments)
          table.insert(parts, "```\n")
          local result = tool_results[call.id]
          if result then
            local tag = result.is_error and "Error" or "Result"
            table.insert(parts, string.format("**%s:**\n", tag))
            table.insert(parts, "```")
            table.insert(parts, result.content)
            table.insert(parts, "```\n")
          end
        end
      end
    end
  end

  return table.concat(parts, "\n")
end

smelt.cmd.register("export", function()
  if #smelt.session.messages() == 0 then
    smelt.notify.error("nothing to export")
    return
  end

  smelt.spawn(function()
    local options_leaf = smelt.dialog.menu({ "Copy to clipboard", "Write to file" })

    local picked = smelt.dialog.open({
      title  = "export",
      height = "30%",
      panels = { { leaf = options_leaf } },
    })

    if not picked or not picked.index then return end

    local markdown = format_markdown()
    if picked.index == 1 then
      smelt.clipboard.write(markdown)
      smelt.notify("conversation copied to clipboard")
    elseif picked.index == 2 then
      local path = default_export_path()
      local f, err = io.open(path, "w")
      if not f then
        smelt.notify.error("export failed: " .. (err or "unknown"))
        return
      end
      f:write(markdown)
      f:close()
      local name = path:match("([^/]+)$") or path
      smelt.notify("exported to " .. name)
    end
  end)
end, { desc = "copy conversation to clipboard" })
