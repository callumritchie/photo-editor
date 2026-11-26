// GPU version of the develop pipeline for linear-RGB (already demosaiced or reduced) frames. Every
// entry point mirrors a function of the CPU reference in pe-pipeline; tests check parity (CIEDE2000).
//
// Intermediate images live in one pool of vec4 planes. Generic kernels (downscale, min filter, box
// blur, guided-filter coefficients) read and write planes chosen by a per-pass uniform `Q`.

struct Params {
    wb: vec4<f32>,        // xyz: camera white-balance multipliers; w: exposure gain
    m0: vec4<f32>,        // camera -> Rec.2020 rows
    m1: vec4<f32>,
    m2: vec4<f32>,
    o0: vec4<f32>,        // Rec.2020 -> output space rows
    o1: vec4<f32>,
    o2: vec4<f32>,
    out_luma: vec4<f32>,  // output-space luminance weights
    tone: vec4<f32>,      // contrast power, white norm, black level, log2(mid grey)
    local: vec4<f32>,     // shadow EV, highlight EV, clarity gain, texture gain
    dims: vec4<u32>,      // source width, height; reduced width, height
    dims2: vec4<u32>,     // reduction factor, unused, orientation, has curve
    dims3: vec4<u32>,     // output width, height, unused, unused
    haze: vec4<f32>,      // airlight rgb, airlight luminance
    haze2: vec4<f32>,     // omega (dehaze) , beta (add haze), unused, unused
    offs: vec4<u32>,      // pool offsets: scene, shadows/highlights coeffs, clarity coeffs, texture blur
    flags: vec4<u32>,     // shadows/highlights, clarity, texture, dehaze coefficients offset
    geo0: vec4<f32>,      // crop left, top, right, bottom
    geo1: vec4<f32>,      // cos, sin (straighten), perspective kx, ky
    geo2: vec4<f32>,      // 1 / scale, distortion k1, lens vignetting (stops), unused
    geo3: vec4<f32>,      // chromatic aberration scale R, G, B, unused
    geo4: vec4<u32>,      // corrected frame width, height, unused, unused
    full: vec4<u32>,      // whole sensor frame width, height; this frame's origin x, y
    outr: vec4<u32>,      // output origin x, y; whole-picture output width, height
    offs2: vec4<u32>,     // pool offsets: masked noise-reduction target (+ sharpness gain), dehaze target
};

// Per-pass parameters for the generic kernels.
struct Pass {
    src: u32,
    dst: u32,
    aux: u32,             // a second input plane, where used
    mode: u32,
    w: u32,               // destination (or source for blurs) width, height
    h: u32,
    sw: u32,              // source width, height (downscale)
    sh: u32,
    r: u32,               // radius or à trous hole
    pad0: u32,
    pad1: u32,
    pad2: u32,
    k: vec4<f32>,
};

@group(0) @binding(0) var<uniform> P: Params;
@group(0) @binding(1) var<storage, read> src: array<f32>;
@group(0) @binding(2) var<storage, read_write> pool: array<vec4<f32>>;
@group(0) @binding(3) var<storage, read> lut: array<f32>;
@group(0) @binding(4) var<storage, read_write> out_rgba: array<u32>;

// Colour stage constants (pe_pipeline::look::LookParams).
struct Look {
    flags: vec4<f32>,                  // enabled, monochrome, saturation gain, vibrance
    lms: array<vec4<f32>, 3>,          // linear Rec.2020 -> LMS rows
    rgb: array<vec4<f32>, 3>,          // LMS -> linear Rec.2020 rows
    centres: array<vec4<f32>, 2>,      // hue band centres (degrees)
    band_hue: array<vec4<f32>, 2>,
    band_sat: array<vec4<f32>, 2>,
    band_lum: array<vec4<f32>, 2>,
    band_bw: array<vec4<f32>, 2>,
    grade: array<vec4<f32>, 4>,        // (a, b, lightness) for shadows, midtones, highlights, global
    shape: vec4<f32>,                  // transition half-width, balance
    fx1: vec4<f32>,                    // vignette amount, inner radius, width, highlights
    fx2: vec4<f32>,                    // vignette axis scales, norm exponent, enabled
    fx3: vec4<f32>,                    // grain amount, cell size (px), roughness, enabled
};
@group(0) @binding(5) var<uniform> L: Look;
@group(0) @binding(6) var<uniform> Q: Pass;
// Masks (masks.rs), packed by the host: header (count, needs display, needs saturation; frame size;
// OKLab matrices), then 30 vec4 per mask. Raster and colour data in MD.
@group(0) @binding(7) var<storage, read> M: array<vec4<f32>>;
@group(0) @binding(8) var<storage, read> MD: array<f32>;

const CLIP: f32 = 0.985;
const MID_GREY: f32 = 0.18;
const REC2020_LUMA: vec3<f32> = vec3<f32>(0.2627, 0.677998, 0.059302);
const LUT_SIZE: u32 = 4096u;

fn index(gid: vec3<u32>, nwg: vec3<u32>) -> u32 {
    return gid.x + gid.y * nwg.x * 256u;
}

fn smoothstep_(e0: f32, e1: f32, x: f32) -> f32 {
    let t = clamp((x - e0) / (e1 - e0), 0.0, 1.0);
    return t * t * (3.0 - 2.0 * t);
}

fn srgb_encode(x: f32) -> f32 {
    if (x <= 0.0031308) { return 12.92 * x; }
    return 1.055 * pow(x, 1.0 / 2.4) - 0.055;
}

fn srgb_decode(v: f32) -> f32 {
    if (v <= 0.04045) { return v / 12.92; }
    return pow((v + 0.055) / 1.055, 2.4);
}

// --- Stage 2-3: white balance with highlight reconstruction, camera -> Rec.2020, exposure ---------

