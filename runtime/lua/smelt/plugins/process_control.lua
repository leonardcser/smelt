-- Ctrl-G: stop following a foreground bash job while it keeps running.

smelt.keymap.set("", "<C-g>", function()
  return smelt.process.detach_foreground()
end, { desc = "move running command to background" })
