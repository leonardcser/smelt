-- Built-in /yank-block command.
--
-- Copies the transcript block under the cursor to the clipboard.
-- Thin wrapper around `smelt.transcript.yank_block()` which
-- handles the extract + copy + notify flow in Rust.

smelt.cmd.register("yank-block", function()
  smelt.transcript.yank_block()
end, { desc = "copy transcript block under cursor" })