fn reconstruct(cam: vec3<f32>) -> vec3<f32> {
    let clipped = cam >= vec3<f32>(CLIP);
    var c = max(cam, vec3<f32>(0.0)) * P.wb.xyz;
    if (any(clipped)) {
        var reference = -1.0e30;
        for (var i = 0; i < 3; i++) {
            if (!clipped[i]) { reference = max(reference, c[i]); }
        }
        if (reference < -1.0e29) { reference = max(c.x, max(c.y, c.z)); }
        for (var i = 0; i < 3; i++) {
            if (clipped[i]) { c[i] = max(c[i], reference); }
        }
    }
    return c;
}


fn luma(c: vec3<f32>) -> f32 { return dot(c, REC2020_LUMA); }

// --- Stage 2-3 -> pool[scene] = (Rec.2020 rgb, log2 Y) ---------------------------------------------

@compute @workgroup_size(256)
fn scene_pass(@builtin(global_invocation_id) gid: vec3<u32>, @builtin(num_workgroups) nwg: vec3<u32>) {
    let i = index(gid, nwg);
    if (i >= P.dims.x * P.dims.y) { return; }
    let cam = vec3<f32>(src[3u * i], src[3u * i + 1u], src[3u * i + 2u]);
    let b = reconstruct(cam);
    var w = vec3<f32>(dot(P.m0.xyz, b), dot(P.m1.xyz, b), dot(P.m2.xyz, b)) * P.wb.w;
    if (P.geo2.z != 0.0) {
        let sw = f32(P.full.x);
        let sh = f32(P.full.y);
        let dx = f32(i % P.dims.x + P.full.z) + 0.5 - sw * 0.5;
        let dy = f32(i / P.dims.x + P.full.w) + 0.5 - sh * 0.5;
        w *= exp2(P.geo2.z * (dx * dx + dy * dy) / (sw * sw * 0.25 + sh * sh * 0.25));
    }
    pool[P.offs.x + i] = vec4<f32>(w, log2(max(luma(w), 1.0e-6)));
}

// --- Generic plane kernels (local.rs, filter.rs) -----------------------------------------------------

// Block-average downscale of the scene plane. mode 0: (dark channel / airlight, guide, 0, 0) for
// dehaze; mode 1: (log Y, log Y, log Y^2, log Y^2) for a self-guided filter.
@compute @workgroup_size(256)
fn downscale(@builtin(global_invocation_id) gid: vec3<u32>, @builtin(num_workgroups) nwg: vec3<u32>) {
    let i = index(gid, nwg);
    if (i >= Q.w * Q.h) { return; }
    let f = P.dims2.x;
    let x2 = i % Q.w;
    let y2 = i / Q.w;
    var s = vec4<f32>(0.0);
    var n = 0.0;
    for (var y = y2 * f; y < min((y2 + 1u) * f, Q.sh); y++) {
        for (var x = x2 * f; x < min((x2 + 1u) * f, Q.sw); x++) {
            s += pool[Q.src + y * Q.sw + x];
            n += 1.0;
        }
    }
    let m = s / max(n, 1.0);
    if (Q.mode == 0u) {
        let a = P.haze.xyz;
        let dark = min(m.x / a.x, min(m.y / a.y, m.z / a.z));
        pool[Q.dst + i] = vec4<f32>(dark, clamp(luma(m.xyz) / P.haze.w, 0.0, 2.0), 0.0, 0.0);
    } else {
        pool[Q.dst + i] = vec4<f32>(m.w, m.w, m.w * m.w, m.w * m.w);
    }
}

// Clamped-window minimum of .x (other channels kept). mode 0 horizontal, 1 vertical.
@compute @workgroup_size(256)
fn min_filter(@builtin(global_invocation_id) gid: vec3<u32>, @builtin(num_workgroups) nwg: vec3<u32>) {
    let i = index(gid, nwg);
    if (i >= Q.w * Q.h) { return; }
    let r = i32(Q.r);
    let x = i32(i % Q.w);
    let y = i32(i / Q.w);
    var m = 1.0e30;
    if (Q.mode == 0u) {
        for (var k = max(x - r, 0); k < min(x + r + 1, i32(Q.w)); k++) { m = min(m, pool[Q.src + u32(y) * Q.w + u32(k)].x); }
    } else {
        for (var k = max(y - r, 0); k < min(y + r + 1, i32(Q.h)); k++) { m = min(m, pool[Q.src + u32(k) * Q.w + u32(x)].x); }
    }
    var v = pool[Q.src + i];
    v.x = m;
    pool[Q.dst + i] = v;
}

// Dehaze guided-filter input: (dark, guide) -> (I, p, I p, I I) with I = guide, p = 1 - omega dark.
@compute @workgroup_size(256)
fn dehaze_input(@builtin(global_invocation_id) gid: vec3<u32>, @builtin(num_workgroups) nwg: vec3<u32>) {
    let i = index(gid, nwg);
    if (i >= Q.w * Q.h) { return; }
    let v = pool[Q.src + i];
    let g = v.y;
    let p = 1.0 - Q.k.x * v.x;   // omega: global or full-strength (masked) dehaze
    pool[Q.dst + i] = vec4<f32>(g, p, g * p, g * g);
}

// Clamped, count-normalised box mean of all four channels. mode 0 horizontal, 1 vertical.
@compute @workgroup_size(256)
fn box(@builtin(global_invocation_id) gid: vec3<u32>, @builtin(num_workgroups) nwg: vec3<u32>) {
    let i = index(gid, nwg);
    if (i >= Q.w * Q.h) { return; }
    let r = i32(Q.r);
    let x = i32(i % Q.w);
    let y = i32(i / Q.w);
    var acc = vec4<f32>(0.0);
    var a = 0;
    var b = 0;
    if (Q.mode == 0u) {
        a = max(x - r, 0);
        b = min(x + r + 1, i32(Q.w));
        for (var k = a; k < b; k++) { acc += pool[Q.src + u32(y) * Q.w + u32(k)]; }
    } else {
        a = max(y - r, 0);
        b = min(y + r + 1, i32(Q.h));
        for (var k = a; k < b; k++) { acc += pool[Q.src + u32(k) * Q.w + u32(x)]; }
    }
    pool[Q.dst + i] = acc / f32(b - a);
}

