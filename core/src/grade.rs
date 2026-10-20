//! Grade parameters shared with the GPU, and tone-curve sampling.
//!
//! Every operation is per-pixel (no spatial filters), so the whole grade is
//! exactly representable as a 3D LUT.

use bytemuck::{Pod, Zeroable};

/// Resolution of each baked tone curve sent to the GPU.
pub const CURVE_RES: usize = 1024;

/// Must match `struct Params` in `grade.wgsl` field for field.
/// All-zero is the identity grade.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Pod, Zeroable)]
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
    /// [`Profile`] of the input signal (the LUT's input domain).
    pub input: u32,
    /// 0 = off, otherwise fraction of the width showing the ungraded image.
    pub split: f32,
    pub width: u32,
    pub height: u32,
    pub lut_size: u32,
}

/// Camera signal the LUT will be applied to.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Profile {
    /// Display-referred Rec.709 / sRGB (camera Standard, Vivid, … photo styles).
    #[default]
    Standard,
    /// Panasonic V-Log / V-Gamut: the LUT maps V-Log to Rec.709.
    VLog,
}

/// Control points of one curve, sorted by x, endpoints included.
pub type Curve = Vec<(f32, f32)>;

pub fn identity_curve() -> Curve {
    vec![(0.0, 0.0), (1.0, 1.0)]
}

/// Full grade state: scalar params plus master/R/G/B curves.
#[derive(Clone, Debug, PartialEq)]
pub struct Grade {
    pub params: Params,
    pub curves: [Curve; 4],
}

impl Default for Grade {
    fn default() -> Self {
        Self { params: Params::default(), curves: std::array::from_fn(|_| identity_curve()) }
    }
}

impl Grade {
    /// Curves packed as 4 × CURVE_RES samples for the GPU.
    pub fn curve_samples(&self) -> Vec<f32> {
        let mut out = vec![0.0; 4 * CURVE_RES];
        for (c, chunk) in self.curves.iter().zip(out.chunks_mut(CURVE_RES)) {
            sample_curve(c, chunk);
        }
        out
    }
}

/// Monotone cubic Hermite interpolation (Fritsch–Carlson): smooth like a
/// spline but never overshoots between points, so no banding/solarisation.
pub fn sample_curve(pts: &[(f32, f32)], out: &mut [f32]) {
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
        let x = i as f32 / last as f32;
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
    use super::*;

    #[test]
    fn params_layout_matches_wgsl() {
        // 12 scalars + 4×4 wheels + 3×8 hue + input,split,width,height,lut_size
        assert_eq!(std::mem::size_of::<Params>(), (12 + 16 + 24 + 5) * 4);
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
}
