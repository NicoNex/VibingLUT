//! Adobe/Resolve `.cube` 3D LUT reading and writing.

use std::fmt::Write as _;
use std::path::Path;

/// A 3D LUT, red varying fastest (the `.cube` order).
#[derive(Clone, Debug)]
pub struct Lut3d {
    pub title: String,
    pub size: usize,
    pub data: Vec<[f32; 3]>,
}

impl Lut3d {
    pub fn to_cube(&self) -> String {
        let mut s = String::with_capacity(self.data.len() * 28 + 128);
        let _ = writeln!(s, "# Created with VibingLUT");
        let _ = writeln!(s, "TITLE \"{}\"", self.title.replace('"', "'"));
        let _ = writeln!(s, "LUT_3D_SIZE {}", self.size);
        let _ = writeln!(s, "DOMAIN_MIN 0.0 0.0 0.0");
        let _ = writeln!(s, "DOMAIN_MAX 1.0 1.0 1.0");
        for [r, g, b] in &self.data {
            let _ = writeln!(s, "{:.6} {:.6} {:.6}", r.clamp(0.0, 1.0), g.clamp(0.0, 1.0), b.clamp(0.0, 1.0));
        }
        s
    }

    pub fn parse(text: &str) -> Result<Self, String> {
        let (mut title, mut size, mut data) = (String::new(), 0usize, Vec::new());
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut it = line.split_whitespace();
            match it.next() {
                Some("TITLE") => title = line[5..].trim().trim_matches('"').to_string(),
                Some("LUT_3D_SIZE") => size = it.next().and_then(|v| v.parse().ok()).ok_or("bad LUT_3D_SIZE")?,
                Some("LUT_1D_SIZE") => return Err("1D LUTs are not supported".into()),
                Some("DOMAIN_MIN") | Some("DOMAIN_MAX") => {
                    // ponytail: only the standard 0..1 domain; rescale here if a shaper-domain LUT ever shows up.
                    let want = if line.starts_with("DOMAIN_MIN") { 0.0 } else { 1.0 };
                    if it.any(|v| v.parse::<f32>().map_or(true, |v| (v - want).abs() > 1e-6)) {
                        return Err("non-standard DOMAIN is not supported".into());
                    }
                }
                Some(first) if first.starts_with(|c: char| c.is_ascii_digit() || c == '-' || c == '.') => {
                    let v: Vec<f32> = line.split_whitespace().filter_map(|v| v.parse().ok()).collect();
                    if v.len() != 3 {
                        return Err(format!("bad data line: {line}"));
                    }
                    data.push([v[0], v[1], v[2]]);
                }
                _ => {} // unknown keywords (LUT_IN_VIDEO_RANGE etc.) are ignored
            }
        }
        if size < 2 || data.len() != size * size * size {
            return Err(format!("expected {}³ entries, found {}", size, data.len()));
        }
        Ok(Self { title, size, data })
    }

    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
        let mut lut = Self::parse(&text)?;
        if lut.title.is_empty() {
            lut.title = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        }
        Ok(lut)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cube_round_trip() {
        let n = 3;
        let data = (0..n * n * n)
            .map(|i| [(i % n) as f32 / 2.0, (i / n % n) as f32 / 2.0, (i / (n * n)) as f32 / 2.0])
            .collect();
        let lut = Lut3d { title: "id".into(), size: n, data };
        let back = Lut3d::parse(&lut.to_cube()).unwrap();
        assert_eq!(back.size, 3);
        assert_eq!(back.title, "id");
        assert_eq!(back.data[1], [0.5, 0.0, 0.0]); // red fastest
        assert_eq!(back.data[9], [0.0, 0.0, 0.5]);
        assert!(Lut3d::parse("LUT_3D_SIZE 2\n0 0 0\n").is_err());
    }
}
