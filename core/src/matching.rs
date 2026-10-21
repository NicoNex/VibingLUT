//! Reference match: the colour transfer that makes a photo look like a
//! reference, expressed as a per-pixel map so it bakes into the LUT.
//!
//! Lightness and colour are matched separately in OKLab, where they are
//! perceptually independent:
//! - **L** by histogram matching (each source quantile goes to the same
//!   reference quantile), a monotone curve with its slope limited so sparse
//!   tones are not stretched into banding;
//! - **a/b** by the Monge–Kantorovich linear transfer (Pitié & Kokaram 2007):
//!   the affine map that carries the source's mean and covariance onto the
//!   reference's while moving colours as little as possible. Unlike a
//!   per-axis mean/std transfer it also matches how colours co-vary (the
//!   tilt of the chroma cloud), without the cast of a full 3D transfer.

use crate::grade::Curve;
use crate::lab::to_oklab;

/// The match as the grade stores it: `Params::match_ab` and the MATCH_L curve.
#[derive(Clone, Debug, PartialEq)]
pub struct Match {
    pub ab: [f32; 6],
    pub lightness: Curve,
}

const BINS: usize = 512;
const NODES: usize = 17;
/// Slope limits of the lightness map: tones sparse in the source cannot be
/// stretched (banding) or crushed (lost detail) beyond this.
const MAX_SLOPE: f32 = 3.0;
const MIN_SLOPE: f32 = 1.0 / 3.0;
/// Covariance floor: a nearly grey source must not get its noise amplified
/// into colour. About a 0.015 OKLab chroma spread.
const COV_FLOOR: f64 = 2.0e-4;

/// OKLab samples of display-encoded RGBA8 pixels (every `step`-th one).
pub fn samples_rgba8(rgba: &[u8], max: usize) -> Vec<[f32; 3]> {
    let n = rgba.len() / 4;
    let step = (n / max).max(1);
    (0..n).step_by(step).map(|i| to_oklab([0, 1, 2].map(|c| rgba[i * 4 + c] as f32 / 255.0))).collect()
}

/// OKLab samples of display-encoded float pixels.
pub fn samples_rgba_f32(pixels: &[[f32; 4]], max: usize) -> Vec<[f32; 3]> {
    let step = (pixels.len() / max).max(1);
    pixels.iter().step_by(step).map(|p| to_oklab([p[0], p[1], p[2]].map(|v| v.clamp(0.0, 1.0)))).collect()
}

/// The transfer from `source` to `reference` (both OKLab samples).
pub fn transfer(source: &[[f32; 3]], reference: &[[f32; 3]]) -> Match {
    Match { ab: ab_transfer(source, reference), lightness: lightness_map(source, reference) }
}

fn cdf(samples: &[[f32; 3]]) -> Vec<f32> {
    let mut hist = vec![0u32; BINS];
    for s in samples {
        hist[((s[0].clamp(0.0, 1.0) * (BINS - 1) as f32).round()) as usize] += 1;
    }
    let total = samples.len().max(1) as f32;
    let mut acc = 0;
    hist.iter()
        .map(|h| {
            acc += h;
            acc as f32 / total
        })
        .collect()
}

/// Lightness at which `cdf` reaches `q` (linear between bins).
fn quantile(cdf: &[f32], q: f32) -> f32 {
    let i = cdf.partition_point(|&c| c < q).min(BINS - 1);
    if i == 0 {
        return 0.0;
    }
    let (c0, c1) = (cdf[i - 1], cdf[i]);
    let t = if c1 > c0 { (q - c0) / (c1 - c0) } else { 0.0 };
    ((i - 1) as f32 + t) / (BINS - 1) as f32
}

fn lightness_map(source: &[[f32; 3]], reference: &[[f32; 3]]) -> Curve {
    let (cs, cr) = (cdf(source), cdf(reference));
    // Quantiles trimmed at the ends: a few stray pixels must not set black/white.
    let map = |x: f32| quantile(&cr, cs[(x * (BINS - 1) as f32).round() as usize].clamp(0.002, 0.998));
    // Outside the tones the source actually has, continue at slope 1: those
    // tones are only shifted, as the nearest matched ones are.
    let (lo, hi) = (quantile(&cs, 0.002), quantile(&cs, 0.998));
    let q = |x: f32| {
        if x < lo {
            map(lo) - (lo - x)
        } else if x > hi {
            map(hi) + (x - hi)
        } else {
            map(x)
        }
    };
    let mut y: Vec<f32> = (0..NODES).map(|i| q(i as f32 / (NODES - 1) as f32)).collect();
    // Limit the slope, forwards then backwards, keeping the curve monotone.
    let dx = 1.0 / (NODES - 1) as f32;
    for i in 1..NODES {
        y[i] = y[i].clamp(y[i - 1] + MIN_SLOPE * dx, y[i - 1] + MAX_SLOPE * dx);
    }
    for i in (0..NODES - 1).rev() {
        y[i] = y[i].clamp(y[i + 1] - MAX_SLOPE * dx, y[i + 1] - MIN_SLOPE * dx);
    }
    (0..NODES).map(|i| (i as f32 * dx, y[i].clamp(0.0, 1.0))).collect()
}

type M2 = [[f64; 2]; 2];

fn mul(a: M2, b: M2) -> M2 {
    std::array::from_fn(|i| std::array::from_fn(|j| a[i][0] * b[0][j] + a[i][1] * b[1][j]))
}

/// Square root of a symmetric positive-definite 2×2 matrix (closed form).
fn sqrt_spd(m: M2) -> M2 {
    let s = (m[0][0] * m[1][1] - m[0][1] * m[1][0]).max(0.0).sqrt();
    let t = (m[0][0] + m[1][1] + 2.0 * s).sqrt();
    [[(m[0][0] + s) / t, m[0][1] / t], [m[1][0] / t, (m[1][1] + s) / t]]
}

