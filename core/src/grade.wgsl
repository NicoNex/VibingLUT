// VibingLUT grade pipeline. Input and output are display-encoded
// (sRGB / Rec.709) in 0..1 — the signal a camera "Standard" profile carries,
// so baking `grade()` on a lattice gives an exact .cube for that profile.

struct Params {
    temperature: f32, tint: f32, exposure: f32, contrast: f32,
    highlights: f32, shadows: f32, whites: f32, blacks: f32,
    saturation: f32, vibrance: f32, sat_shadows: f32, sat_highlights: f32,
    lift: array<f32, 4>, gamma: array<f32, 4>, gain: array<f32, 4>, offset: array<f32, 4>,
    hue_shift: array<f32, 8>, hue_sat: array<f32, 8>, hue_lum: array<f32, 8>,
    split: f32, width: u32, height: u32, lut_size: u32,
}

@group(0) @binding(0) var<storage, read> p: Params;
@group(0) @binding(1) var<storage, read> curves: array<f32>;
@group(0) @binding(2) var<storage, read> src: array<vec4<f32>>;
@group(0) @binding(3) var<storage, read_write> dst_px: array<u32>;
@group(0) @binding(4) var<storage, read_write> dst_lut: array<vec4<f32>>;
@group(0) @binding(5) var<storage, read> ext_lut: array<vec4<f32>>;

const CURVE_RES: u32 = 1024u;
const LUMA = vec3<f32>(0.2126, 0.7152, 0.0722);
const MID_GREY: f32 = 0.18;

// ---------------------------------------------------------------- transfer

fn srgb_to_lin(c: vec3<f32>) -> vec3<f32> {
    let x = max(c, vec3(0.0));
    return select(pow((x + 0.055) / 1.055, vec3(2.4)), x / 12.92, x <= vec3(0.04045));
}

fn lin_to_srgb(c: vec3<f32>) -> vec3<f32> {
    let x = max(c, vec3(0.0));
    return select(1.055 * pow(x, vec3(1.0 / 2.4)) - 0.055, x * 12.92, x <= vec3(0.0031308));
}

// ---------------------------------------------------------- white balance
// CAT16 (CIECAM16) von Kries adaptation from a CIE daylight-locus white to D65.

const RGB_TO_LMS = mat3x3<f32>(  // M16 · sRGB→XYZ(D65), column-major
    vec3(0.302745, 0.153783, 0.027977),
    vec3(0.602344, 0.777333, 0.147874),
    vec3(0.070447, 0.085366, 0.909137));
const XYZ_TO_LMS = mat3x3<f32>(  // M16, column-major
    vec3(0.401288, -0.250268, -0.002079),
    vec3(0.650173, 1.204414, 0.048952),
    vec3(-0.051461, 0.045854, 0.953127));

fn inverse3(m: mat3x3<f32>) -> mat3x3<f32> {
    let a = cross(m[1], m[2]);
    let b = cross(m[2], m[0]);
    let c = cross(m[0], m[1]);
    return transpose(mat3x3(a, b, c)) * (1.0 / dot(m[0], a));
}

fn daylight_lms(cct: f32, dy: f32) -> vec3<f32> {
    let t = cct;
    var x: f32;
    if t <= 7000.0 {
        x = -4.6070e9 / (t * t * t) + 2.9678e6 / (t * t) + 0.09911e3 / t + 0.244063;
    } else {
        x = -2.0064e9 / (t * t * t) + 1.9018e6 / (t * t) + 0.24748e3 / t + 0.237040;
    }
    let y = -3.0 * x * x + 2.87 * x - 0.275 + dy;
    return XYZ_TO_LMS * vec3(x / y, 1.0, (1.0 - x - y) / y);
}

