# Tools Reference

Built-in tools the agent can call. Each call is gated by the
[permissions](permissions.md) system before it runs. Tools are implemented in
Lua and are pluggable; see the [plugins guide](../guide/plugins.md) for how to
add or override tools.

## File I/O

These tools let the agent inspect and mutate source code. File I/O is the most
common operation in a coding workflow, so read tools are auto-approved by
default; writes and edits prompt for confirmation.

### `read_file`

Reads a file from the local filesystem. Supports text, provider-compatible
images (png, jpg/jpeg, gif, webp), SVG as text, and PDFs when the active
model/provider accepts that media. Jupyter notebooks (`.ipynb`) are rendered as
numbered cells with their type, source, and outputs. Use `offset` and `limit` to
read a window of a large text file.

| Parameter    | Description                               |
| ------------ | ----------------------------------------- |
| `file_path`  | Absolute path to the file (required)      |
| `offset`     | 1-based line number to start reading from |
| `limit`      | Number of lines to read (default: 2000)   |
| `timeout_ms` | Read timeout in milliseconds (default: 15000) |

### `write_file`

Writes a file to the local filesystem, overwriting any existing file at the
path. Refuses to overwrite a file that has not been read in this session, and
refuses Jupyter notebooks (use `edit_notebook`). The confirm dialog shows a
syntax-highlighted preview of the new content.

| Parameter   | Description                       |
| ----------- | --------------------------------- |
| `file_path` | Absolute path to write (required) |
| `content`   | Full file content (required)      |

### `edit_file`

Performs exact string replacement in a file. `old_string` must be unique in the
file unless `replace_all` is true. Refuses Jupyter notebooks (use
`edit_notebook`). The confirm dialog shows a scrollable inline diff.

| Parameter     | Description                                       |
| ------------- | ------------------------------------------------- |
| `file_path`   | Absolute path to the file (required)              |
| `old_string`  | Text to replace (required)                        |
| `new_string`  | Replacement text (required, must differ from old) |
| `replace_all` | Replace every occurrence (default: false)         |

### `edit_notebook`

Edits a Jupyter notebook (`.ipynb`) cell. Supports replacing, inserting, and
deleting cells. Identify cells by `cell_id` or `cell_number` (0-indexed).
Inserts show the new cell content in the confirm dialog; replace and delete show
a scrollable diff.

| Parameter       | Description                                                                              |
| --------------- | ---------------------------------------------------------------------------------------- |
| `notebook_path` | Absolute path to the notebook (required)                                                 |
| `cell_number`   | 0-indexed cell number (used when `cell_id` is omitted)                                   |
| `cell_id`       | Cell ID (takes precedence over `cell_number`; for insert, new cell goes after this cell) |
| `new_source`    | New source content (required for replace and insert)                                     |
| `cell_type`     | `code` or `markdown` (required for insert; defaults to current type for replace)         |
| `edit_mode`     | `replace` (default), `insert`, or `delete`                                               |

## Search

Search tools let the agent discover files and grep for patterns without needing
a shell escape. Results are sorted and filtered so the model gets only what it
asked for.

### `glob`

Fast file pattern matching. Supports `**` recursive globs. Results are sorted by
modification time, newest first.

| Parameter    | Description                                                 |
| ------------ | ----------------------------------------------------------- |
| `pattern`    | Glob pattern, e.g. `**/*.rs` (required)                     |
| `path`       | Directory to search (defaults to current working directory) |
| `timeout_ms` | Timeout in milliseconds (default: 30000)                    |

### `grep`

Regex search over file contents. Uses ripgrep when available and falls back to
`grep`. Supports file-type and glob filters, multiline mode, and three output
modes.

| Parameter        | Description                                                         |
| ---------------- | ------------------------------------------------------------------- |
| `pattern`        | Regular expression to search for (required)                         |
| `path`           | File or directory to search (defaults to current working directory) |
| `glob`           | Glob filter for files (e.g. `*.ts`, `*.{ts,tsx}`)                   |
| `type`           | File-type filter (e.g. `js`, `py`, `rust`, `go`)                    |
| `output_mode`    | `files_with_matches` (default), `content`, or `count`               |
| `include_ignored`| Search files and directories ignored by source-control rules        |
| `-i`             | Case-insensitive search                                             |
| `-n`             | Show line numbers (default: true; `content` mode only)              |
| `-A`             | Lines of context after each match (`content` mode only)             |
| `-B`             | Lines of context before each match (`content` mode only)            |
| `-C` / `context` | Lines of context before and after each match (`content` mode only)  |
| `multiline`      | `.` matches newlines and patterns may span lines                    |
| `head_limit`     | Limit output to first N lines (0 = unlimited)                       |
| `offset`         | Skip first N lines before applying `head_limit`                     |
| `timeout_ms`     | Timeout in milliseconds (default: 30000)                            |

