// GPU version of the develop pipeline for linear-RGB (already demosaiced or superpixel) frames.
// Each entry point mirrors a function of the CPU reference in pe-pipeline; see the comments there for
// the reasoning behind each stage. Parity is checked by tests (ΔE2000 against the CPU path).

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
    local: vec4<f32>,     // shadow EV, highlight EV, eps, enabled
    dims: vec4<u32>,      // source width, height; reduced width, height
    dims2: vec4<u32>,     // reduction factor, blur radius (reduced px), orientation, has curve
    dims3: vec4<u32>,     // output width, height, output encoding (0 = sRGB, 1 = linear float), unused
};

@group(0) @binding(0) var<uniform> P: Params;
@group(0) @binding(1) var<storage, read> src: array<f32>;
@group(0) @binding(2) var<storage, read_write> scene: array<f32>;
@group(0) @binding(3) var<storage, read_write> logy: array<f32>;
@group(0) @binding(4) var<storage, read_write> small: array<f32>;
@group(0) @binding(5) var<storage, read_write> tmp: array<vec2<f32>>;
@group(0) @binding(6) var<storage, read_write> stats: array<vec2<f32>>;
@group(0) @binding(7) var<storage, read_write> ab: array<vec2<f32>>;
@group(0) @binding(8) var<storage, read> lut: array<f32>;
@group(0) @binding(9) var<storage, read_write> out_rgba: array<u32>;

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

@compute @workgroup_size(256)
fn scene_pass(@builtin(global_invocation_id) gid: vec3<u32>, @builtin(num_workgroups) nwg: vec3<u32>) {
    let i = index(gid, nwg);
    if (i >= P.dims.x * P.dims.y) { return; }
    let cam = vec3<f32>(src[3u * i], src[3u * i + 1u], src[3u * i + 2u]);
    let b = reconstruct(cam);
    let w = vec3<f32>(dot(P.m0.xyz, b), dot(P.m1.xyz, b), dot(P.m2.xyz, b)) * P.wb.w;
    scene[3u * i] = w.x;
    scene[3u * i + 1u] = w.y;
    scene[3u * i + 2u] = w.z;
    logy[i] = log2(max(dot(w, REC2020_LUMA), 1.0e-6));
}

// --- Stage 4: fast guided filter on log luminance (shadows / highlights) ------------------------

@compute @workgroup_size(256)
fn downscale_pass(@builtin(global_invocation_id) gid: vec3<u32>, @builtin(num_workgroups) nwg: vec3<u32>) {
    let i = index(gid, nwg);
    let sw = P.dims.z;
    if (i >= sw * P.dims.w) { return; }
    let f = P.dims2.x;
    let x2 = i % sw;
    let y2 = i / sw;
    var s = 0.0;
    var n = 0.0;
    for (var y = y2 * f; y < min((y2 + 1u) * f, P.dims.y); y++) {
        for (var x = x2 * f; x < min((x2 + 1u) * f, P.dims.x); x++) {
            s += logy[y * P.dims.x + x];
            n += 1.0;
        }
    }
    small[i] = s / max(n, 1.0);
}

// Horizontal box mean of (I, I^2) with a clamped, count-normalised window.
@compute @workgroup_size(256)
fn blur_h_moments(@builtin(global_invocation_id) gid: vec3<u32>, @builtin(num_workgroups) nwg: vec3<u32>) {
    let i = index(gid, nwg);
    let sw = P.dims.z;
    if (i >= sw * P.dims.w) { return; }
    let r = i32(P.dims2.y);
    let x = i32(i % sw);
    let row = i - u32(x);
    let a = max(x - r, 0);
    let b = min(x + r + 1, i32(sw));
    var acc = vec2<f32>(0.0);
    for (var k = a; k < b; k++) {
        let v = small[row + u32(k)];
        acc += vec2<f32>(v, v * v);
    }
    tmp[i] = acc / f32(b - a);
}

