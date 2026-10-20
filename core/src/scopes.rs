//! Colourist scopes rendered to small RGBA8 images on the render thread.

pub const HIST_W: usize = 256;
pub const HIST_H: usize = 120;
pub const VEC_SIZE: usize = 180;

#[derive(Debug, Clone)]
pub struct Scopes {
    pub histogram: Vec<u8>,
    pub waveform: Vec<u8>,
    pub vectorscope: Vec<u8>,
}

/// Turns hit counts into a soft-glow intensity (log-ish response).
fn glow(count: u32, gain: f32) -> f32 {
    1.0 - (-(count as f32) * gain).exp()
}

impl Scopes {
    /// `split` > 0 marks the left (ungraded) part of a before/after frame,
    /// which is excluded so scopes always describe the grade.
    pub fn compute(rgba: &[u8], width: u32, height: u32, split: f32) -> Self {
        let n = (width * height) as usize;
        let skip_cols = if split > 0.0 { (split * width as f32) as usize + 2 } else { 0 };
        // ponytail: fixed-stride subsampling, ~150k samples is plenty for scopes.
        let step = (n / 150_000).max(1);

        let mut hist = [[0u32; 256]; 3];
        let mut wave = vec![[0u32; 3]; HIST_W * HIST_H];
        let mut vec = vec![[0u32; 4]; VEC_SIZE * VEC_SIZE];
        let mut samples = 0u32;
        for i in (0..n).step_by(step) {
            let x = i % width as usize;
            if x < skip_cols {
                continue;
            }
            let px = &rgba[i * 4..i * 4 + 3];
            let col = x * HIST_W / width as usize;
            for c in 0..3 {
                hist[c][px[c] as usize] += 1;
                let row = HIST_H - 1 - px[c] as usize * (HIST_H - 1) / 255;
                wave[row * HIST_W + col][c] += 1;
            }
            let [r, g, b] = [px[0], px[1], px[2]].map(|v| v as f32 / 255.0);
            // Rec.709 Cb/Cr, scaled so 75% bars land near the edge.
            let cb = -0.1146 * r - 0.3854 * g + 0.5 * b;
            let cr = 0.5 * r - 0.4542 * g - 0.0458 * b;
            let half = VEC_SIZE as f32 / 2.0;
            let x = (half + cb * half * 1.8) as usize;
            let y = (half - cr * half * 1.8) as usize;
            if x < VEC_SIZE && y < VEC_SIZE {
                let cell = &mut vec[y * VEC_SIZE + x];
                cell[0] += px[0] as u32;
                cell[1] += px[1] as u32;
                cell[2] += px[2] as u32;
                cell[3] += 1;
            }
            samples += 1;
        }

        // Histogram: three translucent channels, normalised to the busiest bin.
        let peak = hist.iter().flat_map(|h| h[1..255].iter()).copied().max().unwrap_or(1).max(1) as f32;
        let mut histogram = vec![0u8; HIST_W * HIST_H * 4];
        for x in 0..HIST_W {
            for c in 0..3 {
                let h = ((hist[c][x] as f32 / peak).sqrt().min(1.0) * HIST_H as f32) as usize;
                for y in HIST_H - h..HIST_H {
                    let p = &mut histogram[(y * HIST_W + x) * 4..][..4];
                    p[c] = 230;
                    p[3] = p[3].max(170);
                }
            }
        }

        // RGB overlay waveform.
        let gain = 60.0 * HIST_W as f32 / samples.max(1) as f32;
        let mut waveform = vec![0u8; HIST_W * HIST_H * 4];
        for (cell, p) in wave.iter().zip(waveform.chunks_exact_mut(4)) {
            let v = cell.map(|c| glow(c, gain));
            p[0] = (v[0] * 255.0) as u8;
            p[1] = (v[1] * 255.0) as u8;
            p[2] = (v[2] * 255.0) as u8;
            p[3] = (v.iter().copied().fold(0.0, f32::max) * 255.0) as u8;
        }

        // Vectorscope: each cell tinted by the average colour that lands there.
        let vgain = 400.0 / samples.max(1) as f32;
        let mut vectorscope = vec![0u8; VEC_SIZE * VEC_SIZE * 4];
        for (cell, p) in vec.iter().zip(vectorscope.chunks_exact_mut(4)) {
            if cell[3] == 0 {
                continue;
            }
            let a = glow(cell[3], vgain).max(0.25);
            for c in 0..3 {
                p[c] = (cell[c] / cell[3]).max(90) as u8;
            }
            p[3] = (a * 255.0) as u8;
        }
        Self { histogram, waveform, vectorscope }
    }
}
