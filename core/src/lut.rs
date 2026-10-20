//! 3D LUT files: Adobe/Resolve `.cube`, Panasonic `.vlt` and HALD CLUT `.png`.
//! All three store the lattice with red varying fastest, so one in-memory
//! layout serves them all.

use std::fmt::Write as _;
use std::path::Path;

use image::{ImageBuffer, Rgb};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Format {
    #[default]
    Cube,
    /// Panasonic VariCam/Lumix 3DLUT (VARICAM 3DLUT Reference Manual rev 1.0).
    Vlt,
    /// HALD CLUT image (level L: an L³×L³ PNG holding an L²-point cube).
    Hald,
}

impl Format {
    pub fn from_path(p: &Path) -> Option<Self> {
        match p.extension()?.to_string_lossy().to_ascii_lowercase().as_str() {
            "cube" => Some(Format::Cube),
            "vlt" => Some(Format::Vlt),
            "png" => Some(Format::Hald),
            _ => None,
        }
    }

    pub fn ext(self) -> &'static str {
        match self {
            Format::Cube => "cube",
            Format::Vlt => "vlt",
            Format::Hald => "png",
        }
    }

    /// Short label for UI badges.
    pub fn badge(self) -> &'static str {
        match self {
            Format::Cube => "CUBE",
            Format::Vlt => "VLT",
            Format::Hald => "HALD",
        }
    }
}

/// VLT files are always 17³ with 12-bit integer outputs.
pub const VLT_SIZE: usize = 17;
const VLT_MAX: f32 = 4095.0;

/// A 3D LUT, red varying fastest.
#[derive(Clone, Debug)]
pub struct Lut3d {
    pub title: String,
    pub size: usize,
    pub data: Vec<[f32; 3]>,
    pub format: Format,
}

/// HALD level L for an image side, if the side is L³ (L in 2..=16).
fn hald_level(side: u32) -> Option<u32> {
    (2..=16u32).find(|l| l * l * l == side)
}

impl Lut3d {
    /// The neutral N³ lattice.
    pub fn identity(size: usize, format: Format) -> Self {
        let s = (size - 1) as f32;
        let data = (0..size * size * size)
            .map(|i| [(i % size) as f32 / s, (i / size % size) as f32 / s, (i / (size * size)) as f32 / s])
            .collect();
        Self { title: "identity".into(), size, data, format }
    }

    /// Tetrahedral interpolation at `c` (0..1 per channel).
    pub fn sample(&self, c: [f32; 3]) -> [f32; 3] {
        let n = self.size;
        let s = (n - 1) as f32;
        let x = c.map(|v| v.clamp(0.0, 1.0) * s);
        let i = x.map(|v| (v as usize).min(n - 2));
        let f: [f32; 3] = std::array::from_fn(|k| x[k] - i[k] as f32);
        let at = |dr: usize, dg: usize, db: usize| self.data[(i[0] + dr) + n * ((i[1] + dg) + n * (i[2] + db))];
        // Walk the cube diagonal through the corners of the tetrahedron that holds `f`.
        let mut order = [0usize, 1, 2];
        order.sort_by(|&a, &b| f[b].total_cmp(&f[a]));
        let mut d = [0usize; 3];
        let mut prev = at(0, 0, 0);
        let mut out = prev;
        for &axis in &order {
            d[axis] = 1;
            let next = at(d[0], d[1], d[2]);
            for k in 0..3 {
                out[k] += f[axis] * (next[k] - prev[k]);
            }
            prev = next;
        }
        out
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        match self.format {
            Format::Cube => std::fs::write(path, self.to_cube()).map_err(|e| e.to_string()),
            Format::Vlt => std::fs::write(path, self.to_vlt()?).map_err(|e| e.to_string()),
            Format::Hald => self.to_hald()?.save(path).map_err(|e| e.to_string()),
        }
    }

    /// Panasonic `.vlt`: 4-line header, then integer R G B per lattice point.
    // ponytail: the spec samples inputs at 0,256,…,3840,4095; we bake a uniform
    // 17³ lattice, off by < 0.02% of full scale per step.
    pub fn to_vlt(&self) -> Result<String, String> {
        if self.size != VLT_SIZE {
            return Err(format!("VLT requires {VLT_SIZE}³, got {}³", self.size));
        }
        let mut s = String::from("# panasonic vlt file version 1.0\n# source vlt file \"\"\nLUT_3D_SIZE 17\n\n");
        for c in &self.data {
            let [r, g, b] = c.map(|v| (v.clamp(0.0, 1.0) * VLT_MAX).round() as u32);
            let _ = writeln!(s, "{r} {g} {b}");
        }
        Ok(s)
    }

    fn parse_vlt(text: &str) -> Result<Self, String> {
        let mut size = 0usize;
        let mut data = Vec::new();
        for line in text.lines().map(str::trim) {
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some(v) = line.strip_prefix("LUT_3D_SIZE") {
                size = v.trim().parse().map_err(|_| "bad LUT_3D_SIZE")?;
                continue;
            }
            let v: Vec<f32> = line.split_whitespace().filter_map(|v| v.parse().ok()).collect();
            if v.len() != 3 {
                return Err(format!("bad data line: {line}"));
            }
            data.push([v[0] / VLT_MAX, v[1] / VLT_MAX, v[2] / VLT_MAX]);
        }
        if size < 2 || data.len() != size * size * size {
            return Err(format!("expected {size}³ entries, found {}", data.len()));
        }
        Ok(Self { title: String::new(), size, data, format: Format::Vlt })
    }