// Horizontal box mean of the coefficients (a, b).
@compute @workgroup_size(256)
fn blur_h_coeffs(@builtin(global_invocation_id) gid: vec3<u32>, @builtin(num_workgroups) nwg: vec3<u32>) {
    let i = index(gid, nwg);
    let sw = P.dims.z;
    if (i >= sw * P.dims.w) { return; }
    let r = i32(P.dims2.y);
    let x = i32(i % sw);
    let row = i - u32(x);
    let a = max(x - r, 0);
    let b = min(x + r + 1, i32(sw));
    var acc = vec2<f32>(0.0);
    for (var k = a; k < b; k++) {
        acc += ab[row + u32(k)];
    }
    tmp[i] = acc / f32(b - a);
}

// Vertical box mean: tmp -> stats.
@compute @workgroup_size(256)
fn blur_v(@builtin(global_invocation_id) gid: vec3<u32>, @builtin(num_workgroups) nwg: vec3<u32>) {
    let i = index(gid, nwg);
    let sw = P.dims.z;
    let sh = P.dims.w;
    if (i >= sw * sh) { return; }
    let r = i32(P.dims2.y);
    let x = i % sw;
    let y = i32(i / sw);
    let a = max(y - r, 0);
    let b = min(y + r + 1, i32(sh));
    var acc = vec2<f32>(0.0);
    for (var k = a; k < b; k++) {
        acc += tmp[u32(k) * sw + x];
    }
    stats[i] = acc / f32(b - a);
}

// Linear coefficients: stats = (mean I, mean I^2) -> ab = (a, b).
@compute @workgroup_size(256)
fn coeff_pass(@builtin(global_invocation_id) gid: vec3<u32>, @builtin(num_workgroups) nwg: vec3<u32>) {
    let i = index(gid, nwg);
    if (i >= P.dims.z * P.dims.w) { return; }
    let m = stats[i];
    let v = max(m.y - m.x * m.x, 0.0);
    let a = v / (v + P.local.z);
    ab[i] = vec2<f32>(a, m.x - a * m.x);
}

// Bilinear sample of the reduced (mean a, mean b) planes; pixel centres at integer + 0.5.
fn sample_ab(fx: f32, fy: f32) -> vec2<f32> {
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
    let top = stats[y0 * sw + x0] * (1.0 - tx) + stats[y0 * sw + x1] * tx;
    let bot = stats[y1 * sw + x0] * (1.0 - tx) + stats[y1 * sw + x1] * tx;
    return top * (1.0 - ty) + bot * ty;
}

// --- Stage 5-7: tone, point curve, output conversion, orientation ----------------------------

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

@compute @workgroup_size(256)
fn final_pass(@builtin(global_invocation_id) gid: vec3<u32>, @builtin(num_workgroups) nwg: vec3<u32>) {
    let i = index(gid, nwg);
    let ow = P.dims3.x;
    if (i >= ow * P.dims3.y) { return; }
    let s = oriented_source(i % ow, i / ow, P.dims2.z);
    let j = s.y * P.dims.x + s.x;
    var c = vec3<f32>(scene[3u * j], scene[3u * j + 1u], scene[3u * j + 2u]);

    if (P.local.w > 0.5) {
        let inv = 1.0 / f32(P.dims2.x);
        let m = sample_ab((f32(s.x) + 0.5) * inv, (f32(s.y) + 0.5) * inv);
        let base = m.x * logy[j] + m.y;
        let d = base - P.tone.w;
        let w_shadow = 1.0 - smoothstep_(-5.0, 0.5, d);
        let w_high = smoothstep_(-0.5, 3.0, d);
        c *= exp2(P.local.x * w_shadow + P.local.y * w_high);
    }

    var d = tone(c);
    if (P.dims2.w != 0u) {
        for (var k = 0; k < 3; k++) {
            d[k] = srgb_decode(curve(srgb_encode(clamp(d[k], 0.0, 1.0))));
        }
    }
    let lin = gamut_map(vec3<f32>(dot(P.o0.xyz, d), dot(P.o1.xyz, d), dot(P.o2.xyz, d)));
    let e = vec3<f32>(srgb_encode(lin.x), srgb_encode(lin.y), srgb_encode(lin.z));
    let q = vec3<u32>(e * 255.0 + vec3<f32>(0.5));
    out_rgba[i] = q.x | (q.y << 8u) | (q.z << 16u) | (255u << 24u);
}
