-- /diff and `smelt diff`: a Git status, branch review and history viewer.
-- Each view owns its windows and state, so switching views only swaps the
-- layout. Native documents index patch bytes and materialize visible rows.

local M = {}
local layout = smelt.ui.layout
local active

local VIEWS = { "status", "branch", "history" }
local TITLES = { status = "Status", branch = "Branch", history = "History" }
-- Below this width the two panes cannot both fit, so one shows at a time.
local SINGLE_PANE_WIDTH = 72
local MESSAGE_MS = 5000
-- Git must never wait on a terminal or an editor it does not own.
local GIT_ENV = { GIT_TERMINAL_PROMPT = "0", GIT_EDITOR = "true" }

smelt.keymap.scope("diff", {
  desc = "/diff viewer",
  actions = {
    status       = "Show the working tree status",
    branch       = "Show the branch against its base",
    history      = "Show commit history",
    switch_pane  = "Switch between the file list and the diff",
    close        = "Close the viewer",
    refresh      = "Reload the view",
    stage        = "Stage the selected files, or the selected hunks of the patch",
    unstage      = "Unstage the selected files, or the selected hunks of the patch",
    toggle_stage = "Stage or unstage the selected files or hunks",
    discard      = "Discard changes to the selected files or hunks",
    commit       = "Commit the staged changes",
    amend        = "Amend the last commit",
    push         = "Push the branch",
    pull         = "Pull with rebase",
    fetch        = "Fetch and prune",
    copy         = "Copy the paths or commit ids of the selected rows",
    commits      = "Go back to the commit list",
    next_view    = "Show the next view",
    prev_view    = "Show the previous view",
    revision     = "Show a commit or revision",
    base         = "Change the comparison base",
    next_file    = "Show the next file",
    prev_file    = "Show the previous file",
    next_hunk    = "Jump to the next hunk",
    prev_hunk    = "Jump to the previous hunk",
  },
  -- Letter keys are Normal-mode defaults, so they stay out of the text input.
  keys = {
    { "",  "<M-1>",   "status"       },
    { "",  "<M-2>",   "branch"       },
    { "",  "<M-3>",   "history"      },
    { "",  "<M-w>",   "switch_pane"  },
    { "",  "<C-c>",   "close"        },
    { "",  "<Esc>",   "close"        },
    { "n", "q",       "close"        },
    { "n", "r",       "refresh"      },
    { "n", "s",       "stage"        },
    { "n", "u",       "unstage"      },
    { "n", "-",       "toggle_stage" },
    { "n", "X",       "discard"      },
    { "v", "s",       "stage"        },
    { "v", "u",       "unstage"      },
    { "v", "-",       "toggle_stage" },
    { "v", "X",       "discard"      },
    { "n", "c",       "commit"       },
    { "n", "C",       "amend"        },
    { "n", "p",       "push"         },
    { "n", "P",       "pull"         },
    { "n", "F",       "fetch"        },
    { "n", "Y",       "copy"         },
    { "n", "<BS>",    "commits"      },
    { "n", "<Tab>",   "next_view"    },
    { "n", "<S-Tab>", "prev_view"    },
    { "n", "<M-r>",   "revision"     },
    { "n", "<M-b>",   "base"         },
    { "n", "<C-j>",   "next_file"    },
    { "n", "<C-k>",   "prev_file"    },
    { "n", "]",       "next_file"    },
    { "n", "[",       "prev_file"    },
    { "n", "}",       "next_hunk"    },
    { "n", "{",       "prev_hunk"    },
  },
})

smelt.keymap.scope("diff.files", {
  desc = "/diff file list",
  actions = {
    open     = "Open the selected row, or toggle the folder",
    collapse = "Collapse the folder, or select its parent",
    expand   = "Expand the folder, or open the file",
  },
  keys = {
    { "", "<CR>",    "open"     },
    { "", "<Space>", "open"     },
    { "", "h",       "collapse" },
    { "", "<Left>",  "collapse" },
    { "", "l",       "expand"   },
    { "", "<Right>", "expand"   },
  },
})

smelt.keymap.scope("diff.preview", {
  desc = "/diff patch view",
  actions = {
    fold      = "Fold or unfold the hunk or file under the cursor",
    pan_left  = "Scroll the patch left",
    pan_right = "Scroll the patch right",
  },
  keys = {
    { "n", "<CR>",      "fold"      },
    { "n", "H",         "pan_left"  },
    { "n", "L",         "pan_right" },
    { "",  "<S-Left>",  "pan_left"  },
    { "",  "<S-Right>", "pan_right" },
  },
})

local function display(text)
  return ((text or ""):gsub("%c", function(c) return string.format("\\x%02x", c:byte()) end))
end

-- The line of Git output worth showing: its first fatal or error line, or
-- else its first line.
local function summary(text)
  text = text or ""
  for line in text:gmatch("[^\n]+") do
    local reason = line:match("^%s*fatal:%s*(.+)") or line:match("^%s*error:%s*(.+)")
    if reason then return display(reason) end
  end
  local first = text:match("^%s*([^\n]*)") or ""
  return display(first ~= "" and first or "git failed")
end

