// Recorded smelt sessions (docs/shots), replayed as text in `[data-replay]` figures.
(function () {
  // The dark terminal palette, as the README GIF draws it (docs/shots/src/render.ts).
  var BASE16 = [
    "#000000", "#ef6b5b", "#8fbf6a", "#e6b450", "#6aa4e0", "#c586c0", "#56c2b0", "#e8dfd6",
    "#7d7268", "#ff8577", "#a8d98a", "#f5ca6e", "#8abaf2", "#dba1d6", "#7ad9c6", "#ffffff",
  ];
  // Its light counterpart, for terminals that ask for the light scheme with
  // `--term-scheme: light`. Other colors get their lightness flipped.
  var LIGHT16 = [
    "#3a2f27", "#c8372d", "#3f7d20", "#a86b00", "#1f5fae", "#9b3fa0", "#1b7f72", "#6b5348",
    "#8a7a6e", "#d9483d", "#4f9a2c", "#bf7d00", "#2f73c9", "#b256b8", "#2a9a8b", "#3a2f27",
  ];
  var ANSI16 = [
    [0, 0, 0], [205, 0, 0], [0, 205, 0], [205, 205, 0], [0, 0, 238], [205, 0, 205], [0, 205, 205], [229, 229, 229],
    [127, 127, 127], [255, 0, 0], [0, 255, 0], [255, 255, 0], [92, 92, 255], [255, 0, 255], [0, 255, 255], [255, 255, 255],
  ];
  var CUBE = [0, 95, 135, 175, 215, 255];
  var FG = "var(--term-fg)";
  var BG = "var(--term-bg)";
  var reducedMotion = window.matchMedia("(prefers-reduced-motion: reduce)").matches;

  function xtermRgb(n) {
    if (n < 16) return ANSI16[n];
    if (n >= 232) {
      var g = 8 + (n - 232) * 10;
      return [g, g, g];
    }
    n -= 16;
    return [CUBE[Math.floor(n / 36)], CUBE[Math.floor(n / 6) % 6], CUBE[n % 6]];
  }

  function rgb(c) {
    return "rgb(" + c[0] + "," + c[1] + "," + c[2] + ")";
  }

  // The same hue and saturation with the lightness mirrored.
  function flip(c) {
    var r = c[0] / 255, g = c[1] / 255, b = c[2] / 255;
    var max = Math.max(r, g, b), min = Math.min(r, g, b);
    var l = (max + min) / 2, d = max - min;
    var s = d === 0 ? 0 : d / (1 - Math.abs(2 * l - 1));
    var h = d === 0 ? 0 : max === r ? ((g - b) / d + 6) % 6 : max === g ? (b - r) / d + 2 : (r - g) / d + 4;
    var l2 = 1 - l;
    var a = s * Math.min(l2, 1 - l2);
    function f(n) {
      var k = (n + 2 * h) % 12;
      return Math.round(255 * (l2 - a * Math.max(-1, Math.min(k - 3, 9 - k, 1))));
    }
    return [f(0), f(8), f(4)];
  }

  function xterm256(n, light) {
    if (light) return rgb(flip(xtermRgb(n)));
    return n < 16 ? BASE16[n] : rgb(xtermRgb(n));
  }

  // Applies one SGR sequence to the pen. Recordings use only reset, bold, dim,
  // italic, and default, 256-color or truecolor foreground and background.
  function sgr(pen, params, light) {
    var base = light ? LIGHT16 : BASE16;
    var p = params === "" ? [0] : params.split(";").map(Number);
    for (var i = 0; i < p.length; i++) {
      var c = p[i];
      if (c === 0) (pen.fg = pen.bg = null), (pen.bold = pen.dim = pen.italic = false);
      else if (c === 1) pen.bold = true;
      else if (c === 2) pen.dim = true;
      else if (c === 3) pen.italic = true;
      else if (c === 22) pen.bold = pen.dim = false;
      else if (c === 23) pen.italic = false;
      else if (c === 39) pen.fg = null;
      else if (c === 49) pen.bg = null;
      else if (c >= 30 && c <= 37) pen.fg = base[c - 30];
      else if (c >= 90 && c <= 97) pen.fg = base[c - 82];
      else if (c >= 40 && c <= 47) pen.bg = base[c - 40];
      else if (c >= 100 && c <= 107) pen.bg = base[c - 92];
      else if (c === 38 || c === 48) {
        var color;
        if (p[i + 1] === 5) (color = xterm256(p[i + 2], light)), (i += 2);
        else if (p[i + 1] === 2) {
          var tc = [p[i + 2], p[i + 3], p[i + 4]];
          color = rgb(light ? flip(tc) : tc);
          i += 4;
        }
        if (c === 38) pen.fg = color;
        else pen.bg = color;
      }
    }
  }

  // Characters a terminal gives two cells (East Asian Wide, emoji presentation).
  var WIDE = [
    [0x1100, 0x115f], [0x231a, 0x231b], [0x23e9, 0x23ec], [0x23f0, 0x23f0], [0x23f3, 0x23f3],
    [0x25fd, 0x25fe], [0x2614, 0x2615], [0x2648, 0x2653], [0x267f, 0x267f], [0x2693, 0x2693],
    [0x26a1, 0x26a1], [0x26aa, 0x26ab], [0x26bd, 0x26be], [0x26c4, 0x26c5], [0x26ce, 0x26ce],
    [0x26d4, 0x26d4], [0x26ea, 0x26ea], [0x26f2, 0x26f3], [0x26f5, 0x26f5], [0x26fa, 0x26fa],
    [0x26fd, 0x26fd], [0x2705, 0x2705], [0x270a, 0x270b], [0x2728, 0x2728], [0x274c, 0x274c],
    [0x274e, 0x274e], [0x2753, 0x2755], [0x2757, 0x2757], [0x2795, 0x2797], [0x27b0, 0x27b0],
    [0x27bf, 0x27bf], [0x2b1b, 0x2b1c], [0x2b50, 0x2b50], [0x2b55, 0x2b55], [0x2e80, 0xa4cf],
    [0xac00, 0xd7a3], [0xf900, 0xfaff], [0xfe30, 0xfe4f], [0xff00, 0xff60], [0xffe0, 0xffe6],
    [0x1f300, 0x1f64f], [0x1f900, 0x1f9ff],
  ];
  // Nerd Font icons: the symbols font draws them one em wide, so they are sized to the cell.
  function isIcon(cp) {
    return (cp >= 0xe000 && cp <= 0xf8ff) || cp >= 0xf0000;
  }
  function cellWidth(cp) {
    for (var i = 0; i < WIDE.length; i++) if (cp >= WIDE[i][0] && cp <= WIDE[i][1]) return 2;
    return 1;
  }

  // Box-drawing arms (left, right, up, down), painted to the cell edges so lines
  // join across cells whatever the font does.
  var BOX = {
    0x2500: "lr", 0x2502: "ud", 0x250c: "rd", 0x2510: "ld", 0x2514: "ur", 0x2518: "ul",
    0x251c: "udr", 0x2524: "udl", 0x252c: "lrd", 0x2534: "lru", 0x253c: "lrud",
    0x256d: "rd", 0x256e: "ld", 0x2570: "ur", 0x256f: "ul",
  };
  var ARM = {
    l: "0 50% / calc(50% + var(--stroke) / 2) var(--stroke)",
    r: "100% 50% / calc(50% + var(--stroke) / 2) var(--stroke)",
    u: "50% 0 / var(--stroke) calc(50% + var(--stroke) / 2)",
    d: "50% 100% / var(--stroke) calc(50% + var(--stroke) / 2)",
  };
  function boxBackground(arms, fg, bg) {
    var layers = arms.split("").map(function (a) {
      return "linear-gradient(" + fg + "," + fg + ") " + ARM[a] + " no-repeat";
    });
    return layers.join(",") + "," + (bg || "transparent");
  }

  // Block elements painted as backgrounds, split on whole pixels so stacked
  // half blocks (the logo) tile without seams.
  function blockBackground(cp, fg, bg) {
    if (cp === 0x2580) return "linear-gradient(" + fg + " var(--half)," + bg + " var(--half))";
    if (cp === 0x2584) return "linear-gradient(" + bg + " var(--half)," + fg + " var(--half))";
    if (cp === 0x2588) return fg;
    if (cp === 0x258c) return "linear-gradient(to right," + fg + " 50%," + bg + " 50%)";
    if (cp === 0x2590) return "linear-gradient(to right," + bg + " 50%," + fg + " 50%)";
    // Powerline arrows, as two half-cell diagonals meeting at the tip.
    if (cp === 0xe0b0 || cp === 0xe0b2) {
      var side = cp === 0xe0b0 ? "right" : "left";
      return (
        "linear-gradient(to top " + side + "," + fg + " 50%," + bg + " 50%) 0 0 / 100% var(--half) no-repeat," +
        "linear-gradient(to bottom " + side + "," + fg + " 50%," + bg + " 50%) 0 100% / 100% calc(100% - var(--half)) no-repeat"
      );
    }
    return null;
  }

  // Kitty graphics Unicode placeholders: U+10EEEE cells whose foreground is the
  // image id, with combining marks for the image row and column they show.
  var PLACEHOLDER = 0x10eeee;
  var MARKS = [
    0x305, 0x30d, 0x30e, 0x310, 0x312, 0x33d, 0x33e, 0x33f, 0x346, 0x34a, 0x34b, 0x34c, 0x350, 0x351, 0x352, 0x357,
  ];
  function imageBackground(image, row, col, light) {
    return (
      "background:url(" + image.url + ") calc(" + -col + " * var(--cell)) calc(" + -row + " * var(--row)) / " +
      "calc(" + image.cols + " * var(--cell)) calc(" + image.rows + " * var(--row)) no-repeat;" +
      (light ? "filter:invert(1) hue-rotate(180deg);" : "")
    );
  }

  function styleOf(pen) {
    var fg = pen.fg || FG;
    var bg = pen.bg || BG;
    var css = "color:" + (pen.dim ? "color-mix(in srgb," + fg + " 50%," + bg + ")" : fg) + ";";
    if (pen.bg) css += "background:" + pen.bg + ";";
    if (pen.bold) css += "font-weight:700;";
    if (pen.italic) css += "font-style:italic;";
    return css;
  }

  function renderLine(line, light, images) {
    var row = document.createElement("div");
    var pen = { fg: null, bg: null, bold: false, dim: false, italic: false };
    var run = "";
    var runStyle = "";
    // Every span is sized in whole cells, so columns line up across rows
    // whatever the font's advance rounds to.
    function span(text, cells, css, cls) {
      var s = document.createElement("span");
      s.textContent = text;
      s.style.cssText = (css || "") + "width:calc(" + cells + " * var(--cell));";
      if (cls) s.className = cls;
      row.appendChild(s);
    }
    function flush() {
      if (run) span(run, run.length, runStyle);
      run = "";
    }
    // A run of horizontal rules is one span with one stroke, so it never
    // breaks up into dashes at cell edges.
    var rule = 0;
    var ruleStyle = "";
    var ruleFg = "";
    function flushRule() {
      if (!rule) return;
      var css = ruleStyle + "background:linear-gradient(" + ruleFg + "," + ruleFg + ") 0 50% / 100% var(--stroke) no-repeat;";
      span("\u2500".repeat(rule), rule, css, "x");
      rule = 0;
    }
    // The run of placeholder cells being drawn: its image, row, first column and length.
    var place = null;
    function flushPlace() {
      if (!place) return;
      var image = images && images[place.id];
      var css = image ? imageBackground(image, place.row, place.col, light) : "";
      span("", place.cells, css);
      place = null;
    }
    // Cells drawn since the background last changed.
    var bgCells = 0;
    var parts = line.split(/\x1b\[([0-9;]*)m/);
    for (var i = 0; i < parts.length; i++) {
      if (i % 2) {
        var prevBg = pen.bg;
        var id = parts[i].match(/^38;2;(\d+);(\d+);(\d+)$/);
        pen.image = id ? (+id[1] << 16) | (+id[2] << 8) | +id[3] : pen.image;
        sgr(pen, parts[i], light);
        if (pen.bg !== prevBg) bgCells = 0;
        continue;
      }
      var chars = Array.from(parts[i]);
      for (var c = 0; c < chars.length; c++) {
        var ch = chars[c];
        var cp = ch.codePointAt(0);
        if (cp === PLACEHOLDER) {
          flushRule();
          flush();
          var marks = [];
          while (c + 1 < chars.length && MARKS.indexOf(chars[c + 1].codePointAt(0)) >= 0) {
            marks.push(MARKS.indexOf(chars[++c].codePointAt(0)));
          }
          if (place && (marks.length || place.id !== pen.image)) flushPlace();
          if (!place) place = { id: pen.image, row: marks[0] || 0, col: marks[1] || 0, cells: 0 };
          place.cells++;
          bgCells++;
          continue;
        }
        flushPlace();
        bgCells++;
        var css = styleOf(pen);
        if (cp === 0x2500 && !pen.bg) {
          var fg0 = pen.dim ? "color-mix(in srgb," + (pen.fg || FG) + " 50%," + BG + ")" : pen.fg || FG;
          if (rule && (css !== ruleStyle || fg0 !== ruleFg)) flushRule();
          flush();
          ruleStyle = css;
          ruleFg = fg0;
          rule++;
          continue;
        }
        flushRule();
        if (cp <= 0x7e) {
          if (css !== runStyle) flush();
          runStyle = css;
          run += ch;
          continue;
        }
        // Every other character gets its own cell, so a fallback glyph with
        // a different advance cannot shift the columns after it.
        flush();
        var fg = pen.dim ? "color-mix(in srgb," + (pen.fg || FG) + " 50%," + (pen.bg || BG) + ")" : pen.fg || FG;
        var block = blockBackground(cp, fg, pen.bg || BG);
        if (block) span(ch, 1, css + "background:" + block + ";", "c x");
        else if (BOX[cp]) span(ch, 1, css + "background:" + boxBackground(BOX[cp], fg, pen.bg) + ";", "c x");
        else span(ch, cellWidth(cp), css, isIcon(cp) ? "c n" : "c");
      }
    }
    flushPlace();
    flushRule();
    flush();
    // A background switched on with no cells after it is what tmux records for
    // a line erased in that color: run it to the right edge, as the terminal
    // drew it. Drawn cells, the cursor among them, are always recorded.
    if (pen.bg && bgCells === 0) {
      var rest = document.createElement("span");
      rest.style.cssText = "background:" + pen.bg + ";width:100%;";
      row.appendChild(rest);
    }
    return row;
  }

  function Replay(el) {
    this.el = el;
    this.screen = el.querySelector(".term__screen");
    // The terminal window, which shrinks inside the figure when a recording resizes it.
    this.window = document.createElement("div");
    this.window.className = "term__window";
    this.screen.replaceWith(this.window);
    this.window.appendChild(this.screen);
    // Keys pressed in the frame, shown like a key-capture app.
    this.keys = document.createElement("div");
    this.keys.className = "term__keys";
    this.keys.setAttribute("aria-hidden", "true");
    this.window.appendChild(this.keys);
    this.frames = null;
    this.index = 0;
    this.timer = null;
    this.playing = false;
    this.visible = false;
  }

  Replay.prototype.load = function () {
    if (this.loading) return this.loading;
    var self = this;
    this.loading = fetch(this.el.dataset.replay)
      .then(function (r) {
        return r.json();
      })
      .then(function (data) {
        self.cols = data.cols;
        self.rows = data.rows;
        self.frames = data.frames;
        self.images = {};
        Object.keys(data.img || {}).forEach(function (id) {
          var image = data.img[id];
          self.images[id] = { url: "data:image/png;base64," + image.png, cols: image.cols, rows: image.rows };
        });
        self.total = data.frames.reduce(function (t, f) {
          return t + f.ms;
        }, 0);
        // Until it plays, show the finished session as the poster.
        self.index = self.frames.length - 1;
        self.fit();
        self.show(self.index, true);
      });
    return this.loading;
  };

  // Natural width of the recording's columns in this font at `size`.
  Replay.prototype.advance = function (size) {
    var probe = document.createElement("span");
    probe.textContent = "0".repeat(this.cols);
    probe.style.fontSize = size + "px";
    probe.style.letterSpacing = "0";
    this.screen.appendChild(probe);
    var width = probe.getBoundingClientRect().width;
    probe.remove();
    return width;
  };

  // Each cell is exactly the figure's width over the columns. Hinting makes a
  // font's advance drift from its size, so letter spacing takes up the
  // difference and text runs land on the same grid as everything else.
  Replay.prototype.fit = function () {
    if (!this.cols) return;
    var pad = getComputedStyle(this.el);
    var width = this.el.clientWidth - parseFloat(pad.paddingLeft) - parseFloat(pad.paddingRight);
    var cell = width / this.cols;
    var size = (100 * width) / this.advance(100);
    var spacing = cell - this.advance(size) / this.cols;
    var row = Math.round(size * 1.3);
    var s = this.screen.style;
    s.fontSize = size + "px";
    s.letterSpacing = spacing + "px";
    s.setProperty("--cell", cell + "px");
    s.setProperty("--row", row + "px");
    s.setProperty("--half", Math.round(row / 2) + "px");
    s.setProperty("--stroke", Math.max(1, Math.round(size * 0.08)) + "px");
    this.cell = cell;
    this.row = row;
    this.el.style.height = row * this.rows + "px";
    this.size = null;
    if (this.frames) this.resize(this.index);
  };

  // Sizes the window to frame `n`, which may be smaller than the recording.
  Replay.prototype.resize = function (n) {
    var f = this.frames[n];
    var size = (f.cols || this.cols) + "x" + (f.rows || this.rows);
    if (size === this.size) return;
    this.size = size;
    this.window.style.width = this.cell * (f.cols || this.cols) + "px";
    this.screen.style.height = this.row * (f.rows || this.rows) + "px";
  };

  // Draws frame `n`. Lines are stored as diffs, so a jump redraws from the start.
  Replay.prototype.show = function (n, jump) {
    var rows = this.screen.children;
    var light = getComputedStyle(this.el).getPropertyValue("--term-scheme").trim() === "light";
    var images = this.images;
    var render = function (line) {
      return renderLine(line, light, images);
    };
    if (jump) {
      var lines = [];
      for (var i = 0; i <= n; i++) {
        var l = this.frames[i].l;
        lines.length = l.length;
        for (var j = 0; j < l.length; j++) if (l[j] !== null) lines[j] = l[j];
      }
      this.screen.replaceChildren.apply(this.screen, lines.map(render));
    } else {
      var diff = this.frames[n].l;
      while (rows.length > diff.length) this.screen.lastChild.remove();
      for (var k = 0; k < diff.length; k++) {
        if (diff[k] === null && k < rows.length) continue;
        var row = render(diff[k] || "");
        if (k < rows.length) this.screen.replaceChild(row, rows[k]);
        else this.screen.appendChild(row);
      }
    }
    this.resize(n);
    var keys = this.frames[n].k || [];
    if (keys.join("\n") !== this.shownKeys) {
      this.shownKeys = keys.join("\n");
      this.keys.replaceChildren.apply(
        this.keys,
        keys.map(function (k) {
          var cap = document.createElement("kbd");
          cap.textContent = k;
          return cap;
        }),
      );
    }
    this.index = n;
  };

  Replay.prototype.tick = function () {
    var self = this;
    this.timer = setTimeout(function () {
      var next = self.index + 1;
      if (next >= self.frames.length) self.show(0, true);
      else self.show(next, false);
      self.tick();
    }, this.frames[this.index].ms);
  };

  Replay.prototype.play = function () {
    var self = this;
    this.load().then(function () {
      if (self.playing || reducedMotion || !self.visible || document.hidden) return;
      self.playing = true;
      if (self.index === self.frames.length - 1) self.show(0, true);
      self.tick();
    });
  };

  Replay.prototype.pause = function () {
    clearTimeout(this.timer);
    this.playing = false;
  };

  var replays = [];
  // Toggling the site's color scheme repaints the screens in the new palette.
  new MutationObserver(function () {
    replays.forEach(function (r) {
      if (r.frames) r.show(r.index, true);
    });
  }).observe(document.body, { attributes: true, attributeFilter: ["data-md-color-scheme"] });
  document.addEventListener("visibilitychange", function () {
    replays.forEach(function (r) {
      if (document.hidden) r.pause();
      else if (r.visible) r.play();
    });
  });

  function initReplays() {
    replays.forEach(function (r) {
      r.pause();
    });
    replays = [];
    var seen = new IntersectionObserver(
      function (entries) {
        entries.forEach(function (e) {
          var r = e.target.__replay;
          r.visible = e.isIntersecting;
          if (r.visible) r.play();
          else r.pause();
        });
      },
      { threshold: 0.1 },
    );
    var near = new IntersectionObserver(
      function (entries) {
        entries.forEach(function (e) {
          if (e.isIntersecting) e.target.__replay.load();
        });
      },
      { rootMargin: "400px" },
    );
    var resized = new ResizeObserver(function (entries) {
      entries.forEach(function (e) {
        e.target.__replay.fit();
      });
    });
    document.querySelectorAll("[data-replay]").forEach(function (el) {
      var r = (el.__replay = new Replay(el));
      replays.push(r);
      seen.observe(el);
      near.observe(el);
      resized.observe(el);
    });
  }

  if (window.document$) window.document$.subscribe(initReplays);
  else initReplays();
})();
