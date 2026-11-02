-- A `chart` tool: the agent passes labeled numbers and the transcript draws them as a bar chart,
-- each bar in its own color, measured to an eighth of a cell.
local L = smelt.layout
local EIGHTHS = { "", "▏", "▎", "▍", "▌", "▋", "▊", "▉" }
local COLORS = {}
for i, c in ipairs({ { 110, 160, 250 }, { 250, 150, 70 }, { 140, 210, 110 }, { 200, 130, 250 }, { 240, 110, 130 } }) do
  COLORS[i] = "ChartBar" .. i
  smelt.theme.set(COLORS[i], { fg = c })
end
local WIDTH = 30

smelt.tools.register({
  name = "chart",
  description = "Show the user numbers as a bar chart.",
  parameters = {
    type = "object",
    properties = {
      title = { type = "string" },
      bars = {
        type = "array",
        items = {
          type = "object",
          properties = { label = { type = "string" }, value = { type = "number" } },
          required = { "label", "value" },
        },
      },
    },
    required = { "bars" },
  },
  summary = function(args) return args.title or "chart" end,
  execute = function() return "shown" end,
})

smelt.transcript.register_tool("chart", {
  body = function(tool)
    local bars, top, pad = tool.args.bars or {}, 0, 0
    for _, bar in ipairs(bars) do
      top = math.max(top, bar.value or 0)
      pad = math.max(pad, smelt.text.width(bar.label or ""))
    end
    local rows = {}
    for i, bar in ipairs(bars) do
      local eighths = top > 0 and math.floor((bar.value or 0) / top * WIDTH * 8 + 0.5) or 0
      local fill = ("█"):rep(eighths // 8) .. EIGHTHS[eighths % 8 + 1]
      rows[#rows + 1] = L.line({
        { text = "  " .. smelt.text.fit(bar.label or "", pad) .. "  ", hl = "Comment" },
        { text = fill, hl = COLORS[(i - 1) % #COLORS + 1] },
        { text = " " .. tostring(bar.value), bold = true },
      })
    end
    return L.vbox(rows)
  end,
})
