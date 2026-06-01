# Permission System Refactor Plan

## Goal

Build a permission system that is easy to reason about, hard to bypass by accident, and good enough to be useful without pretending to be a sandbox.

The current system blocks some dangerous or annoying actions, but the mental model is too fragmented:

- some permissions are mode defaults;
- some are tool defaults;
- some are Lua `decide` callbacks;
- some are path callbacks;
- some are runtime approvals;
- some tools only get checked because they happen to have `preflight` or `approval_patterns` hooks;
- the engine keeps its own per-turn mode copy, which can become stale after a mid-turn mode switch.

The target is not perfect security. Bash can always hide work behind another interpreter, a generated script, `python -c`, a downloaded binary, etc. The target is a compact, coherent, useful permission model that catches common mistakes, reduces annoying false positives, and makes the CLI understandable for new users.

The guiding principle for the refactor is:

```text
Tools describe. Modes decide. The evaluator combines. Dispatchers execute.
```

Installed Lua plugins remain trusted code. This permission system governs model-initiated tool calls; it is not a sandbox for arbitrary plugin code.

## Current problems found in the code

### 1. Lua tools can bypass permissions entirely

Lua tool dispatch currently depends on `ToolHookFlags::any()`. If no `approval_patterns` or `preflight` hook is registered, the engine dispatches the tool directly.

Relevant code:

- `crates/protocol/src/event.rs:107` documents the current opt-in hook behavior.
- `crates/protocol/src/event.rs:126` only tracks `approval_patterns` and `preflight`.
- `crates/protocol/src/event.rs:136` makes `any()` true only for those two hooks.
- `crates/engine/src/agent.rs:1291` uses `pt.hooks.any()` to decide whether to ask the TUI to evaluate hooks/permissions.
- `crates/engine/src/agent.rs:1312` directly dispatches Lua tools when no hook is present.

This violates the desired invariant: lower-level tools should not be able to opt out of permissions. Permission evaluation must be mandatory for every tool call.

Examples of affected tools:

- `runtime/lua/smelt/tools/read_file.lua:66` declares permission defaults and `runtime/lua/smelt/tools/read_file.lua:94` declares paths, but it has no `preflight` or `approval_patterns`; it can therefore bypass workspace permission checks.
- `runtime/lua/smelt/tools/glob.lua:16` and `runtime/lua/smelt/tools/glob.lua:40` have the same issue.
- `runtime/lua/smelt/tools/grep.lua:109` and `runtime/lua/smelt/tools/grep.lua:147` have the same issue.
- `runtime/lua/smelt/tools/load_skill.lua`, `stop_process.lua`, `smelt_reload.lua`, and similar tools have no permission metadata that forces evaluation.

Tools like `edit_file`, `write_file`, and `bash` are checked only because they incidentally have permission-triggering hooks:

- `runtime/lua/smelt/tools/edit_file.lua:104`
- `runtime/lua/smelt/tools/write_file.lua:40`
- `runtime/lua/smelt/tools/bash.lua:127`

That is backwards. The gate should always run; hooks should add metadata, previews, preflight errors, and approval suggestions, not decide whether permissions exist.

### 2. `ToolHooks` mixes metadata with authority

`ToolHooks` currently carries both call metadata and the final permission decision:

- `crates/protocol/src/event.rs:141` defines `Decision`.
- `crates/protocol/src/event.rs:146` makes `Decision::default()` be `Allow`.
- `crates/protocol/src/event.rs:162` stores `decision` in `ToolHooks`.
- `crates/core/src/lua/runtime.rs:1198` initializes hook output with `ToolHooks::default()`.

That shape is risky in a centralized gate. A missing hook response should mean "no extra metadata," not "permission allowed." Hook output should become metadata: summary, approval suggestions, and preflight errors. Permission authority should come from the evaluator.

### 3. MCP dispatch owns permission policy today

MCP currently holds global permissions inside the dispatcher:

- `crates/core/src/mcp/dispatcher.rs:9` defines `McpDispatcher`.
- `crates/core/src/mcp/dispatcher.rs:11` stores `Arc<Permissions>`.
- `crates/core/src/mcp/dispatcher.rs:81` makes the final MCP permission decision.

That is the wrong layer for the target model. Dispatchers should identify tools, provide metadata/preflight information, and execute calls. The turn/evaluator should own effective policy, because it has the current mode and per-turn overrides.

### 4. Mode changes are not coherent mid-turn

The TUI updates its current mode and appends a synthetic mode note, but the engine `Turn` keeps the mode it received at turn start.

Relevant code:

- `crates/tui/src/commands.rs:338` updates `self.core.config.mode`.
- `crates/tui/src/commands.rs:356` creates the mode-change note.
- `crates/tui/src/commands.rs:360` queues the note for history.
- `crates/engine/src/agent.rs:57` receives `mode` in `StartTurnPayload`.
- `crates/engine/src/agent.rs:90` stores it in `Turn.mode`.
- `crates/engine/src/agent.rs:1298` sends that stored mode in `ToolHooksRequest`.

There is no `UiCommand::SetMode`, and `handle_turn_cmd` has no mode update branch. This means a turn that starts in `yolo` can keep evaluating tools as `yolo` after the user switches to `normal`.

The model-visible synthetic history note is useful, but it should not be the runtime source of truth for permission decisions.

