-- Transcript block navigation. With vim, `[` and `]` plus a kind key move to the
-- previous or next block: `[[` / `]]` any block, `[u` / `]u` user messages,
-- `[a` / `]a` assistant messages, `[t` / `]t` tool calls. They take counts and
-- operators (`3]u`, `y]]`). Alt+Up / Alt+Down move over blocks and
-- Alt+Shift+Up / Alt+Shift+Down over user messages without vim. With the
-- transcript focused, Ctrl+O walks back through the jump list and Tab (Ctrl+I)
-- forward, as in vim, which handles those keys itself.
-- Disable via `smelt.builtins.disable({ plugins = { "block_nav" } })`.

local ALL = { "user", "assistant", "thinking", "tool", "code", "exec", "compacted" }

smelt.transcript.block_kind("[", ALL)
smelt.transcript.block_kind("]", ALL)
smelt.transcript.block_kind("u", { "user" })
smelt.transcript.block_kind("a", { "assistant" })
smelt.transcript.block_kind("t", { "tool" })

local function move(kind, forward, desc, chord)
  smelt.keymap.set("", chord, function()
    return smelt.transcript.move_block(kind, { forward = forward })
  end, { desc = desc })
end

move("[", false, "previous block", "<A-Up>")
move("]", true, "next block", "<A-Down>")
move("u", false, "previous user message", "<A-S-Up>")
move("u", true, "next user message", "<A-S-Down>")

local transcript = smelt.win.TRANSCRIPT

local function walk(chord, opts, desc)
  smelt.keymap.set("", chord, function()
    return transcript:focused() and smelt.vim.mode() == nil and transcript:jump(opts)
  end, { desc = desc })
end

walk("<C-o>", { older = 1 }, "back in the jump list")
walk("<Tab>", { newer = 1 }, "forward in the jump list")
