---
title: "Benchmarks vs Claude Code, Codex and opencode"
description: "Terminal-Bench 2.1 results for smelt, pi, codex, claude code, opencode and mini-swe-agent on the same model: memory, CPU time and wall time over 360 runs."
---

# Benchmarks

Six coding agents on the same model and the same tasks: 20
[Terminal-Bench 2.1](https://github.com/harbor-framework/terminal-bench-2-1)
tasks, 3 attempts each, 360 runs in total. Every run had a fresh container
limited to 2 CPUs and 4 GiB. The model was Qwen3.8 27B (FP8) on vLLM. The study
finished on 2026-10-17 with a smelt build just after `v0.6.0`.

## Resources

Medians per run, with the interquartile range in parentheses.

| Agent          | Version           | Memory (GiB)         | CPU time (s)       | Wall time (s)     |
| -------------- | ----------------- | -------------------- | ------------------ | ----------------- |
| **smelt**      | v0.6.0-1-246e0f13 | **0.20** (0.16–0.32) | **6.1** (3.5–17.1) | **352** (126–915) |
| pi             | 1.3.0             | 0.71 (0.67–0.78)     | 9.7 (5.6–16.0)     | 399 (148–803)     |
| codex          | 0.162.1           | 2.09 (2.02–2.19)     | 9.8 (6.9–33.9)     | 372 (205–760)     |
| claude code    | 2.1.301           | 1.42 (1.23–1.53)     | 17.5 (8.6–33.8)    | 783 (264–975)     |
| opencode       | 1.19.1            | 2.80 (2.57–2.89)     | 116.0 (44.1–267.5) | 421 (185–857)     |
| mini-swe-agent | 2.4.6             | 1.49 (1.34–1.55)     | 13.1 (9.3–49.7)    | 570 (237–916)     |

Memory is the peak of the container's memory, page cache included. CPU time
covers everything in the container: the agent, its tools and the task itself.

## Over time

Memory and CPU in use at each moment on `regex-log`, a task every agent solved
in all three runs. Each row is one real run, the agent's median-length one,
sampled every second. The area is memory in use, on a scale shared by every
row, with the run's peak on the right. The strip below it is CPU in use: the
darker, the busier, up to the container's 2 cores. Hover for the values at any
moment.

<div class="profiles" data-profile="../../assets/bench/regex-log.json" role="img" aria-label="Memory and CPU in use over time on the regex-log task, a row per agent; smelt peaks at 0.17 GiB while the others reach 0.85 to 2.89 GiB" markdown="0"></div>
