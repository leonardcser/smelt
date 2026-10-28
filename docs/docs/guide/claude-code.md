---
title: "Claude Code backend"
description: "Run the installed Claude Code executable as a smelt backend: authentication, native tools, background subagents, permissions and smelt tool bridging."
---

# Claude Code backend

The `claude-code` backend runs the installed, unmodified Claude Code executable.
It uses Claude Code's bidirectional JSON control channel, as used by the official
Agent SDK. It does not require Node.js or Python, import OAuth tokens, implement
subscription login, or send model requests to Anthropic itself.

## Get started

Install Claude Code using Anthropic's instructions, then authenticate in Claude
Code:

```sh
claude auth login
claude auth status
```

When the `claude` executable is installed, the model picker lists every model
Claude Code offers, such as `claude-code/opus` (Opus 5.5) or
`claude-code/claude-opus-4-8`. No provider configuration is needed. Each Claude
Code session reports the list, and smelt caches it for the next start. smelt asks
Claude Code for it separately, in the background, only when there is no cached
list or the `claude` executable has changed since. Until Claude Code has listed
its models once, the picker offers its `sonnet`, `opus`, and `haiku` aliases.
Claude Code decides which model each alias points to; the prompt bar names the
model it currently runs, such as `claude-code/haiku-5-5`. For a one-shot request:

```sh
smelt --headless --model claude-code/sonnet "Review this project"
```

`/usage` shows your plan's usage limits as Claude Code reports them: the 5-hour
and weekly limits, model-specific weekly limits, and extra usage when it is
enabled. smelt asks Claude Code for them when the dialog opens, without a model
request. API-key accounts have no plan limits to show.

Use a current Claude Code release. The backend requires stream-JSON input,
partial-message output, SDK MCP servers, and the initialization/control protocol.
A CLI with an incompatible protocol fails with an actionable error instead of
falling back to API-key requests.

## Configuration

To set per-model options or a default model, register the provider in
`init.lua`. It replaces the automatic one, and the models Claude Code lists are
still added:

```lua
smelt.provider.register("claude-code", {
  type = "claude-code",
  models = { "sonnet", "opus", "claude-opus-4-8" },
})
smelt.defaults.set({ model = "claude-code/sonnet" })
```

Do not set `api_base` or `api_key_env` on this provider. Configure API keys,
cloud-provider credentials, routers, and endpoints through Claude Code's own
supported configuration instead.

The default executable is `claude` on `PATH`. Set
`SMELT_CLAUDE_CODE_EXECUTABLE` to an executable path when needed. This is a path,
not a shell command; spaces are supported, shell arguments are not. On Windows,
use the native `claude.exe`, not an npm `.cmd` launcher.

`CLAUDE_CONFIG_DIR` is passed through unchanged. Authenticate with the same
configuration directory before starting smelt. The backend does not replace
`HOME`, enumerate Keychain entries, or refresh credentials itself.

## Agent behavior

Claude Code keeps its native agent loop, coding tools, system-prompt preset,
and automatic context management. There is one owner for each capability:

- Claude Code owns file access, editing, shell commands, search, native background
  tasks, and its native skills.
- smelt adds application capabilities, connected MCP tools, and complementary
  custom tools through an SDK MCP server named `smelt`.
- Native `AskUserQuestion` opens smelt's question dialog, including multi-select
  and free-text answers. Native `ExitPlanMode` uses smelt's saved-plan dialog.
  A draft or dismissal does not approve implementation. Claude Code's native
  plan files use a session-scoped `plansDirectory` under smelt's artifacts, with
  narrow read/write/edit grants in plan mode, not workspace write access. When
  the native callback omits plan text, smelt reads the last successfully written
  native plan file in a bounded background worker before asking for approval.
  Reads and failed writes do not select a different plan. The active plan's relative
  path is retained with the native session link. Empty, unavailable, escaped, or oversized plans
  (over 1 MiB) are rejected. Headless interactions are denied without assuming
  user approval.

Duplicate smelt file/shell/search tools and the internal question/plan adapters
are hidden from the model's MCP catalogue. Native tool names and IDs remain
visible in the transcript; their execution is not presented as a smelt job.
Claude Code job IDs and smelt-managed job IDs are separate.

### Prompt composition

By default, smelt preserves Claude Code's native preset and appends, in order:

1. Integration guidance and shared host rules, including managed-worktree rules.
2. smelt's loaded `AGENTS.md` project instructions.
3. The smelt skill catalogue, excluding skills loaded from `.claude/skills`.
4. Fragments registered with `smelt.agent.add_system_prompt`.

Claude Code loads its own `CLAUDE.md`, native skills, and configuration. smelt
still loads its own project instructions; it does not substitute its normal
agent persona or generic tool-use rules for Claude Code's preset.

`--system-prompt` replaces the native preset using Claude Code's actual
`--system-prompt` option. Host additions remain appended. `--no-system-prompt`
uses an empty native replacement and skips smelt's `AGENTS.md` instructions;
it does not remove host integration rules, plugin additions, or Claude Code's
own project configuration. Other model backends keep their normal prompt path.

