---
name: brief
description: Produce a compact but exhaustive brief of planned or completed changes, focused by default on user-facing behavior.
---

# Brief

Create a compact, exhaustive brief of what has changed or what is planned to change.

## Defaults

- Default scope is **user-facing behavior**: visible UX, commands, APIs, workflows, outputs, permissions, and behavior changes.
- Include internal implementation details only when the user asks for them, when they explain user-facing behavior, or when the requested focus is internal/all.
- Infer whether the brief is about a plan, completed work, or current diff from context. Do not ask for a mode unless the context is genuinely ambiguous.

## Output shape

Use this structure:

```markdown
## Brief

One or two sentences with the core outcome.

## Added

- **Thing**
  - New visible behavior or capability.

## Modified

| Area | Before | After |
|---|---|---|
| Thing | Previous behavior | New behavior |

## Removed

- **Thing**
  - Behavior or capability that goes away.

## Out of scope

- **Thing**
  - Explicitly not included.
```

## Rules

- Always include the five sections: `Brief`, `Added`, `Modified`, `Removed`, `Out of scope`.
- Use `- None` for an empty section.
- Use the `Modified` table for real before/after changes.
- Use bullets for `Added`, `Removed`, and `Out of scope`.
- Keep bullets short, but do not omit important behavior.
- Prefer concrete behavior over implementation trivia.
- Include enough context that the reader can understand the change without rereading the full discussion.
- Mention decisions only inside the relevant section; do not add a separate `Decisions` section unless the user asks.
- Do not add risks, test details, or long explanations unless the user explicitly asks for them.

## Scopes

If the user specifies a scope, follow it:

- `user`: only user-facing behavior and visible workflows.
- `internal`: only implementation, architecture, refactors, tests, and code organization.
- `all`: both user-facing and internal changes.

If no scope is specified, use `user`.
