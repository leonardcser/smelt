"""Extract per-agent memory and CPU use over time for one task of the harbor study.

Usage: python3 scripts/bench_profile.py <study-dir> <task> <out.json>

Each agent's curve is one real run, the one with the median duration among its
runs of the task, sampled every second so peaks stay visible. Runs hit by
infrastructure problems are left out, as in the study.
"""

import csv
import gzip
import json
import sys
from collections import defaultdict

NAMES = {"claude-code": "claude code"}


def samples(rows):
    rows = [r for r in rows if r["elapsed"] >= 0]
    out = []
    for prev, row in zip(rows, rows[1:]):
        dt = row["time"] - prev["time"]
        if dt <= 0:
            continue
        cores = max(0.0, (row["cpu_s"] - prev["cpu_s"]) / dt)
        out.append((round(row["elapsed"]), round(row["memory_mib"] / 1024, 3), round(cores, 3)))
    return out


def main(study, task, out):
    with open(f"{study}/data/trials.csv") as f:
        usable = {row["trial"] for row in csv.DictReader(f) if row["outcome"] != "infrastructure"}
    with gzip.open(f"{study}/data/resource-profiles.json.gz") as f:
        profiles = json.load(f)["profiles"]

    runs = defaultdict(list)
    for trial, p in profiles.items():
        if trial in usable and p["metadata"]["task"] == task:
            runs[p["metadata"]["agent"]].append(samples(p["rows"]))

    series = {}
    for agent, agent_runs in sorted(runs.items()):
        agent_runs.sort(key=lambda s: s[-1][0])
        run = agent_runs[len(agent_runs) // 2]
        series[NAMES.get(agent, agent)] = {
            "t": [s[0] for s in run],
            "memory": [s[1] for s in run],
            "cores": [s[2] for s in run],
        }

    with open(out, "w") as f:
        json.dump({"task": task, "series": series}, f, separators=(",", ":"))


if __name__ == "__main__":
    main(*sys.argv[1:])
