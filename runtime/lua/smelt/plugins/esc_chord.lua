-- Esc-Esc: cancel in-flight foreground/background work (`smelt.work.busy`
-- tokens, e.g. /compact), or rewind to the previous turn when idle.

smelt.keymap.set("", "<Esc><Esc>", function(ctx)
  local restore_insert = ctx.vim_mode_at_chord_start == "insert"

  if smelt.engine.is_running() or smelt.work.is_busy() then
    if smelt.engine.is_running() and smelt.session._rewind_active_turn_if_clean({ restore_vim_insert = restore_insert }) then
      return
    end
    smelt.engine.cancel()
    if restore_insert then
      smelt.vim.set_mode("insert")
    end
    return
  end

  if smelt.session.turns() == 0 then
    return
  end

  if restore_insert then
    smelt.cmd.run("rewind insert")
  else
    smelt.cmd.run("rewind")
  end
end, { desc = "cancel work / rewind" })
