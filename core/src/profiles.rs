//! Camera input profiles: log curves (manufacturer OETFs) and gamuts.
//!
//! Every curve here maps scene-linear reflectance (18% grey = 0.18) to the
//! full-range code value the camera writes; `grade.wgsl` holds the matching
//! decoders, keyed by [`Profile::curve`]. Gamut matrices are derived from
//! the published primaries rather than transcribed.

/// Camera signal the LUT will be applied to.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Profile {
    /// Display-referred Rec.709 / sRGB (Standard, Vivid, … photo styles).
    #[default]
    Standard,
    VLog,
    SLog3Cine,
    SLog3,
    CLog3,
    FLog,
    FLog2,
    NLog,
    LogC3,
    LogC4,
    BmdFilmGen5,
    DLog,
    Log3G10,
    AppleLog,
}

type Xy = (f64, f64);
const D65: Xy = (0.3127, 0.3290);
const REC709: [Xy; 3] = [(0.64, 0.33), (0.30, 0.60), (0.15, 0.06)];
const REC2020: [Xy; 3] = [(0.708, 0.292), (0.170, 0.797), (0.131, 0.046)];

impl Profile {
    pub const ALL: [Profile; 14] = [
        Profile::Standard,
        Profile::VLog,
        Profile::SLog3Cine,
        Profile::SLog3,
        Profile::CLog3,
        Profile::FLog,
        Profile::FLog2,
        Profile::NLog,
        Profile::LogC3,
        Profile::LogC4,
        Profile::BmdFilmGen5,
        Profile::DLog,
        Profile::Log3G10,
        Profile::AppleLog,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Profile::Standard => "Standard · Rec.709",
            Profile::VLog => "Panasonic V-Log",
            Profile::SLog3Cine => "Sony S-Log3 · S-Gamut3.Cine",
            Profile::SLog3 => "Sony S-Log3 · S-Gamut3",
            Profile::CLog3 => "Canon C-Log3",
            Profile::FLog => "Fujifilm F-Log",
            Profile::FLog2 => "Fujifilm F-Log2",
            Profile::NLog => "Nikon N-Log",
            Profile::LogC3 => "ARRI LogC3 (EI 800)",
            Profile::LogC4 => "ARRI LogC4",
            Profile::BmdFilmGen5 => "Blackmagic Film Gen 5",
            Profile::DLog => "DJI D-Log",
            Profile::Log3G10 => "RED Log3G10",
            Profile::AppleLog => "Apple Log",
        }
    }

    /// Short file-name tag.
    pub fn tag(self) -> &'static str {
        match self {
            Profile::Standard => "709",
            Profile::VLog => "VLog",
            Profile::SLog3Cine => "SLog3Cine",
            Profile::SLog3 => "SLog3",
            Profile::CLog3 => "CLog3",
            Profile::FLog => "FLog",
            Profile::FLog2 => "FLog2",
            Profile::NLog => "NLog",
            Profile::LogC3 => "LogC3",
            Profile::LogC4 => "LogC4",
            Profile::BmdFilmGen5 => "BMDFilm5",
            Profile::DLog => "DLog",
            Profile::Log3G10 => "Log3G10",
            Profile::AppleLog => "AppleLog",
        }
    }

    /// Decoder id in `grade.wgsl` (0 = display-referred, no decode).
    pub fn curve(self) -> u32 {
        match self {
            Profile::Standard => 0,
            Profile::VLog => 1,
            Profile::SLog3Cine | Profile::SLog3 => 2,
            Profile::CLog3 => 3,
            Profile::FLog => 4,
            Profile::FLog2 => 5,
            Profile::NLog => 6,
            Profile::LogC3 => 7,
            Profile::LogC4 => 8,
            Profile::BmdFilmGen5 => 9,
            Profile::DLog => 10,
            Profile::Log3G10 => 11,
            Profile::AppleLog => 12,
        }
    }

    pub fn is_log(self) -> bool {
        self != Profile::Standard
    }

    /// Camera gamut primaries (R, G, B) and white point.
    fn gamut(self) -> ([Xy; 3], Xy) {
        match self {
            Profile::Standard => (REC709, D65),
            Profile::VLog => ([(0.730, 0.280), (0.165, 0.840), (0.100, -0.030)], D65),
            Profile::SLog3Cine => ([(0.766, 0.275), (0.225, 0.800), (0.089, -0.087)], D65),
            Profile::SLog3 => ([(0.730, 0.280), (0.140, 0.855), (0.100, -0.050)], D65),
            Profile::CLog3 => ([(0.740, 0.270), (0.170, 1.140), (0.080, -0.100)], D65),
            // F-Gamut, N-Log and Apple Log use Rec.2020 primaries.
            Profile::FLog | Profile::FLog2 | Profile::NLog | Profile::AppleLog => (REC2020, D65),
            Profile::LogC3 => ([(0.6840, 0.3130), (0.2210, 0.8480), (0.0861, -0.1020)], D65),
            Profile::LogC4 => ([(0.7347, 0.2653), (0.1424, 0.8576), (0.0991, -0.0308)], D65),
            Profile::BmdFilmGen5 => {
                ([(0.7177215, 0.3171181), (0.2280410, 0.8615690), (0.1005841, -0.0820452)], (0.3127170, 0.3290312))
            }
            Profile::DLog => ([(0.71, 0.31), (0.21, 0.88), (0.09, -0.08)], D65),
            Profile::Log3G10 => ([(0.780308, 0.304253), (0.121595, 1.493994), (0.095612, -0.084589)], D65),
        }
    }

    /// Row-major camera-gamut → linear Rec.709 matrix.
    pub fn to_rec709(self) -> [[f32; 3]; 3] {
        let (p, w) = self.gamut();
        let m = mul(&inverse(&npm(REC709, D65)), &npm(p, w));
        m.map(|r| r.map(|v| v as f32))
    }

    /// Row-major linear Rec.709 → camera-gamut matrix.
    pub fn from_rec709(self) -> [[f32; 3]; 3] {
        let (p, w) = self.gamut();
        let m = mul(&inverse(&npm(p, w)), &npm(REC709, D65));
        m.map(|r| r.map(|v| v as f32))
    }

    /// Manufacturer OETF: scene-linear reflectance → full-range code value.
    pub fn encode(self, l: f32) -> f32 {
        let l = l as f64;
        let log10 = f64::log10;
        let v = match self {
            Profile::Standard => return l as f32,
            Profile::VLog => {
                if l < 0.01 {
                    5.6 * l + 0.125
                } else {
                    0.241514 * log10(l + 0.00873) + 0.598206
                }
            }
            Profile::SLog3Cine | Profile::SLog3 => {
                let cv = if l >= 0.01125 {
                    420.0 + log10((l + 0.01) / 0.19) * 261.5
                } else {
                    (l * (171.2102946929 - 95.0) / 0.01125) + 95.0
                };
                cv / 1023.0
            }
            Profile::CLog3 => {
                // Canon v1.2 formula on IRE (legal range), reflection = 0.9 × linear.
                let x = l / 0.9;
                let ire = if x < -0.014 {
                    -0.36726845 * log10(-x * 14.98325 + 1.0) + 0.12783901
                } else if x <= 0.014 {
                    1.9754798 * x + 0.12512219
                } else {
                    0.36726845 * log10(x * 14.98325 + 1.0) + 0.12240537
                };
                (ire * 876.0 + 64.0) / 1023.0
            }
            Profile::FLog => {
                if l >= 0.00089 {
                    0.344676 * log10(0.555556 * l + 0.009468) + 0.790453
                } else {
                    8.735631 * l + 0.092864
                }
            }
            Profile::FLog2 => {
                if l >= 0.000889 {
                    0.245281 * log10(5.555556 * l + 0.064829) + 0.384316
                } else {
                    8.799461 * l + 0.092864
                }
            }
            Profile::NLog => {
                let cv = if l < 0.328 { 650.0 * (l + 0.0075).cbrt() } else { 150.0 * l.ln() + 619.0 };
                cv / 1023.0
            }
            Profile::LogC3 => {
                if l > 0.010591 {
                    0.247190 * log10(5.555556 * l + 0.052272) + 0.385537
                } else {
                    5.367655 * l + 0.092809
                }
            }
            Profile::LogC4 => {
                let (a, b, c) = LOGC4;
                let s = (7.0 * std::f64::consts::LN_2 * 2f64.powf(7.0 - 14.0 * c / b)) / (a * b);
                let t = (2f64.powf(14.0 * (-c / b) + 6.0) - 64.0) / a;
                if l >= t { ((a * l + 64.0).log2() - 6.0) / 14.0 * b + c } else { (l - t) / s }
            }
            Profile::BmdFilmGen5 => {
                if l < 0.005 {
                    8.283605932 * l + 0.09246575342
                } else {
                    0.08692876065 * (l + 0.005494072432).ln() + 0.5300133392
                }
            }
            Profile::DLog => {
                if l <= 0.0078 {
                    6.025 * l + 0.0929
                } else {
                    log10(l * 0.9892 + 0.0108) * 0.256663 + 0.584555
                }
            }
            Profile::Log3G10 => {
                let x = l + 0.01;
                if x < 0.0 { x * 15.1927 } else { 0.224282 * log10(x * 155.975327 + 1.0) }
            }
            Profile::AppleLog => {
                let (r0, rt, c, b, g, beta) = APPLE_LOG;
                if l >= rt {
                    g * (l + b).log2() + beta
                } else if l >= r0 {
                    c * (l - r0) * (l - r0)
                } else {
                    0.0
                }
            }
        };
        v as f32
    }
}