// Guided-filter coefficients from means (I, p, Ip, II): (a, b, 0, 0); eps = k.x.
@compute @workgroup_size(256)
fn coeffs(@builtin(global_invocation_id) gid: vec3<u32>, @builtin(num_workgroups) nwg: vec3<u32>) {
    let i = index(gid, nwg);
    if (i >= Q.w * Q.h) { return; }
    let m = pool[Q.src + i];
    let v = max(m.w - m.x * m.x, 0.0);
    let cov = m.z - m.x * m.y;
    let a = cov / (v + Q.k.x);
    pool[Q.dst + i] = vec4<f32>(a, m.y - a * m.x, 0.0, 0.0);
}

// --- Detail (detail.rs): noise reduction and sharpening in the opponent domain ---------------------

@compute @workgroup_size(256)
fn to_opponent(@builtin(global_invocation_id) gid: vec3<u32>, @builtin(num_workgroups) nwg: vec3<u32>) {
    let i = index(gid, nwg);
    if (i >= Q.w * Q.h) { return; }
    let v = sqrt(max(pool[Q.src + i].xyz, vec3<f32>(0.0)));
    pool[Q.dst + i] = vec4<f32>((v.x + v.y + v.z) / 3.0, (v.x - v.z) / 2.0, (2.0 * v.y - v.x - v.z) / 4.0, 0.0);
}

@compute @workgroup_size(256)
fn from_opponent(@builtin(global_invocation_id) gid: vec3<u32>, @builtin(num_workgroups) nwg: vec3<u32>) {
    let i = index(gid, nwg);
    if (i >= Q.w * Q.h) { return; }
    let o = pool[Q.src + i];
    let v = max(vec3<f32>(o.x - 2.0 / 3.0 * o.z + o.y, o.x + 4.0 / 3.0 * o.z, o.x - 2.0 / 3.0 * o.z - o.y), vec3<f32>(0.0));
    let c = v * v;
    pool[Q.dst + i] = vec4<f32>(c, log2(max(luma(c), 1.0e-6)));
}

const B3 = array<f32, 5>(1.0 / 16.0, 4.0 / 16.0, 6.0 / 16.0, 4.0 / 16.0, 1.0 / 16.0);

fn shrink(d: f32, t: f32, keep: f32) -> f32 { return d - (1.0 - keep) * clamp(d, -t, t); }

// One whole wavelet level: next (dst) = 5x5 B3 smoothing of cur (src) with hole Q.r (the separable
// CPU passes as one 2D kernel, clamped per axis), and acc (aux) += shrink(cur - next). mode 0 starts
// the accumulator from zero. Thresholds k.xyz, luminance keep k.w.
@compute @workgroup_size(256)
fn atrous_level(@builtin(global_invocation_id) gid: vec3<u32>, @builtin(num_workgroups) nwg: vec3<u32>) {
    let i = index(gid, nwg);
    if (i >= Q.w * Q.h) { return; }
    let x = i32(i % Q.w);
    let y = i32(i / Q.w);
    let hole = i32(Q.r);
    var next = vec4<f32>(0.0);
    for (var j = 0; j < 5; j++) {
        let yy = u32(clamp(y + (j - 2) * hole, 0, i32(Q.h) - 1));
        var row = vec4<f32>(0.0);
        for (var k = 0; k < 5; k++) {
            let xx = u32(clamp(x + (k - 2) * hole, 0, i32(Q.w) - 1));
            row += B3[k] * pool[Q.src + yy * Q.w + xx];
        }
        next += B3[j] * row;
    }
    let d = pool[Q.src + i] - next;
    var a = vec4<f32>(0.0);
    if (Q.mode != 0u) { a = pool[Q.aux + i]; }
    a.x += shrink(d.x, Q.k.x, Q.k.w);
    a.y += shrink(d.y, Q.k.y, 0.0);
    a.z += shrink(d.z, Q.k.z, 0.0);
    pool[Q.aux + i] = a;
    pool[Q.dst + i] = next;
}

// dst = src + aux (coarsest level plus the kept detail).
@compute @workgroup_size(256)
fn add_planes(@builtin(global_invocation_id) gid: vec3<u32>, @builtin(num_workgroups) nwg: vec3<u32>) {
    let i = index(gid, nwg);
    if (i >= Q.w * Q.h) { return; }
    var v = pool[Q.src + i];
    let a = pool[Q.aux + i];
    v.x += a.x;
    v.y += a.y;
    v.z += a.z;
    pool[Q.dst + i] = v;
}

// Gaussian pass, sigma k.x, radius ceil(3 sigma) >= 1. mode 0 horizontal, 1 vertical.
@compute @workgroup_size(256)
fn gauss(@builtin(global_invocation_id) gid: vec3<u32>, @builtin(num_workgroups) nwg: vec3<u32>) {
    let i = index(gid, nwg);
    if (i >= Q.w * Q.h) { return; }
    let x = i32(i % Q.w);
    let y = i32(i / Q.w);
    let s = Q.k.x;
    let r = i32(max(ceil(3.0 * s), 1.0));
    var acc = vec4<f32>(0.0);
    var sum = 0.0;
    for (var k = -r; k <= r; k++) {
        let wgt = exp(-f32(k * k) / (2.0 * s * s));
        var j = 0u;
        if (Q.mode == 0u) { j = u32(y) * Q.w + u32(clamp(x + k, 0, i32(Q.w) - 1)); }
        else { j = u32(clamp(y + k, 0, i32(Q.h) - 1)) * Q.w + u32(x); }
        acc += wgt * pool[Q.src + j];
        sum += wgt;
    }
    pool[Q.dst + i] = acc / sum;
}

