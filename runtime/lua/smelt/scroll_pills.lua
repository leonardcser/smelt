-- Transcript navigation bars. The plugin supplies state and overlay placement.
local bars = require("smelt._bar")

local function new(kind)
  return bars.overlay({
    name = "smelt.scroll_pills." .. kind,
    style = { fg = "Comment", bg = "SmeltScrollPillBg", bold = true },
    win_opts = { surface = "inert" },
    on_press = function(ctx)
      if kind == "bottom" then smelt.transcript.follow_tail()
      elseif ctx.state.target then
        smelt.transcript.reveal(ctx.state.target, { align = "top", move_cursor = false })
      end
    end,
    sources = { { "label", {
      render = function(ctx)
        local text = ctx.state.label or ""
        if kind == "top" then
          text = " " .. smelt.text.fit(text, math.max(0, ctx.width - 2), { suffix = "…" }) .. " "
          text = text .. string.rep(" ", math.max(0, ctx.width - smelt.text.width(text)))
        end
        return { text = text, selectable = false }
      end,
    } } },
  })
end

--- Transcript-anchored bars with a built-in `label` source. Sources, view
--- factories and press handlers receive ctx.state = { label, target?, view }.
---@tier ui_host
---@type { top: smelt.bar.Bar, bottom: smelt.bar.Bar }
smelt.transcript.pills = { top = new("top"), bottom = new("bottom") }
return smelt.transcript.pills
