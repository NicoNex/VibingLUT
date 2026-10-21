//! GPU engine (wgpu → Vulkan / Metal / DX12, native only) and its worker thread.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::sync::{Arc, mpsc};

use wgpu::util::DeviceExt;

use crate::grade::{CURVE_RES, CURVES, Grade, Params};
use crate::lab::{self, Blend, Layer, Source, Stack};
use crate::lut::{Format, Lut3d};
use crate::matching::{self, Match};
use crate::photo::{Photo, THUMB_MAX};
use crate::profiles::Profile;
use crate::scopes::Scopes;

pub struct Gpu {
    device: wgpu::Device,
    queue: wgpu::Queue,
    preview: wgpu::ComputePipeline,
    apply: wgpu::ComputePipeline,
    bake: wgpu::ComputePipeline,
    /// Persistent uniforms, rewritten per frame with `queue.write_buffer`.
    params: wgpu::Buffer,
    curves: wgpu::Buffer,
    /// Bound in place of a look LUT when there is none.
    no_lut: wgpu::Buffer,
    pub name: String,
}

/// A photo resident on the GPU together with its output/readback buffers,
/// so interactive frames allocate nothing.
pub struct GpuPhoto {
    src: wgpu::Buffer,
    out: wgpu::Buffer,
    staging: wgpu::Buffer,
    width: u32,
    height: u32,
}

pub struct GpuLut {
    buf: wgpu::Buffer,
    size: u32,
}

const WORKGROUP: u32 = 256;
/// Pixels sampled per image for the reference match.
const MATCH_SAMPLES: usize = 200_000;