### 5. Workspace restriction is downstream of the same inconsistent gate

Workspace restriction is implemented as an `Allow -> Ask` downgrade:

- `crates/core/src/permissions/mod.rs:263`
- `crates/core/src/permissions/mod.rs:266`

Paths are provided by the Lua `paths_for_workspace` callback:

- `src/main.rs:374`
- `crates/core/src/lua/runtime.rs:1009`

This can work, but only when `Permissions::decide` actually runs. For no-hook Lua tools, it often does not run, so `paths_for_workspace` becomes dead metadata.

### 6. Bash path extraction is too naive

Current bash path extraction uses whitespace token scanning:

- `crates/core/src/permissions/workspace.rs:4`
- `crates/core/src/permissions/workspace.rs:8`
- `crates/core/src/permissions/workspace.rs:10`

This causes false positives:

- `git commit -m "fix /api/foo"` treats `/api/foo` as a local filesystem path.
- `ssh host 'cat /etc/passwd'` treats `/etc/passwd` as local, even though it is remote command text.
- `sed 's#/old#/new#' file` can treat regex fragments as paths.

It also misses useful cases:

- `find ../third_party` can escape the workspace without an absolute path token.
- `cd .. && grep ... other_project` changes effective working directory, but the extractor does not model that.

Because bash is not a security boundary, we should not build a large shell sandbox. We should replace the most annoying false positives and obvious misses with a compact best-effort analyzer.

### 7. Custom-command permission overrides are only partially applied

Custom commands build `permission_overrides` and create a per-turn `Permissions` clone for the TUI side:

- `crates/tui/src/app/agent.rs:246`
- `crates/tui/src/app/agent.rs:268`
- `crates/tui/src/app/agent.rs:307`

But the engine ignores `StartTurnPayload.permission_overrides`:

- `crates/engine/src/agent.rs:60` destructures it as `_`.

MCP decisions happen inside `McpDispatcher`, which holds the global `Arc<Permissions>`:

- `crates/core/src/mcp/dispatcher.rs:11`
- `crates/core/src/mcp/dispatcher.rs:81`

So overrides can apply to TUI-mediated Lua checks but not consistently to engine/MCP-side checks.

### 8. Headless has a separate permission path

Headless handles permission prompts by auto-answering based on `approves_permission`:

- `crates/core/src/headless_app.rs:36`
- `crates/core/src/headless_app.rs:126`
- `crates/core/src/headless_app.rs:206`

It also supports shell escapes with `!cmd`, which directly calls `sh -c` without the permission system:

- `crates/core/src/headless_app.rs:60`
- `crates/core/src/headless_app.rs:63`

Headless should share the same permission evaluator. It can choose a non-interactive response strategy for `Ask`, but it should not have a separate policy model.

### 9. Reload currently does not rebuild permissions

Permissions are built once at startup from Lua mode behaviors, permission rules, and tool defaults:

- `src/main.rs:212`
- `src/main.rs:213`
- `src/main.rs:215`
- `src/main.rs:367`

`/reload` refreshes prompt inputs and engine config, but there is no equivalent permission rebuild command:

- `crates/protocol/src/event.rs:411`
- `crates/engine/src/agent.rs:110`

If mode policies become more expressive, we should either explicitly document them as startup-only or rebuild the permission engine on reload. I recommend rebuilding, because mode/tool config already feels dynamic in Lua.

## Desired mental model

### Permission decisions are based on two things

1. **Tool identity**: what tool is being called?
2. **Effects**: what does this invocation intend to touch?

A call to `edit_file` is not the same as a call to `bash`, even if both write a file. `edit_file` has a narrow implementation and structured path argument. `bash` is broad and ambiguous. The permission engine should be able to account for both.

Example:

```text
tool: edit_file
effects: fs_write(/workspace/src/lib.rs)
```

Likely allowed in `apply`.

```text
tool: bash
effects: shell, fs_write(/workspace/src/lib.rs)
```

May still ask in `apply`, because shell has broader risk than `edit_file`.

### Modes own policy

Modes should answer: "In this mode, which tools/effects are allowed, asked, or denied?"

Tools should describe themselves and their effects. Tools should not decide whether permissions exist.

Modes can be registered dynamically, so missing mode policy must be safe:

- A registered mode with no permission policy inherits `normal`, or defaults to `ask`.
- `yolo` is permissive only because its mode policy explicitly says so.
- `apply` is permissive for structured in-workspace file edits only because its mode policy explicitly says so.

### Tools declare effects, not authority

A tool may declare:

```lua
effects = function(args)
  return {
    { kind = "fs_write", path = args.file_path },
  }
end
```

But the active mode decides whether `fs_write` is allowed.

Built-in tools can still ship suggested default mode policies, but those should compile into the mode policy. They should not be hidden per-call escape hatches.

Lua `decide` callbacks should be retired as authority. They can be replaced by `effects` or `classify` callbacks that describe what the call will do. Returning `allow` from tool-owned Lua should not be able to skip the mode policy or workspace policy.

### Bash is best effort

Bash permission checks are not a sandbox. They are a useful UX guardrail.

The analyzer should be compact and predictable:

- classify common safe read commands;
- classify common write/destructive commands;
- avoid known false positives in commit messages, ssh remote commands, sed scripts, URLs, and regex-looking strings;
- treat unknown commands conservatively in `normal` and `apply`;
- allow unknown commands in `yolo`, subject to workspace policy if enabled.

## Proposed architecture

### Target data flow

The end-state tool path should have one shape regardless of Lua, MCP, or core implementation:

```text
LLM tool call
  -> select implementation and origin
  -> collect metadata/effects
  -> evaluate effective turn policy
  -> ask/deny/error/dispatch
  -> record outcome
```

The current architecture already has the right broad pieces, but they are crossed in the wrong places:

- `Turn::classify_tools` selects tools and builds execution plans, but Lua tools can jump straight to dispatch when `ToolHookFlags::any()` is false.
- The TUI/Lua side can evaluate Lua metadata, but `ToolHooks` currently mixes that metadata with final permission authority.
- `McpDispatcher` can identify and execute MCP tools, but it also owns global permission policy.
- `Permissions::decide` combines several concerns but still depends on tool-owned Lua `decide` and raw path callbacks.

The refactor should converge those into one pipeline. The engine should own the sequencing and execution plan. The host/TUI may supply Lua metadata and user answers. The permission evaluator should combine policy. Dispatchers should only describe and execute.

### Architectural seams to create

Create these seams deliberately, then remove the old parallel paths in the same phase that replaces them:

1. **Tool selection**: resolve `tool_name` to `SelectedTool { origin, execution_mode, dispatch_target }` once. Lua overrides should be resolved here, not rechecked in several branches.
2. **Tool metadata**: collect `ToolMetadata { summary, approval_patterns, preflight_error }` without final authority.
3. **Tool effects**: collect `Vec<ToolEffect>` from structured tool metadata, MCP defaults, or shell analysis.
4. **Effective policy**: construct one per-turn permission snapshot from global config, active mode, custom command overrides, workspace config, and approvals.
5. **Evaluation**: one evaluator maps `PermissionRequest` to `PermissionOutcome`.
6. **Execution planning**: after evaluation, the plan contains only ready tool executions, pending user asks, and final synthetic outcomes. It should not have separate "pending hooks" vs "pending permissions" concepts long term.

A useful internal type for the engine side would be:

```rust
struct ToolCallPlanItem<'a> {
    call: &'a protocol::ToolCall,
    args: HashMap<String, Value>,
    selected: SelectedTool,
    metadata: ToolMetadata,
    effects: Vec<ToolEffect>,
    decision: PermissionOutcome,
}
```

The exact type can be simpler, but the important point is that the engine should plan one selected implementation through one permission pipeline. Avoid another boolean like `hooks.any()` becoming the accidental gate.

### Compatibility strategy

Compatibility bridges are acceptable only at the boundary and only while a phase is being completed. They should not become permanent alternate evaluators.

Allowed temporary bridges:

- translate old Lua `permission_defaults` into the new mode policy while bundled tools are migrated;
- translate old `paths_for_workspace` into filesystem effects until bundled tools expose `effects`;
- translate old `ToolHooks` into `ToolMetadata` while protocol cleanup happens.

Not allowed as end-state:

- `ToolHooks.decision` deciding authority;
- `paths_for_workspace` being the main workspace model;
- Lua `decide` returning final allow/ask/deny;
- MCP/core dispatchers owning final permission policy;
- engine branches where some tools skip the gate.

Each phase should delete the old internal branch it replaces before it is considered complete. Do not leave the system with two permission evaluators that happen to agree in tests.

### Core types

Add a typed effect model in Rust. Initial version should stay intentionally small.

```rust
enum ToolEffect {
    FsRead { raw_path: String, base_dir: PathBuf, path: PathBuf },
    FsWrite { raw_path: String, base_dir: PathBuf, path: PathBuf },
    Shell { command: String, risk: ShellRisk, paths: Vec<ShellPathEffect> },
    Network { url: String, domain: Option<String> },
    Mcp { server: Option<String>, tool: String },
    UserInteraction,
    ProcessControl { id: Option<String> },
    ConfigReload,
    Unknown,
}

enum ToolOrigin {
    Lua,
    Core,
    Mcp { server: Option<String> },
}

enum ShellRisk {
    ReadOnly,
    Writes,
    Destructive,
    Unknown,
}

enum ShellPathAccess {
    Read,
    Write,
    Unknown,
}

struct ShellPathEffect {
    raw_path: String,
    base_dir: PathBuf,
    path: PathBuf,
    access: ShellPathAccess,
}
```

Keep it small. Do not model every possible operation.

`ToolOrigin` matters because Lua can override core tool names. Policy can still mostly be keyed by tool name, but the evaluator should know whether the selected implementation is Lua, core Rust, or MCP. This avoids accidentally giving a broad custom implementation the same treatment as a narrow built-in implementation.

Filesystem effects carry both the raw path and the base directory used for resolution. Relative paths should resolve from the effective cwd for that tool/shell command, not blindly from the workspace root.

### Permission request

Every tool call produces one request before execution:

```rust
struct PermissionRequest {
    mode: AgentMode,
    tool_name: String,
    origin: ToolOrigin,
    args: HashMap<String, Value>,
    effects: Vec<ToolEffect>,
    metadata: ToolMetadata,
    frontend: FrontendKind,
}

struct ToolMetadata {
    summary: StyledLines,
    approval_patterns: Vec<String>,
    preflight_error: Option<String>,
}
```

