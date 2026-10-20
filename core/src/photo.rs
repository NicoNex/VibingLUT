//! Photo loading: JPEG/PNG/TIFF via `image`, camera RAW via `rawler`.
//! Output is always a preview-sized, display-encoded (sRGB/Rec.709) image,
//! i.e. the same signal a camera "Standard" profile feeds into a LUT.

use std::path::Path;

use image::{DynamicImage, ImageDecoder, ImageReader};
use rawler::decoders::Orientation;
use rawler::imgop::develop::{Intermediate, ProcessingStep, RawDevelop};

pub const PREVIEW_MAX: u32 = 2048;
pub const THUMB_MAX: u32 = 384;

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
    pub fn load(path: &Path) -> Result<Self, String> {
        let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
        if RAW_EXTS.contains(&ext.as_str()) { load_raw(path) } else { load_raster(path) }
    }

    /// Box-filtered copy that fits in `max` × `max`.
    pub fn downscaled(&self, max: u32) -> Self {
        let f = self.width.max(self.height).div_ceil(max).max(1);
        if f == 1 {
            return self.clone();
        }
        let (w, h) = (self.width / f, self.height / f);
        let mut pixels = Vec::with_capacity((w * h) as usize);
        let norm = 1.0 / (f * f) as f32;
        for y in 0..h {
            for x in 0..w {
                let mut acc = [0.0f32; 4];
                for dy in 0..f {
                    let row = ((y * f + dy) * self.width + x * f) as usize;
                    for p in &self.pixels[row..row + f as usize] {
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

fn load_raw(path: &Path) -> Result<Photo, String> {
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
    let mut photo = orient(linear.downscaled(PREVIEW_MAX), raw.orientation);
    for p in &mut photo.pixels {
        let [r, g, b] = base_look([p[0], p[1], p[2]]);
        *p = [r, g, b, 1.0];
    }
    Ok(photo)
}

/// Neutral display rendering for RAW: hue-preserving extended Reinhard
/// shoulder, then the sRGB transfer.
// ponytail: approximates, does not replicate, the camera's Standard profile.
// For a pixel-exact LUT match shoot RAW+JPEG and grade the JPEG.
fn base_look(c: [f32; 3]) -> [f32; 3] {
    const WHITE: f32 = 6.0;
    let c = c.map(|v| v.max(0.0));
    let m = c[0].max(c[1]).max(c[2]);
    let k = if m > 0.0 { (1.0 + m / (WHITE * WHITE)) / (1.0 + m) * 1.6 } else { 0.0 };
    c.map(|v| srgb_encode((v * k).min(1.0)))
}

fn srgb_encode(v: f32) -> f32 {
    if v <= 0.003_130_8 { v * 12.92 } else { 1.055 * v.powf(1.0 / 2.4) - 0.055 }
}

fn orient(p: Photo, o: Orientation) -> Photo {
    use Orientation::*;
    if matches!(o, Normal | Unknown) {
        return p;
    }
    let (w, h) = (p.width as usize, p.height as usize);
    let (ow, oh) = if matches!(o, Transpose | Rotate90 | Transverse | Rotate270) { (h, w) } else { (w, h) };
    // Source index for output pixel (x, y), per EXIF orientation.
    let src = |x: usize, y: usize| match o {
        HorizontalFlip => y * w + (w - 1 - x),
        Rotate180 => (h - 1 - y) * w + (w - 1 - x),
        VerticalFlip => (h - 1 - y) * w + x,
        Transpose => x * w + y,
        Rotate90 => (h - 1 - x) * w + y,
        Transverse => (h - 1 - x) * w + (w - 1 - y),
        Rotate270 => x * w + (w - 1 - y),
        Normal | Unknown => y * w + x,
    };
    let pixels = (0..oh).flat_map(|y| (0..ow).map(move |x| (x, y))).map(|(x, y)| p.pixels[src(x, y)]).collect();
    Photo { width: ow as u32, height: oh as u32, pixels }
}
