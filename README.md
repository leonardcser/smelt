# agent

A Rust TUI coding agent. Connects to any OpenAI-compatible API (Ollama, OpenAI,
Anthropic, OpenRouter, etc.) and provides an interactive terminal interface for
code generation, analysis, and assistance.

> [!WARNING]
>
> **This is an early-stage project.** Expect bugs, incomplete features, and
> breaking changes. Make sure to regularly update.

<p align="center">
  <img src="assets/demo.gif" alt="demo" width="800">
</p>

## Quick Start

```bash
cargo install --git https://github.com/leonardcser/agent.git
```

**With Ollama (local):**

```bash
ollama pull qwen3.5:0.8b
agent --model qwen3.5:0.8b --api-base http://localhost:11434/v1
```

**With OpenAI:**

```bash
read -s OPENAI_API_KEY && export OPENAI_API_KEY
agent --model gpt-5.4 --api-base https://api.openai.com/v1 --api-key-env OPENAI_API_KEY
```

**With Anthropic:**

```bash
read -s ANTHROPIC_API_KEY && export ANTHROPIC_API_KEY
agent --model claude-opus-4-5 --api-base https://api.anthropic.com/v1 --api-key-env ANTHROPIC_API_KEY
```

## Features

- **Tool use** — file read/write/edit, glob, grep, bash, notebooks, web
  fetch/search
- **Permission system** — granular allow/ask/deny per tool, bash pattern, URL
- **4 modes** — Normal, Plan, Apply, Yolo (`Shift+Tab` to cycle)
- **Vim mode** — full vi keybindings for the input editor
- **Sessions** — auto-save, resume, fork, rewind conversations
- **Compaction** — LLM-powered summarization to stay within context limits
- **Reasoning effort** — configurable thinking depth (off/low/medium/high/max)
- **File references** — attach files with `@path` syntax
- **Multi-agent** — parallel subagents with inter-agent messaging (opt-in)
- **Skills** — on-demand specialized knowledge via `SKILL.md` files
- **MCP** — connect external tool servers via the Model Context Protocol
- **Custom commands** — user-defined slash commands via markdown files
- **Custom instructions** — project-level `AGENTS.md` files
- **Input prediction** — ghost text suggesting your next message
- **Image support** — paste from clipboard or reference image files
- **Headless mode** — scriptable, no TUI

## Configuration

Config file: `~/.config/agent/config.yaml` (respects `$XDG_CONFIG_HOME`).

```yaml
providers:
  - name: ollama
    type: openai-compatible # or: openai, anthropic
    api_base: http://localhost:11434/v1
    models:
      - qwen3.5:27b

  - name: openai
    type: openai
    api_base: https://api.openai.com/v1
    api_key_env: OPENAI_API_KEY
    models:
      - gpt-5.4

defaults:
  model: ollama/qwen3.5:27b # provider_name/model_name

settings:
  vim_mode: false
  auto_compact: false
```

See the [full documentation](docs/) for all config options, CLI flags,
keybindings, permissions, and more.

## Documentation

Full docs are available at [docs/](docs/) and can be built locally with
[Zensical](https://github.com/zensical/zensical):

```bash
pip install zensical
cd docs && zensical serve
```

## Development

```bash
cargo build       # compile
cargo run         # run
cargo test        # run tests
cargo fmt         # format
cargo clippy      # lint
```

## Acknowledgments

Inspired by [Claude Code](https://github.com/anthropics/claude-code).

## Contributing

Contributions welcome! Open an issue or pull request.

## License

MIT — see [LICENSE](LICENSE).
