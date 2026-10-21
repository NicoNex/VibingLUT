//! Grade presets as readable JSON.
//!
//! ```json
//! { "format": "vibinglut-preset", "version": 1, "name": "Teal & orange",
//!   "params": { "exposure": 0.3, "lift": [0.1, -0.2, 0.0, 0.0], … },
//!   "curves": { "master": [[0.0, 0.0], [1.0, 1.0]], … } }
//! ```
//! Fields are named, and missing ones load as neutral, so presets written by
//! older versions keep working as the grade grows.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::grade::{CURVES, Curve, Grade, Params, is_periodic, neutral_curve};

const FORMAT: &str = "vibinglut-preset";
const VERSION: u32 = 1;
pub const EXT: &str = "json";

#[derive(Serialize, Deserialize)]
struct File {
    format: String,
    version: u32,
    name: String,
    #[serde(default)]
    params: Params,
    #[serde(default)]
    curves: Curves,
}

#[derive(Serialize, Deserialize)]
#[serde(default)]
struct Curves {
    master: Curve,
    red: Curve,
    green: Curve,
    blue: Curve,
    hue_vs_hue: Curve,
    hue_vs_sat: Curve,
    lum_vs_sat: Curve,
    match_lightness: Curve,
}

impl Default for Curves {
    fn default() -> Self {
        Self::from(std::array::from_fn(neutral_curve))
    }
}

impl From<[Curve; CURVES]> for Curves {
    fn from([master, red, green, blue, hue_vs_hue, hue_vs_sat, lum_vs_sat, match_lightness]: [Curve; CURVES]) -> Self {
        Self { master, red, green, blue, hue_vs_hue, hue_vs_sat, lum_vs_sat, match_lightness }
    }
}

