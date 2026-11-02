// Where a scripted session lives: its throwaway home and the project it opens. SHOTS_SLOT gives a
// recorder its own of each, and its own tmux session, so several record at once.
import path from "node:path";

const slot = process.env.SHOTS_SLOT;

export const CACHE = path.resolve(import.meta.dir, "..", slot ? `.cache-${slot}` : ".cache");
/** The project smelt opens, and the agent's tools point into. */
export const PROJECT = slot ? `/tmp/shots-${slot}/acme-app` : "/tmp/acme-app";
export const TMUX = slot ? `smelt-shots-${slot}` : "smelt-shots";
