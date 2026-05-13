-- Built-in /rewind command. Picks a past user turn and rewinds the transcript to it.

local function build_labels(turns)
  local labels = {}
  for i, t in ipairs(turns) do
    table.insert(labels, string.format("%d. %s", i, t.label or ""))
  end
  table.insert(labels, string.format("%d. (current)", #turns + 1))
  return labels
end

smelt.cmd.register("rewind", function(args)
  local turns = smelt.session.turns()
  if #turns == 0 then
    smelt.ui.notify_error("nothing to rewind")
    return
  end

  -- "insert" arg forces vim Insert restoration after rewind (used by the Esc-Esc keymap).
  local restore_vim_insert = (args == "insert") or (smelt.vim.mode() == "insert")

  smelt.spawn(function()
    local labels = build_labels(turns)
    local options_leaf = smelt.ui.dialog.options(labels, { selected = #labels })

    local picked = smelt.ui.dialog.open({
      title  = "rewind",
      height = 50,
      panels = { { leaf = options_leaf } },
      on_submit = function(ctx)
        ctx.resolve((smelt.win.cursor_row(options_leaf) or 0) + 1)
      end,
    })

    if picked == nil then return end

    local block_idx = nil
    if picked <= #turns then
      block_idx = turns[picked].block_idx
    end
    smelt.session.rewind_to(block_idx, { restore_vim_insert = restore_vim_insert })
  end)
end, { desc = "rewind to a previous turn" })
