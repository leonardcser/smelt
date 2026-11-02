#!/usr/bin/env python3
"""Measure the memory cost of coding agents in fresh podman containers.

    python3 run.py install      # bytes each agent's install writes to disk
    python3 run.py cold-start   # memory of the agent's processes at its first request

Every agent runs in its own container limited to 2 CPUs and 4 GiB. Memory is the
PSS (proportional set size) summed over every process in the container. See README.md.
"""

import argparse
import json
import os
import platform
import statistics
import subprocess
import sys
import tempfile
import threading
import time
from concurrent.futures import ThreadPoolExecutor
from datetime import date
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from queue import Queue
from pathlib import Path

IMAGE = "smelt-memory-bench"
PORT = 18080  # first mock port, one more per parallel job
MODEL = "mock-model"
URL = "http://host.containers.internal:@PORT@"
MIB = 2**20

DOCKERFILE = """\
FROM docker.io/library/ubuntu:26.04
ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y --no-install-recommends \\
    curl ca-certificates bash git build-essential procps ripgrep xz-utils python3 \\
    && rm -rf /var/lib/apt/lists/*
"""

NVM = (
    "curl -o- https://raw.githubusercontent.com/nvm-sh/nvm/v0.40.2/install.sh"
    " | env -u NODE_VERSION bash && export NVM_DIR=$HOME/.nvm && . $NVM_DIR/nvm.sh"
    " && nvm install 22 >/dev/null && nvm alias default 22 >/dev/null"
)

USE_NVM = "export NVM_DIR=$HOME/.nvm; . $NVM_DIR/nvm.sh; "

# Agent versions are pinned for repeatable comparisons (see README.md).
# `install` is the agent's official install command; `run` makes the agent
# send its first request to the mock server.
AGENTS = {
    "smelt": {
        "install": (
            "curl -fsSL https://github.com/leonardcser/smelt/releases/download/v0.6.5/"
            "smelt-x86_64-unknown-linux-gnu.tar.gz | tar xz -C /usr/local/bin"
        ),
        "run": (
            f"MOCK_KEY=x smelt --headless --model {MODEL} --api-base {URL}/v1 "
            "--api-key-env MOCK_KEY hello"
        ),
    },
    "pi": {
        "install": f"{NVM} && npm install -g --ignore-scripts @earendil-works/pi-coding-agent@1.5.0",
        "run": USE_NVM + (
            "mkdir -p ~/.pi/agent && cat > ~/.pi/agent/models.json <<'EOF'\n"
            f'{{"providers":{{"mock":{{"baseUrl":"{URL}/v1","api":"openai-completions",'
            f'"apiKey":"x","models":[{{"id":"{MODEL}"}}]}}}}}}\nEOF\n'
            f"pi --provider mock --model {MODEL} -p hello"
        ),
    },
    "codex": {
        "install": f"{NVM} && npm install -g @openai/codex@0.166.0",
        "run": USE_NVM + (
            f"OPENAI_API_KEY=x codex exec --skip-git-repo-check "
            f"-c 'openai_base_url=\"{URL}/v1\"' hello"
        ),
    },
    "claude-code": {
        "install": (
            "curl -fsSL https://downloads.claude.ai/claude-code-releases/bootstrap.sh"
            " | bash -s -- 2.1.316"
        ),
        "run": (
            f"ANTHROPIC_BASE_URL={URL} ANTHROPIC_API_KEY=x DISABLE_AUTOUPDATER=1 "
            'PATH="$HOME/.local/bin:$PATH" claude --print hello'
        ),
    },
    "opencode": {
        "install": f"{NVM} && npm i -g @opencode/cli@2.2.5",
        "run": USE_NVM + (
            'OPENCODE_CONFIG_CONTENT=\'{"providers":{"mock":{"package":"@opencode/ai/providers/openai-compatible",'
            f'"settings":{{"baseURL":"{URL}/v1","apiKey":"x"}},"models":{{"{MODEL}":{{}}}}}}}}}}\' '
            f"opencode run --standalone --model=mock/{MODEL} hello </dev/null"
        ),
    },
}

# Drops every clean file page from the page cache.
EVICT = """\
import os
for root, dirs, files in os.walk("/"):
    if root.split("/")[1:2] in (["proc"], ["sys"], ["dev"]):
        dirs[:] = []
        continue
    for name in files:
        try:
            fd = os.open(os.path.join(root, name), os.O_RDONLY)
            os.posix_fadvise(fd, 0, 0, os.POSIX_FADV_DONTNEED)
            os.close(fd)
        except OSError:
            pass
"""

SHELL = r"""
disk() { du -sk "$HOME" /usr/local /opt 2>/dev/null | awk '{s+=$1} END{print s*1024}'; }
pss() { cat /proc/[0-9]*/smaps_rollup 2>/dev/null | awk '/^Pss:/{s+=$2} END{print s*1024}'; }
d0=$(disk)
( set -e; INSTALL ) >/tmp/install.log 2>&1 || { tail -20 /tmp/install.log >&2; exit 1; }
sync
MODE
"""

INSTALL_TAIL = r"""
printf '{"disk":%s}\n' "$(( $(disk) - d0 ))"
"""

COLD_TAIL = r"""
python3 /tmp/evict.py
sleep 2
( timeout 40 bash -c 'RUN' >/tmp/run.log 2>&1 & )
sleep 20
printf '{"pss":%s}\n' "$(pss)"
"""


