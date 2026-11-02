---
title: "Benchmarks vs Claude Code, Codex and opencode"
description:
  "Memory and install size of smelt, pi, codex, claude code and opencode, and
  their memory and CPU on Terminal-Bench 2.1 tasks."
---

# Benchmarks

smelt against four other coding agents. Lower is better.

- **At start:** smelt uses 34 MiB, the others 110 to 328 MiB.
- **Install:** smelt writes 46 MiB, the others 313 to 1684 MiB.
- **On real tasks:** smelt peaks at 0.10 GiB, the others 0.14 to 0.71 GiB.

The first two you can
[rerun yourself](https://github.com/leonardcser/smelt/tree/main/benchmarks/memory)
on Linux with podman. They were measured on 2026-11-02 with smelt built from
`main` using the `dist` profile (`v0.6.5-46-b1b4ae4d7`), pi 1.5.0, codex
0.166.0, claude code 2.1.316 and opencode 2.2.5, each in its own container.

## Memory at start

Total memory of the agent's processes once it has started and sent its first
request. Median of 7 runs.

<div class="bench" markdown="0">
<!-- chart:cold-start -->
  <figure class="chart">
    <ol>
      <li class="me" title="smelt: median 34 MiB, middle half 33 to 36 MiB, 7 runs"><span>smelt</span><i style="--v:10.2%;--lo:10.1%;--hi:11.0%"></i><b>34 MiB</b></li>
      <li title="pi: median 110 MiB, middle half 106 to 120 MiB, 7 runs"><span>pi</span><i style="--v:33.1%;--lo:31.9%;--hi:36.1%"></i><b>110 MiB</b></li>
      <li title="codex: median 140 MiB, middle half 139 to 141 MiB, 7 runs"><span>codex</span><i style="--v:42.0%;--lo:41.8%;--hi:42.5%"></i><b>140 MiB</b></li>
      <li title="claude code: median 189 MiB, middle half 188 to 190 MiB, 7 runs"><span>claude code</span><i style="--v:56.7%;--lo:56.6%;--hi:57.3%"></i><b>189 MiB</b></li>
      <li title="opencode: median 328 MiB, middle half 327 to 332 MiB, 7 runs"><span>opencode</span><i style="--v:98.7%;--lo:98.5%;--hi:100.0%"></i><b>328 MiB</b></li>
    </ol>
  </figure>
<!-- /chart -->
<p class="legend"><i></i> median <i class="range"></i> middle half of runs</p>
</div>

This is an agent waiting for the model, not working on a task.

## Install size

Bytes each install writes to disk. smelt is one binary; the others install a
runtime and packages.

<div class="bench" markdown="0">
<!-- chart:install -->
  <figure class="chart">
    <ol>
      <li class="me" title="smelt: 46 MiB, 1 run"><span>smelt</span><i style="--v:2.7%"></i><b>46 MiB</b></li>
      <li title="claude code: 313 MiB, 1 run"><span>claude code</span><i style="--v:18.6%"></i><b>313 MiB</b></li>
      <li title="pi: 483 MiB, 1 run"><span>pi</span><i style="--v:28.7%"></i><b>483 MiB</b></li>
      <li title="codex: 853 MiB, 1 run"><span>codex</span><i style="--v:50.7%"></i><b>853 MiB</b></li>
      <li title="opencode: 1684 MiB, 1 run"><span>opencode</span><i style="--v:100.0%"></i><b>1684 MiB</b></li>
    </ol>
  </figure>
<!-- /chart -->
</div>

## Terminal-Bench study

Peak memory and CPU time on 20
[Terminal-Bench 2.1](https://github.com/harbor-framework/terminal-bench-2-1)
tasks, 3 attempts each, with Qwen3.8 27B. The study ran in October 2026 on a
private model server, so you cannot rerun it, and its versions are older: smelt
v0.6.0, pi 1.3.0, codex 0.162.1, claude code 2.1.301 and opencode 1.19.1. CPU
time includes the task's own commands. Median per run.

<div class="bench" markdown="0">
<div class="charts">
  <figure class="chart">
    <figcaption>memory</figcaption>
    <ol>
      <li class="me" title="smelt: median 0.10 GiB, middle half 0.08 to 0.14 GiB"><span>smelt</span><i style="--v:13.2%;--lo:10.8%;--hi:19.2%"></i><b>0.10 GiB</b></li>
      <li title="pi: median 0.14 GiB, middle half 0.13 to 0.16 GiB"><span>pi</span><i style="--v:18.5%;--lo:17.4%;--hi:21.7%"></i><b>0.14 GiB</b></li>
      <li title="codex: median 0.22 GiB, middle half 0.21 to 0.26 GiB"><span>codex</span><i style="--v:29.7%;--lo:28.2%;--hi:34.7%"></i><b>0.22 GiB</b></li>
      <li title="claude code: median 0.24 GiB, middle half 0.23 to 0.28 GiB"><span>claude code</span><i style="--v:32.6%;--lo:31.6%;--hi:37.8%"></i><b>0.24 GiB</b></li>
      <li title="opencode: median 0.71 GiB, middle half 0.68 to 0.74 GiB"><span>opencode</span><i style="--v:95.4%;--lo:91.9%;--hi:100.0%"></i><b>0.71 GiB</b></li>
    </ol>
  </figure>
  <figure class="chart">
    <figcaption>CPU time</figcaption>
    <ol>
      <li class="me" title="smelt: median 6.1 s, middle half 3.5 to 17.1 s"><span>smelt</span><i style="--v:2.3%;--lo:1.3%;--hi:6.4%"></i><b>6.1 s</b></li>
      <li title="pi: median 9.7 s, middle half 5.6 to 16 s"><span>pi</span><i style="--v:3.6%;--lo:2.1%;--hi:6.0%"></i><b>9.7 s</b></li>
      <li title="codex: median 9.8 s, middle half 6.9 to 33.9 s"><span>codex</span><i style="--v:3.7%;--lo:2.6%;--hi:12.7%"></i><b>9.8 s</b></li>
      <li title="claude code: median 17.5 s, middle half 8.6 to 33.8 s"><span>claude code</span><i style="--v:6.5%;--lo:3.2%;--hi:12.6%"></i><b>17.5 s</b></li>
      <li title="opencode: median 116 s, middle half 44.1 to 267.5 s"><span>opencode</span><i style="--v:43.4%;--lo:16.5%;--hi:100.0%"></i><b>116 s</b></li>
    </ol>
  </figure>
</div>
<p class="legend"><i></i> median <i class="range"></i> middle half of runs</p>
</div>

## Limits

- Every agent ran headless, so the cost of drawing a terminal interface is not
  included.
- One machine and one container image, with Node 22 for the Node agents.
