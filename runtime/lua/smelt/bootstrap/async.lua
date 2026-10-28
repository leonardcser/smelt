-- Yielding wrappers over host services: session, notebook, fs, image,
-- process, git, http, auth and grep. Each yields the calling task until the
-- host resolves the request.

local internal = __smelt_internal
local require_yieldable = internal.require_yieldable

local function external_or_err(start)
  local result = smelt.task.external(start)
  if result.err ~= nil then return nil, result.err end
  return result, nil
end

if smelt.session then
  --- Read a bounded page of rewindable user turns off the UI thread, newest
  --- page first and each page in chronological order. Uses the transcript
  --- index without decoding assistant output. Returns `{ turns, next_before? }`;
  --- pass `next_before` to read older turns. Must run in a yieldable task.
  ---@tier ui_host
  ---@type fun(before?: integer): table
  function smelt.session.turns_page(before)
    local request = internal.session.__prepare_turns_page(before)
    if type(request) == "table" then return request end
    local result = smelt.task.external(function(id) internal.session.__start_turns_page(id, request) end)
    if result.err then error(result.err, 2) end
    return result.page
  end

  --- Create or open a managed git worktree off the UI thread, then request a
  --- coherent cwd transition. Must run in `smelt.spawn`, a command, or a tool.
  --- `opts.name` is required and is normalized to a safe folder/branch name;
  --- `opts.base` defaults to main, master, or HEAD. New worktrees use
  --- `smelt.settings.worktree_root`, inside the repository for relative roots
  --- or in a per-repository bucket for absolute roots. Returns
  --- `{ name, branch, path, base, created, pending }`. The cwd, Lua project
  --- config, prompt inputs, permissions, and watcher roots commit together.
  --- Sequential tool results wait for the commit; concurrent cwd-changing
  --- model tools are rejected. Other callers commit at an idle safe point.
  ---@tier ui_host
  ---@type fun(opts: { name: string, base?: string }): table
  function smelt.session.enter_worktree(opts)
    local name = tostring(opts and opts.name or ""):match("^%s*(.-)%s*$")
    if name == "" then error("name is required", 2) end
    local result = smelt.task.external(function(id)
      internal.session.__start_enter_worktree(id, name, opts and opts.base)
    end)
    if result.err then error(result.err, 2) end
    result.pending = smelt.session.switch_cwd(result.path).pending
    return result
  end

  --- List managed worktrees off the UI thread. Must run in `smelt.spawn`, a
  --- command, or a tool. Returns sorted `{ name, branch, path, base, current }` rows.
  ---@tier ui_host
  ---@type fun(): table
  function smelt.session.worktrees()
    local result = smelt.task.external(function(id) internal.session.__start_worktrees(id) end)
    if result.err then error(result.err, 2) end
    return result.worktrees
  end
end

-- Read `path` off the main thread. Must be called from inside
-- `smelt.spawn(fn)` or a `tool.execute` (anything that runs on the Lua
-- task runtime). Returns `(content, nil)` on success or `(nil, err)` on
-- failure - same convention as `smelt.fs.read`. Third return value is the
-- file mtime in milliseconds when available.
---@type fun(path: string): string?, string?, integer?
function smelt.fs.read_async(path)
  local result = smelt.task.external(function(id) internal.fs.__start_read(id, path) end)
  if result.content ~= nil then return result.content, nil, result.mtime_ms end
  return nil, result.err, nil
end

-- Return `{ is_file, len, mtime_ms }` for `path` off the main thread, or `(nil, err)`.
---@type fun(path: string): table?, string?
function smelt.fs.file_info_async(path)
  return external_or_err(function(id) internal.fs.__start_file_info(id, path) end)
end

-- Write `contents` to `path` off the main thread. Same yielding rules as
-- `smelt.fs.read_async`. Returns `(true, nil, mtime_ms)` on success or
-- `(false, err, nil)` on failure - mirrors `smelt.fs.write`.
---@type fun(path: string, contents: string): boolean, string?, integer?
function smelt.fs.write_async(path, contents)
  local result = smelt.task.external(function(id) internal.fs.__start_write(id, path, contents) end)
  if result.ok then return true, nil, result.mtime_ms end
  return false, result.err, nil
end

-- Create `path` and parents off the main thread. Same return shape as
-- `smelt.fs.mkdir_all`.
---@type fun(path: string): boolean, string?
function smelt.fs.mkdir_all_async(path)
  local result = smelt.task.external(function(id) internal.fs.__start_mkdir_all(id, path) end)
  if result.ok then return true, nil end
  return false, result.err
