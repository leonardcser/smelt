---
name: handoff
description: Prepare a complete handoff summary for another agent to continue the current work.
---

# Handoff

Prepare a handoff note for another agent that will continue from the current conversation and repository state.

## Goal

Write the context the next agent needs to continue safely without rereading the whole conversation. Optimize for accurate continuity, not a polished user-facing summary.

## What to include

- Current objective and why the work is being handed off.
- User intent, constraints, and preferences that still matter.
- Repository/worktree state, branch names, and any task ids if known.
- Files and symbols changed or likely relevant, with short notes on why each matters.
- Decisions already made, including rejected alternatives when they affect future choices.
- Validation already run and the results.
- Known failures, incomplete work, blockers, and open questions.
- Concrete next steps in recommended order.
- Suggested skills only when a specific available skill is likely to help the next agent.
- Existing artifacts that already contain detailed context, referenced by path or URL.
- Any commands, environment details, or caveats the next agent should know.

## Output shape

Use this structure:

```markdown
## Handoff

One paragraph with the current objective and state.

## User intent

- Important request, preference, or constraint.

## Current state

- Branch/worktree/repo state.
- What has been changed so far.

## Relevant files and artifacts

- `path/to/file`: why it matters.
- `path-or-url`: existing PRD, plan, ADR, issue, commit, diff, or other artifact to read instead of duplicating it.

## Suggested skills

- `skill-name`: when and why the next agent should invoke it.

## Decisions

- Decision and reason.

## Validation

- Command: result.

## Open issues

- Issue, blocker, or uncertainty.

## Next steps

1. Specific next action.
```

## Rules

- Be factual and concise, but do not omit information needed to continue.
- Prefer concrete file paths, commands, names, ids, and observed outcomes.
- Clearly distinguish completed work from planned or suggested work.
- Do not duplicate detailed content already captured in durable artifacts such as PRDs, plans, ADRs, issues, commits, or diffs. Link or cite the path/URL and summarize only the handoff-relevant takeaway.
- In `Suggested skills`, list only skills that are likely useful for the next step. Include the exact skill name and when to use it. Use `- None` when no skill recommendation is useful.
- If a section has nothing relevant, write `- None`.
- Do not claim tests passed unless they were actually run.
- Do not include secrets, credentials, tokens, or hidden chain-of-thought.
- If the user provides a focus, prioritize that focus but still include critical continuation context.
