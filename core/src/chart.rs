//! Synthetic test chart that shows what a grade or LUT does: neutrals,
//! every hue from highlight to shadow, and ColorChecker patches (skin tones
//! included). Viewed through the same pipeline as a photo.

use crate::photo::{Photo, inverse_view_transform, multiply_row1, srgb_invert_gamma};
use crate::profiles::Profile;

pub const WIDTH: u32 = 1536;
pub const HEIGHT: u32 = 864;

/// ColorChecker Classic, sRGB 8-bit (X-Rite post-2014 values).
const CHECKER: [[u8; 3]; 24] = [
    [115, 82, 68],
    [194, 150, 130],
    [98, 122, 157],
    [87, 108, 67],
    [133, 128, 177],
    [103, 189, 170],
    [214, 126, 44],
    [80, 91, 166],
    [193, 90, 99],
    [94, 60, 108],
    [157, 188, 64],
    [224, 163, 46],
    [56, 61, 150],
    [70, 148, 73],
    [175, 54, 60],
    [231, 199, 31],
    [187, 86, 149],
    [8, 133, 161],
    [243, 243, 242],
    [200, 200, 200],
    [160, 160, 160],
    [122, 122, 121],
    [85, 85, 85],
    [52, 52, 52],
];

/// Display-referred chart pixel at normalised (u, v).
fn display(u: f32, v: f32) -> [f32; 3] {
    match v {
        // Smooth grey ramp, then 21 stepped greys (5% steps).
        v if v < 0.10 => [u; 3],
        v if v < 0.18 => [(u * 21.0).floor().min(20.0) / 20.0; 3],
        // Hue sweep: white → full colour → black, top to bottom.
        v if v < 0.74 => {
            let t = (v - 0.18) / 0.56;
            let hue = hsv(u);
            if t < 0.5 { mix([1.0; 3], hue, t * 2.0) } else { mix(hue, [0.0; 3], (t - 0.5) * 2.0) }
        }
        // ColorChecker: 2 rows × 12 patches with thin gaps.
        v => {
            let (col, row) = ((u * 12.0).floor().min(11.0) as usize, if v < 0.87 { 0 } else { 1 });
            let (fu, fv) = ((u * 12.0).fract(), ((v - 0.74) / 0.13).fract());
            if !(0.04..=0.96).contains(&fu) || !(0.05..=0.95).contains(&fv) {
                return [0.12; 3];
            }
            CHECKER[row * 12 + col].map(|c| c as f32 / 255.0)
        }
    }
}

fn hsv(h: f32) -> [f32; 3] {
    [1.0, 2.0 / 3.0, 1.0 / 3.0].map(|o| ((((h + o).fract()) * 6.0 - 3.0).abs() - 1.0).clamp(0.0, 1.0))
}

fn mix(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
    std::array::from_fn(|i| a[i] + (b[i] - a[i]) * t)
}

/// The chart encoded as `profile`'s camera signal. Log profiles get the
/// scene values that the neutral view transform maps back onto the
/// display chart, so it looks identical whatever the input profile.
pub fn test_chart(profile: Profile) -> Photo {
    let from709 = profile.from_rec709();
    let pixels = (0..HEIGHT)
        .flat_map(|y| (0..WIDTH).map(move |x| (x, y)))
        .map(|(x, y)| {
            let d = display((x as f32 + 0.5) / WIDTH as f32, (y as f32 + 0.5) / HEIGHT as f32);
            let [r, g, b] = if profile.is_log() {
                let scene = inverse_view_transform(d.map(srgb_invert_gamma));
                multiply_row1(&from709, &scene).map(|v| profile.encode(v))
            } else {
                d
            };
            [r, g, b, 1.0]
        })
        .collect();
    Photo { width: WIDTH, height: HEIGHT, pixels }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::photo::view_transform;

    #[test]
    fn log_chart_round_trips_through_the_view_transform() {
        for d in [[0.0, 0.0, 0.0], [0.5, 0.5, 0.5], [1.0, 1.0, 1.0], [0.9, 0.3, 0.1], [0.2, 0.4, 0.9]] {
            let back = view_transform(inverse_view_transform(d.map(srgb_invert_gamma)));
            assert!((0..3).all(|k| (back[k] - d[k]).abs() < 2e-3), "{d:?} -> {back:?}");
        }
        assert_eq!(test_chart(Profile::Standard).pixels.len(), (WIDTH * HEIGHT) as usize);
    }
}