impl Gpu {
    pub fn new() -> Result<Self, String> {
        let mut desc = wgpu::InstanceDescriptor::new_without_display_handle();
        desc.backends = wgpu::Backends::PRIMARY; // Vulkan, Metal, DX12
        let instance = wgpu::Instance::new(desc);
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            ..Default::default()
        }))
        .map_err(|e| format!("no Vulkan/Metal GPU: {e}"))?;
        let info = adapter.get_info();
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("vibinglut"),
            required_limits: adapter.limits(),
            ..Default::default()
        }))
        .map_err(|e| e.to_string())?;
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("grade.wgsl"),
            source: wgpu::ShaderSource::Wgsl(include_str!("grade.wgsl").into()),
        });
        let pipeline = |entry: &str| {
            device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some(entry),
                layout: None,
                module: &module,
                entry_point: Some(entry),
                compilation_options: Default::default(),
                cache: None,
            })
        };
        let uniform = |size: usize| {
            device.create_buffer(&wgpu::BufferDescriptor {
                label: None,
                size: size as u64,
                usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            })
        };
        Ok(Self {
            preview: pipeline("preview"),
            apply: pipeline("apply_lut"),
            bake: pipeline("bake"),
            params: uniform(std::mem::size_of::<Params>()),
            curves: uniform(CURVES * CURVE_RES * 4),
            no_lut: uniform(16),
            name: format!("{} ({:?})", info.name, info.backend),
            device,
            queue,
        })
    }

    fn storage(&self, bytes: &[u8]) -> wgpu::Buffer {
        self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: None,
            contents: bytes,
            usage: wgpu::BufferUsages::STORAGE,
        })
    }

    fn output(&self, bytes: u64) -> (wgpu::Buffer, wgpu::Buffer) {
        let buffer = |usage| {
            self.device.create_buffer(&wgpu::BufferDescriptor {
                label: None,
                size: bytes,
                usage,
                mapped_at_creation: false,
            })
        };
        (
            buffer(wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC),
            buffer(wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST),
        )
    }

    fn bind(&self, pipeline: &wgpu::ComputePipeline, entries: &[(u32, &wgpu::Buffer)]) -> wgpu::BindGroup {
        let entries: Vec<_> = entries
            .iter()
            .map(|(b, buf)| wgpu::BindGroupEntry { binding: *b, resource: buf.as_entire_binding() })
            .collect();
        self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &pipeline.get_bind_group_layout(0),
            entries: &entries,
        })
    }

    pub fn upload(&self, photo: &Photo) -> GpuPhoto {
        let src = self.storage(bytemuck::cast_slice(&photo.pixels));
        let (out, staging) = self.output(photo.width as u64 * photo.height as u64 * 4);
        GpuPhoto { src, out, staging, width: photo.width, height: photo.height }
    }

    pub fn upload_lut(&self, lut: &Lut3d) -> GpuLut {
        let data: Vec<[f32; 4]> = lut.data.iter().map(|&[r, g, b]| [r, g, b, 1.0]).collect();
        GpuLut { buf: self.storage(bytemuck::cast_slice(&data)), size: lut.size as u32 }
    }

    fn write_params(&self, mut p: Params, w: u32, h: u32, lut_size: u32, split: f32) {
        (p.width, p.height, p.lut_size, p.split) = (w, h, lut_size, split);
        self.queue.write_buffer(&self.params, 0, bytemuck::bytes_of(&p));
    }

    /// Dispatches over `count` items, then copies `out` back through `staging`.
    fn run(
        &self,
        pipeline: &wgpu::ComputePipeline,
        bind: &wgpu::BindGroup,
        count: u32,
        out: &wgpu::Buffer,
        staging: &wgpu::Buffer,
    ) -> Vec<u8> {
        let mut enc = self.device.create_command_encoder(&Default::default());
        {
            let mut pass = enc.begin_compute_pass(&Default::default());
            pass.set_pipeline(pipeline);
            pass.set_bind_group(0, bind, &[]);
            pass.dispatch_workgroups(count.div_ceil(WORKGROUP), 1, 1);
        }
        enc.copy_buffer_to_buffer(out, 0, staging, 0, out.size());
        self.queue.submit([enc.finish()]);
        let slice = staging.slice(..);
        slice.map_async(wgpu::MapMode::Read, |r| r.expect("GPU readback failed"));
        self.device.poll(wgpu::PollType::wait_indefinitely()).expect("GPU device lost");
        let bytes = slice.get_mapped_range().expect("map GPU readback").to_vec();
        staging.unmap();
        bytes
    }

    /// Graded preview as RGBA8, optionally seen through a look LUT.
    pub fn render(&self, photo: &GpuPhoto, grade: &Grade, look: Option<&GpuLut>, split: f32) -> Vec<u8> {
        self.write_params(grade.params, photo.width, photo.height, look.map_or(0, |l| l.size), split);
        self.queue.write_buffer(&self.curves, 0, bytemuck::cast_slice(&grade.curve_samples()));
        let lut = look.map_or(&self.no_lut, |l| &l.buf);
        let bind = self
            .bind(&self.preview, &[(0, &self.params), (1, &self.curves), (2, &photo.src), (3, &photo.out), (5, lut)]);
        self.run(&self.preview, &bind, photo.width * photo.height, &photo.out, &photo.staging)
    }

    /// Photo seen through an external LUT (tetrahedral), as RGBA8.
    pub fn apply_lut(&self, photo: &GpuPhoto, lut: &GpuLut, split: f32) -> Vec<u8> {
        self.write_params(Params::default(), photo.width, photo.height, lut.size, split);
        let bind = self.bind(&self.apply, &[(0, &self.params), (2, &photo.src), (3, &photo.out), (5, &lut.buf)]);
        self.run(&self.apply, &bind, photo.width * photo.height, &photo.out, &photo.staging)
    }

    /// Evaluates the grade exactly on every lattice point of an N³ cube.
    pub fn bake(&self, grade: &Grade, size: usize, title: String) -> Lut3d {
        let n = (size * size * size) as u32;
        self.write_params(grade.params, 0, 0, size as u32, 0.0);
        self.queue.write_buffer(&self.curves, 0, bytemuck::cast_slice(&grade.curve_samples()));
        let (out, staging) = self.output(n as u64 * 16);
        let bind = self.bind(&self.bake, &[(0, &self.params), (1, &self.curves), (4, &out)]);
        let bytes = self.run(&self.bake, &bind, n, &out, &staging);
        let data = bytemuck::cast_slice::<u8, [f32; 4]>(&bytes).iter().map(|&[r, g, b, _]| [r, g, b]).collect();
        Lut3d { title, size, data, format: Default::default() }
    }
}

