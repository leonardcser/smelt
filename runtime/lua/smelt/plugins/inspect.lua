-- Optional plugin: `/inspect` opens a local web UI for browsing sessions,
-- their history, and the exact provider requests/responses logged to
-- `requests.jsonl`.

local M = {}

function M.setup()
	smelt.cmd.register("inspect", function(_arg)
		local url = smelt.inspect.url()
		if not url then
			url = smelt.inspect.start()
		end
		if url then
			smelt.notify.info("Inspect UI: " .. url)
		end
	end, {
		desc = "Open the session inspector web UI in a browser.",
		args = {},
		while_busy = true,
	})
end

M.setup()
return M