impl From<Curves> for [Curve; CURVES] {
    fn from(c: Curves) -> Self {
        [c.master, c.red, c.green, c.blue, c.hue_vs_hue, c.hue_vs_sat, c.lum_vs_sat, c.match_lightness]
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Preset {
    pub name: String,
    pub grade: Grade,
}

impl Preset {
    pub fn to_json(&self) -> String {
        let file = File {
            format: FORMAT.into(),
            version: VERSION,
            name: self.name.clone(),
            params: self.grade.params,
            curves: Curves::from(self.grade.curves.clone()),
        };
        serde_json::to_string_pretty(&file).expect("presets always serialise")
    }

    /// Parses and validates a preset: a file from elsewhere must not be able
    /// to feed NaN or broken curves into the grade.
    pub fn from_json(text: &str) -> Result<Self, String> {
        let file: File = serde_json::from_str(text).map_err(|e| e.to_string())?;
        if file.format != FORMAT {
            return Err(format!("not a VibingLUT preset ({})", file.format));
        }
        if file.version > VERSION {
            return Err(format!("made by a newer VibingLUT (preset version {})", file.version));
        }
        let floats: &[f32] = bytemuck::cast_slice(std::slice::from_ref(&file.params));
        if floats.iter().any(|v| !v.is_finite()) {
            return Err("invalid number in params".into());
        }
        let mut curves: [Curve; CURVES] = file.curves.into();
        for (ch, c) in curves.iter_mut().enumerate() {
            validate_curve(c).map_err(|e| format!("curve {ch}: {e}"))?;
            if is_periodic(ch) {
                c.last_mut().expect("validated").1 = c[0].1; // red is one hue at both ends
            }
        }
        // Only the grading fields come from the file; input and preview state stay neutral.
        let mut params = Params::default();
        let p = file.params;
        (params.temperature, params.tint, params.exposure, params.contrast) =
            (p.temperature, p.tint, p.exposure, p.contrast);
        (params.highlights, params.shadows, params.whites, params.blacks) =
            (p.highlights, p.shadows, p.whites, p.blacks);
        (params.saturation, params.vibrance, params.sat_shadows, params.sat_highlights) =
            (p.saturation, p.vibrance, p.sat_shadows, p.sat_highlights);
        (params.lift, params.gamma, params.gain, params.offset) = (p.lift, p.gamma, p.gain, p.offset);
        (params.hue_shift, params.hue_sat, params.hue_lum) = (p.hue_shift, p.hue_sat, p.hue_lum);
        (params.mixer, params.mixer_mono, params.mono_mix) =
            (p.mixer, if p.mixer_mono > 0.5 { 1.0 } else { 0.0 }, p.mono_mix);
        (params.film_sat, params.film_density) = (p.film_sat, p.film_density);
        (params.match_amount, params.match_ab) = (p.match_amount, p.match_ab);
        Ok(Self { name: file.name, grade: Grade { params, curves } })
    }

    pub fn load(path: &Path) -> Result<Self, String> {
        Self::from_json(&std::fs::read_to_string(path).map_err(|e| e.to_string())?)
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        std::fs::write(path, self.to_json()).map_err(|e| e.to_string())
    }
}

fn validate_curve(c: &Curve) -> Result<(), &'static str> {
    if c.len() < 2 || c.len() > 64 {
        return Err("needs 2 to 64 points");
    }
    if c.iter().any(|&(x, y)| !(0.0..=1.0).contains(&x) || !(0.0..=1.0).contains(&y)) {
        return Err("points must lie in 0..1");
    }
    if c[0].0 != 0.0 || c[c.len() - 1].0 != 1.0 {
        return Err("must start at x = 0 and end at x = 1");
    }
    if c.windows(2).any(|w| w[1].0 <= w[0].0) {
        return Err("x must increase");
    }
    Ok(())
}

/// File name for a preset called `name` (path separators and the like removed).
pub fn file_name(name: &str) -> String {
    let stem: String =
        name.trim().chars().map(|c| if c.is_alphanumeric() || " -_&()".contains(c) { c } else { '_' }).collect();
    format!("{}.{EXT}", if stem.is_empty() { "preset" } else { stem.trim() })
}

/// Presets in `dir`, sorted by name; unreadable files are skipped.
pub fn list(dir: &Path) -> Vec<(Preset, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut out: Vec<(Preset, PathBuf)> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == EXT))
        .filter_map(|p| Some((Preset::load(&p).ok()?, p)))
        .collect();
    out.sort_by_key(|(p, _)| p.name.to_lowercase());
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grade::{HUE_HUE, MATCH_L};

    fn graded() -> Grade {
        let mut g = Grade::default();
        let p = &mut g.params;
        (p.exposure, p.lift, p.hue_sat[2], p.mixer[4], p.film_density) =
            (0.3, [0.1, -0.2, 5.0, 0.0], 12.0, -20.0, 40.0);
        (p.match_amount, p.match_ab) = (75.0, [0.1, 0.0, 0.02, -0.1, 0.003, -0.01]);
        g.curves[1] = vec![(0.0, 0.05), (0.5, 0.6), (1.0, 1.0)];
        g.curves[HUE_HUE] = vec![(0.0, 0.55), (0.4, 0.3), (1.0, 0.55)];
        g.curves[MATCH_L] = vec![(0.0, 0.02), (0.5, 0.45), (1.0, 0.98)];
        g
    }

    #[test]
    fn round_trip() {
        let p = Preset { name: "Teal & orange".into(), grade: graded() };
        assert_eq!(Preset::from_json(&p.to_json()).unwrap(), p);
    }

    /// Missing fields are neutral; preview-only state never comes from a file.
    #[test]
    fn partial_and_hostile_files() {
        let p = Preset::from_json(r#"{"format":"vibinglut-preset","version":1,"name":"x","params":{"exposure":0.5,"width":9999,"overlay":1}}"#).unwrap();
        let mut want = Grade::default();
        want.params.exposure = 0.5;
        assert_eq!(p.grade, want);
        for bad in [
            r#"{"format":"other","version":1,"name":"x"}"#,
            r#"{"format":"vibinglut-preset","version":99,"name":"x"}"#,
            r#"{"format":"vibinglut-preset","version":1,"name":"x","curves":{"red":[[0.0,0.0],[0.5,0.2],[0.4,0.3],[1.0,1.0]]}}"#,
            r#"{"format":"vibinglut-preset","version":1,"name":"x","curves":{"blue":[[0.0,2.0],[1.0,1.0]]}}"#,
            r#"{"format":"vibinglut-preset","version":1,"name":"x","params":{"exposure":1e999}}"#,
        ] {
            assert!(Preset::from_json(bad).is_err(), "accepted {bad}");
        }
    }

    #[test]
    fn file_names_are_safe() {
        assert_eq!(file_name("Teal & orange"), "Teal & orange.json");
        assert_eq!(file_name("../../etc/passwd"), "______etc_passwd.json");
        assert_eq!(file_name("  "), "preset.json");
    }
}
