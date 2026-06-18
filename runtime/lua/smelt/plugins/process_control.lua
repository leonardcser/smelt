-- Ctrl-G: move a foreground bash command to the background registry.

smelt.keymap.set("", "<C-g>", function()
  return smelt.process.detach_foreground()
end, { desc = "move running command to background" })
