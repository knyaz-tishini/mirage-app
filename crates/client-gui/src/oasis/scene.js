// m1r4g3 oasis. One drawing function: drawDesert(ctx, t, { w, h, args }).
//
// Canvas subset of uzor-anim-js 1.5.11 only: save, restore, translate, rotate,
// scale, beginPath, moveTo, lineTo, quadraticCurveTo, bezierCurveTo,
// arc (no ccw flag), ellipse (no ccw flag), closePath, fill, stroke, fillRect,
// strokeRect, clip, fillStyle, strokeStyle, lineWidth, globalAlpha.
// Colors are #hex or rgba(). No gradients, text, images, shadows, caps, joins,
// dashes, composite ops. No Math.random: every mark is a hash of index and t.
//
// args[0] link 0..1          (0 = empty desert, 1 = oasis)
// args[1] wall-clock hour 0..24
// args[2] day of year 1..366
// args[3] DST hours 0|1      (wall clock ahead of standard time)
// args[4] glitch style 0..5  нарезка, двоение, всплытие, мерцание, жар, распад
// args[5] target link 0|1    (appear vs vanish)
//
// The oasis is drawn exactly once per frame. Glitches act on that single draw
// through a clip, a transform, per-layer offsets and cheap overlays.
//
// The camel, the rider and the palms are ink layers traced from 4.jpg (figures.js,
// veg.js: delta-encoded loops, filled with the same path ops). Load those two files
// before this one. Everything that moves is animated here: the neck bends to drink,
// crowns and the shawl sway in the wind, eyes blink, the pipe glows and smokes.

