-- Built-in /messages command.
--
-- Persistent log of Lua errors, warnings, and notices. Toasts show
-- only the first line; this dialog renders the full body (multi-line
-- tracebacks). Bottom-docked with a top-edge border + title only —
-- the ui crate's border is configured to paint just the top side, so
-- there is no manual separator buffer. Content panel is read-only +
-- vim-enabled so the user can navigate, select, and yank.
-- Opening clears the unread-error counter.

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
    -- gmatch leaves a trailing empty line on a trailing newline;
    -- strip it.
    if lines[#lines] == "  " then
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

    smelt.ui.dialog.open({
      placement        = "dock_bottom",
      placement_height = 40,
      border           = "top",
      title            = string.format(" messages (%d) ", #entries),
      panels           = {
        { kind = "content", buf = body_buf, height = "fill", interactive = true, focus = true },
      },
      keymaps = {
        { key = "q", on_press = function(ctx) ctx.close() end },
        { key = "<Esc>", on_press = function(ctx) ctx.close() end },
        { key = "c", on_press = function(ctx)
            smelt.messages.clear()
            ctx.close()
          end },
      },
    })
  end)
end, { desc = "show recorded messages (errors, warnings)" })
