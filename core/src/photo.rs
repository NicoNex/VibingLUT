//! Photo loading: JPEG/PNG/TIFF via `image`, camera RAW via `rawler`.
//! Output is always a preview-sized, display-encoded (sRGB/Rec.709) image,
//! i.e. the same signal a camera "Standard" profile feeds into a LUT.

use std::path::Path;
use std::sync::Arc;

use image::metadata::Orientation;
use image::{DynamicImage, ImageDecoder, ImageReader, Rgba32FImage};
use rawler::imgop::chromatic_adaption::bradford_adaption_matrix;
use rawler::imgop::develop::{Intermediate, ProcessingStep, RawDevelop};
#[cfg(test)]
use rawler::imgop::matrix::IDENTITY_MATRIX_3;
pub use rawler::imgop::matrix::multiply_row1;
use rawler::imgop::matrix::{multiply, normalize, pseudo_inverse, transform_1d};
pub use rawler::imgop::srgb::{srgb_apply_gamma, srgb_invert_gamma};
use rawler::imgop::xyz::{Illuminant, SRGB_TO_XYZ_D65};
use rayon::prelude::*;

use crate::profiles::Profile;

pub const PREVIEW_MAX: u32 = 2048;
pub const THUMB_MAX: u32 = 256;

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
        box_downscale(&self.pixels, self.width, self.height, max, |p| *p)
    }

    fn to_image(&self) -> Rgba32FImage {
        Rgba32FImage::from_raw(self.width, self.height, bytemuck::cast_slice(&self.pixels).to_vec())
            .expect("pixel count matches dimensions")
    }

    fn from_image(img: Rgba32FImage) -> Self {
        Self { width: img.width(), height: img.height(), pixels: bytemuck::cast_slice(img.as_raw()).to_vec() }
    }
}

/// Integer-factor box filter over any pixel type convertible to RGBA.
/// Output rows run in parallel.
fn box_downscale<T: Sync>(src: &[T], width: u32, height: u32, max: u32, rgba: impl Fn(&T) -> [f32; 4] + Sync) -> Photo {
    let f = width.max(height).div_ceil(max).max(1);
    let (w, h) = ((width / f).max(1), (height / f).max(1));
    let norm = 1.0 / (f * f) as f32;
    let rgba = &rgba;
    let pixels = (0..h)
        .into_par_iter()
        .flat_map_iter(|y| {
            (0..w).map(move |x| {
                let mut acc = [0.0f32; 4];
                let (rows, cols) = (f.min(height - y * f), f.min(width - x * f));
                for dy in 0..rows {
                    let row = ((y * f + dy) * width + x * f) as usize;
                    for p in &src[row..row + cols as usize] {
                        let p = rgba(p);
                        for c in 0..4 {
                            acc[c] += p[c];
                        }
                    }
                }
                acc.map(|v| v * norm)
            })
        })
        .collect();
    Photo { width: w, height: h, pixels }
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
    // Demosaiced camera RGB, 1.0 = sensor clip. White balance and the colour
    // matrix are ours (see `Calibration`): rawler's Calibrate folds every
    // value above 1.0 back down, which bands and tints bright skies.
    use ProcessingStep::*;
    let dev = RawDevelop::new_with(&[Rescale, Demosaic, FujiRotate, CropActiveArea, CropDefault]);
    let linear = match dev.develop_intermediate(&raw).map_err(|e| e.to_string())? {
        Intermediate::ThreeColor(px) => calibrated(&raw, &px.data, px.width, px.height),
        Intermediate::FourColor(px) => calibrated(&raw, &px.data, px.width, px.height),
        Intermediate::Monochrome(_) => return Err("unsupported sensor layout".into()),
    };
    // Downscale in linear light (physically correct averaging), then encode.
    let orientation = Orientation::from_exif(exif_orientation as u8).unwrap_or(Orientation::NoTransforms);
    let mut img = DynamicImage::ImageRgba32F(linear.to_image());
    img.apply_orientation(orientation);
    let mut photo = Photo::from_image(img.into_rgba32f());
    for p in &mut photo.pixels {
        *p = [p[0] * RAW_TO_SCENE, p[1] * RAW_TO_SCENE, p[2] * RAW_TO_SCENE, 1.0];
    }
    Ok(photo)
}

