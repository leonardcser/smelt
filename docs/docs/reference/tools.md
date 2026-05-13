# Tools Reference

Built-in tools the agent can call. Each call is gated by the
[permissions](permissions.md) system before it runs. Tools are implemented in
Lua and are pluggable — see the [plugins guide](../guide/plugins.md) for how to
add or override tools.

## File I/O

### `read_file`

Reads a file from the local filesystem. Supports text files and image files
(png, jpg, gif, webp, bmp, tiff, svg). Jupyter notebooks (`.ipynb`) are
rendered as numbered cells with their type, source, and outputs. Use `offset`
and `limit` to read a window of a large file.

| Parameter   | Description                                                  |
| ----------- | ------------------------------------------------------------ |
| `file_path` | Absolute path to the file (required)                         |
| `offset`    | 1-based line number to start reading from                    |
| `limit`     | Number of lines to read (default: 2000)                      |

### `write_file`

Writes a file to the local filesystem, overwriting any existing file at the
path. Refuses to overwrite a file that has not been read in this session, and
refuses Jupyter notebooks (use `edit_notebook`). The confirm dialog shows a
syntax-highlighted preview of the new content.

| Parameter   | Description                                          |
| ----------- | ---------------------------------------------------- |
| `file_path` | Absolute path to write (required)                    |
| `content`   | Full file content (required)                         |

### `edit_file`

Performs exact string replacement in a file. `old_string` must be unique in
the file unless `replace_all` is true. Refuses Jupyter notebooks (use
`edit_notebook`). The confirm dialog shows a scrollable inline diff.

| Parameter     | Description                                            |
| ------------- | ------------------------------------------------------ |
| `file_path`   | Absolute path to the file (required)                   |
| `old_string`  | Text to replace (required)                             |
| `new_string`  | Replacement text (required, must differ from old)      |
| `replace_all` | Replace every occurrence (default: false)              |

### `edit_notebook`

Edits a Jupyter notebook (`.ipynb`) cell. Supports replacing, inserting, and
deleting cells. Identify cells by `cell_id` or `cell_number` (0-indexed).
Inserts show the new cell content in the confirm dialog; replace and delete
show a scrollable diff.

| Parameter       | Description                                                                              |
| --------------- | ---------------------------------------------------------------------------------------- |
| `notebook_path` | Absolute path to the notebook (required)                                                 |
| `cell_number`   | 0-indexed cell number (used when `cell_id` is omitted)                                   |
| `cell_id`       | Cell ID (takes precedence over `cell_number`; for insert, new cell goes after this cell) |
| `new_source`    | New source content (required for replace and insert)                                     |
| `cell_type`     | `code` or `markdown` (required for insert; defaults to current type for replace)         |
| `edit_mode`     | `replace` (default), `insert`, or `delete`                                               |

## Search

### `glob`

Fast file pattern matching. Supports `**` recursive globs. Results are sorted
by modification time, newest first.

| Parameter | Description                                                |
| --------- | ---------------------------------------------------------- |
| `pattern` | Glob pattern, e.g. `**/*.rs` (required)                    |
| `path`    | Directory to search (defaults to current working directory) |

### `grep`

Regex search over file contents. Uses ripgrep when available and falls back to
`grep`. Supports file-type and glob filters, multiline mode, and three output
modes.

| Parameter      | Description                                                                                  |
| -------------- | -------------------------------------------------------------------------------------------- |
| `pattern`      | Regular expression to search for (required)                                                  |
| `path`         | File or directory to search (defaults to current working directory)                          |
| `glob`         | Glob filter for files (e.g. `*.ts`, `*.{ts,tsx}`)                                            |
| `type`         | File-type filter (e.g. `js`, `py`, `rust`, `go`)                                             |
| `output_mode`  | `content` (default), `files_with_matches`, or `count`                                        |
| `-i`           | Case-insensitive search                                                                      |
| `-n`           | Show line numbers (default: true; `content` mode only)                                       |
| `-A`           | Lines of context after each match (`content` mode only)                                      |
| `-B`           | Lines of context before each match (`content` mode only)                                     |
| `-C` / `context` | Lines of context before and after each match (`content` mode only)                         |
| `multiline`    | `.` matches newlines and patterns may span lines                                             |
| `head_limit`   | Limit output to first N lines (0 = unlimited)                                                |
| `offset`       | Skip first N lines before applying `head_limit`                                              |
| `timeout_ms`   | Timeout in milliseconds (default: 30000)                                                     |

