//! LUT Lab: combine LUTs, the current grade and profile conversions into one
//! LUT, either as a chain (each step fed by the previous one) or as a
//! weighted mix of their outputs.
//!
//! The result is evaluated exactly on every lattice point of the output cube
//! (each source sampled with tetrahedral interpolation), and blends happen in
//! OKLab so half-way mixes keep their saturation instead of greying out.

use std::sync::Arc;

use crate::grade::Grade;
use crate::lut::{Format, Lut3d};
use crate::photo::{inverse_view_transform, mat3, srgb_decode, view_transform};
use crate::profiles::Profile;

#[derive(Clone, Debug)]
pub enum Source {
    Lut(Arc<Lut3d>),
    Grade(Box<Grade>),
    /// Re-encode the signal as `Profile` (no look, just the conversion).
    Convert(Profile),
}

#[derive(Clone, Debug)]
pub struct Layer {
    pub source: Source,
    /// 0..1. Fades a LUT/grade against the neutral rendering of its input
    /// (chain), or weights it (mix). Ignored for conversions.
    pub intensity: f32,
    /// Signal the LUT or grade expects (e.g. F-Log for Fujifilm's LUTs).
    /// The signal is converted to it automatically.
    pub input: Profile,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Blend {
    #[default]
    Chain,
    Mix,
}

/// Size of the lattice grades are baked at before composing.
pub const SOURCE_SIZE: usize = 65;

/// Re-encodes `c` from one camera signal to another.
///
/// Log → log is exact and scene-referred (decode, gamut A → B, encode).
/// Standard is display-referred: it goes through the neutral view transform
/// (or its exact inverse), the same one the app renders log footage with.
/// Evaluated analytically — never through a baked LUT — because gamut
/// matrices subtract large terms and resampling would amplify lattice error.
pub fn convert(from: Profile, to: Profile, c: [f32; 3]) -> [f32; 3] {
    if from == to {
        return c;
    }
    let scene = if from.is_log() {
        mat3(&from.to_rec709(), c.map(|v| from.decode(v)))
    } else {
        inverse_view_transform(c.map(srgb_decode))
    };
    if to.is_log() {
        mat3(&to.from_rec709(), scene).map(|v| to.encode(v).clamp(0.0, 1.0))
    } else {
        view_transform(scene)
    }
}

struct Step {
    lut: Option<Arc<Lut3d>>,
    input: Profile,
    /// Profile of the step's output (Standard after a LUT or grade).
    output: Profile,
    weight: f32,
}

impl Step {
    /// Applies the step to `c`, a signal in profile `from`.
    fn apply(&self, from: Profile, c: [f32; 3]) -> [f32; 3] {
        let x = convert(from, self.input, c);
        match &self.lut {
            Some(l) => l.sample(x),
            None => x,
        }
    }
}

/// Composes `layers` into a `size`³ LUT whose input is `input`. `bake`
/// turns a grade into a LUT (it runs on the GPU, unknown to this module).
/// A chain ends in whatever profile its last step outputs. A mix blends the
/// looks display-referred (Rec.709), then applies any conversion layers.
pub fn compose(
    input: Profile,
    layers: &[Layer],
    blend: Blend,
    size: usize,
    bake: &dyn Fn(&Grade) -> Arc<Lut3d>,
) -> Lut3d {
    let steps: Vec<Step> = layers
        .iter()
        .map(|l| {
            let w = l.intensity.clamp(0.0, 1.0);
            match &l.source {
                Source::Lut(lut) => {
                    Step { lut: Some(lut.clone()), input: l.input, output: Profile::Standard, weight: w }
                }
                Source::Grade(g) => Step { lut: Some(bake(g)), input: l.input, output: Profile::Standard, weight: w },
                Source::Convert(p) => Step { lut: None, input: *p, output: *p, weight: 1.0 },
            }
        })
        .collect();
    let total: f32 = steps.iter().filter(|s| s.lut.is_some()).map(|s| s.weight).sum();
    let mut out = Lut3d::identity(size, Format::Cube);
    out.title = "VibingLUT Lab".into();
    for c in &mut out.data {
        *c = match blend {
            Blend::Chain => {
                let (mut v, mut at) = (*c, input);
                for s in &steps {
                    let y = s.apply(at, v);
                    // Fade against the neutral rendering in the output's own domain.
                    v = if s.weight < 1.0 { mix_oklab(convert(at, s.output, v), y, s.weight) } else { y };
                    at = s.output;
                }
                v
            }
            Blend::Mix => {
                // Weighted average of the looks; when the weights sum below 1
                // the remainder is the neutral image, so a single layer fades.
                let neutral = convert(input, Profile::Standard, *c);
                let mut v = if total > 0.0 {
                    let lab = steps.iter().filter(|s| s.lut.is_some()).fold([0.0; 3], |acc, s| {
                        let l = to_oklab(convert(s.output, Profile::Standard, s.apply(input, *c)));
                        std::array::from_fn(|k| acc[k] + l[k] * s.weight / total)
                    });
                    mix_oklab(neutral, from_oklab(lab), total.min(1.0))
                } else {
                    neutral
                };
                // Conversions act on the mixed result, in stack order.
                let mut at = Profile::Standard;
                for s in steps.iter().filter(|s| s.lut.is_none()) {
                    v = convert(at, s.output, v);
                    at = s.output;
                }
                v
            }
        }
        .map(|v| v.clamp(0.0, 1.0));
    }
    out
}

fn mix_oklab(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
    if t >= 1.0 {
        return b;
    }
    if t <= 0.0 {
        return a;
    }
    let (la, lb) = (to_oklab(a), to_oklab(b));
    from_oklab(std::array::from_fn(|k| la[k] + (lb[k] - la[k]) * t))
}

/// Display-encoded sRGB → OKLab (Ottosson's published constants, kept verbatim).
#[allow(clippy::excessive_precision)]
fn to_oklab(c: [f32; 3]) -> [f32; 3] {
    let [r, g, b] = c.map(srgb_decode);
    let lms = [
        0.4122214708 * r + 0.5363325363 * g + 0.0514459929 * b,
        0.2119034982 * r + 0.6806995451 * g + 0.1073969566 * b,
        0.0883024619 * r + 0.2817188376 * g + 0.6299787005 * b,
    ]
    .map(f32::cbrt);
    [
        0.2104542553 * lms[0] + 0.7936177850 * lms[1] - 0.0040720468 * lms[2],
        1.9779984951 * lms[0] - 2.4285922050 * lms[1] + 0.4505937099 * lms[2],
        0.0259040371 * lms[0] + 0.7827717662 * lms[1] - 0.8086757660 * lms[2],
    ]
}

/// OKLab → display-encoded sRGB.
#[allow(clippy::excessive_precision)]
fn from_oklab([l, a, b]: [f32; 3]) -> [f32; 3] {
    let lms = [
        l + 0.3963377774 * a + 0.2158037573 * b,
        l - 0.1055613458 * a - 0.0638541728 * b,
        l - 0.0894841775 * a - 1.2914855480 * b,
    ]
    .map(|v| v * v * v);
    let lin = [
        4.0767416621 * lms[0] - 3.3077115913 * lms[1] + 0.2309699292 * lms[2],
        -1.2684380046 * lms[0] + 2.6097574011 * lms[1] - 0.3413193965 * lms[2],
        -0.0041960863 * lms[0] - 0.7034186147 * lms[1] + 1.7076147010 * lms[2],
    ];
    lin.map(|v| rawler::imgop::srgb::srgb_apply_gamma(v.clamp(0.0, 1.0)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_bake(_: &Grade) -> Arc<Lut3d> {
        unreachable!("no GPU sources in these tests")
    }

    fn warm() -> Arc<Lut3d> {
        let mut l = Lut3d::identity(17, Format::Cube);
        for c in &mut l.data {
            *c = [c[0].powf(0.8), c[1], c[2].powf(1.25)];
        }
        Arc::new(l)
    }

    fn close(a: &Lut3d, b: &Lut3d, tol: f32) -> bool {
        a.data.iter().zip(&b.data).all(|(x, y)| (0..3).all(|k| (x[k] - y[k]).abs() < tol))
    }

    #[test]
    fn chain_and_mix_semantics() {
        let id = Lut3d::identity(17, Format::Cube);
        let full = |w| vec![Layer { source: Source::Lut(warm()), intensity: w, input: Profile::Standard }];
        // Intensity 0 → identity; 1 → the LUT itself.
        assert!(close(&compose(Profile::Standard, &full(0.0), Blend::Chain, 17, &no_bake), &id, 1e-4));
        assert!(close(&compose(Profile::Standard, &full(1.0), Blend::Chain, 17, &no_bake), &warm(), 1e-4));
        // Mixing a LUT with itself gives the LUT, whatever the weights.
        let pair = vec![
            Layer { source: Source::Lut(warm()), intensity: 0.3, input: Profile::Standard },
            Layer { source: Source::Lut(warm()), intensity: 0.9, input: Profile::Standard },
        ];
        assert!(close(&compose(Profile::Standard, &pair, Blend::Mix, 17, &no_bake), &warm(), 1e-3));
        // Half intensity lands between identity and the LUT.
        let half = compose(Profile::Standard, &full(0.5), Blend::Chain, 17, &no_bake);
        let i = 8 + 17 * (8 + 17 * 8); // mid grey-ish lattice point
        assert!(half.data[i][0] > id.data[i][0] && half.data[i][0] < warm().data[i][0]);
    }

    #[test]
    fn conversions_round_trip_between_all_profiles() {
        // Start from colours every profile can hold (inside Rec.709, below
        // display white), express them in A, then check A → B → A is exact.
        // A wide-gamut log colour outside Rec.709 can't survive Standard, so
        // samples outside it would test the gamut, not the maths.
        let display = [[0.18, 0.18, 0.18], [0.5, 0.3, 0.2], [0.1, 0.4, 0.6], [0.7, 0.65, 0.6], [0.02, 0.03, 0.05]];
        for a in Profile::ALL {
            for b in Profile::ALL {
                for d in display {
                    let x = convert(Profile::Standard, a, d);
                    let back = convert(b, a, convert(a, b, x));
                    assert!((0..3).all(|k| (back[k] - x[k]).abs() < 2e-3), "{a:?} → {b:?} → {a:?}: {x:?} → {back:?}");
                }
            }
        }
    }

    #[test]
    fn log_lut_retargets_to_another_profile() {
        // A LUT built for F-Log, used from V-Log: the result must equal
        // converting V-Log → F-Log by hand and sampling the LUT.
        let layers = [Layer { source: Source::Lut(warm()), intensity: 1.0, input: Profile::FLog }];
        let out = compose(Profile::VLog, &layers, Blend::Chain, 9, &no_bake);
        let c = [0.5, 0.375, 0.25]; // lattice point (4, 3, 2) of a 9³ cube
        let want = warm().sample(convert(Profile::VLog, Profile::FLog, c));
        let got = out.data[4 + 9 * (3 + 9 * 2)];
        assert!((0..3).all(|k| (got[k] - want[k]).abs() < 1e-5), "{got:?} vs {want:?}");
    }

    #[test]
    fn oklab_round_trip() {
        for c in [[0.0, 0.0, 0.0], [1.0, 1.0, 1.0], [0.8, 0.2, 0.1], [0.05, 0.6, 0.9], [0.5, 0.5, 0.5]] {
            let back = from_oklab(to_oklab(c));
            assert!((0..3).all(|k| (back[k] - c[k]).abs() < 1e-4), "{c:?} → {back:?}");
        }
    }

    #[test]
    fn chain_equals_sequential_application() {
        // The baked chain must match applying the LUTs one after the other,
        // up to the interpolation error of the output lattice.
        let cool = {
            let mut l = Lut3d::identity(17, Format::Cube);
            for c in &mut l.data {
                *c = [c[0] * 0.9, c[1].powf(1.1), (c[2] * 1.1).min(1.0)];
            }
            Arc::new(l)
        };
        let layers =
            [warm(), cool.clone()].map(|l| Layer { source: Source::Lut(l), intensity: 1.0, input: Profile::Standard });
        let out = compose(Profile::Standard, &layers, Blend::Chain, 65, &no_bake);
        for c in [[0.13, 0.77, 0.4], [0.9, 0.05, 0.6], [0.5, 0.5, 0.5], [0.02, 0.3, 0.98]] {
            let want = cool.sample(warm().sample(c));
            let got = out.sample(c);
            assert!((0..3).all(|k| (got[k] - want[k]).abs() < 5e-3), "{c:?}: {got:?} vs {want:?}");
        }
    }

    #[test]
    fn mix_weights_are_normalised_and_symmetric() {
        let layer = |l: Arc<Lut3d>, w| Layer { source: Source::Lut(l), intensity: w, input: Profile::Standard };
        let id = Arc::new(Lut3d::identity(17, Format::Cube));
        let mix = |layers: &[Layer]| compose(Profile::Standard, layers, Blend::Mix, 17, &no_bake);
        // Once the weights reach 100%, only their ratio matters, not scale or order.
        let a = mix(&[layer(warm(), 0.6), layer(id.clone(), 0.6)]);
        let b = mix(&[layer(id.clone(), 0.9), layer(warm(), 0.9)]);
        assert!(close(&a, &b, 1e-5), "equal weights must not depend on scale or order");
        // Below 100% the rest is neutral: one layer at w equals the chain at w.
        let solo = mix(&[layer(warm(), 0.3)]);
        let chain = compose(Profile::Standard, &[layer(warm(), 0.3)], Blend::Chain, 17, &no_bake);
        assert!(close(&solo, &chain, 1e-5));
        // A conversion layer in a mix re-encodes the mixed result.
        let conv = Layer { source: Source::Convert(Profile::VLog), intensity: 1.0, input: Profile::VLog };
        let to_vlog = mix(&[layer(warm(), 1.0), conv]);
        let c = to_vlog.data[5];
        let want = convert(Profile::Standard, Profile::VLog, warm().data[5]);
        assert!((0..3).all(|k| (c[k] - want[k]).abs() < 1e-4), "{c:?} vs {want:?}");
    }

    #[test]
    fn standard_lut_retargeted_to_log_and_back_is_unchanged() {
        // Standard LUT used from V-Log, fed V-Log footage of a Standard
        // colour, must give the original LUT's output. The only error left is
        // the output lattice's: tight on neutrals and mild colours, and on
        // saturated darks (where the V-Gamut matrix subtracts large terms) it
        // must converge as the lattice gets finer.
        let lut = warm();
        let retarget = |size| {
            let layers = [Layer { source: Source::Lut(lut.clone()), intensity: 1.0, input: Profile::Standard }];
            compose(Profile::VLog, &layers, Blend::Chain, size, &no_bake)
        };
        let err = |out: &Lut3d, c: [f32; 3]| {
            let via = out.sample(convert(Profile::Standard, Profile::VLog, c));
            let want = lut.sample(c);
            (0..3).map(|k| (via[k] - want[k]).abs()).fold(0.0, f32::max)
        };
        let (l65, l129) = (retarget(65), retarget(129));
        for c in [[0.5, 0.3, 0.2], [0.18, 0.18, 0.18], [0.7, 0.65, 0.6]] {
            assert!(err(&l65, c) < 4e-3, "{c:?}: {}", err(&l65, c));
        }
        let hard = [0.1, 0.4, 0.6];
        let (e65, e129) = (err(&l65, hard), err(&l129, hard));
        assert!(e65 < 0.03 && e129 < e65 * 0.6, "no convergence: 65³ {e65}, 129³ {e129}");
    }
}
