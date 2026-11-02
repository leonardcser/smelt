import { expect, test } from "bun:test";
import { run, type RunOptions } from "./session";

const options: RunOptions = {
  scheme: "dark", cols: 110, rows: 30, typeDelay: 0, wordDelay: 0,
  live: false, vim: false, tps: false, recordEvery: 0,
};

test("screen waits accept existing matches and follow finder transitions", async () => {
  await run([
    { wait: /○ normal/, timeout: 1000 },
    { wait: /○ normal/, timeout: 1000 },
    { type: "/files" },
    { key: "Enter" },
    { wait: /find files…\s+6 \/ 6/, timeout: 5000 },
    { type: "pricing" },
    { wait: /pricing\s+1 \/ 6/, timeout: 5000 },
  ], options);
}, 30_000);

test("missing screen match fails recording with the expected pattern and screen", async () => {
  await expect(run([
    { wait: /missing-screen-sentinel/, timeout: 200 },
  ], options)).rejects.toThrow(/timed out waiting for \/missing-screen-sentinel\/[\s\S]*○ normal/);
}, 30_000);
