-- Example init.lua configuration.
-- Copy into ~/.config/smelt/init.lua (or dofile this from there).

-- Providers
smelt.provider.register("ollama", {
  type = "openai-compatible",
  api_base = "http://localhost:11434/v1",
  models = {
    "glm-5",
    { name = "qwen3.5:27b", temperature = 0.8, top_p = 0.95 },
  },
})

smelt.provider.register("openai", {
  type = "openai",
  api_base = "https://api.openai.com/v1",
  api_key_env = "OPENAI_API_KEY",
  models = { "gpt-5.4" },
})

-- MCP servers
smelt.mcp.register("filesystem", {
  command = { "npx", "-y", "@modelcontextprotocol/server-filesystem", "/home" },
  env = { DEBUG = "true" },
  timeout = 30000,
})

-- Settings
smelt.settings.vim = true
smelt.settings.auto_compact = false
smelt.settings.show_cost = true

-- Permission rules
smelt.permissions.set_rules({
  default = {
    bash = {
      allow = { "git log *", "git diff *", "git status *" },
    },
  },
  apply = {
    bash = {
      allow = { "git commit *", "git push *" },
    },
  },
  yolo = {
    mcp = {
      allow = { "*" },
    },
  },
})