### Custom tool ownership

Custom tools are visible by default. To hide one only from this backend, add
`claude_code = { enabled = false }` to its registration. To replace a native
capability, name the native tools explicitly:

```lua
smelt.tools.register({
  name = "project_shell",
  description = "Run commands through the project's custom executor",
  parameters = {
    type = "object",
    properties = { command = { type = "string" } },
    required = { "command" },
  },
  claude_code = { replaces = { "Bash" } },
  subpattern_parser = "shell",
  execute = function(args, ctx)
    return require("smelt.tools.bash").execute(args, ctx)
  end,
})
```

The native counterparts are disabled at process launch and denied if requested
through a control callback. The replacement still uses smelt's normal permission
and execution pipeline. Bundled equivalents such as `bash` require explicit
replacement ownership before they can be exposed. `edit_file` must replace both
`Edit` and `MultiEdit`. Conflicting owners and malformed policies fail closed.
These settings do not change other backends.

Ownership is captured at turn startup, including replacements limited to specific
smelt modes. If such a replacement is unavailable in the current mode, the native
counterpart remains disabled. A replacement marked `headless = false` is hidden
in headless mode, and its native counterpart stays disabled. Other tools remain
available. Registry changes take effect on the next turn.

Auxiliary requests, such as session titles and `/btw`, run separate Claude Code
processes with tools disabled. They do not alter the main native conversation.
Structured output uses Claude Code's JSON-schema option. Auxiliary requests
without an assigned model use Claude Code's `haiku` alias, the latest Haiku,
rather than the conversation's model. Assign one with
`smelt.model.preferred(name, key)`, for example `smelt.model.preferred("btw", "claude-code/opus")`.
Each auxiliary request starts its own Claude Code process, which adds under a
second of CLI startup.

## Permissions

Native tools pass through a `PreToolUse` control callback and smelt's central
permission policy. Known tools map to their corresponding smelt permission
names: for example, `Bash` uses `bash` rules and `Read` uses `read_file` rules.
Workspace restrictions still apply. Unknown tools are evaluated conservatively.
MCP tools exposed by smelt use the normal tool evaluation and approval flow.
Scoped artifact grants can override plan mode's default write restriction, but
never an explicit tool deny. The same policy applies in interactive and headless
mode, independent of whether the plan UI plugin is enabled.

Claude Code also applies its native permission system. smelt never starts it
with `--dangerously-skip-permissions`. Plan mode maps to Claude Code's plan mode;
other smelt modes retain Claude Code's default permission mode and use smelt's
policy for each tool. A user-approved tool may still be rejected by Claude Code.
In headless mode, an unresolved approval is denied rather than waiting for input.

Claude Code loads its own user and project configuration, including hooks,
plugins, skills, and MCP servers. These are executable configuration. Only use
this backend in projects whose Claude Code configuration you trust. smelt's
project-configuration trust mechanism does not sandbox Claude Code startup hooks.

## Sessions and cancellation

One Claude Code process serves a smelt session across turns. Each turn is
another user message on that process, so native context, prompt caches, and
background work carry over, and a turn completes as soon as Claude Code answers.
Mode, model, reasoning effort, and fast mode apply to the live process, even
mid-turn: Claude Code uses them from its next model request. A model switch also
recounts the context, so its size is current right away. Turning reasoning on or
off, the system prompt, and the exposed smelt tools are launch settings: a turn
that changes one of them relaunches the process.

The process follows smelt into a new workspace, such as a worktree. Claude Code
cannot change directory mid-turn, so once the tools of the step that moved have
returned, smelt interrupts the turn, moves the process, and has Claude Code
continue where it stopped. Tool calls that race the move are denied with a note
to retry them. When Claude Code asks to trust the new directory, smelt grants
it, since the move went through smelt's own tools and permissions. If Claude
Code cannot move, the turn ends with the reason, and the next turn launches it
in the new workspace.

A process that has had no turn, native background work, or running shell
command for five minutes is released, so idle sessions don't hold memory. The next turn relaunches it and
resumes the native session, which adds Claude Code's startup to that turn. Each
session needs its own process, since Claude Code serves one conversation per
process. With `--ephemeral`, the native session cannot be resumed, so the
process stays until the session ends.

smelt mirrors completed assistant steps and tool results into its canonical
session history. A small session-link file stores the native session ID and a
history digest, not credentials. After a restart or relaunch, the next turn
resumes that native session when the history, workspace, and Claude
configuration directory match.

With `--ephemeral`, smelt disables Claude Code's native session persistence and
does not write a session link. The live process keeps the conversation; a
relaunch imports it into a fresh native process.
One-shot headless and auxiliary requests also disable native session persistence.

