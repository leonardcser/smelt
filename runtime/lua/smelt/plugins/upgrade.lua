-- Autoupgrade plugin. Two channels:
--   * stable   — latest tagged release (any tag, including prereleases)
--   * unstable — main branch HEAD (needs smelt.build.sha)
--
-- Settings:
--   smelt.settings.autoupgrade         "off" | "notify" (default) | "auto"
--   smelt.settings.autoupgrade_channel "stable" (default) | "unstable"
--
-- The plugin polls GitHub every CHECK_INTERVAL_SECS, caches the result in
-- smelt.state.persistent("upgrade"), and surfaces "update available" via:
--   * a right-strip statusline pill, and
--   * a dim subtitle under the splash version.
--
-- Slash commands:
--   /upgrade            install the newest build (no confirm dialog, just a
--                       notification). On stable: downloads the prebuilt
--                       tarball; on unstable: shells `cargo install --branch main`.
--   /upgrade --check    force a refresh of the GitHub cache and notify.
--   /changelog          open a read-only dialog with the release notes for
--                       the currently cached `latest` entry.

local OWNER = "leonardcser"
local REPO = "smelt"
local REPO_URL = "https://github.com/" .. OWNER .. "/" .. REPO .. ".git"
local CHECK_INTERVAL_SECS = 3600

local state = smelt.state.persistent("upgrade")

-- Every toast this plugin raises is tagged "upgrade" so `/messages`
-- attributes the entry to this plugin instead of the generic "lua".
local notify = smelt.notify.scoped("upgrade")

-- Per-channel sub-namespacing. Each channel owns its own `{ etag,
-- latest, last_checked_at }` record so switching channels is free: no
-- cross-channel cache to invalidate, no manual nil-out of stale keys.
-- The wrapper rejects unknown channel names so typos don't silently
-- create empty buckets.
local function channel_state(name)
  if name ~= "stable" and name ~= "unstable" then
    error("upgrade: unknown channel " .. tostring(name), 2)
  end
  state.channels = state.channels or {}
  state.channels[name] = state.channels[name] or {}
  return state.channels[name]
end

-- Module-local view of the latest cached comparison. Statusline /
-- banner read this synchronously; the background check writes it.
local latest = {
  has_update = false,
  channel = nil,
  current = nil,
  next = nil,        -- display string ("v0.6.0" or "main@abc1234")
  details = nil,     -- inner record for /upgrade dialog
}

-- ── helpers ────────────────────────────────────────────────────────────

local function now() return os.time() end

local function settings_channel()
  local ch = smelt.settings.autoupgrade_channel
  if ch ~= "stable" and ch ~= "unstable" then ch = "stable" end
  return ch
end

local function settings_mode()
  local m = smelt.settings.autoupgrade
  if m ~= "off" and m ~= "notify" and m ~= "auto" then m = "notify" end
  return m
end

-- Parse "0.6.0-alpha.2" → { 0, 6, 0, pre = "alpha.2" }. Tag may carry a
-- leading "v". Returns nil for unparseable input.
local function parse_semver(v)
  if type(v) ~= "string" then return nil end
  local s = v:gsub("^v", "")
  local major, minor, patch, rest = s:match("^(%d+)%.(%d+)%.(%d+)(.*)$")
  if not major then return nil end
  local pre, build = nil, nil
  if rest and rest ~= "" then
    local pre_match, build_match = rest:match("^%-?([^+]*)(.*)$")
    if pre_match and pre_match ~= "" then pre = pre_match end
    if build_match and build_match ~= "" then build = build_match:gsub("^%+", "") end
  end
  return {
    major = tonumber(major), minor = tonumber(minor), patch = tonumber(patch),
    pre = pre, build = build, raw = s,
  }
end