fn white_balance(c: vec3<f32>) -> vec3<f32> {
    if p.temperature == 0.0 && p.tint == 0.0 { return c; }
    // ±90 mired around D65; positive = warmer picture (source assumed bluer).
    let src_cct = 1.0e6 / (1.0e6 / 6504.0 - p.temperature * 0.9);
    let gain = daylight_lms(6504.0, 0.0) / daylight_lms(src_cct, -p.tint * 0.0003);
    return inverse3(RGB_TO_LMS) * (gain * (RGB_TO_LMS * c));
}

// ------------------------------------------------------------------- tone
// Scene-style luminance edits in log2 stops around mid grey; applied as a
// ratio so hue and saturation are preserved.

fn tone(c: vec3<f32>) -> vec3<f32> {
    let y = max(dot(c, LUMA), 1e-6);
    var s = log2(y / MID_GREY);
    s *= 1.0 + p.contrast * 0.01;
    s += p.highlights * 0.012 * smoothstep(-0.5, 2.5, s);
    s += p.shadows * 0.015 * (1.0 - smoothstep(-5.0, 0.5, s));
    return c * (MID_GREY * exp2(s) / y);
}

// ------------------------------------------------------------------ OKLab

fn cbrt(v: vec3<f32>) -> vec3<f32> { return sign(v) * pow(abs(v), vec3(1.0 / 3.0)); }

fn lin_to_oklab(c: vec3<f32>) -> vec3<f32> {
    let lms = cbrt(vec3(
        dot(c, vec3(0.4122214708, 0.5363325363, 0.0514459929)),
        dot(c, vec3(0.2119034982, 0.6806995451, 0.1073969566)),
        dot(c, vec3(0.0883024619, 0.2817188376, 0.6299787005))));
    return vec3(
        dot(lms, vec3(0.2104542553, 0.7936177850, -0.0040720468)),
        dot(lms, vec3(1.9779984951, -2.4285922050, 0.4505937099)),
        dot(lms, vec3(0.0259040371, 0.7827717662, -0.8086757660)));
}

fn oklab_to_lin(c: vec3<f32>) -> vec3<f32> {
    let l = vec3(
        dot(c, vec3(1.0, 0.3963377774, 0.2158037573)),
        dot(c, vec3(1.0, -0.1055613458, -0.0638541728)),
        dot(c, vec3(1.0, -0.0894841775, -1.2914855480)));
    let lms = l * l * l;
    return vec3(
        dot(lms, vec3(4.0767416621, -3.3077115913, 0.2309699292)),
        dot(lms, vec3(-1.2684380046, 2.6097574011, -0.3413193965)),
        dot(lms, vec3(-0.0041960863, -0.7034186147, 1.7076147010)));
}

const TAU: f32 = 6.2831853;
// OKLCh hue centres (radians) of red, orange, yellow, green, aqua, blue, purple, magenta.
const HUES = array<f32, 8>(0.506, 0.960, 1.920, 2.478, 3.403, 4.608, 5.236, 5.725);

// Value of an 8-band hue control at hue `h`: smooth partition of unity
// between neighbouring centres, so bands blend without seams.
fn band(v: array<f32, 8>, h: f32) -> f32 {
    var hh = h;
    if hh < HUES[0] { hh += TAU; }
    for (var k = 0u; k < 8u; k++) {
        let k1 = (k + 1u) % 8u;
        var h0 = HUES[k];
        var h1 = HUES[k1];
        if k1 == 0u { h1 += TAU; }
        if hh >= h0 && hh < h1 {
            return mix(v[k], v[k1], smoothstep(0.0, 1.0, (hh - h0) / (h1 - h0)));
        }
    }
    return v[0];
}

