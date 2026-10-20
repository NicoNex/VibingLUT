//! Photo loading: JPEG/PNG/TIFF via `image`, camera RAW via `rawler`.
//! Output is always a preview-sized, display-encoded (sRGB/Rec.709) image,
//! i.e. the same signal a camera "Standard" profile feeds into a LUT.

use std::path::Path;

use image::metadata::Orientation;
use image::{DynamicImage, ImageDecoder, ImageReader, Rgba32FImage};
use rawler::imgop::develop::{Intermediate, ProcessingStep, RawDevelop};
use rawler::imgop::srgb::srgb_apply_gamma;

use crate::grade::Profile;

pub const PREVIEW_MAX: u32 = 2048;
pub const THUMB_MAX: u32 = 384;

pub const RASTER_EXTS: &[&str] = &["jpg", "jpeg", "png", "tif", "tiff"];
pub const RAW_EXTS: &[&str] = &[
    "rw2", "cr2", "cr3", "nef", "nrw", "arw", "srf", "sr2", "raf", "orf", "dng", "pef", "srw", "iiq", "3fr", "erf",
    "kdc", "mrw", "rwl",
];

#[derive(Clone)]
pub struct Photo {
    pub width: u32,
    pub height: u32,
    /// Display-encoded RGBA in 0..1.
    pub pixels: Vec<[f32; 4]>,
}

impl std::fmt::Debug for Photo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Photo({}×{})", self.width, self.height)
    }
}

impl Photo {
    /// Loads `path` as the signal `profile` describes. RAW files are rendered
    /// into that signal; JPEG/PNG/TIFF are taken as already encoded in it
    /// (e.g. a JPEG shot in V-Log).
    pub fn load(path: &Path, profile: Profile) -> Result<Self, String> {
        let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
        if RAW_EXTS.contains(&ext.as_str()) { load_raw(path, profile) } else { load_raster(path) }
    }

    /// Box-filtered copy that fits in `max` × `max`. Averages the stored
    /// values directly, so linear RAW data is downscaled in linear light.
    // ponytail: not image::imageops::thumbnail — it washes out f32 pixels.
    pub fn downscaled(&self, max: u32) -> Self {
        let f = self.width.max(self.height).div_ceil(max).max(1);
        if f == 1 {
            return self.clone();
        }
        let (w, h) = ((self.width / f).max(1), (self.height / f).max(1));
        let norm = 1.0 / (f * f) as f32;
        let mut pixels = Vec::with_capacity((w * h) as usize);
        for y in 0..h {
            for x in 0..w {
                let mut acc = [0.0f32; 4];
                for dy in 0..f.min(self.height - y * f) {
                    let row = ((y * f + dy) * self.width + x * f) as usize;
                    for p in &self.pixels[row..row + f.min(self.width - x * f) as usize] {
                        for c in 0..4 {
                            acc[c] += p[c];
                        }
                    }
                }
                pixels.push(acc.map(|v| v * norm));
            }
        }
        Self { width: w, height: h, pixels }
    }

    fn to_image(&self) -> Rgba32FImage {
        Rgba32FImage::from_raw(self.width, self.height, bytemuck::cast_slice(&self.pixels).to_vec())
            .expect("pixel count matches dimensions")
    }

    fn from_image(img: Rgba32FImage) -> Self {
        Self { width: img.width(), height: img.height(), pixels: bytemuck::cast_slice(img.as_raw()).to_vec() }
    }
}

fn load_raster(path: &Path) -> Result<Photo, String> {
    let mut dec = ImageReader::open(path)
        .map_err(|e| e.to_string())?
        .with_guessed_format()
        .map_err(|e| e.to_string())?
        .into_decoder()
        .map_err(|e| e.to_string())?;
    let orientation = dec.orientation().map_err(|e| e.to_string())?;
    let mut img = DynamicImage::from_decoder(dec).map_err(|e| e.to_string())?;
    img.apply_orientation(orientation);
    let img = img.thumbnail(PREVIEW_MAX, PREVIEW_MAX).into_rgba32f();
    let (width, height) = img.dimensions();
    let pixels = img.pixels().map(|p| [p[0], p[1], p[2], 1.0]).collect();
    Ok(Photo { width, height, pixels })
}