The evaluator returns:

```rust
enum PermissionOutcome {
    Allow,
    Ask { approval_options: ApprovalOptions },
    Deny { reason: Option<String> },
    Error { message: String },
}
```

The protocol can continue using `Decision` for the wire initially, but internally a richer outcome will make dialogs and headless behavior cleaner.

### Permission gate plumbing

Prefer moving the permission round-trip onto the existing host-call channel rather than adding more `EngineEvent` request/response variants. The engine already has host-call plumbing for provider middleware. Tool metadata and permission evaluation can use the same style:

```rust
HostCall::EvaluateToolCall {
    call_id,
    tool_name,
    origin,
    args,
    mode,
    reply,
}
```

The central gate is logical, not necessarily a single function in the engine. Lua metadata must still be evaluated by the host/TUI because the Lua runtime lives there today. The important invariant is that every selected tool implementation produces metadata/effects and then passes through the same policy-combination semantics before dispatch.

`ToolHooks` should be split or reinterpreted as metadata. `preflight`, `approval_patterns`, and `summary` can remain, but final allow/ask/deny authority should come from the evaluator, not from tool hook defaults.

### Dispatcher responsibilities

`ToolDispatcher` should not own final permission policy. Its responsibilities should be:

- list available tools;
- identify the selected implementation/origin;
- provide metadata or preflight information when it can;
- dispatch approved calls.

The turn/evaluator should own final policy because it has the effective mode, per-turn command overrides, workspace settings, and runtime approvals. MCP visibility can still hide tools that are explicitly denied, but MCP dispatch should not be the only place where MCP permission decisions happen.

### Mode policy

A mode policy should combine tool defaults, effect defaults, and workspace behavior.

Conceptually:

```lua
smelt.mode.register({
  name = "apply",
  permissions = {
    default = "ask",
    tools = {
      read_file = "allow",
      glob = "allow",
      grep = "allow",
      edit_file = "allow",
      write_file = "allow",
      edit_notebook = "allow",
      bash = "ask",
    },
    effects = {
      fs_read = "allow",
      fs_write = "allow",
      network = "ask",
      shell_read = "allow",
      shell_write = "ask",
      shell_unknown = "ask",
      user_interaction = "allow",
      process_control = "allow",
      config_reload = "allow",
      mcp = "ask",
    },
    workspace = {
      outside = "ask",
    },
  },
})
```

For `normal`:

```lua
permissions = {
  default = "ask",
  effects = {
    fs_read = "allow",
    fs_write = "ask",
    network = "ask",
    shell_read = "allow",
    shell_write = "ask",
    shell_unknown = "ask",
  },
  workspace = { outside = "ask" },
}
```

For `yolo`:

```lua
permissions = {
  default = "allow",
  effects = {
    fs_read = "allow",
    fs_write = "allow",
    network = "allow",
    shell_read = "allow",
    shell_write = "allow",
    shell_unknown = "allow",
  },
  workspace = { outside = "ask" }, -- or configurable
}
```

This preserves the current idea that `restrict_to_workspace` can still downgrade `yolo` outside the workspace unless the user explicitly disables that.

### Decision combination

The rule should be simple:

```text
deny > ask > allow
```

Evaluate:

1. preflight errors;
2. explicit deny rules;
3. tool policy;
4. effect policy;
5. workspace policy;
6. runtime/session/workspace approvals.

If preflight fails, return `Error`.
If any policy layer says `Deny`, deny; approvals must not override deny.
If no layer denies but any layer says `Ask`, ask.
Only allow if all relevant layers allow or are irrelevant.
Runtime approvals can turn an `Ask` into `Allow` only when they match the exact pending reason for the prompt:

- tool pattern approvals for command/domain-style approvals;
- directory approvals for outside-workspace file effects;
- maybe future effect-specific approvals.

### Tool metadata

Replace `paths_for_workspace` with `effects` over time.

Initial Lua shape:

```lua
smelt.tools.register({
  name = "read_file",
  effects = function(args)
    return { { kind = "fs_read", path = args.file_path or "" } }
  end,
})
```

Keep `paths_for_workspace` as a compatibility shim temporarily:

- if a tool has `effects`, use it;
- else if a tool has `paths_for_workspace`, infer `FsRead` or `FsWrite` from built-in tool metadata;
- else use `Unknown`.

After migration, bundled tools should all have explicit effects and the compatibility path can be deleted. Tool-owned `decide` callbacks should be deprecated as authority during the migration; if a tool needs custom classification, make it return effects/classification data rather than a final decision.

### Workspace path resolution

Workspace checks should operate on normalized effect paths, but they should not pretend to be a filesystem sandbox. Symlinks, races, generated scripts, and external interpreters can still route around advisory checks.

Important implementation details:

- carry the raw path for display and approval matching;
- carry the base directory used to resolve relative paths;
- resolve ordinary tool arguments from the process cwd or tool-specific cwd;
- resolve bash path effects from the shell analyzer's current cwd;
- track simple `cd` across `&&`/`;` command chains where doing so stays compact;
- avoid resolving remote command text, URLs, regexes, and commit messages as local paths.

## Bash plan

