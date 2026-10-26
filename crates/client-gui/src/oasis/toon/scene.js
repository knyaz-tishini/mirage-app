// m1r4g3 oasis, cartoon only. drawDesert(ctx, t, { w, h, args }).
//
// The picture is the skin. Load toon-skin.js first. Before the first frame the
// host sets root.m1r4g3Skin (the rider bitmap, _toon = true) and
// root.m1r4g3PalmImage (the palm bitmap). There is no second look and no
// ink-figure fallback.
//
// args[0] link 0..1          (0 = empty desert, 1 = oasis)
// args[1] wall-clock hour 0..24
// args[2] day of year 1..366
// args[3] DST hours 0|1
// args[4] glitch style 0..3  нарезка, двоение, мерцание, распад
// args[5] target link 0|1    (appear vs vanish)
//
// The player that ships this file has to implement drawImage, transform,
// clip, and an offscreen canvas. The uzor-anim-js op recorder does not.

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
      // -1 at sunrise, 0 at noon, +1 at sunset. Continuous, so the cast
      // swings through the feet instead of flipping.
      side: clamp((hour - noon) / Math.max(0.5, half), -1, 1),
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
    ctx.fillStyle = "#fffaf0";
    for (var i = 0; i < 90; i++) {
      var tw = 0.7 + 0.3 * Math.sin(t * (1.2 + rnd(i + 40) * 2) + i);
      ctx.globalAlpha = BASE * clamp(night * tw * (0.82 + rnd(i + 70) * 0.18), 0, 1);
      var r = 1.15 + rnd(i + 90) * 1.55;
      var sx = rnd(i + 11) * w;
      var sy = rnd(i + 300) * horizon * 0.85;
      ctx.beginPath();
      ctx.arc(sx, sy, Math.max(0.6, r * 0.7), 0, TAU);
      ctx.fill();
      if (r > 2.05) {
        ctx.fillRect(sx - r * 1.35, sy - 0.35, r * 2.7, 0.7);
        ctx.fillRect(sx - 0.35, sy - r * 1.35, 0.7, r * 2.7);
      }
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

  // Portrait: the camel is the center of the frame. Landscape: the 1120×912 frame.
  var SCENE_S = 0;
  function layout(w, h) {
    var portrait = w < h * 0.85;
    if (portrait) {
      // Same seat as the centered fit, then a zoom about the muzzle.
      // The nose stays on the water. The rider, the camel, the lake and
      // the palms grow into the empty sky and the empty sand.
      var camelX = 568;
      var camelY = 540;
      var muzzleX = 992;
      var muzzleY = 590;
      var side = Math.max(6, w * 0.02);
      var sFit = (w * 0.5 - side) / (1000 - camelX);
      var zoom = 1.26;
      var sp = sFit * zoom;
      var ox = w * 0.5 - camelX * sFit + muzzleX * (sFit - sp);
      var oy = h * 0.5 - camelY * sFit + muzzleY * (sFit - sp);
      return { portrait: true, s: sp, ox: ox, oy: oy, horizon: oy + 438 * sp };
    }
    // A band of near sand under the hooves. The frame used to end at the
    // feet, so a downward cast fell off the canvas and only a dot remained.
    var pad = h * 0.16;
    var s = Math.min((h - pad) / 912, w / 760);
    var horizon = Math.min((h - pad) * 0.48, 438 * s);
    return { portrait: false, s: s, ox: w * 0.5 - 590 * s, oy: horizon - 438 * s, horizon: horizon };
  }

  function sunMoon(ctx, w, h, horizon, hour, sun) {
    var R = Math.min(w * 0.13, h * 0.16);
    // Back line of the rider, same poster offset on every window.
    var sFig = SCENE_S > 0 ? SCENE_S : figureScale(w, h);
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
      var ma = clamp((2 - sun.alt) / 5, 0, 1);
      var k;
      for (k = 8; k >= 1; k--) {
        ctx.globalAlpha = BASE * ma * 0.04;
        ctx.fillStyle = "#f4f0e2";
        ctx.beginPath();
        ctx.arc(mx, my, mr * (1 + k * 0.1), 0, TAU);
        ctx.fill();
      }
      ctx.globalAlpha = BASE * ma;
      ctx.fillStyle = "#f7f4ea";
      ctx.beginPath();
      ctx.arc(mx, my, mr, 0, TAU);
      ctx.fill();
      ctx.fillStyle = "rgba(186,176,156,0.55)";
      ctx.beginPath();
      ctx.arc(mx - mr * 0.26, my - mr * 0.16, mr * 0.2, 0, TAU);
      ctx.arc(mx + mr * 0.32, my + mr * 0.26, mr * 0.13, 0, TAU);
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
    ctx.globalAlpha = BASE;
    ctx.strokeStyle = "#140e0a";
    ctx.lineJoin = "round";
    ctx.lineCap = "round";
    ctx.lineWidth = Math.max(4.5, h * 0.008);
    ctx.beginPath();
    for (i = 0; i <= steps; i++) {
      x = (w * i) / steps;
      var yy = ridgeY(peaks, x, base);
      if (i === 0) ctx.moveTo(x, yy);
      else ctx.lineTo(x, yy);
    }
    ctx.stroke();
    ctx.lineWidth = Math.max(2.4, h * 0.0035);
    ctx.beginPath();
    for (k = 0; k < peaks.length; k++) {
      p = peaks[k];
      ctx.moveTo(p.x, base - p.a);
      ctx.quadraticCurveTo(p.x + p.wr * 0.16, base - p.a * 0.35, p.x + p.wr * 0.04, base + p.a * 0.08);
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

  // Shifted right so the muzzle drinks over the water, not off the right corner.
  var POOL_PTS = [
    [130, 612], [180, 600], [280, 598], [400, 596], [550, 594], [700, 592], [860, 588], [1000, 590], [1110, 600],
    [1158, 628], [1164, 668], [1140, 708], [1090, 738], [1000, 768], [900, 790], [800, 800], [680, 798], [560, 792],
    [440, 780], [330, 762], [250, 744], [190, 724], [140, 712], [114, 690], [110, 650],
  ];
  function poolPath(ctx, grow) {
    var P = [];
    for (var i = 0; i < POOL_PTS.length; i++) {
      var p = POOL_PTS[i];
      P.push([580 + (p[0] - 580) * (1 + grow / 470), 695 + (p[1] - 695) * (1 + grow / 105)]);
    }
    smoothClosed(ctx, P);
  }

  // Top lip of the pool, so drink rings sit on the water and not above the rim.
  function poolRimY(x) {
    var n = 0;
    var maxX = -1;
    var i;
    for (i = 0; i < POOL_PTS.length; i++) {
      if (POOL_PTS[i][0] >= maxX) {
        maxX = POOL_PTS[i][0];
        n = i;
      } else break;
    }
    if (x <= POOL_PTS[0][0]) return POOL_PTS[0][1];
    if (x >= POOL_PTS[n][0]) return POOL_PTS[n][1];
    for (i = 0; i < n; i++) {
      var a = POOL_PTS[i];
      var b = POOL_PTS[i + 1];
      if (x >= a[0] && x <= b[0]) {
        var u = (x - a[0]) / ((b[0] - a[0]) || 1);
        return a[1] + (b[1] - a[1]) * u;
      }
    }
    return POOL_PTS[n][1];
  }

  // Near shore: the lower edge of the pool, so a foreground palm stands on sand.
  function poolBottomY(x) {
    var n = 0;
    var maxX = -1;
    var i;
    for (i = 0; i < POOL_PTS.length; i++) {
      if (POOL_PTS[i][0] >= maxX) {
        maxX = POOL_PTS[i][0];
        n = i;
      } else break;
    }
    var pts = [];
    for (i = n; i < POOL_PTS.length; i++) pts.push(POOL_PTS[i]);
    pts.sort(function (a, b) { return a[0] - b[0]; });
    if (x <= pts[0][0]) return pts[0][1];
    if (x >= pts[pts.length - 1][0]) return pts[pts.length - 1][1];
    for (i = 0; i < pts.length - 1; i++) {
      var a = pts[i];
      var b = pts[i + 1];
      if (x >= a[0] && x <= b[0]) {
        var u = (x - a[0]) / ((b[0] - a[0]) || 1);
        return a[1] + (b[1] - a[1]) * u;
      }
    }
    return pts[0][1];
  }

  // Back palms that stand on the far bank, mirrored in the water.
  function palmReflection(ctx, x, top, len, t, i) {
    var wob = function (k) { return Math.sin(t * 1.6 + k * 1.1 + i) * 8; };
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
        ctx.fillRect(0, y0 + ((y1 - y0) * k) / 10, 1300, (y1 - y0) / 10 + 1);
      }
    }
    // Day water keeps the far palms. At night the mirror reads as a second grove.
    if (!look.night) mirrorFarPalms(ctx, t);
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
    // drinking rings, pinned inside the water under the mouth
    if (drink > 0.55 && muzzle) {
      var ringX = clamp(muzzle[0], 48, 1020);
      var ringY = poolRimY(ringX) + 12;
      ctx.strokeStyle = rgba(look.glint);
      ctx.lineWidth = 2.4;
      for (k = 0; k < 4; k++) {
        var rp = (t * 0.85 + k / 4) % 1;
        ctx.globalAlpha = BASE * clamp((1 - rp) * (drink - 0.4) * 2.6, 0, 0.95);
        ctx.beginPath();
        ctx.ellipse(ringX, ringY, 12 + rp * 86, 4.5 + rp * 18, 0, 0, TAU);
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

  // Traced palms. The trunk stays. Each crown point bends by how far it is from
  // the trunk, so the base stays attached and the tips sway.
  var PALM_IMG = null;
  function palmImage() {
    if (root.m1r4g3PalmImage) return root.m1r4g3PalmImage;
    if (PALM_IMG) return PALM_IMG;
    if (typeof Image === "undefined") return null;
    PALM_IMG = new Image();
    PALM_IMG.src = "palm-tall.png";
    return PALM_IMG;
  }
  // Around the shifted lake. Feet sit inside the far shore so the water covers the base.
  var SPRITE_PALMS = [
    { x: 230, h: 500, phase: 0.5, flip: false, bank: "far" },
    { x: 440, h: 580, phase: 1.9, flip: true, bank: "far" },
    { x: 760, h: 320, phase: 0.7, flip: false, bank: "far" },
    { x: 1080, h: 380, phase: 2.5, flip: true, bank: "far" },
  ];
  // Portrait may scale the lake and the palms together, same on both axes.
  // 1 leaves them at the picture size. Never squash only the vertical.
  var OASIS_SQUEEZE = 1;
  var OASIS_AX = 992;
  var OASIS_AY = 590;
  function withOasisSqueeze(ctx, draw) {
    if (OASIS_SQUEEZE === 1) {
      draw();
      return;
    }
    ctx.save();
    ctx.translate(OASIS_AX, OASIS_AY);
    ctx.scale(OASIS_SQUEEZE, OASIS_SQUEEZE);
    ctx.translate(-OASIS_AX, -OASIS_AY);
    draw();
    ctx.restore();
  }
  function palmSway(t, wind, i, phase) {
    return -wind * 34 + Math.sin(t * (0.72 + i * 0.05) + phase) * 26 * wind + Math.sin(t * 1.55 + phase) * 8;
  }
  function mirrorFarPalms(ctx, t) {
    var img = palmImage();
    if (!(img && img.complete && img.naturalWidth)) return;
    var wind = windAt(t);
    for (var i = 0; i < SPRITE_PALMS.length; i++) {
      var p = SPRITE_PALMS[i];
      if (p.bank !== "far") continue;
      var rim = poolRimY(p.x);
      ctx.save();
      ctx.translate(p.x, rim);
      ctx.scale(1, -0.3);
      ctx.globalAlpha = BASE * 0.4;
      drawSwayPalm(ctx, img, 0, 0, p.h, palmSway(t, wind, i, p.phase), p.flip);
      ctx.restore();
    }
  }
  function drawSwayPalm(ctx, img, x, base, h, sway, flip) {
    var w = h * (img.width / img.height);
    var left = x - w * 0.5;
    var top = base - h;
    var n = 40;
    for (var i = 0; i < n; i++) {
      var u0 = i / n;
      var u1 = (i + 1) / n;
      var sy = (img.height * u0) | 0;
      var sh = Math.max(1, ((img.height * u1) | 0) - sy + 1);
      if (sy + sh > img.height) sh = img.height - sy;
      var k = Math.pow(1 - (u0 + u1) * 0.5, 2.6);
      var dx = sway * k;
      var dh = (sh / img.height) * h + 0.5;
      var dy = top + h * u0;
      if (flip) ctx.drawImage(img, 0, sy, img.width, sh, left + w + dx, dy, -w, dh);
      else ctx.drawImage(img, 0, sy, img.width, sh, left + dx, dy, w, dh);
    }
  }
  function tracedPalms(ctx, t, bank) {
    var img = palmImage();
    if (!(img && img.complete && img.naturalWidth)) return false;
    var wind = windAt(t);
    for (var i = 0; i < SPRITE_PALMS.length; i++) {
      var p = SPRITE_PALMS[i];
      if (bank && p.bank !== bank) continue;
      var base = p.bank === "far" ? poolRimY(p.x) + 46 : poolBottomY(p.x) + 46;
      drawSwayPalm(ctx, img, p.x, base, p.h, palmSway(t, wind, i, p.phase), p.flip);
    }
    return true;
  }
  function crownReach(layers, px, py) {
    var maxd = 30;
    for (var l = 0; l < layers.length; l++) {
      var loops = layers[l].p;
      for (var j = 0; j < loops.length; j++) {
        var P = loops[j];
        for (var i = 0; i < P.length; i += 2) {
          var dx = P[i] - px;
          var dy = P[i + 1] - py;
          var d = Math.sqrt(dx * dx + dy * dy);
          if (d > maxd) maxd = d;
        }
      }
    }
    return Math.max(30, maxd - 8);
  }
  // Cubic deflection. The joint stays on the trunk; only the tips travel,
  // so a frond bends instead of hinging open.
  function bendPt(x, y, px, py, ang, reach) {
    var dx = x - px;
    var dy = y - py;
    var d = Math.sqrt(dx * dx + dy * dy);
    var u = d / (reach || 1);
    if (u > 1) u = 1;
    var w = u * u * u;
    var off = ang * reach * w;
    return [x + off, y + Math.abs(off) * 0.12];
  }
  function drawBent(ctx, layers, px, py, ang, reach) {
    for (var l = 0; l < layers.length; l++) {
      var loops = layers[l].p;
      ctx.beginPath();
      for (var j = 0; j < loops.length; j++) {
        var P = loops[j];
        var n = P.length / 2;
        var prev = bendPt(P[2 * n - 2], P[2 * n - 1], px, py, ang, reach);
        var first = bendPt(P[0], P[1], px, py, ang, reach);
        ctx.moveTo((prev[0] + first[0]) / 2, (prev[1] + first[1]) / 2);
        for (var i = 0; i < n; i++) {
          var c = bendPt(P[2 * i], P[2 * i + 1], px, py, ang, reach);
          var nx = P[(2 * i + 2) % (2 * n)];
          var ny = P[(2 * i + 3) % (2 * n)];
          var nxt = bendPt(nx, ny, px, py, ang, reach);
          if (SMOOTH) ctx.quadraticCurveTo(c[0], c[1], (c[0] + nxt[0]) / 2, (c[1] + nxt[1]) / 2);
          else ctx.lineTo(c[0], c[1]);
        }
        ctx.closePath();
      }
      ctx.fillStyle = layers[l].c;
      ctx.fill();
    }
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

  // The cartoon picture is the skin. The rig under it (toon-skin.js) was
  // measured on that picture. The neck is a chain of cross-sections: each
  // station turns by its own weight, so the arc stays smooth. The scarf tail
  // still swings from its own root. At rest the mesh lays the picture back down.
  function spinAbout(pt, piv, a) {
    if (!a) return pt;
    var dx = pt[0] - piv[0];
    var dy = pt[1] - piv[1];
    var c = Math.cos(a);
    var s = Math.sin(a);
    return [piv[0] + dx * c - dy * s, piv[1] + dx * s + dy * c];
  }
  function toonNeckAngle(drink, t) {
    var rig = root.m1r4g3ToonSkin;
    var ang = drink * rig.neck.ang;
    if (drink > 0.9) ang += Math.sin(t * 5) * 0.03 * drink;
    return ang;
  }
  function toonFlap(t) {
    var wind = windAt(t);
    return -wind * 0.16 + Math.sin(t * 2.6) * 0.1 * wind + Math.sin(t * 5.8) * 0.04;
  }
  function neckFrame(part, ang) {
    if (part._frameAng === ang && part._frame) return part._frame;
    var stations = part.w.length / 2;
    var i, k;
    if (!part._restC) {
      part._restC = [];
      for (i = 0; i < stations; i++) {
        var left = vert(part, i * 2);
        var right = vert(part, i * 2 + 1);
        part._restC.push([(left[0] + right[0]) / 2, (left[1] + right[1]) / 2]);
      }
    }
    var dest = new Array(part.w.length);
    var centers = new Array(stations);
    centers[0] = part._restC[0];
    dest[0] = vert(part, 0);
    dest[1] = vert(part, 1);
    for (i = 1; i < stations; i++) {
      var rot = part.w[i * 2] * ang;
      var prev = part._restC[i - 1];
      var cur = part._restC[i];
      var dx = cur[0] - prev[0];
      var dy = cur[1] - prev[1];
      var co = Math.cos(rot);
      var si = Math.sin(rot);
      var pc = centers[i - 1];
      centers[i] = [pc[0] + dx * co - dy * si, pc[1] + dx * si + dy * co];
      for (k = 0; k < 2; k++) {
        var v = vert(part, i * 2 + k);
        var ox = v[0] - cur[0];
        var oy = v[1] - cur[1];
        dest[i * 2 + k] = [centers[i][0] + ox * co - oy * si, centers[i][1] + ox * si + oy * co];
      }
    }
    part._frame = { dest: dest, centers: centers };
    part._frameAng = ang;
    return part._frame;
  }
  function toonMuzzle(drink, t) {
    var part = root.m1r4g3ToonSkin.neck;
    var ang = toonNeckAngle(drink, t);
    var frame = neckFrame(part, ang);
    if (part._mz == null) {
      var best = 0;
      var bestD = 1e9;
      var mx = part.muzzle[0];
      var my = part.muzzle[1];
      for (var i = 0; i < part._restC.length; i++) {
        var c = part._restC[i];
        var ddx = c[0] - mx;
        var ddy = c[1] - my;
        var d = ddx * ddx + ddy * ddy;
        if (d < bestD) {
          bestD = d;
          best = i;
        }
      }
      part._mz = best;
    }
    var si = part._mz;
    var cur = part._restC[si];
    var rot = part.w[si * 2] * ang;
    var co = Math.cos(rot);
    var sn = Math.sin(rot);
    var ox = part.muzzle[0] - cur[0];
    var oy = part.muzzle[1] - cur[1];
    var pc = frame.centers[si];
    return [pc[0] + ox * co - oy * sn, pc[1] + ox * sn + oy * co];
  }
  function vert(part, i) {
    return [part.xy[i * 2], part.xy[i * 2 + 1]];
  }
  function fillSkinTri(g, p0, p1, p2, grow) {
    var cx = (p0[0] + p1[0] + p2[0]) / 3;
    var cy = (p0[1] + p1[1] + p2[1]) / 3;
    function sh(p) {
      var dx = p[0] - cx;
      var dy = p[1] - cy;
      var L = Math.sqrt(dx * dx + dy * dy) || 1;
      return [p[0] + (dx / L) * grow, p[1] + (dy / L) * grow];
    }
    var a = sh(p0);
    var b = sh(p1);
    var c = sh(p2);
    g.beginPath();
    g.moveTo(a[0], a[1]);
    g.lineTo(b[0], b[1]);
    g.lineTo(c[0], c[1]);
    g.closePath();
    g.fill();
  }
  function punchPart(g, part) {
    var tri = part.tri;
    var w = part.w;
    for (var t = 0; t < tri.length; t += 3) {
      var i0 = tri[t];
      var i1 = tri[t + 1];
      var i2 = tri[t + 2];
      if (w[i0] + w[i1] + w[i2] <= 0) continue;
      fillSkinTri(g, vert(part, i0), vert(part, i1), vert(part, i2), -0.35);
    }
  }
  function punchedSkin(img, rig) {
    if (img._punched) return img._punched;
    var c = document.createElement("canvas");
    c.width = img.width;
    c.height = img.height;
    var g = c.getContext("2d");
    g.drawImage(img, 0, 0);
    g.globalCompositeOperation = "destination-out";
    g.fillStyle = "#000";
    punchPart(g, rig.neck);
    punchPart(g, rig.shawl);
    img._punched = c;
    return c;
  }
  function blitSkinTri(ctx, img, s0, s1, s2, d0, d1, d2) {
    var x0 = s0[0];
    var y0 = s0[1];
    var x1 = s1[0];
    var y1 = s1[1];
    var x2 = s2[0];
    var y2 = s2[1];
    var den = x0 * (y1 - y2) + x1 * (y2 - y0) + x2 * (y0 - y1);
    if (den > -1e-4 && den < 1e-4) return;
    var u0 = d0[0];
    var v0 = d0[1];
    var u1 = d1[0];
    var v1 = d1[1];
    var u2 = d2[0];
    var v2 = d2[1];
    var a = (u0 * (y1 - y2) + u1 * (y2 - y0) + u2 * (y0 - y1)) / den;
    var b = (v0 * (y1 - y2) + v1 * (y2 - y0) + v2 * (y0 - y1)) / den;
    var cc = (u0 * (x2 - x1) + u1 * (x0 - x2) + u2 * (x1 - x0)) / den;
    var d = (v0 * (x2 - x1) + v1 * (x0 - x2) + v2 * (x1 - x0)) / den;
    var e = (u0 * (x1 * y2 - x2 * y1) + u1 * (x2 * y0 - x0 * y2) + u2 * (x0 * y1 - x1 * y0)) / den;
    var f = (v0 * (x1 * y2 - x2 * y1) + v1 * (x2 * y0 - x0 * y2) + v2 * (x0 * y1 - x1 * y0)) / den;
    var cx = (u0 + u1 + u2) / 3;
    var cy = (v0 + v1 + v2) / 3;
    function grow(u, v) {
      var dx = u - cx;
      var dy = v - cy;
      var L = Math.sqrt(dx * dx + dy * dy) || 1;
      return [u + (dx / L) * 0.8, v + (dy / L) * 0.8];
    }
    var g0 = grow(u0, v0);
    var g1 = grow(u1, v1);
    var g2 = grow(u2, v2);
    ctx.save();
    ctx.beginPath();
    ctx.moveTo(g0[0], g0[1]);
    ctx.lineTo(g1[0], g1[1]);
    ctx.lineTo(g2[0], g2[1]);
    ctx.closePath();
    ctx.clip();
    ctx.transform(a, b, cc, d, e, f);
    ctx.drawImage(img, 0, 0);
    ctx.restore();
  }
  function drawSkinPart(ctx, img, part, ang) {
    var tri = part.tri;
    var w = part.w;
    var piv = part.p;
    for (var t = 0; t < tri.length; t += 3) {
      var i0 = tri[t];
      var i1 = tri[t + 1];
      var i2 = tri[t + 2];
      if (w[i0] + w[i1] + w[i2] <= 0) continue;
      var s0 = vert(part, i0);
      var s1 = vert(part, i1);
      var s2 = vert(part, i2);
      blitSkinTri(ctx, img, s0, s1, s2, spinAbout(s0, piv, w[i0] * ang), spinAbout(s1, piv, w[i1] * ang), spinAbout(s2, piv, w[i2] * ang));
    }
  }
  function drawNeck(ctx, img, part, ang) {
    var dest = neckFrame(part, ang).dest;
    var tri = part.tri;
    var w = part.w;
    for (var ti = 0; ti < tri.length; ti += 3) {
      var i0 = tri[ti];
      var i1 = tri[ti + 1];
      var i2 = tri[ti + 2];
      if (w[i0] + w[i1] + w[i2] <= 0) continue;
      blitSkinTri(ctx, img, vert(part, i0), vert(part, i1), vert(part, i2), dest[i0], dest[i1], dest[i2]);
    }
  }
  function camel(ctx, t, pose, sun) {
    var skin = root.m1r4g3Skin;
    var rig = skin && skin._toon && root.m1r4g3ToonSkin;
    if (!rig) return;
    ctx.save();
    riderFrame(ctx, pose);
    var eyed = eyedImage(skin, eyeLook(sun));
    ctx.drawImage(punchedSkin(eyed, rig), 0, 0);
    drawSkinPart(ctx, eyed, rig.shawl, toonFlap(t));
    drawNeck(ctx, eyed, rig.neck, toonNeckAngle(pose.drink, t));
    toonPipe(ctx, t);
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

  function rider(_ctx, _t, _pose) {
    // The cartoon picture already contains the rider.
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

  // Both eyes are the same slit. The light sits inside that slit. Nothing is drawn outside it.
  var EYE_AT = [[544, 60], [567, 61]];
  var EYE_RX = 5;
  var EYE_RY = 2.4;
  function sunEyeColor(sun) {
    var c = mix([238, 226, 182], [255, 200, 140], (sun.set || 0) * 0.8);
    return [(c[0] / 4 | 0) * 4, (c[1] / 4 | 0) * 4, (c[2] / 4 | 0) * 4];
  }
  function eyeLook(sun) {
    if (sun && sun.alt > 3) {
      var c = sunEyeColor(sun);
      return {
        key: c[0] + "," + c[1] + "," + c[2],
        edge: "rgb(" + c[0] + "," + c[1] + "," + c[2] + ")",
        hot: "rgb(" + Math.min(255, c[0] + 14) + "," + Math.min(255, c[1] + 12) + "," + Math.min(255, c[2] + 20) + ")"
      };
    }
    return { key: "blue", edge: "rgb(40,176,255)", flat: true };
  }
  function paintEye(g, x, y, look) {
    g.save();
    g.beginPath();
    g.ellipse(x, y, EYE_RX, EYE_RY, 0, 0, TAU);
    g.clip();
    if (look.flat) {
      g.fillStyle = look.edge;
    } else {
      var grd = g.createRadialGradient(x - 1.3, y - 0.7, 0.2, x, y, EYE_RX);
      grd.addColorStop(0, look.hot);
      grd.addColorStop(0.38, look.edge);
      grd.addColorStop(1, look.edge);
      g.fillStyle = grd;
    }
    g.fillRect(x - EYE_RX - 2, y - EYE_RY - 2, EYE_RX * 2 + 4, EYE_RY * 2 + 4);
    g.restore();
  }
  function clearEyeBloom(g, w, h) {
    var id = g.getImageData(0, 0, w, h);
    var d = id.data;
    var y, x, i, r, gc, b;
    // The source bloom is a blue wash across the visor. Put that cloth back.
    // The only lit pixels after this are the two slits painted on top.
    for (y = 15; y < 165; y++) {
      for (x = 400; x < 670; x++) {
        i = (y * w + x) * 4;
        if (d[i + 3] < 200) continue;
        r = d[i];
        gc = d[i + 1];
        b = d[i + 2];
        // Blue wash across the visor, and the old white-hot core that reads as a pupil.
        if ((b > 22 && b > r + 8) || (x > 510 && x < 600 && y > 35 && y < 90 && r + gc + b > 90 && b + 12 >= r && b + 8 >= gc)) {
          d[i] = 5;
          d[i + 1] = 4;
          d[i + 2] = 3;
        }
      }
    }
    g.putImageData(id, 0, 0);
  }
  function eyedImage(img, look) {
    if (img._eyedKey === look.key && img._eyed) return img._eyed;
    var w = img.naturalWidth || img.width;
    var h = img.naturalHeight || img.height;
    var c = document.createElement("canvas");
    c.width = w;
    c.height = h;
    var g = c.getContext("2d");
    g.drawImage(img, 0, 0);
    clearEyeBloom(g, w, h);
    paintEye(g, EYE_AT[0][0], EYE_AT[0][1], look);
    paintEye(g, EYE_AT[1][0], EYE_AT[1][1], look);
    img._eyed = c;
    img._eyedKey = look.key;
    return c;
  }
  function eyeStamp(img, look) {
    if (img._stampKey === look.key && img._stamp) return img._stamp;
    var c = document.createElement("canvas");
    c.width = img.naturalWidth || img.width;
    c.height = img.naturalHeight || img.height;
    var g = c.getContext("2d");
    paintEye(g, EYE_AT[0][0], EYE_AT[0][1], look);
    paintEye(g, EYE_AT[1][0], EYE_AT[1][1], look);
    img._stamp = c;
    img._stampKey = look.key;
    return c;
  }
  function toonPipe(ctx, t) {
    var wind = windAt(t);
    ember(ctx, t, 1, 621, 119, 0.55);
    smoke(ctx, t, wind, 617, 110, 548, 104, false);
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

  function ember(ctx, t, a, ox, oy, sc) {
    if (ox == null) { ox = 614; oy = 96; }
    if (sc == null) sc = 1;
    var d = drawIn(t);
    var lift = d * 3 * sc;
    var glow = 0.45 + 0.55 * d + 0.1 * Math.sin(t * 9);
    ctx.globalAlpha = BASE * clamp(a * glow * 0.4, 0, 1);
    ctx.fillStyle = "#ff9a3a";
    ctx.beginPath();
    ctx.ellipse(ox, oy - lift, 13 * sc, 6 * sc, 0, 0, TAU);
    ctx.fill();
    ctx.globalAlpha = BASE * clamp(a * glow, 0, 1);
    ctx.fillStyle = "#ff6a1e";
    ctx.beginPath();
    ctx.ellipse(ox, oy - lift, 8 * sc, 2.6 * sc, 0, 0, TAU);
    ctx.fill();
    ctx.globalAlpha = BASE;
  }

  function smoke(ctx, t, wind, bx, by, mx, my, puff) {
    var lift = drawIn(t) * 3;
    if (bx == null) { bx = 614; by = 92; mx = 600; my = 112; }
    by -= lift;
    my -= lift;
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
    // exhaled puff drifting out of the veil. The cartoon keeps only the pipe smoke.
    var ph = puffPhase(t);
    if (puff !== false && ph > 0.33 && ph < 0.75) {
      var q = (ph - 0.33) / 0.42;
      ctx.fillStyle = "#ece6d6";
      for (i = 0; i < 6; i++) {
        var qq = clamp(q * 1.15 - i * 0.03, 0, 1);
        ctx.globalAlpha = BASE * clamp((1 - qq) * 0.35, 0, 1);
        ctx.beginPath();
        ctx.arc(mx + qq * 46 - wind * qq * qq * 110 + Math.sin(i * 2 + t) * 5, my - qq * 70 - i * 5, 4 + qq * 18 + i * 2, 0, TAU);
        ctx.fill();
      }
    }
    ctx.globalAlpha = BASE;
  }

  // ---------- long shadows from the clock sun ----------

  function toonShadowSheet(img) {
    if (img._shadow) return img._shadow;
    var c = document.createElement("canvas");
    c.width = img.naturalWidth || img.width || 1120;
    c.height = img.naturalHeight || img.height || 912;
    var g = c.getContext("2d");
    g.drawImage(img, 0, 0);
    g.globalCompositeOperation = "source-in";
    g.fillStyle = "#240e08";
    g.fillRect(0, 0, c.width, c.height);
    img._shadow = c;
    return c;
  }
  function castToonShadow(ctx, sun, img) {
    var gx = 540;
    var gy = 890;
    // Length follows the altitude and eases at both ends, so the slider
    // does not stick around noon and then jump off a clamp.
    // side is 0 at noon: the cast sits under the hooves. It grows to the
    // left in the afternoon (sun on the right) and to the right in the morning.
    var u = clamp((36 - sun.alt) / 32, 0, 1);
    var reach = u * u * (3 - 2 * u);
    var side = sun.side || 0;
    var down = 0.025 + 0.155 * reach;
    var skew = side * (0.95 * reach);
    var horizonFade = clamp(sun.alt / 6, 0, 1);
    horizonFade = horizonFade * horizonFade * (3 - 2 * horizonFade);
    var alpha = clamp(0.22 + sun.day * 0.22, 0.18, 0.5) * horizonFade;
    if (alpha > 0.02) {
      ctx.save();
      ctx.translate(gx, gy);
      ctx.transform(1, 0, skew, -down, 0, 0);
      ctx.translate(-gx, -gy);
      ctx.globalAlpha = BASE * alpha;
      ctx.filter = "blur(" + (1.5 + reach * 1.1).toFixed(2) + "px)";
      ctx.drawImage(toonShadowSheet(img), 0, 0, 1120, 912);
      ctx.filter = "none";
      ctx.restore();
    }
    if (horizonFade < 0.98) {
      ctx.save();
      ctx.globalAlpha = BASE * 0.3 * (1 - horizonFade);
      ctx.fillStyle = "#240e08";
      ctx.beginPath();
      ctx.ellipse(gx, gy + 4, 168, 13, 0, 0, TAU);
      ctx.fill();
      ctx.restore();
    }
  }
  function castShadow(ctx, sun, _pose) {
    var skin = root.m1r4g3Skin;
    if (skin && skin._toon) castToonShadow(ctx, sun, skin);
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
    withOasisSqueeze(ctx, function () { tracedPalms(ctx, t, "far"); });
    layerOff(ctx, on, fx);

    on = layerOn(ctx, fx, 1);
    var muzzle = root.m1r4g3Skin && root.m1r4g3Skin._toon && root.m1r4g3ToonSkin ? toonMuzzle(pose.drink, t) : pose.muzzle;
    withOasisSqueeze(ctx, function () { pool(ctx, t, env.water, pose.drink, muzzle); });
    castShadow(ctx, sun, pose);
    layerOff(ctx, on, fx);

    on = layerOn(ctx, fx, 2);
    tracedPalms(ctx, t, "near");
    camel(ctx, t, pose, sun);
    layerOff(ctx, on, fx);

    on = layerOn(ctx, fx, 3);
    rider(ctx, t, pose);
    layerOff(ctx, on, fx);

    ctx.restore();
    BASE = 1;
  }

  // Night pass: blue eyes and the ember stay lit above the darkness overlay.
  function glowPass(ctx, t, env, fx, a, sun) {
    var skin = root.m1r4g3Skin;
    if (skin && !skin._toon) return;
    if (a < 0.02 || fx.alpha < 0.02) return;
    var pose = camelPose(t);
    ctx.save();
    if (fx.clip) fx.clip(ctx);
    oasisTransform(ctx, env, fx);
    BASE = clamp(fx.alpha, 0, 1);
    var on = layerOn(ctx, fx, 3);
    riderFrame(ctx, pose);
    if (skin && skin._toon) {
      if (sun && sun.alt <= 3) {
        var look = eyeLook(sun);
        ctx.globalAlpha = BASE;
        paintEye(ctx, EYE_AT[0][0], EYE_AT[0][1], look);
        paintEye(ctx, EYE_AT[1][0], EYE_AT[1][1], look);
      }
      ember(ctx, t, Math.max(a, 0.55), 621, 119, 0.55);
    }
    else {
      eyes(ctx, t, a);
      ember(ctx, t, a);
    }
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
      // мерцание: on/off flicker, probability follows the link
      var lit = rnd(frame * 13 + 1) < e * 1.1;
      fx.alpha = lit ? 0.55 + 0.45 * e : 0.08 * e;
      fx.dx = lit ? 0 : (rnd(frame) - 0.5) * 40 * g;
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
      // rolling scanlines
      for (i = 0; i < 40; i++) {
        y = ((i / 40) * h + t * h * 0.6) % h;
        ctx.globalAlpha = BASE * clamp(g * 0.25, 0, 1);
        ctx.fillStyle = i % 2 ? "#1a100c" : "#f6e2b0";
        ctx.fillRect(0, y, w, Math.max(1, h * 0.003));
      }
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
  function bigDune(ctx, w, h, horizon, lit, shade, seed, portrait) {
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
    // The crescent sits behind the legs in the vertical frame and reads as a
    // second shadow. The wide frame keeps it: there it is a dune fold.
    if (!portrait) {
      var cres = [P(0.215, 0.08), P(0.12, 0.15), P(0.1, 0.22), P(0.2, 0.29), P(0.4, 0.33), P(0.6, 0.335), P(0.7, 0.31), P(0.55, 0.28), P(0.38, 0.235), P(0.28, 0.17), P(0.26, 0.13)];
      ctx.beginPath();
      smoothLine(ctx, cres, true);
      ctx.closePath();
      ctx.fill();
    }
    ctx.strokeStyle = "#140e0a";
    ctx.lineJoin = "round";
    ctx.lineCap = "round";
    ctx.lineWidth = Math.max(5, h * 0.009);
    ctx.beginPath();
    smoothLine(ctx, crest.slice(1), true);
    smoothLine(ctx, ridge2, true);
    ctx.moveTo(P(0.86, 0.06)[0], P(0.86, 0.06)[1]);
    ctx.quadraticCurveTo(P(0.9, 0.1)[0], P(0.9, 0.1)[1], P(0.88, 0.15)[0], P(0.88, 0.15)[1]);
    ctx.stroke();
  }

  // Sepia spots and fibres live on the sky and the sand. The camel is drawn
  // later, so a spot cannot sit on the sky and on the head at the same time.
  function printSpots(ctx, w, h) {
    var m = Math.min(w, h);
    var i, k, r;
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
    ctx.globalAlpha = BASE;
    streaks(ctx, 120, 9500, 0, 0, w, h, Math.PI / 2, h * 0.06, "#3a1606", 1, 0.07, null);
  }

  // Fine paper grain over the whole print. The sepia wash stays; the blotches do not.
  function printTexture(ctx, w, h) {
    var m = Math.min(w, h);
    var i, r;
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
  // Faint over the empty horizon by day.
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
    var style = ((args[4] | 0) % 4 + 4) % 4;
    var target = args[5] == null ? (link > 0.5 ? 1 : 0) : +args[5];
    var appear = target > 0.5;
    var sun = solar(hour, day, dst);
    var lay = layout(w, h);
    var s = lay.s;
    SCENE_S = s;
    OASIS_SQUEEZE = 1;
    SMOOTH = s > 1.3;
    var horizon = lay.horizon;
    var light = 0.25 + 0.75 * sun.day;
    var gold = sun.set * (1 - sun.day * 0.7);

    // 1. sky, then sun and moon, still behind the dunes and the rider
    sky(ctx, w, h, horizon, sun);
    stars(ctx, w, horizon, t, sun.night);
    sunMoon(ctx, w, h, horizon, hour, sun);

    // 2. dunes, back to front (palette from 5.jpg)
    var rows = duneRows(w, h, horizon);
    var tint = function (c) { return mix(mix(c, [c[0] * 0.95, c[1] * 0.6, c[2] * 0.55], gold * 0.5), [c[0] * 0.3, c[1] * 0.28, c[2] * 0.4], (1 - light) * 0.4); };
    mirageBand(ctx, w, h, horizon - h * 0.15, horizon + h * 0.04, w * 0.02, w * 0.74, t, smooth(0.15, 0.6, sun.day) * (1 - smooth(0, 0.5, link)) * 0.8, 3);
    duneRow(ctx, w, h, rows[0].base, rows[0].peaks, tint([232, 148, 78]), tint([186, 96, 48]), 1, 0, 100);
    duneRow(ctx, w, h, rows[1].base, rows[1].peaks, tint([214, 118, 58]), tint([150, 72, 40]), 1, 0, 200);
    bigDune(ctx, w, h, horizon, tint([236, 142, 68]), tint([158, 74, 38]), 300, lay.portrait);
    printSpots(ctx, w, h);

    // 3. oasis: once
    var e = smooth(0, 1, link);
    var g = Math.sin(Math.PI * clamp(link, 0, 1));
    var night = sun.night;
    var dusk = gold;
    var wcol = function (day, set, nt) { return mix(mix(day, set, dusk), nt, night); };
    var env = {
      s: s,
      ox: lay.ox,
      oy: lay.oy,
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
        night: night,
      },
    };
    var fx = glitchFx(style, e, g, t, w, h, appear);
    if (fx.alpha > 0.005 && (link > 0.001)) oasis(ctx, t, env, fx, sun);

    // 4. wind, glitch overlays, print grain
    glitchOverlay(ctx, style, e, g, t, w, h, horizon, appear);
    halo(ctx, w, h, sun.day * (1 - sun.night));
    printTexture(ctx, w, h);
    vignette(ctx, w, h, 1.2);

    // 5. light level: winter / dusk / night dim everything; eyes and ember stay lit
    var dark = clamp((1 - sun.day) * 0.32 + sun.night * 0.42, 0, 0.72);
    if (dark > 0.01) {
      var duskCol = mix([60, 20, 40], [6, 10, 34], sun.night);
      var horizonU = clamp(horizon / h, 0, 1);
      var skyKeep = 1 - sun.night * 0.9;
      var row;
      for (row = 0; row < h; row++) {
        var ground = smooth(horizonU - 0.16, horizonU + 0.2, row / h);
        ctx.fillStyle = rgba(duskCol, dark * (skyKeep + (1 - skyKeep) * ground));
        ctx.fillRect(0, row, w, 1);
      }
    }
    if (gold > 0.05) {
      ctx.fillStyle = rgba([255, 90, 30], gold * 0.12 * (1 - sun.night));
      ctx.fillRect(0, 0, w, h);
    }
    if (link > 0.001) glowPass(ctx, t, env, fx, clamp(dark * 1.6, 0, 1), sun);
    ctx.globalAlpha = BASE;
    return sun;
  }

  var root = typeof globalThis !== "undefined" ? globalThis : window;
  root.uzorAnims = root.uzorAnims || {};
  root.uzorAnims.desert = draw;
  root.drawDesert = draw;
  root.m1r4g3Solar = solar;
})();
