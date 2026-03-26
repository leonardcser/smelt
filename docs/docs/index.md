# Agent

A Rust TUI coding agent for any OpenAI-compatible API.

!!! warning

    **Early-stage project.** Expect bugs, incomplete features, and breaking
    changes. Update regularly.

<p align="center">
  <img src="https://raw.githubusercontent.com/leonardcser/agent/main/assets/demo.gif" alt="demo" width="800">
</p>

## Quick Start

```bash
cargo install --git https://github.com/leonardcser/agent.git
```

=== ":simple-ollama: Ollama"

    ```bash
    ollama pull qwen3.5:0.8b
    agent --model qwen3.5:0.8b --api-base http://localhost:11434/v1
    ```

=== ":fontawesome-brands-openai: OpenAI"

    ```bash
    read -s OPENAI_API_KEY && export OPENAI_API_KEY
    agent --model gpt-5.4 --api-base https://api.openai.com/v1 --api-key-env OPENAI_API_KEY
    ```

=== ":simple-anthropic: Anthropic"

    ```bash
    read -s ANTHROPIC_API_KEY && export ANTHROPIC_API_KEY
    agent --model claude-opus-4-5 --api-base https://api.anthropic.com/v1 --api-key-env ANTHROPIC_API_KEY
    ```

=== ":simple-openrouter: OpenRouter"

    ```bash
    read -s OPENROUTER_API_KEY && export OPENROUTER_API_KEY
    agent --model anthropic/claude-sonnet-4-6 --api-base https://openrouter.ai/api/v1 --api-key-env OPENROUTER_API_KEY
    ```

## Next Steps

<div class="grid cards" markdown>

-   **:lucide-rocket: Getting Started**

    ---

    Install, connect a provider, write a config file

    [:octicons-arrow-right-24: Guide](guide/getting-started.md)

-   **:lucide-terminal: Usage**

    ---

    Modes, tools, slash commands, file refs, queuing

    [:octicons-arrow-right-24: Guide](guide/usage.md)

-   **:lucide-paintbrush: Customization**

    ---

    Themes, settings, custom commands, AGENTS.md

    [:octicons-arrow-right-24: Guide](guide/customization.md)

-   **:lucide-book-open: Reference**

    ---

    CLI flags, keybindings, tools, permissions

    [:octicons-arrow-right-24: Reference](reference/cli.md)

</div>