### Recommendation: do not start with tree-sitter

`tree-sitter-bash` is attractive because it gives a real syntax tree for shell grammar. But it does not solve the semantic problems causing the most annoying false positives:

- commit message vs local path;
- ssh remote command vs local command;
- sed script vs path;
- URL path component vs filesystem path;
- command-specific flags like `git -C`, `find`, `tar -C`, etc.

Even with tree-sitter, we still need command-specific classification. Since the goal is not full security, I recommend starting with a compact custom analyzer built on the existing splitter plus better word parsing/classification. Add tree-sitter only if the compact analyzer becomes unmaintainable.

### Compact shell analyzer

Build a best-effort analyzer that returns a `ShellAnalysis`:

```rust
struct ShellAnalysis {
    risk: ShellRisk,
    path_effects: Vec<ShellPathEffect>,
    approval_patterns: Vec<String>,
    notes: Vec<String>,
}
```

Keep the existing split logic if useful, but replace `extract_paths_from_command` with command-aware extraction.

Phase 2 only needs a minimal bash effect adapter: command string, redirections, obvious paths, and `ShellRisk::Unknown` when unsure. Phase 3 is where the command-aware classifier should become good. This keeps the effect model unblocked without hiding a full bash rewrite inside the effects commit.

For each subcommand:

1. parse shell words with quote awareness;
2. identify command basename;
3. apply a small classifier table;
4. merge risk and path effects.

### Classifier table

Start with a small table for common commands.

#### Read-ish commands

- `ls`, `tree`, `cat`, `head`, `tail`, `grep`, `rg`, `find`, `wc`, `du`, `df`, `stat`, `file`, `realpath`, `pwd`, `which`
- `git status`, `git diff`, `git log`, `git show`, `git grep`, `git ls-files`
- `cargo check`, `cargo test`, `cargo nextest`, `cargo build` can be `ShellRisk::ReadOnly` or `Unknown` depending how conservative we want to be; they can write build artifacts, but we usually do not want prompts for them.

#### Write/destructive commands

- `rm`, `rmdir`, `mv`, `cp`, `touch`, `mkdir`, `chmod`, `chown`, `ln`
- `sed -i`, `perl -pi`
- `git commit`, `git reset`, `git checkout`, `git clean`, `git stash`, `git apply`, `git am`, `git merge`, `git rebase`
- package manager install/update commands

#### Remote/network commands

- `ssh`: do not parse the remote command string as local paths. Only local flag operands like `-i keyfile`, config files, or redirections count as local effects.
- `scp`/`rsync`: classify local operands only when distinguishable; otherwise `Unknown`.
- `curl`/`wget`: network, plus file write when `-o`, `-O`, or redirection is present.

#### Script/interpreter commands

- `python -c`, `node -e`, `ruby -e`, `perl -e`, `bash -c`, `sh -c`: `ShellRisk::Unknown` or `Writes` depending mode policy. Do not try to parse nested language strings.
- Running a script file: path read for the script; risk unknown unless command is explicitly allowed by pattern.

### Avoid the known false positives

The classifier should intentionally ignore path-like strings in these positions:

- `git commit -m/--message <text>`
- `git commit -F <file>` should count the file, because that is local input.
- `ssh <host> <remote-command>` remote command string should not count local paths.
- `sed <script> <files...>` script argument should not count as path unless `-f <script-file>`.
- URLs should not count as local paths.
- Regex/glob/pattern arguments for `grep`, `rg`, `find -name`, etc. should not count as local paths.

This directly addresses the annoying cases without claiming to secure shell.

### Redirections

Redirections are local effects and should remain easy:

- `> file`, `>> file`, `&> file`, `&>> file` => `FsWrite(file)` unless `/dev/null`.
- `< file` => `FsRead(file)`.
- `2>&1` and similar fd duplication => no file effect.

The existing output-redirection scanner can be reused or folded into the analyzer.

### Unknown commands

Unknown shell command policy should be mode-owned:

- `normal`: ask.
- `apply`: ask, unless explicitly allowed by pattern or classified read-ish.
- `yolo`: allow, subject to workspace restriction.

This keeps the implementation small and predictable.

## Runtime approvals and persistence

Current runtime approvals are tool/pattern based plus directory approvals:

- `crates/core/src/permissions/approvals.rs:12`
- `crates/core/src/permissions/store.rs:6`

This can stay initially.

But the evaluator should apply approvals after effects are known:

- Tool approvals can approve repeated command/domain/tool patterns.
- Directory approvals can approve outside-workspace `FsRead`/`FsWrite` effects.
- Workspace approvals should persist in the existing store format initially to avoid migration churn.

Later, we may add effect-scoped approvals, but not in the first refactor.

## Headless behavior

Headless should use the same evaluator.

For `Ask`, because there is no UI:

- default behavior should be deny/fail closed with a clear message;
- provide CLI flags later if desired, e.g. `--allow`, `--allow-tool`, `--allow-effect`, `--yolo`, or `--ask-policy=deny|allow`.

The `!cmd` headless escape should either:

1. remain explicitly documented as a local manual shell escape outside agent permissions; or
2. be routed through the same bash analyzer/evaluator.

I recommend option 2 for consistency unless this escape is intentionally meant as a user-operated shortcut rather than an agent action.