const LOGC4: (f64, f64, f64) = ((262144.0 - 16.0) / 117.45, (1023.0 - 95.0) / 1023.0, 95.0 / 1023.0);
const APPLE_LOG: (f64, f64, f64, f64, f64, f64) = (-0.05641088, 0.01, 47.28711236, 0.00964052, 0.08550479, 0.69336945);

type M3 = [[f64; 3]; 3];

/// Normalised primary matrix (RGB → XYZ) from xy primaries and white.
fn npm(p: [Xy; 3], w: Xy) -> M3 {
    let xyz = |(x, y): Xy| [x / y, 1.0, (1.0 - x - y) / y];
    let cols = p.map(xyz);
    let prim = [
        [cols[0][0], cols[1][0], cols[2][0]],
        [cols[0][1], cols[1][1], cols[2][1]],
        [cols[0][2], cols[1][2], cols[2][2]],
    ];
    let wxyz = xyz(w);
    let inv = inverse(&prim);
    let s: [f64; 3] = std::array::from_fn(|i| (0..3).map(|k| inv[i][k] * wxyz[k]).sum());
    prim.map(|row| std::array::from_fn(|j| row[j] * s[j]))
}

fn mul(a: &M3, b: &M3) -> M3 {
    std::array::from_fn(|i| std::array::from_fn(|j| (0..3).map(|k| a[i][k] * b[k][j]).sum()))
}

