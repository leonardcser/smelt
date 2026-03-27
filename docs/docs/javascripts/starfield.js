(function () {
  var container = document.getElementById('starfield-container');
  if (!container) return;

  var pre = document.createElement('pre');
  pre.id = 'starfield-canvas';
  container.appendChild(pre);

  var COLS, ROWS;
  var buf;
  var animFrame;
  var lastTime = 0;

  var NUM_STARS = 2000;
  var stars = [];
  var streakChars = '  ..-=+*#@';

  function lerp(a, b, t) { return a + (b - a) * t; }

  function initStars() {
    stars.length = 0;
    for (var i = 0; i < NUM_STARS; i++) {
      stars.push({
        x: (Math.random() - 0.5) * 200,
        y: (Math.random() - 0.5) * 200,
        z: Math.random() * 100
      });
    }
  }

  function resize() {
    var w = container.clientWidth;
    var h = container.clientHeight;
    COLS = Math.floor(w / 10.8);
    ROWS = Math.floor(h / 21.6);
    buf = new Array(ROWS * COLS);
  }

  function render(dt) {
    buf.fill(' ');
    var cx = COLS / 2, cy = ROWS / 2;
    var speed = 25;

    for (var s = 0; s < stars.length; s++) {
      var star = stars[s];
      star.z -= speed * dt;
      if (star.z <= 0.5) {
        star.x = (Math.random() - 0.5) * 200;
        star.y = (Math.random() - 0.5) * 200;
        star.z = 80 + Math.random() * 20;
      }

      var sx = (star.x / star.z) * 60 + cx;
      var sy = (star.y / star.z) * 30 + cy;
      var brightness = 1 - star.z / 100;

      var col = Math.round(sx), row = Math.round(sy);
      if (col >= 0 && col < COLS && row >= 0 && row < ROWS) {
        var idx = row * COLS + col;
        var ci = Math.floor(brightness * (streakChars.length - 1));
        buf[idx] = streakChars[Math.max(0, ci)];
      }

      var pz = star.z + speed * dt * 2.5;
      var px = (star.x / pz) * 60 + cx;
      var py = (star.y / pz) * 30 + cy;
      var steps = Math.floor(Math.sqrt((sx - px) * (sx - px) + (sy - py) * (sy - py)));
      for (var i = 1; i < steps; i++) {
        var frac = i / steps;
        var ix = Math.round(lerp(px, sx, frac));
        var iy = Math.round(lerp(py, sy, frac));
        if (ix >= 0 && ix < COLS && iy >= 0 && iy < ROWS) {
          var tidx = iy * COLS + ix;
          if (buf[tidx] === ' ') {
            var b2 = brightness * frac * 0.5;
            var ci2 = Math.floor(b2 * (streakChars.length - 1));
            buf[tidx] = streakChars[Math.max(0, ci2)];
          }
        }
      }
    }

    var out = '';
    for (var r = 0; r < ROWS; r++) {
      for (var c = 0; c < COLS; c++) {
        out += buf[r * COLS + c];
      }
      if (r < ROWS - 1) out += '\n';
    }
    pre.textContent = out;
  }

  function loop(timestamp) {
    if (!lastTime) lastTime = timestamp;
    var dt = Math.min((timestamp - lastTime) / 1000, 0.05);
    lastTime = timestamp;
    render(dt);
    animFrame = requestAnimationFrame(loop);
  }

  var observer = new IntersectionObserver(function (entries) {
    if (entries[0].isIntersecting) {
      lastTime = 0;
      animFrame = requestAnimationFrame(loop);
    } else {
      cancelAnimationFrame(animFrame);
    }
  }, { threshold: 0.1 });

  resize();
  initStars();
  observer.observe(container);

  window.addEventListener('resize', function () {
    resize();
  });
})();