## Reload behavior

Because Lua can register modes/tools, and the plan moves more policy into mode/tool registration, reload should rebuild the permission policy snapshot.

Suggested behavior:

- reload Lua;
- rebuild mode registry;
- rebuild tool effect metadata and mode policy;
- preserve runtime approvals;
- preserve workspace approvals;
- send an engine command to update current mode/policy-relevant config if needed.

If full reload support is too much for the first commit, document permissions as startup-only for that commit. But the target should be reloadable.

## Implementation methodology

This is a cross-cutting refactor, so the safest method is not "add the new system beside the old system and slowly hope callers move." That would create exactly the kind of stale bypass paths this refactor is meant to remove.

Use this methodology instead:

1. **Characterize first.** Add regression tests around the current bugs and expected compatibility before restructuring: no-hook Lua tools, MCP overrides, mid-turn mode changes, workspace downgrades, approvals, and bash false positives.
2. **Factor the pipeline before changing the policy language.** First make all tool calls flow through one selected-tool pipeline using the existing policy semantics. This keeps behavior changes small while removing accidental bypasses.
3. **Make metadata non-authoritative before adding effects.** Split hook metadata from final decisions early. If this is delayed, the new effect model will inherit the same authority confusion.
4. **Cut over whole internal paths.** When a phase replaces a branch, delete the replaced branch in that phase. Do not leave a Lua path, MCP path, and core path each making final permission decisions differently.
5. **Keep compatibility at registration boundaries only.** If old Lua fields need support temporarily, normalize them into the new internal structures immediately when tools/modes are registered. The evaluator should see only the new model.
6. **Move tests with the abstraction.** Unit-test the evaluator and shell analyzer directly, but also keep integration tests that prove every dispatch path uses the gate.
7. **Regenerate docs/stubs in the same phase as API changes.** Lua-facing permission APIs should not drift from generated docs.

### Definition of done for each phase

A phase is complete only when:

- there is one internal path for the behavior the phase changes;
- old internal helpers are deleted or reduced to boundary-only normalization;
- tests cover Lua, MCP/core, headless if applicable, and workspace restriction;
- the user-visible prompts remain acceptable for bundled tools;
- comments describe the new model, not historical behavior.

### Recommended commit shape

Keep the user-requested larger commits, but structure each large commit internally as a clean cutover:

1. add characterization tests;
2. introduce the new seam/type;
3. migrate all callers in that seam;
4. delete the old branch/helper/protocol path;
5. update docs and tests.

If a commit cannot delete the old branch, it is probably too broad or the seam is wrong.

## Implementation phases

The phases are intentionally large. The goal is to make meaningful commits, not tiny mechanical patches. They should still be clean cutovers: each phase should leave less duplicated permission logic than it found.

### Phase 1: Mandatory permission pipeline and architecture cleanup

Objectives:

- Every tool call goes through one permission decision path before execution.
- Mid-turn mode changes update the engine immediately.
- Hook metadata no longer implies authority.
- MCP/core/Lua dispatch no longer have separate final policy paths.
- Existing user-facing behavior is preserved where possible.

Work:

1. Add characterization tests before restructuring:
   - no-hook Lua tools still dispatch successfully when allowed;
   - no-hook Lua tools cannot bypass workspace restriction;
   - MCP tools respect per-turn permission overrides;
   - `Decision::default() == Allow` cannot grant permission by omission;
   - mid-turn mode changes affect the next tool decision.
2. Add `UiCommand::SetMode { mode }` and send it from `TuiApp::set_mode` whenever the mode changes.
3. Update `Turn.handle_turn_cmd`, `execute_concurrent`, `call_llm`, `wait_for_tool_result`, and the outer engine loop to apply mode changes immediately instead of deferring them.
4. Create a `SelectedTool`/tool-selection helper used by `Turn::classify_tools` so Lua overrides, MCP/core tools, sequential execution, and dispatch target are resolved once.
5. Move tool metadata and tool dispatch request/response traffic onto the existing host-call channel where practical. The current `crates/engine/src/host.rs` already has the right direction with `DispatchTool`, `EvalHooks`, and `AskPermission`; use that instead of growing more `EngineEvent`/`UiCommand` response pairs.
6. Split or reinterpret `ToolHooks` so hook output is metadata (`summary`, `approval_patterns`, `preflight_error`) rather than authority. Do not let `Decision::default() == Allow` decide anything.
7. Stop using `ToolHookFlags::any()` as the permission gate trigger. Metadata hooks may be optional; the gate is not optional.
8. For Lua tools, always evaluate metadata/effective permission before dispatch. If a tool has no metadata, use empty metadata plus the legacy policy default, not direct dispatch.
9. Move final MCP/core permission decisions out of `McpDispatcher` and into the turn/evaluator path. `ToolDispatcher::evaluate_hooks` should either become metadata-only or be replaced by a clearer metadata method.
10. Apply custom-command `permission_overrides` consistently by making the turn hold an effective permission snapshot or override-aware evaluator used for Lua and MCP/core paths.
11. Route headless permission decisions through the same evaluator/host-call shape. Its non-interactive `Ask` strategy can fail closed, but it should not have a separate policy model.
12. Inventory every bundled Lua tool before enabling mandatory gating and assign a legacy default/effect category so the UX does not regress into prompt spam.
13. Delete the old internal bypass branches before the phase is done:
    - no `pt.hooks.any()` dispatch bypass;
    - no authoritative `ToolHooks.decision` default path;
    - no MCP-only final decision path;
    - no separate TUI-only permission override behavior.