// Unsharp mask on O1: src/dst opponent plane, aux blurred plane. k = (amount, knee, mask, 0).
@compute @workgroup_size(256)
fn sharpen(@builtin(global_invocation_id) gid: vec3<u32>, @builtin(num_workgroups) nwg: vec3<u32>) {
    let i = index(gid, nwg);
    if (i >= Q.w * Q.h) { return; }
    let x = i % Q.w;
    let y = i / Q.w;
    var v = pool[Q.src + i];
    let b = pool[Q.aux + i].x;
    var d = v.x - b;
    d = d / (1.0 + abs(d) / Q.k.y);
    var m = 1.0;
    if (Q.k.z > 0.0) {
        let xr = min(x + 1u, Q.w - 1u);
        let xl = select(x - 1u, 0u, x == 0u);
        let yd = min(y + 1u, Q.h - 1u);
        let yu = select(y - 1u, 0u, y == 0u);
        let gx = pool[Q.aux + y * Q.w + xr].x - pool[Q.aux + y * Q.w + xl].x;
        let gy = pool[Q.aux + yd * Q.w + x].x - pool[Q.aux + yu * Q.w + x].x;
        m = smoothstep_(Q.k.z, 2.0 * Q.k.z, abs(gx) + abs(gy));
    }
    v.x += Q.k.x * d * m;
    pool[Q.dst + i] = v;
}

// Bilinear sample of a reduced coefficient plane (pixel centres at integer + 0.5).
fn sample_plane(off: u32, fx: f32, fy: f32) -> vec2<f32> {
    let sw = P.dims.z;
    let sh = P.dims.w;
    let x = clamp(fx - 0.5, 0.0, f32(sw - 1u));
    let y = clamp(fy - 0.5, 0.0, f32(sh - 1u));
    let x0 = u32(floor(x));
    let y0 = u32(floor(y));
    let x1 = min(x0 + 1u, sw - 1u);
    let y1 = min(y0 + 1u, sh - 1u);
    let tx = x - f32(x0);
    let ty = y - f32(y0);
    let top = pool[off + y0 * sw + x0].xy * (1.0 - tx) + pool[off + y0 * sw + x1].xy * tx;
    let bot = pool[off + y1 * sw + x0].xy * (1.0 - tx) + pool[off + y1 * sw + x1].xy * tx;
    return top * (1.0 - ty) + bot * ty;
}

// Masked sharpness: luminance gain (minus 1) of a full-strength unsharp mask into dst.w; src is the
// opponent plane, aux its Gaussian blur (pe_pipeline::sharp_gain_of).
@compute @workgroup_size(256)
fn sharp_target(@builtin(global_invocation_id) gid: vec3<u32>, @builtin(num_workgroups) nwg: vec3<u32>) {
    let i = index(gid, nwg);
    if (i >= Q.w * Q.h) { return; }
    let o1 = pool[Q.src + i].x;
    let b = pool[Q.aux + i].x;
    var e = 0.0;
    if (o1 > 1.0e-4) {
        let s = max(2.0 * o1 - b, 0.0) / o1;
        e = clamp(s * s - 1.0, -0.75, 3.0);
    }
    var v = pool[Q.dst + i];
    v.w = e;
    pool[Q.dst + i] = v;
}

// Masked dehaze: the scene dehazed at full strength (coefficients in aux) into dst.
@compute @workgroup_size(256)
fn dehaze_target(@builtin(global_invocation_id) gid: vec3<u32>, @builtin(num_workgroups) nwg: vec3<u32>) {
    let i = index(gid, nwg);
    let w = P.dims.x;
    if (i >= w * P.dims.y) { return; }
    var c = pool[Q.src + i].xyz;
    let a = P.haze.xyz;
    let inv = 1.0 / f32(P.dims2.x);
    let m = sample_plane(Q.aux, (f32(i % w) + 0.5) * inv, (f32(i / w) + 0.5) * inv);
    let g = clamp(luma(c) / P.haze.w, 0.0, 2.0);
    let t = clamp(m.x * g + m.y, 0.1, 1.0);
    c = max((c - a) / t + a, vec3<f32>(0.0));
    pool[Q.dst + i] = vec4<f32>(c, 0.0);
}

// Recover (or add) haze in the scene plane, then refresh log Y.
@compute @workgroup_size(256)
fn dehaze_apply(@builtin(global_invocation_id) gid: vec3<u32>, @builtin(num_workgroups) nwg: vec3<u32>) {
    let i = index(gid, nwg);
    let w = P.dims.x;
    if (i >= w * P.dims.y) { return; }
    var c = pool[P.offs.x + i].xyz;
    let a = P.haze.xyz;
    if (P.haze2.y > 0.0) {
        c = c * (1.0 - P.haze2.y) + a * P.haze2.y;
    } else {
        let inv = 1.0 / f32(P.dims2.x);
        let m = sample_plane(P.flags.w, (f32(i % w) + 0.5) * inv, (f32(i / w) + 0.5) * inv);
        let g = clamp(luma(c) / P.haze.w, 0.0, 2.0);
        let t = clamp(m.x * g + m.y, 0.1, 1.0);
        c = max((c - a) / t + a, vec3<f32>(0.0));
    }
    pool[P.offs.x + i] = vec4<f32>(c, log2(max(luma(c), 1.0e-6)));
}


fn aces_fit(x0: f32) -> f32 {
    let x = max(x0, 0.0);
    return (x * (2.51 * x + 0.03)) / (x * (2.43 * x + 0.59) + 0.14);
}

fn tone(px: vec3<f32>) -> vec3<f32> {
    var c = px;
    let power = P.tone.x;
    if (power != 1.0) {
        let y = dot(c, REC2020_LUMA);
        if (y > 1.0e-6) {
            c *= pow(y / MID_GREY, power) * MID_GREY / y;
        }
    }
    let n = max(c.x, max(c.y, c.z));
    if (n <= 0.0) { return vec3<f32>(0.0); }
    let yd = min(aces_fit(n) * P.tone.y, 1.0);
    var o = c / n * yd;
    let t = smoothstep_(0.8, 1.0, yd);
    if (t > 0.0) { o += (vec3<f32>(yd) - o) * t; }
    let black = P.tone.z;
    if (black != 0.0) { o = max((o - vec3<f32>(black)) / (1.0 - black), vec3<f32>(0.0)); }
    return o;
}

