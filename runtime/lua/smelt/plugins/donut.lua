-- /donut - Andy Sloane's donut.c, spinning over everything on screen. The
-- session keeps working under it; /donut again stops it.
--
-- A straight port: a circle of radius 1 swept at radius 2 around the y axis,
-- 5 units from the eye, turned by A about the x axis and B about the z axis,
-- lit from behind the viewer and above. Only the scale follows the screen, and
-- the turn follows the clock, so a late frame still lands where it should.

local TICK_MS = 16
-- donut.c's turn: 0.07 about x and 0.035 about z every 30 ms frame.
local A_PER_MS, B_PER_MS = 0.07 / 30, 0.035 / 30
-- donut.c's luminance ramp, each step in an ember from the unlit side to
-- white-hot.
local GLYPHS = ".,-~:;=!*#$@"
local RAMP = {
  { 46, 16, 8 }, { 72, 24, 10 }, { 104, 34, 12 }, { 138, 46, 14 }, { 170, 60, 16 }, { 200, 78, 20 },
  { 226, 98, 26 }, { 244, 122, 40 }, { 252, 150, 66 }, { 255, 180, 104 }, { 255, 208, 150 }, { 255, 232, 200 },
}
local STEPS = {}
for i, rgb in ipairs(RAMP) do STEPS[i] = { glyph = GLYPHS:sub(i, i), style = { fg = { rgb = rgb } } } end

local floor, sin, cos = math.floor, math.sin, math.cos

local ui, spin

-- The sines and cosines of `n` angles evenly round the circle.
local function circle(n)
  local s, c = {}, {}
  for k = 1, n do
    local angle = (k - 1) * 2 * math.pi / n
    s[k], c[k] = sin(angle), cos(angle)
  end
  return s, c
end

-- The sample angles at vertical scale `ky`: donut.c's steps (0.07 round the
-- tube, 0.02 round the ring) at its scale of 15. A bigger donut needs fewer
-- samples per cell for the same shading, so the count grows with the square
-- root of the scale, which still beats donut.c's accuracy at every size.
local samples = {}
local function angles(ky)
  local key = floor(ky)
  if not samples[key] then
    local grow = math.sqrt(math.max(1, ky / 15))
    local tube_sin, tube_cos = circle(math.ceil(90 * grow))
    local ring_sin, ring_cos = circle(math.ceil(314 * grow))
    samples[key] = { tube_sin, tube_cos, ring_sin, ring_cos }
  end
  return table.unpack(samples[key])
end

-- Buffers kept from frame to frame, so drawing allocates nothing. A cell is in
-- the current frame while its stamp is the frame's generation; `cells` lists
-- those cells, `depth` and `step` hold their nearest sample and its shade, and
-- the `ring_*` arrays hold this frame's terms for each ring angle.
local frame = {
  gen = 0, count = 0, cols = 0,
  stamp = {}, cells = {}, depth = {}, step = {},
  ring_ce = {}, ring_cg = {}, ring_x = {}, ring_y = {}, ring_n = {},
}

-- Draw the torus turned by `A` and `B` into `frame`. The math is donut.c's,
-- with each term computed as soon as everything it depends on is known; the
-- letters are donut.c's names. Cells are twice as tall as they are wide, so y
-- scales by half.
local function render(cols, rows, A, B)
  local gen = frame.gen + 1
  local stamp, cells, depth, step = frame.stamp, frame.cells, frame.depth, frame.step
  local count = 0
  local e, g, m, n = sin(A), cos(A), cos(B), sin(B)
  -- donut.c draws 80x22 at 30 across and 15 down.
  local ky = math.min(cols / 2, rows) * 15 / 22
  local kx = 2 * ky
  local cx, cy = cols / 2, rows / 2
  local kxn, kym = kx * n, ky * m
  local fs, ds, cs, ls = angles(ky)

  local ring_ce, ring_cg, ring_x, ring_y, ring_n = frame.ring_ce, frame.ring_cg, frame.ring_x, frame.ring_y, frame.ring_n
  for k = 1, #cs do
    local c, l = cs[k], ls[k]
    ring_ce[k], ring_cg[k] = c * e, c * g
    ring_x[k], ring_y[k] = kx * l * m, ky * l * n
    -- N = 8 * ((f*e - c*d*g)*m - c*d*e - f*g - l*d*n), split into its tube
    -- part and d times this ring part.
    ring_n[k] = 8 * (c * g * m + c * e + l * n)
  end

  for j = 1, #fs do
    local f, d = fs[j], ds[j]
    local h = d + 2
    local fe, fg5 = f * e, f * g + 5
    local tube_n = 8 * (fe * m - f * g)
    -- The surface faces away from the eye where its normal does:
    -- 1 + 2*d + 5*(c*d*e + f*g) > 0. Those samples always lose the depth test.
    local away, away_c = 1 + 2 * d + 5 * f * g, 5 * d * e
    for k = 1, #cs do
      if away + away_c * cs[k] > 0 then goto next end
      -- D = 1 / (c*h*e + f*g + 5), t = c*h*g - f*e.
      local D = 1 / (ring_ce[k] * h + fg5)
      local t = ring_cg[k] * h - fe
      -- x = kx*D*(l*h*m - t*n), y = ky*D*(l*h*n + t*m), from the center.
      local x = floor(cx + D * (ring_x[k] * h - t * kxn))
      local y = floor(cy + D * (ring_y[k] * h + t * kym))
      if x >= 0 and x < cols and y >= 0 and y < rows then
        local o = y * cols + x
        local fresh = stamp[o] ~= gen
        if fresh or D > depth[o] then
          if fresh then
            stamp[o] = gen
            count = count + 1
            cells[count] = o
          end
          depth[o] = D
          local N = tube_n - d * ring_n[k]
          step[o] = N > 0 and math.min(#STEPS, floor(N) + 1) or 1
        end
      end
      ::next::
    end
  end
  frame.gen, frame.count, frame.cols = gen, count, cols
end

local function paint(slice)
  if not spin then return end
  local cols, rows = slice:width(), slice:height()
  if spin.stale or spin.cols ~= cols or spin.rows ~= rows then
    local ms = smelt.time.monotonic_ms() - spin.start
    render(cols, rows, ms * A_PER_MS, ms * B_PER_MS)
    spin.stale, spin.cols, spin.rows = false, cols, rows
  end
  local cells, step = frame.cells, frame.step
  for i = 1, frame.count do
    local o = cells[i]
    local s = STEPS[step[o]]
    slice:set(o // cols, o % cols, s.glyph, s.style)
  end
end

local function tick()
  spin.stale = true
end

local function close()
  if not ui then return end
  ui.timer:remove()
  ui.overlay:close()
  ui.paint:remove()
  ui, spin = nil, nil
end

local function open()
  spin = { start = smelt.time.monotonic_ms(), stale = true }
  ui = { paint = smelt.paint.register(paint, { name = "smelt.donut.paint" }) }
  -- A see-through layer over the whole screen: only the torus covers what is
  -- under it, and keys and the mouse still reach the session.
  ui.overlay = smelt.overlay.new({
    name = "smelt.donut",
    anchor = "screen_at", corner = "nw", row = 0, col = 0,
    width = "100%", height = "100%",
    layout = smelt.ui.layout.leaf(ui.paint),
    border = "none",
    transparent = true,
    modal = false,
    z = 200,
  })
  ui.timer = smelt.timer.every(TICK_MS, tick)
end

smelt.cmd.register("donut", function()
  if ui then close() else open() end
end, { desc = "spin a donut over everything" })