Important compatibility notes:

- Keep `preflight`, `approval_patterns`, `summary`, `decide`, and `paths_for_workspace` working during this phase if needed, but normalize them into metadata/legacy policy at the boundary.
- Treat Lua `decide` as legacy compatibility only. It may feed the old policy adapter in Phase 1, but it should not survive as final authority after Phase 2.
- Do not migrate to typed effects in this phase unless it is necessary for a clean seam. The main goal is one mandatory pipeline with old semantics.

Phase 1 is the most important phase. If it is done well, later policy changes become local evaluator/tool-metadata work instead of another engine/TUI/MCP cross-cutting rewrite.

### Phase 2: Effects, mode-owned policy, and removal of legacy permission internals

Objectives:

- Modes become the source of permission policy.
- Tools describe effects.
- Workspace restriction operates on typed effects instead of raw path strings.
- Legacy `decide`/`paths_for_workspace` logic is removed from the internal evaluator.

Work:

1. Add characterization tests for the policy model before changing internals:
   - missing custom mode policy falls back safely;
   - missing tool effects become `Unknown` and ask in normal/apply;
   - structured in-workspace file writes are allowed in apply;
   - outside-workspace effects are downgraded to ask;
   - runtime approvals cannot override explicit deny.
2. Add `ToolEffect`, `ToolOrigin`, `ToolMetadata`, `PermissionRequest`, and `PermissionOutcome` as real internal types rather than extending old hook structs indefinitely.
3. Extend `smelt.tools.register` with `effects = function(args) ... end`.
4. Extend `smelt.mode.register` permissions from the current behavior-only fields into a mode policy table.
5. Normalize old registration fields into the new structures at the Lua registration boundary only:
   - old `permission_defaults` -> mode policy defaults;
   - old `paths_for_workspace` -> fallback filesystem effects while bundled tools are migrated;
   - old `decide` -> temporary compatibility warning/error path, not final authority.
6. Compile mode policies into Rust at startup and reload.
7. Resolve filesystem effects with raw path + base directory + normalized path so workspace checks do not assume every relative path starts at the workspace root.
8. Add a minimal bash effect adapter using the existing parser/redirection scanner: command string, obvious path effects, and conservative `Unknown` risk when unsure.
9. Migrate bundled tools completely:
   - `read_file` => `FsRead(file_path)`
   - `write_file` => `FsWrite(file_path)`
   - `edit_file` => `FsWrite(file_path)`
   - `edit_notebook` => `FsWrite(notebook_path)`
   - `glob`/`grep` => `FsRead(path or cwd)`
   - `bash` => `Shell(...)`
   - `web_fetch`/`web_search` => `Network(...)`
   - `ask_user_question` => `UserInteraction`
   - `read_process_output`/`stop_process` => `ProcessControl`
   - `smelt_reload` => `ConfigReload`
10. Replace `Permissions::decide(tool_name, args, is_mcp)` with an evaluator over `PermissionRequest`/effects. Keep a thin compatibility wrapper only if external callers still need it, and make it build a request internally.
11. Delete old internal permission mechanisms before the phase is done:
    - no internal `paths_for_workspace` workspace model for bundled tools;
    - no authoritative Lua `decide` callback;
    - no `is_mcp` boolean deciding a separate policy branch;
    - no `ModeBehavior` booleans as the main policy representation.
12. Update docs/reference stubs after Lua API changes.

Mode defaults should be explicit and readable in `runtime/lua/smelt/modes.lua`. The final Rust evaluator should receive one normalized policy representation, not a mixture of mode behaviors, tool defaults, rule sets, callbacks, and MCP special cases.

### Phase 3: Compact bash analyzer and deletion of shell-token heuristics

Objectives:

- Remove the annoying false positives.
- Catch obvious workspace escapes.
- Keep the analyzer compact.
- Delete raw whitespace path scanning and basename-only approval heuristics.

Work:

1. Add characterization tests for current desired shell behavior before changing the analyzer:
   - commit messages, ssh remote commands, sed scripts, URLs, regexes, redirections, and simple `cd` chains.
2. Replace `extract_paths_from_command` with `analyze_shell_command`.
3. Reuse existing split/redirection code where it stays simple, but make the analyzer the single shell metadata/effects entry point.
4. Add command classifiers for common commands.
5. Track simple cwd changes from `cd` across straightforward command chains.
6. Ignore path-like strings in known non-path positions.
7. Generate bash approval patterns from classifier output rather than raw basename-only heuristics.
8. Delete or reduce old shell permission helpers to private parser utilities only. End state should not have one helper deciding shell allow/ask and another helper extracting workspace paths from unrelated token logic.
9. Add tests for:
   - `git commit -m "fix /api/foo"` does not report `/api/foo` as local path;
   - `ssh host 'cat /etc/passwd'` does not report `/etc/passwd` as local path;
   - `sed 's#/old#/new#' file` reports only `file`;
   - `find ../third_party` reports an outside-workspace read;
   - `cd .. && grep needle other_project` resolves `other_project` from the updated cwd;
   - output redirection reports write effects;
   - `/dev/null` redirection is ignored;
   - unknown commands are classified as `Unknown`.

