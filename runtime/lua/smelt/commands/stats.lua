-- Built-in /stats and /cost commands.
--
-- Both open a passive scrollable text dialog formatted Rust-side via
-- `smelt.metrics.*`; q / ? / Esc dismiss.

local function open_text_modal(title, text)
  smelt.spawn(function()
    smelt.ui.dialog.open({
      title  = title,
      panels = {
        { kind = "content", text = text, height = "fill" },
      },
      keymaps = {
        { key = "q", on_press = function(ctx) ctx.close() end },
        { key = "?", on_press = function(ctx) ctx.close() end },
      },
    })
  end)
end

smelt.cmd.register("stats", function()
  open_text_modal("stats", smelt.metrics.stats_text())
end, { desc = "show token usage statistics" })

smelt.cmd.register("cost", function()
  open_text_modal("cost", smelt.metrics.session_cost_text())
end, { desc = "show session cost" })
