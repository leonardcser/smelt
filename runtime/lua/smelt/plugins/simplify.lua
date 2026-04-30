-- Built-in /simplify command. Body lives in
-- `crates/engine/src/prompts/commands/simplify.md` and is resolved by
-- `smelt.engine.submit_builtin_command` (frontmatter overrides + minijinja
-- template rendering with the current `multi_agent` context apply).

smelt.cmd.register("simplify", function(arg)
  smelt.engine.submit_builtin_command("simplify", arg)
end, { desc = "review changed code for reuse, quality, and efficiency" })
