//! GPU engine (wgpu → Vulkan / Metal / DX12, native only) and its worker thread.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::sync::{Arc, mpsc};

use wgpu::util::DeviceExt;

use crate::grade::{Grade, Params};
use crate::lut::Lut3d;
use crate::photo::{Photo, THUMB_MAX};
use crate::scopes::Scopes;

pub struct Gpu {
    device: wgpu::Device,
    queue: wgpu::Queue,
    preview: wgpu::ComputePipeline,
    apply: wgpu::ComputePipeline,
    bake: wgpu::ComputePipeline,
    pub name: String,
}

pub struct GpuPhoto {
    buf: wgpu::Buffer,
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
        Ok(Self {
            preview: pipeline("preview"),
            apply: pipeline("apply_lut"),
            bake: pipeline("bake"),
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

    pub fn upload(&self, photo: &Photo) -> GpuPhoto {
        GpuPhoto { buf: self.storage(bytemuck::cast_slice(&photo.pixels)), width: photo.width, height: photo.height }
    }

    /// Dispatches `pipeline` over `count` items and reads back `out_bytes` from
    /// a fresh output buffer bound at `out_binding`.
    fn run(
        &self,
        pipeline: &wgpu::ComputePipeline,
        inputs: &[(u32, &wgpu::Buffer)],
        out_binding: u32,
        count: u32,
        out_bytes: u64,
    ) -> Vec<u8> {
        let out = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: out_bytes,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        let staging = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: out_bytes,
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let mut entries: Vec<_> = inputs
            .iter()
            .map(|(b, buf)| wgpu::BindGroupEntry { binding: *b, resource: buf.as_entire_binding() })
            .collect();
        entries.push(wgpu::BindGroupEntry { binding: out_binding, resource: out.as_entire_binding() });
        let bind = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &pipeline.get_bind_group_layout(0),
            entries: &entries,
        });
        let mut enc = self.device.create_command_encoder(&Default::default());
        {
            let mut pass = enc.begin_compute_pass(&Default::default());
            pass.set_pipeline(pipeline);
            pass.set_bind_group(0, &bind, &[]);
            pass.dispatch_workgroups(count.div_ceil(WORKGROUP), 1, 1);
        }
        enc.copy_buffer_to_buffer(&out, 0, &staging, 0, out_bytes);
        self.queue.submit([enc.finish()]);
        let slice = staging.slice(..);
        slice.map_async(wgpu::MapMode::Read, |r| r.expect("GPU readback failed"));
        self.device.poll(wgpu::PollType::wait_indefinitely()).expect("GPU device lost");
        let bytes = slice.get_mapped_range().expect("map GPU readback").to_vec();
        staging.unmap();
        bytes
    }

    fn params(&self, mut p: Params, w: u32, h: u32, lut_size: u32, split: f32) -> wgpu::Buffer {
        (p.width, p.height, p.lut_size, p.split) = (w, h, lut_size, split);
        self.storage(bytemuck::bytes_of(&p))
    }

    /// Graded preview as RGBA8.
    pub fn render(&self, photo: &GpuPhoto, grade: &Grade, split: f32) -> Vec<u8> {
        let params = self.params(grade.params, photo.width, photo.height, 0, split);
        let curves = self.storage(bytemuck::cast_slice(&grade.curve_samples()));
        let n = photo.width * photo.height;
        self.run(&self.preview, &[(0, &params), (1, &curves), (2, &photo.buf)], 3, n, n as u64 * 4)
    }

    pub fn upload_lut(&self, lut: &Lut3d) -> GpuLut {
        let data: Vec<[f32; 4]> = lut.data.iter().map(|&[r, g, b]| [r, g, b, 1.0]).collect();
        GpuLut { buf: self.storage(bytemuck::cast_slice(&data)), size: lut.size as u32 }
    }

    /// Photo seen through an external LUT (tetrahedral), as RGBA8.
    pub fn apply_lut(&self, photo: &GpuPhoto, lut: &GpuLut, split: f32) -> Vec<u8> {
        let params = self.params(Params::default(), photo.width, photo.height, lut.size, split);
        let n = photo.width * photo.height;
        self.run(&self.apply, &[(0, &params), (2, &photo.buf), (5, &lut.buf)], 3, n, n as u64 * 4)
    }