(function () {
  var LAT = (55 * Math.PI) / 180;
  var TAU = Math.PI * 2;
  var INK = "#16100c";
  // Base opacity of whatever is being drawn (the oasis during a fade); every alpha write multiplies it,
  // so nothing ever reads ctx.globalAlpha back.
  var BASE = 1;
  // Traced loops are 1 px polygons: straight segments are enough unless the frame is scaled up a lot.
  var SMOOTH = false;

  // ---------- small helpers ----------

  function clamp(v, a, b) {
    return v < a ? a : v > b ? b : v;
  }
  function lerp(a, b, t) {
    return a + (b - a) * t;
  }
  function smooth(a, b, x) {
    var u = clamp((x - a) / (b - a), 0, 1);
    return u * u * (3 - 2 * u);
  }
  function mix(a, b, t) {
    return [lerp(a[0], b[0], t), lerp(a[1], b[1], t), lerp(a[2], b[2], t)];
  }
  function rgba(c, a) {
    return "rgba(" + (c[0] | 0) + "," + (c[1] | 0) + "," + (c[2] | 0) + "," + (a == null ? 1 : +a.toFixed(3)) + ")";
  }
  function hash(n) {
    var x = ((n | 0) * 374761393 + 0x9e3779b9) >>> 0;
    x = Math.imul(x ^ (x >>> 13), 1274126177) >>> 0;
    return (x ^ (x >>> 16)) >>> 0;
  }
  function rnd(n) {
    return (hash(n) % 100000) / 100000;
  }
  function rectPath(ctx, x, y, w, h) {
    ctx.moveTo(x, y);
    ctx.lineTo(x + w, y);
    ctx.lineTo(x + w, y + h);
    ctx.lineTo(x, y + h);
    ctx.closePath();
  }
  function dot(ctx, x, y, r) {
    ctx.fillRect(x - r, y - r, r * 2, r * 2);
  }
  // Wind blows to the left. 0.35..1, gusty.
  function windAt(t) {
    return 0.62 + 0.25 * Math.sin(t * 0.31) + 0.13 * Math.sin(t * 1.13 + 1.7);
  }

  // ---------- sun by local clock ----------

  function solar(hour, day, dstHours) {
    var decl = ((23.44 * Math.PI) / 180) * Math.sin((TAU * (day - 81)) / 365);
    var cosHa = -Math.tan(LAT) * Math.tan(decl);
    var ha = Math.acos(clamp(cosHa, -1, 1));
    var half = (ha * 12) / Math.PI;
    var noon = 12 + (dstHours ? 1 : 0);
    var hAng = ((hour - noon) / 12) * Math.PI;
    var sinAlt = Math.sin(LAT) * Math.sin(decl) + Math.cos(LAT) * Math.cos(decl) * Math.cos(hAng);
    var alt = Math.asin(clamp(sinAlt, -1, 1));
    var altDeg = (alt * 180) / Math.PI;
    var noonAlt = 90 - 55 + (decl * 180) / Math.PI;
    return {
      alt: altDeg,
      noonAlt: noonAlt,
      decl: decl,
      noon: noon,
      // light level: 0 at the horizon, 1 at a high summer sun
      day: clamp(sinAlt / 0.62, 0, 1),
      // 1 near the horizon (golden hour / dusk)
      set: Math.exp(-Math.pow((altDeg - 1) / 8, 2)),
      night: clamp(-altDeg / 14, 0, 1),
      sunrise: noon - half,
      sunset: noon + half,
      morning: hour < noon,
    };
  }

  // ---------- sky ----------

  // Sky stops sampled from 4.jpg (day), warmer/redder at golden hour, indigo at night.
  // measured from 4.jpg, rows 0..440 of 492
  var SKY_DAY = [[0, [224, 77, 27]], [0.2, [217, 84, 28]], [0.3, [223, 110, 32]], [0.41, [226, 143, 41]], [0.51, [224, 170, 44]], [0.61, [223, 190, 50]], [0.71, [229, 210, 58]], [0.81, [228, 218, 60]], [1, [226, 222, 64]]];
  var SKY_SET = [[0, [112, 30, 44]], [0.3, [190, 56, 38]], [0.6, [232, 104, 40]], [0.85, [250, 156, 60]], [1, [255, 186, 90]]];
  var SKY_NT = [[0, [8, 10, 28]], [0.6, [20, 20, 46]], [1, [44, 32, 62]]];
  function stopAt(S, u) {
    for (var i = 1; i < S.length; i++) {
      if (u <= S[i][0]) return mix(S[i - 1][1], S[i][1], (u - S[i - 1][0]) / (S[i][0] - S[i - 1][0]));
    }
    return S[S.length - 1][1];
  }

  function sky(ctx, w, h, horizon, sun) {
    var gold = sun.set * (1 - sun.day * 0.7);
    var bands = 56;
    var bottom = horizon + h * 0.06;
    for (var i = 0; i < bands; i++) {
      var u = i / (bands - 1);
      var c = mix(stopAt(SKY_DAY, u), stopAt(SKY_SET, u), gold);
      // low winter sun: dimmer, dustier sky
      c = mix(c, mix([130, 60, 44], [190, 130, 80], u), (1 - sun.day) * (1 - sun.night) * 0.3);
      c = mix(c, stopAt(SKY_NT, u), sun.night);
      ctx.fillStyle = rgba(c);
      ctx.fillRect(0, (bottom * i) / bands, w, bottom / bands + 1);
    }
  }

  function stars(ctx, w, horizon, t, night) {
    if (night < 0.05) return;
    ctx.fillStyle = "#f6f1e4";
    for (var i = 0; i < 90; i++) {
      var tw = 0.55 + 0.45 * Math.sin(t * (1.2 + rnd(i + 40) * 2) + i);
      ctx.globalAlpha = BASE * clamp(night * tw * (0.35 + rnd(i + 70) * 0.65), 0, 1);
      var r = 0.6 + rnd(i + 90) * 1.2;
      dot(ctx, rnd(i + 11) * w, rnd(i + 300) * horizon * 0.85, r);
    }
    ctx.globalAlpha = BASE;
  }

  // Poster scale for any window. The camel barrel stays on the vertical
  // center. The rider's head (poster y 9) always stays inside the top edge.
  // Width only limits the scale; sky above and sand below are whatever is left.
  function figureScale(w, h) {
    var head = 9;
    var body = 540;
    var margin = Math.max(4, h * 0.012);
    var fitW = w / 760;
    var fitHead = (h * 0.5 - margin) / (body - head);
    var s = Math.min(fitW, fitHead);
    return s > 0.02 ? s : fitW;
  }

  function sunMoon(ctx, w, h, horizon, hour, sun) {
    var R = Math.min(w * 0.13, h * 0.16);
    // Back line of the rider, same poster offset on every window.
    var sFig = figureScale(w, h);
    var backY = horizon + (240 - 438) * sFig;
    // The disc center runs off the left edge, across the whole window, and off
    // the right edge. The span is this window's width, not a fixed inset.
    var across = function (radius, t) {
      var u = clamp(t, 0, 1);
      return -radius + (w + radius * 2) * u;
    };
    if (sun.alt > -5) {
      var span = Math.max(0.5, sun.sunset - sun.sunrise);
      var u = (hour - sun.sunrise) / span;
      var sx = across(R, u);
      var sy = backY;
      var a = clamp((sun.alt + 5) / 6, 0, 1);
      var halo = mix([250, 240, 190], [255, 170, 100], sun.set);
      // soft glow: many faint discs, the print's blurry pale sun (4.jpg, 5.jpg)
      for (var k = 14; k >= 1; k--) {
        ctx.globalAlpha = BASE * clamp(a * 0.04, 0, 1);
        ctx.fillStyle = rgba(halo);
        ctx.beginPath();
        ctx.arc(sx, sy, R * (1 + k * 0.05), 0, TAU);
        ctx.fill();
      }
      ctx.globalAlpha = BASE * clamp(a * 0.88, 0, 1);
      ctx.fillStyle = rgba(mix([238, 226, 182], [255, 200, 140], sun.set * 0.8));
      ctx.beginPath();
      ctx.arc(sx, sy, R, 0, TAU);
      ctx.fill();
      // mottled, dusty disc
      ctx.globalAlpha = BASE;
      specks(ctx, 240, 4100, sx - R, sy - R, sx + R, sy + R, rgba(mix([214, 190, 120], [230, 140, 80], sun.set)), a * 0.35, 0.6, R * 0.03, function (u, v) {
        var dx = u - 0.5;
        var dy = v - 0.5;
        return dx * dx + dy * dy < 0.25 ? 1 : 0;
      });
      specks(ctx, 200, 4500, sx - R * 1.4, sy - R * 1.4, sx + R * 1.4, sy + R * 1.4, rgba(halo), a * 0.45, 0.6, R * 0.02, function (u, v) {
        var dx = u - 0.5;
        var dy = v - 0.5;
        var d = dx * dx + dy * dy;
        return d > 0.11 && d < 0.2 ? 1 : 0;
      });
    }
    if (sun.alt < 2) {
      // moon opposite the sun
      var mu = ((hour - sun.sunset + 24) % 24) / Math.max(1, 24 - (sun.sunset - sun.sunrise));
      var mr = R * 0.38;
      var mx = across(mr, mu);
      var my = backY;
      ctx.globalAlpha = BASE * clamp(clamp((2 - sun.alt) / 8, 0, 0.95), 0, 1);
      ctx.fillStyle = "#ece6d2";
      ctx.beginPath();
      ctx.arc(mx, my, R * 0.38, 0, TAU);
      ctx.fill();
      ctx.fillStyle = "rgba(170,160,140,0.6)";
      ctx.beginPath();
      ctx.arc(mx - R * 0.1, my - R * 0.06, R * 0.08, 0, TAU);
      ctx.arc(mx + R * 0.12, my + R * 0.1, R * 0.05, 0, TAU);
      ctx.fill();
      ctx.globalAlpha = BASE;
    }
  }

  // ---------- dunes ----------

  // A dune row is a few sharp ridges. y grows downward.
  function ridgeY(peaks, x, base) {
    var y = base;
    for (var i = 0; i < peaks.length; i++) {
      var p = peaks[i];
      var d = x - p.x;
      var u = d < 0 ? -d / p.wl : d / p.wr;
      if (u < 1) {
        var v = 1 - u;
        y = Math.min(y, base - p.a * v * v * (3 - 2 * v) * (0.7 + 0.3 * v));
      }
    }
    return y;
  }

  function makePeaks(seed, n, w, amp, width) {
    var out = [];
    for (var i = 0; i < n; i++) {
      var x = ((i + 0.2 + rnd(seed + i * 3) * 0.6) / n) * w * 1.2 - w * 0.1;
      out.push({
        x: x,
        a: amp * (0.6 + rnd(seed + i * 5 + 1) * 0.6),
        wl: width * (0.7 + rnd(seed + i * 7 + 2) * 0.6),
        wr: width * (1.1 + rnd(seed + i * 11 + 3) * 0.8),
      });
    }
    return out;
  }

  function duneRow(ctx, w, h, base, peaks, lit, shade, inkA, stip, seed) {
    var steps = 90;
    var i, x;
    ctx.beginPath();
    ctx.moveTo(0, h);
    for (i = 0; i <= steps; i++) {
      x = (w * i) / steps;
      ctx.lineTo(x, ridgeY(peaks, x, base));
    }
    ctx.lineTo(w, h);
    ctx.closePath();
    ctx.fillStyle = rgba(lit);
    ctx.fill();
    // leeward shadows: sweeping crescents right of every crest (as in 5.jpg)
    ctx.fillStyle = rgba(shade);
    for (var k = 0; k < peaks.length; k++) {
      var p = peaks[k];
      var py = base - p.a;
      ctx.beginPath();
      ctx.moveTo(p.x, py);
      var x1 = p.x + p.wr;
      var mid = ridgeY(peaks, p.x + p.wr * 0.5, base);
      ctx.quadraticCurveTo(p.x + p.wr * 0.45, mid - p.a * 0.05, x1, base + 1);
      ctx.quadraticCurveTo(p.x + p.wr * 0.35, base + p.a * 0.35, p.x - p.wl * 0.12, base + p.a * 0.18);
      ctx.quadraticCurveTo(p.x + p.wr * 0.05, base - p.a * 0.4, p.x, py);
      ctx.closePath();
      ctx.fill();
    }
    // stipple
    ctx.fillStyle = rgba(shade);
    for (i = 0; i < stip; i++) {
      x = rnd(seed + i * 13) * w;
      var top = ridgeY(peaks, x, base);
      var y = top + rnd(seed + i * 17 + 5) * (base - top + h * 0.05);
      ctx.globalAlpha = BASE * clamp(0.5, 0, 1);
      dot(ctx, x, y, 0.8 + rnd(seed + i) * 1.4);
    }
    ctx.globalAlpha = BASE * clamp(inkA, 0, 1);
    ctx.strokeStyle = INK;
    ctx.lineWidth = Math.max(1, h * 0.0022);
    ctx.beginPath();
    for (i = 0; i <= steps; i++) {
      x = (w * i) / steps;
      var yy = ridgeY(peaks, x, base);
      if (i === 0) ctx.moveTo(x, yy);
      else ctx.lineTo(x, yy);
    }
    ctx.stroke();
    // a few ridge lines running down the slip faces
    ctx.lineWidth = Math.max(0.8, h * 0.0015);
    ctx.beginPath();
    for (k = 0; k < peaks.length; k++) {
      p = peaks[k];
      if (rnd(seed + k * 31) < 0.4) continue;
      ctx.moveTo(p.x, base - p.a);
      ctx.quadraticCurveTo(p.x + p.wr * 0.12, base - p.a * 0.4, p.x + p.wr * 0.05, base + p.a * 0.15);
    }
    ctx.stroke();
    ctx.globalAlpha = BASE;
  }

  // Sand lifted off the crests and streaming along the ground.
  function windSand(ctx, w, h, horizon, t, rows, light) {
    var wind = windAt(t);
    var col = rgba(mix([150, 110, 90], [250, 214, 150], light));
    ctx.fillStyle = col;
    ctx.strokeStyle = col;
    var r, k, i, p;
    for (r = 0; r < rows.length; r++) {
      var row = rows[r];
      for (k = 0; k < row.peaks.length; k++) {
        p = row.peaks[k];
        if (p.x < -50 || p.x > w + 50) continue;
        var py = row.base - p.a;
        for (i = 0; i < 9; i++) {
          var ph = (t * (0.35 + wind * 0.4) + i / 9 + rnd(k * 17 + r)) % 1;
          var dx = -ph * p.wl * (0.9 + wind) * 0.9;
          var dy = -Math.sin(ph * Math.PI) * p.a * 0.18 * wind + ph * ph * p.a * 0.25;
          ctx.globalAlpha = BASE * clamp((1 - ph) * 0.55 * wind, 0, 1);
          dot(ctx, p.x + dx, py + dy, 0.7 + (1 - ph) * 1.1 * row.scale);
        }
      }
    }
    // streaks over the ground
    ctx.lineWidth = Math.max(1, h * 0.0016);
    for (i = 0; i < 70; i++) {
      var y = horizon + (0.05 + rnd(i + 21) * 0.95) * (h - horizon);
      var depth = (y - horizon) / (h - horizon);
      var speed = (0.12 + 0.3 * depth) * w * (0.5 + wind);
      var span = w * 1.3;
      var x = w * 1.15 - ((rnd(i + 7) * span + t * speed) % span);
      var len = (10 + rnd(i + 3) * 40) * (0.4 + depth) * wind;
      ctx.globalAlpha = BASE * clamp((0.12 + rnd(i + 5) * 0.25) * wind * (0.4 + light * 0.6), 0, 1);
      ctx.beginPath();
      ctx.moveTo(x, y);
      ctx.quadraticCurveTo(x + len * 0.5, y - 2 - Math.sin(t * 3 + i) * 2, x + len, y + Math.sin(t * 2 + i));
      ctx.stroke();
    }
    ctx.globalAlpha = BASE;
  }

  // ---------- oasis pieces (local units: ground at y=0, ~900 units tall view) ----------

  function smoothLine(ctx, P, move) {
    if (move) ctx.moveTo(P[0][0], P[0][1]);
    else ctx.lineTo(P[0][0], P[0][1]);
    for (var i = 1; i < P.length - 1; i++) {
      ctx.quadraticCurveTo(P[i][0], P[i][1], (P[i][0] + P[i + 1][0]) / 2, (P[i][1] + P[i + 1][1]) / 2);
    }
    ctx.lineTo(P[P.length - 1][0], P[P.length - 1][1]);
  }


  // ---------- palms ----------




  // ---------- shared drawing helpers (reference units: 4.jpg pixels, 1120 x 912) ----------

  // Closed smooth outline through points (quadratic through midpoints).
  function smoothClosed(ctx, P) {
    var n = P.length;
    ctx.beginPath();
    ctx.moveTo((P[n - 1][0] + P[0][0]) / 2, (P[n - 1][1] + P[0][1]) / 2);
    for (var i = 0; i < n; i++) {
      var q = P[(i + 1) % n];
      ctx.quadraticCurveTo(P[i][0], P[i][1], (P[i][0] + q[0]) / 2, (P[i][1] + q[1]) / 2);
    }
    ctx.closePath();
  }

  // Many round specks as one path, one fill. bias(u, v) is the keep-probability.
  function specks(ctx, n, seed, x0, y0, x1, y1, color, alpha, r0, r1, bias) {
    ctx.beginPath();
    for (var i = 0; i < n; i++) {
      var u = rnd(seed + i * 13);
      var v = rnd(seed + i * 29 + 7);
      if (bias && rnd(seed + i * 41 + 3) > bias(u, v)) continue;
      var x = x0 + u * (x1 - x0);
      var y = y0 + v * (y1 - y0);
      var r = r0 + (r1 - r0) * rnd(seed + i * 5) * rnd(seed + i * 7 + 1);
      ctx.moveTo(x + r, y);
      if (rnd(seed + i * 3 + 2) < 0.4) ctx.ellipse(x, y, r * 1.6, r * 0.7, rnd(seed + i) * 3, 0, TAU);
      else ctx.arc(x, y, r, 0, TAU);
    }
    ctx.fillStyle = color;
    ctx.globalAlpha = BASE * clamp(alpha, 0, 1);
    ctx.fill();
    ctx.globalAlpha = BASE;
  }


  // Short strokes running along a direction (fur / form lines), one stroke call.
  function streaks(ctx, n, seed, x0, y0, x1, y1, ang, len, color, lw, alpha, bias) {
    var cx = Math.cos(ang);
    var cy = Math.sin(ang);
    ctx.beginPath();
    for (var i = 0; i < n; i++) {
      var u = rnd(seed + i * 17);
      var v = rnd(seed + i * 23 + 5);
      if (bias && rnd(seed + i * 31 + 9) > bias(u, v)) continue;
      var x = x0 + u * (x1 - x0);
      var y = y0 + v * (y1 - y0);
      var L = len * (0.4 + 0.6 * rnd(seed + i * 3));
      var b = (rnd(seed + i * 11) - 0.5) * L * 0.4;
      ctx.moveTo(x, y);
      ctx.quadraticCurveTo(x + cx * L * 0.5 - cy * b, y + cy * L * 0.5 + cx * b, x + cx * L, y + cy * L);
    }
    ctx.strokeStyle = color;
    ctx.lineWidth = lw;
    ctx.globalAlpha = BASE * clamp(alpha, 0, 1);
    ctx.stroke();
    ctx.globalAlpha = BASE;
  }

  // ---------- pool ----------

  var POOL_PTS = [
    [30, 612], [80, 600], [180, 598], [300, 596], [450, 594], [600, 592], [760, 588], [900, 590], [1010, 600],
    [1058, 628], [1064, 668], [1040, 708], [990, 738], [900, 768], [800, 790], [700, 800], [580, 798], [460, 792],
    [340, 780], [230, 762], [150, 744], [90, 724], [40, 712], [14, 690], [10, 650],
  ];
  function poolPath(ctx, grow) {
    var P = [];
    for (var i = 0; i < POOL_PTS.length; i++) {
      var p = POOL_PTS[i];
      P.push([580 + (p[0] - 580) * (1 + grow / 470), 695 + (p[1] - 695) * (1 + grow / 105)]);
    }
    smoothClosed(ctx, P);
  }

  // Back palms that stand on the far bank, mirrored in the water.
  function palmReflection(ctx, x, top, len, t, i) {
    var wob = function (k) { return Math.sin(t * 2.1 + k * 1.4 + i) * 3; };
    ctx.beginPath();
    ctx.moveTo(x - 5, top);
    ctx.lineTo(x + 5, top);
    ctx.lineTo(x + 4 + wob(1), top + len * 0.5);
    ctx.lineTo(x + 3 + wob(2), top + len);
    ctx.lineTo(x - 3 + wob(2), top + len);
    ctx.lineTo(x - 4 + wob(1), top + len * 0.5);
    ctx.closePath();
    // inverted crown: a burst of drooping blades
    var cx = x + wob(3);
    var cy = top + len;
    for (var k = 0; k < 13; k++) {
      var a = Math.PI / 2 + (k / 12 - 0.5) * 3.4;
      var L = len * (0.45 + rnd(i * 31 + k) * 0.3);
      var ex = cx + Math.cos(a) * L;
      var ey = cy + Math.sin(a) * L * 0.75 - Math.abs(Math.cos(a)) * L * 0.35;
      var nx = -Math.sin(a) * 4;
      var ny = Math.cos(a) * 4;
      ctx.moveTo(cx + nx, cy + ny);
      ctx.quadraticCurveTo((cx + ex) / 2 + wob(k), (cy + ey) / 2 + 6, ex, ey);
      ctx.quadraticCurveTo((cx + ex) / 2 + wob(k), (cy + ey) / 2 + 10, cx - nx, cy - ny);
      ctx.closePath();
    }
    ctx.fill();
  }

  function pool(ctx, t, look, drink, muzzle) {
    // wet bank, darker sand ring
    poolPath(ctx, 7);
    ctx.fillStyle = rgba(look.bank, 0.6);
    ctx.fill();
    ctx.save();
    poolPath(ctx, 0);
    ctx.clip();
    // the water mirrors the sky: deep blue at both edges, a yellow-green band in the middle (4.jpg)
    var stops = look.bands;
    for (var i = 0; i < stops.length - 1; i++) {
      var y0 = stops[i][0];
      var y1 = stops[i + 1][0];
      for (var k = 0; k < 10; k++) {
        ctx.fillStyle = rgba(mix(stops[i][1], stops[i + 1][1], k / 10));
        ctx.fillRect(100, y0 + ((y1 - y0) * k) / 10, 980, (y1 - y0) / 10 + 1);
      }
    }
    // reflections of the far-bank palms and the trunks
    ctx.fillStyle = rgba(look.refl, 0.62);
    palmReflection(ctx, 730, 590, 150, t, 1);
    palmReflection(ctx, 930, 592, 110, t, 2);
    palmReflection(ctx, 300, 596, 120, t, 3);
    palmReflection(ctx, 225, 598, 170, t, 4);
    palmReflection(ctx, 100, 604, 110, t, 5);
    // the camel's legs in the water, faint
    ctx.fillStyle = rgba(look.refl, 0.25);
    ctx.fillRect(316, 590, 50, 70);
    ctx.fillRect(505, 590, 60, 40);
    // ripple glints drifting with the wind
    ctx.strokeStyle = rgba(look.glint);
    ctx.lineWidth = 2;
    for (i = 0; i < 34; i++) {
      var ry = 600 + rnd(i + 3) * 200;
      var span = 1000;
      var rx = 1080 - ((rnd(i + 8) * span + t * 14 * (0.5 + rnd(i))) % span);
      var len = 10 + rnd(i + 11) * 34;
      ctx.globalAlpha = BASE * clamp(0.3 + 0.25 * Math.sin(t * 1.7 + i), 0, 1);
      ctx.beginPath();
      ctx.moveTo(rx, ry);
      ctx.quadraticCurveTo(rx + len / 2, ry - 2, rx + len, ry);
      ctx.stroke();
    }
    ctx.globalAlpha = BASE;
    // grain on the water
    specks(ctx, 700, 880, 100, 585, 1060, 810, "#02101c", 0.3, 0.5, 2.4, null);
    specks(ctx, 260, 980, 100, 585, 1060, 810, "#e8f0c0", 0.18, 0.5, 1.8, null);
    // drinking rings
    if (drink > 0.6) {
      ctx.strokeStyle = rgba(look.glint);
      ctx.lineWidth = 2;
      for (k = 0; k < 3; k++) {
        var rp = (t * 0.7 + k / 3) % 1;
        ctx.globalAlpha = BASE * clamp((1 - rp) * (drink - 0.6) * 2.2, 0, 1);
        ctx.beginPath();
        ctx.ellipse(muzzle[0], muzzle[1] + 3, 8 + rp * 60, 2.5 + rp * 11, 0, 0, TAU);
        ctx.stroke();
      }
      ctx.globalAlpha = BASE;
    }
    ctx.restore();
    poolPath(ctx, 0);
    ctx.strokeStyle = INK;
    ctx.lineWidth = 2.6;
    ctx.stroke();
  }

  // ---------- traced figures (figures.js): camel and rider ink layers from 4.jpg ----------

  var FIGS = null;
  function decodeLoop(str) {
    var out = [];
    var prev = [0, 0];
    var i = 0;
    var k = 0;
    while (i < str.length) {
      var d;
      if (str.charCodeAt(i) === 126) {
        d = parseInt(str.substr(i + 1, 5), 10);
        i += 6;
      } else {
        d = str.charCodeAt(i) - 80;
        i += 1;
      }
      prev[k % 2] += d;
      out.push(prev[k % 2]);
      k++;
    }
    return out;
  }
  function decodeLayers(list) {
    return list.map(function (layer) {
      return { c: layer.c, p: layer.p.map(decodeLoop) };
    });
  }
  function figures() {
    if (FIGS) return FIGS;
    var src = root.m1r4g3Figures;
    if (!src) return null;
    FIGS = {};
    for (var name in src) FIGS[name] = decodeLayers(src[name]);
    return FIGS;
  }
  var VEG = null;
  function vegetation() {
    if (VEG) return VEG;
    var src = root.m1r4g3Veg;
    if (!src) return null;
    VEG = {
      palms: decodeLayers(src.palms),
      crowns: src.crowns.map(function (c) {
        return { pivot: c.pivot, layers: decodeLayers(c.layers) };
      }),
    };
    return VEG;
  }

  // Traced palms (veg.js): trunks and tufts stand still, every crown sways about its top in the wind.
  function tracedPalms(ctx, t) {
    var V = vegetation();
    if (!V) return false;
    var wind = windAt(t);
    drawFig(ctx, V.palms);
    for (var i = 0; i < V.crowns.length; i++) {
      var c = V.crowns[i];
      var a = -wind * 0.02 + Math.sin(t * (0.8 + i * 0.07) + i * 1.7) * 0.022 * wind + Math.sin(t * 2.9 + i) * 0.006;
      ctx.save();
      ctx.translate(c.pivot[0], c.pivot[1]);
      ctx.rotate(a);
      ctx.translate(-c.pivot[0], -c.pivot[1]);
      drawFig(ctx, c.layers);
      ctx.restore();
    }
    return true;
  }
  // One fill per tone layer; loops are closed with quadratic midpoints so the trace reads as ink, not pixels.
  function drawFig(ctx, layers) {
    for (var l = 0; l < layers.length; l++) {
      var loops = layers[l].p;
      ctx.beginPath();
      for (var j = 0; j < loops.length; j++) {
        var P = loops[j];
        var n = P.length / 2;
        var lx = P[2 * n - 2];
        var ly = P[2 * n - 1];
        ctx.moveTo((lx + P[0]) / 2, (ly + P[1]) / 2);
        for (var i = 0; i < n; i++) {
          var x = P[2 * i];
          var y = P[2 * i + 1];
          var nx = P[(2 * i + 2) % (2 * n)];
          var ny = P[(2 * i + 3) % (2 * n)];
          if (SMOOTH) ctx.quadraticCurveTo(x, y, (x + nx) / 2, (y + ny) / 2);
          else ctx.lineTo(x, y);
        }
        ctx.closePath();
      }
      ctx.fillStyle = layers[l].c;
      ctx.fill();
    }
  }

  // ---------- camel pose ----------

  var DRINK_PERIOD = 23;
  function drinkPose(t) {
    var u = (t % DRINK_PERIOD) / DRINK_PERIOD;
    return smooth(0.5, 0.58, u) * (1 - smooth(0.84, 0.93, u));
  }

  // The neck bends as a chain of three traced segments. Each pivot sits on the mane edge
  // (the convex side when the head goes down), so that edge stays unbroken.
  var NECK_PIVOTS = [[650, 500], [691, 442], [735, 381]];
  var HEAD_PIVOT = [815, 336];
  var MUZZLE = [968, 324];

  function rotAbout(p, c, a) {
    var dx = p[0] - c[0];
    var dy = p[1] - c[1];
    return [c[0] + dx * Math.cos(a) - dy * Math.sin(a), c[1] + dx * Math.sin(a) + dy * Math.cos(a)];
  }

  function camelPose(t) {
    var d = drinkPose(t);
    var sip = d > 0.95 ? Math.sin(t * 5) * 0.02 : 0;
    var nod = Math.sin(t * 0.7) * 0.01 * (1 - d);
    var bend = d * 0.41;
    var angles = [bend * 0.8 + nod, bend, bend * 1.1, d * 0.3 + sip + Math.sin(t * 0.5) * 0.02 * (1 - d)];
    var m = MUZZLE;
    var piv = NECK_PIVOTS.concat([HEAD_PIVOT]);
    for (var i = piv.length - 1; i >= 0; i--) m = rotAbout(m, piv[i], angles[i]);
    return { drink: d, br: Math.sin(t * 1.4) * 1.5, angles: angles, muzzle: m };
  }

  function camel(ctx, t, pose) {
    var F = figures();
    if (!F) return;
    drawFig(ctx, F.camelBody);
    // shoulder filler: only seen while the neck swings down to drink
    var fill = smooth(0, 0.25, pose.drink);
    if (fill > 0.01) {
      ctx.globalAlpha = BASE * clamp(fill, 0, 1);
      ctx.beginPath();
      ctx.moveTo(650, 470);
      ctx.bezierCurveTo(664, 450, 690, 446, 706, 456);
      ctx.bezierCurveTo(716, 500, 712, 560, 700, 600);
      ctx.lineTo(660, 600);
      ctx.closePath();
      ctx.fillStyle = HIDE;
      ctx.fill();
      specks(ctx, 90, 4000, 650, 446, 716, 600, SPOT, 0.8 * fill, 0.8, 2.6, null);
      ctx.globalAlpha = BASE;
    }
    var piv = NECK_PIVOTS;
    var segs = [F.camelNeck1, F.camelNeck2, F.camelNeck3];
    ctx.save();
    for (var i = 0; i < 3; i++) {
      ctx.translate(piv[i][0], piv[i][1]);
      ctx.rotate(pose.angles[i]);
      ctx.translate(-piv[i][0], -piv[i][1]);
      drawFig(ctx, segs[i]);
    }
    ctx.translate(HEAD_PIVOT[0], HEAD_PIVOT[1]);
    ctx.rotate(pose.angles[3]);
    ctx.translate(-HEAD_PIVOT[0], -HEAD_PIVOT[1]);
    drawFig(ctx, F.camelHead);
    ctx.restore();
  }

  var HIDE = "#bda77f";
  var SPOT = "#24190f";

  // ---------- rider: traced robes, live eyes, pipe and smoke ----------

  function blinkAt(t) {
    var P = 4.1;
    var k = Math.floor(t / P);
    var start = rnd(k + 400) * (P - 0.4);
    var tau = t - k * P - start;
    var b = 1;
    if (tau > 0 && tau < 0.18) b = Math.abs(tau - 0.09) / 0.09;
    if (rnd(k + 700) > 0.7 && tau > 0.26 && tau < 0.44) b = Math.min(b, Math.abs(tau - 0.35) / 0.09);
    return Math.max(0.08, b);
  }

  var PUFF = 9;
  function puffPhase(t) {
    return (t % PUFF) / PUFF;
  }
  function drawIn(t) {
    var ph = puffPhase(t);
    return smooth(0.05, 0.2, ph) * (1 - smooth(0.25, 0.32, ph));
  }

  // Rider frame: breathes with the camel, leans a touch when it drinks.
  function riderFrame(ctx, pose) {
    ctx.translate(0, pose.br);
    ctx.translate(520, 380);
    ctx.rotate(pose.drink * 0.02);
    ctx.translate(-520, -380);
  }

  function rider(ctx, t, pose) {
    var F = figures();
    if (!F) return;
    var wind = windAt(t);
    ctx.save();
    riderFrame(ctx, pose);
    // the loose shawl end flutters in the wind
    ctx.save();
    ctx.translate(470, 64);
    ctx.rotate(-0.02 - wind * 0.03 + Math.sin(t * 3.3) * 0.025 * wind + Math.sin(t * 7.1) * 0.008);
    ctx.translate(-470, -64);
    drawFig(ctx, F.riderShawl);
    ctx.restore();
    drawFig(ctx, F.rider);
    eyes(ctx, t, 1);
    pipe(ctx, t);
    smoke(ctx, t, wind);
    ctx.restore();
  }

  // A short pipe clenched under the veil, bowl up, ember glowing on each draw.
  function pipe(ctx, t) {
    var lift = drawIn(t) * 3;
    ctx.strokeStyle = INK;
    ctx.lineWidth = 6;
    ctx.beginPath();
    ctx.moveTo(566, 104);
    ctx.quadraticCurveTo(588, 106 - lift, 608, 116 - lift);
    ctx.stroke();
    ctx.strokeStyle = "#6a4426";
    ctx.lineWidth = 2.4;
    ctx.stroke();
    ctx.beginPath();
    ctx.moveTo(604, 96 - lift);
    ctx.lineTo(624, 96 - lift);
    ctx.lineTo(622, 116 - lift);
    ctx.quadraticCurveTo(614, 124 - lift, 606, 117 - lift);
    ctx.closePath();
    ctx.fillStyle = "#3a2216";
    ctx.fill();
    ctx.strokeStyle = INK;
    ctx.lineWidth = 2.6;
    ctx.stroke();
    ember(ctx, t, 1);
  }

  function eyes(ctx, t, a) {
    var b = blinkAt(t);
    ctx.globalAlpha = BASE * clamp(a * 0.22, 0, 1);
    ctx.fillStyle = "#2f6cff";
    ctx.beginPath();
    ctx.ellipse(543.5, 60, 6, 2.8 * Math.max(b, 0.3), -0.05, 0, TAU);
    ctx.ellipse(566, 61.5, 6, 2.8 * Math.max(b, 0.3), 0.05, 0, TAU);
    ctx.fill();
    ctx.globalAlpha = BASE * clamp(a, 0, 1);
    ctx.fillStyle = "#2a62ff";
    ctx.beginPath();
    ctx.ellipse(543.5, 60, 5, 2.1 * b, -0.05, 0, TAU);
    ctx.ellipse(566, 61.5, 5, 2.1 * b, 0.05, 0, TAU);
    ctx.fill();
    ctx.fillStyle = "#d4e8ff";
    ctx.beginPath();
    ctx.ellipse(544, 60, 2, 0.9 * b, 0, 0, TAU);
    ctx.ellipse(566.5, 61.5, 2, 0.9 * b, 0, 0, TAU);
    ctx.fill();
    ctx.globalAlpha = BASE;
  }

  function ember(ctx, t, a) {
    var d = drawIn(t);
    var lift = d * 3;
    var glow = 0.45 + 0.55 * d + 0.1 * Math.sin(t * 9);
    ctx.globalAlpha = BASE * clamp(a * glow * 0.4, 0, 1);
    ctx.fillStyle = "#ff9a3a";
    ctx.beginPath();
    ctx.ellipse(614, 96 - lift, 13, 6, 0, 0, TAU);
    ctx.fill();
    ctx.globalAlpha = BASE * clamp(a * glow, 0, 1);
    ctx.fillStyle = "#ff6a1e";
    ctx.beginPath();
    ctx.ellipse(614, 96 - lift, 8, 2.6, 0, 0, TAU);
    ctx.fill();
    ctx.globalAlpha = BASE;
  }

  function smoke(ctx, t, wind) {
    var lift = drawIn(t) * 3;
    var bx = 614;
    var by = 92 - lift;
    var i, k;
    ctx.strokeStyle = "#f0eadc";
    for (k = 0; k < 3; k++) {
      var seg = 16;
      var px = bx;
      var py = by;
      for (i = 1; i <= seg; i++) {
        var u = i / seg;
        var x = bx - wind * Math.pow(u, 1.5) * 70 + Math.sin(t * 1.6 - u * 7 + k * 2.1) * (3 + u * 18) + k * 3;
        var y = by - u * 120;
        ctx.globalAlpha = BASE * clamp((1 - u) * (0.6 - k * 0.14), 0, 1);
        ctx.lineWidth = 1.6 + u * 4 + k;
        ctx.beginPath();
        ctx.moveTo(px, py);
        ctx.lineTo(x, y);
        ctx.stroke();
        px = x;
        py = y;
      }
    }
    // exhaled puff drifting out of the veil
    var ph = puffPhase(t);
    if (ph > 0.33 && ph < 0.75) {
      var q = (ph - 0.33) / 0.42;
      ctx.fillStyle = "#ece6d6";
      for (i = 0; i < 6; i++) {
        var qq = clamp(q * 1.15 - i * 0.03, 0, 1);
        ctx.globalAlpha = BASE * clamp((1 - qq) * 0.35, 0, 1);
        ctx.beginPath();
        ctx.arc(600 + qq * 46 - wind * qq * qq * 110 + Math.sin(i * 2 + t) * 5, 112 - qq * 70 - i * 5, 4 + qq * 18 + i * 2, 0, TAU);
        ctx.fill();
      }
    }
    ctx.globalAlpha = BASE;
  }

  // ---------- long shadows from the clock sun ----------

  function castShadow(ctx, sun, pose) {
    if (sun.alt < 1) return;
    var len = clamp(1 / Math.tan((sun.alt * Math.PI) / 180), 0.3, 3.2);
    var dir = sun.morning ? 1 : -1;
    var a = clamp(sun.alt / 6, 0, 1) * 0.55;
    ctx.fillStyle = "rgba(80,24,8," + a.toFixed(3) + ")";
    var feet = [[325, 826, 20], [430, 860, 26], [530, 862, 24], [660, 858, 24]];
    var dx = dir * 150 * len;
    var dy = 26 * len + 20;
    ctx.beginPath();
    for (var i = 0; i < feet.length; i++) {
      var f = feet[i];
      ctx.moveTo(f[0] - f[2], f[1]);
      ctx.lineTo(f[0] + f[2], f[1]);
      ctx.lineTo(f[0] + f[2] + dx, f[1] + dy);
      ctx.lineTo(f[0] - f[2] * 1.4 + dx, f[1] + dy);
      ctx.closePath();
    }
    ctx.fill();
    ctx.beginPath();
    ctx.ellipse(500 + dx * 1.3, 880 + dy * 0.8, 120 + len * 30, 14 + len * 4, 0, 0, TAU);
    ctx.fill();
  }

  // ---------- the oasis: one draw ----------

  // Layers: 0 back palms, 1 pool + shadows, 2 camel, 3 rider, 4 front palms.
  function layerOn(ctx, fx, i) {
    var o = fx.layer ? fx.layer(i) : null;
    if (!o) return false;
    ctx.save();
    ctx.translate(o[0], o[1]);
    if (o[2] != null) {
      o.prev = BASE;
      BASE = clamp(o[2], 0, 1) * fx.alpha;
      ctx.globalAlpha = BASE;
    }
    fx.lastLayer = o;
    return true;
  }
  function layerOff(ctx, on, fx) {
    if (on && fx && fx.lastLayer && fx.lastLayer.prev != null) BASE = fx.lastLayer.prev;
    if (on) ctx.restore();
  }

  // Reference frame -> screen; fx.sy squashes toward the ground line (y = 860).
  function oasisTransform(ctx, env, fx) {
    ctx.translate(env.ox + fx.dx, env.oy + 860 * env.s + fx.dy);
    ctx.scale(env.s * fx.sx, env.s * fx.sy);
    ctx.translate(0, -860);
  }

  function oasis(ctx, t, env, fx, sun) {
    var pose = camelPose(t);
    ctx.save();
    if (fx.clip) fx.clip(ctx);
    oasisTransform(ctx, env, fx);
    BASE = clamp(fx.alpha, 0, 1);
    ctx.globalAlpha = BASE;

    var on = layerOn(ctx, fx, 0);
    tracedPalms(ctx, t);
    layerOff(ctx, on, fx);

    on = layerOn(ctx, fx, 1);
    pool(ctx, t, env.water, pose.drink, pose.muzzle);
    castShadow(ctx, sun, pose);
    layerOff(ctx, on, fx);

    on = layerOn(ctx, fx, 2);
    camel(ctx, t, pose);
    layerOff(ctx, on, fx);

    on = layerOn(ctx, fx, 3);
    rider(ctx, t, pose);
    layerOff(ctx, on, fx);

    ctx.restore();
    BASE = 1;
  }

  // Night pass: blue eyes and the ember stay lit above the darkness overlay.
  function glowPass(ctx, t, env, fx, a) {
    if (a < 0.02 || fx.alpha < 0.02) return;
    var pose = camelPose(t);
    ctx.save();
    if (fx.clip) fx.clip(ctx);
    oasisTransform(ctx, env, fx);
    BASE = clamp(fx.alpha, 0, 1);
    var on = layerOn(ctx, fx, 3);
    riderFrame(ctx, pose);
    eyes(ctx, t, a);
    ember(ctx, t, a);
    layerOff(ctx, on, fx);
    ctx.restore();
    BASE = 1;
  }

  // ---------- mirage glitches ----------

  function bandsClip(ctx, w, h, t, p, appear) {
    var n = 22;
    ctx.beginPath();
    for (var i = 0; i < n; i++) {
      var gate = rnd(i * 7 + 1) * 0.8 + (appear ? (i / n) * 0.2 : (1 - i / n) * 0.2);
      if (gate > p) continue;
      var y = (h * i) / n;
      var jit = (rnd(i + Math.floor(t * 24) * 31) - 0.5) * h * 0.012 * (1 - p);
      rectPath(ctx, 0, y + jit, w, h / n + 1);
    }
    ctx.clip();
  }

  function blocksClip(ctx, w, h, p, appear) {
    var cell = Math.max(16, Math.min(w, h) / 22);
    var cols = Math.ceil(w / cell);
    var rows = Math.ceil(h / cell);
    ctx.beginPath();
    for (var r = 0; r < rows; r++) {
      for (var c = 0; c < cols; c++) {
        var g = rnd(r * 977 + c * 131 + 5);
        // reveal from the centre out on appear, scatter outward on vanish
        var dx = (c + 0.5) / cols - 0.5;
        var dy = (r + 0.5) / rows - 0.6;
        var d = Math.sqrt(dx * dx + dy * dy);
        var gate = g * 0.65 + d * 0.5;
        if (gate > p * 1.15) continue;
        rectPath(ctx, c * cell, r * cell, cell + 0.5, cell + 0.5);
      }
    }
    ctx.clip();
  }

  function glitchFx(style, e, g, t, w, h, appear) {
    var fx = { alpha: 1, dx: 0, dy: 0, sx: 1, sy: 1, layer: null, clip: null };
    if (g < 0.002) {
      fx.alpha = e;
      return fx;
    }
    var frame = Math.floor(t * 30);
    if (style === 0) {
      // нарезка: horizontal slices snap in, each layer torn sideways
      fx.clip = function (ctx) { bandsClip(ctx, w, h, t, e * 1.02, appear); };
      fx.layer = function (i) {
        return [(rnd(frame * 7 + i * 3) - 0.5) * 220 * g, 0];
      };
    } else if (style === 1) {
      // двоение: every layer strobes between two positions → a double image
      fx.alpha = 0.35 + 0.65 * e;
      fx.layer = function (i) {
        var side = (frame + i) % 2 ? 1 : -1;
        return [side * 70 * g * (1 + i * 0.25), side * 6 * g, 0.55 + 0.45 * e];
      };
    } else if (style === 2) {
      // всплытие: rises out of the ground line, stretched, wobbling
      fx.alpha = clamp(e * 1.6, 0, 1);
      fx.sy = 0.05 + 0.95 * e;
      fx.sx = 1 + 0.25 * (1 - e) + Math.sin(t * 40) * 0.02 * g;
      fx.dy = (1 - e) * h * 0.05;
    } else if (style === 3) {
      // мерцание: on/off flicker, probability follows the link
      var lit = rnd(frame * 13 + 1) < e * 1.1;
      fx.alpha = lit ? 0.55 + 0.45 * e : 0.08 * e;
      fx.dx = lit ? 0 : (rnd(frame) - 0.5) * 40 * g;
    } else if (style === 4) {
      // жар: heat waver, layers sway on different phases
      fx.alpha = e;
      fx.sx = 1 + Math.sin(t * 17) * 0.05 * g;
      fx.sy = 1 + Math.sin(t * 13) * 0.08 * g;
      fx.layer = function (i) {
        return [Math.sin(t * 14 + i * 1.7) * 60 * g, Math.sin(t * 11 + i) * 10 * g];
      };
    } else {
      // распад: assembles from / falls apart into blocks
      fx.clip = function (ctx) { blocksClip(ctx, w, h, e, appear); };
      fx.dx = (rnd(frame) - 0.5) * 16 * g;
    }
    return fx;
  }

  function glitchOverlay(ctx, style, e, g, t, w, h, horizon, appear) {
    if (g < 0.02) return;
    var frame = Math.floor(t * 30);
    var i, y;
    if (style === 0) {
      // bright tear lines along cut edges
      ctx.fillStyle = "#fff0c8";
      for (i = 0; i < 8; i++) {
        ctx.globalAlpha = BASE * clamp(g * 0.7 * rnd(frame * 3 + i), 0, 1);
        y = rnd(frame * 11 + i * 5) * h;
        ctx.fillRect((rnd(i + frame) - 0.3) * w, y, w * (0.2 + rnd(i * 9 + frame) * 0.6), 2 + rnd(i) * 3);
      }
    } else if (style === 1) {
      // chromatic ghost bars
      ctx.fillStyle = "#3fd0ff";
      ctx.globalAlpha = BASE * clamp(g * 0.12, 0, 1);
      ctx.fillRect(0, horizon - h * 0.25, w, h * 0.04);
      ctx.fillStyle = "#ff5a2a";
      ctx.fillRect(0, horizon + h * 0.2, w, h * 0.03);
    } else if (style === 2) {
      // rising haze line at the ground
      ctx.strokeStyle = "#f8e8b8";
      ctx.lineWidth = Math.max(2, h * 0.006);
      for (i = 0; i < 4; i++) {
        ctx.globalAlpha = BASE * clamp(g * (0.5 - i * 0.1), 0, 1);
        y = h * (0.9 - e * 0.35) + i * h * 0.012;
        ctx.beginPath();
        for (var x = 0; x <= w; x += w / 40) {
          var yy = y + Math.sin(x * 0.04 + t * 18 + i) * h * 0.006;
          if (x === 0) ctx.moveTo(x, yy);
          else ctx.lineTo(x, yy);
        }
        ctx.stroke();
      }
    } else if (style === 3) {
      // rolling scanlines
      for (i = 0; i < 40; i++) {
        y = ((i / 40) * h + t * h * 0.6) % h;
        ctx.globalAlpha = BASE * clamp(g * 0.25, 0, 1);
        ctx.fillStyle = i % 2 ? "#1a100c" : "#f6e2b0";
        ctx.fillRect(0, y, w, Math.max(1, h * 0.003));
      }
    } else if (style === 4) {
      // the 5.jpg / 6.jpg mirage swallowing (or giving back) the oasis
      mirageBand(ctx, w, h, horizon - h * 0.3, horizon + h * 0.36, w * 0.05, w * 0.95, t * 3, g, 7);
    } else {
      // dust blowing off the edge of the blocks
      var cell = Math.max(16, Math.min(w, h) / 22);
      ctx.fillStyle = "#f2c88a";
      var wind = windAt(t);
      for (i = 0; i < 60; i++) {
        var ph = (t * 1.5 + rnd(i + 3)) % 1;
        var bx = rnd(i * 5 + 1) * w;
        var by = h * (0.15 + rnd(i * 7 + 2) * 0.75);
        ctx.globalAlpha = BASE * clamp(g * (1 - ph) * 0.8, 0, 1);
        var sz = cell * 0.25 * (1 - ph);
        ctx.fillRect(bx - ph * w * 0.15 * wind, by - ph * h * 0.05, sz, sz);
      }
    }
    ctx.globalAlpha = BASE;
  }

  // Dots scattered around a polyline: the dithered edge of a printed shadow.
  function sprayAlong(ctx, P, spread, n, seed, color, alpha, r0, r1) {
    ctx.beginPath();
    for (var i = 0; i < n; i++) {
      var k = Math.floor(rnd(seed + i) * (P.length - 1));
      var u = rnd(seed + i * 7 + 1);
      var x = lerp(P[k][0], P[k + 1][0], u) + (rnd(seed + i * 13 + 2) - 0.5) * spread * 2;
      var y = lerp(P[k][1], P[k + 1][1], u) + (rnd(seed + i * 17 + 3) - 0.5) * spread * 2;
      var r = r0 + (r1 - r0) * rnd(seed + i * 19 + 4);
      ctx.moveTo(x + r, y);
      ctx.arc(x, y, r, 0, TAU);
    }
    ctx.fillStyle = color;
    ctx.globalAlpha = BASE * clamp(alpha, 0, 1);
    ctx.fill();
    ctx.globalAlpha = BASE;
  }

  // The big sweeping foreground dune of 5.jpg: a sinuous inked crest and a dithered shadow crescent.
  function bigDune(ctx, w, h, horizon, lit, shade, seed) {
    var W = Math.max(w, h * 1.4);
    var off = (W - w) / 2;
    var P = function (x, y) { return [x * W - off, horizon + y * h]; };
    var crest = [P(0, 0.1), P(0.1, 0.06), P(0.2, 0.0), P(0.235, 0.03), P(0.215, 0.08), P(0.26, 0.13), P(0.45, 0.18), P(0.65, 0.215), P(0.85, 0.26), P(1, 0.28)];
    var ridge2 = [P(0.42, 0.13), P(0.6, 0.07), P(0.78, 0.0), P(0.86, 0.06), P(0.84, 0.12), P(0.92, 0.19), P(1, 0.21)];
    // lit body of the near dunes
    ctx.fillStyle = rgba(lit);
    ctx.beginPath();
    smoothLine(ctx, crest, true);
    ctx.lineTo(w + off, h);
    ctx.lineTo(-off, h);
    ctx.closePath();
    ctx.fill();
    // right-hand dune behind the crest
    ctx.beginPath();
    smoothLine(ctx, ridge2, true);
    ctx.lineTo(w + off, horizon + h * 0.3);
    ctx.lineTo(0.5 * W - off, horizon + h * 0.3);
    ctx.closePath();
    ctx.fill();
    var sh2 = [P(0.78, 0.0), P(0.86, 0.06), P(0.84, 0.12), P(0.92, 0.19), P(1, 0.21), P(1, 0.12), P(0.9, 0.06)];
    ctx.fillStyle = rgba(shade);
    ctx.beginPath();
    smoothLine(ctx, sh2, true);
    ctx.closePath();
    ctx.fill();
    // the crescent shadow sweeping under the crest
    var cres = [P(0.215, 0.08), P(0.12, 0.15), P(0.1, 0.22), P(0.2, 0.29), P(0.4, 0.33), P(0.6, 0.335), P(0.7, 0.31), P(0.55, 0.28), P(0.38, 0.235), P(0.28, 0.17), P(0.26, 0.13)];
    ctx.beginPath();
    smoothLine(ctx, cres, true);
    ctx.closePath();
    ctx.fill();
    sprayAlong(ctx, cres.slice(1, 7), h * 0.022, 1300, seed, rgba(shade), 0.85, 0.6, 2.4);
    sprayAlong(ctx, cres.slice(6), h * 0.012, 260, seed + 900, rgba(lit), 0.7, 0.6, 1.8);
    sprayAlong(ctx, sh2, h * 0.014, 260, seed + 1500, rgba(shade), 0.8, 0.6, 2);
    // ink: crest lines and a slip-face rill
    ctx.strokeStyle = INK;
    ctx.lineWidth = Math.max(1.2, h * 0.0024);
    ctx.beginPath();
    smoothLine(ctx, crest.slice(1), true);
    smoothLine(ctx, ridge2, true);
    ctx.moveTo(P(0.86, 0.06)[0], P(0.86, 0.06)[1]);
    ctx.quadraticCurveTo(P(0.9, 0.1)[0], P(0.9, 0.1)[1], P(0.88, 0.15)[0], P(0.88, 0.15)[1]);
    ctx.stroke();
  }

  // Printed-poster grunge: blotches, grain, paper fibres. Fixed per position, it does not crawl.
  function printTexture(ctx, w, h) {
    var m = Math.min(w, h);
    var i, k, r;
    // mottling: loose clusters of faint dots
    ctx.fillStyle = "#4a1a06";
    for (i = 0; i < 26; i++) {
      var bx = rnd(i * 5 + 7100) * w;
      var by = rnd(i * 7 + 7200) * h;
      var br = m * (0.03 + rnd(i + 7300) * 0.06);
      ctx.globalAlpha = BASE * 0.12;
      for (k = 0; k < 70; k++) {
        var a = rnd(i * 97 + k) * TAU;
        var d = Math.sqrt(rnd(i * 89 + k * 3)) * br;
        r = Math.max(0.8, m * 0.0016) * (0.6 + rnd(i * 83 + k * 5) * 1.6);
        ctx.fillRect(bx + Math.cos(a) * d - r, by + Math.sin(a) * d * 0.7 - r, r * 2, r * 2);
      }
    }
    // fine grain, dark and light
    var g = Math.max(1, m * 0.0018);
    ctx.fillStyle = "#3a1606";
    for (i = 0; i < 1100; i++) {
      ctx.globalAlpha = BASE * (0.1 + rnd(i * 3 + 8100) * 0.18);
      r = g * (0.4 + rnd(i + 8200));
      ctx.fillRect(rnd(i * 7 + 8300) * w, rnd(i * 11 + 8400) * h, r, r);
    }
    ctx.fillStyle = "#fff0d0";
    for (i = 0; i < 420; i++) {
      ctx.globalAlpha = BASE * (0.08 + rnd(i * 3 + 9100) * 0.12);
      r = g * (0.4 + rnd(i + 9200) * 0.8);
      ctx.fillRect(rnd(i * 13 + 9300) * w, rnd(i * 17 + 9400) * h, r, r);
    }
    ctx.globalAlpha = BASE;
    streaks(ctx, 120, 9500, 0, 0, w, h, Math.PI / 2, h * 0.06, "#3a1606", 1, 0.07, null);
  }

  // The warm light pooled in the middle of the print (5.jpg): stacked faint ellipses.
  function halo(ctx, w, h, a) {
    if (a < 0.01) return;
    ctx.fillStyle = "#ffc080";
    for (var i = 0; i < 6; i++) {
      ctx.globalAlpha = BASE * clamp(a * 0.035, 0, 1);
      ctx.beginPath();
      ctx.ellipse(w * 0.45, h * 0.66, w * (0.22 + i * 0.07), h * (0.2 + i * 0.05), 0, 0, TAU);
      ctx.fill();
    }
    ctx.globalAlpha = BASE;
  }

  // Darker, redder edges like the print: wide strokes of growing ellipses (no gradients needed).
  function vignette(ctx, w, h, a) {
    var d = Math.sqrt(w * w + h * h);
    ctx.strokeStyle = "#5a1406";
    ctx.lineWidth = d * 0.12;
    for (var i = 0; i < 8; i++) {
      ctx.globalAlpha = BASE * clamp(a * 0.05, 0, 1);
      ctx.beginPath();
      ctx.ellipse(w / 2, h / 2, w * (0.62 + i * 0.06), h * (0.62 + i * 0.06), 0, 0, TAU);
      ctx.stroke();
    }
    ctx.globalAlpha = BASE;
  }

  // The 5.jpg mirage: a band of sharp zigzag stripes (cream, teal, olive, rust) with a teal glow.
  // Used faintly over the empty horizon by day, and at full strength as the жар glitch.
  var MIRAGE = ["#f4e6c8", "#c89870", "#8a8a52", "#fff4dc", "#b07a50", "#e8d4b0", "#6aa8a0", "#f0dcc0"];
  function mirageBand(ctx, w, h, top, bottom, x0, x1, t, amt, seed) {
    if (amt < 0.01) return;
    var n = Math.max(6, Math.round((bottom - top) / (h * 0.012)));
    var bandH = (bottom - top) / n;
    var cx = (x0 + x1) / 2;
    // teal glow in the middle, like water seen from far away
    ctx.fillStyle = "#8fd8d0";
    for (var g = 0; g < 4; g++) {
      ctx.globalAlpha = BASE * clamp(amt * 0.13, 0, 1);
      ctx.beginPath();
      ctx.ellipse(cx, (top + bottom) / 2 + bandH, (x1 - x0) * (0.18 + g * 0.06), (bottom - top) * (0.18 + g * 0.06), 0, 0, TAU);
      ctx.fill();
    }
    var wl = Math.max(12, w * 0.045);
    ctx.lineWidth = bandH * 1.3;
    for (var i = 0; i < n; i++) {
      var y = top + (i + 0.5) * bandH;
      // the band is widest in the middle rows, tapering at top and bottom
      var mid = 1 - Math.abs(i / (n - 1) - 0.55) * 1.4;
      var half = (x1 - x0) * 0.5 * clamp(mid, 0.15, 1) * (0.8 + rnd(seed + i) * 0.4);
      var a0 = cx - half + (rnd(seed + i * 3) - 0.5) * wl * 2;
      var a1 = cx + half + (rnd(seed + i * 5) - 0.5) * wl * 2;
      var amp = bandH * (0.75 + 0.3 * Math.sin(t * 2.3 + i * 0.9));
      var ph = t * (1.4 + rnd(seed + i * 7)) * wl + i * wl * 0.37;
      ctx.strokeStyle = MIRAGE[(i + seed) % MIRAGE.length];
      ctx.globalAlpha = BASE * clamp(amt * (0.45 + 0.4 * rnd(seed + i * 11)), 0, 1);
      ctx.beginPath();
      // rounded zigzag: quadratic through the midpoints of the peaks
      var k = Math.floor((a0 + ph) / (wl / 2));
      var px = a0;
      var py = y;
      ctx.moveTo(px, py);
      for (var x = (k + 1) * (wl / 2) - ph; x < a1; x += wl / 2) {
        k++;
        var qy = y + (k % 2 ? amp : -amp);
        ctx.quadraticCurveTo(px, py, (px + x) / 2, (py + qy) / 2);
        px = x;
        py = qy;
      }
      ctx.lineTo(a1, y);
      ctx.stroke();
    }
    // the teal heart sits on top of the stripes
    ctx.fillStyle = "#7cd0c8";
    for (g = 0; g < 3; g++) {
      ctx.globalAlpha = BASE * clamp(amt * 0.12, 0, 1);
      ctx.beginPath();
      ctx.ellipse(cx - (x1 - x0) * 0.05, (top + bottom) / 2 + bandH, (x1 - x0) * (0.1 + g * 0.06), (bottom - top) * (0.12 + g * 0.08), 0, 0, TAU);
      ctx.fill();
    }
    ctx.globalAlpha = BASE;
  }

  // ---------- frame ----------

  var rowCache = { w: 0, h: 0, rows: null };
  function duneRows(w, h, horizon) {
    if (rowCache.w === w && rowCache.h === h) return rowCache.rows;
    rowCache.w = w;
    rowCache.h = h;
    // lay peaks out on a landscape-wide strip so a tall phone screen keeps broad dunes
    var W = Math.max(w, h * 1.6);
    var off = (W - w) / 2;
    var shiftX = function (peaks) {
      for (var i = 0; i < peaks.length; i++) peaks[i].x -= off;
      return peaks;
    };
    rowCache.rows = [
      { base: horizon + h * 0.02, peaks: shiftX(makePeaks(10, 6, W, h * 0.07, W * 0.12)), scale: 0.6 },
      { base: horizon + h * 0.12, peaks: shiftX(makePeaks(30, 4, W, h * 0.11, W * 0.2)), scale: 0.8 },
      { base: horizon + h * 0.3, peaks: shiftX(makePeaks(50, 3, W, h * 0.12, W * 0.28)), scale: 1 },
    ];
    return rowCache.rows;
  }

  function draw(ctx, t, p) {
    BASE = 1;
    var w = p.w;
    var h = p.h;
    var args = p.args || [];
    var link = clamp(+args[0] || 0, 0, 1);
    var hour = (((args[1] == null ? 12 : +args[1]) % 24) + 24) % 24;
    var day = args[2] == null ? 172 : +args[2];
    var dst = +args[3] || 0;
    var style = ((args[4] | 0) % 6 + 6) % 6;
    var target = args[5] == null ? (link > 0.5 ? 1 : 0) : +args[5];
    var appear = target > 0.5;
    var sun = solar(hour, day, dst);
    // Oasis art is the 1120×912 poster. Scale comes from the window: the camel
    // barrel (poster y 540) is the vertical center, and the head always fits.
    var s = figureScale(w, h);
    SMOOTH = s > 1.3;
    var horizon = h * 0.5 - (540 - 438) * s;
    var light = 0.25 + 0.75 * sun.day;
    var gold = sun.set * (1 - sun.day * 0.7);

    // 1. sky and stars. Sun and moon are drawn last, on the rider's back line.
    sky(ctx, w, h, horizon, sun);
    stars(ctx, w, horizon, t, sun.night);

    // 2. dunes, back to front (palette from 5.jpg)
    var rows = duneRows(w, h, horizon);
    var tint = function (c) { return mix(mix(c, [c[0] * 0.95, c[1] * 0.6, c[2] * 0.55], gold * 0.5), [c[0] * 0.3, c[1] * 0.28, c[2] * 0.4], (1 - light) * 0.4); };
    mirageBand(ctx, w, h, horizon - h * 0.15, horizon + h * 0.04, w * 0.02, w * 0.74, t, smooth(0.15, 0.6, sun.day) * (1 - smooth(0, 0.5, link)) * 0.8, 3);
    duneRow(ctx, w, h, rows[0].base, rows[0].peaks, tint([218, 132, 78]), tint([190, 96, 56]), 0.6, 160, 100);
    duneRow(ctx, w, h, rows[1].base, rows[1].peaks, tint([226, 124, 64]), tint([176, 86, 48]), 0.85, 260, 200);
    bigDune(ctx, w, h, horizon, tint([228, 128, 70]), tint([162, 76, 42]), 300);

    // 3. oasis: once
    var e = smooth(0, 1, link);
    var g = Math.sin(Math.PI * clamp(link, 0, 1));
    var night = sun.night;
    var dusk = gold;
    var wcol = function (day, set, nt) { return mix(mix(day, set, dusk), nt, night); };
    var env = {
      s: s,
      ox: w * 0.5 - 590 * s,
      oy: horizon - 438 * s,
      water: {
        bands: [
          [588, wcol([20, 52, 80], [60, 30, 50], [6, 10, 26])],
          [615, wcol([24, 70, 110], [110, 60, 70], [12, 18, 40])],
          [636, wcol([96, 140, 104], [180, 110, 70], [24, 30, 56])],
          [652, wcol([214, 208, 70], [246, 170, 70], [56, 52, 80])],
          [680, wcol([196, 204, 80], [220, 140, 70], [44, 46, 74])],
          [700, wcol([70, 136, 140], [120, 80, 80], [20, 28, 54])],
          [720, wcol([34, 98, 160], [70, 50, 80], [12, 18, 42])],
          [746, wcol([22, 72, 152], [40, 30, 64], [8, 12, 34])],
          [776, wcol([12, 48, 120], [26, 18, 46], [4, 8, 24])],
          [810, wcol([6, 30, 60], [16, 12, 30], [2, 6, 18])],
        ],
        bank: wcol([150, 70, 34], [130, 50, 30], [40, 24, 30]),
        refl: wcol([4, 22, 30], [30, 10, 20], [2, 4, 12]),
        glint: mix([236, 246, 214], [150, 160, 190], night),
      },
    };
    var fx = glitchFx(style, e, g, t, w, h, appear);
    if (fx.alpha > 0.005 && (link > 0.001)) oasis(ctx, t, env, fx, sun);

    // 4. wind, glitch overlays, print grain
    windSand(ctx, w, h, horizon, t, rows.slice(0, 2), light);
    glitchOverlay(ctx, style, e, g, t, w, h, horizon, appear);
    halo(ctx, w, h, sun.day * (1 - sun.night));
    printTexture(ctx, w, h);
    vignette(ctx, w, h, 1.2);

    // 5. light level: winter / dusk / night dim everything; eyes and ember stay lit
    var dark = clamp((1 - sun.day) * 0.32 + sun.night * 0.42, 0, 0.72);
    if (dark > 0.01) {
      ctx.fillStyle = rgba(mix([60, 20, 40], [6, 10, 34], sun.night), dark);
      ctx.fillRect(0, 0, w, h);
    }
    if (gold > 0.05) {
      ctx.fillStyle = rgba([255, 90, 30], gold * 0.12 * (1 - sun.night));
      ctx.fillRect(0, 0, w, h);
    }
    if (link > 0.001) glowPass(ctx, t, env, fx, clamp(dark * 1.6, 0, 1));
    sunMoon(ctx, w, h, horizon, hour, sun);
    ctx.globalAlpha = BASE;
    return sun;
  }

  var root = typeof globalThis !== "undefined" ? globalThis : window;
  root.uzorAnims = root.uzorAnims || {};
  root.uzorAnims.desert = draw;
  root.drawDesert = draw;
  root.m1r4g3Solar = solar;
})();