The link also records a checkpoint at each answer. Rewinding to one of them
forks the native session at that answer, so the new branch keeps Claude Code's
own context without the rewound turns. A cancelled or failed turn may have done
more than smelt saw, so smelt never resumes past it, but earlier checkpoints stay
usable. After edited history or a provider switch, smelt starts a fresh native
session and imports the recent visible conversation as readable historical
context. This does not replay old tool calls.

Queued messages steer the running turn as they do with smelt's own agent: Claude
Code takes each one at its next step, alongside the results of the tools it is
running, or right after its answer. smelt acknowledges a message when Claude
Code takes it. Until then, pulling it back from the queue withdraws it from
Claude Code too.

Native tools that share a capability with a smelt tool are shown as that tool:
`Bash` renders as `bash` with a highlighted command, `Read` as `read_file`,
`Edit` as `edit_file` with a full-file diff, and so on, with the same permission
rules and dialogs. Native tools without a smelt counterpart are shown under the
provider's name, such as `claude-code/ToolSearch`, while permission rules match
their native name, `ToolSearch`.

Cancelling a turn (**Esc** or **Ctrl-C**) interrupts it and keeps the process:
background shell commands and background subagents keep running, and the next
turn continues on the same process. smelt declares that it stops background
tasks one by one, so Claude Code's interrupt spares them. Queued messages go
back to the prompt instead of reaching Claude Code. Resetting, loading,
rewinding, or switching the session ends the process with all its work. Claude
Code runs each shell command in a process session of its own, which outlives
its process, so smelt stops the shell commands first whenever it releases the
process, and bounds process shutdown.
When Claude Code cancels an individual request, smelt cancels its host evaluation
and execution tasks, including child tasks, and closes its question or permission
dialog. Cancelled requests cannot receive a late approval or tool reply.
On Unix, the backend contains the subprocess in its own process group and
terminates that group during cleanup.

## Shell commands

Claude Code runs `Bash` inside its own process, in the foreground or in the
background. `/ps` lists these commands next to smelt's own jobs, by their
command line, with the end of their output (the last 8 KiB Claude Code keeps).
**Alt-D** stops one with `stop_task`, and **Ctrl-G** moves the running foreground
commands to the background, so the turn continues without waiting for them.
Once Claude Code registers a foreground command, a few seconds in, its output
streams into the `bash` block as it runs.

When a background command ends, Claude Code wakes on its own to report it. The
work indicator shows the wake reply as it runs, and the reply is appended to the
session history like any answer. In headless mode smelt waits for running
background commands and their wake replies before reporting completion.

## Native subagents

Claude Code executes its own `Agent` tool (or `Task` on older releases). This does
not enable smelt-owned delegation tools. `/subagents` shows native child agents
and their separate, live, read-only transcripts, including nested agents. Child
text, reasoning, and tool steps are not appended to the parent's canonical
history. Native task IDs are namespaced separately from smelt-owned workers.

Child native tool calls use the same mandatory permission callback as parent
calls. A live record can expose **Alt-S** to request `stop_task` for that child.
This never falls back to interrupting the parent. A successful control reply is
not treated as proof of cancellation; task lifecycle events determine the
outcome. Control errors and unconfirmed stops remain visible in the viewer.
Restored records are read-only until their execution owner announces live work.

Task token totals are shown as agent-only telemetry. They are not added to the
parent's usage, because native model totals can already include child work.

Background subagents outlive the parent's result. In the TUI the parent turn
completes normally, and the prompt stays free while children run, as it does
for background shell commands. When a child ends, Claude Code wakes to report
it; the work indicator shows the wake reply, and a prompt sent meanwhile is
queued until the reply ends, so a turn never races it. Late child permission
requests, wake replies, and usage are routed to the same session. A wake reply
is appended to the canonical history exactly once. In headless mode smelt waits
for all background work and the wake reply before reporting completion.

**Ctrl-C** interrupts a running answer or wake reply and leaves background work
running: **Alt-S** in `/subagents` stops one child, and **Alt-D** in `/ps` stops
one shell command. Resetting, loading, or rewinding the session ends all of it.

## Boundaries

This is a separate agent backend, not subscription authentication for smelt's
native model/tool loop. Claude Code owns its model-request bodies, retries,
compaction, tool output, and native conversation files. smelt's HTTP provider
middleware and raw request audit do not inspect those requests.

The backend refuses to start when `smelt.settings.redact_secrets` is enabled.
smelt cannot redact native tool output before Claude Code sends it to its model.
Disable that setting explicitly to use this backend, or keep smelt's native
agent if pre-request redaction is required.

smelt tool definitions are captured when the process launches; a registry reload
relaunches it on the next turn.
A backend switch requires cancelling the active turn. Native background work is
scoped to the process rather than managed by smelt's job supervisor. The process
ends when the session is reset, loaded, or rewound, when a turn uses another
backend, when it has been idle for five minutes, or when smelt exits.

Anthropic's terms apply to use of Claude Code. This integration does not claim
Anthropic endorsement or special permission to intermediate subscriptions. Each
user authenticates through Claude Code and is billed under their own agreement.