impl Gpu {
    /// The grade followed by `look` (chained, as on the Grade page) on an N³ lattice.
    pub fn bake_with_look(&self, grade: Grade, look: Vec<Layer>, size: usize, title: String) -> Lut3d {
        let baked = self.bake(&grade, size, title.clone());
        if look.is_empty() {
            return baked;
        }
        // Grade baked on the export lattice itself, so its nodes are exact.
        let baked = Arc::new(baked);
        let base = CompareBase { grade, looks: look, input: Profile::Standard };
        Lut3d { title, ..base.compose(size, &|_| baked.clone()) }
    }
}

// ------------------------------------------------------------------ worker

/// What Compare LUTs shows candidate LUTs on: the Grade page's result,
/// handed back to each LUT in the source's own signal.
#[derive(Clone, Debug)]
pub struct CompareBase {
    pub grade: Grade,
    pub looks: Vec<Layer>,
    /// Profile of the source signal, which the compared LUTs expect.
    pub input: Profile,
}

impl CompareBase {
    /// grade → looks → back to `input`, on an N³ lattice.
    fn compose(&self, size: usize, bake: &dyn Fn(&Grade) -> Arc<Lut3d>) -> Lut3d {
        let grade = Layer { source: Source::Grade(Box::new(self.grade.clone())), intensity: 1.0, input: self.input };
        let back = Layer { source: Source::Convert(self.input), intensity: 1.0, input: self.input };
        let layers = std::iter::once(grade).chain(self.looks.iter().cloned()).chain([back]).collect();
        lab::compose(&Stack { input: self.input, layers, blend: Blend::Chain }, size, bake)
    }
}

/// `lut` applied after `base`: lut(base(c)) on base's lattice.
fn after(base: &Lut3d, lut: &Lut3d) -> Lut3d {
    let data = base.data.iter().map(|&c| lut.sample(c)).collect();
    Lut3d { title: lut.title.clone(), size: base.size, data, format: lut.format }
}

pub enum Job {
    SetPhoto(Arc<Photo>),
    /// LUTs applied after the grade on the Grade page (empty = none).
    SetLook(Vec<Layer>),
    /// Base of the Compare LUTs previews; None = the ungraded source.
    SetCompareBase(Option<CompareBase>),
    Render(Grade, f32),
    /// The filmstrip photos (already preview-sized); `generation` lets the
    /// UI drop frames of an older strip.
    SetRoll(u64, Vec<Arc<Photo>>),
    /// Every filmstrip photo through the grade and the look LUTs.
    RenderRoll(Grade),
    /// Colour transfer from the current photo (decoded by `input`, otherwise
    /// ungraded) to a display-referred reference.
    Match {
        reference: Arc<Photo>,
        input: Grade,
    },
    /// The photo through the LUT file at `path` (kept on the GPU while selected).
    ApplyLut(PathBuf, f32),
    /// Thumbnails of the LUT files, read one at a time; `generation` lets
    /// the UI drop stale results.
    Gallery(u64, Arc<Vec<PathBuf>>),
    /// Preview of a LUT Lab stack on the current photo.
    Lab {
        stack: Stack,
        split: f32,
    },
    ExportLab {
        stack: Stack,
        size: usize,
        format: Format,
        title: String,
        path: PathBuf,
    },
    /// The grade followed by its look LUTs.
    Export {
        grade: Grade,
        look: Vec<Layer>,
        size: usize,
        format: Format,
        title: String,
        path: PathBuf,
    },
}

#[derive(Clone, Copy, PartialEq)]
enum Slot {
    Grade,
    Compare,
    Gallery,
    Lab,
    Look,
    Roll,
}

