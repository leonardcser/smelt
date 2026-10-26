// One benchmark task over time, from the JSON written by scripts/bench_profile.py
// (one real run per agent, sampled every second) and named by `[data-profile]`.
// A row per agent on a shared time axis: memory in use as an area, CPU in use as
// a heat strip that darkens with the cores in use.
(function () {
  var NS = "http://www.w3.org/2000/svg";
  var CORES = 2;

  function el(name, attrs, parent) {
    var node = document.createElementNS(NS, name);
    for (var k in attrs) node.setAttribute(k, attrs[k]);
    if (parent) parent.appendChild(node);
    return node;
  }

  function text(parent, x, y, value, cls, anchor) {
    var node = el("text", { x: x, y: y, class: cls, "text-anchor": anchor || "start" }, parent);
    node.textContent = value;
    return node;
  }

  // Round ticks from 0, about `count` of them, the last at or above `max`.
  function ticks(max, count) {
    var raw = max / count;
    var mag = Math.pow(10, Math.floor(Math.log10(raw)));
    var step = [1, 2, 2.5, 5, 10].map(function (m) { return m * mag; }).find(function (s) { return s >= raw; });
    var out = [0];
    while (out[out.length - 1] < max) out.push(+(out.length * step).toFixed(6));
    return out;
  }

  // Index of the sample in sorted `ts` closest to `t`.
  function nearest(ts, t) {
    var lo = 0, hi = ts.length - 1;
    while (lo < hi) {
      var mid = (lo + hi) >> 1;
      if (ts[mid] < t) lo = mid + 1;
      else hi = mid;
    }
    return lo > 0 && t - ts[lo - 1] < ts[lo] - t ? lo - 1 : lo;
  }

  function last(xs) {
    return xs[xs.length - 1];
  }

  function clock(t) {
    return Math.floor(t / 60) + ":" + String(t % 60).padStart(2, "0");
  }

  // Agents with smelt first, then by peak memory.
  function order(data) {
    return Object.keys(data.series).sort(function (a, b) {
      if (a === "smelt") return -1;
      if (b === "smelt") return 1;
      return Math.max.apply(null, data.series[a].memory) - Math.max.apply(null, data.series[b].memory);
    });
  }

  function measure(svg, values, cls) {
    var probe = text(svg, 0, 0, "", cls);
    var width = 0;
    values.forEach(function (v) {
      probe.textContent = v;
      width = Math.max(width, probe.getComputedTextLength());
    });
    probe.remove();
    return width;
  }

  function linePath(s, x, y) {
    return s.t.map(function (t, i) { return (i ? "L" : "M") + x(t).toFixed(1) + "," + y(s.memory[i]).toFixed(1); }).join("");
  }

  function areaPath(s, x, y, base) {
    var b = base.toFixed(1);
    return linePath(s, x, y) + "L" + x(last(s.t)).toFixed(1) + "," + b + "L" + x(s.t[0]).toFixed(1) + "," + b + "Z";
  }

  function minuteTicks(svg, x, tMax, y) {
    for (var m = 0; m * 60 <= tMax; m += 5) text(svg, x(m * 60), y, m + "m", "tick", m ? "middle" : "start");
  }

  // A crosshair over the plot band and a tooltip listing `rows(t)`.
  function hover(root, svg, band, x, tMax, rows) {
    var cross = el("line", { y1: band.top, y2: band.bottom, class: "cross" }, svg);
    var tip = document.createElement("div");
    tip.className = "profile__tip";
    root.appendChild(tip);
    var hit = el("rect", { x: band.left, y: band.top, width: band.right - band.left, height: band.bottom - band.top, class: "hit" }, svg);

    function move(event) {
      var box = svg.getBoundingClientRect();
      var px = Math.min(Math.max(event.clientX - box.left, band.left), band.right);
      var t = Math.round(((px - band.left) / (band.right - band.left)) * tMax);
      var cx = x(t);
      cross.setAttribute("x1", cx);
      cross.setAttribute("x2", cx);
      cross.style.display = "";
      tip.replaceChildren();
      var head = document.createElement("div");
      head.textContent = clock(t);
      tip.appendChild(head);
      rows(t).forEach(function (row) {
        var line = document.createElement("div");
        line.textContent = row.text;
        if (row.me) line.className = "me";
        tip.appendChild(line);
      });
      tip.style.display = "block";
      var left = cx + 10;
      if (left + tip.offsetWidth > box.width) left = Math.max(0, cx - 10 - tip.offsetWidth);
      tip.style.left = left + "px";
    }

    function leave() {
      cross.style.display = "none";
      tip.style.display = "none";
    }

    hit.addEventListener("pointermove", move);
    hit.addEventListener("pointerdown", move);
    hit.addEventListener("pointerleave", leave);
    leave();
  }

  function pad(name, width) {
    return name + " ".repeat(Math.max(1, width - name.length));
  }

  function draw(root, data) {
    var names = order(data);
    var series = data.series;
    var width = root.clientWidth;
    var MEM = 26, GAP = 3, CPU = 9, ROW = MEM + GAP + CPU + 14;
    var height = names.length * ROW + 14;
    var svg = el("svg", { width: width, height: height }, root);
    var nameW = measure(svg, names, "label me");
    var peakW = measure(svg, ["0.00 GiB"], "tick");
    var P = { left: nameW + 12, right: peakW + 10 };
    var tMax = Math.max.apply(null, names.map(function (n) { return last(series[n].t); }));
    var yMax = last(ticks(Math.max.apply(null, names.map(function (n) { return Math.max.apply(null, series[n].memory); })), 3));
    var x = function (t) { return P.left + (t / tMax) * (width - P.left - P.right); };

    names.forEach(function (n, row) {
      var s = series[n];
      var me = n === "smelt";
      var top = row * ROW;
      var base = top + MEM;
      var y = function (v) { return base - (v / yMax) * MEM; };
      text(svg, 0, top + (MEM + GAP + CPU) / 2 + 3, n, me ? "label me" : "label");
      el("line", { x1: P.left, x2: width - P.right, y1: base, y2: base, class: "grid" }, svg);
      el("path", { d: areaPath(s, x, y, base), class: me ? "area me" : "area" }, svg);
      el("path", { d: linePath(s, x, y), class: me ? "line me" : "line" }, svg);
      var peak = Math.max.apply(null, s.memory);
      text(svg, width - P.right + 6, base, peak.toFixed(2) + " GiB", "tick");

      var cpuTop = base + GAP;
      el("rect", { x: x(0), y: cpuTop, width: x(last(s.t)) - x(0), height: CPU, class: "track" }, svg);
      // Merge neighbouring seconds of the same shade into one rect.
      var shade = function (i) { return Math.round(Math.min(1, s.cores[i] / CORES) * 20) / 20; };
      for (var i = 0; i < s.t.length; ) {
        var j = i + 1;
        var level = shade(i);
        while (j < s.t.length && shade(j) === level) j++;
        if (level > 0) {
          var x0 = x(s.t[i] - 1);
          el("rect", { x: x0, y: cpuTop, width: x(s.t[j - 1]) - x0, height: CPU, class: me ? "heat me" : "heat", "fill-opacity": level }, svg);
        }
        i = j;
      }
    });
    minuteTicks(svg, x, tMax, height - 2);

    var nameChars = Math.max.apply(null, names.map(function (n) { return n.length; })) + 2;
    hover(root, svg, { left: P.left, right: width - P.right, top: 0, bottom: height - 14 }, x, tMax, function (t) {
      return names.filter(function (n) { return t <= last(series[n].t); }).map(function (n) {
        var s = series[n];
        var i = nearest(s.t, t);
        return { me: n === "smelt", text: pad(n, nameChars) + s.memory[i].toFixed(2) + " GiB  " + s.cores[i].toFixed(2) + " cores" };
      });
    });
  }

  function init() {
    document.querySelectorAll("[data-profile]").forEach(function (root) {
      if (root.__profile) return;
      root.__profile = true;
      fetch(root.dataset.profile)
        .then(function (r) { return r.json(); })
        .then(function (data) {
          var width = 0;
          new ResizeObserver(function () {
            if (root.clientWidth === width) return;
            width = root.clientWidth;
            root.replaceChildren();
            draw(root, data);
          }).observe(root);
        });
    });
  }

  if (window.document$) window.document$.subscribe(init);
  else if (document.readyState === "loading") document.addEventListener("DOMContentLoaded", init);
  else init();
})();