end

-- Find paths matching `pattern` under `path` off the main thread. `opts`
-- accepts `max`, `max_scanned`, and `timeout_ms`. Returns a table with
-- `{ paths, scanned, truncated, timed_out }` or `(nil, err)`.
---@type fun(pattern: string, path: string?, opts: table?): table?, string?
function smelt.fs.glob_async(pattern, path, opts)
  return external_or_err(function(id) internal.fs.__start_glob(id, pattern, path or "", opts or {}) end)
end

-- Read and base64-encode a file off the main thread. Without `mime`, the MIME
-- type is inferred as for `smelt.image.read_as_data_url`.
---@type fun(path: string, mime?: string): string?, string?
function smelt.image.read_as_data_url_async(path, mime)
  local result = smelt.task.external(function(id)
    internal.image.__start_read_as_data_url(id, path, mime)
  end)
  if result.url ~= nil then return result.url, nil end
  return nil, result.err
end

if smelt.notebook then
  -- Render a notebook off the main thread. Same return shape as
  -- `smelt.notebook.read`, plus raw notebook JSON and mtime on success.
  ---@type fun(path: string, offset: integer, limit: integer): string?, string?, string?, integer?
  function smelt.notebook.read_async(path, offset, limit)
    local result = smelt.task.external(function(id)
      internal.notebook.__start_read(id, path, offset, limit)
    end)
    if result.content ~= nil then return result.content, nil, result.raw, result.mtime_ms end
    return nil, result.err, nil, nil
  end

  -- Apply a notebook edit off the main thread. Same return shape as
  -- `smelt.notebook.apply_edit`.
  -- `expected_mtime_ms` rejects writes if the notebook changed after preparation.
  ---@type fun(args: table, expected_mtime_ms?: integer): table?, string?
  function smelt.notebook.apply_edit_async(args, expected_mtime_ms)
    local result = smelt.task.external(function(id)
      internal.notebook.__start_apply_edit(id, args or {}, expected_mtime_ms)
    end)
    if result.err ~= nil then return nil, result.err end
    return { message = result.message, metadata = result.metadata }, nil
  end
end

-- Spawn `command` as a contained background shell job. Yields while smelt
-- selects and verifies the containment backend, then returns an opaque job ID.
-- Must be called from inside `smelt.spawn(fn)` or a `tool.execute`.
---@type fun(command: string): string
function smelt.process.spawn_bg(command)
  local result = smelt.task.external(function(id)
    internal.process.__start_spawn_bg(id, command)
  end)
  if result.err ~= nil then error(result.err, 2) end
  return result.id
end

-- Run `cmd` with `args` off the main thread. Yields the calling
-- coroutine until the child exits; must be called from inside
-- `smelt.spawn(fn)` or a `tool.execute`. `opts` accepts `cwd`, `env`,
-- `timeout_secs`, `stdin`, and `max_output_bytes` (default 100 KB, maximum
-- 32 MB). Returns `({ stdout, stderr, exit_code, timed_out }, nil)` on success or
-- `(nil, err)` on spawn failure. If the calling coroutine is cancelled
-- (e.g. by `smelt.task.timeout` or by `:remove()` on the parent spawn),
-- the child process is killed (SIGTERM to its process group) and
-- `smelt.task.external` raises `cancelled` - same shape as every other
-- yielding API.
---@type fun(cmd: string, args: string[]?, opts: table?): { stdout: string, stderr: string, exit_code: integer, timed_out: boolean }?, string?
function smelt.process.run(cmd, args, opts)
  return external_or_err(function(id) internal.process.__start_run(id, cmd, args, opts) end)
end

--- Load separate unstaged (index to worktree) and staged (HEAD to index) changes
--- without passing patch bodies through Lua. Yields inside `smelt.spawn`; cancellation terminates Git. `cwd`
--- defaults to the runtime workspace and `context` to 3. The snapshot is stable
--- until explicitly refreshed; a clean repository returns an empty diff.
---@type fun(opts: { cwd: string?, context: integer? }?): { root: string, branch: string, diff: smelt.diff.Diff }?, string?
function smelt.git.diff(opts)
  require_yieldable("smelt.git.diff")
  local id = smelt.task.alloc()
  local job = internal.git.__start_diff(id, opts)
  smelt.task.wait(id)
  return job:take()
end

