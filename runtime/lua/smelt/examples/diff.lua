-- `/diff <filepath>` — side-by-side diff of <filepath> vs `git show HEAD:<filepath>`.
-- Demo of `smelt.diff.render_split` + `smelt.ui.layout.hbox`.
-- Not autoloaded; add `require("smelt.examples.diff")` to init.lua.

local M = {}

local STATE = nil

local function notify_err(msg)
	smelt.ui.notify_error("/diff: " .. msg)
end

local function git_root()
	local res, err = smelt.process.run("git", { "rev-parse", "--show-toplevel" })
	if err or not res or res.exit_code ~= 0 then
		return nil, "not in a git repo"
	end
	return (res.stdout or ""):gsub("%s+$", ""), nil
end

local function head_blob(root, rel)
	local res, err = smelt.process.run("git", { "-C", root, "show", "HEAD:" .. rel })
	if err or not res then
		return nil, err or "git show failed"
	end
	if res.exit_code ~= 0 then
		-- New file (no HEAD blob) — treat as empty so the right side shows all-insert.
		return "", nil
	end
	return res.stdout or "", nil
end

local function close()
	if not STATE then
		return
	end
	-- `win.close(leaf)` closes the whole overlay when the leaf belongs to one.
	if STATE.left_win then
		smelt.win.close(STATE.left_win)
	end
	STATE = nil
end

local function open(filepath)
	if STATE then
		close()
	end
	filepath = (filepath or ""):gsub("^%s+", ""):gsub("%s+$", "")
	if filepath == "" then
		notify_err("usage: /diff <filepath>")
		return
	end

	local root, gerr = git_root()
	if not root then
		notify_err(gerr)
		return
	end

	local abs = filepath
	if not smelt.path.is_absolute(abs) then
		abs = smelt.path.normalize(smelt.path.join(root, filepath))
	end
	local rel = smelt.path.relative(root, abs)

	local new_text, rerr = smelt.fs.read(abs)
	if not new_text then
		notify_err(rerr or ("could not read " .. abs))
		return
	end
	local old_text, herr = head_blob(root, rel)
	if not old_text then
		notify_err(herr)
		return
	end

	local left_buf = smelt.buf.create()
	local right_buf = smelt.buf.create()

	smelt.diff.render_split(left_buf, right_buf, {
		old = old_text,
		new = new_text,
		path = rel,
	})
	smelt.buf.set_readonly(left_buf, true)
	smelt.buf.set_readonly(right_buf, true)

	local vim = smelt.settings.vim and true or false
	local win_opts = {
		focusable = true,
		vim_enabled = vim,
		cursor_line_highlight = true,
		selectable = true,
	}
	local left_win = smelt.win.open(left_buf, win_opts)
	local right_win = smelt.win.open(right_buf, win_opts)

	local overlay = smelt.ui.overlay.open({
		title = {
			{ text = " diff ", fg = "green", bold = true },
			{ text = rel .. " ", fg = "white" },
			{ text = "(esc to close) ", fg = "grey", dim = true },
		},
		anchor = "center",
		width = "90%",
		height = "85%",
		layout = smelt.ui.layout.hbox({
			{
				smelt.ui.layout.leaf(left_win, { title = { { text = " HEAD ", fg = "red", dim = true } } }),
				width = "fill",
			},
			{
				smelt.ui.layout.leaf(right_win, { title = { { text = " working ", fg = "green", dim = true } } }),
				width = "fill",
			},
		}, { gap = 1 }),
		modal = true,
		draggable = true,
		resizable = true,
	})

	STATE = { overlay = overlay, left_win = left_win, right_win = right_win }

	smelt.win.link_scroll({ left_win, right_win })

	-- No Esc keymap: with `modal = true`, Tier 5 of the key cascade dismisses
	-- the overlay on bare Esc / Ctrl-C — and only when vim is in idle Normal
	-- (so Esc in Visual still exits Visual instead of closing the dialog).
	-- We listen for `close` to keep STATE in sync once the overlay tears down.
	smelt.win.on_event(left_win, "close", function()
		STATE = nil
	end)
	smelt.win.set_focus(left_win)
end

smelt.cmd.register("diff", function(value)
	open(value)
end, { desc = "side-by-side diff of <filepath> vs HEAD (demo)" })

return M
