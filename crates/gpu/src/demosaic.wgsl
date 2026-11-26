// GPU demosaic, mirroring pe_pipeline::demosaic::high_quality: green by Hamilton-Adams (2x2 Bayer,
// mirrored borders) or neighbourhood average (other CFAs), red/blue as colour differences, then a
// 3x3 median of R-G and B-G (false-colour suppression).

struct D {
    w: u32,
    h: u32,
    cfa_w: u32,
    cfa_h: u32,
    pattern: array<vec4<u32>, 9>,   // 36 colour indices (0 R, 1 G, 2 B), row-major over the CFA tile
};

@group(0) @binding(0) var<uniform> U: D;
@group(0) @binding(1) var<storage, read> mosaic: array<f32>;
@group(0) @binding(2) var<storage, read_write> green: array<f32>;
@group(0) @binding(3) var<storage, read_write> rgb: array<f32>;
@group(0) @binding(4) var<storage, read_write> diff: array<vec2<f32>>;

fn idx(gid: vec3<u32>, nwg: vec3<u32>) -> u32 { return gid.x + gid.y * nwg.x * 256u; }

fn color_at(y: u32, x: u32) -> u32 {
    let k = (y % U.cfa_h) * U.cfa_w + (x % U.cfa_w);
    return U.pattern[k / 4u][k % 4u];
}

fn reflect(v: i32, n: u32) -> u32 {
    let nn = i32(n);
    var r = v;
    if (v < 0) { r = -v; } else if (v >= nn) { r = 2 * (nn - 1) - v; }
    return u32(clamp(r, 0, nn - 1));
}

fn at(x: i32, y: i32) -> f32 { return mosaic[reflect(y, U.h) * U.w + reflect(x, U.w)]; }

// Mean of same-colour samples (`data` = mosaic, minus green when `use_green`) in a (2r+1)^2 window,
// excluding nothing (the centre never has the colour being filled). Returns (mean, found).
fn window_mean(x: u32, y: u32, color: u32, r: u32, use_green: bool) -> vec2<f32> {
    let x0 = select(x - r, 0u, x < r);
    let y0 = select(y - r, 0u, y < r);
    let x1 = min(x + r, U.w - 1u);
    let y1 = min(y + r, U.h - 1u);
    var sum = 0.0;
    var n = 0u;
    for (var yy = y0; yy <= y1; yy++) {
        for (var xx = x0; xx <= x1; xx++) {
            if (color_at(yy, xx) == color) {
                let i = yy * U.w + xx;
                var v = mosaic[i];
                if (use_green) { v -= green[i]; }
                sum += v;
                n++;
            }
        }
    }
    if (n == 0u) { return vec2<f32>(0.0, 0.0); }
    return vec2<f32>(sum / f32(n), 1.0);
}

@compute @workgroup_size(256)
fn green_pass(@builtin(global_invocation_id) gid: vec3<u32>, @builtin(num_workgroups) nwg: vec3<u32>) {
    let i = idx(gid, nwg);
    if (i >= U.w * U.h) { return; }
    let x = i % U.w;
    let y = i / U.w;
    let c = mosaic[i];
    if (color_at(y, x) == 1u) { green[i] = c; return; }
    if (U.cfa_w == 2u && U.cfa_h == 2u) {
        let xi = i32(x);
        let yi = i32(y);
        let gl = at(xi - 1, yi);
        let gr = at(xi + 1, yi);
        let gu = at(xi, yi - 1);
        let gd = at(xi, yi + 1);
        let lap_h = 2.0 * c - at(xi - 2, yi) - at(xi + 2, yi);
        let lap_v = 2.0 * c - at(xi, yi - 2) - at(xi, yi + 2);
        let grad_h = abs(gl - gr) + abs(lap_h);
        let grad_v = abs(gu - gd) + abs(lap_v);
        let est_h = (gl + gr) / 2.0 + lap_h / 4.0;
        let est_v = (gu + gd) / 2.0 + lap_v / 4.0;
        var est = (est_h + est_v) / 2.0;
        let tie = abs(grad_h - grad_v) <= 1.0e-6 + 1.0e-4 * (grad_h + grad_v);
        if (!tie) { if (grad_h < grad_v) { est = est_h; } else { est = est_v; } }
        let lo = min(min(gl, gr), min(gu, gd));
        let hi = max(max(gl, gr), max(gu, gd));
        green[i] = clamp(est, lo, hi);
    } else {
        var m = window_mean(x, y, 1u, 1u, false);
        if (m.y == 0.0) { m = window_mean(x, y, 1u, 2u, false); }
        green[i] = m.x;
    }
}

@compute @workgroup_size(256)
fn colour_pass(@builtin(global_invocation_id) gid: vec3<u32>, @builtin(num_workgroups) nwg: vec3<u32>) {
    let i = idx(gid, nwg);
    if (i >= U.w * U.h) { return; }
    let x = i % U.w;
    let y = i / U.w;
    let own = color_at(y, x);
    let g = green[i];
    var px = vec3<f32>(0.0, g, 0.0);
    for (var k = 0u; k < 2u; k++) {
        let c = k * 2u;
        var v = 0.0;
        if (c == own) {
            v = mosaic[i];
        } else {
            var m = window_mean(x, y, c, 1u, true);
            if (m.y == 0.0) { m = window_mean(x, y, c, 2u, true); }
            v = g + m.x;
        }
        px[c] = v;
    }
    rgb[3u * i] = px.x;
    rgb[3u * i + 1u] = px.y;
    rgb[3u * i + 2u] = px.z;
    diff[i] = vec2<f32>(px.x - px.y, px.z - px.y);
}

fn median9(v: array<f32, 9>) -> f32 {
    var a = v;
    // Partial selection sort up to the middle element (same result as a full sort's median).
    for (var i = 0; i < 5; i++) {
        var m = i;
        for (var j = i + 1; j < 9; j++) { if (a[j] < a[m]) { m = j; } }
        let t = a[i]; a[i] = a[m]; a[m] = t;
    }
    return a[4];
}

@compute @workgroup_size(256)
fn median_pass(@builtin(global_invocation_id) gid: vec3<u32>, @builtin(num_workgroups) nwg: vec3<u32>) {
    let i = idx(gid, nwg);
    if (i >= U.w * U.h) { return; }
    let x = i % U.w;
    let y = i / U.w;
    let xs = array<u32, 3>(select(x - 1u, 0u, x == 0u), x, min(x + 1u, U.w - 1u));
    let ys = array<u32, 3>(select(y - 1u, 0u, y == 0u), y, min(y + 1u, U.h - 1u));
    var rg: array<f32, 9>;
    var bg: array<f32, 9>;
    var k = 0;
    for (var a = 0; a < 3; a++) {
        for (var b = 0; b < 3; b++) {
            let d = diff[ys[a] * U.w + xs[b]];
            rg[k] = d.x;
            bg[k] = d.y;
            k++;
        }
    }
    let g = rgb[3u * i + 1u];
    rgb[3u * i] = g + median9(rg);
    rgb[3u * i + 2u] = g + median9(bg);
}