impl Job {
    /// Jobs whose newer instance makes older ones pointless.
    fn slot(&self) -> Option<Slot> {
        match self {
            Job::Render(..) => Some(Slot::Grade),
            Job::ApplyLut(..) => Some(Slot::Compare),
            Job::Gallery(..) => Some(Slot::Gallery),
            Job::Lab { .. } => Some(Slot::Lab),
            Job::SetLook(..) => Some(Slot::Look),
            Job::RenderRoll(..) => Some(Slot::Roll),
            _ => None,
        }
    }
}

#[derive(Debug)]
pub enum Done {
    Ready(String),
    Failed(String),
    Graded {
        width: u32,
        height: u32,
        rgba: Vec<u8>,
        scopes: Scopes,
    },
    Compared {
        width: u32,
        height: u32,
        rgba: Vec<u8>,
    },
    Lab {
        width: u32,
        height: u32,
        rgba: Vec<u8>,
    },
    Thumb {
        generation: u64,
        index: usize,
        width: u32,
        height: u32,
        rgba: Vec<u8>,
    },
    Exported(Result<PathBuf, String>),
    RollFrame {
        generation: u64,
        index: usize,
        width: u32,
        height: u32,
        rgba: Vec<u8>,
    },
    Matched(Match),
    /// A compared LUT file could not be read (deleted or broken since the scan).
    Unreadable(PathBuf, String),
}

#[derive(Default)]
struct Worker {
    source: Option<Arc<Photo>>,
    photo: Option<GpuPhoto>,
    thumb: Option<GpuPhoto>,
    /// The LUT shown in the comparison view, with the file's modification time
    /// so an edited file is re-read.
    selected: Option<(PathBuf, Option<std::time::SystemTime>, GpuLut)>,
    /// Composite of the Grade page's look LUTs.
    look: Option<GpuLut>,
    /// Compare base composed at 33³ (thumbnails) and 65³ (large preview).
    base: Option<(Lut3d, Lut3d)>,
    /// Thumbnail pass in progress: generation, LUT files, next index.
    // ponytail: each pass re-reads the files (no lattice cache, keeps memory flat
    // for big libraries); cache by path + mtime if passes get slow.
    gallery: Option<(u64, Arc<Vec<PathBuf>>, usize)>,
    /// Filmstrip photos on the GPU, with their generation.
    roll: (u64, Vec<GpuPhoto>),
    /// Last grade baked for the Lab, reused until the grade changes.
    grade_lut: std::cell::RefCell<Option<(Grade, Arc<Lut3d>)>>,
}

impl Worker {
    /// Renders the next gallery thumbnail; false once the pass is done.
    fn gallery_step(&mut self, gpu: &Gpu, notify: &impl Fn(Done)) -> bool {
        let (Some((generation, paths, index)), Some(th)) = (&mut self.gallery, &self.thumb) else {
            return false;
        };
        let (generation, i) = (*generation, *index);
        *index += 1;
        if let Some(Ok(lut)) = paths.get(i).map(|p| Lut3d::load(p)) {
            let lut = match &self.base {
                Some((small, _)) => after(small, &lut),
                None => lut,
            };
            let rgba = gpu.apply_lut(th, &gpu.upload_lut(&lut), 0.0);
            notify(Done::Thumb { generation, index: i, width: th.width, height: th.height, rgba });
        }
        if i + 1 >= paths.len() {
            self.gallery = None;
        }
        self.gallery.is_some()
    }

    fn bake_cached(&self, gpu: &Gpu, g: &Grade) -> Arc<Lut3d> {
        if let Some((cached, lut)) = &*self.grade_lut.borrow()
            && cached == g
        {
            return lut.clone();
        }
        let lut = Arc::new(gpu.bake(g, lab::SOURCE_SIZE, String::new()));
        *self.grade_lut.borrow_mut() = Some((g.clone(), lut.clone()));
        lut
    }