fn inverse(m: &M3) -> M3 {
    let [[a, b, c], [d, e, f], [g, h, i]] = *m;
    let det = a * (e * i - f * h) - b * (d * i - f * g) + c * (d * h - e * g);
    [
        [(e * i - f * h) / det, (c * h - b * i) / det, (b * f - c * e) / det],
        [(f * g - d * i) / det, (a * i - c * g) / det, (c * d - a * f) / det],
        [(d * h - e * g) / det, (b * g - a * h) / det, (a * e - b * d) / det],
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn curves_match_published_grey_points() {
        // 18% grey code values from the manufacturers' white papers.
        let cases = [
            (Profile::VLog, 0.4233),
            (Profile::SLog3, 420.0 / 1023.0),
            (Profile::CLog3, 0.3566),
            (Profile::FLog, 0.4593),
            (Profile::NLog, 372.0 / 1023.0), // cube-root segment: 650·∛(0.18 + 0.0075)
            (Profile::LogC3, 0.3910),
            (Profile::LogC4, 0.2784),
        ];
        for (p, want) in cases {
            assert!((p.encode(0.18) - want).abs() < 2e-3, "{p:?}: {} vs {want}", p.encode(0.18));
        }
    }

    #[test]
    fn gamuts_keep_white_and_round_trip() {
        // Panasonic's published V-Gamut → Rec.709 first row.
        let v = Profile::VLog.to_rec709();
        assert!((v[0][0] - 1.806574).abs() < 2e-3 && (v[0][1] + 0.695698).abs() < 2e-3);
        for p in Profile::ALL {
            let (to, from) = (p.to_rec709(), p.from_rec709());
            for (r, row) in to.iter().enumerate() {
                assert!((row.iter().sum::<f32>() - 1.0).abs() < 1e-3, "{p:?} white row {r}");
                for c in 0..3 {
                    let id: f32 = row.iter().zip(&from).map(|(a, f)| a * f[c]).sum();
                    assert!((id - if r == c { 1.0 } else { 0.0 }).abs() < 1e-4, "{p:?}");
                }
            }
        }
    }
}
