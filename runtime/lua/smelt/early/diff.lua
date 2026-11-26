-- Declare `smelt diff`, the Git viewer that runs without an agent session.
-- `smelt/plugins/diff.lua` opens it with `smelt.cli.on_command`.

smelt.cli.register_command({
  name = "diff",
  description = "Review, stage, commit and push Git changes without an agent session",
  args = "VIEW",
})
