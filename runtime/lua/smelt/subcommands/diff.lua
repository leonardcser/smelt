-- `smelt diff`, and /diff, which reads the same spec.

return {
  name = "diff",
  description = "Review, stage, commit and push Git changes without an agent session",
  args = "REVISION",
  args_description = "A revision to show in the history, or BASE..HEAD to review HEAD against BASE without a checkout. Either side defaults: BASE to main or master, HEAD to HEAD; `...` reads the same",
  rest = "PATH",
  rest_description = "Show only changes to these paths",
  flags = {
    {
      name = "branch",
      short = "b",
      kind = "boolean",
      description = "Review the branch against its base, by default main or master",
    },
    { name = "base", kind = "string", description = "Review the branch against BASE" },
    { name = "history", kind = "boolean", description = "Open the commit history" },
  },
}
