//! Grade parameters shared with the GPU, and tone-curve sampling.
//!
//! Every operation is per-pixel (no spatial filters), so the whole grade is
//! exactly representable as a 3D LUT.

use bytemuck::{Pod, Zeroable};
use serde::{Deserialize, Serialize};

use crate::profiles::Profile;

/// OKLCh hue centres (radians) of the 8 selective-colour bands — red,
/// orange, yellow, green, teal, blue, purple, magenta. Mirrors `HUES` in
/// `grade.wgsl`.
pub const BAND_HUES: [f32; 8] = [0.506, 0.960, 1.920, 2.478, 3.403, 4.608, 5.236, 5.725];
/// Hue shift at ±100 on a band, in radians (`band(p.hue_shift) * 0.005`).
pub const BAND_HUE_RANGE: f32 = 0.5;

/// Rec.709 luminance weights. Mirrors `LUMA` in `grade.wgsl`.
pub const LUMA: [f32; 3] = [0.2126, 0.7152, 0.0722];

/// Resolution of each baked tone curve sent to the GPU.
pub const CURVE_RES: usize = 1024;

/// Curve channels, in the order of `curves` and of the GPU buffer.
pub const MASTER: usize = 0;
pub const HUE_HUE: usize = 4;
pub const HUE_SAT: usize = 5;
pub const LUM_SAT: usize = 6;
/// Lightness map of the reference match (OKLab L in → L out).
pub const MATCH_L: usize = 7;
pub const CURVES: usize = 8;

/// Must match `struct Params` in `grade.wgsl` field for field.
/// All-zero is the identity grade. Presets serialise the grading fields by
/// name; missing ones load as neutral, so older presets stay valid.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Pod, Zeroable, Serialize, Deserialize)]
#[serde(default)]
pub struct Params {
    pub temperature: f32,
    pub tint: f32,
    pub exposure: f32,
    pub contrast: f32,
    pub highlights: f32,
    pub shadows: f32,
    pub whites: f32,
    pub blacks: f32,
    pub saturation: f32,
    pub vibrance: f32,
    pub sat_shadows: f32,
    pub sat_highlights: f32,
    /// Colour wheels: [chroma x, chroma y, luma, unused].
    pub lift: [f32; 4],
    pub gamma: [f32; 4],
    pub gain: [f32; 4],
    pub offset: [f32; 4],
    /// OKLCh hue bands: red, orange, yellow, green, aqua, blue, purple, magenta.
    pub hue_shift: [f32; 8],
    pub hue_sat: [f32; 8],
    pub hue_lum: [f32; 8],
    /// Channel mixer, row-major (out R, G, B × in R, G, B), percent added to the identity.
    pub mixer: [f32; 9],
    /// 1 = monochrome, weighted by luminance plus `mono_mix` (percent per input channel).
    pub mixer_mono: f32,
    pub mono_mix: [f32; 3],
    /// Film colour: saturation in dye-density space (−100..100) and density
    /// of saturated colours (0..100).
    pub film_sat: f32,
    pub film_density: f32,
    /// Reference match strength (0..100) and its OKLab a/b affine map, as a
    /// delta from the identity: [m00, m01, m10, m11, t0, t1].
    pub match_amount: f32,
    pub match_ab: [f32; 6],
    /// Input decoder id ([`Profile::curve`]); 0 = display-referred Rec.709.
    #[serde(skip)]
    pub input: u32,
    /// Row-major camera gamut → Rec.709 matrix for log inputs.
    #[serde(skip)]
    pub gamut: [f32; 9],
    /// 0 = off, otherwise fraction of the width showing the ungraded image.
    #[serde(skip)]
    pub split: f32,
    #[serde(skip)]
    pub width: u32,
    #[serde(skip)]
    pub height: u32,
    #[serde(skip)]
    pub lut_size: u32,
    /// Preview only: 1 = mark clipped and out-of-gamut pixels.
    #[serde(skip)]
    pub overlay: u32,
}

impl Params {
    /// Makes the LUT's input domain the signal of `profile`.
    pub fn set_input(&mut self, profile: Profile) {
        self.input = profile.curve();
        self.gamut = bytemuck::cast(profile.to_rec709());
    }
}

/// Control points of one curve, sorted by x, endpoints included.
pub type Curve = Vec<(f32, f32)>;

pub fn identity_curve() -> Curve {
    vec![(0.0, 0.0), (1.0, 1.0)]
}

/// Hue vs Hue / Hue vs Sat / Lum vs Sat are offsets around 0.5 (no change);
/// the rest map input to output.
pub fn neutral_curve(ch: usize) -> Curve {
    if (HUE_HUE..=LUM_SAT).contains(&ch) { vec![(0.0, 0.5), (1.0, 0.5)] } else { identity_curve() }
}

/// Hue curves wrap around: x = 0 and x = 1 are the same hue.
pub fn is_periodic(ch: usize) -> bool {
    ch == HUE_HUE || ch == HUE_SAT
}

