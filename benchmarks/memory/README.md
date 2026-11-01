# Memory benchmark

Measures how much memory coding agents use when they start, and how much their
install writes to disk, so you can check the
[benchmarks guide](../../docs/docs/guide/benchmarks.md) yourself.

```bash
python3 run.py cold-start   # memory of the agent at its first request
python3 run.py install      # bytes the install writes to disk
```

Options: `--runs N` (default 3), `--agents smelt,pi`, `--jobs N` (runs in
parallel, default 1), `--out file.json`, and `--smelt-binary PATH` to test a
local smelt build instead of the v0.6.5 release. Results are the median over
runs, with the middle half (q1 to q3). You need Linux with rootless
[podman](https://podman.io).

## How it works

Each agent runs in its own container with 2 CPUs and 4 GiB, on Ubuntu 26.04.

- **`cold-start`** installs the agent and clears the file cache, as if it had
  been installed days ago. It then starts the agent headless against a local
  server that accepts the request and never answers. After 20 seconds it adds up
  the memory of every process in the container (PSS, which counts shared memory
  once). The `requests` column shows how many runs reached the server, so an
  agent that failed to start is visible.
- **`install`** runs the install command and reports the bytes written to disk.

With `--jobs N`, N containers run at once. Each is pinned to two CPUs of its own
(2 and 3, 4 and 5, and so on), so they never share a core, and CPUs 0 and 1 stay
free for the script and the local servers. It needs `2 + 2N` CPUs.

## Agents

Every agent runs with default settings. The only changes point it at the local
server: a base URL, a dummy API key and a model name. claude code also gets
`DISABLE_AUTOUPDATER=1`, so it does not try to update itself. The exact commands
are in `AGENTS` in `run.py`.

| Agent       | Version | Install                                          |
| ----------- | ------- | ------------------------------------------------ |
| smelt       | v0.6.5  | release archive, or `--smelt-binary`             |
| pi          | 1.4.0   | `npm install -g @earendil-works/pi-coding-agent` |
| codex       | 0.166.0 | `npm install -g @openai/codex`                   |
| claude code | 2.1.316 | `bootstrap.sh`                                   |
| opencode    | 2.2.5   | `npm install -g @opencode/cli`                   |

The Node agents use Node 22.

## Results

`results/<date>/` holds the raw output of each published run, with the machine
it ran on. `results/2026-10-31` is in the guide: cold-start with 7 runs and
smelt built from `main` (`v0.6.5-19-gae02ec9d5`), install with 1 run and the
v0.6.5 release. To redraw the charts in the guide and on the homepage from a
results folder:

```bash
python3 charts.py results/2026-10-31
```

## Limits

- It measures an agent waiting for its first reply, not a task with tool calls,
  so it says nothing about memory growth over a long session.
- Agents run headless, so the cost of drawing a terminal interface is left out.
- Installs need network access, and package versions they pull can drift.
  Compare runs made on the same day.