fn curve(v: f32) -> f32 {
    let x = clamp(v, 0.0, 1.0) * f32(LUT_SIZE - 1u);
    let i = min(u32(x), LUT_SIZE - 2u);
    let t = x - f32(i);
    return lut[i] * (1.0 - t) + lut[i + 1u] * t;
}

fn gamut_map(px: vec3<f32>) -> vec3<f32> {
    let y = clamp(dot(px, P.out_luma.xyz), 0.0, 1.0);
    let m = min(px.x, min(px.y, px.z));
    var c = px;
    if (m < 0.0) {
        let t = min(-m / max(y - m, 1.0e-6), 1.0);
        c += (vec3<f32>(y) - c) * t;
    }
    return clamp(c, vec3<f32>(0.0), vec3<f32>(1.0));
}

// --- Colour stage (look.rs) ------------------------------------------------------------------

fn cbrt_(x: f32) -> f32 { return sign(x) * pow(abs(x), 1.0 / 3.0); }

fn at8(a: array<vec4<f32>, 2>, i: u32) -> f32 { return a[i / 4u][i % 4u]; }

const LMS_TO_LAB = mat3x3<f32>(   // columns
    vec3<f32>(0.21045426, 1.9779985, 0.025904037),
    vec3<f32>(0.7936178, -2.4285922, 0.78277177),
    vec3<f32>(-0.004072047, 0.4505937, -0.80867577));
const LAB_TO_LMS = mat3x3<f32>(
    vec3<f32>(1.0, 1.0, 1.0),
    vec3<f32>(0.39633778, -0.105561346, -0.08948418),
    vec3<f32>(0.21580376, -0.06385417, -1.2914855));

fn look(d: vec3<f32>) -> vec3<f32> {
    if (L.flags.x < 0.5) { return d; }
    let lms = vec3<f32>(dot(L.lms[0].xyz, d), dot(L.lms[1].xyz, d), dot(L.lms[2].xyz, d));
    let lab = LMS_TO_LAB * vec3<f32>(cbrt_(lms.x), cbrt_(lms.y), cbrt_(lms.z));
    var l = lab.x;
    let c0 = sqrt(lab.y * lab.y + lab.z * lab.z);
    var h = degrees(atan2(lab.z, lab.y));
    if (h < 0.0) { h += 360.0; }

    // Bands either side of the hue.
    var i = 7u;
    for (var k = 0u; k < 8u; k++) {
        if (h >= at8(L.centres, k)) { i = k; }
    }
    let j = (i + 1u) % 8u;
    let start = at8(L.centres, i);
    var span = at8(L.centres, j) - start;
    var dh = h - start;
    if (span <= 0.0) { span += 360.0; }
    if (dh < 0.0) { dh += 360.0; }
    let t = smoothstep_(0.0, 1.0, dh / span);
    let chroma_w = smoothstep_(0.0, 0.06, c0);

    h += at8(L.band_hue, i) * (1.0 - t) + at8(L.band_hue, j) * t;
    var c = c0 * max(1.0 + at8(L.band_sat, i) * (1.0 - t) + at8(L.band_sat, j) * t, 0.0);
    l *= exp2((at8(L.band_lum, i) * (1.0 - t) + at8(L.band_lum, j) * t) * chroma_w);

    c *= L.flags.z;
    let vib = L.flags.w;
    if (vib != 0.0) {
        let low = 1.0 - smoothstep_(0.0, 0.2, c);
        let skin = 1.0 - 0.5 * (1.0 - smoothstep_(0.0, 30.0, abs(h - 60.0)));
        var v = vib;
        if (vib > 0.0) { v = vib * skin; }
        c *= max(1.0 + v * low, 0.0);
    }
    if (L.flags.y > 0.5) {
        l *= exp2((at8(L.band_bw, i) * (1.0 - t) + at8(L.band_bw, j) * t) * chroma_w);
        c = 0.0;
    }
    let hr = radians(h);
    var a2 = c * cos(hr);
    var b2 = c * sin(hr);

    let wd = L.shape.x;
    let bal = L.shape.y;
    let ws = 1.0 - smoothstep_(0.33 + bal - wd, 0.33 + bal + wd, l);
    let wh = smoothstep_(0.66 + bal - wd, 0.66 + bal + wd, l);
    let wm = max(1.0 - ws - wh, 0.0);
    let g = L.grade[0] * ws + L.grade[1] * wm + L.grade[2] * wh + L.grade[3];
    a2 += g.x;
    b2 += g.y;
    l = max(l + g.z, 0.0);

    let m = LAB_TO_LMS * vec3<f32>(l, a2, b2);
    let lms2 = m * m * m;
    return vec3<f32>(dot(L.rgb[0].xyz, lms2), dot(L.rgb[1].xyz, lms2), dot(L.rgb[2].xyz, lms2));
}

// Source coordinates for output pixel (dx, dy) under EXIF orientation `code` (as orient::apply).
fn oriented_source(dx: u32, dy: u32, code: u32) -> vec2<u32> {
    let w = P.dims.x;
    let h = P.dims.y;
    switch code {
        case 2u: { return vec2<u32>(w - 1u - dx, dy); }
        case 3u: { return vec2<u32>(w - 1u - dx, h - 1u - dy); }
        case 4u: { return vec2<u32>(dx, h - 1u - dy); }
        case 5u: { return vec2<u32>(dy, dx); }
        case 6u: { return vec2<u32>(dy, h - 1u - dx); }
        case 7u: { return vec2<u32>(w - 1u - dy, h - 1u - dx); }
        case 8u: { return vec2<u32>(w - 1u - dy, dx); }
        default: { return vec2<u32>(dx, dy); }
    }
}


// --- Effects (effects.rs) -----------------------------------------------------------------------