--- Stage, unstage, or toggle one file from a Git snapshot. File indices are
--- one-based. Toggle stages an unstaged entry and unstages a staged entry.
--- Updates the real index, never worktree contents, then acquires and indexes
--- both sections off the UI thread. Yields inside `smelt.spawn`. A successful
--- index operation returns index_updated=true even if refresh fails: diff is then
--- nil and refresh_error explains why. Retain the old snapshot as stale and
--- refresh before another mutation. nil, error means the mutation did not report
--- success; cancellation/timeouts may require refreshing to verify repository state.
---@type fun(diff: smelt.diff.Diff, file: integer, action: "stage"|"unstage"|"toggle"): { root: string, index_updated: boolean, branch: string?, diff: smelt.diff.Diff?, refresh_error: string? }?, string?
function smelt.git.index(diff, file, action)
  require_yieldable("smelt.git.index")
  local id = smelt.task.alloc()
  local job = internal.git.__start_index(id, diff, file, action)
  smelt.task.wait(id)
  return job:take()
end

-- Stop the supervised shell job `id` and return its bounded output. Yields until
-- containment termination and removes the completed job. Returns `({ text },
-- nil)` on success or `(nil, err)` when the job does not exist or cannot stop.
---@type fun(id: string): { text: string }?, string?
function smelt.process.stop(id)
  return external_or_err(function(task_id) internal.process.__start_stop(task_id, id) end)
end

-- Perform an HTTP GET against `url`. Yields the calling coroutine until the
-- response lands; the runtime stays responsive throughout. `opts` accepts
-- `headers`, `timeout_secs`, `max_redirects`, `max_response_bytes`, and
-- `max_retries`. The total timeout includes retries. `truncated` reports that
-- the body limit was reached. Non-UTF-8 response bytes are base64 encoded in
-- `body` and reported by `body_encoding = "base64"`. Cancellation of the parent
-- task drops the in-flight request and raises `cancelled` from this call.
---@type fun(url: string, opts: table?): { status: integer, final_url: string, body: string, body_encoding: string?, headers: table, truncated: boolean }?, string?
function smelt.http.get(url, opts)
  return external_or_err(function(id) internal.http.__start_get(id, url, opts) end)
end

-- Perform an HTTP POST against `url` with `body` bytes. Yields the calling
-- coroutine until the response lands. Same return shape as `smelt.http.get`.
---@type fun(url: string, body: string?, opts: table?): { status: integer, final_url: string, body: string, body_encoding: string?, headers: table, truncated: boolean }?, string?
function smelt.http.post(url, body, opts)
  return external_or_err(function(id) internal.http.__start_post(id, url, body, opts) end)
end

--- Run an authenticated request against a provider-owned endpoint using
--- smelt-managed credentials. Credentials stay in Rust; Lua receives only
--- the HTTP status and body. `opts.path` must be an absolute path without a
--- URL scheme; `opts.method` defaults to `GET`.
---@type fun(provider: string, opts: { path: string, method: string?, body: string? }): { status: integer, body: string }?, string?
function smelt.auth.request(provider, opts)
  return external_or_err(function(id) internal.auth.__start_request(id, provider, opts or {}) end)
end

--- Fetch parsed managed-provider usage. Credentials stay in Rust; Lua receives
--- provider-neutral `{ summary, limits }` rows with `{ label, used, limit,
--- resetHint? }`. Supported for `kimi-code` and `claude-code`, which asks the
--- installed Claude Code for its account's plan limits.
---@type fun(provider: string): { summary: table?, limits: table[] }?, string?
function smelt.auth.managed_usage(provider)
  return external_or_err(function(id) internal.auth.__start_managed_usage(id, provider) end)
end

-- Run ripgrep with `pattern` over `path` off the main thread. Yields the
-- calling coroutine until the child exits; must be called from inside
-- `smelt.spawn(fn)` or a `tool.execute`. `opts` accepts the same fields
-- as the underlying `smelt.grep` namespace (`mode`,
-- `case_insensitive`, `multiline`, `line_numbers`, `before_context`,
-- `after_context`, `context`, `glob`, `type`, `include_ignored`,
-- `timeout_secs`). Returns
-- `({ stdout, stderr, exit_code, timed_out }, nil)` on success or
-- `(nil, err)` on spawn failure. Cancellation kills the child (SIGKILL)
-- and `smelt.task.external` raises `cancelled`. Exit code 1 (no match)
-- is not an error - inspect `exit_code` on the result.
---@type fun(pattern: string, path: string, opts: table?): { stdout: string, stderr: string, exit_code: integer, timed_out: boolean }?, string?
function smelt.grep.run(pattern, path, opts)
  return external_or_err(function(id) internal.grep.__start_run(id, pattern, path, opts) end)
end
