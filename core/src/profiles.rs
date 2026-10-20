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

    /// Best guess of the input profile a LUT file was made for, from its
    /// name (vendors put it there, e.g. `XH2S_FLog_FGamut_to_ETERNA_BT.709`).
    pub fn guess_from_name(name: &str) -> Profile {
        // Split into words (separators and camelCase), then match whole words
        // or runs of up to three adjacent words ("F-Log", "S-Gamut3.Cine"), so
        // creative names like "Analog" never read as a log profile.
        let mut words: Vec<String> = Vec::new();
        let mut prev: Option<char> = None;
        for c in name.chars() {
            if !c.is_ascii_alphanumeric() {
                prev = None;
                continue;
            }
            let boundary =
                prev.is_none_or(|p| (p.is_ascii_lowercase() || p.is_ascii_digit()) && c.is_ascii_uppercase());
            if boundary {
                words.push(String::new());
            }
            words.last_mut().expect("pushed above").push(c.to_ascii_lowercase());
            prev = Some(c);
        }
        let runs: Vec<String> =
            (1..=3).flat_map(|k| words.windows(k).map(|w| w.concat()).collect::<Vec<_>>()).collect();
        // Most specific first.
        const TOKENS: [(&str, Profile); 15] = [
            ("flog2", Profile::FLog2),
            ("flog", Profile::FLog),
            ("vlog", Profile::VLog),
            ("vgamut", Profile::VLog),
            ("sgamut3cine", Profile::SLog3Cine),
            ("slog3", Profile::SLog3),
            ("clog3", Profile::CLog3),
            ("nlog", Profile::NLog),
            ("logc4", Profile::LogC4),
            ("logc", Profile::LogC3),
            ("bmdfilm", Profile::BmdFilmGen5),
            ("blackmagicfilm", Profile::BmdFilmGen5),
            ("dlog", Profile::DLog),
            ("log3g10", Profile::Log3G10),
            ("applelog", Profile::AppleLog),
        ];
        TOKENS.iter().find(|(t, _)| runs.iter().any(|r| r == t)).map_or(Profile::Standard, |(_, p)| *p)
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
        MATRICES[self as usize].0
    }

    /// Row-major linear Rec.709 → camera-gamut matrix.
    pub fn from_rec709(self) -> [[f32; 3]; 3] {
        MATRICES[self as usize].1
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
                let (s, t) = logc4_toe();
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

impl Profile {
    /// Inverse of [`Profile::encode`]: code value → scene-linear reflectance.
    /// Mirrors `decode()` in `grade.wgsl` (tests keep them in sync).
    pub fn decode(self, v: f32) -> f32 {
        let v = v as f64;
        let pow10 = |x: f64| 10f64.powf(x);
        let l = match self {
            Profile::Standard => v,
            Profile::VLog => {
                if v < 0.181 {
                    (v - 0.125) / 5.6
                } else {
                    pow10((v - 0.598206) / 0.241514) - 0.00873
                }
            }
            Profile::SLog3Cine | Profile::SLog3 => {
                let cv = v * 1023.0;
                if cv >= 171.2102946929 {
                    pow10((cv - 420.0) / 261.5) * 0.19 - 0.01
                } else {
                    (cv - 95.0) * 0.01125 / (171.2102946929 - 95.0)
                }
            }
            Profile::CLog3 => {
                let c = (v * 1023.0 - 64.0) / 876.0;
                let x = if c < 0.097465473 {
                    -(pow10((0.12783901 - c) / 0.36726845) - 1.0) / 14.98325
                } else if c <= 0.15277891 {
                    (c - 0.12512219) / 1.9754798
                } else {
                    (pow10((c - 0.12240537) / 0.36726845) - 1.0) / 14.98325
                };
                x * 0.9
            }
            Profile::FLog => {
                if v >= 0.100537775223865 {
                    (pow10((v - 0.790453) / 0.344676) - 0.009468) / 0.555556
                } else {
                    (v - 0.092864) / 8.735631
                }
            }
            Profile::FLog2 => {
                if v >= 0.100686685370811 {
                    (pow10((v - 0.384316) / 0.245281) - 0.064829) / 5.555556
                } else {
                    (v - 0.092864) / 8.799461
                }
            }
            Profile::NLog => {
                let cv = v * 1023.0;
                if cv >= 452.0 { ((cv - 619.0) / 150.0).exp() } else { (cv / 650.0).powi(3) - 0.0075 }
            }
            Profile::LogC3 => {
                if v > 0.149658 {
                    (pow10((v - 0.385537) / 0.247190) - 0.052272) / 5.555556
                } else {
                    (v - 0.092809) / 5.367655
                }
            }
            Profile::LogC4 => {
                let (a, b, c) = LOGC4;
                let (s, t) = logc4_toe();
                if v >= 0.0 { (2f64.powf(14.0 * (v - c) / b + 6.0) - 64.0) / a } else { v * s + t }
            }
            Profile::BmdFilmGen5 => {
                if v >= 8.283605932 * 0.005 + 0.09246575342 {
                    ((v - 0.5300133392) / 0.08692876065).exp() - 0.005494072432
                } else {
                    (v - 0.09246575342) / 8.283605932
                }
            }
            Profile::DLog => {
                if v > 0.14 {
                    (pow10(3.89616 * v - 2.27752) - 0.0108) / 0.9892
                } else {
                    (v - 0.0929) / 6.025
                }
            }
            Profile::Log3G10 => (if v >= 0.0 { (pow10(v / 0.224282) - 1.0) / 155.975327 } else { v / 15.1927 }) - 0.01,
            Profile::AppleLog => {
                let (r0, rt, c, b, g, beta) = APPLE_LOG;
                if v >= c * (rt - r0) * (rt - r0) {
                    2f64.powf((v - beta) / g) - b
                } else if v >= 0.0 {
                    (v / c).sqrt() + r0
                } else {
                    r0
                }
            }
        };
        l as f32
    }
}

/// (to Rec.709, from Rec.709) per profile, indexed by `Profile as usize`
/// (declaration order = `Profile::ALL`). Derived once, in f64, from primaries.
type M3f = [[f32; 3]; 3];
static MATRICES: std::sync::LazyLock<[(M3f, M3f); 14]> = std::sync::LazyLock::new(|| {
    Profile::ALL.map(|p| {
        let (prim, w) = p.gamut();
        let (cam, rec) = (npm(prim, w), npm(REC709, D65));
        let f32m = |m: M3| m.map(|r| r.map(|v| v as f32));
        (f32m(mul(&inverse(&rec), &cam)), f32m(mul(&inverse(&cam), &rec)))
    })
});

/// LogC4 linear-toe slope and offset (s, t) from the ARRI spec.
fn logc4_toe() -> (f64, f64) {
    let (a, b, c) = LOGC4;
    let s = (7.0 * std::f64::consts::LN_2 * 2f64.powf(7.0 - 14.0 * c / b)) / (a * b);
    let t = (2f64.powf(14.0 * (-c / b) + 6.0) - 64.0) / a;
    (s, t)
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
    fn guesses_profile_from_vendor_file_names() {
        let cases = [
            ("XH2S_FLog_FGamut_to_ETERNA_BT.709_33grid_V.1.00", Profile::FLog),
            ("X-T5_F-Log2_F-Gamut_to_WDR_BT.709", Profile::FLog2),
            ("VLog_to_V709_forV35_ver100", Profile::VLog),
            ("SLog3SGamut3.CineToLC-709", Profile::SLog3Cine),
            ("ARRI_LogC4_to_Rec709", Profile::LogC4),
            ("Kodak 2383", Profile::Standard),
            ("Kodak Analog Classic", Profile::Standard),
            ("Teal&Orange_Vlogger", Profile::Standard),
            ("Apple Log to Rec709", Profile::AppleLog),
            ("Log3G10_RWG_to_709", Profile::Log3G10),
        ];
        for (name, want) in cases {
            assert_eq!(Profile::guess_from_name(name), want, "{name}");
        }
    }

    #[test]
    fn decode_inverts_encode() {
        for p in Profile::ALL {
            for l in [-0.005f32, 0.0, 0.003, 0.02, 0.18, 0.9, 4.0, 20.0] {
                if p == Profile::AppleLog && l < -0.05 {
                    continue;
                }
                let back = p.decode(p.encode(l));
                assert!((back - l).abs() < 1e-3 * l.abs().max(1.0), "{p:?} {l}: {back}");
            }
        }
    }

    #[test]
    fn matrix_table_matches_declaration_order() {
        for (i, p) in Profile::ALL.into_iter().enumerate() {
            assert_eq!(p as usize, i, "MATRICES is indexed by declaration order");
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
