#!/usr/bin/env python3
"""Rewrite the benchmark charts in the docs from saved `run.py --out` results.

    python3 charts.py results/2026-10-31

Reads `cold-start.json` and `install.json` from the results folder and rewrites:

- the bar charts in the benchmarks guide, between `<!-- chart:<mode> -->` and
  `<!-- /chart -->`. The bar is the median over runs, and the capped line spans
  the middle half (quartiles), left out when every run gave the same value.
- the bar charts on the homepage, between `<!-- bench:<mode> -->` and
  `<!-- /bench -->`, with the same rows as the guide.

smelt comes first, then the others by median.
"""

import json
import re
import statistics
import sys
from pathlib import Path

NAMES = {"claude-code": "claude code"}
KEY = {"cold-start": "pss", "install": "disk"}
MIB = 2**20
DOCS = Path(__file__).resolve().parents[2] / "docs"
GUIDE = DOCS / "docs/guide/benchmarks.md"
HOME = DOCS / "overrides/home.html"


def quartiles(values):
    if len(values) < 2:
        return (values[0],) * 3
    return statistics.quantiles(values, n=4, method="inclusive")


def medians(mode, results):
    """{agent: (q1, median, q3, runs)}, smelt first, then by median."""
    stats = {}
    for agent, runs in results["agents"].items():
        stats[NAMES.get(agent, agent)] = (*quartiles([r[KEY[mode]] / MIB for r in runs]), len(runs))
    return dict(sorted(stats.items(), key=lambda kv: (kv[0] != "smelt", kv[1][1])))


def runs_label(n):
    return f"{n} run" + ("s" if n != 1 else "")


def bars(mode, results):
    stats = medians(mode, results)
    top = max(q3 for _, _, q3, _ in stats.values())
    rows = []
    for agent, (q1, med, q3, n) in stats.items():
        me = ' class="me"' if agent == "smelt" else ""
        style = f"--v:{med / top * 100:.1f}%"
        title = f"{agent}: {med:.0f} MiB, {runs_label(n)}"
        if q1 != q3:
            style += f";--lo:{q1 / top * 100:.1f}%;--hi:{q3 / top * 100:.1f}%"
            title = f"{agent}: median {med:.0f} MiB, middle half {q1:.0f} to {q3:.0f} MiB, {runs_label(n)}"
        rows.append(f'      <li{me} title="{title}"><span>{agent}</span><i style="{style}"></i><b>{med:.0f} MiB</b></li>')
    return "\n".join(rows)


def home_bars(mode, results):
    """The same rows as the guide, indented for the homepage."""
    return "\n".join("    " + row for row in bars(mode, results).split("\n"))


def fill(text, marker, close, mode, body):
    pattern = re.compile(rf"(<!-- {marker}:{mode} -->\n).*?(\n?<!-- {close} -->)", re.S)
    text, count = pattern.subn(lambda m: m.group(1) + body + "\n" + m.group(2).lstrip("\n"), text)
    if count != 1:
        sys.exit(f"{marker}:{mode} not found")
    return text


def main(folder):
    results = {mode: json.loads(Path(folder, f"{mode}.json").read_text()) for mode in KEY}

    guide = GUIDE.read_text()
    for mode in KEY:
        pattern = re.compile(rf"(<!-- chart:{mode} -->\n.*?<ol>\n).*?(\n    </ol>)", re.S)
        guide, count = pattern.subn(lambda m: m.group(1) + bars(mode, results[mode]) + m.group(2), guide)
        if count != 1:
            sys.exit(f"chart:{mode} not found in {GUIDE}")
    GUIDE.write_text(guide)

    home = HOME.read_text()
    for mode in KEY:
        home = fill(home, "bench", "/bench", mode, home_bars(mode, results[mode]))
    HOME.write_text(home)


if __name__ == "__main__":
    main(*sys.argv[1:])