fn lattice(ix: i32, iy: i32, octave: u32) -> f32 {
    var h = (bitcast<u32>(ix) * 0x8DA6B343u) ^ (bitcast<u32>(iy) * 0xD8163841u) ^ (octave * 0xCB1AB31Fu);
    h ^= h >> 15u;
    h *= 0x2C1B3C6Du;
    h ^= h >> 12u;
    h *= 0x297A2D39u;
    h ^= h >> 15u;
    return f32(h >> 8u) / 8388607.5 - 1.0;
}

fn value_noise(x: f32, y: f32, octave: u32) -> f32 {
    let fx = floor(x);
    let fy = floor(y);
    let ix = i32(fx);
    let iy = i32(fy);
    let tx = x - fx;
    let ty = y - fy;
    let sx = tx * tx * (3.0 - 2.0 * tx);
    let sy = ty * ty * (3.0 - 2.0 * ty);
    let a = lattice(ix, iy, octave);
    let b = lattice(ix + 1, iy, octave);
    let c = lattice(ix, iy + 1, octave);
    let d = lattice(ix + 1, iy + 1, octave);
    let top = a + (b - a) * sx;
    let bot = c + (d - c) * sx;
    return top + (bot - top) * sy;
}

fn effects(c0: vec3<f32>, x: u32, y: u32, w: u32, h: u32) -> vec3<f32> {
    var c = c0;
    if (L.fx2.w > 0.5) {
        let u = ((f32(x) + 0.5) / f32(w) * 2.0 - 1.0) * L.fx2.x;
        let v = ((f32(y) + 0.5) / f32(h) * 2.0 - 1.0) * L.fx2.y;
        let p = L.fx2.z;
        let d = pow(pow(abs(u), p) + pow(abs(v), p), 1.0 / p);
        let t = smoothstep_(L.fx1.y, L.fx1.y + L.fx1.z, d);
        let amount = L.fx1.x;
        if (amount < 0.0) {
            let keep = L.fx1.w * smoothstep_(0.5, 1.0, luma(c));
            c *= 1.0 + amount * t * (1.0 - keep);
        } else {
            c += max(vec3<f32>(1.0) - c, vec3<f32>(0.0)) * amount * t;
        }
    }
    if (L.fx3.w > 0.5) {
        let gx = f32(x) / L.fx3.y;
        let gy = f32(y) / L.fx3.y;
        let n1 = value_noise(gx, gy, 0u);
        let n2 = value_noise(gx * 2.3, gy * 2.3, 1u);
        let n = n1 + (n2 - n1) * L.fx3.z;
        let l = sqrt(clamp(luma(c), 0.0, 1.0));
        c *= 1.0 + L.fx3.x * n * 4.0 * l * (1.0 - l);
    }
    return c;
}

// --- Masks (masks.rs) ---------------------------------------------------------------------------

fn mask_oklab(d: vec3<f32>) -> vec3<f32> {
    let lms = vec3<f32>(dot(M[2].xyz, d), dot(M[3].xyz, d), dot(M[4].xyz, d));
    return LMS_TO_LAB * vec3<f32>(cbrt_(lms.x), cbrt_(lms.y), cbrt_(lms.z));
}

// Oriented-frame position of sensor pixel (x, y) (masks::frame_position).
fn frame_pos(x: u32, y: u32) -> vec2<f32> {
    let sx = f32(x + P.full.z) + 0.5;
    let sy = f32(y + P.full.w) + 0.5;
    let sw = f32(P.full.x);
    let sh = f32(P.full.y);
    switch P.dims2.z {
        case 2u: { return vec2<f32>(sw - sx, sy); }
        case 3u: { return vec2<f32>(sw - sx, sh - sy); }
        case 4u: { return vec2<f32>(sx, sh - sy); }
        case 5u: { return vec2<f32>(sy, sx); }
        case 6u: { return vec2<f32>(sh - sy, sx); }
        case 7u: { return vec2<f32>(sh - sy, sw - sx); }
        case 8u: { return vec2<f32>(sy, sw - sx); }
        default: { return vec2<f32>(sx, sy); }
    }
}

fn raster_sample(off: u32, w: u32, h: u32, fx: f32, fy: f32) -> f32 {
    let x = clamp(fx * f32(w) - 0.5, 0.0, f32(w - 1u));
    let y = clamp(fy * f32(h) - 0.5, 0.0, f32(h - 1u));
    let x0 = u32(floor(x));
    let y0 = u32(floor(y));
    let x1 = min(x0 + 1u, w - 1u);
    let y1 = min(y0 + 1u, h - 1u);
    let tx = x - f32(x0);
    let ty = y - f32(y0);
    let top = MD[off + y0 * w + x0] * (1.0 - tx) + MD[off + y0 * w + x1] * tx;
    let bot = MD[off + y1 * w + x0] * (1.0 - tx) + MD[off + y1 * w + x1] * tx;
    return top * (1.0 - ty) + bot * ty;
}

