-- Which-key: ctrl+space in the prompt opens a popup of what the next key does; press one to run it,
-- or Esc to close. The keys are a table you can fill with your own.
local KEYS = {
  { "n", "new session", "new" },
  { "c", "compact the conversation", "compact" },
  { "m", "switch model", "model" },
  { "t", "pick a theme", "theme" },
  { "r", "resume a session", "resume" },
  { "h", "help", "help" },
}
local ui

local function close()
  if not ui then return end
  ui.paint:remove()
  ui.win:close()
  ui = nil
end

local function draw(s)
  for i, k in ipairs(KEYS) do
    local y = i - 1
    s:put_str(0, y, " " .. k[1] .. " ", { fg = { 20, 20, 28 }, bg = { 130, 150, 255 }, bold = true })
    s:put_str(4, y, "→", { fg = { 110, 110, 125 } })
    s:put_str(6, y, k[2], { fg = { 220, 220, 230 } })
  end
end

local function open()
  ui = { win = smelt.win.new(smelt.buf.new(), { surface = "readonly_text", hide_cursor = true }) }
  ui.paint = smelt.paint.register(draw)
  local layout = smelt.ui.layout
  local keymaps = {
    { key = "<Esc>", on_press = close }, { key = "q", on_press = close }, { key = "<C-c>", on_press = close },
  }
  for _, k in ipairs(KEYS) do
    keymaps[#keymaps + 1] = { key = k[1], on_press = function()
      close()
      smelt.cmd.run(k[3])
    end }
  end
  smelt.overlay.new({
    name = "whichkey", title = " ctrl+space ", anchor = "screen_at", corner = "se", row = 4, col = 2,
    width = 34, height = #KEYS + 2, modal = false,
    layout = layout.vbox({
      { layout.leaf(ui.win), height = 0 },
      { layout.leaf(ui.paint, { measure = { 32, #KEYS } }), height = "fit" },
    }),
    keymaps = keymaps,
  })
  ui.win:focus()
end

smelt.prompt.win:key("<C-Space>", function()
  if ui then close() else open() end
end)
