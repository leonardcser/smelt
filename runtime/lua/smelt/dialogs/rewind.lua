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
  local restore_vim_insert = (args == "insert") or (smelt.win.mode() == "Insert")

  smelt.spawn(function()
    local labels = build_labels(turns)
    local options = {}
    for _, label in ipairs(labels) do
      table.insert(options, { label = label })
    end

    local result = smelt.ui.dialog.open({
      title  = "rewind",
      panels = {
        { kind = "options", items = options, selected = #options },
      },
    })

    if result.action == "dismiss" or result.option_index == nil then
      return
    end

    local idx = result.option_index
    local block_idx = nil
    if idx <= #turns then
      block_idx = turns[idx].block_idx
    end

    smelt.session.rewind_to(block_idx, {
      restore_vim_insert = restore_vim_insert,
    })
  end)
end, { desc = "rewind to a previous turn" })