    /// 16-bit HALD CLUT image; requires size = L² (e.g. 64 → level 8, 512×512).
    pub fn to_hald(&self) -> Result<ImageBuffer<Rgb<u16>, Vec<u16>>, String> {
        let level = (2..=16).find(|l| l * l == self.size).ok_or(format!("{}³ is not a HALD size (L²)", self.size))?;
        let side = (level * level * level) as u32;
        let px: Vec<u16> =
            self.data.iter().flat_map(|c| c.map(|v| (v.clamp(0.0, 1.0) * 65535.0).round() as u16)).collect();
        ImageBuffer::from_raw(side, side, px).ok_or_else(|| "HALD buffer size mismatch".into())
    }

    fn read_hald(path: &Path) -> Result<Self, String> {
        let img = image::open(path).map_err(|e| e.to_string())?.into_rgb32f();
        let level = (img.width() == img.height()).then(|| hald_level(img.width())).flatten();
        let level = level.ok_or("not a HALD CLUT image (side must be L³)")? as usize;
        let data = img.pixels().map(|p| [p[0], p[1], p[2]]).collect();
        Ok(Self { title: String::new(), size: level * level, data, format: Format::Hald })
    }

    /// True for PNGs shaped like a HALD CLUT (reads only the header), so
    /// preview images living next to LUTs are skipped instead of reported.
    fn is_hald(path: &Path) -> bool {
        image::image_dimensions(path).is_ok_and(|(w, h)| w == h && hald_level(w).is_some())
    }

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
        Ok(Self { title, size, data, format: Format::Cube })
    }

    pub fn load(path: &Path) -> Result<Self, String> {
        let text = || std::fs::read_to_string(path).map_err(|e| e.to_string());
        let mut lut = match Format::from_path(path) {
            Some(Format::Vlt) => Self::parse_vlt(&text()?)?,
            Some(Format::Hald) => Self::read_hald(path)?,
            _ => Self::parse(&text()?)?,
        };
        // TITLE lines are wildly inconsistent in the wild; the file name is what users recognise.
        lut.title = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or(lut.title);
        Ok(lut)
    }

    /// Every LUT (`.cube`, `.vlt`, HALD `.png`) under `dir`, recursively, paired with its sub-folder
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

/// Recursive LUT search. Hidden entries are skipped and symlinked
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
        } else if match Format::from_path(&path) {
            Some(Format::Hald) => Lut3d::is_hald(&path),
            Some(_) => true,
            None => false,
        } {
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
        let lut = Lut3d { title: "id".into(), size: n, data, format: Format::Cube };
        let back = Lut3d::parse(&lut.to_cube()).unwrap();
        assert_eq!(back.size, 3);
        assert_eq!(back.title, "id");
        assert_eq!(back.data[1], [0.5, 0.0, 0.0]); // red fastest
        assert_eq!(back.data[9], [0.0, 0.0, 0.5]);
        assert!(Lut3d::parse("LUT_3D_SIZE 2\n0 0 0\n").is_err());
    }

    #[test]
    fn tetrahedral_sample_is_exact_on_linear_luts() {
        // Tetrahedral interpolation reproduces any affine map exactly.
        let mut l = Lut3d::identity(5, Format::Cube);
        for c in &mut l.data {
            *c = [0.2 + 0.5 * c[1], c[0] * 0.8, 0.1 + 0.3 * c[2] + 0.4 * c[0]];
        }
        for c in [[0.13, 0.77, 0.4], [0.9, 0.05, 0.6], [1.0, 1.0, 1.0], [0.0, 0.5, 0.25]] {
            let s = l.sample(c);
            let want = [0.2 + 0.5 * c[1], c[0] * 0.8, 0.1 + 0.3 * c[2] + 0.4 * c[0]];
            assert!((0..3).all(|k| (s[k] - want[k]).abs() < 1e-5), "{c:?}: {s:?} vs {want:?}");
        }
    }

    #[test]
    fn vlt_and_hald_round_trip() {
        // VLT: header per the Panasonic manual, R fastest, 0..4095.
        let vlt = Lut3d::identity(VLT_SIZE, Format::Vlt).to_vlt().unwrap();
        let lines: Vec<&str> = vlt.lines().collect();
        assert_eq!(&lines[..4], ["# panasonic vlt file version 1.0", "# source vlt file \"\"", "LUT_3D_SIZE 17", ""]);
        assert_eq!(lines[5], "256 0 0");
        assert_eq!(lines[4 + 17], "0 256 0");
        assert_eq!(*lines.last().unwrap(), "4095 4095 4095");
        let back = Lut3d::parse_vlt(&vlt).unwrap();
        assert_eq!((back.size, back.data[1]), (17, [256.0 / 4095.0, 0.0, 0.0]));

        // HALD level 4: 16³ cube in a 64×64 PNG.
        let path = std::env::temp_dir().join(format!("vibinglut-hald-{}.png", std::process::id()));
        Lut3d::identity(16, Format::Hald).save(&path).unwrap();
        assert!(Lut3d::is_hald(&path));
        let back = Lut3d::load(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!((back.size, back.format), (16, Format::Hald));
        let id = Lut3d::identity(16, Format::Hald);
        assert!(back.data.iter().zip(&id.data).all(|(a, b)| (0..3).all(|k| (a[k] - b[k]).abs() < 1e-4)));
    }

    #[test]
    fn load_dir_recurses_into_subfolders() {
        let root = std::env::temp_dir().join(format!("vibinglut-test-{}", std::process::id()));
        let cube = Lut3d { title: String::new(), size: 2, data: vec![[0.0; 3]; 8], format: Format::Cube }.to_cube();
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
