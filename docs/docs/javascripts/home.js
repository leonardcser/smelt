// Homepage behavior: the install method tabs and the install command's copy button.
(function () {
  function initTabs() {
    var tabs = document.querySelectorAll(".installer__tabs [role=tab]");
    tabs.forEach(function (tab) {
      tab.addEventListener("click", function () {
        tabs.forEach(function (other) {
          var selected = other === tab;
          other.setAttribute("aria-selected", selected);
          document.getElementById(other.getAttribute("aria-controls")).hidden = !selected;
        });
      });
    });
  }

  function initCopy() {
    document.querySelectorAll("[data-copy]").forEach(function (button) {
      button.addEventListener("click", function () {
        navigator.clipboard.writeText(button.dataset.copy).then(
          function () {
            button.dataset.state = "copied";
            setTimeout(function () {
              delete button.dataset.state;
            }, 1600);
          },
          function () {},
        );
      });
    });
  }

  // One still ASCII field behind the whole page, shown through every
  // `.ascii` layer: islands of characters with empty space between them, each
  // denser and hotter toward its middle. Each layer samples the field at its
  // own place on the page, so the layers continue each other and a resize
  // keeps every island in place. The islands thin out down the page, from
  // dense around the hero to sparse at the bottom.
  function initFields() {
    var RAMP = ".::--==++**";
    var HEAT = ["#8a1c0c", "#c2360f", "#e8641a", "#f59a2a", "#ffc94a"];
    var DOTS = 0.49; // noise level where an island's fringe of dots starts; below it is empty
    var SHORE = 0.56; // where the island itself starts
    var THIN = 0.16; // how much both rise by the bottom of the page, leaving fewer, smaller islands

    // A per-lattice-point hash in [0, 1).
    function hash(x, y) {
      var n = Math.sin(x * 127.1 + y * 311.7) * 43758.5453;
      return n - Math.floor(n);
    }

    // Smooth value noise: lattice hashes blended with a smoothstep.
    function noise(x, y) {
      var x0 = Math.floor(x), y0 = Math.floor(y);
      var fx = x - x0, fy = y - y0;
      var sx = fx * fx * (3 - 2 * fx), sy = fy * fy * (3 - 2 * fy);
      var top = hash(x0, y0) + (hash(x0 + 1, y0) - hash(x0, y0)) * sx;
      var bottom = hash(x0, y0 + 1) + (hash(x0 + 1, y0 + 1) - hash(x0, y0 + 1)) * sx;
      return top + (bottom - top) * sy;
    }

    // Each line is runs of one color; empty cells stay uncolored spaces.
    function draw(el) {
      var probe = document.createElement("span");
      probe.textContent = "0123456789";
      el.replaceChildren(probe);
      var cw = probe.getBoundingClientRect().width / 10;
      var lh = parseFloat(getComputedStyle(el).lineHeight);
      var box = el.getBoundingClientRect();
      var left = Math.round((box.left + window.scrollX) / cw);
      var top = Math.round((box.top + window.scrollY) / lh);
      var cols = Math.ceil(el.clientWidth / cw);
      var rows = Math.ceil(el.clientHeight / lh);
      var pageRows = document.documentElement.scrollHeight / lh;
      var html = "";
      for (var y = top; y < top + rows; y++) {
        var thin = THIN * Math.min(1, y / pageRows);
        var dots = DOTS + thin, shore = SHORE + thin;
        var run = "", color = null;
        for (var x = left; x < left + cols; x++) {
          var v = 0.7 * noise(x / 14, y / 7) + 0.3 * noise(x / 5, y / 2.5);
          var heat = (v - shore) / (1 - shore);
          var ch = " ", c = null;
          if (v > dots && heat <= 0) {
            ch = ".";
            c = HEAT[0];
          } else if (heat > 0) {
            ch = RAMP[Math.min(RAMP.length - 1, Math.floor(heat * 2 * RAMP.length))];
            c = HEAT[Math.min(HEAT.length - 1, Math.floor(heat * 2 * HEAT.length))];
          }
          if (c !== color) {
            html += close(run, color);
            run = "";
            color = c;
          }
          run += ch;
        }
        html += close(run, color) + "\n";
      }
      el.innerHTML = html;
    }

    function close(run, color) {
      if (!run) return "";
      return color ? '<span style="color:' + color + '">' + run + "</span>" : run;
    }

    // The page above a layer can grow (replays fit their terminals), so a
    // layer redraws when the page does, not only when it resizes itself.
    var layers = document.querySelectorAll(".ascii");
    var redraw = function () {
      layers.forEach(draw);
    };
    new ResizeObserver(redraw).observe(document.querySelector(".home"));
  }

  initTabs();
  initCopy();
  initFields();
})();