fn color(c: vec3<f32>) -> vec3<f32> {
    let lab = lin_to_oklab(c);
    var chroma = length(lab.yz);
    if chroma < 1e-7 { return c; }
    var hue = atan2(lab.z, lab.y);
    if hue < 0.0 { hue += TAU; }
    let w = smoothstep(0.0, 0.06, chroma); // leave neutrals alone
    hue += band(p.hue_shift, hue) * 0.005 * w;   // ±0.5 rad ≈ ±29°
    var l = lab.x * (1.0 + band(p.hue_lum, hue) * 0.004 * w);
    chroma *= max(0.0, 1.0 + band(p.hue_sat, hue) * 0.01 * w);
    chroma *= max(0.0, 1.0 + p.saturation * 0.01);
    chroma *= max(0.0, 1.0 + p.vibrance * 0.01 * (1.0 - smoothstep(0.0, 0.2, chroma)));
    // Luma vs Sat: saturation by tonal range (OKLab L is perceptual lightness).
    let lv = lab.x;
    chroma *= max(0.0, 1.0 + 0.01 * (p.sat_shadows * (1.0 - smoothstep(0.15, 0.55, lv)) + p.sat_highlights * smoothstep(0.55, 0.95, lv)));
    return oklab_to_lin(vec3(l, chroma * cos(hue), chroma * sin(hue)));
}

// ------------------------------------------------------- gamut / highlight
// Over-range highlights desaturate toward their luminance instead of
// clipping per channel (which skews hue, e.g. skies going cyan). The
// smoothstep weight is C1 at the gamut edge, so the result stays smooth
// enough to be sampled by a LUT lattice without visible error.

fn fit_gamut(c: vec3<f32>) -> vec3<f32> {
    let hi = max(c.r, max(c.g, c.b));
    let t = smoothstep(1.0, 3.0, hi);
    return clamp(mix(c, vec3(dot(c, LUMA)), t), vec3(0.0), vec3(1.0));
}

// ------------------------------------------------------- display-referred

fn wheel(w: array<f32, 4>) -> vec3<f32> {
    // Chroma-only RGB offset (zero luma) from the wheel position.
    let a = atan2(w[1], w[0]);
    let amt = length(vec2(w[0], w[1]));
    let rgb = clamp(abs(fract(a / TAU + vec3(1.0, 2.0 / 3.0, 1.0 / 3.0)) * 6.0 - 3.0) - 1.0, vec3(0.0), vec3(1.0));
    return (rgb - dot(rgb, LUMA)) * amt;
}

fn curve(i: u32, v: f32) -> f32 {
    let x = clamp(v, 0.0, 1.0) * f32(CURVE_RES - 1u);
    let k = min(u32(x), CURVE_RES - 2u);
    let base = i * CURVE_RES;
    return mix(curves[base + k], curves[base + k + 1u], x - f32(k));
}

fn grade(input: vec3<f32>) -> vec3<f32> {
    var c = srgb_to_lin(input);
    c = white_balance(c);
    c *= exp2(p.exposure);
    c = tone(c);
    c = color(c);
    var d = lin_to_srgb(fit_gamut(c));

    // Levels: whites/blacks move the end points.
    let bp = -p.blacks * 0.0008;
    let wp = 1.0 - p.whites * 0.0025;
    d = (d - bp) / (wp - bp);

    // Offset / lift / gamma / gain primaries (Resolve-style wheels).
    d += wheel(p.offset) * 0.2 + p.offset[2] * 0.002;
    let lift = wheel(p.lift) * 0.25 + p.lift[2] * 0.0025;
    let gain = 1.0 + wheel(p.gain) * 0.5 + p.gain[2] * 0.005;
    let gamma = max(1.0 + wheel(p.gamma) * 0.5 + p.gamma[2] * 0.005, vec3(0.05));
    d = d + lift * (1.0 - d);
    d *= gain;
    d = pow(max(d, vec3(0.0)), 1.0 / gamma);

    // Master curve, then per-channel curves.
    d = vec3(curve(1u, curve(0u, d.r)), curve(2u, curve(0u, d.g)), curve(3u, curve(0u, d.b)));
    return clamp(d, vec3(0.0), vec3(1.0));
}

// ---------------------------------------------------------------- 3D LUT
// Tetrahedral interpolation: the industry standard for 3D LUTs (smoother,
// neutral-preserving vs trilinear).

