//! GPU develop pipeline (wgpu; Metal on macOS), verified against the CPU reference in `pe-pipeline`.
//!
//! Usage: create one [`Gpu`] per process, [`Gpu::upload`] a linear-RGB frame once (e.g. the reduced
//! preview from `pe_pipeline::prepare_preview`), then call [`Gpu::render`] for every settings change.
//! Only the uniform block and, when it changes, the curve table are sent per render.

use std::sync::Mutex;

use bytemuck::{Pod, Zeroable};
use pe_colour::{ColorSpace, REC2020_TO_XYZ_D65};
use pe_pipeline::{look, tone, wb, Settings, WhiteBalance};
use pe_raw::{Layout, RawFrame};
use wgpu::util::DeviceExt;

#[derive(Debug, thiserror::Error)]
pub enum GpuError {
    #[error("frame must be linear RGB (prepare a preview or demosaic first)")]
    NotLinear,
    #[error("camera colour matrix is singular")]
    SingularMatrix,
    #[error("frame too large for this GPU ({0} bytes)")]
    TooLarge(u64),
    #[error("GPU readback failed: {0}")]
    Readback(String),
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Params {
    wb: [f32; 4],
    m: [[f32; 4]; 3],
    o: [[f32; 4]; 3],
    out_luma: [f32; 4],
    tone: [f32; 4],
    local: [f32; 4],
    dims: [u32; 4],
    dims2: [u32; 4],
    dims3: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct LookUniform {
    flags: [f32; 4],
    lms: [[f32; 4]; 3],
    rgb: [[f32; 4]; 3],
    centres: [f32; 8],
    band_hue: [f32; 8],
    band_sat: [f32; 8],
    band_lum: [f32; 8],
    band_bw: [f32; 8],
    grade: [[f32; 4]; 4],
    shape: [f32; 4],
}

impl LookUniform {
    fn new(p: &look::LookParams) -> Self {
        let g = |r: [f32; 3]| [r[0], r[1], r[2], 0.0];
        Self {
            flags: [
                p.enabled as u8 as f32,
                p.monochrome as u8 as f32,
                p.saturation_gain,
                p.vibrance,
            ],
            lms: p.lms_from_rgb.map(row4),
            rgb: p.rgb_from_lms.map(row4),
            centres: p.centres,
            band_hue: p.band_hue,
            band_sat: p.band_sat,
            band_lum: p.band_lum,
            band_bw: p.band_bw,
            grade: p.grade.map(g),
            shape: [p.grade_width, p.grade_balance, 0.0, 0.0],
        }
    }
}

const ENTRY_POINTS: [&str; 7] = [
    "scene_pass",
    "downscale_pass",
    "blur_h_moments",
    "blur_v",
    "coeff_pass",
    "blur_h_coeffs",
    "final_pass",
];

/// A GPU device with the develop pipelines compiled.
pub struct Gpu {
    device: wgpu::Device,
    queue: wgpu::Queue,
    layout: wgpu::BindGroupLayout,
    pipelines: Vec<wgpu::ComputePipeline>,
    name: String,
    warmed: std::sync::atomic::AtomicBool,
}

/// A frame resident on the GPU, with scratch buffers for rendering it.
pub struct GpuFrame {
    width: u32,
    height: u32,
    small: (u32, u32),
    factor: u32,
    radius: u32,
    orientation: u8,
    header: RawFrame,
    auto_wb: [f32; 3],
    params: wgpu::Buffer,
    look: wgpu::Buffer,
    lut: wgpu::Buffer,
    lut_state: Mutex<Option<Vec<[f32; 2]>>>,
    out: wgpu::Buffer,
    readback: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    /// Renders of one frame share its buffers, so they run one at a time.
    busy: Mutex<()>,
}

impl GpuFrame {
    pub fn width(&self) -> u32 {
        self.width
    }
    pub fn height(&self) -> u32 {
        self.height
    }
}

fn workgroups(n: u32) -> (u32, u32) {
    let groups = n.div_ceil(256);
    if groups <= 65_535 {
        (groups.max(1), 1)
    } else {
        (65_535, groups.div_ceil(65_535))
    }
}

fn row4(r: [f32; 3]) -> [f32; 4] {
    [r[0], r[1], r[2], 0.0]
}

impl Gpu {
    /// Open the default high-performance adapter. `None` when no usable GPU is present (e.g. CI), or
    /// when the environment variable `PE_NO_GPU` is set.
    pub fn new() -> Option<Gpu> {
        if std::env::var_os("PE_NO_GPU").is_some() {
            return None;
        }
        pollster::block_on(Self::init())
    }

    async fn init() -> Option<Gpu> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                ..Default::default()
            })
            .await
            .ok()?;
        let info = adapter.get_info();
        if info.device_type == wgpu::DeviceType::Cpu {
            return None; // a software rasteriser is slower than our CPU path
        }
        let limits = adapter.limits();
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("pe-gpu"),
                required_limits: wgpu::Limits {
                    max_storage_buffer_binding_size: limits.max_storage_buffer_binding_size,
                    max_buffer_size: limits.max_buffer_size,
                    max_storage_buffers_per_shader_stage: 9,
                    ..wgpu::Limits::defaults()
                },
                ..Default::default()
            })
            .await
            .ok()?;
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("develop"),
            source: wgpu::ShaderSource::Wgsl(include_str!("develop.wgsl").into()),
        });
        let buffer = |binding: u32, ty: wgpu::BufferBindingType| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::COMPUTE,
            ty: wgpu::BindingType::Buffer {
                ty,
                has_dynamic_offset: false,
                min_binding_size: None,
            },
            count: None,
        };
        let storage = |read_only| wgpu::BufferBindingType::Storage { read_only };
        let entries: Vec<_> = (0..=10)
            .map(|b| match b {
                0 | 10 => buffer(b, wgpu::BufferBindingType::Uniform),
                1 | 8 => buffer(b, storage(true)),
                _ => buffer(b, storage(false)),
            })
            .collect();
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("develop"),
            entries: &entries,
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("develop"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipelines = ENTRY_POINTS
            .iter()
            .map(|ep| {
                device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                    label: Some(ep),
                    layout: Some(&pipeline_layout),
                    module: &module,
                    entry_point: Some(ep),
                    compilation_options: Default::default(),
                    cache: None,
                })
            })
            .collect();
        Some(Gpu {
            device,
            queue,
            layout,
            pipelines,
            name: format!("{} ({:?})", info.name, info.backend),
            warmed: std::sync::atomic::AtomicBool::new(false),
        })
    }

    /// Adapter name and backend, e.g. "Apple M1 (Metal)".
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Upload a linear-RGB frame and allocate everything needed to render it.
    pub fn upload(&self, frame: &RawFrame) -> Result<GpuFrame, GpuError> {
        if !matches!(frame.layout, Layout::LinearRgb) {
            return Err(GpuError::NotLinear);
        }
        let (w, h) = (frame.width, frame.height);
        let (radius, factor) = tone::local_tone_geometry(w, h);
        let (sw, sh) = (w.div_ceil(factor), h.div_ceil(factor));
        let n = (w * h) as u64;
        let rgb_bytes = n * 12;
        if rgb_bytes > self.device.limits().max_storage_buffer_binding_size {
            return Err(GpuError::TooLarge(rgb_bytes));
        }
        let d = &self.device;
        let storage = |label: &str, size: u64| {
            d.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size: size.max(16),
                usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
                mapped_at_creation: false,
            })
        };
        let src = d.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("src"),
            contents: bytemuck::cast_slice(&frame.data),
            usage: wgpu::BufferUsages::STORAGE,
        });
        let scene = storage("scene", rgb_bytes);
        let logy = storage("logy", n * 4);
        let small_n = (sw * sh) as u64;
        let small = storage("small", small_n * 4);
        let tmp = storage("tmp", small_n * 8);
        let stats = storage("stats", small_n * 8);
        let ab = storage("ab", small_n * 8);
        let lut = d.create_buffer(&wgpu::BufferDescriptor {
            label: Some("lut"),
            size: 4096 * 4,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let out = storage("out", n * 4);
        let readback = d.create_buffer(&wgpu::BufferDescriptor {
            label: Some("readback"),
            size: n * 4,
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let params = d.create_buffer(&wgpu::BufferDescriptor {
            label: Some("params"),
            size: std::mem::size_of::<Params>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let look = d.create_buffer(&wgpu::BufferDescriptor {
            label: Some("look"),
            size: std::mem::size_of::<LookUniform>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let bufs = [
            &params, &src, &scene, &logy, &small, &tmp, &stats, &ab, &lut, &out, &look,
        ];
        let entries: Vec<_> = bufs
            .iter()
            .enumerate()
            .map(|(i, b)| wgpu::BindGroupEntry {
                binding: i as u32,
                resource: b.as_entire_binding(),
            })
            .collect();
        let bind_group = d.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("develop"),
            layout: &self.layout,
            entries: &entries,
        });
        Ok(GpuFrame {
            width: w as u32,
            height: h as u32,
            small: (sw as u32, sh as u32),
            factor: factor as u32,
            radius: (radius / factor).max(1) as u32,
            orientation: frame.orientation,
            header: frame.clone_header(),
            // Grey-world needs the pixels; compute it now so no CPU copy has to be kept.
            auto_wb: wb::multipliers(frame, &WhiteBalance::Auto, &frame.data),
            params,
            look,
            lut,
            lut_state: Mutex::new(None),
            out,
            readback,
            bind_group,
            busy: Mutex::new(()),
        })
        .inspect(|f| {
            // The first dispatch of each pipeline pays a one-off driver cost (~70 ms on an M1). Pay it
            // here, during loading, rather than on the first slider movement.
            if !self.warmed.swap(true, std::sync::atomic::Ordering::Relaxed) {
                let s = Settings {
                    shadows: 1.0,
                    ..Default::default()
                };
                let _ = self.render(f, &s);
            }
        })
    }

    /// Render with `settings` to sRGB RGBA8, oriented for display. Returns (width, height, pixels).
    pub fn render(
        &self,
        f: &GpuFrame,
        settings: &Settings,
    ) -> Result<(u32, u32, Vec<u8>), GpuError> {
        let _busy = f.busy.lock().unwrap();
        let s = settings.clone().clamped();
        let wbm = match s.white_balance {
            WhiteBalance::Auto => f.auto_wb,
            ref mode => wb::multipliers(&f.header, mode, &[]),
        };
        let to_work = pe_colour::camera_to_space(&f.header.xyz_to_cam, &REC2020_TO_XYZ_D65)
            .ok_or(GpuError::SingularMatrix)?;
        let out_m = ColorSpace::Srgb.from_space(&REC2020_TO_XYZ_D65);
        let tp = tone::ToneParams::new(s.contrast, s.whites, s.blacks);
        let local = s.shadows != 0.0 || s.highlights != 0.0;
        let (sev, hev) = tone::local_tone_amounts(s.shadows, s.highlights);
        let orientation = if s.apply_orientation {
            f.orientation
        } else {
            1
        };
        let (ow, oh) = if (5..=8).contains(&orientation) {
            (f.height, f.width)
        } else {
            (f.width, f.height)
        };

        // Curve table: upload only when the curve changes.
        let curve = tone::CurveLut::new(&s.tone_curve);
        {
            let mut state = f.lut_state.lock().unwrap();
            if state.as_deref() != Some(&s.tone_curve[..]) {
                if let Some(c) = &curve {
                    self.queue
                        .write_buffer(&f.lut, 0, bytemuck::cast_slice(c.table()));
                }
                *state = Some(s.tone_curve.clone());
            }
        }

        let luma = ColorSpace::Srgb.luma();
        let p = Params {
            wb: [wbm[0], wbm[1], wbm[2], s.exposure.exp2()],
            m: [row4(to_work[0]), row4(to_work[1]), row4(to_work[2])],
            o: [row4(out_m[0]), row4(out_m[1]), row4(out_m[2])],
            out_luma: [luma[0], luma[1], luma[2], 0.0],
            tone: [
                tp.contrast_power,
                tp.white_norm,
                tp.black,
                tone::MID_GREY.log2(),
            ],
            local: [sev, hev, tone::LOCAL_EPS, if local { 1.0 } else { 0.0 }],
            dims: [f.width, f.height, f.small.0, f.small.1],
            dims2: [
                f.factor,
                f.radius,
                orientation as u32,
                curve.is_some() as u32,
            ],
            dims3: [ow, oh, 0, 0],
        };
        self.queue
            .write_buffer(&f.params, 0, bytemuck::bytes_of(&p));
        let lu = LookUniform::new(&look::LookParams::new(&s));
        self.queue.write_buffer(&f.look, 0, bytemuck::bytes_of(&lu));

        let mut enc = self.device.create_command_encoder(&Default::default());
        {
            let mut pass = enc.begin_compute_pass(&Default::default());
            pass.set_bind_group(0, &f.bind_group, &[]);
            let full = workgroups(f.width * f.height);
            let small = workgroups(f.small.0 * f.small.1);
            let mut run = |i: usize, g: (u32, u32)| {
                pass.set_pipeline(&self.pipelines[i]);
                pass.dispatch_workgroups(g.0, g.1, 1);
            };
            run(0, full);
            if local {
                run(1, small); // downscale log luminance
                run(2, small); // horizontal means of I, I^2
                run(3, small); // vertical -> stats = (mean I, mean I^2)
                run(4, small); // coefficients a, b
                run(5, small); // horizontal means of a, b
                run(3, small); // vertical -> stats = (mean a, mean b)
            }
            run(6, workgroups(ow * oh));
        }
        let bytes = (ow * oh * 4) as u64;
        enc.copy_buffer_to_buffer(&f.out, 0, &f.readback, 0, bytes);
        self.queue.submit([enc.finish()]);

        let slice = f.readback.slice(..bytes);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        self.device
            .poll(wgpu::PollType::wait_indefinitely())
            .map_err(|e| GpuError::Readback(e.to_string()))?;
        rx.recv()
            .map_err(|e| GpuError::Readback(e.to_string()))?
            .map_err(|e| GpuError::Readback(e.to_string()))?;
        let pixels = slice
            .get_mapped_range()
            .map_err(|e| GpuError::Readback(e.to_string()))?
            .to_vec();
        f.readback.unmap();
        Ok((ow, oh, pixels))
    }
}