fn mask_value(base: u32, p: vec2<f32>, display: vec3<f32>) -> f32 {
    let n = u32(M[base].x);
    var acc = 0.0;
    var have_lab = false;
    var lab = vec3<f32>(0.0);
    for (var k = 0u; k < n; k++) {
        let c0 = M[base + 7u + 3u * k];
        let c1 = M[base + 8u + 3u * k];
        let c2 = M[base + 9u + 3u * k];
        var v = 0.0;
        switch u32(c0.x) {
            case 0u: {
                let t = ((p.x - c1.x) * c1.z + (p.y - c1.y) * c1.w) * c2.x;
                v = 1.0 - smoothstep_(0.0, 1.0, t);
            }
            case 1u: {
                let dx = p.x - c1.x;
                let dy = p.y - c1.y;
                let u = (dx * c1.z + dy * c1.w) * c2.x;
                let w = (-dx * c1.w + dy * c1.z) * c2.y;
                v = 1.0 - smoothstep_(c2.z, 1.0, sqrt(u * u + w * w));
            }
            case 2u: {
                v = raster_sample(u32(c1.x), u32(c1.y), u32(c1.z), p.x / M[1].x, p.y / M[1].y);
            }
            case 3u: {
                let l = mask_oklab(display).x;
                v = smoothstep_(c1.x - c1.z, c1.x, l) * (1.0 - smoothstep_(c1.y, c1.y + c1.z, l));
            }
            case 4u: {
                if (!have_lab) { lab = mask_oklab(display); have_lab = true; }
                let off = u32(c1.x);
                for (var q = 0u; q < u32(c1.y); q++) {
                    let t = vec3<f32>(MD[off + 3u * q], MD[off + 3u * q + 1u], MD[off + 3u * q + 2u]);
                    let dd = 2.0 * sqrt((lab.y - t.y) * (lab.y - t.y) + (lab.z - t.z) * (lab.z - t.z)) + 0.5 * abs(lab.x - t.x);
                    v = max(v, 1.0 - smoothstep_(c1.z * 0.5, c1.z, dd));
                }
            }
            default: { v = 0.0; }
        }
        if (c0.z > 0.5) { v = 1.0 - v; }
        if (k == 0u) {
            if (u32(c0.y) == 0u) { acc = v; } else { acc = 0.0; }
        } else {
            switch u32(c0.y) {
                case 0u: { acc = acc + v - acc * v; }
                case 1u: { acc = acc * (1.0 - v); }
                default: { acc = acc * v; }
            }
        }
    }
    return acc * M[base].y;
}

const LOCAL_HAZE_MAX = 0.5;

// Develop one sensor-space pixel (index j at (sx, sy)) up to display-linear values after the curve.
fn develop_pixel(j: u32, sx: u32, sy: u32) -> vec3<f32> {
    let px = pool[P.offs.x + j];
    var c = px.xyz;
    let l = px.w;

    // Local gain: shadows/highlights, clarity, texture (local.rs local_gains).
    let inv = 1.0 / f32(P.dims2.x);
    let fx = (f32(sx) + 0.5) * inv;
    let fy = (f32(sy) + 0.5) * inv;
    var d = 0.0;
    var cd = 0.0;
    var td = 0.0;
    if (P.flags.x != 0u) {
        let m = sample_plane(P.offs.y, fx, fy);
        let b = m.x * l + m.y - P.tone.w;
        d += P.local.x * (1.0 - smoothstep_(-5.0, 0.5, b)) + P.local.y * smoothstep_(-0.5, 3.0, b);
    }
    if (P.flags.y != 0u) {
        let m = sample_plane(P.offs.z, fx, fy);
        let base = m.x * l + m.y;
        let mg = base - P.tone.w;
        cd = (l - base) * exp(-(mg * mg) / 8.0);
        d += P.local.z * cd;
    }
    if (P.flags.z != 0u) {
        let dt = l - pool[P.offs.w + j].w;
        td = dt / (1.0 + abs(dt) / 0.35);
        d += P.local.w * td;
    }
    var lgain = 1.0;
    if (P.local.x != 0.0 || P.local.y != 0.0 || P.local.z != 0.0 || P.local.w != 0.0) { lgain = exp2(d); c *= lgain; }

    // Masks: scene-linear adjustments now, saturation after the colour stage.
    var vals: array<f32, 16>;
    let mcount = u32(M[0].x);
    if (mcount > 0u) {
        var display = c;
        if (M[0].y > 0.5) { display = tone(c); }
        let fp = frame_pos(sx, sy);
        for (var i = 0u; i < mcount; i++) {
            let base = 8u + i * 31u;
            let v = mask_value(base, fp, display);
            vals[i] = v;
            if (v <= 0.0) { continue; }
            let h0 = M[base];
            let h1 = M[base + 1u];
            let h2 = M[base + 2u];
            // Local noise reduction, dehaze, sharpness (pe_pipeline::masks apply_scene).
            let dt = M[base + 6u];
            if (dt.x > 0.0) {
                let k = v * dt.x * lgain;
                c = max(c + k * (pool[P.offs2.x + j].xyz - px.xyz), vec3<f32>(0.0));
            }
            if (dt.y > 0.0) {
                let k = v * dt.y * lgain;
                c = max(c + k * (pool[P.offs2.y + j].xyz - px.xyz), vec3<f32>(0.0));
            } else if (dt.y < 0.0) {
                c += v * -dt.y * LOCAL_HAZE_MAX * (P.haze.xyz - c);
            }
            if (dt.z != 0.0) {
                c *= max(1.0 + v * dt.z * pool[P.offs2.x + j].w, 0.0);
            }
            if (h0.z > 0.5) {
                let a = vec3<f32>(dot(M[base + 3u].xyz, c), dot(M[base + 4u].xyz, c), dot(M[base + 5u].xyz, c));
                c += v * (a - c);
            }
            let y = max(luma(c), 1.0e-6);
            let ll = log2(y / MID_GREY);
            let g = h0.w + h1.x * ll
                + h1.y * (1.0 - smoothstep_(-5.0, 0.5, ll))
                + h1.z * smoothstep_(-0.5, 3.0, ll)
                + h1.w * smoothstep_(1.5, 4.5, ll)
                + h2.x * (1.0 - smoothstep_(-7.0, -3.0, ll))
                + h2.y * cd + h2.z * td;
            c *= exp2(v * g);
        }
    }

    var e = look(tone(c));
    if (mcount > 0u && M[0].z > 0.5) {
        var gain = 1.0;
        for (var i = 0u; i < mcount; i++) {
            gain *= max(1.0 + vals[i] * M[8u + i * 31u + 2u].w, 0.0);
        }
        if (gain != 1.0) {
            let lab = mask_oklab(e);
            let m = LAB_TO_LMS * vec3<f32>(lab.x, lab.y * gain, lab.z * gain);
            let lms = m * m * m;
            e = vec3<f32>(dot(M[5].xyz, lms), dot(M[6].xyz, lms), dot(M[7].xyz, lms));
        }
    }
    if (P.dims2.w != 0u) {
        for (var k = 0; k < 3; k++) {
            e[k] = srgb_decode(curve(srgb_encode(clamp(e[k], 0.0, 1.0))));
        }
    }
    return e;
}