## Execution

Run shell commands and background processes. Bash is the agent's escape hatch
into your build system, package manager, and test runner. Output is captured and
returned so the model can react to failures.

### `bash`

Executes a non-interactive bash command and returns its output. Each call starts
in the session's working directory; `cd` inside one command does not persist to
the next call.

| Parameter               | Description                                                                 |
| ----------------------- | --------------------------------------------------------------------------- |
| `command`               | Shell command to execute (required)                                         |
| `description`           | Short (max 10 words) description of what this command does                  |
| `timeout_ms`            | Timeout in milliseconds (default: 120000, max: 600000)                      |
| `background`            | Start the command in the background and return immediately (default: false) |
| `background_on_timeout` | Move a foreground command to the background if it times out (default: true) |

**Behavior:**

- Interactive commands (editors, pagers, interactive rebases) are blocked
- Shell backgrounding (`&`) in the command string is rejected
- Stdout and stderr are multiplexed as bounded line output
- A non-zero exit code is flagged as an error
- The call can be cancelled from the UI while it is still foreground
- Each job runs in its own systemd scope on supported Linux systems, with a
  process-group fallback on Unix and a Job Object on Windows
- `background=true` starts the command in the background immediately and returns
  an opaque job ID such as `proc_123`; the OS PID is tracked separately
- With `background_on_timeout=true` (default), a foreground command that reaches
  `timeout_ms` keeps running as the same supervised background job
- Linux cgroup OOM termination is reported separately from an ordinary signal
- At most 64 shell jobs run concurrently; excess spawns fail instead of allowing
  unbounded process and output growth
- Completed output remains available in a bounded recent-job cache; older
  snapshots are evicted by count or aggregate memory usage

Use `read_process_output` and `stop_process` with the returned job ID. `/ps`
shows the same supervised jobs with bounded output, PID, and duration.

### `read_process_output`

Reads the captured output snapshot from a background bash process without
draining it or waiting. Running processes return only buffered stdout/stderr,
which may be empty; exited processes append a typed final status such as
`process exited with code 1` or `process was terminated by a signal`. Completed
snapshots can expire from the bounded recent-job cache.

| Parameter | Description                                                |
| --------- | ---------------------------------------------------------- |
| `id`      | Opaque background job ID, e.g. `proc_123` (required)       |

### `stop_process`

Stops a running background bash process and returns its buffered output.

| Parameter | Description                                                |
| --------- | ---------------------------------------------------------- |
| `id`      | Opaque background job ID, e.g. `proc_123` (required)       |

## Workspace and runtime

These tools change smelt itself, not only a subprocess. The directory-switching
tools are interactive-only and omitted in headless mode; `smelt_reload` remains
available because it schedules config work at a safe turn boundary.

### `switch_cwd`

Switches smelt's real process working directory. Future relative tool calls,
project instructions, skills, session metadata, and workspace permission checks
use the new checkout. This differs from `cd` inside `bash`, which affects only
that one shell process.

| Parameter | Description |
| --------- | ----------- |
| `path` | Directory to enter, absolute, relative to the current cwd, or starting with `~` (required) |

### `enter_worktree`

Creates or opens a smelt-managed git worktree and then performs the equivalent
of `switch_cwd`. Names are normalized, capped at 64 characters, and deduplicated.
The default base is `main`, then `master`, then `HEAD`; the configured
`smelt.settings.worktree_root` controls where worktrees are stored.

| Parameter | Description |
| --------- | ----------- |
| `name` | Semantic worktree name (required) |
| `base` | Optional git base ref |