class Mock:
    """Local model server on one port. It accepts every request and never answers,
    so an agent waits after its first request."""

    def __init__(self, port: int) -> None:
        self.requests = 0
        mock = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_) -> None:
                pass

            def do_POST(self) -> None:
                self.rfile.read(int(self.headers.get("Content-Length") or 0))
                mock.requests += 1
                time.sleep(3600)

        self.server = ThreadingHTTPServer(("0.0.0.0", port), Handler)
        self.server.daemon_threads = True
        threading.Thread(target=self.server.serve_forever, daemon=True).start()


def build_image() -> None:
    with tempfile.TemporaryDirectory() as d:
        Path(d, "Dockerfile").write_text(DOCKERFILE)
        subprocess.run(["podman", "build", "-q", "-t", IMAGE, d], check=True, stdout=subprocess.DEVNULL)


class Slot:
    """One parallel job: two CPUs of its own and a mock server on its own port.

    Slot `i` is pinned to CPUs `2 + 2i` and `3 + 2i`, so containers never share a
    core. CPUs 0 and 1 stay free for this script, the mock servers and the host.
    """

    def __init__(self, index: int) -> None:
        self.port = PORT + index
        self.pin = ["taskset", "-c", f"{2 + 2 * index},{3 + 2 * index}"]
        self.mock = Mock(self.port)


def run_job(agent: str, mode: str, evict: Path, smelt_binary: Path | None, slot: Slot) -> dict:
    before = slot.mock.requests
    run = container(agent, mode, evict, smelt_binary, slot)
    run["requests"] = slot.mock.requests - before
    return run


def container(agent: str, mode: str, evict: Path, smelt_binary: Path | None, slot: Slot) -> dict:
    spec = {k: v.replace("@PORT@", str(slot.port)) for k, v in AGENTS[agent].items()}
    mounts = ["-v", f"{evict}:/tmp/evict.py:ro"]
    if agent == "smelt" and smelt_binary:
        mounts += ["-v", f"{smelt_binary}:/tmp/smelt-local:ro"]
        spec["install"] = "install -m 755 /tmp/smelt-local /usr/local/bin/smelt"
    if mode == "install":
        tail = INSTALL_TAIL
    else:
        tail = COLD_TAIL.replace("RUN", spec["run"].replace("'", "'\\''"))
    script = SHELL.replace("INSTALL", spec["install"]).replace("MODE", tail)
    out = subprocess.run(
        [*slot.pin, "podman", "run", "--rm", "--memory", "4g", "--cpus", "2",
         *mounts, IMAGE, "bash", "-c", script],
        capture_output=True, text=True,
    )
    if out.returncode:
        raise RuntimeError(f"{agent} failed:\n{out.stderr[-2000:]}")
    return json.loads(out.stdout.strip().splitlines()[-1])


def quartiles(values: list[float]) -> tuple[float, float, float]:
    return statistics.quantiles(values, n=4, method="inclusive") if len(values) > 1 else (values[0],) * 3


def machine() -> dict:
    """Where the runs happened, so results from different machines are not mixed up."""
    cpu = next((line.split(":", 1)[1].strip() for line in open("/proc/cpuinfo") if line.startswith("model name")), "")
    podman = subprocess.run(["podman", "--version"], capture_output=True, text=True).stdout.strip()
    return {"cpu": cpu, "kernel": platform.release(), "podman": podman}


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("mode", choices=["install", "cold-start"])
    ap.add_argument("--runs", type=int, default=3)
    ap.add_argument("--agents", default=",".join(AGENTS))
    ap.add_argument("--jobs", type=int, default=1, help="runs in parallel, each on 2 CPUs of its own")
    ap.add_argument("--smelt-binary", type=Path, help="test this smelt binary instead of the v0.6.5 release")
    ap.add_argument("--out", help="write all runs as JSON")
    args = ap.parse_args()

    if 2 + 2 * args.jobs > os.cpu_count():
        sys.exit(f"--jobs {args.jobs} needs {2 + 2 * args.jobs} CPUs, this machine has {os.cpu_count()}")
    if args.jobs > 1:
        os.sched_setaffinity(0, {0, 1})

    build_image()
    slots: Queue = Queue()
    for i in range(args.jobs):
        slots.put(Slot(i))
    smelt_binary = args.smelt_binary and args.smelt_binary.resolve()
    agents = args.agents.split(",")
    results: dict[str, list[dict]] = {agent: [] for agent in agents}

    with tempfile.TemporaryDirectory() as d:
        evict = Path(d, "evict.py")
        evict.write_text(EVICT)

        def job(agent: str) -> None:
            slot = slots.get()
            try:
                run = run_job(agent, args.mode, evict, smelt_binary, slot)
            finally:
                slots.put(slot)
            results[agent].append(run)
            print(f"done {agent} {len(results[agent])}/{args.runs}", file=sys.stderr, flush=True)

        with ThreadPoolExecutor(args.jobs) as pool:
            for future in [pool.submit(job, agent) for agent in agents for _ in range(args.runs)]:
                future.result()

    cold = args.mode == "cold-start"
    key = "pss" if cold else "disk"
    print(f"{'agent':15s}{'median MiB':>12s}{'q1':>8s}{'q3':>8s}" + ("  requests" if cold else ""))
    for agent, runs in results.items():
        q1, med, q3 = quartiles([r[key] / MIB for r in runs])
        requests = f"  {sum(1 for r in runs if r['requests'])}/{args.runs}"
        print(f"{agent:15s}{med:12.1f}{q1:8.1f}{q3:8.1f}" + (requests if cold else ""))
    if args.out:
        smelt = "local build" if smelt_binary else "release"
        meta = {"mode": args.mode, "date": date.today().isoformat(), "machine": machine(), "smelt": smelt}
        Path(args.out).write_text(json.dumps({**meta, "agents": results}, indent=1))


if __name__ == "__main__":
    main()