Do not add tree-sitter in this phase unless the custom analyzer becomes larger or more fragile than expected. If tree-sitter is introduced, it should replace the custom word/command parser, not sit beside it as a second partial shell model.

### Cross-cutting cleanup checklist

Before calling the refactor done, do a deliberate deletion pass:

- remove protocol comments that describe hook opt-in security-sensitive tools;
- remove or rename `ToolHookFlags::any()` so it cannot be mistaken for a permission gate;
- remove authoritative `ToolHooks.decision` use, or make it impossible to default to allow;
- remove `McpDispatcher`'s dependency on `Permissions` if final policy has moved to the evaluator;
- remove internal `paths_for_workspace` and `decide_hook` usage for bundled tools;
- remove shell whitespace path scanning as a permission input;
- update generated Lua API docs/stubs;
- ensure `cargo fmt`, clippy, and permission tests pass;
- add an architecture comment near the evaluator stating the invariant: every selected model tool call passes through this gate before dispatch.

This cleanup pass is not a separate long-lived migration phase. It is the final part of the large commits above. The goal is to end with fewer permission concepts than the current code, not a new system layered over the old one.

## Why this plan fits the existing code

### It works with the current engine/TUI split

The engine already separates Rust/MCP dispatch from Lua tool dispatch:

- MCP/core tools use `ToolDispatcher::evaluate_hooks` and `dispatch`.
- Lua tools are proxied through `ToolHooksRequest` and `ToolDispatch`.

The plan keeps the execution split but changes the invariant: permission evaluation happens before every dispatch path. Where possible, the refactor should move tool metadata/permission round-trips to the existing host-call channel so the protocol does not keep growing ad hoc request/response event pairs.

### It works with Lua-defined tools

Lua already stores handles for permission-relevant callbacks:

- `crates/core/src/lua/shared.rs:62`
- `crates/core/src/lua/shared.rs:67`
- `crates/core/src/lua/shared.rs:69`

Adding `effects` is a natural extension. Existing `paths_for_workspace` can be used only as a boundary compatibility input while bundled tools migrate; it should not remain an internal evaluator concept.

### It works with registered modes

Modes already have permission behavior metadata:

- `runtime/lua/smelt/modes.lua:96`
- `runtime/lua/smelt/modes.lua:104`
- `runtime/lua/smelt/modes.lua:116`
- `runtime/lua/smelt/modes.lua:128`

The plan extends this from a few behavior booleans into a complete policy table. Unknown or incomplete policy remains safe by defaulting to normal/ask.

### It works with existing approvals

The current approval store can stay initially:

- `crates/core/src/permissions/store.rs:6`
- `crates/core/src/permissions/approvals.rs:12`

Directory approvals become more meaningful because they apply to typed filesystem effects instead of raw strings pulled from arbitrary command text.

## Things to avoid

### Do not make permissions an optional Lua plugin

That would make the default agent easier to ship but harder to reason about. It would also widen the TUI/headless gap. Lua should define tools, effects, summaries, and mode policy tables; Rust should enforce the mandatory gate.

### Do not aim for bash sandboxing

We cannot make bash perfect. The agent can route around it through generated scripts or interpreters. The right goal is useful classification, fewer annoying prompts, and clear behavior for unknown commands.

### Do not let missing metadata mean allow

Missing effects should become `Unknown`, and the active mode should decide what to do with unknown effects. In normal/apply, that should usually ask.

### Do not preserve hook-triggered permission semantics

`preflight` and `approval_patterns` are not permission opt-ins. They are extras attached to a permission request.

### Do not let dispatchers decide final policy

MCP/core/Lua dispatchers can describe and execute. They should not own the final allow/ask/deny decision because they do not have the full effective turn policy.

### Do not treat installed plugins as untrusted code

This refactor should prevent model-initiated tool calls from bypassing permissions. It should not attempt to sandbox trusted local Lua plugins. Trying to solve that here would make the design much larger without addressing the user's main UX and policy problems.

## Final recommendation

Use the effect/capability model as the target, but implement it in three large clean-cutover commits:

1. **Mandatory permission pipeline.** Characterize current behavior, factor tool selection/metadata/dispatch through one pipeline, fix mid-turn mode coherence, split hook metadata from authority, move MCP/core final decisions out of dispatchers, make per-turn overrides effective everywhere, and delete the bypass branches.
2. **Mode-owned policies + typed tool effects.** Add `ToolEffect`/`ToolOrigin`, Lua `effects`, base-aware filesystem resolution, safe defaults for missing metadata, normalized mode policy, and remove legacy internal `decide`/`paths_for_workspace` authority.
3. **Compact bash analyzer replacing raw path token scanning.** Improve common shell classification, track simple cwd changes, reduce known false positives, keep unknown commands conservative and mode-owned, and delete the old whitespace-token shell permission inputs.

This gives the best end-state without overengineering. It closes real bypasses, preserves user-extensible modes/tools, improves the annoying bash edge cases, and keeps the system understandable. The implementation method matters as much as the target model: characterize first, cut over a whole seam, delete the old branch, and leave the codebase with fewer permission concepts than it has today.