fn lut_at(r: u32, g: u32, b: u32) -> vec3<f32> {
    let n = p.lut_size;
    return ext_lut[r + n * (g + n * b)].rgb;
}

fn tetrahedral(c: vec3<f32>) -> vec3<f32> {
    let n = p.lut_size;
    let x = clamp(c, vec3(0.0), vec3(1.0)) * f32(n - 1u);
    let i = min(vec3<u32>(x), vec3(n - 2u));
    let f = x - vec3<f32>(i);
    let c000 = lut_at(i.x, i.y, i.z);
    let c111 = lut_at(i.x + 1u, i.y + 1u, i.z + 1u);
    if f.x > f.y {
        if f.y > f.z {
            let c100 = lut_at(i.x + 1u, i.y, i.z);
            let c110 = lut_at(i.x + 1u, i.y + 1u, i.z);
            return c000 + f.x * (c100 - c000) + f.y * (c110 - c100) + f.z * (c111 - c110);
        } else if f.x > f.z {
            let c100 = lut_at(i.x + 1u, i.y, i.z);
            let c101 = lut_at(i.x + 1u, i.y, i.z + 1u);
            return c000 + f.x * (c100 - c000) + f.z * (c101 - c100) + f.y * (c111 - c101);
        } else {
            let c001 = lut_at(i.x, i.y, i.z + 1u);
            let c101 = lut_at(i.x + 1u, i.y, i.z + 1u);
            return c000 + f.z * (c001 - c000) + f.x * (c101 - c001) + f.y * (c111 - c101);
        }
    }
    if f.z > f.y {
        let c001 = lut_at(i.x, i.y, i.z + 1u);
        let c011 = lut_at(i.x, i.y + 1u, i.z + 1u);
        return c000 + f.z * (c001 - c000) + f.y * (c011 - c001) + f.x * (c111 - c011);
    } else if f.z > f.x {
        let c010 = lut_at(i.x, i.y + 1u, i.z);
        let c011 = lut_at(i.x, i.y + 1u, i.z + 1u);
        return c000 + f.y * (c010 - c000) + f.z * (c011 - c010) + f.x * (c111 - c011);
    }
    let c010 = lut_at(i.x, i.y + 1u, i.z);
    let c110 = lut_at(i.x + 1u, i.y + 1u, i.z);
    return c000 + f.y * (c010 - c000) + f.x * (c110 - c010) + f.z * (c111 - c110);
}

// ------------------------------------------------------------ entry points

fn pack(c: vec3<f32>) -> u32 { return pack4x8unorm(vec4(c, 1.0)); }

// Before/after split: left of the divider shows the untouched photo.
fn finish(idx: u32, after: vec3<f32>, before: vec3<f32>) -> u32 {
    if p.split > 0.0 {
        let x = f32(idx % p.width) - p.split * f32(p.width);
        if abs(x) < 1.0 { return pack(vec3(1.0)); }
        if x < 0.0 { return pack(before); }
    }
    return pack(after);
}

@compute @workgroup_size(256)
fn preview(@builtin(global_invocation_id) id: vec3<u32>) {
    let idx = id.x;
    if idx >= p.width * p.height { return; }
    let c = src[idx].rgb;
    dst_px[idx] = finish(idx, grade(c), c);
}

@compute @workgroup_size(256)
fn apply_lut(@builtin(global_invocation_id) id: vec3<u32>) {
    let idx = id.x;
    if idx >= p.width * p.height { return; }
    let c = src[idx].rgb;
    dst_px[idx] = finish(idx, tetrahedral(c), c);
}

@compute @workgroup_size(256)
fn bake(@builtin(global_invocation_id) id: vec3<u32>) {
    let n = p.lut_size;
    let idx = id.x;
    if idx >= n * n * n { return; }
    let rgb = vec3<f32>(vec3(idx % n, (idx / n) % n, idx / (n * n))) / f32(n - 1u);
    dst_lut[idx] = vec4(grade(rgb), 1.0);
}
