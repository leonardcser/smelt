# Multi-Agent Mode

!!! warning

    **Experimental.** Subagents inherit the parent's model and run in Normal
    mode with workspace restriction and auto-compact enabled. The workspace
    boundary is best-effort — use a container for strong isolation.

## Enabling

```bash
smelt --multi-agent
```

Or in config:

```yaml
settings:
  multi_agent: true
```

Disable with `--no-multi-agent` (overrides config).

## How It Works

When enabled, the agent gains five additional tools for spawning and
communicating with subagents:

| Tool            | Description                                      |
| --------------- | ------------------------------------------------ |
| `spawn_agent`   | Launch a subagent with a task prompt             |
| `list_agents`   | List owned subagents and discovered peers        |
| `message_agent` | Send a message to one or more agents             |
| `peek_agent`    | Query an agent's context without interrupting it |
| `stop_agent`    | Terminate a subagent                             |

## Subagent Behavior

- Inherit the parent's **model** and **reasoning effort**
- Run in **Normal mode** with default permissions
- Have **auto-compact** enabled
- Are **workspace-restricted** to the parent's CWD
- **Persist between turns** — they listen for incoming messages
- **Self-terminate** when their task is complete

Agents get human-readable names (e.g., cedar, birch, plum).

## Depth and Limits

| Flag                    | Default | Description                                                |
| ----------------------- | ------- | ---------------------------------------------------------- |
| `--max-agent-depth <N>` | 1       | How deep agents can nest (agents can't spawn at max depth) |
| `--max-agents <N>`      | 8       | Max concurrent agents per session                          |

## Peer Discovery

Other interactive sessions in the same repository are automatically discovered.
The agent can communicate with peers using the same `message_agent` and
`peek_agent` tools.

## Managing Subagents

Use `/agents` to open a dialog listing all running subagents with their status
and task. From the parent session you can also use `stop_agent` via the model to
terminate a subagent, or `message_agent` / `peek_agent` to interact with it.

The status line shows the number of active subagents when any are running.
