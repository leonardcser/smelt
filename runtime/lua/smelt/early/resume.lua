-- Declare the `--resume` CLI flag. Acted on by the `on_ready` hook in
-- `smelt/dialogs/resume.lua`: nil = no flag, "" = open picker, else = load
-- the given session id. Lives in the early phase so clap sees the flag.

smelt.cli.register_flag({
  name = "resume",
  short = "r",
  kind = "string",
  value_optional = true,
  description = "Resume a saved session by id, or open the picker when no id is given",
})
