//! GPU develop pipeline (wgpu; Metal on macOS), verified against the CPU reference in `pe-pipeline`.
//!
//! Usage: create one [`Gpu`] per process, [`Gpu::upload`] a linear-RGB frame once (e.g. the reduced
//! preview from `pe_pipeline::prepare_preview`), then call [`Gpu::render`] for every settings change.
//! Per render only small uniform blocks (and, when it changes, the curve table) are sent.
//!
//! Intermediate images live in one storage buffer ("pool") of vec4 planes: three at full size (scene,
//! texture blur and its horizontal pass) and six on the reduced grid used by the guided filters. Each
//! render is a list of passes; generic kernels get their planes and radii from a per-pass uniform
//! record selected with a dynamic offset.

use std::sync::Mutex;

use bytemuck::{Pod, Zeroable};
use pe_colour::{ColorSpace, REC2020_TO_XYZ_D65};
use pe_pipeline::masks::{Combine, MaskParams, PreparedShape};
use pe_pipeline::{
    detail, effects, geometry, local, look, masks, region, tone, wb, Settings, WhiteBalance,
};
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
    haze: [f32; 4],
    haze2: [f32; 4],
    offs: [u32; 4],
    flags: [u32; 4],
    geo0: [f32; 4],
    geo1: [f32; 4],
    geo2: [f32; 4],
    geo3: [f32; 4],
    geo4: [u32; 4],
    /// Whole sensor frame size and this frame's origin in it.
    full: [u32; 4],
    /// Output origin and whole-picture output size.
    outr: [u32; 4],
    /// Local-detail target planes: noise reduction / sharpness, dehaze; unused, unused.
    offs2: [u32; 4],
    /// Lens profile (pe_pipeline::geometry): distortion d0..d3; d4, radius scale, fill zoom, on;
    /// red TCA, vignetting radius scale; blue TCA, vignetting amount; vignetting k1..k3.
    lens0: [f32; 4],
    lens1: [f32; 4],
    lens2: [f32; 4],
    lens3: [f32; 4],
    lens4: [f32; 4],
}

/// Per-pass record for the generic kernels (matches `struct Pass` in the shader).
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable, Default)]
struct PassUniform {
    src: u32,
    dst: u32,
    aux: u32,
    mode: u32,
    w: u32,
    h: u32,
    sw: u32,
    sh: u32,
    r: u32,
    pad: [u32; 3],
    k: [f32; 4],
}

/// Mask buffer size in vec4 records: an 8-record header plus 31 per mask.
const MASK_RECORDS: usize = 8 + masks::MAX_MASKS * 31;

/// Pack prepared masks for the shader: records (see develop.wgsl) and raster / colour data.
fn pack_masks(mp: &MaskParams) -> (Vec<[f32; 4]>, Vec<f32>) {
    let mut rec = vec![[0.0f32; 4]; MASK_RECORDS];
    let mut data: Vec<f32> = Vec::new();
    rec[0] = [
        mp.masks.len() as f32,
        mp.needs_display as u8 as f32,
        mp.needs_saturation as u8 as f32,
        0.0,
    ];
    rec[1] = [mp.frame_w, mp.frame_h, 0.0, 0.0];
    for r in 0..3 {
        rec[2 + r] = row4(mp.lms_from_rgb[r]);
        rec[5 + r] = row4(mp.rgb_from_lms[r]);
    }
    let mut rasters: Vec<(*const masks::Raster, u32)> = Vec::new();
    for (i, m) in mp.masks.iter().enumerate() {
        let b = 8 + i * 31;
        let a = &m.adjust;
        rec[b] = [
            m.components.len() as f32,
            m.amount,
            a.wb.is_some() as u8 as f32,
            a.exposure,
        ];
        rec[b + 1] = [a.contrast, a.shadows, a.highlights, a.whites];
        rec[b + 2] = [a.blacks, a.clarity, a.texture, a.saturation];
        let wbm =
            a.wb.unwrap_or([[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]]);
        for r in 0..3 {
            rec[b + 3 + r] = row4(wbm[r]);
        }
        rec[b + 6] = [a.noise, a.dehaze, a.sharpness, 0.0];
        for (k, c) in m.components.iter().enumerate() {
            let at = b + 7 + 3 * k;
            let mode = match c.mode {
                Combine::Add => 0.0,
                Combine::Subtract => 1.0,
                Combine::Intersect => 2.0,
            };
            let inv = c.invert as u8 as f32;
            let (t, c1, c2) = match &c.shape {
                PreparedShape::Linear { p0, d, inv_len2 } => {
                    (0.0, [p0[0], p0[1], d[0], d[1]], [*inv_len2, 0.0, 0.0, 0.0])
                }
                PreparedShape::Radial {
                    c,
                    cos,
                    sin,
                    inv_rx,
                    inv_ry,
                    inner,
                } => (
                    1.0,
                    [c[0], c[1], *cos, *sin],
                    [*inv_rx, *inv_ry, *inner, 0.0],
                ),
                PreparedShape::Raster(r) => {
                    let ptr = std::sync::Arc::as_ptr(r);
                    let off = match rasters.iter().find(|(p, _)| *p == ptr) {
                        Some((_, o)) => *o,
                        None => {
                            let o = data.len() as u32;
                            data.extend_from_slice(&r.data);
                            rasters.push((ptr, o));
                            o
                        }
                    };
                    (
                        2.0,
                        [off as f32, r.width as f32, r.height as f32, 0.0],
                        [0.0; 4],
                    )
                }
                PreparedShape::Luminance { lo, hi, soft } => {
                    (3.0, [*lo, *hi, *soft, 0.0], [0.0; 4])
                }
                PreparedShape::Color { colors, tol } => {
                    let off = data.len() as f32;
                    for q in colors {
                        data.extend_from_slice(q);
                    }
                    (4.0, [off, colors.len() as f32, *tol, 0.0], [0.0; 4])
                }
                PreparedShape::Nothing => (5.0, [0.0; 4], [0.0; 4]),
            };
            rec[at] = [t, mode, inv, 0.0];
            rec[at + 1] = c1;
            rec[at + 2] = c2;
        }
    }
    if data.is_empty() {
        data.push(0.0);
    }
    (rec, data)
}

