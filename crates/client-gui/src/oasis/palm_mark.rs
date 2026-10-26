//! Menu mark. Vectors from `assets/palm.svg`, parsed once. The same file is
//! rasterized only where a platform asks for pixels: the exe icon, the tray,
//! the window icon, and the Apple icon set.

use std::sync::OnceLock;

use uzor::render::RenderContext;

const SVG: &str = include_str!("../../assets/palm.svg");

enum Op {
    Move(f64, f64),
    Line(f64, f64),
    Close,
}

struct Shape {
    fill: Option<String>,
    stroke: Option<(String, f64)>,
    ops: Vec<Op>,
}

struct Mark {
    tx: f64,
    ty: f64,
    shapes: Vec<Shape>,
}

pub fn paint(ctx: &mut dyn RenderContext, x: f64, y: f64, w: f64, h: f64) {
    let mark = mark();
    let s = w.min(h) / 1024.0;
    let ox = x + (w - 1024.0 * s) * 0.5;
    let oy = y + (h - 1024.0 * s) * 0.5;
    for shape in &mark.shapes {
        trace(ctx, shape, ox, oy, s, mark.tx, mark.ty);
        if let Some(color) = &shape.fill {
            ctx.set_fill_color(color);
            ctx.fill();
        }
        if let Some((color, width)) = &shape.stroke {
            if shape.fill.is_some() {
                trace(ctx, shape, ox, oy, s, mark.tx, mark.ty);
            }
            ctx.set_stroke_color(color);
            ctx.set_stroke_width(width * s);
            ctx.set_line_join("miter");
            ctx.set_line_cap("butt");
            ctx.set_line_dash(&[]);
            ctx.stroke();
        }
    }
}

fn trace(ctx: &mut dyn RenderContext, shape: &Shape, ox: f64, oy: f64, s: f64, tx: f64, ty: f64) {
    ctx.begin_path();
    for op in &shape.ops {
        match *op {
            Op::Move(px, py) => ctx.move_to(ox + (px + tx) * s, oy + (py + ty) * s),
            Op::Line(px, py) => ctx.line_to(ox + (px + tx) * s, oy + (py + ty) * s),
            Op::Close => ctx.close_path(),
        }
    }
}

fn mark() -> &'static Mark {
    static MARK: OnceLock<Mark> = OnceLock::new();
    MARK.get_or_init(parse)
}

fn parse() -> Mark {
    let (tx, ty) = translate(SVG);
    let mut shapes = Vec::new();
    let mut rest = SVG;
    while let Some(at) = rest.find("<path") {
        rest = &rest[at..];
        let end = rest.find("/>").unwrap_or(rest.len());
        let tag = &rest[..end];
        rest = &rest[end..];
        let Some(d) = attr(tag, "d") else { continue };
        let mut ops = parse_ops(d);
        simplify(&mut ops, 2.0);
        if ops.is_empty() {
            continue;
        }
        let width = attr(tag, "stroke-width").and_then(|v| v.parse().ok());
        shapes.push(Shape {
            fill: match attr(tag, "fill") {
                Some("none") | None => None,
                Some(color) => Some(color.to_string()),
            },
            stroke: match attr(tag, "stroke") {
                Some("none") | None => None,
                Some(color) => Some((color.to_string(), width.unwrap_or(10.0))),
            },
            ops,
        });
    }
    Mark { tx, ty, shapes }
}

fn translate(svg: &str) -> (f64, f64) {
    let Some(at) = svg.find("translate(") else { return (0.0, 0.0) };
    let inner = &svg[at + "translate(".len()..];
    let Some(end) = inner.find(')') else { return (0.0, 0.0) };
    let nums: Vec<f64> = inner[..end]
        .split([',', ' '])
        .filter(|s| !s.is_empty())
        .filter_map(|s| s.parse().ok())
        .collect();
    match nums.as_slice() {
        [x, y, ..] => (*x, *y),
        [x] => (*x, 0.0),
        _ => (0.0, 0.0),
    }
}

fn attr<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let key = format!("{name}=\"");
    let at = tag.find(&key)? + key.len();
    let end = tag[at..].find('"')? + at;
    Some(&tag[at..end])
}

