-- Override a built-in command and remap a keybind.
-- Drop this into ~/.config/smelt/init.lua to try.

-- Override /compact to show a confirmation first.
smelt.api.cmd.register("compact", function(arg)
  smelt.notify("compacting conversation...")
  smelt.api.cmd.run("/compact " .. (arg or ""))
end)

-- Remap Ctrl-S in normal mode to run /fork.
smelt.keymap("n", "<C-s>", function()
  smelt.api.cmd.run("/fork")
  smelt.notify("session forked")
end)