    /// Evaluates the grade exactly on every lattice point of an N³ cube.
    pub fn bake(&self, grade: &Grade, size: usize, title: String) -> Lut3d {
        let n = (size * size * size) as u32;
        let params = self.params(grade.params, 0, 0, size as u32, 0.0);
        let curves = self.storage(bytemuck::cast_slice(&grade.curve_samples()));
        let bytes = self.run(&self.bake, &[(0, &params), (1, &curves)], 4, n, n as u64 * 16);
        let data = bytemuck::cast_slice::<u8, [f32; 4]>(&bytes).iter().map(|&[r, g, b, _]| [r, g, b]).collect();
        Lut3d { title, size, data }
    }
}

// ------------------------------------------------------------------ worker

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Grade,
    Compare,
}

pub enum Job {
    SetPhoto(Photo),
    Render(Grade, f32),
    ApplyLut(Arc<Lut3d>, f32),
    /// Thumbnails of every LUT; `generation` lets the UI drop stale results.
    Gallery {
        generation: u64,
        luts: Vec<Arc<Lut3d>>,
    },
    Export {
        grade: Grade,
        size: usize,
        title: String,
        path: PathBuf,
    },
}

#[derive(PartialEq)]
enum Slot {
    Grade,
    Compare,
    Gallery,
}

impl Job {
    /// Jobs whose newer instance makes older ones pointless.
    fn slot(&self) -> Option<Slot> {
        match self {
            Job::Render(..) => Some(Slot::Grade),
            Job::ApplyLut(..) => Some(Slot::Compare),
            Job::Gallery { .. } => Some(Slot::Gallery),
            _ => None,
        }
    }
}

#[derive(Debug)]
pub enum Done {
    Ready(String),
    Failed(String),
    Frame { view: View, width: u32, height: u32, rgba: Vec<u8>, scopes: Option<Scopes> },
    Thumb { generation: u64, index: usize, width: u32, height: u32, rgba: Vec<u8> },
    Exported(Result<PathBuf, String>),
}

#[derive(Default)]
struct Worker {
    photo: Option<GpuPhoto>,
    thumb: Option<GpuPhoto>,
    /// Last applied LUT stays on the GPU so split drags don't re-upload it.
    lut: Option<(Arc<Lut3d>, GpuLut)>,
}

impl Worker {
    fn handle(&mut self, gpu: &Gpu, job: Job, notify: &impl Fn(Done)) {
        match job {
            Job::SetPhoto(p) => {
                self.thumb = Some(gpu.upload(&p.downscaled(THUMB_MAX)));
                self.photo = Some(gpu.upload(&p));
            }
            Job::Render(grade, split) => {
                let Some(ph) = &self.photo else { return };
                let rgba = gpu.render(ph, &grade, split);
                let scopes = Some(Scopes::compute(&rgba, ph.width, ph.height, split));
                notify(Done::Frame { view: View::Grade, width: ph.width, height: ph.height, rgba, scopes });
            }
            Job::ApplyLut(lut, split) => {
                let Some(ph) = &self.photo else { return };
                if !self.lut.as_ref().is_some_and(|(l, _)| Arc::ptr_eq(l, &lut)) {
                    self.lut = Some((lut.clone(), gpu.upload_lut(&lut)));
                }
                let rgba = gpu.apply_lut(ph, &self.lut.as_ref().expect("uploaded above").1, split);
                notify(Done::Frame { view: View::Compare, width: ph.width, height: ph.height, rgba, scopes: None });
            }
            Job::Gallery { generation, luts } => {
                let Some(th) = &self.thumb else { return };
                for (index, lut) in luts.iter().enumerate() {
                    let rgba = gpu.apply_lut(th, &gpu.upload_lut(lut), 0.0);
                    notify(Done::Thumb { generation, index, width: th.width, height: th.height, rgba });
                }
            }
            Job::Export { grade, size, title, path } => {
                let cube = gpu.bake(&grade, size, title).to_cube();
                notify(Done::Exported(std::fs::write(&path, cube).map(|_| path).map_err(|e| e.to_string())));
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
                let mut jobs: Vec<Job> = std::iter::once(first).chain(rx.try_iter()).collect();
                let superseded = |i: usize, jobs: &[Job]| {
                    jobs[i].slot().is_some_and(|s| jobs[i + 1..].iter().any(|j| j.slot().as_ref() == Some(&s)))
                };
                let keep: Vec<bool> = (0..jobs.len()).map(|i| !superseded(i, &jobs)).collect();
                // Gallery passes are slow; run them after this batch's previews.
                let mut order: Vec<usize> = (0..jobs.len()).filter(|&i| keep[i]).collect();
                order.sort_by_key(|&i| jobs[i].slot() == Some(Slot::Gallery));
                let mut taken: Vec<Option<Job>> = jobs.drain(..).map(Some).collect();
                for i in order {
                    let job = taken[i].take().expect("each job runs once");
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
}
