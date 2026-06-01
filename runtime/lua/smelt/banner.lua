-- Smelt logo art and rendering helpers. Pure data; no side effects.
-- Override `PALETTE`, `FIRE_PIXELS`, `WORDMARK_PIXELS` after require to
-- retheme, or `smelt.builtins.disable({ plugins = { "banner" } })` from
-- `early.lua` to drop the bundled splash/shutdown surface entirely.
--
-- Capital and lowercase keys in PALETTE both paint a pixel. `.` is
-- transparent. Two pixel rows pack into one terminal row via `▀` / `▄`.

local M = {}

M.PALETTE = {
	R = 124, -- dark red (outer flame edge)
	O = 202, -- red-orange
	o = 208, -- orange
	Y = 220, -- yellow (hot inner glow)
	W = 15, -- wordmark
	G = 244, -- wordmark shadow
}

M.LIGHT_PALETTE = {
	R = 124,
	O = 202,
	o = 208,
	Y = 220,
	W = 235, -- wordmark
	G = 250, -- wordmark shadow
}

function M.palette()
	if smelt.theme.is_light() then return M.LIGHT_PALETTE end
	return M.PALETTE
end

-- 5-glyph "smelt" assembled from a 4-row pixel font with 1-pixel gaps.
M.WORDMARK_PIXELS = {
	"WWW.WWWWW.WWW.W..WWW",
	"WGG.WGWGW.WGG.W..GWG",
	"..W.W.W.W.W...W...W.",
	"WWW.W.W.W.WWW.WW..WW",
}

M.FIRE_PIXELS = {
	"......R.............",
	"......OO............",
	".....ROooOR.........",
	"....ROoYYoOR.RO.....",
	"...ROoYYYYYoOooO....",
	".ROooYYYYYYYoYYoOR..",
	"....................",
}

-- Compose fire above wordmark, both centered, with a leading blank row
-- when needed so pixel pairs align on cell-row boundaries.
function M.fire_wordmark(fire, wordmark)
	fire = fire or M.FIRE_PIXELS
	wordmark = wordmark or M.WORDMARK_PIXELS
	local fire_w, word_w = #fire[1], #wordmark[1]
	local width = math.max(fire_w, word_w)
	local function centered(row, row_w)
		local left = math.floor((width - row_w) / 2)
		return string.rep(".", left) .. row .. string.rep(".", width - left - row_w)
	end
	local rows = {}
	for _, row in ipairs(fire) do
		rows[#rows + 1] = centered(row, fire_w)
	end
	for _, row in ipairs(wordmark) do
		rows[#rows + 1] = centered(row, word_w)
	end
	if #rows % 2 == 1 then
		table.insert(rows, 1, string.rep(".", width))
	end
	return rows
end

M.LOGO_MARK_PIXELS = M.fire_wordmark()

function M.logo_mark_pixels(fire)
	return M.fire_wordmark(fire or M.FIRE_PIXELS, M.WORDMARK_PIXELS)
end

function M.logo_mark_size()
	return #M.LOGO_MARK_PIXELS[1], math.ceil(#M.LOGO_MARK_PIXELS / 2)
end

-- Paint a pixel grid into a `smelt.paint.Slice` using half-block glyphs.
function M.paint_pixels(slice, row0, col0, pixels, palette)
	palette = palette or M.palette()
	for y = 1, #pixels, 2 do
		local top = pixels[y]
		local bot = pixels[y + 1] or string.rep(".", #top)
		local r = row0 + math.floor((y - 1) / 2)
		for x = 1, #top do
			local fg = palette[top:sub(x, x)]
			local bg = palette[bot:sub(x, x)]
			local c = col0 + x - 1
			if fg and bg then
				slice:set(r, c, "▀", { fg = fg, bg = bg })
			elseif fg then
				slice:set(r, c, "▀", { fg = fg })
			elseif bg then
				slice:set(r, c, "▄", { fg = bg })
			end
		end
	end
end

-- Render a pixel grid to an ANSI-escape string for stdout.
function M.ansi_render(pixels, palette)
	palette = palette or M.palette()
	local out = {}
	for y = 1, #pixels, 2 do
		local top = pixels[y]
		local bot = pixels[y + 1] or string.rep(".", #top)
		local line = {}
		for x = 1, #top do
			local fg = palette[top:sub(x, x)]
			local bg = palette[bot:sub(x, x)]
			if not fg and not bg then
				line[#line + 1] = " "
			elseif fg and bg then
				line[#line + 1] = string.format("\27[38;5;%d;48;5;%dm▀\27[0m", fg, bg)
			elseif fg then
				line[#line + 1] = string.format("\27[38;5;%dm▀\27[0m", fg)
			else
				line[#line + 1] = string.format("\27[38;5;%dm▄\27[0m", bg)
			end
		end
		out[#out + 1] = table.concat(line)
	end
	return table.concat(out, "\n")
end

-- Named-source registry for the splash banner. Mirrors
-- `require("smelt.statusline").add`: each call by `name` replaces any prior
-- registration with the same name; the bundled banner plugin queries
-- every source each time it opens the splash. Each source returns one
-- of:
--   * nil                                - contribute nothing
--   * string                             - one dim line
--   * { text, dim?, style_group? }       - one styled entry
--   * { entry, entry, ... }              - multiple styled entries
-- Returns a `Reg` whose `:remove()` drops the source.

local banner_sources = {}

function M.collect_subtitles()
	local lines = {}
	for _, fn in pairs(banner_sources) do
		local ok, result = pcall(fn)
		if not ok then
			smelt.notify.error("banner.source: " .. tostring(result))
		elseif result == nil then
			-- skip
		elseif type(result) == "string" then
			if result ~= "" then
				lines[#lines + 1] = { text = result, dim = true }
			end
		elseif type(result) == "table" then
			if result.text then
				lines[#lines + 1] = {
					text = tostring(result.text),
					dim = result.dim ~= false,
					style_group = result.style_group,
				}
			else
				for _, entry in ipairs(result) do
					if entry and entry.text then
						lines[#lines + 1] = {
							text = tostring(entry.text),
							dim = entry.dim ~= false,
							style_group = entry.style_group,
						}
					end
				end
			end
		end
	end
	return lines
end

smelt.banner = smelt.banner or {}

function smelt.banner.source(name, fn)
	if type(name) ~= "string" or name == "" then
		error("smelt.banner.source: name must be a non-empty string", 2)
	end
	if type(fn) ~= "function" then
		error("smelt.banner.source: fn must be a function", 2)
	end
	banner_sources[name] = fn
	return smelt.reg.new(function()
		if banner_sources[name] == fn then
			banner_sources[name] = nil
		end
	end)
end

return M
