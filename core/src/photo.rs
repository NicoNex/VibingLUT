//! Photo loading: JPEG/PNG/TIFF via `image`, camera RAW via `rawler`.
//! Output is always a preview-sized, display-encoded (sRGB/Rec.709) image,
//! i.e. the same signal a camera "Standard" profile feeds into a LUT.

use std::path::Path;

use image::metadata::Orientation;
use image::{DynamicImage, ImageDecoder, ImageReader, Rgba32FImage};
use rawler::imgop::develop::{Intermediate, ProcessingStep, RawDevelop};
use rawler::imgop::srgb::srgb_apply_gamma;

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
    pub fn load(path: &Path) -> Result<Self, String> {
        let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
        if RAW_EXTS.contains(&ext.as_str()) { load_raw(path) } else { load_raster(path) }
    }

    /// Area-averaged copy that fits in `max` × `max`, keeping the aspect ratio.
    pub fn downscaled(&self, max: u32) -> Self {
        let scale = max as f32 / self.width.max(self.height) as f32;
        if scale >= 1.0 {
            return self.clone();
        }
        let dim = |v: u32| ((v as f32 * scale).round() as u32).max(1);
        Self::from_image(image::imageops::thumbnail(&self.to_image(), dim(self.width), dim(self.height)))
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
    let orientation = Orientation::from_exif(raw.orientation.to_u16() as u8).unwrap_or(Orientation::NoTransforms);
    let mut img = DynamicImage::ImageRgba32F(linear.downscaled(PREVIEW_MAX).to_image());
    img.apply_orientation(orientation);
    let mut photo = Photo::from_image(img.into_rgba32f());
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
    const EXPOSURE: f32 = 2.2; // Lumix RAWs sit ~1 EV under the camera JPEG
    const WHITE: f32 = 6.0; // exposed scene value mapped to display white
    let c = c.map(|v| v.max(0.0) * EXPOSURE);
    let m = c[0].max(c[1]).max(c[2]);
    let k = if m > 0.0 { (1.0 + m / (WHITE * WHITE)) / (1.0 + m) } else { 0.0 };
    c.map(|v| srgb_apply_gamma((v * k).min(1.0)))
}