function M.open(opts)
  opts = opts or {}
  if active then active.close() end
  local cwd = smelt.session.cwd()
  local surface = require("smelt.surface").new({ standalone = opts.standalone, width = 0.94, height = 0.85 })
  local standalone = surface.standalone
  local closed = false
  local current = opts.view or "status"
  local base = opts.base
  local views = {}
  local overlay, message, pending, query
  -- The Git operation in progress: `{ label, job }`.
  local change
  local tab_ranges = {}
  -- Line totals by commit ID, shared by the lists. They never change.
  local stats = {}
  local single_pane = false
  local split = layout.split("horizontal", { key = "diff", size = 42, min_first = 26, min_second = 32 })
  local render, render_header, flash, ask, dismiss, focus_pane

  local function window(name, o)
    local buf = smelt.buf.new()
    o.name = "smelt.diff." .. name
    o.wrap = false
    return smelt.win.new(buf, o), buf
  end

  for _, name in ipairs(VIEWS) do
    local v = { name = name, focus = "files" }
    v.sidebar, v.sidebar_buf = window(name .. ".files", {
      surface = "readonly_text", vim_enabled = true, hide_cursor = true, cursor_line = true,
      scrollbar = true, pad_left = 1, pad_right = 1,
    })
    v.preview, v.preview_buf = window(name .. ".preview", {
      surface = "readonly_text", vim_enabled = true, scrollbar = true, pad_left = 1, pad_right = 1,
    })
    if name ~= "status" then
      v.info, v.info_buf = window(name .. ".commit", { surface = "readonly_text", pad_left = 1, scrollbar = true })
      v.commits, v.entries = {}, {}
    end
    views[name] = v
  end
  local status = views.status
  -- Filling the terminal, the header is a status line; in an overlay it is
  -- the frame's top edge.
  local header, header_buf = window("header", {
    surface = "inert", pad_left = 0, pad_right = 0, scrollbar = false,
    style = standalone and { bg = "SmeltStatusBg" } or nil,
  })
  local input, input_buf = window("revision", {
    surface = "editable_text", kind = "input", vim_enabled = false, pad_left = 1,
    placeholder = "Commit ID or revision (Enter)",
  })

  local function view() return views[current] end

  -- ── preview ──────────────────────────────────────────────────────────

  -- One-line placeholder: a dim message and a key hint that is dropped
  -- rather than cut off when the pane is narrow.
  -- Whatever the preview shows next replaces the comparison still loading
  -- for it, so a late result never covers a newer selection.
  local function cancel_preview(v)
    if v.preview_job then v.preview_job:remove(); v.preview_job = nil end
  end

  local function notice(v, text, hint)
    cancel_preview(v)
    v.preview:document(nil)
    v.shown, v.shown_diff = nil, nil
    local line = { { text = text, style = "Comment" } }
    local width = v.preview:content_width()
    if hint and (not width or smelt.text.width(text .. "  " .. hint) <= width) then
      line[2] = { text = "  " .. hint, style = { dim = true } }
    end
    v.preview_buf:styled({ line })
  end

  -- Show a diff in the preview. `owner` names what it belongs to, so a
  -- repeated selection keeps the reading position.
  local function show_diff(v, diff, owner)
    cancel_preview(v)
    if v.shown == owner and v.shown_diff == diff then return end
    v.shown, v.shown_diff = owner, diff
    v.preview:document(diff:document())
    v.preview:scroll(0)
    v.preview:cursor(0)
  end

  -- Move the sidebar cursor without treating it as a selection by the user.
  -- Scroll first: setting the scroll keeps the cursor on its screen row.
  local function place_cursor(v, row)
    if not row then return end
    v.placed = row
    v.sidebar:cursor(row)
  end

  -- The file tree a view's sidebar shows: the status tree, or the files of
  -- a commit opened from a list. Both carry `diff`, `tree` and `file`.
  local function files_of(v)
    if v.name == "status" then return status.tree and status end
    return v.opened
  end

  -- Show a file of the preview's diff from its start: its header row at
  -- the top and the left edge in view.
  local function show_file(v, files, index)
    cancel_preview(v)
    if v.shown ~= files or v.shown_diff ~= files.diff then
      v.shown, v.shown_diff = files, files.diff
      v.preview:document(files.diff:document())
    end
    files.file = index
    local row = files.diff:file_row(index)
    v.preview:scroll(row)
    v.preview:cursor(row)
    v.preview:pan(-((v.preview:scroll() or {}).left or 0))
  end

  -- ── lifecycle ────────────────────────────────────────────────────────

  local function stop(v)
    if v.job then v.job:remove(); v.job = nil end
    cancel_preview(v)
    if v.stats_job then v.stats_job:remove(); v.stats_job = nil end
    v.busy = nil
  end

  local function cleanup()
    if closed then return end
    closed = true
    for _, v in pairs(views) do stop(v) end
    if change and change.job then change.job:remove() end
    if message then message.timer:remove() end
    active = nil
  end

  local function close()
    cleanup()
    if overlay then overlay:close() end
  end

  -- End the viewer because it cannot run at all, saying why.
  local function fail(reason)
    surface:fail(reason)
    close()
  end
  for _, v in pairs(views) do
    v.sidebar:on("close", cleanup)
    v.preview:on("close", cleanup)
  end

  -- One Git operation at a time changes the repository. `work` runs in a
  -- task and may return a follow-up to run once the operation is released.
  local function run_change(label, work)
    if change or closed then return end
    local current_change = { label = label }
    change = current_change
    render_header()
    local job = smelt.spawn(function()
      local ok, after = pcall(work)
      if closed then return end
      change = nil
      render_header()
      if not ok then flash(tostring(after), "error")
      elseif after then after() end
    end)
    -- A task that finished at once already cleared `change`.
    current_change.job = job
  end

  local function git(args)
    local result, err = smelt.process.run("git", args, { cwd = cwd, env = GIT_ENV, timeout_secs = 300 })
    if not result then return nil, err end
    if result.timed_out then return nil, "git " .. args[1] .. " timed out" end
    if result.exit_code ~= 0 then return nil, result.stderr ~= "" and result.stderr or result.stdout end
    return result
  end

  -- Compare revisions for the preview of `v`; `loading` names what for.
  -- `on_diff` gets the diff unless the preview showed something else first.
  local function load_preview(v, loading, opts, on_diff)
    notice(v, "Loading")
    opts.cwd = cwd
    v.loading = loading
    v.preview_job = smelt.spawn(function()
      local result, err = smelt.git.diff(opts)
      if closed then return end
      v.preview_job = nil
      if not result then notice(v, "Comparison failed: " .. summary(err), "r retry"); return end
      on_diff(result.diff)
    end)
  end

  -- ── status ───────────────────────────────────────────────────────────

  local load_status, load_list, reload_list

  local function tracking_caption()
    local t = status.tracking
    if not (t and (t.branch or t.head)) then return nil end
    local text = t.branch and display(t.branch) or ("detached " .. (t.head or "unborn"):sub(1, 8))
    if t.upstream then
      text = text .. (t.ahead > 0 and (" ↑" .. t.ahead) or "") .. (t.behind > 0 and (" ↓" .. t.behind) or "")
        .. "  " .. display(t.upstream)
    end
    if t.operation then text = text .. " · " .. t.operation .. " in progress" end
    return text
  end

  local function show_status_commit(commit)
    status.caption = commit.id:sub(1, 8) .. " " .. display(commit.subject)
    render_header()
    if status.shown == commit.id or (status.preview_job and status.loading == commit.id) then return end
    load_preview(status, commit.id, { revision = commit.id }, function(diff)
      if diff:file_count() > 0 then show_diff(status, diff, commit.id); return end
      notice(status, "No changes")
      status.shown = commit.id
    end)
  end

  local function select_status_row(row)
    local node = status.tree and status.tree:node(row)
    if not node then return end
    if node.commit then
      show_status_commit(node.commit)
      return
    end
    status.caption = nil
    render_header()
    if node.index then
      show_file(status, status, node.index)
    elseif node.section == "unpushed" then
      notice(status, "Commits not on " .. display(status.tracking.upstream), "p push")
    elseif node.section == "unpulled" then
      notice(status, "Commits only on " .. display(status.tracking.upstream), "P pull --rebase")
    elseif node.last < node.first then
      notice(status, status.diff:file_count() == 0 and "Working tree clean"
        or (node.section == "staged" and "Nothing staged" or "Everything is staged"), "r refresh")
    elseif status.shown ~= status then
      -- A heading or folder with files replaces a notice with its first file.
      show_file(status, status, node.first)
    end
  end

  -- What the status cursor is on, in terms that survive a refresh.
  local function status_selection()
    local node = status.tree and status.tree:node(status.sidebar:cursor() or 0)
    if not node then return {} end
    if node.index then return { file = status.diff:file(node.index) } end
    if node.commit then return { commit = node.commit.id } end
    return { group = true }
  end

  -- Install a working `smelt.git.diff` result: its diff and branch state.
  -- `o.keep` names the selection to keep (the current one by default),
  -- `o.next_file` the file to fall back to, and `o.restore` keeps the
  -- preview position within the kept file.
  local function install_status(snapshot, o)
    local diff = snapshot.diff
    o = o or {}
    local keep = o.keep or status_selection()
    local previous = status.tree
    local row = status.sidebar:cursor() or 0
    local scroll = previous and status.sidebar:scroll()
    local offset = scroll and (row - scroll.top)
    local preview_scroll = status.preview:scroll() or {}
    local was_reading = status.shown == status and status.diff
    local reading = o.restore and was_reading
    local cursor, top
    if reading then
      cursor, top = diff:restore(status.diff, status.preview:cursor() or 0, preview_scroll.top or 0)
    end
    status.diff, status.tracking, status.stale, status.file = diff, snapshot, false, nil
    status.tree = diff:tree(previous and previous:collapsed() or nil)
    status.sidebar:document(status.tree:document())

    local target, index, same_file
    if keep.commit then
      target = status.tree:commit_row(keep.commit)
    elseif keep.group and previous then
      target = status.tree:restore_group(previous, row)
    elseif keep.file then
      index = diff:find_file(keep.file.section, keep.file.key)
      same_file = index ~= nil
      if not index and o.next_file then index = diff:find_file(o.next_file.section, o.next_file.key) end
    end
    if not target and not index then
      local section = keep.file and keep.file.section or "unstaged"
      local first, last = diff:section_range(section)
      if first <= last then index = first elseif diff:file_count() > 0 and not keep.file then index = 1 end
      if not index then target = status.tree:section_row(section) end
    end
    if index then
      target = status.tree:file_row(index, true)
      show_file(status, status, index)
      if same_file and reading then
        if top then status.preview:scroll(top) end
        if cursor then status.preview:cursor(cursor) end
      end
      if was_reading then status.preview:pan(preview_scroll.left or 0) end
    end
    if offset then status.sidebar:scroll(math.max(0, (target or 0) - offset)) end
    place_cursor(status, target or 0)
    if not index then select_status_row(target or 0) end
  end

  load_status = function()
    if change then return end
    if status.job then status.job:remove() end
    status.busy = status.diff and "refreshing" or "loading"
    render_header()
    status.job = smelt.spawn(function()
      local result, err = smelt.git.diff({ cwd = cwd })
      if closed then return end
      status.job, status.busy = nil, nil
      if not result then
        if standalone and not status.diff then
          fail("smelt diff: " .. summary(err))
          return
        end
        if status.diff then
          status.stale = true
          flash("Refresh failed: " .. summary(err), "error")
        else
          notice(status, "Load failed: " .. summary(err), "r retry")
          render_header()
        end
        return
      end
      install_status(result, { restore = true })
      render_header()
    end)
  end

  -- Files under the cursor, or under every selected row, as an inclusive
  -- range of one section.
  local function file_range(v, files)
    if v.focus == "preview" then
      if v.shown ~= files or not files.file then return end
      return files.file, files.file, files.diff:file(files.file).section
    end
    local selection = v.sidebar:selection()
    local from = selection and selection.first or (v.sidebar:cursor() or 0)
    local to = selection and selection.last or from
    local first, last
    for row = from, to do
      local node = files.tree:node(row)
      if node and not node.commit then
        local lo, hi = node.index or node.first, node.index or node.last
        if lo <= hi then first, last = math.min(first or lo, lo), math.max(last or hi, hi) end
      end
    end
    if not first then return end
    local section = files.diff:file(first).section
    if files.diff:file(last).section ~= section then
      flash("Select files of one section", "error")
      return
    end
    return first, last, section
  end

  local function leave_visual(v)
    if (v.focus == "files" and v.sidebar or v.preview):selection() then smelt.vim.set_mode("normal") end
  end

  -- `s`, `u`, `-` and `X` change the files under the file list's cursor or
  -- selection, or the hunks under the preview's, as fugitive does.
  local function change_files(action)
    if current ~= "status" or change or status.job or status.stale or not status.diff then return end
    local diff = status.diff
    local first, last, section, rows
    if status.focus == "preview" then
      if status.shown ~= status then return end
      local selection = status.preview:selection()
      local cursor = status.preview:cursor() or 0
      rows = { first = selection and selection.first or cursor, last = selection and selection.last or cursor }
      first = diff:file_at(rows.first)
      last = diff:file_at(rows.last)
      if not first or not last then return end
      section = diff:file(first).section
    else
      first, last, section = file_range(status, status)
      if not first then return end
    end
    if (action == "stage" and section ~= "unstaged") or (action == "unstage" and section ~= "staged") then return end
    if action == "discard" and section ~= "unstaged" then
      flash("Only unstaged changes can be discarded; unstage first", "error")
      return
    end
    local keep = diff:file(first)
    local lo, hi = diff:section_range(section)
    local next_file = last < hi and diff:file(last + 1) or (first > lo and diff:file(first - 1) or nil)
    local verb = action == "discard" and "Discard" or (section == "unstaged" and "Stage" or "Unstage")
    local function apply()
      if status.diff ~= diff then flash("Changes were refreshed; select again", "error"); return end
      leave_visual(status)
      run_change(verb:lower():gsub("e$", "") .. "ing", function()
        local result, err
        if rows and action == "discard" then result, err = smelt.git.discard_hunks(diff, rows.first, rows.last)
        elseif rows then result, err = smelt.git.index_hunks(diff, rows.first, rows.last, action)
        elseif action == "discard" then result, err = smelt.git.discard(diff, first, last)
        else result, err = smelt.git.index(diff, first, action, last) end
        if closed then return end
        if not result or result.refresh_error then
          status.stale = true
          flash(result and ("Refresh failed: " .. summary(result.refresh_error))
            or (verb .. " failed: " .. summary(err)), "error")
          return
        end
        -- Hunk changes keep the reading position in what is left of the file.
        install_status(result, { keep = { file = keep }, next_file = next_file, restore = rows ~= nil })
      end)
    end
    if action ~= "discard" then apply(); return end
    local what
    if rows then what = "the selected hunks"
    else what = first == last and display(keep.path) or ((last - first + 1) .. " files") end
    ask("Discard changes to " .. what .. "?", { { key = "y", label = "discard", run = apply } })
  end

  -- ── commits and remotes ──────────────────────────────────────────────

  -- New commits or refs: every view reloads, lists when next shown.
  local function repository_changed()
    for _, name in ipairs({ "branch", "history" }) do
      stop(views[name])
      views[name].loaded = false
    end
    if current ~= "status" then load_list(view(), false) end
    load_status()
  end

  -- Run a Git command that changes refs. `after` runs once it succeeds.
  local function remote(label, done, args, after)
    run_change(label, function()
      local result, err = git(args)
      if closed then return end
      if not result then flash(summary(err), "error"); return end
      flash(done, "info")
      return after or repository_changed
    end)
  end

  -- Where a branch without an upstream goes: `remote.pushDefault`, else
  -- `origin`, else the only remote.
  local function push_remote()
    local default = git({ "config", "--get", "remote.pushDefault" })
    if default then return (default.stdout:gsub("%s+$", "")) end
    local listed, err = git({ "remote" })
    if not listed then return nil, summary(err) end
    local remotes = {}
    for name in listed.stdout:gmatch("%S+") do
      if name == "origin" then return name end
      remotes[#remotes + 1] = name
    end
    if #remotes == 1 then return remotes[1] end
    return nil, #remotes == 0 and "No remote to push to" or "Several remotes: set remote.pushDefault"
  end

  -- Push the branch, setting its upstream on the first push. A rejected
  -- push asks how to go on: rebasing keeps the remote commits, forcing
  -- drops them.
  local function push(force)
    local t = status.tracking
    if change or not t then return end
    if not t.branch then flash("Detached HEAD: check out a branch to push", "error"); return end
    run_change(force and "force pushing" or "pushing", function()
      local args = { "push" }
      if force then args[#args + 1] = "--force-with-lease" end
      if not t.upstream then
        local name, err = push_remote()
        if not name then flash(err, "error"); return end
        for _, arg in ipairs({ "--set-upstream", name, t.branch }) do args[#args + 1] = arg end
      end
      local result, err = git(args)
      if closed then return end
      if result then
        flash(force and "Force pushed" or "Pushed", "info")
        return repository_changed
      end
      local rejected = err:find("[rejected]", 1, true) or err:find("non-fast-forward", 1, true)
      if force or not rejected then flash(summary(err), "error"); return end
      return function()
        ask("Push rejected: remote has commits you lack.", {
          { key = "r", label = "rebase, push", run = function()
            remote("pulling", "Pulled", { "pull", "--rebase" }, function()
              repository_changed()
              push(false)
            end)
          end },
          { key = "f", label = "force, drop them", run = function() push(true) end },
        })
      end
    end)
  end

  -- Commit with `git commit` on the terminal, so Git's editor, template
  -- and hooks work as they do outside smelt.
  local function commit(amend)
    if change or not status.diff then return end
    if not amend then
      local first, last = status.diff:section_range("staged")
      if first > last then flash("Nothing staged", "error"); return end
    elseif not (status.tracking and status.tracking.head) then
      flash("Nothing to amend", "error")
      return
    end
    run_change(amend and "amending" or "committing", function()
      local ok, err = smelt.terminal.run({ "git", "commit", amend and "--amend" or nil }, { cwd = cwd })
      if closed then return end
      if ok then
        local head = git({ "log", "-1", "--format=%h %s" })
        flash(head and summary(head.stdout) or "Committed", "info")
      else
        flash(summary(err), "error")
      end
      -- Hooks may change files even when the commit fails.
      return repository_changed
    end)
  end

  -- ── branch and history lists ─────────────────────────────────────────

  -- One list row: a dim prefix, a label cut to fit, then its line totals
  -- once they are known.
  local function list_row(prefix, label, style, totals, width)
    local counts = {}
    if totals then
      if totals.additions > 0 then counts[#counts + 1] = { text = " +" .. totals.additions, style = "SmeltDiffAddCount" } end
      if totals.deletions > 0 then counts[#counts + 1] = { text = " -" .. totals.deletions, style = "SmeltDiffDeleteCount" } end
      if totals.binary then counts[#counts + 1] = { text = " binary", style = "Comment" }
      elseif #counts == 0 then counts[1] = { text = " empty", style = "Comment" } end
    end
    local room = width - smelt.text.width(prefix)
    for _, count in ipairs(counts) do room = room - #count.text end
    local row = { { text = prefix, style = "Comment" },
      { text = smelt.text.truncate_cells(label, math.max(1, room)), style = style } }
    for _, count in ipairs(counts) do row[#row + 1] = count end
    return row
  end

  local function render_list(v)
    v.entries = {}
    local rows = {}
    local width = v.sidebar:content_width() or 38
    if v.name == "branch" and v.head and #v.commits > 0 then
      v.entries[1] = { combined = true }
      rows[1] = list_row("", "Combined changes vs " .. display(v.base), "SmeltAccent", v.totals, width)
    end
    for _, entry in ipairs(v.commits) do
      v.entries[#v.entries + 1] = entry
      rows[#rows + 1] = list_row(entry.id:sub(1, 7) .. " ", display(entry.subject), nil, stats[entry.id], width)
    end
    if v.more then
      v.entries[#v.entries + 1] = { more = true }
      rows[#rows + 1] = { { text = "Load older commits (Enter)", style = "Comment" } }
    end
    if #rows == 0 then
      local empty = v.error or (not v.loaded and "Loading")
        or (v.name == "branch" and ("Up to date with " .. display(v.base or "the base")) or "No commits")
      rows[1] = { { text = empty, style = "Comment" } }
    end
    v.sidebar:document(nil)
    v.sidebar_buf:styled(rows)
  end

  -- Draw the list again where it stands, such as when totals arrive.
  local function redraw_list(v)
    if v.opened then return end
    local row = v.sidebar:cursor() or 0
    render_list(v)
    place_cursor(v, row)
  end

  -- Totals need a diff of every commit, so they load after the list.
  local function load_stats(v)
    local ids = {}
    for _, entry in ipairs(v.commits) do
      if not stats[entry.id] then ids[#ids + 1] = entry.id end
    end
    if #ids == 0 then return end
    if v.stats_job then v.stats_job:remove() end
    v.stats_job = smelt.spawn(function()
      local result = smelt.git.stats({ cwd = cwd, commits = ids })
      if closed then return end
      v.stats_job = nil
      -- Totals only decorate the list, which stays usable without them.
      if not result then return end
      for id, totals in pairs(result) do stats[id] = totals end
      redraw_list(v)
    end)
  end

  local function entry_info(v, entry)
    if entry.combined then
      local count = #v.commits .. (v.more and "+ commits" or (#v.commits == 1 and " commit" or " commits"))
      return { "Branch changes vs " .. display(v.base), "Committed changes only", count }
    end
    local lines = { entry.id:sub(1, 8) .. " | " .. display(entry.author) .. " | " .. display(entry.age) }
    for line in (entry.message .. "\n"):gmatch("(.-)\n") do lines[#lines + 1] = display(line) end
    return lines
  end

  local function select_entry(v, force)
    local row = v.sidebar:cursor() or 0
    if not force and row == v.row then return end
    v.row = row
    cancel_preview(v)
    v.selected = nil
    local entry = v.entries[row + 1]
    if not entry or entry.more then
      v.caption = nil
      v.info_buf:lines({ "" })
      notice(v, entry and "Older commits" or (v.error or (v.loaded and "No commits" or "Loading")),
        entry and "Enter load" or nil)
      if v.name == current then render_header() end
      return
    end
    v.caption = entry.combined and (display(v.base) .. "..." .. v.head:sub(1, 8))
      or (entry.id:sub(1, 8) .. " - first parent")
    v.info_buf:lines(entry_info(v, entry))
    v.info:scroll(0)
    if v.name == current then render_header() end
    local owner = entry.combined and "combined" or entry.id
    local opts = { revision = entry.combined and v.head or entry.id, base = entry.combined and v.base_id or nil }
    load_preview(v, owner, opts, function(diff)
      v.selected = diff
      if entry.combined then
        local additions, deletions, binary = diff:totals()
        v.totals = { additions = additions, deletions = deletions, binary = binary }
        redraw_list(v)
      end
      if diff:file_count() > 0 then show_diff(v, diff, owner)
      else notice(v, "No changes") end
    end)
  end

  load_list = function(v, append)
    if v.job then return end
    v.busy = "loading commits"
    if v.name == current then render_header() end
    v.job = smelt.spawn(function()
      local page, err = smelt.git.history({ cwd = cwd, branch = v.name == "branch",
        base = append and v.base_id or (v.name == "branch" and base or nil),
        head = append and v.head or nil, skip = append and #v.commits or 0 })
      if closed then return end
      v.job, v.busy = nil, nil
      if not page then
        if append then
          flash("History load failed: " .. summary(err), "error")
          return
        end
        v.loaded, v.error, v.commits, v.head, v.more = true, summary(err), {}, nil, false
        if not v.opened then
          render_list(v)
          v.info_buf:lines({ "History unavailable" })
          notice(v, v.error, v.name == "branch" and "alt-b base  r retry" or "r retry")
        end
        if v.name == current then render_header() end
        return
      end
      v.loaded, v.error = true, nil
      if not append then v.commits, v.row, v.totals = {}, nil, nil end
      for _, entry in ipairs(page.commits) do v.commits[#v.commits + 1] = entry end
      v.head, v.more, v.base_id = page.head, page.more, page.base_id
      if not append and page.base then v.base = page.base end
      if not v.opened then
        local row = v.sidebar:cursor() or 0
        render_list(v)
        place_cursor(v, append and math.min(row, #v.entries - 1) or 0)
        select_entry(v, true)
      end
      if v.name == current then render_header() end
      load_stats(v)
    end)
  end

  reload_list = function(v)
    stop(v)
    v.opened = nil
    load_list(v, false)
  end

  -- Show the files of a commit in the sidebar; Backspace returns to the list.
  local function open_files(v, diff)
    if not v.opened then v.list_top = (v.sidebar:scroll() or {}).top or 0 end
    v.opened = { diff = diff, tree = diff:tree() }
    v.sidebar:document(v.opened.tree:document())
    if diff:file_count() > 0 then
      show_file(v, v.opened, 1)
      place_cursor(v, v.opened.tree:file_row(1, true))
    else
      notice(v, "No changes")
    end
    focus_pane(v, "files")
  end

  local function close_files(v)
    if not v.opened then return end
    v.opened = nil
    if not v.loaded and not v.job then load_list(v, false) end
    render_list(v)
    v.sidebar:scroll(v.list_top or 0)
    place_cursor(v, v.row or 0)
    select_entry(v, true)
    focus_pane(v, "files")
  end

  local function show_revision(revision)
    local v = views.history
    v.caption = display(revision) .. " - first parent"
    v.info_buf:lines({ "Revision " .. display(revision), "Read-only commit review", "Backspace returns to commits" })
    load_preview(v, revision, { revision = revision }, function(diff)
      open_files(v, diff)
      if v.name == current then render_header() end
    end)
  end

  -- ── sidebar navigation ───────────────────────────────────────────────

  local function on_cursor(v, row)
    -- A move that a later placement already superseded is stale.
    if row ~= v.sidebar:cursor() then return end
    local placed = v.placed
    v.placed = nil
    if placed == row then v.last_row = row; return end
    -- A question is about the old selection.
    dismiss()
    local files = files_of(v)
    if not files then select_entry(v); return end
    local last = files.tree:document():row_count() - 1
    if not files.tree:node(row) and row > 0 and row < last then
      -- Vim motions may land on the blank row between sections; step over it.
      v.sidebar:cursor(row + (row >= (v.last_row or 0) and 1 or -1))
      return
    end
    v.last_row = row
    if v.name == "status" then
      select_status_row(row)
      return
    end
    local node = files.tree:node(row)
    if node and node.index then show_file(v, files, node.index) end
  end

  -- Follow the preview with the sidebar while reading a file diff. Folded
  -- folders stay folded.
  local function follow_preview(v)
    local files = files_of(v)
    if closed or not files or v.shown ~= files or v.focus ~= "preview" or v.press then return end
    local scroll = v.preview:scroll()
    if not scroll then return end
    local index = files.diff:file_at(math.max(scroll.top, v.preview:cursor() or 0))
    if index and index ~= files.file then
      files.file = index
      place_cursor(v, files.tree:file_row(index))
    end
  end

  local function step_file(delta)
    local v = view()
    local files = files_of(v)
    if not files then
      v.sidebar:cursor(math.max(0, math.min(#v.entries - 1, (v.sidebar:cursor() or 0) + delta)))
      return
    end
    local count = files.diff:file_count()
    if count == 0 then return end
    local node = v.focus == "files" and files.tree:node(v.sidebar:cursor() or 0)
    local index
    local commits = node and (node.section == "unpushed" or node.section == "unpulled")
    if node and not node.index and not commits then
      -- From a heading or folder, step to its first file or the one before.
      index = delta > 0 and node.first or node.first - 1
    else
      index = (files.file or 0) + delta
    end
    index = math.max(1, math.min(count, index))
    show_file(v, files, index)
    place_cursor(v, files.tree:file_row(index, true))
    if v.name == "status" then
      status.caption = nil
      render_header()
    end
  end

  local function toggle_folder(v, files, row)
    local toggled = files.tree:toggle(row)
    if toggled then place_cursor(v, toggled) end
  end

  local function activate(v)
    local files = files_of(v)
    if not files then
      local entry = v.entries[(v.sidebar:cursor() or 0) + 1]
      if entry and entry.more then load_list(v, true)
      elseif v.selected and not v.preview_job then open_files(v, v.selected) end
      return
    end
    local row = v.sidebar:cursor() or 0
    local node = files.tree:node(row)
    if not node then return end
    if node.children then toggle_folder(v, files, row)
    elseif node.index or node.commit then focus_pane(v, "preview") end
  end

  local function collapse(v)
    local files = files_of(v)
    if not files then return end
    local row = v.sidebar:cursor() or 0
    local node = files.tree:node(row)
    if node and node.children and node.expanded then toggle_folder(v, files, row)
    elseif node and node.parent then v.sidebar:cursor(node.parent)
    elseif v.name ~= "status" then close_files(v) end
  end

  local function expand(v)
    local files = files_of(v)
    local row = v.sidebar:cursor() or 0
    local node = files and files.tree:node(row)
    if node and node.children and not node.expanded then toggle_folder(v, files, row)
    elseif node and node.children then v.sidebar:cursor(row + 1)
    else activate(v) end
  end

  -- Copy what identifies the rows under the cursor or selection: file paths,
  -- or full commit ids with subjects. Vim `y` copies rows as shown.
  local function copy_identity()
    local v = view()
    local selection = v.focus == "files" and v.sidebar:selection()
    local from = selection and selection.first or (v.sidebar:cursor() or 0)
    local to = selection and selection.last or from
    local files = files_of(v)
    local lines = {}
    for row = from, to do
      local node = files and files.tree:node(row)
      local entry = not files and v.entries[row + 1]
      if node and node.index then
        lines[#lines + 1] = files.diff:file(node.index).path
      elseif node and node.commit then
        lines[#lines + 1] = node.commit.id .. " " .. node.commit.subject
      elseif entry and entry.id then
        lines[#lines + 1] = entry.id .. " " .. entry.subject
      end
    end
    leave_visual(v)
    if #lines == 0 then return end
    smelt.clipboard.write(table.concat(lines, "\n"))
    flash("Copied " .. #lines .. (#lines == 1 and " line" or " lines"), "info")
  end

  local function hunk(forward)
    local v = view()
    if not v.shown_diff then return end
    local row = v.shown_diff:hunk(v.preview:cursor() or 0, forward)
    if row then v.preview:scroll(row); v.preview:cursor(row); focus_pane(v, "preview") end
  end

  local function fold(v)
    if not v.shown_diff then return end
    local row = v.shown_diff:toggle_fold(v.preview:cursor() or 0)
    if row then v.preview:scroll(row); v.preview:cursor(row) end
  end

  for _, v in pairs(views) do
    v.sidebar:on("cursor_moved", function(event) on_cursor(v, event.row) end)
    v.sidebar:on("focus", function() v.focus = "files" end)
    v.preview:on("focus", function() v.focus = "preview"; follow_preview(v) end)
    v.preview:on("scrolled", function() follow_preview(v) end)
    v.preview:on("selection_changed", function() follow_preview(v) end)
    v.sidebar:scope("diff.files", {
      open = function() activate(v) end,
      collapse = function() collapse(v) end,
      expand = function() expand(v) end,
    })
    v.preview:scope("diff.preview", {
      fold = function() fold(v) end,
      pan_left = function() v.preview:pan(-4) end,
      pan_right = function() v.preview:pan(4) end,
    })
    -- A folder click toggles it when released on the same row.
    local function pointer_row(event)
      local rect, scroll = v.sidebar:rect(), v.sidebar:scroll()
      if not rect or not scroll then return end
      if event.x < 0 or event.y < 0 or event.x >= rect.width or event.y >= rect.height then return end
      return scroll.top + event.y
    end
    v.sidebar:on("press", function(event)
      v.press = nil
      local files = files_of(v)
      local row = event.button == "left" and files and pointer_row(event)
      local node = row and files.tree:node(row)
      if node and node.children then v.press = { tree = files.tree, row = row } end
      -- Clicking a file or commit shows it from the top, even when selected.
      if node and (node.index or node.commit) then
        v.placed = row
        if v.name == "status" then select_status_row(row) else show_file(v, files, node.index) end
      end
    end)
    v.sidebar:on("release", function(event)
      local press = v.press
      v.press = nil
      local files = files_of(v)
      if press and files and press.tree == files.tree and pointer_row(event) == press.row then
        toggle_folder(v, files, press.row)
      end
    end)
    v.sidebar:on("resized", function()
      if not files_of(v) and v.loaded then
        render_list(v)
        place_cursor(v, v.row or 0)
      end
      -- A dragged split moves the divider joint in the header line.
      if v.name == current then render_header() end
    end)
  end

  -- ── header: tabs and a caption ───────────────────────────────────────

  local function caption()
    if pending then return pending.question .. "  " .. pending.hint, "SmeltAccent" end
    local v = view()
    -- The stale marker leads, so a long message cannot truncate it away.
    if message then return (v.stale and "stale · " or "") .. message.text, message.style end
    local busy = change and change.label or v.busy
    if busy then return busy, "Comment" end
    if v.stale then return "stale - r refresh", "ErrorMsg" end
    if v.name == "status" then return v.caption or tracking_caption(), "Comment" end
    return v.caption, "Comment"
  end

  -- The column of the split divider within the header, when both panes show.
  local function divider_column()
    local v = view()
    local files, detail, top = v.sidebar:rect(), (v.info or v.preview):rect(), header:rect()
    if single_pane or not (files and detail and top) then return nil end
    local column = files.x + files.width
    if detail.x ~= column + 1 then return nil end
    return column - top.x
  end

  -- The header is the top border: tabs on the left and the caption on the
  -- right of one line. Inside smelt the line closes a box with the frame.
  render_header = function()
    if closed then return end
    local width = header:content_width() or smelt.terminal.size().width or 80
    local tail = standalone and " " or "─┐"
    local spans = { { text = standalone and "" or "┌", style = "Comment" } }
    local x = standalone and 0 or 1
    local compact = width < 44
    tab_ranges = {}
    for _, name in ipairs(VIEWS) do
      local title = TITLES[name]
      if compact and name ~= current then title = title:sub(1, 1) end
      local text = " " .. title .. " "
      spans[#spans + 1] = { text = text, style = name == current
        and { fg = false, bg = "CursorLine", bold = true } or { dim = true } }
      tab_ranges[#tab_ranges + 1] = { first = x, last = x + #text - 1, view = name }
      x = x + #text
    end
    local text, style = caption()
    -- The caption stays right of the divider joint, with a fill cell on
    -- each side of its padding.
    local divider = divider_column()
    local start = math.max(x, divider and divider + 1 or x)
    local room = width - start - smelt.text.width(tail) - 3
    text = text and room > 3 and (" " .. smelt.text.truncate_cells(text, room) .. " ") or ""
    local fill = math.max(0, width - x - smelt.text.width(text) - smelt.text.width(tail))
    local line = {}
    for column = x, x + fill - 1 do
      line[#line + 1] = standalone and " " or (column == divider and "┬" or "─")
    end
    spans[#spans + 1] = { text = table.concat(line), style = "Comment" }
    if text ~= "" then spans[#spans + 1] = { text = text, style = style } end
    spans[#spans + 1] = { text = tail, style = "Comment" }
    header_buf:styled({ spans })
  end

  flash = function(text, kind)
    if closed then return end
    if message then message.timer:remove() end
    local shown = { text = text, style = kind == "error" and "ErrorMsg" or "SmeltAccent" }
    message = shown
    shown.timer = smelt.timer.set(MESSAGE_MS, function()
      if message == shown and not closed then
        message = nil
        render_header()
      end
    end)
    render_header()
  end

  dismiss = function()
    if not pending then return end
    for _, reg in ipairs(pending.regs) do reg:remove() end
    pending = nil
    render_header()
  end

  -- Ask a question in the header, listing each choice's key. n or Esc
  -- cancels, and any other viewer action cancels before it runs.
  ask = function(question, choices)
    dismiss()
    local v = view()
    local hints = {}
    pending = { question = question, regs = {} }
    for _, win in ipairs({ v.sidebar, v.preview }) do
      for _, choice in ipairs(choices) do
        pending.regs[#pending.regs + 1] = win:key(choice.key, function() dismiss(); choice.run() end)
      end
      for _, key in ipairs({ "n", "esc", "q" }) do
        pending.regs[#pending.regs + 1] = win:key(key, dismiss)
      end
    end
    for _, choice in ipairs(choices) do hints[#hints + 1] = choice.key .. " " .. choice.label end
    pending.hint = table.concat(hints, " · ")
    render_header()
  end

  -- ── layout ───────────────────────────────────────────────────────────

  local function narrow()
    return surface:width() < SINGLE_PANE_WIDTH
  end
  single_pane = narrow()

  focus_pane = function(v, pane)
    v.focus = pane
    if single_pane and v.name == current then render() end
    if pane == "preview" then
      v.preview:focus()
      follow_preview(v)
    else
      v.sidebar:focus()
    end
  end

  local actions
  render = function()
    if closed then return end
    local v = view()
    local shown = {}
    local function leaf(win, o)
      shown[win] = true
      return layout.leaf(win, o)
    end
    -- The split draws the only divider.
    local files = leaf(v.sidebar)
    local detail = leaf(v.preview)
    if v.info then
      detail = layout.vbox({
        { leaf(v.info), height = 3 },
        { leaf(v.preview, { border = { top = "Comment" } }), height = "fill" },
      })
    end
    local body = files
    if not single_pane then
      body = split:layout(files, detail)
    elseif v.focus == "files" then
      shown[v.preview] = nil
      if v.info then shown[v.info] = nil end
    else
      body = detail
      shown[v.sidebar] = nil
    end
    local rows = { { body, height = "fill" } }
    if query then
      rows[#rows + 1] = { leaf(input, { border = { top = "Comment" },
        title = smelt.dialog.title(query == "base" and " comparison base " or " revision ") }), height = 2 }
    end
    -- Windows left out of the layout close, so hidden ones stay as empty rows.
    local hidden = { input }
    for _, name in ipairs(VIEWS) do
      local w = views[name]
      hidden[#hidden + 1] = w.sidebar
      hidden[#hidden + 1] = w.preview
      hidden[#hidden + 1] = w.info
    end
    for _, win in pairs(hidden) do
      if not shown[win] then
        shown[win] = true
        rows[#rows + 1] = { layout.leaf(win), height = 0 }
      end
    end
    local content = layout.vbox(rows)
    if not standalone then
      content = layout.frame(content, { border = { left = "Comment", right = "Comment", bottom = "Comment" } })
    end
    local root = layout.vbox({ { leaf(header), height = 1 }, { content, height = "fill" } })
    overlay = surface:overlay({
      name = "smelt.diff", anchor = "center", border = "none",
      layout = root, scope = "diff", actions = actions,
    })
  end

  local function switch_view(name)
    if name == current or change then return end
    dismiss()
    query = nil
    current = name
    local v = view()
    render()
    render_header()
    if v.name ~= "status" and not v.loaded and not v.job then load_list(v, false) end
    focus_pane(v, v.focus)
  end

  local function cycle_view(step)
    for index, name in ipairs(VIEWS) do
      if name == current then
        switch_view(VIEWS[(index - 1 + step) % #VIEWS + 1])
        return
      end
    end
  end

  header:on("press", function(event)
    if event.button ~= "left" then return end
    for _, range in ipairs(tab_ranges) do
      if event.x >= range.first and event.x <= range.last then switch_view(range.view) end
    end
  end)
  header:on("resized", function()
    if narrow() ~= single_pane then
      single_pane = not single_pane
      render()
    end
    render_header()
  end)

  -- ── revision and base input ──────────────────────────────────────────

  local function edit_query(purpose)
    if change then return end
    query = purpose
    input_buf:lines({ purpose == "base" and (views.branch.base or base or "") or "" })
    input:placeholder(purpose == "base" and "Comparison base (Enter)" or "Commit ID or revision (Enter)",
      { dismiss_keys = {} })
    render()
    input:focus()
  end

  local function close_query()
    query = nil
    input_buf:lines({ "" })
    render()
    focus_pane(view(), view().focus)
  end

  input:key("enter", function()
    local value = (input_buf:line(1) or ""):match("^%s*(.-)%s*$")
    if value == "" then return end
    local purpose = query
    close_query()
    if purpose == "base" then
      base = value
      views.branch.base = value
      reload_list(views.branch)
      switch_view("branch")
    else
      switch_view("history")
      show_revision(value)
    end
  end)
  input:key("esc", close_query)

  -- ── keys ─────────────────────────────────────────────────────────────

  local function refresh()
    local v = view()
    if v.name == "status" then load_status() else reload_list(v) end
  end

  local function dismissing(fn)
    return function()
      dismiss()
      fn()
    end
  end

  -- Every action but closing cancels a pending question first.
  actions = {
    status = dismissing(function() switch_view("status") end),
    branch = dismissing(function() switch_view("branch") end),
    history = dismissing(function() switch_view("history") end),
    switch_pane = dismissing(function()
      local v = view()
      focus_pane(v, v.focus == "files" and "preview" or "files")
    end),
    -- Questions, inputs and Vim's visual or pending state take Esc first.
    close = close,
    refresh = dismissing(refresh),
    stage = dismissing(function() change_files("stage") end),
    unstage = dismissing(function() change_files("unstage") end),
    toggle_stage = dismissing(function() change_files("toggle") end),
    discard = dismissing(function() change_files("discard") end),
    commit = dismissing(function() commit(false) end),
    amend = dismissing(function() commit(true) end),
    push = dismissing(function() push(false) end),
    pull = dismissing(function() remote("pulling", "Pulled", { "pull", "--rebase" }) end),
    fetch = dismissing(function() remote("fetching", "Fetched", { "fetch", "--prune" }) end),
    copy = dismissing(copy_identity),
    commits = dismissing(function() if current ~= "status" then close_files(view()) end end),
    next_view = dismissing(function() cycle_view(1) end),
    prev_view = dismissing(function() cycle_view(-1) end),
    revision = dismissing(function() edit_query("revision") end),
    base = dismissing(function() edit_query("base") end),
    next_file = dismissing(function() step_file(1) end),
    prev_file = dismissing(function() step_file(-1) end),
    next_hunk = dismissing(function() hunk(true) end),
    prev_hunk = dismissing(function() hunk(false) end),
  }
  -- Unhandled dismiss chords close the overlay through its first leaf.
  header:on("dismiss", close)

  -- ── start ────────────────────────────────────────────────────────────

  active = { close = close }
  for _, name in ipairs({ "branch", "history" }) do
    render_list(views[name])
    views[name].info_buf:lines({ "" })
    notice(views[name], "Loading")
  end
  notice(status, "Loading")
  render()
  render_header()
  focus_pane(view(), "files")
  if opts.revision then show_revision(opts.revision) end
  load_status()
  if current ~= "status" and not opts.revision then load_list(view(), false) end
end

-- "status", "branch [base]", "history" or a revision.
local function parse(arg)
  local word, value = (arg or ""):match("^(%S+)%s*(.-)%s*$")
  if not word or word == "status" then return {} end
  if word == "branch" then return { view = "branch", base = value ~= "" and value or nil } end
  if word == "history" then return { view = "history" } end
  return { view = "history", revision = arg:match("^%s*(.-)%s*$") }
end

smelt.cmd.register("diff", function(arg) M.open(parse(arg)) end, {
  desc = "review status, branch commits, history, or one revision",
  args = { "[status|branch [base]|history|revision]" }, busy = "run",
})

smelt.cli.on_command("diff", function(args)
  local o = parse(table.concat(args, " "))
  o.standalone = true
  M.open(o)
end)

return M
