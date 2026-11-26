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
use pe_pipeline::{detail, local, look, tone, wb, Settings, WhiteBalance};
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
}

const ENTRY_POINTS: [&str; 14] = [
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
    sh: u32,
    cl: u32,
    dz: u32,
}

/// A frame resident on the GPU, with scratch buffers for rendering it.
pub struct GpuFrame {
    width: u32,
    height: u32,
    geo: local::Geometry,
    planes: Planes,
    orientation: u8,
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
        let geo = local::geometry(w, h);
        let n = (w * h) as u32;
        let m = (geo.small_w * geo.small_h) as u32;
        let planes = Planes {
            scene: 0,
            tex_h: n,
            tex: 2 * n,
            a: 3 * n,
            acc: 4 * n,
            s: [5 * n, 5 * n + m, 5 * n + 2 * m],
            sh: 5 * n + 3 * m,
            cl: 5 * n + 4 * m,
            dz: 5 * n + 5 * m,
        };
        let pool_bytes = (5 * n as u64 + 6 * m as u64) * 16;
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
        fn whole(b: &wgpu::Buffer) -> wgpu::BindingResource<'_> {
            b.as_entire_binding()
        }
        let entries = [
            (0, whole(&params)),
            (1, whole(&src)),
            (2, whole(&pool)),
            (3, whole(&lut)),
            (4, whole(&out)),
            (5, whole(&look)),
            (
                6,
                wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                    buffer: &passes,
                    offset: 0,
                    size: std::num::NonZeroU64::new(std::mem::size_of::<PassUniform>() as u64),
                }),
            ),
        ]
        .map(|(binding, resource)| wgpu::BindGroupEntry { binding, resource });
        let bind_group = d.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("develop"),
            layout: &self.layout,
            entries: &entries,
        });
        let frame_out = GpuFrame {
            width: w as u32,
            height: h as u32,
            geo,
            planes,
            orientation: frame.orientation,
            header: frame.clone_header(),
            // Grey-world needs the pixels; compute it now so no CPU copy has to be kept.
            auto_wb: wb::multipliers(frame, &WhiteBalance::Auto, &frame.data),
            host: frame.data.clone(),
            scale: frame.scale,
            noise: Mutex::new(None),
            params,
            look,
            passes,
            lut,
            lut_state: Mutex::new(None),
            out,
            readback,
            bind_group,
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
        let _busy = f.busy.lock().unwrap();
        let s = settings.clone().clamped();
        let wbm = match s.white_balance {
            WhiteBalance::Auto => f.auto_wb,
            ref mode => wb::multipliers(&f.header, mode, &[]),
        };
        let to_work = pe_colour::camera_to_space(&f.header.xyz_to_cam, &REC2020_TO_XYZ_D65)
            .ok_or(GpuError::SingularMatrix)?;
        let gain = s.exposure.exp2();
        let out_m = ColorSpace::Srgb.from_space(&REC2020_TO_XYZ_D65);
        let tp = tone::ToneParams::new(s.contrast, s.whites, s.blacks);
        let (sev, hev) = tone::local_tone_amounts(s.shadows, s.highlights);
        let use_sh = s.shadows != 0.0 || s.highlights != 0.0;
        let (use_cl, use_tx) = (s.clarity != 0.0, s.texture != 0.0);
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

        // Scene pixel from the kept camera data, exactly as the CPU path computes it.
        let fw = f.width as usize;
        let scene_at = |x: usize, y: usize| {
            let i = (y * fw + x) * 3;
            pe_pipeline::scene_pixel(
                [f.host[i], f.host[i + 1], f.host[i + 2]],
                wbm,
                &to_work,
                gain,
            )
        };
        let (wu, hu) = (f.width as usize, f.height as usize);
        // Airlight from the same sampled scene pixels the CPU path uses.
        let airlight = if s.dehaze != 0.0 {
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
            let noise = if probe.nr {
                // Measured without exposure and scaled by sqrt(gain), as the CPU path does.
                let key = [wbm[0].to_bits(), wbm[1].to_bits(), wbm[2].to_bits(), 0];
                let mut cache = f.noise.lock().unwrap();
                let n = match *cache {
                    Some((k, n)) if k == key => n,
                    _ => {
                        let n = detail::noise_sigma(wu, hu, |x, y| {
                            let i = (y * fw + x) * 3;
                            let c = [f.host[i], f.host[i + 1], f.host[i + 2]];
                            pe_pipeline::scene_pixel(c, wbm, &to_work, 1.0)
                        });
                        *cache = Some((key, n));
                        n
                    }
                };
                n.map(|v| v * gain.sqrt())
            } else {
                [0.0; 3]
            };
            detail::DetailParams::new(&s, noise, f.scale)
        };
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
        };
        self.queue
            .write_buffer(&f.params, 0, bytemuck::bytes_of(&p));
        let lu = LookUniform::new(&look::LookParams::new(&s));
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
            small(&mut list, Kernel::DehazeInput, s0, s1, 0, 0, 0.0);
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
        list.push((Kernel::Final, PassUniform::default(), ow * oh));
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
            let mut pass = enc.begin_compute_pass(&Default::default());
            for (i, (kern, _, items)) in list.iter().enumerate() {
                pass.set_pipeline(&self.pipelines[*kern as usize]);
                pass.set_bind_group(0, &f.bind_group, &[(i as u64 * PASS_STRIDE) as u32]);
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
