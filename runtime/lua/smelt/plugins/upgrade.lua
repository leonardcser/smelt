-- Upgrade plugin: when to run `smelt upgrade`, and what to show.
--
-- The `smelt upgrade` CLI checks GitHub, downloads, verifies and swaps the
-- binary. This plugin runs it in the background at startup and every hour, and
-- shows the outcome as one dim line under the startup banner:
--   * "update vX available  →  /upgrade"
--   * "vX installed, restart smelt to use it"
--   * "previously vX  →  /changelog" on the first launch after a version change
--
-- With `autoupgrade = "auto"` (the default), updates the CLI marks as safe
-- install without asking. The running session keeps its version until the
-- next launch. Background work never raises error toasts: failures go to the
-- log and a later check retries.
--
-- Commands:
--   /upgrade        install the newest build now
--   /upgrade check  check now and report the result
--   /changelog      release notes of recent releases

local RELEASES_URL = "https://api.github.com/repos/leonardcser/smelt/releases?per_page=20"
local CHECK_EVERY_SECS = 3600
local RETRY_SECS = 300
local CHECK_TIMEOUT_SECS = 60
local INSTALL_TIMEOUT_SECS = { stable = 900, unstable = 3600 }

local notify = smelt.notify.scoped("upgrade")

-- Per-process state that survives /reload.
local session = smelt.state.get("upgrade")
-- Captured once at startup: after an upgrade replaces the file, the OS may
-- report the running image under another name.
if session.exe == nil then session.exe = smelt.os.exe_path() or false end
-- Versions whose automatic install failed; `/upgrade` still retries them.
session.failed = session.failed or {}
session.last_check = session.last_check or 0
-- session.status: nil | { kind = "available" | "installed", version = string }

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

smelt.banner.source("upgrade", function()
  local status = session.status
  if status and status.kind == "installed" then
    return status.version .. " installed, restart smelt to use it"
  elseif status and status.kind == "available" then
    return "update " .. status.version .. " available  →  /upgrade"
  elseif session.updated_from then
    return "previously v" .. session.updated_from .. "  →  /changelog"
  end
end)

-- ── checks and installs ───────────────────────────────────────────────

-- Run `smelt upgrade [args] --json`. Returns the report, or nil and a one-line
-- error.
local function run_cli(args, ch, timeout_secs)
  if not session.exe then return nil, "cannot locate the smelt executable" end
  local argv = { "upgrade" }
  for _, arg in ipairs(args) do argv[#argv + 1] = arg end
  for _, arg in ipairs({ "--json", "--channel", ch }) do argv[#argv + 1] = arg end
  local result, err = smelt.process.run(session.exe, argv, { timeout_secs = timeout_secs })
  if not result then return nil, err or "failed to run smelt upgrade" end
  if result.timed_out then return nil, "smelt upgrade timed out" end
  if result.exit_code ~= 0 then
    smelt.log.warn("upgrade.failed", {
      args = argv, exit_code = result.exit_code, stderr = result.stderr,
    })
    return nil, (result.stderr or ""):match(".*error: ([^\n]+)")
      or ("smelt upgrade exited with " .. tostring(result.exit_code))
  end
  local report = smelt.parse.json(result.stdout or "")
  if type(report) ~= "table" or type(report.latest) ~= "string" then
    return nil, "unexpected output from smelt upgrade"
  end
  return report
end

local function set_status(kind, version)
  local status = session.status or {}
  if status.kind == kind and status.version == version then return end
  session.status = kind and { kind = kind, version = version } or nil
  smelt.banner.refresh()
end

-- `version` is what the binary on disk reports. Another session or a manual
-- install may have replaced it; this session runs its old image until restart.
local function set_disk_version(version)
  if version ~= smelt.build.display then
    set_status("installed", version)
  else
    set_status(nil)
  end
end

-- One check, then an install when `install` asks for it: "never", "auto"
-- (when the CLI marks the update safe), or "always". `verbose` reports every
-- outcome; background passes only announce new updates in notify mode.
local function pass(ch, install, verbose)
  local report, err = run_cli({ "check" }, ch, CHECK_TIMEOUT_SECS)
  if not report then
    session.last_check = os.time() - CHECK_EVERY_SECS + RETRY_SECS
    if verbose then notify.warn("upgrade check failed: " .. err) end
    return
  end

  if not report.update then
    set_disk_version(report.current)
    if verbose then
      if session.status then
        notify.info(report.current .. " is installed, restart smelt to use it")
      else
        notify.info("smelt is up to date (" .. report.current .. ")")
      end
    end
    return
  end

  local latest = report.latest
  local wanted = install == "always"
    or (install == "auto" and report.auto_install and not session.failed[latest])
  if not wanted then
    local status = session.status
    local announced = status and status.kind == "available" and status.version == latest
    set_status("available", latest)
    if verbose then
      notify.info("update " .. latest .. " available, run /upgrade to install it")
    elseif not announced and mode() == "notify" then
      notify.info("update " .. latest .. " available, run /upgrade")
    end
    return
  end

  if verbose then notify.info("installing " .. latest .. " in the background…") end
  local done, install_err = run_cli({}, ch, INSTALL_TIMEOUT_SECS[ch])
  if not done then
    session.failed[latest] = true
    set_status("available", latest)
    if verbose then notify.error("upgrade to " .. latest .. " failed: " .. install_err) end
    return
  end
  set_disk_version(done.installed or done.current)
  if verbose and session.status then
    notify.info(session.status.version .. " installed, restart smelt to use it")
  end
end

local busy = false

local function start(install, verbose)
  if busy then
    if verbose then notify.info("an upgrade check is already running") end
    return
  end
  busy = true
  local ch = channel()
  smelt.spawn(function()
    local ok, err = pcall(pass, ch, install, verbose)
    busy = false
    if not ok then error(err, 0) end
  end)
end

-- Ticks every minute so that switching `autoupgrade` on takes effect soon.
smelt.tick.every(60, function()
  if mode() == "off" or os.time() - session.last_check < CHECK_EVERY_SECS then return end
  session.last_check = os.time()
  start(mode() == "auto" and "auto" or "never", false)
end)

smelt.cmd.register("upgrade", function(args)
  local check_only = (args or ""):match("^%s*check%s*$") ~= nil
  notify.info("checking for updates…")
  start(check_only and "never" or "always", true)
end, { desc = "install the newest smelt build", args = { "check" } })

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
