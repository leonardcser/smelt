-- Shared helpers for auxiliary LLM calls (prediction, title generation,
-- and similar one-shot background tasks).
--
-- Every auxiliary plugin sends the SAME `SYSTEM` string so they all share
-- a single Anthropic prompt-cache slot for the system block. Adding a few
-- extra tokens per request is the price for cache reuse across the
-- predict/title/etc. fleet.

local M = {}

M.SYSTEM = [[You are an auxiliary helper running alongside an interactive coding agent. Each request gives you a short transcript of the user's session followed by a single task instruction at the end. Follow ONLY the instruction at the end. Respond with exactly what the task asks for — no preamble, no apology, no explanation, no markdown fencing unless explicitly requested. If you cannot satisfy the task, reply with an empty string.]]

return M
