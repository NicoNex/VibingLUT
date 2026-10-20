//! Photo loading: JPEG/PNG/TIFF via `image`, camera RAW via `rawler`.
//! Output is always a preview-sized, display-encoded (sRGB/Rec.709) image,
//! i.e. the same signal a camera "Standard" profile feeds into a LUT.

use std::path::Path;
use std::sync::Arc;

use image::metadata::Orientation;
use image::{DynamicImage, ImageDecoder, ImageReader, Rgba32FImage};
use rawler::imgop::develop::{Intermediate, ProcessingStep, RawDevelop};
pub use rawler::imgop::matrix::multiply_row1;
pub use rawler::imgop::srgb::{srgb_apply_gamma, srgb_invert_gamma};

use crate::profiles::Profile;

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

pub fn is_raw(path: &Path) -> bool {
    path.extension().is_some_and(|e| RAW_EXTS.contains(&e.to_string_lossy().to_lowercase().as_str()))
}

/// A decoded file, before it is expressed as a camera signal.
#[derive(Clone, Debug)]
pub enum Decoded {
    /// JPEG/PNG/TIFF: already a camera signal (Standard, or e.g. a V-Log JPEG).
    Raster(Arc<Photo>),
    /// RAW: scene-linear Rec.709, so a profile change only re-encodes.
    Raw(Arc<Photo>),
}

impl Decoded {
    pub fn load(path: &Path) -> Result<Self, String> {
        if is_raw(path) {
            load_raw(path).map(|p| Decoded::Raw(Arc::new(p)))
        } else {
            load_raster(path).map(|p| Decoded::Raster(Arc::new(p)))
        }
    }

    /// The pixels as `profile`'s signal: RAWs are rendered into it, rasters
    /// are taken as already encoded in it.
    pub fn encode(&self, profile: Profile) -> Arc<Photo> {
        match self {
            Decoded::Raster(p) => p.clone(),
            Decoded::Raw(scene) => {
                let from709 = profile.from_rec709();
                let pixels = scene
                    .pixels
                    .iter()
                    .map(|p| {
                        let s = [p[0], p[1], p[2]];
                        let [r, g, b] = if profile.is_log() {
                            multiply_row1(&from709, &s).map(|v| profile.encode(v))
                        } else {
                            view_transform(s)
                        };
                        [r, g, b, 1.0]
                    })
                    .collect();
                Arc::new(Photo { width: scene.width, height: scene.height, pixels })
            }
        }
    }
}

impl Photo {
    /// Loads `path` as the signal `profile` describes (see [`Decoded::encode`]).
    pub fn load(path: &Path, profile: Profile) -> Result<Arc<Self>, String> {
        Ok(Decoded::load(path)?.encode(profile))
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

/// Scene-linear Rec.709 preview of a RAW.
fn load_raw(path: &Path) -> Result<Photo, String> {
    let src = rawler::rawsource::RawSource::new(path).map_err(|e| e.to_string())?;
    let params = rawler::decoders::RawDecodeParams::default();
    let raw = rawler::decode(&src, &params).map_err(|e| e.to_string())?;
    // Decoders such as Panasonic's leave `raw.orientation` at Normal; the EXIF
    // Orientation tag is the reliable source (6/8 = portrait shots).
    let exif_orientation = rawler::get_decoder(&src)
        .and_then(|d| d.raw_metadata(&src, &params))
        .ok()
        .and_then(|m| m.exif.orientation)
        .unwrap_or_else(|| raw.orientation.to_u16());
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
    let orientation = Orientation::from_exif(exif_orientation as u8).unwrap_or(Orientation::NoTransforms);
    let mut img = DynamicImage::ImageRgba32F(linear.downscaled(PREVIEW_MAX).to_image());
    img.apply_orientation(orientation);
    let mut photo = Photo::from_image(img.into_rgba32f());
    for p in &mut photo.pixels {
        *p = [p[0] * RAW_TO_SCENE, p[1] * RAW_TO_SCENE, p[2] * RAW_TO_SCENE, 1.0];
    }
    Ok(photo)
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

/// Exact inverse of [`view_transform`] before its sRGB step: display-linear
/// Rec.709 in, scene-linear out (the shoulder's quadratic solved for m).
pub fn inverse_view_transform(display_linear: [f32; 3]) -> [f32; 3] {
    let y = display_linear.iter().copied().fold(0.0f32, f32::max).min(1.0);
    if y <= 0.0 {
        return [0.0; 3];
    }
    let w2 = VIEW_WHITE * VIEW_WHITE;
    // m²/W² + m(1 − y) − y = 0
    let m = (-(1.0 - y) + ((1.0 - y) * (1.0 - y) + 4.0 * y / w2).sqrt()) * w2 / 2.0;
    display_linear.map(|v| v.max(0.0) / y * m / VIEW_EXPOSURE)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Portrait RAWs must come out portrait. Uses local sample files when
    /// present (set VIBINGLUT_PORTRAIT_RAW), otherwise skips.
    #[test]
    fn portrait_raw_is_rotated() {
        let Ok(path) = std::env::var("VIBINGLUT_PORTRAIT_RAW") else { return eprintln!("no sample, skipped") };
        let Decoded::Raw(p) = Decoded::load(Path::new(&path)).unwrap() else { panic!("not a RAW") };
        assert!(p.height > p.width, "{}×{} is landscape", p.width, p.height);
    }
}
