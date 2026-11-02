-- Esc-Esc: restore queued input first, otherwise cancel in-flight work
-- or rewind to the previous turn when idle.

smelt.keymap.set("", "<Esc><Esc>", function(ctx)
  local restore_insert = ctx.vim_mode_at_chord_start == "insert"

  if __smelt_internal.prompt._unqueue() then
    if restore_insert then
      smelt.vim.set_mode("insert")
    end
    return
  end

  local continuation = smelt.engine.continuation_state()
  local paused = continuation.paused and continuation.token ~= nil
    and continuation.error_kind ~= "cancelled"
  if paused or smelt.engine.has_active_turn() or smelt.work.is_busy() then
    if __smelt_internal.session._rewind_active_turn_if_clean({ restore_vim_insert = restore_insert }) then
      return
    end
    smelt.engine.cancel()
    if restore_insert then
      smelt.vim.set_mode("insert")
    end
    return
  end

  if restore_insert then
    smelt.cmd.run("rewind insert quiet")
  else
    smelt.cmd.run("rewind quiet")
  end
end, { desc = "unqueue / cancel work / rewind" })
