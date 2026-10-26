// Encodes recorded frames to a GIF, holding each unique frame for its real duration.
import { $ } from "bun";
import { mkdir, rm, writeFile } from "node:fs/promises";
import path from "node:path";

export async function encodeGif(pngs: Buffer[], durations: number[], output: string, fps = 12) {
  const dir = path.resolve(import.meta.dir, "../.cache/gif");
  await rm(dir, { recursive: true, force: true });
  await mkdir(dir, { recursive: true });
  const list: string[] = [];
  for (const [i, png] of pngs.entries()) {
    const name = `f${String(i).padStart(4, "0")}.png`;
    await Bun.write(path.join(dir, name), png);
    list.push(`file '${name}'`, `duration ${(durations[i] / 1000).toFixed(3)}`);
  }
  // The concat demuxer ignores the last duration unless the final file repeats.
  list.push(`file 'f${String(pngs.length - 1).padStart(4, "0")}.png'`);
  await writeFile(path.join(dir, "frames.txt"), list.join("\n") + "\n");
  const input = ["-f", "concat", "-i", path.join(dir, "frames.txt")];
  const filter = `fps=${fps},split[a][b];[a]palettegen=stats_mode=full:max_colors=256[p];[b][p]paletteuse=dither=sierra2_4a:diff_mode=rectangle`;
  await $`ffmpeg -y -loglevel error ${input} -filter_complex ${filter} -loop 0 ${output}`;
}
