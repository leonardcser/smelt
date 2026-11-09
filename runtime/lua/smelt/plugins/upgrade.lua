-- Upgrade plugin: when to run `smelt upgrade`, and what to show.
--
-- The `smelt upgrade` CLI checks GitHub, downloads, verifies and swaps the
-- binary. This plugin runs it in the background at startup and every hour, and
-- shows the outcome on the startup banner's version row, after the running
-- version:
--   * "→ vX · /upgrade" when an update is available
--   * "→ vX · downloading 56%" while an installation runs
--   * "→ vX · restart to use it" once installed
--   * "· upgrade failed · /upgrade status"
--   * "· updated from vX · /changelog" on the first launch after a version change
--
-- With `autoupgrade = "auto"` (the default), updates the CLI marks as safe
-- install without asking. The running session keeps its version until the
-- next launch. Automatic installs stay quiet; failure details remain in shared
-- status, and `/upgrade` can retry them.
--
-- Commands:
--   /upgrade         start or follow an installation
--   /upgrade check   check now and report the result
--   /upgrade status  live status shared by sessions using this installation
--   /changelog      release notes of recent releases

local RELEASES_URL = "https://api.github.com/repos/leonardcser/smelt/releases?per_page=20"
local CHECK_EVERY_SECS = 3600
local RETRY_SECS = 300
local REQUEST_TIMEOUT_SECS = 60

local notify = smelt.notify.scoped("upgrade")

-- Per-process state that survives /reload.
local session = smelt.state.get("upgrade")
-- Captured once at startup: after an upgrade replaces the file, the OS may
-- report the running image under another name.
if session.exe == nil then session.exe = smelt.os.exe_path() or false end
session.last_check = session.last_check or 0

local function mode()
  local m = smelt.settings.autoupgrade
  if m == "off" or m == "notify" or m == "auto" then return m end
  return "auto"
end

local function channel()
  return smelt.settings.autoupgrade_channel == "unstable" and "unstable" or "stable"
end

-- ── first launch after an update ──────────────────────────────────────

-- Record the version this launch runs. Returns the previous one when it
-- differs, which happens on the first launch after an update or downgrade.
-- Source builds return nil.
local function previous_version()
  if not smelt.build.release then return nil end
  local seen = smelt.state.persistent("last_version")
  local previous = seen.version
  seen.version = smelt.build.version
  if previous ~= smelt.build.version then return previous end
end

if session.updated_from == nil then session.updated_from = previous_version() or false end

-- ── checks and installs ───────────────────────────────────────────────

