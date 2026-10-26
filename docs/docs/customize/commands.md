---
title: "Custom slash commands"
description: "Add your own smelt slash commands as Markdown prompt templates or Lua functions, in your config or per project."
---

# Custom commands

Add your own `/commands` either as a Markdown prompt template (no Lua needed) or
as a Lua function. Type `/` in smelt to see built-in and custom commands
together.

## Markdown commands

Drop a `.md` file in `~/.config/smelt/commands/` (or `.smelt/commands/` for a
project). The filename is the command name and the body is the prompt. Markdown
commands are easy to version-control and share with a team.

`~/.config/smelt/commands/commit.md`:

```markdown
---
description: commit staged changes
model: openai/gpt-4o
reasoning_effort: low
bash:
  allow: ["git *"]
---

Create a conventional commit for the staged changes.

Staged diff:

!`git diff --cached`

Recent commits for style reference:

!`git log --oneline -5`
```

Type `/commit` and the agent receives the prompt with the shell output inlined.
Arguments are appended: `/commit fix typos`.

### Frontmatter

All fields are optional.

| Key                | Description |
| ------------------ | ----------- |
| `description`      | Shown in the `/` picker |
| `model`            | Model for this command. Prefer `provider/model`; a bare name works when unambiguous |
| `provider`         | Provider used to resolve a bare `model` |
| `temperature`, `top_p`, `top_k`, `min_p`, `repeat_penalty` | Sampling overrides |
| `reasoning_effort` | Reasoning level supported by the selected model |
| `override`         | When true, replaces a built-in command of the same name |
| `agent_skill`      | When true, also expose the command as a loadable [skill](../reference/api/skills.md) |
| `tools`            | `allow` / `ask` / `deny` lists for tool permissions |
| `bash`             | `allow` / `ask` / `deny` glob patterns for `bash` |
| `web_fetch`        | `allow` / `ask` / `deny` glob patterns for URLs |

### Running shell commands in the template

- Inline: `` !`command` `` is replaced by the command's output.
- Fenced: a ` ```! ` code block is replaced by its output.
- Escape: write `` \!`command` `` to keep it literal; the backslash is dropped.

Output is wrapped in a `<command_output>` element that records the exit code, so
the model can tell a failure from empty output.

## Lua commands

Use a Lua command when you need to open UI, change runtime state or call other
commands. Register it from `init.lua` or a plugin:

```lua
smelt.cmd.register("hello", function(arg)
  local name = (arg and arg ~= "") and arg or "world"
  smelt.notify.info("hello, " .. name .. "!")
end, { desc = "say hi" })
```

Use `smelt.cmd.run("name args")` to call another command. See
[Commands and events](../extend/commands-and-events.md) for options such as busy
behaviour and hidden commands.
