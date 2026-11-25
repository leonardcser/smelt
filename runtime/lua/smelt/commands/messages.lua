-- Built-in /messages command. Full-body log of Lua errors, warnings, and notices.

smelt.keymap.scope("messages", {
  desc = "/messages dialog",
  extends = "dialog.view",
  actions = { clear = "Clear recorded messages and close" },
  keys = { { "n", "c", "clear" } },
})

local function format_lines(entries)
  if #entries == 0 then
    return { "  (no messages)" }
  end
  local lines = {}
  for i, e in ipairs(entries) do
    if i > 1 then table.insert(lines, "") end
    local repeats = e.count > 1 and string.format(" ×%d", e.count) or ""
    table.insert(lines, string.format("[%s] %s%s", e.kind, e.source, repeats))
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

    local body_buf = smelt.buf.new({ readonly = true })
    body_buf:lines(body_lines)
    local body_leaf = smelt.dialog.content({ buf = body_buf, interactive = true })

    smelt.dialog.open({
      title      = string.format("messages (%d)", #entries),
      max_height = "50%",
      panels     = { { leaf = body_leaf } },
      scope      = "messages",
      actions    = {
        clear = function(ctx) smelt.messages.clear(); ctx.close() end,
      },
    })
  end)
end, { desc = "show recorded messages (errors, warnings)" })
