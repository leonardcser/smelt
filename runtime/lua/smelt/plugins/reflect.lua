-- Built-in /reflect command. Body lives in
-- `crates/engine/src/prompts/commands/reflect.md` and is resolved by
-- `smelt.engine.submit_builtin_command` (frontmatter overrides + minijinja
-- template rendering with the current `multi_agent` context apply).

smelt.cmd.register("reflect", function(arg)
  smelt.engine.submit_builtin_command("reflect", arg)
end, { desc = "step back and rethink recent changes before moving on" })
