-- Idle-mode <Esc><Esc>:
--   * cancels an in-flight `/compact` if one is running, or
--   * rewinds the conversation to the previous user turn.
--
-- Defers to Rust when an agent turn is running: the running-mode
-- Esc-Esc cancel-agent path lives in `resolve_agent_esc` and treats
-- the second Esc as an immediate unqueue-or-cancel; folding it into
-- this chord would lose that nuance and tie agent state to a Lua
-- handler.

smelt.keymap.set("", "<Esc><Esc>", function(ctx)
  if smelt.engine.is_running() then
    return false
  end

  local restore_insert = ctx.vim_mode_at_chord_start == "Insert"

  if smelt.engine.is_compacting() then
    smelt.engine.cancel()
    if restore_insert then
      smelt.vim.set_mode("Insert")
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