// Effects at output (x, y), conversion to the output space and 8-bit packing.
fn finish(e0: vec3<f32>, x: u32, y: u32) -> u32 {
    let e = effects(e0, x + P.outr.x, y + P.outr.y, P.outr.z, P.outr.w);
    let lin = gamut_map(vec3<f32>(dot(P.o0.xyz, e), dot(P.o1.xyz, e), dot(P.o2.xyz, e)));
    let enc = vec3<f32>(srgb_encode(lin.x), srgb_encode(lin.y), srgb_encode(lin.z));
    let q = vec3<u32>(enc * 255.0 + vec3<f32>(0.5));
    return q.x | (q.y << 8u) | (q.z << 16u) | (255u << 24u);
}

// Without geometry: develop each output pixel from its (oriented) source pixel.
@compute @workgroup_size(256)
fn final_pass(@builtin(global_invocation_id) gid: vec3<u32>, @builtin(num_workgroups) nwg: vec3<u32>) {
    let i = index(gid, nwg);
    let ow = P.dims3.x;
    if (i >= ow * P.dims3.y) { return; }
    let s = oriented_source(i % ow, i / ow, P.dims2.z);
    out_rgba[i] = finish(develop_pixel(s.y * P.dims.x + s.x, s.x, s.y), i % ow, i / ow);
}

// --- Geometry (geometry.rs) --------------------------------------------------------------------

// With geometry: develop every sensor-space pixel into plane Q.dst first.
@compute @workgroup_size(256)
fn display_pass(@builtin(global_invocation_id) gid: vec3<u32>, @builtin(num_workgroups) nwg: vec3<u32>) {
    let i = index(gid, nwg);
    let w = P.dims.x;
    if (i >= w * P.dims.y) { return; }
    pool[Q.dst + i] = vec4<f32>(develop_pixel(i, i % w, i / w), 0.0);
}

// Continuous sensor position sampled for output pixel (x, y), channel c.
fn geo_source(x: u32, y: u32, c: u32) -> vec2<f32> {
    let fw = f32(P.geo4.x);
    let fh = f32(P.geo4.y);
    let xt = (P.geo0.x + (f32(x + P.outr.x) + 0.5) / f32(P.outr.z) * (P.geo0.z - P.geo0.x)) * fw;
    let yt = (P.geo0.y + (f32(y + P.outr.y) + 0.5) / f32(P.outr.w) * (P.geo0.w - P.geo0.y)) * fh;
    let cx = xt - fw * 0.5;
    let cy = yt - fh * 0.5;
    let rx = cx * P.geo1.x + cy * P.geo1.y;
    let ry = -cx * P.geo1.y + cy * P.geo1.x;
    let nx = rx / (fw * 0.5);
    let ny = ry / (fh * 0.5);
    let wq = 1.0 + P.geo1.z * nx + P.geo1.w * ny;
    let px = nx / wq * P.geo2.x * fw * 0.5 + fw * 0.5;
    let py = ny / wq * P.geo2.x * fh * 0.5 + fh * 0.5;
    let sw = f32(P.full.x);
    let sh = f32(P.full.y);
    var s = vec2<f32>(px, py);
    switch P.dims2.z {
        case 2u: { s = vec2<f32>(sw - px, py); }
        case 3u: { s = vec2<f32>(sw - px, sh - py); }
        case 4u: { s = vec2<f32>(px, sh - py); }
        case 5u: { s = vec2<f32>(py, px); }
        case 6u: { s = vec2<f32>(py, sh - px); }
        case 7u: { s = vec2<f32>(sw - py, sh - px); }
        case 8u: { s = vec2<f32>(sw - py, px); }
        default: {}
    }
    let dx = s.x - sw * 0.5;
    let dy = s.y - sh * 0.5;
    let r2 = (dx * dx + dy * dy) / (sw * sw * 0.25 + sh * sh * 0.25);
    let f = (1.0 + P.geo2.y * r2) * P.geo3[c];
    // Into this frame's pixel coordinates.
    return vec2<f32>(sw * 0.5 + dx * f - f32(P.full.z), sh * 0.5 + dy * f - f32(P.full.w));
}

fn geo_sample(off: u32, p: vec2<f32>, c: u32) -> f32 {
    let w = P.dims.x;
    let h = P.dims.y;
    if (p.x < 0.0 || p.x > f32(w) || p.y < 0.0 || p.y > f32(h)) { return 0.0; }
    let x = clamp(p.x - 0.5, 0.0, f32(w - 1u));
    let y = clamp(p.y - 0.5, 0.0, f32(h - 1u));
    let x0 = u32(floor(x));
    let y0 = u32(floor(y));
    let x1 = min(x0 + 1u, w - 1u);
    let y1 = min(y0 + 1u, h - 1u);
    let tx = x - f32(x0);
    let ty = y - f32(y0);
    let top = pool[off + y0 * w + x0][c] * (1.0 - tx) + pool[off + y0 * w + x1][c] * tx;
    let bot = pool[off + y1 * w + x0][c] * (1.0 - tx) + pool[off + y1 * w + x1][c] * tx;
    return top * (1.0 - ty) + bot * ty;
}

@compute @workgroup_size(256)
fn geometry_pass(@builtin(global_invocation_id) gid: vec3<u32>, @builtin(num_workgroups) nwg: vec3<u32>) {
    let i = index(gid, nwg);
    let ow = P.dims3.x;
    if (i >= ow * P.dims3.y) { return; }
    let x = i % ow;
    let y = i / ow;
    var e = vec3<f32>(0.0);
    for (var c = 0u; c < 3u; c++) {
        e[c] = geo_sample(Q.src, geo_source(x, y, c), c);
    }
    out_rgba[i] = finish(e, x, y);
}
