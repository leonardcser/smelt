-- Optional /yank-block command. Copies the transcript block under the cursor to the clipboard.
-- Not autoloaded; add `require("smelt.plugins.yank_block")` to init.lua to enable.

smelt.cmd.register("yank-block", function()
  smelt.transcript.yank_block()
end, { desc = "copy transcript block under cursor" })
