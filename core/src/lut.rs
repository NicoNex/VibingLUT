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
                Some(k @ ("DOMAIN_MIN" | "DOMAIN_MAX")) => {
                    // ponytail: only the standard 0..1 domain; rescale here if a shaper-domain LUT ever shows up.
                    let want = if k == "DOMAIN_MIN" { 0.0 } else { 1.0 };
                    if it.any(|v| v.parse::<f32>().map_or(true, |v| (v - want).abs() > 1e-6)) {
                        return Err("non-standard DOMAIN is not supported".into());
                    }
                }
                Some(first) if first.starts_with(|c: char| c.is_ascii_digit() || c == '-' || c == '.') => {
                    let v: Vec<f32> = std::iter::once(first).chain(it).filter_map(|v| v.parse().ok()).collect();
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
        // TITLE lines are wildly inconsistent in the wild; the file name is what users recognise.
        lut.title = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or(lut.title);
        Ok(lut)
    }

    /// Every `.cube` under `dir`, recursively, paired with its sub-folder
    /// relative to `dir` ("" for the top level), sorted by folder then name;
    /// plus one message per file that failed to parse.
    pub fn load_dir(dir: &Path) -> (Vec<(String, Self)>, Vec<String>) {
        let mut paths = Vec::new();
        collect_cubes(dir, &mut paths);
        let group = |p: &Path| {
            p.parent()
                .and_then(|d| d.strip_prefix(dir).ok())
                .map(|d| d.to_string_lossy().into_owned())
                .unwrap_or_default()
        };
        paths.sort_by_cached_key(|p| (group(p), p.file_name().map(|n| n.to_ascii_lowercase())));
        let (mut luts, mut errors) = (Vec::new(), Vec::new());
        for p in paths {
            match Self::load(&p) {
                Ok(l) => luts.push((group(&p), l)),
                Err(e) => errors.push(format!("{}: {e}", p.strip_prefix(dir).unwrap_or(&p).display())),
            }
        }
        (luts, errors)
    }
}

/// Recursive `.cube` search. Hidden entries are skipped and symlinked
/// directories are not followed (`file_type` doesn't resolve links), so
/// link cycles can't loop forever.
fn collect_cubes(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let (path, Ok(ft)) = (e.path(), e.file_type()) else { continue };
        if e.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        if ft.is_dir() {
            collect_cubes(&path, out);
        } else if path.extension().is_some_and(|x| x.eq_ignore_ascii_case("cube")) {
            out.push(path);
        }
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

    #[test]
    fn load_dir_recurses_into_subfolders() {
        let root = std::env::temp_dir().join(format!("vibinglut-test-{}", std::process::id()));
        let cube = Lut3d { title: String::new(), size: 2, data: vec![[0.0; 3]; 8] }.to_cube();
        for rel in ["b.cube", "Film/Kodak/a.CUBE", "Film/z.cube", ".hidden/x.cube"] {
            let p = root.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, &cube).unwrap();
        }
        std::fs::write(root.join("Film/bad.cube"), "LUT_3D_SIZE 2\n").unwrap();
        let (luts, errors) = Lut3d::load_dir(&root);
        std::fs::remove_dir_all(&root).unwrap();
        let got: Vec<_> = luts.iter().map(|(g, l)| format!("{g}/{}", l.title)).collect();
        let kodak = format!("Film{}Kodak/a", std::path::MAIN_SEPARATOR);
        assert_eq!(got, ["/b", "Film/z", kodak.as_str()]);
        assert_eq!(errors.len(), 1);
    }
}