/// Records are placed at this stride so each can be bound with a dynamic offset.
const PASS_STRIDE: u64 = 256;
const MAX_PASSES: u64 = 64;

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
    fx1: [f32; 4],
    fx2: [f32; 4],
    fx3: [f32; 4],
}

impl LookUniform {
    fn new(p: &look::LookParams, e: &effects::EffectsParams) -> Self {
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
            fx1: [e.amount, e.inner, e.width, e.highlights],
            fx2: [e.sx, e.sy, e.p, e.vignette as u8 as f32],
            fx3: [e.grain_amount, e.cell, e.roughness, e.grain as u8 as f32],
        }
    }
}

#[derive(Clone, Copy)]
enum Kernel {
    Scene,
    Downscale,
    MinFilter,
    DehazeInput,
    Box,
    Coeffs,
    DehazeApply,
    Final,
    ToOpponent,
    FromOpponent,
    AddPlanes,
    Gauss,
    Sharpen,
    AtrousLevel,
    Display,
    Geometry,
    SharpTarget,
    DehazeTarget,
}

const ENTRY_POINTS: [&str; 18] = [
    "scene_pass",
    "downscale",
    "min_filter",
    "dehaze_input",
    "box",
    "coeffs",
    "dehaze_apply",
    "final_pass",
    "to_opponent",
    "from_opponent",
    "add_planes",
    "gauss",
    "sharpen",
    "atrous_level",
    "display_pass",
    "geometry_pass",
    "sharp_target",
    "dehaze_target",
];

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct DemosaicUniform {
    w: u32,
    h: u32,
    cfa_w: u32,
    cfa_h: u32,
    pattern: [u32; 36],
}

/// A GPU device with the develop pipelines compiled.
pub struct Gpu {
    demosaic_layout: wgpu::BindGroupLayout,
    demosaic_pipelines: Vec<wgpu::ComputePipeline>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    layout: wgpu::BindGroupLayout,
    pipelines: Vec<wgpu::ComputePipeline>,
    name: String,
    warmed: std::sync::atomic::AtomicBool,
}

/// Offsets (in vec4 elements) of the planes in a frame's pool.
#[derive(Clone, Copy)]
struct Planes {
    scene: u32,
    tex_h: u32,
    tex: u32,
    s: [u32; 3],
    /// Detail stage: opponent plane and wavelet accumulator (full size).
    a: u32,
    acc: u32,
    /// Masked noise reduction target (rgb) with the sharpness gain in .w; masked dehaze target.
    e1: u32,
    e2: u32,
    /// Full-strength dehaze coefficients (masked dehaze).
    dz2: u32,
    sh: u32,
    cl: u32,
    dz: u32,
}