fn parse_ops(d: &str) -> Vec<Op> {
    let b = d.as_bytes();
    let mut i = 0;
    let mut cmd = b'M';
    let mut ops = Vec::new();
    while i < b.len() {
        while i < b.len() && (b[i] == b' ' || b[i] == b',') {
            i += 1;
        }
        if i >= b.len() {
            break;
        }
        if b[i].is_ascii_alphabetic() {
            cmd = b[i];
            i += 1;
            if cmd == b'Z' || cmd == b'z' {
                ops.push(Op::Close);
            }
            continue;
        }
        if cmd == b'M' || cmd == b'L' {
            let (x, i2) = num(b, i);
            let (y, i3) = num(b, i2);
            i = i3;
            if cmd == b'M' {
                ops.push(Op::Move(x, y));
                cmd = b'L';
            } else {
                ops.push(Op::Line(x, y));
            }
        } else {
            i += 1;
        }
    }
    ops
}

fn num(b: &[u8], mut i: usize) -> (f64, usize) {
    while i < b.len() && (b[i] == b' ' || b[i] == b',') {
        i += 1;
    }
    let start = i;
    if i < b.len() && (b[i] == b'-' || b[i] == b'+') {
        i += 1;
    }
    while i < b.len() && (b[i].is_ascii_digit() || b[i] == b'.') {
        i += 1;
    }
    let v = std::str::from_utf8(&b[start..i]).ok().and_then(|s| s.parse().ok()).unwrap_or(0.0);
    (v, i)
}

fn simplify(ops: &mut Vec<Op>, eps: f64) {
    let src = std::mem::take(ops);
    let mut ring: Vec<(f64, f64)> = Vec::new();
    let mut open = false;
    for op in src {
        match op {
            Op::Move(x, y) => {
                flush(ops, &mut ring, open, eps);
                ops.push(Op::Move(x, y));
                ring.clear();
                open = true;
            }
            Op::Line(x, y) => ring.push((x, y)),
            Op::Close => {
                flush(ops, &mut ring, open, eps);
                ops.push(Op::Close);
                open = false;
            }
        }
    }
    flush(ops, &mut ring, open, eps);
}

fn flush(out: &mut Vec<Op>, ring: &mut Vec<(f64, f64)>, open: bool, eps: f64) {
    if !open || ring.is_empty() {
        ring.clear();
        return;
    }
    let pts = if ring.len() >= 8 { simplify_closed(ring, eps) } else { ring.clone() };
    for (x, y) in pts {
        out.push(Op::Line(x, y));
    }
    ring.clear();
}

fn simplify_closed(pts: &[(f64, f64)], eps: f64) -> Vec<(f64, f64)> {
    let mut a = 0;
    for i in 1..pts.len() {
        if pts[i].0 < pts[a].0 {
            a = i;
        }
    }
    let mut b = 0;
    let mut best = -1.0;
    for i in 0..pts.len() {
        let d = dist(pts[i], pts[a]);
        if d > best {
            best = d;
            b = i;
        }
    }
    let mut keep = vec![false; pts.len()];
    keep[a] = true;
    keep[b] = true;
    dp(pts, a, b, eps, &mut keep);
    dp(pts, b, a, eps, &mut keep);
    let mut ordered = Vec::new();
    for i in 0..pts.len() {
        let k = (a + i) % pts.len();
        if keep[k] {
            ordered.push(pts[k]);
        }
    }
    ordered
}

fn dp(pts: &[(f64, f64)], i: usize, j: usize, eps: f64, keep: &mut [bool]) {
    let n = pts.len();
    if i == j {
        return;
    }
    let mut max = 0.0;
    let mut at = None;
    let mut k = (i + 1) % n;
    let mut guard = 0;
    while k != j && guard < n {
        let d = perp(pts[i], pts[j], pts[k]);
        if d > max {
            max = d;
            at = Some(k);
        }
        k = (k + 1) % n;
        guard += 1;
    }
    if let Some(at) = at {
        if max > eps {
            keep[at] = true;
            dp(pts, i, at, eps, keep);
            dp(pts, at, j, eps, keep);
        }
    }
}

fn perp(a: (f64, f64), b: (f64, f64), p: (f64, f64)) -> f64 {
    let dx = b.0 - a.0;
    let dy = b.1 - a.1;
    let len = (dx * dx + dy * dy).sqrt();
    if len < 1e-6 {
        return dist(a, p);
    }
    ((p.0 - a.0) * dy - (p.1 - a.1) * dx).abs() / len
}

fn dist(a: (f64, f64), b: (f64, f64)) -> f64 {
    let dx = a.0 - b.0;
    let dy = a.1 - b.1;
    (dx * dx + dy * dy).sqrt()
}
