# Tools Reference

## File I/O

### `read_file`

Reads a file and returns its contents with line numbers. Supports offset/limit
for reading specific ranges of large files.

### `write_file`

Creates or overwrites a file. In the confirm dialog, a syntax-highlighted
preview of the new content is shown.

### `edit_file`

Applies diff-based edits to an existing file. The confirm dialog shows a
scrollable inline diff (old vs. new).

### `notebook_edit`

Reads and edits Jupyter notebooks (`.ipynb`). Cells are displayed as numbered
blocks with their type (code/markdown), source content, and outputs. Supports
offset/limit slicing.

## Search

### `glob`

Finds files matching glob patterns (e.g., `**/*.rs`, `src/**/*.ts`). Returns
matching paths.

### `grep`

Searches file contents with regex. Returns matching lines with context.

## Execution

### `bash`

Runs a shell command synchronously with streaming output. Each line of
stdout/stderr is streamed to the conversation in real-time.

**Behavior details:**

- Default timeout: 120 seconds (max: 600 seconds)
- Interactive commands are rejected (vim, nano, less, etc.)
- Shell backgrounding (`&`) is rejected
- Output is line-buffered (stdout and stderr multiplexed)
- Non-zero exit codes are flagged as errors
- Cancellable via the UI

### `bash_background`

Runs a command asynchronously. Returns immediately with a process ID. Use
`read_process_output` and `stop_process` to manage it. Monitor all background
processes with `/ps`.

### `read_process_output`

Reads buffered output from a background process started with
`bash_background`.

### `stop_process`

Kills a running background process.

## Web

### `web_fetch`

Fetches a URL and returns its content.

**Processing pipeline:**

- **HTML pages** → converted to markdown (title extracted as h1, links listed)
- **Images** → encoded as base64 data URLs
- **Other content** → returned as-is

**Limits:**

- Response body capped at 5 MB
- Output capped at 2,000 lines or 50 KB (truncation noted)
- Timeout: 30 seconds (max: 120 seconds)
- Results are cached by URL

### `web_search`

Searches the web via DuckDuckGo. Returns up to 20 results with title, URL, and
description. Results are cached by query.

## Interaction

### `ask_user_question`

Asks you a question with selectable options. Supports single-select and
multi-select modes. Available in interactive mode only.

## Mode-Specific

### `exit_plan_mode`

Plan mode only. Called by the agent when its plan is ready for your review.
The confirm dialog renders the plan as markdown. Approving switches to Apply
mode.