/// Full grade state: scalar params plus master/R/G/B, hue and match curves.
#[derive(Clone, Debug, PartialEq)]
pub struct Grade {
    pub params: Params,
    pub curves: [Curve; CURVES],
}

impl Default for Grade {
    fn default() -> Self {
        Self { params: Params::default(), curves: std::array::from_fn(neutral_curve) }
    }
}

/// Groups of grade controls that can be switched off together.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    WhiteBalance,
    Tone,
    Color,
    Wheels,
    Selective,
    Curves,
    Mixer,
    Film,
    HueCurves,
    Match,
}

impl Stage {
    pub const ALL: [Stage; 10] = [
        Stage::Match,
        Stage::WhiteBalance,
        Stage::Tone,
        Stage::Mixer,
        Stage::Color,
        Stage::Film,
        Stage::Selective,
        Stage::HueCurves,
        Stage::Wheels,
        Stage::Curves,
    ];
}

impl Grade {
    /// True when only the input decoding remains: every stage is at its neutral value.
    pub fn is_neutral(&self) -> bool {
        self.neutralized() == *self
    }

    /// The grade with a neutral input decoding: what a preset stores, since
    /// the input belongs to the photo, not to the look.
    pub fn neutral_input(&self) -> Self {
        let mut g = self.clone();
        (g.params.input, g.params.gamut) = (0, [0.0; 9]);
        g
    }

    /// Every stage bypassed: only the input decoding remains.
    pub fn neutralized(&self) -> Self {
        let mut n = self.clone();
        Stage::ALL.into_iter().for_each(|s| n.bypass(s));
        n
    }

    /// Resets `stage` to neutral, leaving the rest of the grade (and the input) as is.
    pub fn bypass(&mut self, stage: Stage) {
        let (p, d, curves) = (&mut self.params, Params::default(), &mut self.curves);
        match stage {
            Stage::WhiteBalance => (p.temperature, p.tint) = (d.temperature, d.tint),
            Stage::Tone => {
                (p.exposure, p.contrast, p.highlights) = (d.exposure, d.contrast, d.highlights);
                (p.shadows, p.whites, p.blacks) = (d.shadows, d.whites, d.blacks);
            }
            Stage::Color => {
                (p.saturation, p.vibrance) = (d.saturation, d.vibrance);
                (p.sat_shadows, p.sat_highlights) = (d.sat_shadows, d.sat_highlights);
            }
            Stage::Wheels => (p.lift, p.gamma, p.gain, p.offset) = (d.lift, d.gamma, d.gain, d.offset),
            Stage::Selective => (p.hue_shift, p.hue_sat, p.hue_lum) = (d.hue_shift, d.hue_sat, d.hue_lum),
            Stage::Curves => (0..HUE_HUE).for_each(|c| curves[c] = neutral_curve(c)),
            Stage::Mixer => (p.mixer, p.mixer_mono, p.mono_mix) = (d.mixer, d.mixer_mono, d.mono_mix),
            Stage::Film => (p.film_sat, p.film_density) = (d.film_sat, d.film_density),
            Stage::HueCurves => (HUE_HUE..=LUM_SAT).for_each(|c| curves[c] = neutral_curve(c)),
            Stage::Match => {
                (p.match_amount, p.match_ab) = (d.match_amount, d.match_ab);
                curves[MATCH_L] = neutral_curve(MATCH_L);
            }
        }
    }

    /// Curves packed as CURVES × CURVE_RES samples for the GPU.
    pub fn curve_samples(&self) -> Vec<f32> {
        let mut out = vec![0.0; CURVES * CURVE_RES];
        for (ch, (c, chunk)) in self.curves.iter().zip(out.chunks_mut(CURVE_RES)).enumerate() {
            if is_periodic(ch) { sample_periodic(c, chunk) } else { sample_curve(c, chunk) }
        }
        out
    }
}

/// Monotone cubic Hermite interpolation (Fritsch–Carlson): smooth like a
/// spline but never overshoots between points, so no banding/solarisation.
pub fn sample_curve(pts: &[(f32, f32)], out: &mut [f32]) {
    sample_range(pts, out, 0.0, 1.0);
}

/// [`sample_curve`] for a hue curve: the points repeat one period to each
/// side, so the slope at red is continuous across 0 / 1.
pub fn sample_periodic(pts: &[(f32, f32)], out: &mut [f32]) {
    let inner = &pts[..pts.len() - 1]; // the last point is the first one again
    let ext: Vec<(f32, f32)> = [-1.0, 0.0, 1.0]
        .iter()
        .flat_map(|&k| inner.iter().map(move |&(x, y)| (x + k, y)))
        .chain(std::iter::once((2.0, pts[0].1)))
        .collect();
    sample_range(&ext, out, 0.0, 1.0);
}