    fn handle(&mut self, gpu: &Gpu, job: Job, notify: &impl Fn(Done)) {
        match job {
            Job::SetPhoto(p) => {
                // Same pixels (e.g. only the input profile changed): keep the upload.
                if self.source.as_ref().is_some_and(|s| Arc::ptr_eq(s, &p)) {
                    return;
                }
                self.thumb = Some(gpu.upload(&p.downscaled(THUMB_MAX)));
                self.photo = Some(gpu.upload(&p));
                self.source = Some(p);
            }
            Job::SetLook(layers) => {
                // ponytail: 33³ preview composite, like the Lab; exports compose exactly.
                self.look = (!layers.is_empty()).then(|| {
                    let stack = Stack { input: Profile::Standard, layers, blend: Blend::Chain };
                    gpu.upload_lut(&lab::compose(&stack, 33, &|g| self.bake_cached(gpu, g)))
                });
            }
            Job::SetCompareBase(base) => {
                self.base = base.map(|b| {
                    let bake = |g: &Grade| self.bake_cached(gpu, g);
                    (b.compose(33, &bake), b.compose(lab::SOURCE_SIZE, &bake))
                });
                self.selected = None; // its GPU LUT was folded into the old base
            }
            Job::Render(grade, split) => {
                let Some(ph) = &self.photo else { return };
                let rgba = gpu.render(ph, &grade, self.look.as_ref(), split);
                let scopes = Scopes::compute(&rgba, ph.width, ph.height, split);
                notify(Done::Graded { width: ph.width, height: ph.height, rgba, scopes });
            }
            Job::SetRoll(generation, photos) => {
                self.roll = (generation, photos.iter().map(|p| gpu.upload(p)).collect())
            }
            Job::RenderRoll(grade) => {
                let generation = self.roll.0;
                for (index, ph) in self.roll.1.iter().enumerate() {
                    let rgba = gpu.render(ph, &grade, self.look.as_ref(), 0.0);
                    notify(Done::RollFrame { generation, index, width: ph.width, height: ph.height, rgba });
                }
            }
            Job::Match { reference, input } => {
                let Some(ph) = &self.photo else { return };
                // ponytail: 8-bit display render as the source sample, plenty for statistics.
                let source = matching::samples_rgba8(&gpu.render(ph, &input, None, 0.0), MATCH_SAMPLES);
                let reference = matching::samples_rgba_f32(&reference.pixels, MATCH_SAMPLES);
                notify(Done::Matched(matching::transfer(&source, &reference)));
            }
            Job::ApplyLut(path, split) => {
                let Some(ph) = &self.photo else { return };
                let mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
                if self.selected.as_ref().is_none_or(|(p, t, _)| *p != path || *t != mtime) {
                    match Lut3d::load(&path) {
                        Ok(l) => {
                            let l = match &self.base {
                                Some((_, large)) => after(large, &l),
                                None => l,
                            };
                            self.selected = Some((path, mtime, gpu.upload_lut(&l)))
                        }
                        Err(e) => {
                            self.selected = None;
                            return notify(Done::Unreadable(path, e));
                        }
                    }
                }
                let lut = &self.selected.as_ref().expect("loaded above").2;
                let rgba = gpu.apply_lut(ph, lut, split);
                notify(Done::Compared { width: ph.width, height: ph.height, rgba });
            }
            // Rendered one thumbnail at a time by `gallery_step`, so previews never wait for a pass.
            Job::Gallery(generation, paths) => self.gallery = Some((generation, paths, 0)),
            Job::Lab { stack, split } => {
                let Some(ph) = &self.photo else { return };
                // ponytail: 33³ preview lattice; exports compose at full size.
                let lut = lab::compose(&stack, 33, &|g| self.bake_cached(gpu, g));
                let rgba = gpu.apply_lut(ph, &gpu.upload_lut(&lut), split);
                notify(Done::Lab { width: ph.width, height: ph.height, rgba });
            }
            Job::ExportLab { stack, size, format, title, path } => {
                let size = format.fixed_size().unwrap_or(size);
                let lut = lab::compose(&stack, size, &|g| self.bake_cached(gpu, g));
                notify(Done::Exported(Lut3d { title, format, ..lut }.save(&path).map(|_| path)));
            }
            Job::Export { grade, look, size, format, title, path } => {
                let size = format.fixed_size().unwrap_or(size);
                let lut = Lut3d { format, ..gpu.bake_with_look(grade, look, size, title) };
                notify(Done::Exported(lut.save(&path).map(|_| path)));
            }
        }
    }
}