/// A frame resident on the GPU, with scratch buffers for rendering it.
pub struct GpuFrame {
    width: u32,
    height: u32,
    geo: local::Geometry,
    /// Where this frame sits in the whole picture.
    ctx: region::Ctx,
    planes: Planes,
    header: RawFrame,
    auto_wb: [f32; 3],
    /// Camera RGB, for the airlight and noise estimates (computed on the CPU from sampled pixels).
    host: Vec<f32>,
    scale: f32,
    /// Noise estimate cached for the scene transform it was made with.
    noise: Mutex<Option<([u32; 4], [f32; 3])>>,
    params: wgpu::Buffer,
    look: wgpu::Buffer,
    passes: wgpu::Buffer,
    src: wgpu::Buffer,
    pool: wgpu::Buffer,
    lut: wgpu::Buffer,
    lut_state: Mutex<Option<Vec<[f32; 2]>>>,
    out: wgpu::Buffer,
    readback: wgpu::Buffer,
    /// Packed mask records (fixed size) and their raster / colour data (grows as needed).
    masks: wgpu::Buffer,
    mask_data: Mutex<wgpu::Buffer>,
    bind_group: Mutex<wgpu::BindGroup>,
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
        let buffer =
            |binding: u32, ty: wgpu::BufferBindingType, dynamic: bool| wgpu::BindGroupLayoutEntry {
                binding,
                visibility: wgpu::ShaderStages::COMPUTE,
                ty: wgpu::BindingType::Buffer {
                    ty,
                    has_dynamic_offset: dynamic,
                    min_binding_size: None,
                },
                count: None,
            };
        let storage = |read_only| wgpu::BufferBindingType::Storage { read_only };
        let uniform = wgpu::BufferBindingType::Uniform;
        let entries = [
            buffer(0, uniform, false),
            buffer(1, storage(true), false),
            buffer(2, storage(false), false),
            buffer(3, storage(true), false),
            buffer(4, storage(false), false),
            buffer(5, uniform, false),
            buffer(6, uniform, true),
            buffer(7, storage(true), false),
            buffer(8, storage(true), false),
        ];
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
        // Demosaic pipelines (their own bindings: uniform, mosaic, green, rgb, diff).
        let dmodule = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("demosaic"),
            source: wgpu::ShaderSource::Wgsl(include_str!("demosaic.wgsl").into()),
        });
        let dentries = [
            buffer(0, uniform, false),
            buffer(1, storage(true), false),
            buffer(2, storage(false), false),
            buffer(3, storage(false), false),
            buffer(4, storage(false), false),
        ];
        let demosaic_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("demosaic"),
            entries: &dentries,
        });
        let dpl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("demosaic"),
            bind_group_layouts: &[Some(&demosaic_layout)],
            immediate_size: 0,
        });
        let demosaic_pipelines = ["green_pass", "colour_pass", "median_pass"]
            .iter()
            .map(|ep| {
                device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                    label: Some(ep),
                    layout: Some(&dpl),
                    module: &dmodule,
                    entry_point: Some(ep),
                    compilation_options: Default::default(),
                    cache: None,
                })
            })
            .collect();
        Some(Gpu {
            demosaic_layout,
            demosaic_pipelines,
            device,
            queue,
            layout,
            pipelines,
            name: format!("{} ({:?})", info.name, info.backend),
            warmed: std::sync::atomic::AtomicBool::new(false),
        })
    }

    /// Bind group over buffers in binding order 0..=8.
    fn bind_group(&self, b: &[&wgpu::Buffer; 9]) -> wgpu::BindGroup {
        let entries: Vec<wgpu::BindGroupEntry> = b
            .iter()
            .enumerate()
            .map(|(i, buf)| wgpu::BindGroupEntry {
                binding: i as u32,
                resource: if i == 6 {
                    wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                        buffer: buf,
                        offset: 0,
                        size: std::num::NonZeroU64::new(std::mem::size_of::<PassUniform>() as u64),
                    })
                } else {
                    buf.as_entire_binding()
                },
            })
            .collect();
        self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("develop"),
            layout: &self.layout,
            entries: &entries,
        })
    }

    /// Demosaic a mosaic frame (as `pe_pipeline::demosaic::high_quality`) into a linear-RGB frame.
    pub fn demosaic(&self, frame: &RawFrame) -> Result<RawFrame, GpuError> {
        let Layout::Mosaic(cfa) = &frame.layout else {
            return Ok(frame.clone());
        };
        if cfa.width * cfa.height > 36 {
            return Err(GpuError::NotLinear);
        }
        let (w, h) = (frame.width as u32, frame.height as u32);
        let n = (w * h) as u64;
        if n * 12 > self.device.limits().max_storage_buffer_binding_size {
            return Err(GpuError::TooLarge(n * 12));
        }
        let mut pattern = [0u32; 36];
        for (k, c) in cfa.colors.iter().enumerate() {
            pattern[k] = *c as u32;
        }
        let u = DemosaicUniform {
            w,
            h,
            cfa_w: cfa.width as u32,
            cfa_h: cfa.height as u32,
            pattern,
        };
        use wgpu::BufferUsages as U;
        let d = &self.device;
        let init = |label: &str, bytes: &[u8], usage| {
            d.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some(label),
                contents: bytes,
                usage,
            })
        };
        let buf = |label: &str, size: u64, usage| {
            d.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size,
                usage,
                mapped_at_creation: false,
            })
        };
        let ubuf = init("demosaic params", bytemuck::bytes_of(&u), U::UNIFORM);
        let mosaic = init("mosaic", bytemuck::cast_slice(&frame.data), U::STORAGE);
        let green = buf("green", n * 4, U::STORAGE);
        let rgb = buf("rgb", n * 12, U::STORAGE | U::COPY_SRC);
        let diff = buf("diff", n * 8, U::STORAGE);
        let readback = buf("rgb readback", n * 12, U::MAP_READ | U::COPY_DST);
        let bg = d.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("demosaic"),
            layout: &self.demosaic_layout,
            entries: &[&ubuf, &mosaic, &green, &rgb, &diff]
                .iter()
                .enumerate()
                .map(|(i, b)| wgpu::BindGroupEntry {
                    binding: i as u32,
                    resource: b.as_entire_binding(),
                })
                .collect::<Vec<_>>(),
        });
        let mut enc = d.create_command_encoder(&Default::default());
        {
            let mut pass = enc.begin_compute_pass(&Default::default());
            pass.set_bind_group(0, &bg, &[]);
            let (gx, gy) = workgroups(w * h);
            for p in &self.demosaic_pipelines {
                pass.set_pipeline(p);
                pass.dispatch_workgroups(gx, gy, 1);
            }
        }
        enc.copy_buffer_to_buffer(&rgb, 0, &readback, 0, n * 12);
        self.queue.submit([enc.finish()]);
        let slice = readback.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        d.poll(wgpu::PollType::wait_indefinitely())
            .map_err(|e| GpuError::Readback(e.to_string()))?;
        rx.recv()
            .map_err(|e| GpuError::Readback(e.to_string()))?
            .map_err(|e| GpuError::Readback(e.to_string()))?;
        let data: Vec<f32> = bytemuck::cast_slice(
            &slice
                .get_mapped_range()
                .map_err(|e| GpuError::Readback(e.to_string()))?,
        )
        .to_vec();
        readback.unmap();
        Ok(RawFrame {
            layout: Layout::LinearRgb,
            data,
            ..frame.clone_header()
        })
    }

    /// Adapter name and backend, e.g. "Apple M1 (Metal)".
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Upload a linear-RGB frame and allocate everything needed to render it.
    pub fn upload(&self, frame: &RawFrame) -> Result<GpuFrame, GpuError> {
        self.upload_region(frame, region::Ctx::whole(frame.width, frame.height))
    }

    /// Upload a linear-RGB frame that is part of a larger picture (`ctx`), for region renders.
    pub fn upload_region(&self, frame: &RawFrame, ctx: region::Ctx) -> Result<GpuFrame, GpuError> {
        if !matches!(frame.layout, Layout::LinearRgb) {
            return Err(GpuError::NotLinear);
        }
        let (w, h) = (frame.width, frame.height);
        let geo = local::geometry_in(w, h, ctx.full_w, ctx.full_h);
        let n = (w * h) as u32;
        let m = (geo.small_w * geo.small_h) as u32;
        let planes = Planes {
            scene: 0,
            tex_h: n,
            tex: 2 * n,
            a: 3 * n,
            acc: 4 * n,
            e1: 5 * n,
            e2: 6 * n,
            s: [7 * n, 7 * n + m, 7 * n + 2 * m],
            sh: 7 * n + 3 * m,
            cl: 7 * n + 4 * m,
            dz: 7 * n + 5 * m,
            dz2: 7 * n + 6 * m,
        };
        let pool_bytes = (7 * n as u64 + 7 * m as u64) * 16;
        if pool_bytes > self.device.limits().max_storage_buffer_binding_size {
            return Err(GpuError::TooLarge(pool_bytes));
        }
        let d = &self.device;
        let buffer = |label: &str, size: u64, usage: wgpu::BufferUsages| {
            d.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size: size.max(16),
                usage,
                mapped_at_creation: false,
            })
        };
        use wgpu::BufferUsages as U;
        let src = d.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("src"),
            contents: bytemuck::cast_slice(&frame.data),
            usage: U::STORAGE,
        });
        let pool = buffer("pool", pool_bytes, U::STORAGE);
        let lut = buffer("lut", 4096 * 4, U::STORAGE | U::COPY_DST);
        let out = buffer("out", n as u64 * 4, U::STORAGE | U::COPY_SRC);
        let readback = buffer("readback", n as u64 * 4, U::MAP_READ | U::COPY_DST);
        let params = buffer(
            "params",
            std::mem::size_of::<Params>() as u64,
            U::UNIFORM | U::COPY_DST,
        );
        let look = buffer(
            "look",
            std::mem::size_of::<LookUniform>() as u64,
            U::UNIFORM | U::COPY_DST,
        );
        let passes = buffer("passes", PASS_STRIDE * MAX_PASSES, U::UNIFORM | U::COPY_DST);
        let masks_buf = buffer("masks", MASK_RECORDS as u64 * 16, U::STORAGE | U::COPY_DST);
        let mask_data = buffer("mask data", 16, U::STORAGE | U::COPY_DST);
        let bind_group = self.bind_group(&[
            &params, &src, &pool, &lut, &out, &look, &passes, &masks_buf, &mask_data,
        ]);
        let frame_out = GpuFrame {
            width: w as u32,
            height: h as u32,
            geo,
            ctx,
            planes,
            header: frame.clone_header(),
            // Grey-world needs the pixels; compute it now so no CPU copy has to be kept.
            auto_wb: wb::multipliers(frame, &WhiteBalance::Auto, &frame.data),
            host: frame.data.clone(),
            scale: frame.scale,
            noise: Mutex::new(None),
            params,
            look,
            passes,
            src,
            pool,
            lut,
            lut_state: Mutex::new(None),
            out,
            readback,
            masks: masks_buf,
            mask_data: Mutex::new(mask_data),
            bind_group: Mutex::new(bind_group),
            busy: Mutex::new(()),
        };
        // The first dispatch of each pipeline pays a one-off driver cost (~70 ms on an M1). Pay it
        // here, during loading, rather than on the first slider movement.
        if !self.warmed.swap(true, std::sync::atomic::Ordering::Relaxed) {
            let s = Settings {
                shadows: 1.0,
                clarity: 1.0,
                texture: 1.0,
                dehaze: 1.0,
                ..Default::default()
            };
            let _ = self.render(&frame_out, &s);
        }
        Ok(frame_out)
    }

    /// Render with `settings` to sRGB RGBA8, oriented for display. Returns (width, height, pixels).
    pub fn render(
        &self,
        f: &GpuFrame,
        settings: &Settings,
    ) -> Result<(u32, u32, Vec<u8>), GpuError> {
        self.render_in(f, settings, None, &region::Globals::default())
    }

    /// Render the output rectangle `out` (whole-picture output coordinates) of a frame uploaded with
    /// [`upload_region`](Self::upload_region), with whole-picture measurements where supplied.
    pub fn render_region(
        &self,
        f: &GpuFrame,
        settings: &Settings,
        out: [usize; 4],
        globals: &region::Globals,
    ) -> Result<(u32, u32, Vec<u8>), GpuError> {
        self.render_in(f, settings, Some(out), globals)
    }

    fn render_in(
        &self,
        f: &GpuFrame,
        settings: &Settings,
        out: Option<[usize; 4]>,
        globals: &region::Globals,
    ) -> Result<(u32, u32, Vec<u8>), GpuError> {
        let _busy = f.busy.lock().unwrap();
        let s = settings.clone().clamped();
        let ctx = f.ctx;
        let wbm = match s.white_balance {
            WhiteBalance::Auto => globals.auto_wb.unwrap_or(f.auto_wb),
            ref mode => wb::multipliers(&f.header, mode, &[]),
        };
        let to_work = pe_colour::camera_to_space(&f.header.xyz_to_cam, &REC2020_TO_XYZ_D65)
            .ok_or(GpuError::SingularMatrix)?;
        let gain = s.exposure.exp2();
        let out_m = ColorSpace::Srgb.from_space(&REC2020_TO_XYZ_D65);
        let tp = tone::ToneParams::new(s.contrast, s.whites, s.blacks);
        let (sev, hev) = tone::local_tone_amounts(s.shadows, s.highlights);
        let use_sh = s.shadows != 0.0 || s.highlights != 0.0;
        let mask_geo = geometry::GeometryParams::for_frame(&s, &f.header, ctx.full_w, ctx.full_h);
        let mp = MaskParams::new(&s.masks, &s.rasters, mask_geo.frame_w, mask_geo.frame_h);
        let (use_cl, use_tx) = (
            s.clarity != 0.0 || mp.needs_clarity,
            s.texture != 0.0 || mp.needs_texture,
        );
        let mut geo = geometry::GeometryParams::for_frame(&s, &f.header, ctx.full_w, ctx.full_h);
        let orientation = geo.orientation;
        // A region always goes through the geometry pass; with no geometry set, its output is the
        // oriented frame.
        let use_geometry = geo.active || out.is_some();
        if !geo.active {
            geo.out_w = geo.frame_w;
            geo.out_h = geo.frame_h;
            geo.crop = [0.0, 0.0, 1.0, 1.0];
        }
        let (full_ow, full_oh) = (geo.out_w as u32, geo.out_h as u32);
        let [ox, oy, ow, oh] = match out {
            Some(r) => r.map(|v| v as u32),
            None => [0, 0, full_ow, full_oh],
        };
        if (ow as u64) * (oh as u64) > f.width as u64 * f.height as u64 {
            return Err(GpuError::TooLarge((ow as u64) * (oh as u64) * 4));
        }

        // Scene pixel from the kept camera data, exactly as the CPU path computes it.
        let fw = f.width as usize;
        let scene_at = |x: usize, y: usize| {
            let i = (y * fw + x) * 3;
            let c = pe_pipeline::scene_pixel(
                [f.host[i], f.host[i + 1], f.host[i + 2]],
                wbm,
                &to_work,
                gain,
            );
            if geo.has_lens_gain() {
                let g = geo.lens_gain(x + ctx.x0, y + ctx.y0);
                c.map(|v| v * g)
            } else {
                c
            }
        };
        let (wu, hu) = (f.width as usize, f.height as usize);
        // Airlight from the same sampled scene pixels the CPU path uses.
        let mask_dehaze = s.masks.iter().any(|m| m.enabled && m.adjust.dehaze != 0.0);
        let want_airlight = s.dehaze != 0.0 || mask_dehaze;
        let airlight = if let (true, Some(a)) = (want_airlight, globals.airlight) {
            a
        } else if want_airlight {
            let samples: Vec<[f32; 3]> = (0..hu)
                .step_by(local::AIRLIGHT_STRIDE)
                .flat_map(|y| (0..wu).step_by(local::AIRLIGHT_STRIDE).map(move |x| (x, y)))
                .map(|(x, y)| scene_at(x, y))
                .collect();
            local::airlight(&samples)
        } else {
            [1.0; 3]
        };
        let detail_params = {
            let probe = detail::DetailParams::new(&s, [0.0; 3], f.scale);
            let want_noise = probe.nr || mp.needs_noise;
            let noise = if let (true, Some(n)) = (want_noise, globals.noise) {
                n
            } else if want_noise {
                // Measured without exposure and scaled by sqrt(gain), as the CPU path does.
                let key = [
                    wbm[0].to_bits(),
                    wbm[1].to_bits(),
                    wbm[2].to_bits(),
                    geo.lens_vignette.to_bits()
                        ^ geo.pv_amount.to_bits().rotate_left(7)
                        ^ geo.pvig[0].to_bits().rotate_left(13)
                        ^ geo.pv_scale.to_bits().rotate_left(19),
                ];
                let mut cache = f.noise.lock().unwrap();
                let n = match *cache {
                    Some((k, n)) if k == key => n,
                    _ => {
                        let n = detail::noise_sigma(wu, hu, |x, y| {
                            let i = (y * fw + x) * 3;
                            let c = [f.host[i], f.host[i + 1], f.host[i + 2]];
                            let c = pe_pipeline::scene_pixel(c, wbm, &to_work, 1.0);
                            if geo.has_lens_gain() {
                                let g = geo.lens_gain(x + ctx.x0, y + ctx.y0);
                                c.map(|v| v * g)
                            } else {
                                c
                            }
                        });
                        *cache = Some((key, n));
                        n
                    }
                };
                n.map(|v| v * gain.sqrt())
            } else {
                [0.0; 3]
            };
            (detail::DetailParams::new(&s, noise, f.scale), noise)
        };
        let (detail_params, noise) = detail_params;
        let ya = (airlight[0] * pe_colour::REC2020_LUMA[0]
            + airlight[1] * pe_colour::REC2020_LUMA[1]
            + airlight[2] * pe_colour::REC2020_LUMA[2])
            .max(1e-6);

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

        let pl = f.planes;
        let g = f.geo;
        let luma = ColorSpace::Srgb.luma();
        let p = Params {
            wb: [wbm[0], wbm[1], wbm[2], gain],
            m: [row4(to_work[0]), row4(to_work[1]), row4(to_work[2])],
            o: [row4(out_m[0]), row4(out_m[1]), row4(out_m[2])],
            out_luma: [luma[0], luma[1], luma[2], 0.0],
            tone: [
                tp.contrast_power,
                tp.white_norm,
                tp.black,
                tone::MID_GREY.log2(),
            ],
            local: [
                sev,
                hev,
                s.clarity / 100.0 * local::CLARITY_MAX,
                s.texture / 100.0 * local::TEXTURE_MAX,
            ],
            dims: [f.width, f.height, g.small_w as u32, g.small_h as u32],
            dims2: [
                g.factor as u32,
                0,
                orientation as u32,
                curve.is_some() as u32,
            ],
            dims3: [ow, oh, 0, 0],
            haze: [airlight[0], airlight[1], airlight[2], ya],
            haze2: [
                (s.dehaze.max(0.0)) / 100.0 * 0.95,
                (-s.dehaze).max(0.0) / 100.0 * local::HAZE_MAX,
                0.0,
                0.0,
            ],
            offs: [pl.scene, pl.sh, pl.cl, pl.tex],
            flags: [use_sh as u32, use_cl as u32, use_tx as u32, pl.dz],
            geo0: geo.crop,
            geo1: [geo.cos, geo.sin, geo.kx, geo.ky],
            geo2: [geo.inv_scale, geo.k1, geo.lens_vignette, 0.0],
            geo3: [geo.ca[0], geo.ca[1], geo.ca[2], 0.0],
            geo4: [geo.frame_w as u32, geo.frame_h as u32, 0, 0],
            full: [
                ctx.full_w as u32,
                ctx.full_h as u32,
                ctx.x0 as u32,
                ctx.y0 as u32,
            ],
            outr: [ox, oy, full_ow, full_oh],
            offs2: [pl.e1, pl.e2, 0, 0],
            lens0: [geo.pdist[0], geo.pdist[1], geo.pdist[2], geo.pdist[3]],
            lens1: [
                geo.pdist[4],
                geo.pr_scale,
                geo.pfit,
                (geo.pdist != [1.0, 0.0, 0.0, 0.0, 0.0] || geo.ptca != [[1.0, 0.0, 0.0]; 2]) as u32
                    as f32,
            ],
            lens2: [geo.ptca[0][0], geo.ptca[0][1], geo.ptca[0][2], geo.pv_scale],
            lens3: [
                geo.ptca[1][0],
                geo.ptca[1][1],
                geo.ptca[1][2],
                geo.pv_amount,
            ],
            lens4: [geo.pvig[0], geo.pvig[1], geo.pvig[2], 0.0],
        };
        self.queue
            .write_buffer(&f.params, 0, bytemuck::bytes_of(&p));
        let fx = effects::EffectsParams::new(&s, full_ow as usize, full_oh as usize);
        let lu = LookUniform::new(&look::LookParams::new(&s), &fx);
        let (mrec, mdata) = pack_masks(&mp);
        self.queue
            .write_buffer(&f.masks, 0, bytemuck::cast_slice(&mrec));
        {
            let mut md = f.mask_data.lock().unwrap();
            let need = (mdata.len() * 4) as u64;
            if md.size() < need {
                *md = self.device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("mask data"),
                    size: need.next_power_of_two(),
                    usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                });
                *f.bind_group.lock().unwrap() = self.bind_group(&[
                    &f.params, &f.src, &f.pool, &f.lut, &f.out, &f.look, &f.passes, &f.masks, &md,
                ]);
            }
            self.queue
                .write_buffer(&md, 0, bytemuck::cast_slice(&mdata));
        }
        self.queue.write_buffer(&f.look, 0, bytemuck::bytes_of(&lu));

        // The pass list.
        let (fw, fh) = (f.width, f.height);
        let (sw, sh) = (g.small_w as u32, g.small_h as u32);
        let mut list: Vec<(Kernel, PassUniform, u32)> = Vec::new(); // kernel, record, items
        let rec = |src, dst, w, h, r, mode, k: f32| PassUniform {
            src,
            dst,
            w,
            h,
            sw: fw,
            sh: fh,
            r,
            mode,
            k: [k, 0.0, 0.0, 0.0],
            ..Default::default()
        };
        // Full-size pass with an aux plane and four constants.
        let full = |kern, src, dst, aux, r, mode, k: [f32; 4]| {
            (
                kern,
                PassUniform {
                    src,
                    dst,
                    aux,
                    mode,
                    w: fw,
                    h: fh,
                    r,
                    k,
                    ..Default::default()
                },
                fw * fh,
            )
        };
        let small = |list: &mut Vec<_>, kern, src, dst, r, mode, k| {
            list.push((kern, rec(src, dst, sw, sh, r, mode, k), sw * sh));
        };
        // Box blur (h then v) of `src` into `dst`, via `tmp`.
        let box2 = |list: &mut Vec<_>, src, tmp, dst, r| {
            small(list, Kernel::Box, src, tmp, r, 0, 0.0);
            small(list, Kernel::Box, tmp, dst, r, 1, 0.0);
        };
        let [s0, s1, s2] = pl.s;
        list.push((Kernel::Scene, PassUniform::default(), fw * fh));
        if detail_params.enabled() {
            let (a, t, c, acc) = (pl.a, pl.tex_h, pl.tex, pl.acc);
            list.push(full(Kernel::ToOpponent, pl.scene, a, 0, 0, 0, [0.0; 4]));
            let mut cur = a;
            if detail_params.nr {
                let mut next = c;
                for (l, th) in detail_params.thresholds.iter().enumerate() {
                    let hole = 1u32 << l;
                    list.push(full(
                        Kernel::AtrousLevel,
                        cur,
                        next,
                        acc,
                        hole,
                        (l > 0) as u32,
                        *th,
                    ));
                    std::mem::swap(&mut cur, &mut next);
                }
                list.push(full(Kernel::AddPlanes, cur, a, acc, 0, 0, [0.0; 4]));
                cur = a;
            }
            if detail_params.sharpen {
                let sg = [detail_params.sigma, 0.0, 0.0, 0.0];
                list.push(full(Kernel::Gauss, cur, t, 0, 0, 0, sg));
                list.push(full(Kernel::Gauss, t, c, 0, 0, 1, sg));
                let k = [
                    detail_params.amount,
                    detail_params.knee,
                    detail_params.mask,
                    0.0,
                ];
                list.push(full(Kernel::Sharpen, cur, cur, c, 0, 0, k));
            }
            list.push(full(Kernel::FromOpponent, cur, pl.scene, 0, 0, 0, [0.0; 4]));
        }
        if s.dehaze > 0.0 {
            let pr = g.patch_r as u32;
            small(&mut list, Kernel::Downscale, pl.scene, s0, 0, 0, 0.0);
            small(&mut list, Kernel::MinFilter, s0, s1, pr, 0, 0.0);
            small(&mut list, Kernel::MinFilter, s1, s0, pr, 1, 0.0);
            small(
                &mut list,
                Kernel::DehazeInput,
                s0,
                s1,
                0,
                0,
                s.dehaze / 100.0 * 0.95,
            );
            box2(&mut list, s1, s2, s0, g.guide_r as u32);
            small(&mut list, Kernel::Coeffs, s0, s1, 0, 0, local::DEHAZE_EPS);
            box2(&mut list, s1, s2, pl.dz, g.guide_r as u32);
        }
        if s.dehaze != 0.0 {
            list.push((Kernel::DehazeApply, PassUniform::default(), fw * fh));
        }
        if use_sh || use_cl {
            small(&mut list, Kernel::Downscale, pl.scene, s0, 0, 1, 0.0);
            for (on, r, eps, dst) in [
                (use_sh, g.shadows_r, tone::LOCAL_EPS, pl.sh),
                (use_cl, g.clarity_r, local::CLARITY_EPS, pl.cl),
            ] {
                if on {
                    box2(&mut list, s0, s1, s2, r as u32);
                    small(&mut list, Kernel::Coeffs, s2, s1, 0, 0, eps);
                    box2(&mut list, s1, s2, dst, r as u32);
                }
            }
        }
        if use_tx {
            let r = g.texture_r as u32;
            list.push((
                Kernel::Box,
                rec(pl.scene, pl.tex_h, fw, fh, r, 0, 0.0),
                fw * fh,
            ));
            list.push((
                Kernel::Box,
                rec(pl.tex_h, pl.tex, fw, fh, r, 1, 0.0),
                fw * fh,
            ));
        }
        // Targets for masked noise reduction, sharpness and dehaze, from the scene before the global
        // local-tone gain (the final pass scales them by it), as in pe_pipeline::local_targets.
        if mp.needs_noise {
            let nr_settings = Settings {
                noise_reduction: pe_pipeline::NoiseReduction {
                    luminance: masks::LOCAL_NR_STRENGTH,
                    color: masks::LOCAL_NR_STRENGTH,
                    detail: 50.0,
                },
                sharpening: pe_pipeline::Sharpening {
                    amount: 0.0,
                    ..Default::default()
                },
                ..s.clone()
            };
            let dp = detail::DetailParams::new(&nr_settings, noise, f.scale);
            let (a, t, acc) = (pl.a, pl.tex_h, pl.acc);
            list.push(full(Kernel::ToOpponent, pl.scene, a, 0, 0, 0, [0.0; 4]));
            let (mut cur, mut next) = (a, t);
            for (l, th) in dp.thresholds.iter().enumerate() {
                list.push(full(
                    Kernel::AtrousLevel,
                    cur,
                    next,
                    acc,
                    1u32 << l,
                    (l > 0) as u32,
                    *th,
                ));
                std::mem::swap(&mut cur, &mut next);
            }
            list.push(full(Kernel::AddPlanes, cur, a, acc, 0, 0, [0.0; 4]));
            list.push(full(Kernel::FromOpponent, a, pl.e1, 0, 0, 0, [0.0; 4]));
        }
        if mp.needs_sharpness {
            let sigma = (1.0 / f.scale.max(1e-3)).max(0.2);
            let sg = [sigma, 0.0, 0.0, 0.0];
            list.push(full(Kernel::ToOpponent, pl.scene, pl.a, 0, 0, 0, [0.0; 4]));
            list.push(full(Kernel::Gauss, pl.a, pl.tex_h, 0, 0, 0, sg));
            list.push(full(Kernel::Gauss, pl.tex_h, pl.acc, 0, 0, 1, sg));
            list.push(full(
                Kernel::SharpTarget,
                pl.a,
                pl.e1,
                pl.acc,
                0,
                0,
                [0.0; 4],
            ));
        }
        if mp.needs_dehaze {
            let pr = g.patch_r as u32;
            small(&mut list, Kernel::Downscale, pl.scene, s0, 0, 0, 0.0);
            small(&mut list, Kernel::MinFilter, s0, s1, pr, 0, 0.0);
            small(&mut list, Kernel::MinFilter, s1, s0, pr, 1, 0.0);
            small(&mut list, Kernel::DehazeInput, s0, s1, 0, 0, 0.95);
            box2(&mut list, s1, s2, s0, g.guide_r as u32);
            small(&mut list, Kernel::Coeffs, s0, s1, 0, 0, local::DEHAZE_EPS);
            box2(&mut list, s1, s2, pl.dz2, g.guide_r as u32);
            list.push(full(
                Kernel::DehazeTarget,
                pl.scene,
                pl.e2,
                pl.dz2,
                0,
                0,
                [0.0; 4],
            ));
        }
        if use_geometry {
            list.push(full(Kernel::Display, 0, pl.tex_h, 0, 0, 0, [0.0; 4]));
            list.push((
                Kernel::Geometry,
                PassUniform {
                    src: pl.tex_h,
                    ..Default::default()
                },
                ow * oh,
            ));
        } else {
            list.push((Kernel::Final, PassUniform::default(), ow * oh));
        }
        assert!(list.len() as u64 <= MAX_PASSES);

        let mut records = vec![0u8; list.len() * PASS_STRIDE as usize];
        for (i, (_, r, _)) in list.iter().enumerate() {
            let at = i * PASS_STRIDE as usize;
            records[at..at + std::mem::size_of::<PassUniform>()]
                .copy_from_slice(bytemuck::bytes_of(r));
        }
        self.queue.write_buffer(&f.passes, 0, &records);

        let mut enc = self.device.create_command_encoder(&Default::default());
        {
            let bind_group = f.bind_group.lock().unwrap();
            let mut pass = enc.begin_compute_pass(&Default::default());
            for (i, (kern, _, items)) in list.iter().enumerate() {
                pass.set_pipeline(&self.pipelines[*kern as usize]);
                pass.set_bind_group(0, &*bind_group, &[(i as u64 * PASS_STRIDE) as u32]);
                let (gx, gy) = workgroups(*items);
                pass.dispatch_workgroups(gx, gy, 1);
            }
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
