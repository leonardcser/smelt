-- Messages you send while smelt is working wait above the prompt as numbered chips, in the order
-- they will go, instead of rows of gray text.
local COLORS = { { 130, 150, 255 }, { 250, 160, 90 }, { 140, 210, 110 }, { 200, 130, 250 } }
local queue = smelt.prompt.queue

queue:add("queued", function(ctx)
  local queued = smelt.prompt.queued_rows()
  if #queued == 0 then return nil end
  local runs, room = {}, ctx.width
  for i, row in ipairs(queued) do
    local color = COLORS[(i - 1) % #COLORS + 1]
    local text = (row.text or ""):gsub("%s+", " ")
    text = smelt.text.truncate_cells(text, math.min(28, room - 8))
    local chip = {
      { text = " " .. i .. " ", style = { fg = { 18, 18, 26 }, bg = color, bold = true } },
      { text = " " .. text .. " ", style = { fg = color, bg = { 36, 37, 48 } } },
      { text = " ", style = {} },
    }
    local w = smelt.text.width(text) + 6
    if w > room then break end
    room = room - w
    for _, r in ipairs(chip) do runs[#runs + 1] = r end
  end
  return { runs = runs, row = 1 }
end)