fn load_raw(path: &Path, profile: Profile) -> Result<Photo, String> {
    let raw = rawler::decode_file(path).map_err(|e| e.to_string())?;
    // Everything but the final sRGB encode: we want linear camera-calibrated RGB.
    let dev = RawDevelop::new_with(&[
        ProcessingStep::Rescale,
        ProcessingStep::Demosaic,
        ProcessingStep::FujiRotate,
        ProcessingStep::CropActiveArea,
        ProcessingStep::WhiteBalance,
        ProcessingStep::Calibrate,
        ProcessingStep::CropDefault,
    ]);
    let Intermediate::ThreeColor(rgb) = dev.develop_intermediate(&raw).map_err(|e| e.to_string())? else {
        return Err("unsupported sensor layout".into());
    };
    let (w, h) = (rgb.dim().w as u32, rgb.dim().h as u32);
    let linear = Photo { width: w, height: h, pixels: rgb.data.iter().map(|p| [p[0], p[1], p[2], 1.0]).collect() };
    // Downscale in linear light (physically correct averaging), then encode.
    let orientation = Orientation::from_exif(raw.orientation.to_u16() as u8).unwrap_or(Orientation::NoTransforms);
    let mut img = DynamicImage::ImageRgba32F(linear.downscaled(PREVIEW_MAX).to_image());
    img.apply_orientation(orientation);
    let mut photo = Photo::from_image(img.into_rgba32f());
    for p in &mut photo.pixels {
        let scene = [p[0], p[1], p[2]].map(|v| v * RAW_TO_SCENE);
        let [r, g, b] = match profile {
            Profile::Standard => view_transform(scene),
            Profile::VLog => mat3(&REC709_TO_VGAMUT, scene).map(vlog_encode),
        };
        *p = [r, g, b, 1.0];
    }
    Ok(photo)
}

/// Linear Rec.709 → V-Gamut (Panasonic V-Gamut primaries, D65).
const REC709_TO_VGAMUT: [[f32; 3]; 3] =
    [[0.585197, 0.322642, 0.092162], [0.078588, 0.819627, 0.101784], [0.022794, 0.114217, 0.862989]];

fn mat3(m: &[[f32; 3]; 3], c: [f32; 3]) -> [f32; 3] {
    m.map(|row| row[0] * c[0] + row[1] * c[1] + row[2] * c[2])
}

/// Panasonic V-Log OETF (scene linear reflection → code value).
pub fn vlog_encode(l: f32) -> f32 {
    if l < 0.01 { 5.6 * l + 0.125 } else { 0.241514 * (l + 0.00873).log10() + 0.598206 }
}

/// Scene-linear scale of rawler output: Lumix RAWs sit ~0.8 EV under scene grey.
const RAW_TO_SCENE: f32 = 1.76;
const VIEW_EXPOSURE: f32 = 1.25;
const VIEW_WHITE: f32 = 6.0; // exposed scene value mapped to display white

/// Neutral scene → display rendering (linear Rec.709 in, sRGB out):
/// hue-preserving extended Reinhard shoulder. `grade.wgsl` mirrors it for
/// V-Log input, so RAW and V-Log start from the same look.
// ponytail: approximates, does not replicate, the camera's Standard profile.
// For a pixel-exact LUT match shoot RAW+JPEG and grade the JPEG.
pub fn view_transform(lin: [f32; 3]) -> [f32; 3] {
    let c = lin.map(|v| v.max(0.0) * VIEW_EXPOSURE);
    let m = c[0].max(c[1]).max(c[2]);
    let k = if m > 0.0 { (1.0 + m / (VIEW_WHITE * VIEW_WHITE)) / (1.0 + m) } else { 0.0 };
    c.map(|v| srgb_apply_gamma((v * k).min(1.0)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vlog_matches_panasonic_reference() {
        // Panasonic: 18% grey → 42% code value, 0% → 7.3% (10-bit 128/1023 ≈ 0.125).
        assert!((vlog_encode(0.18) - 0.4233).abs() < 1e-3);
        assert!((vlog_encode(0.0) - 0.125).abs() < 1e-6);
        // Continuous at the linear/log breakpoint.
        assert!((vlog_encode(0.00999) - vlog_encode(0.01)).abs() < 1e-3);
    }
}
