-- `smelt.transcript.pending`: a strip bar under the transcript for blocks that
-- wait to join it. A delivery that arrives during a turn joins the transcript
-- at the next request boundary; until then it shows here, dimmed.
--
-- Deliveries of one `run` share a row, and the rows stack in the order each
-- run first arrived, which is also the order the runs join history. Add a
-- source for other blocks with `smelt.transcript.pending:add_source(name,
-- source)` and read them with `smelt.transcript.pending_blocks()`.

local bar = require("smelt._bar")
local builtins = require("smelt.transcript.builtins")
local defaults = require("smelt.transcript.defaults")

-- One item uses its header; several use the committed group's summary.
local function run_item(blocks, row)
  local text, hl = blocks[1].text, defaults.delivery_hl(blocks[1].run, blocks[1].failed)
  if #blocks > 1 then text, hl = builtins.delivery_summary(blocks[1].run, blocks) end
  return { text = text, style = { fg = hl, italic = true, dim = true }, truncatable = true, row = row }
end

local deliveries = {
  signals = { "transcript_pending_revision" },
  render = function()
    local runs, order = {}, {}
    for _, block in ipairs(smelt.transcript.pending_blocks()) do
      if block.kind == "delivery" then
        local run = block.run or "process"
        if not runs[run] then
          runs[run] = {}
          order[#order + 1] = run
        end
        local blocks = runs[run]
        blocks[#blocks + 1] = block
      end
    end
    local items = {}
    for row, run in ipairs(order) do items[row] = run_item(runs[run], row) end
    return items
  end,
}

--- The strip of blocks that wait to join the transcript. It takes no rows
--- while nothing is pending. Its built-in `deliveries` source shows one row
--- per run of pending deliveries, in the order each run first arrived.
---@tier ui_host
---@type smelt.bar.Bar
smelt.transcript.pending = bar.new({
  name = "smelt.transcript.pending",
  min_rows = 0,
  sources = { { "deliveries", deliveries } },
  win_opts = { pad_left = 0, pad_right = 1, focus_on_click = smelt.transcript.win },
})

return smelt.transcript.pending