/// Samples `pts` at `out.len()` evenly spaced x from `x0` to `x1`.
fn sample_range(pts: &[(f32, f32)], out: &mut [f32], x0: f32, x1: f32) {
    let n = pts.len();
    let d: Vec<f32> = pts.windows(2).map(|w| (w[1].1 - w[0].1) / (w[1].0 - w[0].0).max(1e-6)).collect();
    let mut m = vec![0.0f32; n];
    m[0] = d[0];
    m[n - 1] = d[n - 2];
    for k in 1..n - 1 {
        m[k] = if d[k - 1] * d[k] <= 0.0 { 0.0 } else { (d[k - 1] + d[k]) * 0.5 };
    }
    for k in 0..n - 1 {
        if d[k] == 0.0 {
            m[k] = 0.0;
            m[k + 1] = 0.0;
            continue;
        }
        let (a, b) = (m[k] / d[k], m[k + 1] / d[k]);
        let s = a * a + b * b;
        if s > 9.0 {
            let t = 3.0 / s.sqrt();
            m[k] = t * a * d[k];
            m[k + 1] = t * b * d[k];
        }
    }
    let last = out.len() - 1;
    let mut k = 0;
    for (i, o) in out.iter_mut().enumerate() {
        let x = x0 + (x1 - x0) * i as f32 / last as f32;
        while k < n - 2 && x > pts[k + 1].0 {
            k += 1;
        }
        let (x0, y0) = pts[k];
        let (x1, y1) = pts[k + 1];
        let h = (x1 - x0).max(1e-6);
        let t = ((x - x0) / h).clamp(0.0, 1.0);
        let (t2, t3) = (t * t, t * t * t);
        let y = (2.0 * t3 - 3.0 * t2 + 1.0) * y0
            + (t3 - 2.0 * t2 + t) * h * m[k]
            + (-2.0 * t3 + 3.0 * t2) * y1
            + (t3 - t2) * h * m[k + 1];
        *o = y.clamp(0.0, 1.0);
    }
}

#[cfg(test)]
mod tests {
    /// Bypassing every stage leaves only the input decoding: a neutral grade.
    #[test]
    fn bypassing_all_stages_is_neutral() {
        use super::*;
        let mut g = Grade::default();
        let p = &mut g.params;
        (p.temperature, p.exposure, p.saturation, p.sat_highlights) = (10.0, 0.5, 20.0, -5.0);
        (p.whites, p.lift, p.hue_lum[3]) = (7.0, [0.1, 0.0, 0.2, 0.0], 30.0);
        (p.mixer[1], p.mixer_mono, p.film_sat, p.film_density) = (20.0, 1.0, 30.0, 40.0);
        (p.match_amount, p.match_ab[4]) = (80.0, 0.01);
        g.curves[2] = vec![(0.0, 0.1), (1.0, 0.9)];
        g.curves[HUE_SAT] = vec![(0.0, 0.5), (0.3, 0.7), (1.0, 0.5)];
        g.curves[MATCH_L] = vec![(0.0, 0.1), (1.0, 1.0)];
        g.params.set_input(Profile::VLog);
        let mut want = Grade::default();
        want.params.set_input(Profile::VLog);
        Stage::ALL.into_iter().for_each(|s| g.bypass(s));
        assert_eq!(g, want);
    }

    use super::*;

    #[test]
    fn params_layout_matches_wgsl() {
        // 12 scalars + 4×4 wheels + 3×8 hue + mixer 9+1+3 + film 2 + match 1+6
        // + input + 3×3 gamut + split,width,height,lut_size,overlay
        assert_eq!(std::mem::size_of::<Params>(), (12 + 16 + 24 + 13 + 2 + 7 + 1 + 9 + 5) * 4);
    }

    #[test]
    fn curves_identity_and_monotone() {
        let mut out = vec![0.0; 256];
        sample_curve(&identity_curve(), &mut out);
        for (i, v) in out.iter().enumerate() {
            assert!((v - i as f32 / 255.0).abs() < 1e-5);
        }
        // Monotone data must stay monotone (no overshoot).
        sample_curve(&[(0.0, 0.0), (0.3, 0.6), (0.35, 0.62), (1.0, 1.0)], &mut out);
        assert!(out.windows(2).all(|w| w[1] >= w[0] - 1e-6));
    }

    /// A hue curve meets itself at red: same value and no kink across 0 / 1.
    #[test]
    fn periodic_curves_wrap() {
        let mut out = vec![0.0; 1024];
        sample_periodic(&neutral_curve(HUE_HUE), &mut out);
        assert!(out.iter().all(|v| (v - 0.5).abs() < 1e-6));
        sample_periodic(&[(0.0, 0.6), (0.5, 0.3), (1.0, 0.6)], &mut out);
        let n = out.len();
        assert!((out[0] - out[n - 1]).abs() < 1e-6);
        let (slope_end, slope_start) = (out[n - 1] - out[n - 2], out[1] - out[0]);
        assert!((slope_end - slope_start).abs() < 1e-3, "{slope_end} vs {slope_start}");
    }
}
