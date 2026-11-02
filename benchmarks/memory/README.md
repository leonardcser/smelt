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
`DISABLE_AUTOUPDATER=1`, so it does not try to update itself. Version pins and
install commands are in [`AGENTS` in run.py](run.py). The Node agents use Node 22.

## Results

The published data is in [`results/2026-11-02`](results/2026-11-02/): 7 startup
runs and 1 install run per agent. Each JSON file records the machine, measured
versions and smelt build provenance. Earlier results stay in their dated folders.

## Refreshing published results

Pin the latest stable comparison agents in `run.py` and build smelt with
`cargo build --locked --profile dist --bin smelt`. From this directory, run
both measurements with the same binary:

```bash
folder="results/$(date +%F)"
mkdir -p "$folder"
python3 run.py cold-start --runs 7 --smelt-binary /path/to/smelt --out "$folder/cold-start.json"
python3 run.py install --runs 1 --smelt-binary /path/to/smelt --out "$folder/install.json"
```

Check that every startup sample has `requests > 0`. Add measured `versions`
and `smelt_build` metadata to both JSON files, then regenerate:

```bash
python3 charts.py "$folder"
```

This updates the root README's versions and memory/install columns and the
guide and homepage charts. Update the guide and homepage summary text and the
results link above by hand. Leave historical Terminal-Bench task data unchanged.

Run `python3 -m unittest discover -s .`, check that a second generation makes
no changes, and run `git diff --check`. For docs changes, also run
`zensical build --strict` from `docs/`.

## Limits

- It measures an agent waiting for its first reply, not a task with tool calls,
  so it says nothing about memory growth over a long session.
- Agents run headless, so the cost of drawing a terminal interface is left out.
- Installs need network access, and package versions they pull can drift.
  Compare runs made on the same day.