-- Run `smelt upgrade [args] --json`. Returns the report, or nil and a one-line
-- error.
local function run_cli(args)
  if not session.exe then return nil, "cannot locate the smelt executable" end
  local argv = { "upgrade" }
  for _, arg in ipairs(args) do argv[#argv + 1] = arg end
  for _, arg in ipairs({ "--json", "--channel", channel() }) do argv[#argv + 1] = arg end
  local result, err = smelt.process.run(session.exe, argv, { timeout_secs = REQUEST_TIMEOUT_SECS })
  if not result then return nil, err or "failed to run smelt upgrade" end
  if result.timed_out then return nil, "smelt upgrade timed out" end
  if result.exit_code ~= 0 then
    smelt.log.warn("upgrade.failed", {
      args = argv, exit_code = result.exit_code, stderr = result.stderr,
    })
    -- The CLI ends its stderr with one `error:` line that names the cause.
    local last
    for line in ("\n" .. (result.stderr or "")):gmatch("\nerror: ([^\r\n]+)") do last = line end
    return nil, last or ("smelt upgrade exited with " .. tostring(result.exit_code))
  end
  local reply = smelt.parse.json(result.stdout or "")
  if type(reply) ~= "table" or type(reply.current) ~= "string"
      or (reply.status ~= nil and type(reply.status) ~= "table")
      or (args[1] == "check" and (type(reply.latest) ~= "string" or type(reply.update) ~= "boolean")) then
    return nil, "unexpected output from smelt upgrade"
  end
  return reply
end

local stages = {
  starting = "starting", checking = "checking for updates", downloading = "downloading",
  available = "update available, run /upgrade to install",
  verifying = "verifying checksum", extracting = "extracting", validating = "validating executable",
  installing = "installing", building = "building", installed = "installed, restart smelt to use it",
  up_to_date = "up to date", failed = "failed", interrupted = "interrupted, run /upgrade to retry",
}

local function download_text(status)
  local bytes, total = status.downloaded_bytes or 0, status.total_bytes
  local text = string.format("%.1f MiB", bytes / 1048576)
  if total and total > 0 then
    text = text .. string.format(" / %.1f MiB (%d%%)", total / 1048576,
      math.min(100, math.floor(bytes * 100 / total)))
  end
  return text
end

local function status_text(status)
  local text = (status.target or "smelt") .. " (" .. (status.channel or "unknown channel") .. "): "
    .. (stages[status.stage] or status.stage or "unknown")
  if status.stage == "downloading" then text = text .. " " .. download_text(status) end
  if status.activity then text = text .. ": " .. status.activity end
  if status.error then text = text .. ": " .. status.error end
  return text
end

-- Short progress for the banner: "downloading 56%".
local function progress_text(status)
  local text = (stages[status.stage] or status.stage or "upgrading"):match("^[^,]+")
  local total = status.total_bytes
  if status.stage == "downloading" and total and total > 0 then
    text = text .. string.format(" %d%%", math.min(100, math.floor((status.downloaded_bytes or 0) * 100 / total)))
  end
  return text
end

-- What follows the running version on the banner's version row: "→ target"
-- when a newer version is known, then what happened or the command to run.
local function banner_text()
  local snapshot, check = session.snapshot, session.check
  local status = snapshot and snapshot.status
  local current = check and check.current or snapshot and snapshot.current
  if status and status.running then
    local target = status.target and ("→ " .. status.target .. " ") or ""
    return target .. "· " .. progress_text(status)
  elseif status and (status.stage == "failed" or status.stage == "interrupted") then
    return "· upgrade " .. status.stage .. " · /upgrade status"
  elseif current and current ~= smelt.build.display then
    return "→ " .. current .. " · restart to use it"
  elseif check and check.update then
    return "→ " .. check.latest .. " · /upgrade"
  elseif not check and status and status.stage == "available" and status.target then
    return "→ " .. status.target .. " · /upgrade"
  elseif session.updated_from then
    return "· updated from v" .. session.updated_from .. " · /changelog"
  end
end

smelt.banner.source("upgrade", function()
  local suffix = banner_text()
  return suffix and { suffix = suffix }
end)

local status_view
-- Timer control, defined with the timers below.
local follow_progress, schedule_check

local function update_view(status, err)
  if not status_view then return end
  local lines = err and { "Could not read upgrade status", err }
    or { "No upgrades yet", "Run /upgrade to check and install updates." }
  if status then
    local details = {}
    if status.target then details[#details + 1] = status.target end
    if status.channel then details[#details + 1] = status.channel end
    if status.started_at then
      local ended = status.running and os.time() or (status.updated_at or os.time())
      details[#details + 1] = smelt.text.format_duration(math.max(0, ended - status.started_at))
    end
    lines = { stages[status.stage] or status.stage or "unknown", table.concat(details, " · ") }
    if status.stage == "downloading" then lines[#lines + 1] = download_text(status) end
    if status.activity then lines[#lines + 1] = status.activity end
    if status.error then
      if lines[#lines] ~= "" then lines[#lines + 1] = "" end
      lines[#lines + 1] = status.error
    end
  end
  local text = table.concat(lines, "\n")
  if status_view.text ~= text then
    status_view.text = text
    local styled = {}
    for i, line in ipairs(lines) do
      styled[i] = { { text = line, style = { bold = i == 1, dim = i == 2 and not err } } }
    end
    status_view.buf:styled(styled)
  end
end

local function notify_result(snapshot)
  local status = snapshot.status
  local text = status_text(status)
  if status.stage == "up_to_date" then
    text = snapshot.current == smelt.build.display and ("smelt is up to date (" .. snapshot.current .. ")")
      or (snapshot.current .. " installed, restart smelt to use it")
  end
  if status.stage == "failed" then notify.error(text)
  elseif status.stage == "interrupted" then notify.warn(text)
  else notify.info(text) end
end

local function apply_shared(snapshot, observe)
  local previous_banner = banner_text()
  local notified = false
  session.snapshot = snapshot
  if session.check and session.check.current ~= snapshot.current then session.check = nil end
  local status = snapshot.status
  if status and status.running then
    if observe then session.observed = status.operation_id end
  else
    if status and session.observed and session.observed == status.operation_id then
      notify_result(snapshot)
      notified = true
    end
    session.observed = nil
  end
  if previous_banner ~= banner_text() then smelt.banner.refresh() end
  update_view(status)
  follow_progress()
  return notified
end

local function apply_check(report, verbose)
  local previous_banner = banner_text()
  session.check = report
  if report.update then
    local key = report.channel .. ":" .. report.latest
    if verbose or (mode() == "notify" and session.announced ~= key) then
      session.announced = key
      notify.info("update " .. report.latest .. " available, run /upgrade")
    end
  elseif verbose then
    notify.info(report.current == smelt.build.display and ("smelt is up to date (" .. report.current .. ")")
      or (report.current .. " installed, restart smelt to use it"))
  end
  if previous_banner ~= banner_text() then smelt.banner.refresh() end
end

-- Serialize CLI requests, but keep the latest explicit command when a quiet
-- poll is in flight. Operation liveness always comes from the next snapshot.
local busy, pending = false, nil
local function request(action, verbose)
  if busy then
    if verbose then pending = action end
    return
  end
  busy = true
  smelt.spawn(function()
    local ok, err = pcall(function()
      local args = action == "status" and { "status" } or { "--background" }
      if action == "auto" then
        args = { "--background", "--policy", "auto" }
      end
      local snapshot, read_err = run_cli(args)
      if not snapshot then
        session.last_check = os.time() - CHECK_EVERY_SECS + RETRY_SECS
        schedule_check()
        update_view(nil, read_err)
        if verbose then notify.warn("upgrade request failed: " .. read_err) end
        return
      end
      local notified = apply_shared(snapshot, verbose)
      if verbose and not notified and action ~= "status" and snapshot.status then
        if snapshot.status.running then
          notify.info(status_text(snapshot.status) .. "; run /upgrade status for progress")
        else
          notify_result(snapshot)
        end
      end
    end)
    busy = false
    if not ok then error(err, 0) end
    if pending then
      local action = pending
      pending = nil
      request(action, true)
    end
  end)
end

-- Availability checks are session-local and do not serialize with installers.
local checking, check_verbose = false, false
local function check_updates(automatic, verbose)
  check_verbose = check_verbose or verbose
  if checking then return end
  checking = true
  smelt.spawn(function()
    local ok, err = pcall(function()
      local report, check_err = run_cli({ "check" })
      if not report then
        session.last_check = os.time() - CHECK_EVERY_SECS + RETRY_SECS
        if check_verbose then notify.warn("upgrade check failed: " .. check_err) end
        return
      end
      apply_check(report, check_verbose)
      if automatic and mode() == "auto" and report.update and report.auto_install == true then
        request("auto", false)
      end
    end)
    checking, check_verbose = false, false
    schedule_check()
    if not ok then error(err, 0) end
  end)
end

-- Polls shared status every second only while an installation runs or the
-- status view is open: lock release, which ends an installation, writes no file.
local progress
local function wants_progress()
  local status = session.snapshot and session.snapshot.status
  return (status and status.running) or status_view ~= nil
end

function follow_progress()
  local wanted = wants_progress()
  if wanted and not progress then
    progress = smelt.tick.every(1, function()
      if wants_progress() then request("status", false) end
    end)
  elseif not wanted and progress then
    progress:remove()
    progress = nil
  end
end

-- One timer per pending check, so an idle session sleeps until it is due.
local next_check
function schedule_check()
  if next_check then next_check:remove() end
  next_check = nil
  if mode() == "off" then return end
  local delay = math.max(0, session.last_check + CHECK_EVERY_SECS - os.time())
  next_check = smelt.timer.set(delay * 1000, function()
    next_check = nil
    if mode() == "off" or checking then return end
    session.last_check = os.time()
    check_updates(mode() == "auto", false)
  end)
end

smelt.signal.subscribe("settings_autoupgrade", schedule_check)

-- Installations started elsewhere (another session, `smelt upgrade` in a
-- shell) announce themselves by writing the shared status file.
if session.exe then
  local ok, err = pcall(smelt.fs.watch, session.exe:match("^(.*)[/\\]") or ".", function(event)
    if progress then return end
    for _, path in ipairs(event.paths) do
      if path:match("[/\\]%.smelt%-upgrade%.status%.json$") then
        request("status", false)
        return
      end
    end
  end, { recursive = false })
  if not ok then smelt.log.warn("upgrade.watch_failed", { exe = session.exe, error = tostring(err) }) end
end

-- Without the installation lock no upgrade has run here, and the CLI would
-- report no status, so the startup read is skipped.
local function upgrade_recorded()
  local dir = session.exe and session.exe:match("^(.*)[/\\]")
  return dir ~= nil and smelt.fs.exists(dir .. "/.smelt-upgrade.lock")
end

if session.snapshot == nil and upgrade_recorded() then request("status", false) end
follow_progress()
schedule_check()

smelt.cmd.register("upgrade", function(args)
  local action = (args or ""):match("^%s*(.-)%s*$")
  if action == "status" then
    if not status_view then
      local buf = smelt.buf.new({ readonly = true })
      buf:source("Reading upgrade status…")
      local handle = smelt.dialog.viewer({
        title = "upgrade status", buf = buf, interactive = false, max_height = "50%",
      })
      local view = { buf = buf }
      status_view = view
      handle.win:on("close", function()
        if status_view == view then status_view = nil end
        follow_progress()
      end)
      follow_progress()
    end
    request("status", true)
    return
  end
  if action ~= "" and action ~= "check" then
    notify.warn("usage: /upgrade [check|status]")
    return
  end
  if action == "check" then check_updates(false, true) else request("start", true) end
end, { desc = "start or follow an upgrade", args = { "check", "status" } })

-- ── /changelog ────────────────────────────────────────────────────────

-- Release notes of recent stable releases as markdown, newest first.
local function fetch_release_notes()
  local response, err = smelt.http.get(RELEASES_URL, {
    headers = {
      ["Accept"] = "application/vnd.github+json",
      ["User-Agent"] = "smelt/" .. smelt.build.version,
    },
    timeout_secs = 15,
  })
  if not response then return nil, err end
  if response.status ~= 200 then return nil, "github returned HTTP " .. tostring(response.status) end
  local releases = smelt.parse.json(response.body or "")
  if type(releases) ~= "table" then return nil, "unexpected response from github" end
  local sections = {}
  for _, release in ipairs(releases) do
    local tag = release.tag_name
    if not release.draft and not release.prerelease
        and type(tag) == "string" and tag:match("^v%d+%.%d+%.%d+$") then
      local heading = "# " .. tag
      if type(release.published_at) == "string" then
        heading = heading .. " · " .. release.published_at:sub(1, 10)
      end
      if tag == smelt.build.display then heading = heading .. " (running)" end
      local body = type(release.body) == "string" and release.body ~= "" and release.body
        or "_No release notes._"
      sections[#sections + 1] = heading .. "\n\n" .. body:gsub("\r\n", "\n")
    end
  end
  if #sections == 0 then return nil, "no releases found" end
  return table.concat(sections, "\n\n")
end

smelt.cmd.register("changelog", function()
  local buf = smelt.buf.new({ mode = "markdown", readonly = true })
  buf:source(session.notes or "_Fetching release notes…_")
  local handle = smelt.dialog.viewer({
    title = "changelog", buf = buf, min_height = "30%", max_height = "70%",
  })
  if session.notes then return end
  local closed = false
  handle.win:on("close", function() closed = true end)
  smelt.spawn(function()
    local notes, err = fetch_release_notes()
    session.notes = notes
    if not closed then buf:source(notes or ("Could not fetch release notes: " .. tostring(err))) end
  end)
end, { desc = "show release notes of recent smelt releases" })