/// Calibrates while downscaling, straight from the camera buffer: a
/// full-size RGBA copy would add ~400 MB of peak memory on a 24 MP file.
fn calibrated<const N: usize>(raw: &rawler::RawImage, px: &[[f32; N]], width: usize, height: usize) -> Photo {
    let cal = Calibration::<N>::new(raw);
    box_downscale(px, width as u32, height as u32, PREVIEW_MAX, |p| {
        let [r, g, b] = cal.apply(p);
        [r, g, b, 1.0]
    })
}

/// Camera RGB (N = 3) or CYGM/RGBE (N = 4) → linear Rec.709, with clipped
/// highlights kept neutral.
struct Calibration<const N: usize> {
    wb: [f32; N],
    cam2rgb: [[f32; N]; 3],
}

impl<const N: usize> Calibration<N> {
    fn new(raw: &rawler::RawImage) -> Self {
        let wb: [f32; N] =
            std::array::from_fn(|c| if raw.wb_coeffs[0].is_nan() { 1.0 } else { raw.wb_coeffs[c] / raw.wb_coeffs[1] });
        use Illuminant::*;
        let xyz2cam = raw
            .color_matrix_find_first([D65, A, B, C, D50, D55, D75, Daylight, Flash])
            .and_then(|(illu, m)| {
                let m = transform_1d::<N, 3>(m.get(..N * 3)?)?;
                Some(if illu == D65 { m } else { multiply(&m, &bradford_adaption_matrix(&illu, &D65)) })
            })
            .unwrap_or(std::array::from_fn(|i| std::array::from_fn(|j| if i == j { 1.0 } else { 0.0 })));
        // Same derivation as rawler: rows normalised so camera white maps to RGB white.
        let cam2rgb = pseudo_inverse(normalize(multiply(&xyz2cam, &SRGB_TO_XYZ_D65)));
        Self { wb, cam2rgb }
    }

    fn apply(&self, p: &[f32; N]) -> [f32; 3] {
        let cam: [f32; N] = std::array::from_fn(|c| p[c] * self.wb[c]);
        let rgb = self.cam2rgb.map(|row| row.iter().zip(&cam).map(|(m, v)| m * v).sum::<f32>());
        // A clipped channel has lost its true value, so its hue is wrong
        // (white balance turns clipped skies magenta). Fade to neutral over
        // the last 15% before clip: wide enough that no edge appears (5% shows one).
        // ponytail: also pales vivid colours that come close to clip;
        // highlight reconstruction (inpainting from neighbours) would keep them.
        let t = ((p.iter().copied().fold(0.0, f32::max) - 0.85) / 0.15).clamp(0.0, 1.0);
        let t = t * t * (3.0 - 2.0 * t);
        // The brightest channel, since unclipped ones still carry the detail.
        let white = cam.iter().copied().fold(0.0, f32::max);
        rgb.map(|v| (v + (white - v) * t).max(0.0))
    }
}

/// Scene-linear scale of the calibrated RAW: Lumix RAWs sit ~0.8 EV under scene grey.
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

    /// A green-clipped pixel comes out neutral, and the fade has no step.
    #[test]
    fn clipped_highlights_are_neutral_and_smooth() {
        let cal = Calibration { wb: [2.0, 1.0, 1.7], cam2rgb: IDENTITY_MATRIX_3 };
        let [r, g, b] = cal.apply(&[0.4, 1.0, 0.5]);
        assert!(r == g && g == b, "{r} {g} {b}");
        let (lo, hi) = (cal.apply(&[0.3, 0.8499, 0.4]), cal.apply(&[0.3, 0.8501, 0.4]));
        assert!((0..3).all(|c| (lo[c] - hi[c]).abs() < 1e-3), "{lo:?} {hi:?}");
    }

    /// Portrait RAWs must come out portrait. Uses local sample files when
    /// present (set VIBINGLUT_PORTRAIT_RAW), otherwise skips.
    #[test]
    fn portrait_raw_is_rotated() {
        let Ok(path) = std::env::var("VIBINGLUT_PORTRAIT_RAW") else { return eprintln!("no sample, skipped") };
        let Decoded::Raw(p) = Decoded::load(Path::new(&path)).unwrap() else { panic!("not a RAW") };
        assert!(p.height > p.width, "{}×{} is landscape", p.width, p.height);
    }
}