/// Spawns the render thread. Previews and gallery passes are coalesced to
/// the latest request so slider drags never queue up stale frames.
pub fn spawn(notify: impl Fn(Done) + Send + 'static) -> mpsc::Sender<Job> {
    let (tx, rx) = mpsc::channel::<Job>();
    std::thread::Builder::new()
        .name("gpu".into())
        .spawn(move || {
            let gpu = match Gpu::new() {
                Ok(g) => g,
                Err(e) => return notify(Done::Failed(e)),
            };
            notify(Done::Ready(gpu.name.clone()));
            let mut worker = Worker::default();
            let mut thumbs_left = false;
            loop {
                // Block for work only when no thumbnail pass is pending.
                let first = if thumbs_left {
                    rx.try_recv().ok()
                } else {
                    match rx.recv() {
                        Ok(job) => Some(job),
                        Err(_) => return,
                    }
                };
                // Keep only the newest job per slot, preserving order otherwise.
                let mut batch: Vec<Job> = first.into_iter().chain(rx.try_iter()).collect();
                let mut seen = [false; 6];
                batch.reverse();
                batch.retain(|j| j.slot().is_none_or(|s| !std::mem::replace(&mut seen[s as usize], true)));
                batch.reverse();
                // A device loss or validation error panics inside wgpu: report it instead of dying silently.
                let ok = catch_unwind(AssertUnwindSafe(|| {
                    batch.into_iter().for_each(|job| worker.handle(&gpu, job, &notify));
                    thumbs_left = worker.gallery_step(&gpu, &notify);
                }));
                if ok.is_err() {
                    return notify(Done::Failed("GPU error, restart the application".into()));
                }
            }
        })
        .expect("spawn gpu thread");
    tx
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The neutral grade must bake to an identity LUT, and the baked LUT
    /// must reproduce the direct per-pixel grade (preview == export).
    #[test]
    fn identity_and_preview_matches_lut() {
        let Ok(gpu) = Gpu::new() else { return eprintln!("no GPU, skipped") };
        let lut = gpu.bake(&Grade::default(), 17, "id".into());
        for (i, v) in lut.data.iter().enumerate() {
            let e = [(i % 17) as f32 / 16.0, (i / 17 % 17) as f32 / 16.0, (i / 289) as f32 / 16.0];
            for c in 0..3 {
                assert!((v[c] - e[c]).abs() < 2e-4, "lattice {i}: {v:?} vs {e:?}");
            }
        }

        let mut grade = Grade::default();
        grade.params.exposure = 0.4;
        grade.params.temperature = 30.0;
        grade.params.saturation = 20.0;
        grade.params.hue_shift[5] = 40.0;
        (grade.params.film_sat, grade.params.film_density, grade.params.mixer[1]) = (40.0, 50.0, 15.0);
        (grade.params.match_amount, grade.params.match_ab) = (60.0, [0.1, 0.02, -0.03, 0.05, 0.01, -0.005]);
        grade.curves[0] = vec![(0.0, 0.05), (0.5, 0.55), (1.0, 0.95)];
        grade.curves[crate::grade::HUE_SAT] = vec![(0.0, 0.5), (0.6, 0.7), (1.0, 0.5)];
        grade.curves[crate::grade::MATCH_L] = vec![(0.0, 0.03), (0.5, 0.47), (1.0, 0.97)];
        let photo = Photo {
            width: 64,
            height: 1,
            pixels: (0..64)
                .map(|i| {
                    let t = i as f32 / 63.0;
                    [t, (t * 7.0) % 1.0, 1.0 - t, 1.0]
                })
                .collect(),
        };
        let gp = gpu.upload(&photo);
        let direct = gpu.render(&gp, &grade, None, 0.0);
        let via_lut = gpu.apply_lut(&gp, &gpu.upload_lut(&gpu.bake(&grade, 65, "t".into())), 0.0);
        let diff: Vec<u8> = direct.iter().zip(&via_lut).map(|(a, b)| a.abs_diff(*b)).collect();
        let worst = *diff.iter().max().unwrap();
        let mean = diff.iter().map(|&d| d as f32).sum::<f32>() / diff.len() as f32;
        // ponytail: peak error comes from out-of-gamut clamping of strongly saturated darks.
        assert!(worst <= 8 && mean < 0.5, "65³ LUT vs direct grade: worst {worst}/255, mean {mean}");
    }

    /// Film colour leaves neutrals alone and makes colours denser, never
    /// lighter; the monochrome mixer outputs grey; the clipping overlay marks
    /// only pixels that clip.
    #[test]
    fn film_mixer_and_overlay() {
        let Ok(gpu) = Gpu::new() else { return eprintln!("no GPU, skipped") };
        let px = [[0.5, 0.5, 0.5], [0.8, 0.3, 0.2], [0.2, 0.5, 0.8], [1.0, 1.0, 1.0], [0.0, 0.0, 0.0]];
        let photo = Photo { width: 5, height: 1, pixels: px.iter().map(|&[r, g, b]| [r, g, b, 1.0]).collect() };
        let gp = gpu.upload(&photo);
        let render = |g: &Grade| gpu.render(&gp, g, None, 0.0);
        let luma =
            |o: &[u8], i: usize| 0.2126 * o[i * 4] as f32 + 0.7152 * o[i * 4 + 1] as f32 + 0.0722 * o[i * 4 + 2] as f32;
        let plain = render(&Grade::default());
        for (k, v) in [("film_sat", 60.0), ("film_density", 80.0)] {
            let mut g = Grade::default();
            if k == "film_sat" {
                g.params.film_sat = v
            } else {
                g.params.film_density = v
            }
            let out = render(&g);
            assert!((0..3).all(|c| out[c].abs_diff(plain[c]) <= 1), "{k} moved grey: {:?}", &out[..4]);
            for i in [1, 2] {
                assert!(luma(&out, i) < luma(&plain, i) - 2.0, "{k} did not darken colour {i}");
            }
        }
        let mut g = Grade::default();
        g.params.mixer_mono = 1.0;
        let out = render(&g);
        for i in 0..5 {
            assert!(
                out[i * 4].abs_diff(out[i * 4 + 1]) <= 1 && out[i * 4].abs_diff(out[i * 4 + 2]) <= 1,
                "mono pixel {i} not grey"
            );
        }
        let mut g = Grade::default();
        g.params.exposure = 1.5;
        let bright = render(&g);
        g.params.overlay = 1;
        // 5×1 image: stripes cover x + y in 4..8, i.e. pixel 4 only.
        let marked = render(&g);
        assert_eq!(&marked[..16], &bright[..16]);
        let mut g = Grade::default();
        g.params.overlay = 1;
        assert_eq!(&render(&g)[16..19], &[46, 107, 255], "crushed black not marked");
    }

    /// A look made of an identity LUT leaves the exported grade unchanged;
    /// a full-strength look LUT replaces the grade's output with lut(grade).
    #[test]
    fn look_luts_follow_the_grade() {
        let Ok(gpu) = Gpu::new() else { return eprintln!("no GPU, skipped") };
        let mut grade = Grade::default();
        grade.params.exposure = 0.3;
        grade.params.saturation = 15.0;
        let plain = gpu.bake(&grade, 17, "g".into());
        let layer = |lut: Lut3d| Layer { source: Source::Lut(Arc::new(lut)), intensity: 1.0, input: Profile::Standard };
        let same = gpu.bake_with_look(grade.clone(), vec![layer(Lut3d::identity(33, Format::Cube))], 17, "g".into());
        let mut inverted = Lut3d::identity(17, Format::Cube);
        inverted.data.iter_mut().for_each(|c| *c = c.map(|v| 1.0 - v));
        let looked = gpu.bake_with_look(grade, vec![layer(inverted)], 17, "g".into());
        for ((a, b), c) in plain.data.iter().zip(&same.data).zip(&looked.data) {
            for k in 0..3 {
                assert!((a[k] - b[k]).abs() < 1e-4, "identity look changed {a:?} → {b:?}");
                assert!((1.0 - a[k].clamp(0.0, 1.0) - c[k]).abs() < 1e-3, "look not applied: {a:?} → {c:?}");
            }
        }
    }

    /// With a neutral grade the Compare base is the identity, so a compared
    /// LUT looks as it does on the ungraded photo; a graded base feeds it lut(grade).
    #[test]
    fn compare_base_puts_the_grade_before_the_lut() {
        let Ok(gpu) = Gpu::new() else { return eprintln!("no GPU, skipped") };
        let bake = |g: &Grade| Arc::new(gpu.bake(g, lab::SOURCE_SIZE, String::new()));
        let neutral = CompareBase { grade: Grade::default(), looks: Vec::new(), input: Profile::Standard };
        for (v, e) in neutral.compose(17, &bake).data.iter().zip(&Lut3d::identity(17, Format::Cube).data) {
            assert!((0..3).all(|k| (v[k] - e[k]).abs() < 1e-3), "{v:?} vs {e:?}");
        }
        let mut grade = Grade::default();
        grade.params.exposure = 0.4;
        let base = CompareBase { grade: grade.clone(), looks: Vec::new(), input: Profile::Standard };
        let mut inverted = Lut3d::identity(17, Format::Cube);
        inverted.data.iter_mut().for_each(|c| *c = c.map(|v| 1.0 - v));
        let shown = after(&base.compose(17, &bake), &inverted);
        for (s, g) in shown.data.iter().zip(&gpu.bake(&grade, 17, String::new()).data) {
            assert!((0..3).all(|k| (s[k] - (1.0 - g[k].clamp(0.0, 1.0))).abs() < 2e-3), "{s:?} vs inverted {g:?}");
        }
    }

    /// Every log profile: CPU encode (gamut + OETF) followed by the GPU
    /// decode + view transform must reproduce the CPU view transform, so all
    /// profiles start from the same look and the decoders match the encoders.
    #[test]
    fn log_inputs_match_cpu_view_transform() {
        use crate::photo::{multiply_row1, view_transform};
        let Ok(gpu) = Gpu::new() else { return eprintln!("no GPU, skipped") };
        let scene: Vec<[f32; 3]> = [0.0f32, 0.02, 0.09, 0.18, 0.36, 0.9, 2.0]
            .iter()
            .map(|&g| [g; 3])
            .chain([[0.30, 0.12, 0.05], [0.05, 0.20, 0.40], [0.10, 0.25, 0.08]])
            .collect();
        for profile in Profile::ALL.into_iter().filter(|p| p.is_log()) {
            let pixels = scene
                .iter()
                .map(|&c| {
                    let [r, g, b] = multiply_row1(&profile.from_rec709(), &c).map(|v| profile.encode(v));
                    [r, g, b, 1.0]
                })
                .collect();
            let photo = Photo { width: scene.len() as u32, height: 1, pixels };
            let mut grade = Grade::default();
            grade.params.set_input(profile);
            let out = gpu.render(&gpu.upload(&photo), &grade, None, 0.0);
            for (i, c) in scene.iter().enumerate() {
                let want = view_transform(*c).map(|v| (v * 255.0).round() as i32);
                let got = [0, 1, 2].map(|k| out[i * 4 + k] as i32);
                let ok = (0..3).all(|k| (got[k] - want[k]).abs() <= 2);
                assert!(ok, "{profile:?} {c:?}: gpu {got:?} cpu {want:?}");
            }
        }
    }
}
