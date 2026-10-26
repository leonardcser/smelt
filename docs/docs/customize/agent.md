---
title: "Skills, MCP servers and AGENTS.md"
description: "Shape what the smelt coding agent knows and can reach: AGENTS.md instructions, skills and MCP servers, with little or no Lua."
---

# Skills, MCP and instructions

Three ways to shape what the agent knows and what it can reach, none of which
need Lua beyond a line or two.

## Instructions (AGENTS.md)

Put an `AGENTS.md` in your project (the current directory or the nearest parent that has one), or in `~/.config/smelt/AGENTS.md` for
instructions that apply everywhere. Its contents are appended to the system
prompt for every conversation in that directory.

Use it for conventions, coding standards and anything the agent should always
know. Because it lives in the repo, a new teammate's agent picks up the same
rules. Disable it with `--no-system-prompt`.

## Skills

Skills are on-demand knowledge packs the agent loads with the `load_skill` tool.
Only the skills relevant to a task are pulled in, which keeps the system prompt
small.

Each skill is a directory with a `SKILL.md`:

```
skills/
  frontend-design/
    SKILL.md
    reference/
      examples.html
```

```markdown
---
name: frontend-design
description: Create production-grade frontend interfaces
---

## Instructions

Detailed instructions for the agent...
```

smelt scans these locations; later entries override earlier ones:

1. `~/.config/smelt/skills/*/SKILL.md`
2. `~/.claude/skills/*/SKILL.md`
3. `~/.agents/skills/*/SKILL.md`
4. `.smelt/skills/*/SKILL.md`
5. `.claude/skills/*/SKILL.md`
6. `.agents/skills/*/SKILL.md`

Run `/skills` to see what is loaded and where it came from.

## MCP servers

Connect external tool servers over the
[Model Context Protocol](https://modelcontextprotocol.io). If a server exists
for Postgres, Slack or your internal API, the agent can use it without any
plugin code. Each server runs as a child process.

```lua
smelt.mcp.register("filesystem", {
  description = "Read and write files under /tmp via MCP.",
  command = { "npx", "-y", "@modelcontextprotocol/server-filesystem", "/tmp" },
  env = { DEBUG = "true" },
  timeout = 30000,
})
```

| Field         | Description |
| ------------- | ----------- |
| `description` | Summary shown by `/mcp` |
| `command`     | Executable, as a string or an argv array |
| `args`        | Extra arguments appended to `command` |
| `env`         | Environment variables for the server process |
| `timeout`     | Connect and tool-call timeout in milliseconds (default `30000`) |
| `enabled`     | Set `false` to skip connecting on startup (default `true`) |

MCP tools appear to the agent as `servername_toolname` (for example
`filesystem_read_file`) and ask for permission by default. Control them with the
`mcp` bucket in [Permissions](../reference/permissions.md#configuring-permissions).
Run `/mcp` to see server state and tool names, or use `smelt.mcp.list()` and
`smelt.mcp.status(name)` from Lua.