Managed worktree agents do not push, open pull requests, merge, or remove the
worktree unless you explicitly ask. See
[Managed worktrees](../guide/usage.md#managed-worktrees).

### `smelt_reload`

Schedules a reload of smelt's Lua config at the end of the current agent turn.
It takes no parameters. Use it after editing files under `~/.config/smelt/` or
`.smelt/` so the in-flight tool call is not disrupted. Multiple calls in one
turn coalesce into one reload.

## Web

Fetch live documentation, API specs, or reference material from the internet.
Results are cached for 15 minutes so repeated queries don't hammer the same
endpoint.

### `web_fetch`

Fetches a URL, converts the page to markdown, and asks an isolated LLM call to
extract only what the `prompt` asks for. In the default `auto` rendering mode,
challenge responses and sparse SPA shells are retried through an optional renderer
executable. Responses are cached for 15 minutes per URL, format, and rendering mode.

| Parameter | Description                                                      |
| --------- | ---------------------------------------------------------------- |
| `url`     | URL to fetch, must start with `http://` or `https://` (required) |
| `prompt`  | What to extract or answer from the page content (required)       |
| `format`  | `markdown` (default), `text`, or `html`                          |
| `timeout` | Timeout in seconds (default: 30, max: 120)                       |

**Limits:**

- Direct HTTP response bodies are capped at 5 MB while streaming
- Transient direct HTTP failures are retried within the requested total timeout
- Final output is capped at 2,000 lines or 50 KB (truncation noted)
- Cross-domain redirects are refused; redirects that only add or remove `www.` are accepted
- Browser rendering requires `web_fetch_renderer_command`. The executable receives `{url, timeout_ms, max_response_bytes}` JSON on stdin and must return `{status, final_url, html, truncated}` JSON on stdout
- Direct HTTP and renderer fallback share one total timeout, and renderer redirects receive the same cross-domain validation

### `web_search`

Searches the web using DuckDuckGo or the official Brave Search API. Returns a
numbered list of results with title, URL, and description. Results are cached for
15 minutes per query. The default `auto` provider uses Brave when the environment
variable named by `brave_search_api_key_env` is set, otherwise it uses keyless
DuckDuckGo. If Brave fails transiently in `auto` mode, DuckDuckGo is attempted as
a fallback.

| Parameter | Description             |
| --------- | ----------------------- |
| `query`   | Search query (required) |

## Interaction

Ask the user structured questions when the agent needs a decision it can't make
on its own, for example, choosing between two implementation approaches or
confirming an ambiguous requirement.

### `ask_user_question`

Asks you 1-4 questions with 2-4 selectable options each. A free-text "Other"
input is offered alongside the options for each question. Available in
interactive mode only; the agent's turn is blocked until you reply.

| Parameter   | Description                                                                                                                                                         |
| ----------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `questions` | List of 1-4 question objects (required). Each has `question`, `header` (short label, max 12 chars), `options` (2-4 `{label, description}` items), and `multiSelect` |

`multiSelect` is accepted by the schema, but the current dialog records one
option or the free-text Other value per question.

## Knowledge

Load domain-specific instructions on demand. Skills keep the system prompt lean
by injecting only the knowledge relevant to the current task.

### `load_skill`

Loads a skill by name to give the agent specialized instructions and knowledge
for a task. See [Skills](configuration.md#skills) in the configuration reference
for how to create and organize skills.

| Parameter | Description                          |
| --------- | ------------------------------------ |
| `name`    | Name of the skill to load (required) |

## Goal coordination

The autoloaded goal plugin adds tools for persistent, user-requested objectives.
Goals survive across turns in the current session and can drive idle
auto-continue. The agent must not create one merely because a task is long.

### `get_goal`

Returns the current goal's objective, lifecycle state, progress, auto-continue
setting, id, and timestamps. It takes no parameters.

### `create_goal`

Creates a goal when you explicitly ask to start or set one. It fails while an
unfinished goal exists.

| Parameter | Description |
| --------- | ----------- |
| `objective` | Objective to pursue (required) |
| `summary` | Optional short label for the goal bar |
| `auto_continue` | Continue automatically while idle (default: true) |

### `update_goal_progress`

Records durable phase or milestone progress for an active goal. Its required
`progress` object accepts a short `label` and optional numeric `current`, `total`,
or `percent` values. Numeric progress should only be used when grounded in an
explicit plan or measurable work.

### `update_goal`

Marks the current goal `done` with completion evidence, or `blocked` with the
exact external blocker. It cannot pause, resume, clear, or rewrite a goal.

| Parameter | Description |
| --------- | ----------- |
| `state` | `done` or `blocked` (required) |
| `reason` | Completion evidence or blocker |

## Mode-Specific

Tools that only appear in certain modes.

### `present_plan`

Plan mode only. Built in and autoloaded by default. Called by the agent to
present a written plan after discussion. The review dialog renders the proposed
plan as markdown and lets you choose `save draft`, `approve` (switch to Normal),
or `approve and apply` (switch to Apply). The transcript only shows the plan
body after one of those options is accepted, using the same line-numbered file
view as `write_file`. Existing drafts should be revised with file tools against
the full saved `plan.md` path, then presented again with `plan_path`.

| Parameter   | Description                                                                  |
| ----------- | ---------------------------------------------------------------------------- |
| `title`     | Human-readable plan title; required for a new artifact                       |
| `slug`      | Kebab-case filename slug; required for a new artifact                        |
| `plan`      | Markdown plan body; required for a new artifact                              |
| `plan_path` | Full path to an existing saved `plan.md` to present after editing/refinement |
