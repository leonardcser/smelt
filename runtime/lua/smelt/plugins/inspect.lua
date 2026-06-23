-- Optional plugin: `/inspect` opens a local web UI for browsing sessions,
-- their history, and provider request/response audit data.

local M = {}
local notify = smelt.notify.scoped("inspect")

-- Start the inspector server off the main thread and return its URL, or
-- return nil plus an error string on failure.
function smelt.inspect.start()
	local result = smelt.task.external(function(id)
		smelt.inspect.__start(id)
	end)
	if result.ok then
		return result.url
	end
	return nil, result.error
end

-- Stop the running inspector server.
function smelt.inspect.stop()
	local result = smelt.task.external(function(id)
		smelt.inspect.__stop(id)
	end)
	return result.ok, result.error
end

-- Open the inspector URL in a browser when the host environment can do so.
function smelt.inspect.open(url)
	return smelt.inspect.__open_url(url)
end

function M.setup()
	smelt.cmd.register("inspect", function(_arg)
		local url, err = smelt.inspect.url()
		if not url then
			url, err = smelt.inspect.start()
		end
		if url then
			local opened = smelt.inspect.open(url)
			if opened.opened then
				notify.info("Opened UI: " .. url)
			elseif opened.error then
				notify.error("UI: " .. url .. " (could not open browser: " .. opened.error .. ")")
			elseif opened.reason then
				notify.info("UI: " .. url .. " (browser auto-open unavailable: " .. opened.reason .. ")")
			else
				notify.info("UI: " .. url)
			end
		else
			notify.error(err or "failed to start server")
		end
	end, {
		desc = "Open the session inspector web UI in a browser.",
		args = {},
		while_busy = true,
	})
end

M.setup()
return M