-- Per semver: any prerelease < no prerelease. Numeric identifiers
-- compare numerically; alphanumeric compare lexically.
local function compare_pre(a, b)
  if a == b then return 0 end
  if not a then return 1 end
  if not b then return -1 end
  local ai, bi = 1, 1
  while true do
    local a_dot = a:find(".", ai, true) or (#a + 1)
    local b_dot = b:find(".", bi, true) or (#b + 1)
    local a_part = a:sub(ai, a_dot - 1)
    local b_part = b:sub(bi, b_dot - 1)
    if a_part == "" and b_part == "" then return 0 end
    if a_part == "" then return -1 end
    if b_part == "" then return 1 end
    local an, bn = tonumber(a_part), tonumber(b_part)
    if an and bn then
      if an ~= bn then return an < bn and -1 or 1 end
    elseif an and not bn then
      return -1
    elseif bn and not an then
      return 1
    elseif a_part ~= b_part then
      return a_part < b_part and -1 or 1
    end
    ai = a_dot + 1
    bi = b_dot + 1
  end
end

-- Returns -1/0/1 ⇔ a < / == / > b. Unparseable inputs compare as equal.
local function compare_semver(a, b)
  local pa, pb = parse_semver(a), parse_semver(b)
  if not pa or not pb then return 0 end
  if pa.major ~= pb.major then return pa.major < pb.major and -1 or 1 end
  if pa.minor ~= pb.minor then return pa.minor < pb.minor and -1 or 1 end
  if pa.patch ~= pb.patch then return pa.patch < pb.patch and -1 or 1 end
  return compare_pre(pa.pre, pb.pre)
end

-- ── transport ──────────────────────────────────────────────────────────
--
-- Anonymous `api.github.com` enforces 60 requests/hour/IP, shared across
-- every gh client, browser, IDE extension, etc. behind the same NAT or
-- VPN exit. We try the user's local `gh` CLI first: its `gh auth login`
-- token raises the cap to 5000/hr per user and sidesteps the IP bucket
-- entirely. Only when gh is missing or fails do we fall back to anonymous
-- HTTP, and we 304-condition each request via ETag so we burn one bucket
-- slot per actual change.
--
-- api_fetch(path, etag) returns one of:
--   { json = table, new_etag = string|nil }   success
--   { not_modified = true }                    server matched ETag (HTTP path only)
--   { backoff_until = epoch }                  rate-limited, caller defers silently
--   { err = string }                           transport / non-rate-limit HTTP failure

local function github_error(resp)
  local snippet = (resp.body or ""):gsub("^%s+", ""):gsub("%s+$", ""):sub(1, 200)
  return "github HTTP " .. tostring(resp.status) .. ": " .. snippet
end

local function api_fetch(path, etag)
  local gh_args = { "api", path, "-H", "Accept:application/vnd.github+json" }
  if etag and etag ~= "" then
    table.insert(gh_args, "-H")
    table.insert(gh_args, "If-None-Match:" .. etag)
  end
  local gh = smelt.process.run("gh", gh_args, { timeout_secs = 15 })
  if gh and gh.exit_code == 0 and gh.stdout and #gh.stdout > 0 then
    local v = smelt.parse.json(gh.stdout)
    if type(v) ~= "table" then return { err = "gh: bad response" } end
    -- `gh api` doesn't surface response headers, so we can't refresh the
    -- ETag from the gh path. Preserve the prior value; if gh later
    -- disappears the HTTP fallback will repopulate it.
    return { json = v, new_etag = etag }
  end

  local req_headers = {
    ["Accept"]     = "application/vnd.github+json",
    ["User-Agent"] = "smelt-upgrade/" .. (smelt.build.version or "0"),
  }
  if etag and etag ~= "" then req_headers["If-None-Match"] = etag end
  local resp, err = smelt.http.get("https://api.github.com/" .. path, {
    headers = req_headers, timeout_secs = 15,
  })
  if not resp then return { err = err } end
  if resp.status == 304 then return { not_modified = true } end
  if resp.status == 403 then
    local h = resp.headers or {}
    local remaining = h["x-ratelimit-remaining"] or h["X-RateLimit-Remaining"]
    if remaining == "0" then
      local reset = tonumber(h["x-ratelimit-reset"] or h["X-RateLimit-Reset"])
      return { backoff_until = reset or (now() + 600) }
    end
  end
  if resp.status ~= 200 then return { err = github_error(resp) } end
  local v = smelt.parse.json(resp.body)
  if type(v) ~= "table" then return { err = "github: bad response" } end
  local new_etag = resp.headers and (resp.headers.etag or resp.headers.ETag)
  return { json = v, new_etag = new_etag }
end

-- ── channel fetchers ───────────────────────────────────────────────────

local function fetch_stable()
  local ch = channel_state("stable")
  local r = api_fetch("repos/" .. OWNER .. "/" .. REPO .. "/releases?per_page=30", ch.etag)
  if r.err then return nil, r.err end
  if r.backoff_until then return nil, nil, r.backoff_until end
  if r.not_modified then return ch.latest end
  ch.etag = r.new_etag
  local best = nil
  for _, rel in ipairs(r.json) do
    if not rel.draft and type(rel.tag_name) == "string" then
      if not best or compare_semver(rel.tag_name, best.tag_name) > 0 then
        best = rel
      end
    end
  end
  if not best then return nil, "github: no releases" end
  local rec = {
    tag_name     = best.tag_name,
    version      = best.tag_name:gsub("^v", ""),
    html_url     = best.html_url,
    name         = best.name,
    published_at = best.published_at,
    body         = best.body,
  }
  ch.latest = rec
  return rec
end

local function fetch_unstable()
  local ch = channel_state("unstable")
  local r = api_fetch("repos/" .. OWNER .. "/" .. REPO .. "/commits/main", ch.etag)
  if r.err then return nil, r.err end
  if r.backoff_until then return nil, nil, r.backoff_until end
  if r.not_modified then return ch.latest end
  ch.etag = r.new_etag
  local commit = r.json
  if type(commit.sha) ~= "string" then return nil, "github: bad response" end
  local rec = {
    sha      = commit.sha,
    short    = commit.sha:sub(1, 7),
    html_url = commit.html_url,
    date     = commit.commit and commit.commit.committer and commit.commit.committer.date,
    message  = commit.commit and commit.commit.message,
  }
  ch.latest = rec
  return rec
end

-- ── comparison + cache write ──────────────────────────────────────────

local function compute_stable()
  local rec = channel_state("stable").latest
  if not rec then return nil end
  local current = smelt.build.version or "0.0.0"
  local cmp = compare_semver(rec.version, current)
  return {
    has_update = cmp > 0,
    channel    = "stable",
    current    = "v" .. current,
    next       = "v" .. rec.version,
    details    = rec,
  }
end

local function compute_unstable()
  local rec = channel_state("unstable").latest
  if not rec then return nil end
  local sha = smelt.build.sha
  if not sha then
    return { has_update = false, channel = "unstable", current = "main", next = nil, details = rec }
  end
  local has = rec.short ~= nil and rec.sha:sub(1, #sha) ~= sha
  return {
    has_update = has,
    channel    = "unstable",
    current    = "main@" .. sha,
    next       = "main@" .. rec.short,
    details    = rec,
  }
end

local function recompute()
  local channel = settings_channel()
  local v = channel == "unstable" and compute_unstable() or compute_stable()
  if v then latest = v end
  -- The statusline composer reads `latest` on its next refresh tick;
  -- no explicit invalidation is needed.
end

-- ── periodic check ─────────────────────────────────────────────────────

local function should_check_now()
  if settings_mode() == "off" then return false end
  local last = tonumber(channel_state(settings_channel()).last_checked_at) or 0
  return (now() - last) >= CHECK_INTERVAL_SECS
end

local checking = false
local function run_check(force)
  if checking then return end
  if not force and not should_check_now() then
    recompute()
    return
  end
  checking = true
  smelt.spawn(function()
    local channel = settings_channel()
    local rec, err
    if channel == "unstable" and not smelt.build.sha then
      checking = false
      notify.error("autoupgrade: unstable channel requires a build SHA (this binary was built without git)")
      return
    end
    local backoff
    if channel == "stable" then
      rec, err, backoff = fetch_stable()
    else
      rec, err, backoff = fetch_unstable()
    end
    -- On a rate-limit response, defer the next check until github's
    -- reset window (capped to a 10 min fallback) by parking
    -- last_checked_at so `should_check_now` only fires again then.
    if backoff then
      channel_state(channel).last_checked_at = backoff - CHECK_INTERVAL_SECS
    else
      channel_state(channel).last_checked_at = now()
    end
    state.save()
    checking = false
    if backoff then return end
    if not rec then
      notify.error("autoupgrade: " .. tostring(err))
      return
    end
    local before = latest.has_update
    recompute()
    if latest.has_update and not before then
      notify("update available: " .. latest.next ..
                   (settings_mode() == "auto"
                    and " — installing in background"
                    or  " — run /upgrade"))
      if settings_mode() == "auto" then dispatch_install() end
    end
  end)
end

-- ── banner subtitle ────────────────────────────────────────────────────

smelt.banner.source("upgrade", function()
  if not latest.has_update or settings_mode() == "off" then return nil end
  return "update " .. (latest.next or "") .. " available  →  /upgrade"
end)

-- ── install paths ─────────────────────────────────────────────────────
--
-- Stable channel installs the prebuilt tarball from GitHub Releases
-- (seconds, no toolchain required). Unstable channel falls back to
-- `cargo install --branch main` because there's no prebuilt for an
-- arbitrary main HEAD. Both paths run on a background coroutine so
-- the user keeps using smelt while the install proceeds. Replacing a
-- running binary on Unix is safe: `tar -xzf -C $dir` overwrites the
-- inode, the running process keeps executing from memory, the next
-- launch picks up the new build.

-- Single in-flight guard so a periodic check that fires during an
-- install doesn't spawn a second one. `attempted[key]` remembers a
-- target we've already tried this session so a transient failure
-- doesn't loop on every tick.
local installing = false
local attempted = {}

local function install_stable_async(tag, on_done)
  if installing then return end
  if attempted["stable:" .. tag] then return end
  attempted["stable:" .. tag] = true
  installing = true
  smelt.spawn(function()
    local function done(ok, msg)
      installing = false
      if on_done then on_done(ok, msg) end
    end

    local target = smelt.build.target
    if not target or target == "" then
      notify.error("/upgrade: unknown target triple; can't pick a release asset")
      return done(false, "unknown target")
    end
    local exe, err = smelt.os.exe_path()
    if not exe then
      notify.error("/upgrade: " .. tostring(err))
      return done(false, err)
    end
    local dir = exe:match("(.*)/[^/]+$") or "."
    local asset = "smelt-" .. target .. ".tar.gz"
    local url = string.format(
      "https://github.com/%s/%s/releases/download/%s/%s",
      OWNER, REPO, tag, asset
    )
    local tmp_tar = exe .. ".upgrade.tar.gz"

    notify("downloading " .. tag .. "…")
    local r = smelt.process.run(
      "curl", { "-fLso", tmp_tar, url },
      { timeout_secs = 300 }
    )
    if not r or r.exit_code ~= 0 then
      smelt.fs.remove_file(tmp_tar)
      notify.error("/upgrade: download failed (" ..
        (r and tostring(r.exit_code) or "spawn") .. ")")
      return done(false, "download")
    end

    -- Extract `smelt` next to the running binary. Overwriting an
    -- in-use binary on Unix is safe (unlink + create new inode).
    local x = smelt.process.run(
      "tar", { "-xzf", tmp_tar, "-C", dir, "smelt" },
      { timeout_secs = 60 }
    )
    smelt.fs.remove_file(tmp_tar)
    if not x or x.exit_code ~= 0 then
      notify.error("/upgrade: tar extract failed: " ..
        (x and x.stderr or "spawn"))
      return done(false, "extract")
    end

    -- If the user renamed their binary, move the extracted `smelt`
    -- over the real path. No-op when basename is already "smelt".
    local extracted = dir .. "/smelt"
    if extracted ~= exe then
      local ok2, mverr = smelt.fs.rename(extracted, exe)
      if not ok2 then
        notify.error("/upgrade: rename to " .. exe .. " failed: " ..
          tostring(mverr))
        return done(false, "rename")
      end
    end

    notify("✓ upgraded to " .. tag .. " — restart smelt to use it")
    done(true)
  end)
end

local function install_unstable_async(sha, on_done)
  if installing then return end
  if attempted["unstable:" .. sha] then return end
  attempted["unstable:" .. sha] = true
  installing = true
  smelt.spawn(function()
    local function done(ok, msg)
      installing = false
      if on_done then on_done(ok, msg) end
    end
    notify("building main@" .. sha:sub(1, 7) ..
                 " via cargo (this may take a few minutes)…")
    -- The workspace has multiple bin crates (`smelt-agent`, `xtask`), so
    -- cargo refuses an ambiguous `cargo install --git`. Pin the package so
    -- it picks the user-facing `smelt` binary every time.
    local r = smelt.process.run("cargo", {
      "install", "--git", REPO_URL, "--branch", "main",
      "--package", "smelt-agent", "--force", "--locked",
    }, { timeout_secs = 1800 })
    if not r then
      notify.error("/upgrade: failed to spawn cargo")
      return done(false, "spawn")
    end
    if r.exit_code ~= 0 then
      local tail = r.stderr or ""
      if #tail > 600 then tail = tail:sub(-600) end
      notify.error("/upgrade: cargo install exited " ..
        r.exit_code .. "\n" .. tail)
      return done(false, "cargo")
    end
    notify("✓ upgraded to main@" .. sha:sub(1, 7) ..
                 " — restart smelt to use it")
    done(true)
  end)
end

local function dispatch_install()
  if not latest.has_update then return end
  if settings_channel() == "stable" then
    local tag = latest.details and latest.details.tag_name
    if not tag then return end
    install_stable_async(tag)
  else
    local sha = latest.details and latest.details.sha
    if not sha then return end
    install_unstable_async(sha)
  end
end

-- ── /upgrade command ──────────────────────────────────────────────────
--
-- `/upgrade` is the one-shot "make me current" action: it refreshes the
-- cache when stale, then either kicks off a background install (with a
-- notification) or reports "already up to date". The interactive
-- changelog viewer lives behind `/changelog` so this stays a single
-- keystroke decision.

local function notify_already_current()
  notify("already up to date (" .. (latest.current or "?") .. ")")
end

local function notify_install_kickoff()
  local target = latest.next or "?"
  if settings_channel() == "stable" then
    notify("upgrading to " .. target .. " in the background…")
  else
    notify("building " .. target .. " in the background (cargo install)…")
  end
end

smelt.cmd.register("upgrade", function(args)
  args = args or ""
  if args:match("%-%-check") or args == "check" then
    run_check(true)
    notify("checking for upgrades…")
    return
  end
  if should_check_now() then
    run_check(true)
    notify("checking for upgrades — install will start automatically if one is found")
    return
  end
  if not latest.has_update then
    notify_already_current()
    return
  end
  notify_install_kickoff()
  dispatch_install()
end, { desc = "install the newest smelt build (background)" })

-- ── /changelog command ────────────────────────────────────────────────
--
-- Opens a read-only dialog with the release notes (stable) or the
-- HEAD commit message (unstable) for the channel's `latest` cache.
-- When the cache is empty we trigger a fetch and surface a notification
-- instead of opening an empty panel.

local function changelog_lines()
  local lines = {}
  local body = latest.details
      and (latest.details.body or latest.details.message)
  if body and body ~= "" then
    for line in body:gmatch("([^\n]*)\n?") do
      table.insert(lines, line)
    end
  else
    table.insert(lines, "(no notes available)")
  end
  return lines
end

local function open_changelog_dialog()
  local body_buf = smelt.buf.new({ readonly = true })
  body_buf:lines(changelog_lines())
  local leaf = smelt.dialog.content({ buf = body_buf, interactive = true })

  smelt.dialog.open({
    title      = "changelog",
    min_height = "30%",
    max_height = "70%",
    panels     = { { leaf = leaf, height = "fill" } },
    keymaps    = {
      { key = "q",     on_press = function(ctx) ctx.close() end },
      { key = "<Esc>", on_press = function(ctx) ctx.close() end },
      { key = "r",     on_press = function(ctx)
          ctx.close()
          run_check(true)
          notify("refreshing changelog…")
      end },
    },
  })
end

smelt.cmd.register("changelog", function()
  if not latest.details then
    if should_check_now() or not channel_state(settings_channel()).latest then
      run_check(true)
      notify("fetching changelog…")
      return
    end
  end
  open_changelog_dialog()
end, { desc = "show release notes for the latest smelt build" })

-- ── boot ───────────────────────────────────────────────────────────────

-- `smelt.tick.every` is the reload-safe periodic primitive: subscribes
-- to the host's one-second `now` cell, throttles to CHECK_INTERVAL_SECS,
-- and dispatches into a fresh coroutine so the HTTP fetch can yield.
-- Fires the first check shortly after bring-up (no separate on_ready
-- nudge required).
smelt.tick.every(CHECK_INTERVAL_SECS, function() run_check(false) end)
