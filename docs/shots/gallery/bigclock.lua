-- /bigclock: a big clock in the middle of the screen. q or Esc puts it away.
local FONT = {
  ["0"] = { "███", "█ █", "█ █", "█ █", "███" }, ["1"] = { " █ ", "██ ", " █ ", " █ ", "███" },
  ["2"] = { "███", "  █", "███", "█  ", "███" }, ["3"] = { "███", "  █", "███", "  █", "███" },
  ["4"] = { "█ █", "█ █", "███", "  █", "  █" }, ["5"] = { "███", "█  ", "███", "  █", "███" },
  ["6"] = { "███", "█  ", "███", "█ █", "███" }, ["7"] = { "███", "  █", "  █", "  █", "  █" },
  ["8"] = { "███", "█ █", "███", "█ █", "███" }, ["9"] = { "███", "█ █", "███", "  █", "███" },
  [":"] = { " ", "▪", " ", "▪", " " },
}
local ui

local function close()
  if not ui then return end
  ui.paint:remove()
  ui.win:close()
  ui = nil
end

local function open()
  ui = { win = smelt.win.new(smelt.buf.new(), { surface = "readonly_text", hide_cursor = true }) }
  ui.paint = smelt.paint.register(function(s)
    local x = 0
    for ch in os.date("%H:%M:%S"):gmatch(".") do
      for row, line in ipairs(FONT[ch]) do s:put_str(x, row - 1, line, { fg = { 255, 140, 66 } }) end
      x = x + utf8.len(FONT[ch][1]) + 1
    end
    ui.paint:invalidate_after(1000)
  end)
  local layout = smelt.ui.layout
  smelt.overlay.new({
    name = "bigclock", anchor = "center", width = 27, height = 5, border = "none", modal = false,
    -- The empty window holds focus so the keymaps fire; the clock is a paint.
    layout = layout.vbox({
      { layout.leaf(ui.win), height = 0 },
      { layout.leaf(ui.paint, { measure = { 27, 5 } }), height = "fit" },
    }),
    keymaps = {
      { key = "q", on_press = close },
      { key = "<Esc>", on_press = close },
      { key = "<C-c>", on_press = close },
    },
  })
  ui.win:focus()
end

smelt.cmd.register("bigclock", function()
  if ui then close() else open() end
end, { desc = "show a big clock" })
