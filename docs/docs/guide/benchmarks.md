---
title: "Benchmarks vs Claude Code, Codex and opencode"
description:
  "Startup time, memory, CPU and install size of smelt, pi, codex, claude code,
  opencode, omp and fx on the same scripted coding work."
---

# Benchmarks

smelt and six other coding agents on the same scripted work. Lower is better.
Median of 3 runs, measured on 2026-11-08.

<div class="bench" markdown="0">
  <div class="charts">
    <figure class="chart">
      <figcaption>install size</figcaption>
      <ol>
        <li><span>fx</span><i style="--v:2.4%;--lo:2.4%;--hi:2.4%"></i><b>12 MiB</b></li>
        <li class="me"><span>smelt</span><i style="--v:9.1%;--lo:9.1%;--hi:9.1%"></i><b>47 MiB</b></li>
        <li><span>opencode</span><i style="--v:47.9%;--lo:47.9%;--hi:47.9%"></i><b>247 MiB</b></li>
        <li><span>omp</span><i style="--v:52.6%;--lo:52.6%;--hi:52.6%"></i><b>271 MiB</b></li>
        <li><span>claude code</span><i style="--v:62.9%;--lo:62.9%;--hi:62.9%"></i><b>325 MiB</b></li>
        <li><span>pi</span><i style="--v:74.6%;--lo:74.6%;--hi:74.6%"></i><b>384 MiB</b></li>
        <li><span>codex</span><i style="--v:100.0%;--lo:100.0%;--hi:100.0%"></i><b>516 MiB</b></li>
      </ol>
    </figure>
    <figure class="chart">
      <figcaption>startup time</figcaption>
      <ol>
        <li><span>fx</span><i style="--v:0.8%;--lo:0.7%;--hi:0.8%"></i><b>7 ms</b></li>
        <li class="me"><span>smelt</span><i style="--v:3.9%;--lo:3.9%;--hi:4.1%"></i><b>36 ms</b></li>
        <li><span>codex</span><i style="--v:8.4%;--lo:8.4%;--hi:9.0%"></i><b>76 ms</b></li>
        <li><span>claude code</span><i style="--v:29.7%;--lo:29.4%;--hi:30.0%"></i><b>267 ms</b></li>
        <li><span>pi</span><i style="--v:32.0%;--lo:31.6%;--hi:32.3%"></i><b>288 ms</b></li>
        <li><span>opencode</span><i style="--v:91.1%;--lo:91.0%;--hi:100.0%"></i><b>820 ms</b></li>
        <li><span>omp</span><i style="--v:91.4%;--lo:91.3%;--hi:91.5%"></i><b>822 ms</b></li>
      </ol>
    </figure>
    <figure class="chart">
      <figcaption>memory at start</figcaption>
      <ol>
        <li><span>fx</span><i style="--v:1.4%;--lo:1.4%;--hi:1.4%"></i><b>6 MiB</b></li>
        <li class="me"><span>smelt</span><i style="--v:4.5%;--lo:4.5%;--hi:4.5%"></i><b>19 MiB</b></li>
        <li><span>pi</span><i style="--v:23.4%;--lo:23.2%;--hi:23.4%"></i><b>101 MiB</b></li>
        <li><span>codex</span><i style="--v:29.4%;--lo:29.3%;--hi:33.8%"></i><b>127 MiB</b></li>
        <li><span>claude code</span><i style="--v:40.4%;--lo:40.3%;--hi:40.4%"></i><b>174 MiB</b></li>
        <li><span>omp</span><i style="--v:66.1%;--lo:59.3%;--hi:71.5%"></i><b>285 MiB</b></li>
        <li><span>opencode</span><i style="--v:99.9%;--lo:94.7%;--hi:100.0%"></i><b>430 MiB</b></li>
      </ol>
    </figure>
    <figure class="chart">
      <figcaption>CPU on a task, 24 tool calls</figcaption>
      <ol>
        <li><span>pi</span><i style="--v:34.6%;--lo:34.5%;--hi:34.6%"></i><b>1.4 s</b></li>
        <li class="me"><span>smelt</span><i style="--v:41.7%;--lo:41.5%;--hi:41.8%"></i><b>1.6 s</b></li>
        <li><span>fx</span><i style="--v:47.0%;--lo:46.4%;--hi:47.1%"></i><b>1.9 s</b></li>
        <li><span>codex</span><i style="--v:65.8%;--lo:65.8%;--hi:66.4%"></i><b>2.6 s</b></li>
        <li><span>omp</span><i style="--v:72.9%;--lo:72.3%;--hi:74.6%"></i><b>2.9 s</b></li>
        <li><span>claude code</span><i style="--v:88.4%;--lo:87.8%;--hi:90.0%"></i><b>3.5 s</b></li>
        <li><span>opencode</span><i style="--v:99.9%;--lo:99.0%;--hi:100.0%"></i><b>3.9 s</b></li>
      </ol>
    </figure>
    <figure class="chart">
      <figcaption>CPU when idle, 5 minutes</figcaption>
      <ol>
        <li class="me"><span>smelt</span><i style="--v:0.0%;--lo:0.0%;--hi:0.0%"></i><b>0.5 ms</b></li>
        <li><span>codex</span><i style="--v:1.5%;--lo:1.4%;--hi:1.5%"></i><b>34 ms</b></li>
        <li><span>pi</span><i style="--v:3.8%;--lo:3.7%;--hi:5.0%"></i><b>89 ms</b></li>
        <li><span>claude code</span><i style="--v:21.3%;--lo:21.1%;--hi:21.5%"></i><b>504 ms</b></li>
        <li><span>omp</span><i style="--v:33.2%;--lo:33.1%;--hi:36.8%"></i><b>785 ms</b></li>
        <li><span>fx</span><i style="--v:35.9%;--lo:35.8%;--hi:36.0%"></i><b>849 ms</b></li>
        <li><span>opencode</span><i style="--v:51.5%;--lo:49.7%;--hi:100.0%"></i><b>1.2 s</b></li>
      </ol>
    </figure>
    <figure class="chart">
      <figcaption>memory after 64 turns</figcaption>
      <ol>
        <li class="me"><span>smelt</span><i style="--v:13.7%;--lo:13.6%;--hi:13.8%"></i><b>102 MiB</b></li>
        <li><span>pi</span><i style="--v:28.5%;--lo:27.2%;--hi:29.7%"></i><b>211 MiB</b></li>
        <li><span>fx</span><i style="--v:34.1%;--lo:32.5%;--hi:37.9%"></i><b>253 MiB</b></li>
        <li><span>codex</span><i style="--v:44.3%;--lo:44.0%;--hi:44.4%"></i><b>329 MiB</b></li>
        <li><span>omp</span><i style="--v:65.0%;--lo:64.0%;--hi:66.5%"></i><b>482 MiB</b></li>
        <li><span>claude code</span><i style="--v:67.0%;--lo:65.5%;--hi:68.7%"></i><b>497 MiB</b></li>
        <li><span>opencode</span><i style="--v:96.7%;--lo:93.2%;--hi:100.0%"></i><b>717 MiB</b></li>
      </ol>
    </figure>
  </div>
  <p class="legend"><i></i> median <i class="range"></i> range of runs</p>
</div>

## Method

A scripted model gives every agent the same work and checks each tool result.
The agents run for real, each alone in a container with 2 CPUs and 4 GiB, and
the host measures the container from outside. All runs use one Linux host.

Versions: smelt `0.8.0`, pi `1.5.0`, codex `0.167.0`, claude code `2.1.319`, opencode `2.2.6`, omp `18.18.0`, fx `0.0.16`.

Session disk use, CPU by streaming speed, the full method and the code to rerun
it are on [harness-resource-bench](https://leonardcser.github.io/harness-resource-bench).