## Execution

### `bash`

Executes a non-interactive bash command and returns its output. The working
directory persists between calls.

| Parameter           | Description                                                                                   |
| ------------------- | --------------------------------------------------------------------------------------------- |
| `command`           | Shell command to execute (required)                                                           |
| `description`      | Short (max 10 words) description of what this command does                                    |
| `timeout_ms`        | Timeout in milliseconds (default: 120000, max: 600000)                                        |
| `run_in_background` | Run asynchronously and return a process ID                                                    |

**Behavior:**

- Interactive commands (editors, pagers, interactive rebases) are blocked
- Shell backgrounding (`&`) in the command string is rejected — use
  `run_in_background` instead
- Output is line-buffered (stdout and stderr multiplexed)
- A non-zero exit code is flagged as an error
- The call can be cancelled from the UI
- When `run_in_background` is true, the tool returns immediately with a
  process id. Use `read_process_output` and `stop_process` to manage it, and
  `/ps` to list all background processes.

### `read_process_output`

Reads output from a background bash process started by `bash` with
`run_in_background=true`. Blocks until the process finishes by default; set
`block=false` for a non-blocking check.

| Parameter     | Description                                                            |
| ------------- | ---------------------------------------------------------------------- |
| `id`          | Background process ID, e.g. `proc_1` (required)                        |
| `block`       | Wait for the process to finish (default: true)                         |
| `timeout_ms`  | Max wait time in ms when blocking (default: 30000)                     |

### `stop_process`

Stops a running background bash process and returns its accumulated output.

| Parameter | Description                                     |
| --------- | ----------------------------------------------- |
| `id`      | Background process ID, e.g. `proc_1` (required) |

## Web

### `web_fetch`

Fetches a URL, converts the page to markdown, and asks an isolated LLM call
to extract only what the `prompt` asks for. Responses are cached for 15
minutes per URL and format.

| Parameter | Description                                                |
| --------- | ---------------------------------------------------------- |
| `url`     | URL to fetch, must start with `http://` or `https://` (required) |
| `prompt`  | What to extract or answer from the page content (required) |
| `format`  | `markdown` (default), `text`, or `html`                    |
| `timeout` | Timeout in seconds (default: 30, max: 120)                 |

**Limits:**

- Response body capped at 5 MB
- Final output capped at 2,000 lines or 50 KB (truncation noted)
- Cross-domain redirects are refused; re-run with the final URL if intended

### `web_search`

Searches the web via DuckDuckGo. Returns a numbered list of results with
title, URL, and description. Results are cached for 15 minutes per query.

| Parameter | Description                |
| --------- | -------------------------- |
| `query`   | Search query (required)    |

## Interaction

### `ask_user_question`

Asks you 1-4 questions with 2-4 selectable options each. A free-text "Other"
input is offered alongside the options for each question. Available in
interactive mode only; the agent's turn is blocked until you reply.

| Parameter   | Description                                                  |
| ----------- | ------------------------------------------------------------ |
| `questions` | List of 1-4 question objects (required). Each has `question`, `header` (short label, max 12 chars), `options` (2-4 `{label, description}` items), and `multiSelect` |

## Knowledge

### `load_skill`

Loads a skill by name to give the agent specialized instructions and
knowledge for a task. See [Skills](configuration.md#skills) in the
configuration reference for how to create and organize skills.

| Parameter | Description                          |
| --------- | ------------------------------------ |
| `name`    | Name of the skill to load (required) |

## Mode-Specific

### `exit_plan_mode`

Plan mode only. Called by the agent when its plan is finalized and ready for
your review. The confirm dialog renders the plan as markdown; approving
either keeps the current mode or switches to Apply.

| Parameter      | Description                                                            |
| -------------- | ---------------------------------------------------------------------- |
| `plan_summary` | Concise summary of the implementation plan for you to approve (required) |
