-- Ctrl-G: stop waiting for a foreground command or agent while it keeps running.

smelt.keymap.set("", "<C-g>", function()
  return smelt.process.detach_foreground()
end, { desc = require("smelt._detach").DESC })
