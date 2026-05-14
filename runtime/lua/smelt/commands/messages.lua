-- Built-in /messages command. Full-body log of Lua errors, warnings, and notices.

local function format_lines(entries)
  if #entries == 0 then
    return { "  (no messages)" }
  end
  local lines = {}
  for i, e in ipairs(entries) do
    if i > 1 then table.insert(lines, "") end
    table.insert(lines, string.format("[%s] %s", e.kind, e.source))
    for body_line in (e.full or e.summary):gmatch("([^\n]*)\n?") do
      table.insert(lines, "  " .. body_line)
    end
    if lines[#lines] == "  " then -- gmatch trailing empty line on trailing newline
      table.remove(lines, #lines)
    end
  end
  return lines
end

smelt.cmd.register("messages", function()
  smelt.spawn(function()
    local entries = smelt.messages.list()
    smelt.messages.mark_read()
    local body_lines = format_lines(entries)

    local body_buf = smelt.buf.create({ readonly = true })
    smelt.buf.set_lines(body_buf, body_lines)
    local body_leaf = smelt.ui.dialog.content({ buf = body_buf, interactive = true })

    smelt.ui.dialog.open({
      title      = string.format("messages (%d)", #entries),
      max_height = 50,
      panels     = { { leaf = body_leaf } },
      keymaps    = {
        { key = "q",     on_press = function(ctx) ctx.close() end },
        { key = "<Esc>", on_press = function(ctx) ctx.close() end },
        { key = "c",     on_press = function(ctx) smelt.messages.clear(); ctx.close() end },
      },
    })
  end)
end, { desc = "show recorded messages (errors, warnings)" })