fn inverse(m: M2) -> M2 {
    let det = m[0][0] * m[1][1] - m[0][1] * m[1][0];
    [[m[1][1] / det, -m[0][1] / det], [-m[1][0] / det, m[0][0] / det]]
}

fn mean_cov(samples: &[[f32; 3]]) -> ([f64; 2], M2) {
    let n = samples.len().max(1) as f64;
    let mu = [1, 2].map(|k| samples.iter().map(|s| s[k] as f64).sum::<f64>() / n);
    let mut cov = [[0.0; 2]; 2];
    for s in samples {
        let d = [s[1] as f64 - mu[0], s[2] as f64 - mu[1]];
        for i in 0..2 {
            for j in 0..2 {
                cov[i][j] += d[i] * d[j] / n;
            }
        }
    }
    cov[0][0] += COV_FLOOR;
    cov[1][1] += COV_FLOOR;
    (mu, cov)
}

/// Monge–Kantorovich linear map T = Σs^-½ (Σs^½ Σr Σs^½)^½ Σs^-½,
/// t = μr − T μs, as a delta from the identity.
fn ab_transfer(source: &[[f32; 3]], reference: &[[f32; 3]]) -> [f32; 6] {
    let ((ms, cs), (mr, cr)) = (mean_cov(source), mean_cov(reference));
    let half = sqrt_spd(cs);
    let inv_half = inverse(half);
    let t = mul(mul(inv_half, sqrt_spd(mul(mul(half, cr), half))), inv_half);
    let off = [0, 1].map(|i| mr[i] - t[i][0] * ms[0] - t[i][1] * ms[1]);
    [t[0][0] - 1.0, t[0][1], t[1][0], t[1][1] - 1.0, off[0], off[1]].map(|v| v as f32)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apply_ab(ab: &[f32; 6], s: [f32; 3]) -> [f32; 2] {
        [(1.0 + ab[0]) * s[1] + ab[1] * s[2] + ab[4], ab[2] * s[1] + (1.0 + ab[3]) * s[2] + ab[5]]
    }

    /// Deterministic pseudo-random OKLab cloud with the given L range and a/b shape.
    fn cloud(n: usize, l: (f32, f32), mean: [f32; 2], axes: [[f32; 2]; 2]) -> Vec<[f32; 3]> {
        let mut x = 12345u32;
        let mut rnd = || {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x as f32 / u32::MAX as f32 * 2.0 - 1.0
        };
        (0..n)
            .map(|_| {
                let (u, v, w) = (rnd(), rnd(), rnd());
                let a = mean[0] + axes[0][0] * u + axes[1][0] * v;
                let b = mean[1] + axes[0][1] * u + axes[1][1] * v;
                [l.0 + (l.1 - l.0) * (w * 0.5 + 0.5), a, b]
            })
            .collect()
    }

    /// Matching a photo to itself changes nothing.
    #[test]
    fn self_match_is_identity() {
        let s = cloud(20_000, (0.1, 0.9), [0.02, -0.01], [[0.05, 0.01], [-0.01, 0.03]]);
        let m = transfer(&s, &s);
        assert!(m.ab.iter().all(|v| v.abs() < 1e-3), "{:?}", m.ab);
        for (x, y) in &m.lightness {
            assert!((x - y).abs() < 0.02, "L {x} → {y}");
        }
    }

    /// The mapped source has the reference's a/b mean and covariance, and its
    /// lightness lands in the reference's range, monotonically.
    #[test]
    fn moves_source_statistics_onto_reference() {
        let s = cloud(40_000, (0.05, 0.95), [0.0, 0.0], [[0.04, 0.0], [0.0, 0.04]]);
        let r = cloud(40_000, (0.2, 0.7), [0.03, 0.04], [[0.06, 0.03], [-0.01, 0.02]]);
        let m = transfer(&s, &r);
        let mapped: Vec<[f32; 3]> = s
            .iter()
            .map(|&p| {
                let [a, b] = apply_ab(&m.ab, p);
                [p[0], a, b]
            })
            .collect();
        let ((mm, cm), (mr, cr)) = (mean_cov(&mapped), mean_cov(&r));
        for i in 0..2 {
            assert!((mm[i] - mr[i]).abs() < 2e-3, "mean {mm:?} vs {mr:?}");
            for j in 0..2 {
                assert!((cm[i][j] - cr[i][j]).abs() < 2e-4, "cov {cm:?} vs {cr:?}");
            }
        }
        let l = &m.lightness;
        assert!(l.windows(2).all(|w| w[1].1 > w[0].1), "not monotone: {l:?}");
        assert!((l[0].1 - 0.2).abs() < 0.06 && (l[NODES - 1].1 - 0.7).abs() < 0.06, "{l:?}");
        assert!(l.windows(2).all(|w| (w[1].1 - w[0].1) * (NODES - 1) as f32 <= MAX_SLOPE + 1e-4));
    }

    /// A grey source is not given colour out of nothing: the covariance floor
    /// keeps the a/b gain bounded.
    #[test]
    fn grey_source_stays_tame() {
        let s = cloud(10_000, (0.1, 0.9), [0.0, 0.0], [[0.001, 0.0], [0.0, 0.001]]);
        let r = cloud(10_000, (0.1, 0.9), [0.0, 0.0], [[0.08, 0.0], [0.0, 0.08]]);
        let m = transfer(&s, &r);
        assert!(m.ab[0] < 5.0 && m.ab[3] < 5.0, "gain {:?}", m.ab);
    }
}
