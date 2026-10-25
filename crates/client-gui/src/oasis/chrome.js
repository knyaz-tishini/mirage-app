// Menu mark. No fill: an amber frame and a stroked palm, no sun.
// Amber is the shell accent, shell/menu.rs AMBER (#FBB26A).

(function () {
  var AMBER = "#FBB26A";

  function rrect(ctx, x, y, w, h, r) {
    ctx.beginPath();
    ctx.moveTo(x + r, y);
    ctx.lineTo(x + w - r, y);
    ctx.quadraticCurveTo(x + w, y, x + w, y + r);
    ctx.lineTo(x + w, y + h - r);
    ctx.quadraticCurveTo(x + w, y + h, x + w - r, y + h);
    ctx.lineTo(x + r, y + h);
    ctx.quadraticCurveTo(x, y + h, x, y + h - r);
    ctx.lineTo(x, y + r);
    ctx.quadraticCurveTo(x, y, x + r, y);
    ctx.closePath();
  }

  function frond(ctx, x0, y0, cx, cy, x1, y1) {
    ctx.beginPath();
    ctx.moveTo(x0, y0);
    ctx.quadraticCurveTo(cx, cy, x1, y1);
    ctx.stroke();
  }

  function draw(ctx, _t, p) {
    var w = p.w;
    var h = p.h;
    var s = Math.min(w, h);
    var pen = Math.max(1.2, s * 0.055);
    ctx.strokeStyle = AMBER;
    ctx.lineWidth = pen;
    var inset = pen;
    var rw = w - inset * 2;
    var rh = h - inset * 2;
    rrect(ctx, inset, inset, rw, rh, s * 0.22);
    ctx.stroke();

    var cx = w * 0.5;
    var crown = h * 0.5;
    var base = h * 0.78;
    ctx.lineWidth = pen * 1.35;
    ctx.beginPath();
    ctx.moveTo(cx, base);
    ctx.lineTo(cx, crown);
    ctx.stroke();

    ctx.lineWidth = pen;
    var arms = [
      [0.0, -0.26, 0.0, -0.14],
      [-0.16, -0.22, -0.24, -0.06],
      [0.16, -0.22, 0.24, -0.06],
      [-0.24, -0.02, -0.22, 0.1],
      [0.24, -0.02, 0.22, 0.1],
      [-0.12, 0.1, -0.18, 0.16],
      [0.12, 0.1, 0.18, 0.16],
    ];
    var i;
    for (i = 0; i < arms.length; i++) {
      var a = arms[i];
      frond(ctx, cx, crown, cx + a[0] * s, crown + a[1] * s, cx + a[2] * s, crown + a[3] * s);
    }
  }

  var root = typeof globalThis !== "undefined" ? globalThis : this;
  root.uzorAnims = root.uzorAnims || {};
  root.uzorAnims.chrome = draw;
})();
