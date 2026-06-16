-- Optional plugin: `/inspect` opens a local web UI for browsing sessions,
-- their history, and the exact provider requests/responses logged to
-- `requests.jsonl`.

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

function M.setup()
	smelt.cmd.register("inspect", function(_arg)
		local url, err = smelt.inspect.url()
		if not url then
			url, err = smelt.inspect.start()
		end
		if url then
			notify("UI: " .. url)
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
