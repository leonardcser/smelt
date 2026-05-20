-- Idle-mode Esc-Esc: cancel any in-flight background work (`smelt.work.busy`
-- tokens, e.g. /compact), or rewind to the previous turn. Defers to Rust when
-- the agent is running — the cancel-agent path lives there.

smelt.keymap.set("", "<Esc><Esc>", function(ctx)
  if smelt.engine.is_running() then
    return false
  end

  local restore_insert = ctx.vim_mode_at_chord_start == "insert"

  if smelt.work.is_busy() then
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
end)
