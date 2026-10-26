// Plays the built-in snake from what is on screen, for recordings: finds the board under the
// `┌ snake` title, reads the snake (green) and the food (red) out of its half blocks, and picks
// the move toward the food that leaves the snake room to keep going.

const W = 32;
const H = 32;
type Rgb = [number, number, number];
type Cell = { ch: string; fg?: Rgb; bg?: Rgb };

const isSnake = ([r, g, b]: Rgb) => g > r + 20 && g > b + 20;
const isFood = ([r, g]: Rgb) => r > 200 && g < 120;

export type Board = { snake: Set<number>; food?: number };

const key = (x: number, y: number) => y * W + x;

/** One screen line as cells with their truecolor foreground and background. */
function cells(line: string): Cell[] {
  const out: Cell[] = [];
  let fg: Rgb | undefined;
  let bg: Rgb | undefined;
  for (const part of line.split(/(\x1b\[[0-9;:]*m)/)) {
    const sgr = part.match(/^\x1b\[([0-9;:]*)m$/);
    if (!sgr) {
      for (const ch of part) out.push({ ch, fg, bg });
      continue;
    }
    const p = sgr[1].split(/[;:]/).map(Number);
    for (let i = 0; i < p.length; i++) {
      if (p[i] === 0) fg = bg = undefined;
      else if (p[i] === 39) fg = undefined;
      else if (p[i] === 49) bg = undefined;
      else if ((p[i] === 38 || p[i] === 48) && p[i + 1] === 5) i += 2;
      else if ((p[i] === 38 || p[i] === 48) && p[i + 1] === 2) {
        const rgb: Rgb = [p[i + 2], p[i + 3], p[i + 4]];
        if (p[i] === 38) fg = rgb;
        else bg = rgb;
        i += 4;
      }
    }
  }
  return out;
}

export function readBoard(ansi: string): Board | null {
  const lines = ansi.split("\n").map(cells);
  const top = lines.findIndex((l) => l.map((c) => c.ch).join("").includes("┌ snake"));
  if (top < 0) return null;
  const left = lines[top].findIndex((c) => c.ch === "┌");
  const board: Board = { snake: new Set() };
  for (let row = 0; row < H / 2; row++) {
    const line = lines[top + 1 + row] ?? [];
    // The left border, nearest the title's column in case wide glyphs shifted this line.
    let edge = -1;
    line.forEach((c, i) => {
      if (c.ch === "│" && (edge < 0 || Math.abs(i - left) < Math.abs(edge - left))) edge = i;
    });
    for (let x = 0; x < W; x++) {
      const c = line[edge + 1 + x];
      if (!c) continue;
      const halves = [c.ch === "▀" ? c.fg : undefined, c.ch === "▀" ? c.bg : undefined];
      halves.forEach((color, half) => {
        const k = key(x, row * 2 + half);
        if (!color) return;
        if (isSnake(color)) board.snake.add(k);
        else if (isFood(color)) board.food = k;
      });
    }
  }
  return board;
}

const MOVES: Record<string, [number, number]> = { Right: [1, 0], Left: [-1, 0], Down: [0, 1], Up: [0, -1] };
const OPPOSITE: Record<string, string> = { Right: "Left", Left: "Right", Down: "Up", Up: "Down" };

// Cells reachable from (x, y), capped at the snake's length.
function room(x: number, y: number, snake: Set<number>): number {
  const seen = new Set([key(x, y)]);
  const stack = [[x, y]];
  let n = 0;
  while (stack.length && n <= snake.size) {
    const [cx, cy] = stack.pop()!;
    n++;
    for (const [dx, dy] of Object.values(MOVES)) {
      const nx = cx + dx;
      const ny = cy + dy;
      const k = key(nx, ny);
      if (nx >= 0 && ny >= 0 && nx < W && ny < H && !snake.has(k) && !seen.has(k)) {
        seen.add(k);
        stack.push([nx, ny]);
      }
    }
  }
  return n;
}

/** The move from `head` toward the food, never reversing and avoiding walls, the body and pockets. */
export function pickMove(board: Board, head: number, dir: string): string {
  const hx = head % W;
  const hy = Math.floor(head / W);
  const fx = board.food === undefined ? W / 2 : board.food % W;
  const fy = board.food === undefined ? H / 2 : Math.floor(board.food / W);
  let best = dir;
  let bestScore = Infinity;
  for (const [name, [dx, dy]] of Object.entries(MOVES)) {
    if (name === OPPOSITE[dir]) continue;
    const x = hx + dx;
    const y = hy + dy;
    if (x < 0 || y < 0 || x >= W || y >= H || board.snake.has(key(x, y))) continue;
    let score = Math.abs(x - fx) + Math.abs(y - fy);
    if (room(x, y, board.snake) <= board.snake.size) score += 1000;
    if (score < bestScore) [best, bestScore] = [name, score];
  }
  return best;
}
