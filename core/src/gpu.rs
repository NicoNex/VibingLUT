//! GPU engine (wgpu → Vulkan / Metal / DX12, native only) and its worker thread.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::sync::{Arc, mpsc};

use wgpu::util::DeviceExt;

use crate::grade::{CURVE_RES, Grade, Params};
use crate::lab::{self, Blend, Layer};
use crate::lut::{Format, Lut3d};
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
    pub name: String,
}

/// A photo resident on the GPU together with its output/readback buffers,
/// so interactive frames allocate nothing.
pub struct GpuPhoto {
    src: wgpu::Buffer,
    out: wgpu::Buffer,
    staging: wgpu::Buffer,
    preview_bind: wgpu::BindGroup,
    width: u32,
    height: u32,
}

pub struct GpuLut {
    buf: wgpu::Buffer,
    size: u32,
}

const WORKGROUP: u32 = 256;

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
            curves: uniform(4 * CURVE_RES * 4),
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
        let preview_bind = self.bind(&self.preview, &[(0, &self.params), (1, &self.curves), (2, &src), (3, &out)]);
        GpuPhoto { src, out, staging, preview_bind, width: photo.width, height: photo.height }
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

    /// Graded preview as RGBA8.
    pub fn render(&self, photo: &GpuPhoto, grade: &Grade, split: f32) -> Vec<u8> {
        self.write_params(grade.params, photo.width, photo.height, 0, split);
        self.queue.write_buffer(&self.curves, 0, bytemuck::cast_slice(&grade.curve_samples()));
        self.run(&self.preview, &photo.preview_bind, photo.width * photo.height, &photo.out, &photo.staging)
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

// ------------------------------------------------------------------ worker

pub enum Job {
    SetPhoto(Photo),
    /// Uploads the comparison LUTs once; later jobs refer to them by index.
    SetLuts(Vec<Arc<Lut3d>>),
    Render(Grade, f32),
    ApplyLut(usize, f32),
    /// Thumbnails of every LUT; `generation` lets the UI drop stale results.
    Gallery(u64),
    /// Preview of a LUT Lab stack on the current photo.
    Lab {
        input: Profile,
        layers: Vec<Layer>,
        blend: Blend,
        split: f32,
    },
    ExportLab {
        input: Profile,
        layers: Vec<Layer>,
        blend: Blend,
        size: usize,
        format: Format,
        title: String,
        path: PathBuf,
    },
    Export {
        grade: Grade,
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
}

impl Job {
    /// Jobs whose newer instance makes older ones pointless.
    fn slot(&self) -> Option<Slot> {
        match self {
            Job::Render(..) => Some(Slot::Grade),
            Job::ApplyLut(..) => Some(Slot::Compare),
            Job::Gallery(..) => Some(Slot::Gallery),
            Job::Lab { .. } => Some(Slot::Lab),
            _ => None,
        }
    }
}

#[derive(Debug)]
pub enum Done {
    Ready(String),
    Failed(String),
    Graded { width: u32, height: u32, rgba: Vec<u8>, scopes: Scopes },
    Compared { width: u32, height: u32, rgba: Vec<u8> },
    Lab { width: u32, height: u32, rgba: Vec<u8> },
    Thumb { generation: u64, index: usize, width: u32, height: u32, rgba: Vec<u8> },
    Exported(Result<PathBuf, String>),
}

#[derive(Default)]
struct Worker {
    photo: Option<GpuPhoto>,
    thumb: Option<GpuPhoto>,
    luts: Vec<GpuLut>,
    /// Last grade baked for the Lab, reused until the grade changes.
    grade_lut: std::cell::RefCell<Option<(Grade, Arc<Lut3d>)>>,
}

impl Worker {
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
                self.thumb = Some(gpu.upload(&p.downscaled(THUMB_MAX)));
                self.photo = Some(gpu.upload(&p));
            }
            Job::SetLuts(luts) => self.luts = luts.iter().map(|l| gpu.upload_lut(l)).collect(),
            Job::Render(grade, split) => {
                let Some(ph) = &self.photo else { return };
                let rgba = gpu.render(ph, &grade, split);
                let scopes = Scopes::compute(&rgba, ph.width, ph.height, split);
                notify(Done::Graded { width: ph.width, height: ph.height, rgba, scopes });
            }
            Job::ApplyLut(index, split) => {
                let (Some(ph), Some(lut)) = (&self.photo, self.luts.get(index)) else { return };
                let rgba = gpu.apply_lut(ph, lut, split);
                notify(Done::Compared { width: ph.width, height: ph.height, rgba });
            }
            Job::Gallery(generation) => {
                let Some(th) = &self.thumb else { return };
                for (index, lut) in self.luts.iter().enumerate() {
                    let rgba = gpu.apply_lut(th, lut, 0.0);
                    notify(Done::Thumb { generation, index, width: th.width, height: th.height, rgba });
                }
            }
            Job::Lab { input, layers, blend, split } => {
                let Some(ph) = &self.photo else { return };
                // ponytail: 33³ preview lattice; exports compose at full size.
                let lut = lab::compose(input, &layers, blend, 33, &|g| self.bake_cached(gpu, g));
                let rgba = gpu.apply_lut(ph, &gpu.upload_lut(&lut), split);
                notify(Done::Lab { width: ph.width, height: ph.height, rgba });
            }
            Job::ExportLab { input, layers, blend, size, format, title, path } => {
                let lut = lab::compose(input, &layers, blend, size, &|g| self.bake_cached(gpu, g));
                notify(Done::Exported(Lut3d { title, format, ..lut }.save(&path).map(|_| path)));
            }
            Job::Export { grade, size, format, title, path } => {
                let lut = Lut3d { format, ..gpu.bake(&grade, size, title) };
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
            while let Ok(first) = rx.recv() {
                // Keep only the newest job per slot, preserving order otherwise.
                let mut batch: Vec<Job> = std::iter::once(first).chain(rx.try_iter()).collect();
                let mut seen = [false; 4];
                batch.reverse();
                batch.retain(|j| j.slot().is_none_or(|s| !std::mem::replace(&mut seen[s as usize], true)));
                batch.reverse();
                // Gallery passes are slow; run them after this batch's previews.
                let (gallery, rest): (Vec<Job>, Vec<Job>) =
                    batch.into_iter().partition(|j| j.slot() == Some(Slot::Gallery));
                for job in rest.into_iter().chain(gallery) {
                    // A device loss or validation error panics inside wgpu: report it instead of dying silently.
                    if catch_unwind(AssertUnwindSafe(|| worker.handle(&gpu, job, &notify))).is_err() {
                        return notify(Done::Failed("errore della GPU, riavvia l'applicazione".into()));
                    }
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
        grade.curves[0] = vec![(0.0, 0.05), (0.5, 0.55), (1.0, 0.95)];
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
        let direct = gpu.render(&gp, &grade, 0.0);
        let via_lut = gpu.apply_lut(&gp, &gpu.upload_lut(&gpu.bake(&grade, 65, "t".into())), 0.0);
        let diff: Vec<u8> = direct.iter().zip(&via_lut).map(|(a, b)| a.abs_diff(*b)).collect();
        let worst = *diff.iter().max().unwrap();
        let mean = diff.iter().map(|&d| d as f32).sum::<f32>() / diff.len() as f32;
        // ponytail: peak error comes from out-of-gamut clamping of strongly saturated darks.
        assert!(worst <= 8 && mean < 0.5, "65³ LUT vs direct grade: worst {worst}/255, mean {mean}");
    }

    /// Every log profile: CPU encode (gamut + OETF) followed by the GPU
    /// decode + view transform must reproduce the CPU view transform, so all
    /// profiles start from the same look and the decoders match the encoders.
    #[test]
    fn log_inputs_match_cpu_view_transform() {
        use crate::photo::{mat3, view_transform};
        use crate::profiles::Profile;
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
                    let [r, g, b] = mat3(&profile.from_rec709(), c).map(|v| profile.encode(v));
                    [r, g, b, 1.0]
                })
                .collect();
            let photo = Photo { width: scene.len() as u32, height: 1, pixels };
            let mut grade = Grade::default();
            grade.params.set_input(profile);
            let out = gpu.render(&gpu.upload(&photo), &grade, 0.0);
            for (i, c) in scene.iter().enumerate() {
                let want = view_transform(*c).map(|v| (v * 255.0).round() as i32);
                let got = [0, 1, 2].map(|k| out[i * 4 + k] as i32);
                let ok = (0..3).all(|k| (got[k] - want[k]).abs() <= 2);
                assert!(ok, "{profile:?} {c:?}: gpu {got:?} cpu {want:?}");
            }
        }
    }
}
