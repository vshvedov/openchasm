//! Optional SDL/wgpu presenter. All gameplay pixels stay on the GPU after upload.
//!
//! The borrowed SDL window bounds the raw surface lifetime and keeps this object
//! on its owning thread. Drop this presenter before creating an SDL Canvas on the
//! same window. The caller owns fallback/recreation and retains its Session.
//! GPU resources and packing storage are reused; Queue uploads may allocate
//! internal staging memory. Readback exists only in the tests below.

use crate::color_pipeline::{ColorSettings, LinearRgb};
use sdl2::video::Window;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const MAX_PIXELS: usize = 16_777_216;
const MAX_SIDE: usize = 16_384;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameStatus {
    Presented,
    Skipped,
    /// Drop the presenter before recreating it or selecting the SDL fallback.
    RecreateSurface,
}

/// Owns the surface while borrowing the SDL window that supplies both handles.
pub struct GpuPresenter<'w> {
    surface: wgpu::Surface<'w>,
    window: &'w Window,
    core: PostProcessor,
    config: wgpu::SurfaceConfiguration,
    configured: bool,
    adapter_name: String,
    profile: GpuProfile,
}

impl<'w> GpuPresenter<'w> {
    pub fn new(window: &'w Window, width: usize, height: usize) -> Result<Self, String> {
        checked_extent(width, height)?;
        let mut descriptor = wgpu::InstanceDescriptor::new_without_display_handle();
        descriptor.backends = wgpu::Backends::VULKAN | wgpu::Backends::METAL | wgpu::Backends::DX12;
        let instance = wgpu::Instance::new(descriptor);
        // SAFETY: window is borrowed for 'w and retains SDL's window/video context.
        // The resulting Surface<'w> cannot outlive that borrow. No thread-safety
        // promises are added to SDL; Window is !Send/!Sync. The caller must build
        // the window on the main thread with .metal_view() on macOS.
        let surface = unsafe {
            let target = wgpu::SurfaceTargetUnsafe::from_display_and_window(window, window)
                .map_err(|e| format!("SDL GPU window handles: {e}"))?;
            instance.create_surface_unsafe(target)
        }
        .map_err(|e| format!("GPU surface: {e}"))?;
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
            ..Default::default()
        }))
        .map_err(|e| format!("GPU adapter unavailable: {e}"))?;
        let info = adapter.get_info();
        let adapter_name = format!("{} ({:?}, {:?})", info.name, info.backend, info.device_type);
        let capabilities = surface.get_capabilities(&adapter);
        let format = capabilities
            .formats
            .iter()
            .copied()
            .find(wgpu::TextureFormat::is_srgb)
            .or_else(|| capabilities.formats.first().copied())
            .ok_or("GPU surface has no supported format")?;
        let physical = physical_size(window);
        let mut config = surface
            .get_default_config(&adapter, physical.0.max(1), physical.1.max(1))
            .ok_or("GPU adapter cannot configure this surface")?;
        config.format = format;
        config.color_space = wgpu::SurfaceColorSpace::Auto;
        config.present_mode = wgpu::PresentMode::Fifo;
        config.usage = wgpu::TextureUsages::RENDER_ATTACHMENT;
        let core = PostProcessor::new(&adapter, width, height, format)?;
        let mut result = Self {
            surface,
            window,
            core,
            config,
            configured: false,
            adapter_name,
            profile: GpuProfile::new(),
        };
        result.ensure_configured()?;
        Ok(result)
    }

    pub fn adapter_name(&self) -> &str {
        &self.adapter_name
    }

    /// Empty overlay means identity; otherwise exactly one [add RGB, opacity]
    /// per internal pixel. All components must be finite in 0..=1.
    /// Invalid arguments are rejected before any upload or surface acquisition.
    pub fn present_hdr(
        &mut self,
        frame: &[LinearRgb],
        settings: ColorSettings,
        bloom_strength: f32,
        overlay: &[[f32; 4]],
    ) -> Result<FrameStatus, String> {
        self.profile.begin();
        let started = self.profile.start();
        self.core
            .prepare_hdr(frame, settings, bloom_strength, overlay)?;
        self.profile.record(0, started);
        let started = self.profile.start();
        let Some((image, suboptimal)) = self.acquire()? else {
            return Ok(FrameStatus::Skipped);
        };
        if let Acquired::Recreate = image {
            return Ok(FrameStatus::RecreateSurface);
        }
        let Acquired::Image(image) = image else {
            unreachable!()
        };
        self.profile.record(1, started);
        let started = self.profile.start();
        self.core.upload_packed();
        self.profile.record(3, started);
        self.draw(image, suboptimal, settings, bloom_strength, overlay, false)
    }

    /// Encoded 0xRRGGBB pixels bypass all HDR color/bloom operations.
    pub fn present_rgb(
        &mut self,
        frame: &[u32],
        overlay: &[[f32; 4]],
    ) -> Result<FrameStatus, String> {
        self.profile.begin();
        let started = self.profile.start();
        self.core.validate_rgb(frame, overlay)?;
        self.profile.record(0, started);
        let started = self.profile.start();
        let Some((image, suboptimal)) = self.acquire()? else {
            return Ok(FrameStatus::Skipped);
        };
        if let Acquired::Recreate = image {
            return Ok(FrameStatus::RecreateSurface);
        }
        let Acquired::Image(image) = image else {
            unreachable!()
        };
        self.profile.record(1, started);
        let started = self.profile.start();
        self.core.pack_rgb(frame);
        self.profile.record(2, started);
        let started = self.profile.start();
        self.core.upload_packed();
        self.profile.record(3, started);
        self.draw(
            image,
            suboptimal,
            ColorSettings::default(),
            0.0,
            overlay,
            true,
        )
    }

    fn ensure_configured(&mut self) -> Result<bool, String> {
        self.core.check_error()?;
        let (width, height) = physical_size(self.window);
        // SDL can retain its last size while a window is minimized.
        if width == 0 || height == 0 || self.window.is_minimized() {
            return Ok(false);
        }
        let max = self.core.device.limits().max_texture_dimension_2d;
        if width > max || height > max {
            return Err("GPU surface exceeds adapter dimensions".into());
        }
        if !self.configured || self.config.width != width || self.config.height != height {
            self.config.width = width;
            self.config.height = height;
            self.surface.configure(&self.core.device, &self.config);
            self.core.check_error()?;
            self.configured = true;
        }
        Ok(true)
    }

    fn acquire(&mut self) -> Result<Option<(Acquired, bool)>, String> {
        if !self.ensure_configured()? {
            return Ok(None);
        }
        match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(image) => {
                Ok(Some((Acquired::Image(image), false)))
            }
            wgpu::CurrentSurfaceTexture::Suboptimal(image) => {
                Ok(Some((Acquired::Image(image), true)))
            }
            wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {
                Ok(None)
            }
            wgpu::CurrentSurfaceTexture::Outdated => {
                self.configured = false;
                Ok(None)
            }
            wgpu::CurrentSurfaceTexture::Lost => Ok(Some((Acquired::Recreate, false))),
            wgpu::CurrentSurfaceTexture::Validation => {
                Err("GPU surface acquisition validation failed".into())
            }
        }
    }

    fn draw(
        &mut self,
        image: wgpu::SurfaceTexture,
        suboptimal: bool,
        settings: ColorSettings,
        strength: f32,
        overlay: &[[f32; 4]],
        legacy: bool,
    ) -> Result<FrameStatus, String> {
        let started = self.profile.start();
        let view = image.texture.create_view(&Default::default());
        let mut encoder =
            self.core
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("OpenChasm GPU frame"),
                });
        let upload_time = self.core.encode(
            &mut encoder,
            &view,
            [self.config.width, self.config.height],
            PostSettings {
                color: settings,
                bloom: strength,
                profile: self.profile.enabled,
            },
            overlay,
            legacy,
        );
        self.profile.current[4] = upload_time;
        self.profile.current[5] = started
            .map_or(Duration::ZERO, |t| t.elapsed())
            .saturating_sub(upload_time);
        let started = self.profile.start();
        self.core.queue.submit([encoder.finish()]);
        self.profile.record(6, started);
        let started = self.profile.start();
        self.core.queue.present(image);
        self.profile.record(7, started);
        if suboptimal {
            self.configured = false;
        }
        let started = self.profile.start();
        self.core
            .device
            .poll(wgpu::PollType::Poll)
            .map_err(|e| format!("GPU poll: {e}"))?;
        self.core.check_error()?;
        self.profile.record(8, started);
        self.profile.finish();
        Ok(FrameStatus::Presented)
    }
}

enum Acquired {
    Image(wgpu::SurfaceTexture),
    Recreate,
}

/// CPU wall times only: submission is asynchronous and presentation may wait.
/// HDR preparation includes packing in `validate`; legacy packing is separate.
/// Disabled profiling avoids clock reads; output is aggregated every 120 frames.
struct GpuProfile {
    enabled: bool,
    current: [Duration; 9],
    totals: [Duration; 9],
    frames: u32,
}
impl GpuProfile {
    fn new() -> Self {
        Self {
            enabled: std::env::var("OPENCHASM_PROFILE").as_deref() == Ok("1"),
            current: [Duration::ZERO; 9],
            totals: [Duration::ZERO; 9],
            frames: 0,
        }
    }
    fn begin(&mut self) {
        self.current.fill(Duration::ZERO);
    }
    fn start(&self) -> Option<Instant> {
        self.enabled.then(Instant::now)
    }
    fn record(&mut self, stage: usize, started: Option<Instant>) {
        self.current[stage] = started.map_or(Duration::ZERO, |t| t.elapsed());
    }
    fn finish(&mut self) {
        if !self.enabled {
            return;
        }
        for (total, current) in self.totals.iter_mut().zip(self.current) {
            *total += current;
        }
        self.frames += 1;
        if self.frames == 120 {
            let ms = self
                .totals
                .map(|time| time.as_secs_f64() * 1000.0 / f64::from(self.frames));
            eprintln!("GPU CPU profile (120 frames, ms): validate {:.3}, acquire {:.3}, pack {:.3}, scene-upload {:.3}, params/UI-upload {:.3}, encode {:.3}, submit {:.3}, present {:.3}, poll {:.3}",
                ms[0], ms[1], ms[2], ms[3], ms[4], ms[5], ms[6], ms[7], ms[8]);
            self.frames = 0;
            self.totals.fill(Duration::ZERO);
        }
    }
}

fn physical_size(window: &Window) -> (u32, u32) {
    let mut width = 0;
    let mut height = 0;
    // SAFETY: Window owns a live SDL_Window; outputs point to local integers.
    unsafe {
        sdl2::sys::SDL_GetWindowSizeInPixels(window.raw(), &mut width, &mut height);
    }
    (width.max(0) as u32, height.max(0) as u32)
}

fn checked_extent(width: usize, height: usize) -> Result<usize, String> {
    if width == 0 || height == 0 || width > MAX_SIDE || height > MAX_SIDE {
        return Err("GPU internal dimensions outside 1..=16384".into());
    }
    let count = width.checked_mul(height).ok_or("GPU image size overflow")?;
    if count > MAX_PIXELS {
        return Err("GPU image exceeds 16M pixels".into());
    }
    Ok(count)
}

#[inline]
#[allow(
    clippy::manual_range_contains,
    reason = "Bitwise comparisons intentionally avoid short-circuiting the SIMD validity reduction"
)]
fn valid_channel(value: f32, maximum: f32) -> bool {
    (value >= 0.0) & (value <= maximum)
}

fn viewport(width: u32, height: u32, target: [u32; 2]) -> [f32; 4] {
    let scale = (target[0] as f32 / width as f32).min(target[1] as f32 / height as f32);
    let w = (width as f32 * scale)
        .round()
        .max(1.0)
        .min(target[0] as f32);
    let h = (height as f32 * scale)
        .round()
        .max(1.0)
        .min(target[1] as f32);
    [
        ((target[0] as f32 - w) * 0.5).floor(),
        ((target[1] as f32 - h) * 0.5).floor(),
        w,
        h,
    ]
}

struct PostProcessor {
    device: wgpu::Device,
    queue: wgpu::Queue,
    failure: Arc<Mutex<Option<String>>>,
    width: u32,
    height: u32,
    srgb: bool,
    packed: Vec<f32>,
    input: wgpu::Buffer,
    overlay: wgpu::Buffer,
    uniform: wgpu::Buffer,
    bloom_bind: wgpu::BindGroup,
    post_bind: wgpu::BindGroup,
    extract: wgpu::ComputePipeline,
    horizontal: wgpu::ComputePipeline,
    vertical: wgpu::ComputePipeline,
    post: wgpu::RenderPipeline,
}

#[derive(Clone, Copy)]
struct PostSettings {
    color: ColorSettings,
    bloom: f32,
    profile: bool,
}

impl PostProcessor {
    fn new(
        adapter: &wgpu::Adapter,
        width: usize,
        height: usize,
        format: wgpu::TextureFormat,
    ) -> Result<Self, String> {
        let count = checked_extent(width, height)?;
        let limits = adapter.limits();
        let largest = (count as u64) * 16;
        if largest > limits.max_storage_buffer_binding_size || largest > limits.max_buffer_size {
            return Err("GPU frame exceeds adapter buffer limits".into());
        }
        let mut required = wgpu::Limits::default();
        required.max_storage_buffer_binding_size =
            required.max_storage_buffer_binding_size.max(largest);
        required.max_buffer_size = required.max_buffer_size.max(largest);
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("OpenChasm post device"),
            required_limits: required,
            ..Default::default()
        }))
        .map_err(|e| format!("GPU device: {e}"))?;
        let failure = Arc::new(Mutex::new(None));
        let error_sink = failure.clone();
        device.on_uncaptured_error(Arc::new(move |error: wgpu::Error| {
            if let Ok(mut slot) = error_sink.lock() {
                if slot.is_none() {
                    *slot = Some(error.to_string());
                }
            }
        }));
        let error_sink = failure.clone();
        device.set_device_lost_callback(move |reason, message| {
            if let Ok(mut slot) = error_sink.lock() {
                if slot.is_none() {
                    *slot = Some(format!("device lost ({reason:?}): {message}"));
                }
            }
        });
        let validation = device.push_error_scope(wgpu::ErrorFilter::Validation);
        let buffer = |label, size, usage| {
            device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size,
                usage,
                mapped_at_creation: false,
            })
        };
        let input = buffer(
            "HDR RGB input",
            count as u64 * 12,
            wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
        );
        let overlay = buffer(
            "display-space UI",
            largest,
            wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
        );
        let uniform = buffer(
            "post parameters",
            64,
            wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        );
        let half_bytes = (width.div_ceil(2) * height.div_ceil(2) * 16) as u64;
        let bright = buffer("bloom bright", half_bytes, wgpu::BufferUsages::STORAGE);
        let scratch = buffer("bloom scratch", half_bytes, wgpu::BufferUsages::STORAGE);
        let layout = |label, visibility, writable: bool| {
            let entries = std::array::from_fn::<_, 4, _>(|binding| wgpu::BindGroupLayoutEntry {
                binding: binding as u32,
                visibility,
                ty: wgpu::BindingType::Buffer {
                    ty: if binding == 0 {
                        wgpu::BufferBindingType::Uniform
                    } else {
                        wgpu::BufferBindingType::Storage {
                            read_only: !(writable && binding >= 2),
                        }
                    },
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            });
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some(label),
                entries: &entries,
            })
        };
        let bloom_layout = layout("bloom bindings", wgpu::ShaderStages::COMPUTE, true);
        let post_layout = layout("post bindings", wgpu::ShaderStages::FRAGMENT, false);
        let bind = |label, layout, buffers: [&wgpu::Buffer; 4]| {
            let entries = std::array::from_fn::<_, 4, _>(|i| wgpu::BindGroupEntry {
                binding: i as u32,
                resource: buffers[i].as_entire_binding(),
            });
            device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some(label),
                layout,
                entries: &entries,
            })
        };
        let bloom_bind = bind(
            "bloom buffers",
            &bloom_layout,
            [&uniform, &input, &bright, &scratch],
        );
        let post_bind = bind(
            "post buffers",
            &post_layout,
            [&uniform, &input, &overlay, &bright],
        );
        let pipeline_layout = |label, bind| {
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some(label),
                bind_group_layouts: &[Some(bind)],
                immediate_size: 0,
            })
        };
        let bloom_pipeline_layout = pipeline_layout("bloom pipeline layout", &bloom_layout);
        let post_pipeline_layout = pipeline_layout("post pipeline layout", &post_layout);
        let bloom_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("independent bloom"),
            source: wgpu::ShaderSource::Wgsl(include_str!("gpu_bloom.wgsl").into()),
        });
        let post_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("independent color post"),
            source: wgpu::ShaderSource::Wgsl(include_str!("gpu_post.wgsl").into()),
        });
        let compute = |entry| {
            device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some(entry),
                layout: Some(&bloom_pipeline_layout),
                module: &bloom_shader,
                entry_point: Some(entry),
                compilation_options: Default::default(),
                cache: None,
            })
        };
        let extract = compute("extract");
        let horizontal = compute("horizontal");
        let vertical = compute("vertical");
        let post = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("HDR / UI presentation"),
            layout: Some(&post_pipeline_layout),
            vertex: wgpu::VertexState {
                module: &post_shader,
                entry_point: Some("vertex"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            primitive: Default::default(),
            depth_stencil: None,
            multisample: Default::default(),
            fragment: Some(wgpu::FragmentState {
                module: &post_shader,
                entry_point: Some("fragment"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });
        if let Some(error) = pollster::block_on(validation.pop()) {
            return Err(format!("GPU initialization validation: {error}"));
        }
        let mut packed = Vec::new();
        packed
            .try_reserve_exact(count * 3)
            .map_err(|_| "unable to allocate GPU packing buffer")?;
        packed.resize(count * 3, 0.0);
        let result = Self {
            device,
            queue,
            failure,
            width: width as u32,
            height: height as u32,
            srgb: format.is_srgb(),
            packed,
            input,
            overlay,
            uniform,
            bloom_bind,
            post_bind,
            extract,
            horizontal,
            vertical,
            post,
        };
        result.check_error()?;
        Ok(result)
    }

    fn check_error(&self) -> Result<(), String> {
        let error = self
            .failure
            .lock()
            .map_err(|_| "GPU error handler poisoned")?;
        match &*error {
            Some(error) => Err(format!("GPU failure: {error}")),
            None => Ok(()),
        }
    }

    fn validate_overlay(&self, overlay: &[[f32; 4]]) -> Result<(), String> {
        let count = self.width as usize * self.height as usize;
        if !overlay.is_empty() && overlay.len() != count {
            return Err("GPU overlay length differs from image".into());
        }
        // Independent, non-short-circuit lane reductions allow vectorization.
        // Ordered comparisons also reject NaN and either infinity; no separate
        // is_finite test or data-dependent per-component branch is needed.
        let mut valid = [true; 4];
        for pixel in overlay {
            for (valid, &value) in valid.iter_mut().zip(pixel) {
                *valid &= valid_channel(value, 1.0);
            }
        }
        if !valid.into_iter().all(|value| value) {
            return Err("GPU overlay requires finite components in 0..=1".into());
        }
        Ok(())
    }

    fn prepare_hdr(
        &mut self,
        frame: &[LinearRgb],
        settings: ColorSettings,
        strength: f32,
        overlay: &[[f32; 4]],
    ) -> Result<(), String> {
        settings.validate().map_err(str::to_owned)?;
        if !strength.is_finite() || !(0.0..=16.0).contains(&strength) {
            return Err("GPU bloom strength outside 0..=16".into());
        }
        if frame.len() != self.width as usize * self.height as usize {
            return Err("GPU HDR frame length differs from image".into());
        }
        // Internal scratch may change on error, but neither caller memory nor
        // GPU resources change. Every validation completes before acquisition
        // or upload; a subsequent valid frame overwrites the entire scratch.
        let mut valid = [true; 3];
        for (out, pixel) in self.packed.as_chunks_mut::<3>().0.iter_mut().zip(frame) {
            out.copy_from_slice(&pixel.0);
            for (valid, &value) in valid.iter_mut().zip(&pixel.0) {
                *valid &= valid_channel(value, 1.0e12);
            }
        }
        if !valid.into_iter().all(|value| value) {
            return Err("GPU HDR requires finite nonnegative channels up to 1e12".into());
        }
        self.validate_overlay(overlay)
    }

    fn validate_rgb(&self, frame: &[u32], overlay: &[[f32; 4]]) -> Result<(), String> {
        if frame.len() != self.width as usize * self.height as usize {
            return Err("GPU RGB frame length differs from image".into());
        }
        self.validate_overlay(overlay)
    }

    #[cfg(test)]
    fn pack_hdr(&mut self, frame: &[LinearRgb]) {
        for (out, pixel) in self.packed.as_chunks_mut::<3>().0.iter_mut().zip(frame) {
            out.copy_from_slice(&pixel.0);
        }
    }

    fn pack_rgb(&mut self, frame: &[u32]) {
        for (out, pixel) in self.packed.as_chunks_mut::<3>().0.iter_mut().zip(frame) {
            out[0] = ((pixel >> 16) & 255) as f32 / 255.0;
            out[1] = ((pixel >> 8) & 255) as f32 / 255.0;
            out[2] = (pixel & 255) as f32 / 255.0;
        }
    }

    fn upload_packed(&self) {
        self.queue
            .write_buffer(&self.input, 0, bytemuck::cast_slice(&self.packed));
    }

    #[cfg(test)]
    fn upload_hdr(&mut self, frame: &[LinearRgb]) {
        self.pack_hdr(frame);
        self.upload_packed();
    }

    #[cfg(test)]
    fn upload_rgb(&mut self, frame: &[u32]) {
        self.pack_rgb(frame);
        self.upload_packed();
    }

    fn encode(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        view: &wgpu::TextureView,
        target: [u32; 2],
        post_settings: PostSettings,
        overlay: &[[f32; 4]],
        legacy: bool,
    ) -> Duration {
        let started = post_settings.profile.then(Instant::now);
        let settings = post_settings.color;
        let strength = post_settings.bloom;
        let half = [self.width.div_ceil(2), self.height.div_ceil(2)];
        let exposure = 2.0 * settings.brightness * settings.fade.powi(3);
        let area = viewport(self.width, self.height, target);
        // Four 16-byte vectors, explicit scalar bytes; no Rust/WGSL struct-layout assumption.
        let mut parameters = [0u32; 16];
        parameters[..4].copy_from_slice(&[self.width, self.height, half[0], half[1]]);
        for (out, v) in parameters[4..8].iter_mut().zip(area) {
            *out = v.to_bits();
        }
        for (out, v) in parameters[8..12].iter_mut().zip([
            settings.gamma,
            settings.saturation,
            exposure,
            strength,
        ]) {
            *out = v.to_bits();
        }
        parameters[12..].copy_from_slice(&[
            u32::from(settings.tone_mapping),
            u32::from(legacy),
            u32::from(!overlay.is_empty()),
            u32::from(self.srgb),
        ]);
        self.queue
            .write_buffer(&self.uniform, 0, bytemuck::cast_slice(&parameters));
        if !overlay.is_empty() {
            self.queue
                .write_buffer(&self.overlay, 0, bytemuck::cast_slice(overlay));
        }
        let upload = started.map_or(Duration::ZERO, |t| t.elapsed());
        if !legacy && strength > 0.0 && exposure > 0.0 {
            // Separate passes establish storage write/read ordering between filters.
            for pipeline in [&self.extract, &self.horizontal, &self.vertical] {
                let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                    label: Some("bloom"),
                    timestamp_writes: None,
                });
                pass.set_pipeline(pipeline);
                pass.set_bind_group(0, &self.bloom_bind, &[]);
                pass.dispatch_workgroups(half[0].div_ceil(8), half[1].div_ceil(8), 1);
            }
        }
        let attachments = [Some(wgpu::RenderPassColorAttachment {
            view,
            depth_slice: None,
            resolve_target: None,
            ops: wgpu::Operations {
                load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                store: wgpu::StoreOp::Store,
            },
        })];
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("tone, UI, aspect bars"),
            color_attachments: &attachments,
            ..Default::default()
        });
        pass.set_pipeline(&self.post);
        pass.set_bind_group(0, &self.post_bind, &[]);
        pass.draw(0..3, 0..1);
        drop(pass);
        upload
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{bloom::Bloom, color_pipeline::ColorPipeline};
    use std::time::Duration;

    fn adapter() -> Option<wgpu::Adapter> {
        let mut options = wgpu::InstanceDescriptor::new_without_display_handle();
        options.backends = wgpu::Backends::VULKAN | wgpu::Backends::METAL | wgpu::Backends::DX12;
        let instance = wgpu::Instance::new(options);
        match pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default()))
        {
            Ok(adapter) => {
                eprintln!("GPU offscreen tests: {:?}", adapter.get_info());
                Some(adapter)
            }
            Err(error) => {
                eprintln!("SKIP GPU offscreen tests: no adapter ({error})");
                None
            }
        }
    }

    fn read_frame(
        core: &PostProcessor,
        extent: [u32; 2],
        settings: ColorSettings,
        strength: f32,
        overlay: &[[f32; 4]],
        legacy: bool,
    ) -> Vec<[u8; 4]> {
        let format = if core.srgb {
            wgpu::TextureFormat::Rgba8UnormSrgb
        } else {
            wgpu::TextureFormat::Rgba8Unorm
        };
        let texture = core.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("offscreen test target"),
            size: wgpu::Extent3d {
                width: extent[0],
                height: extent[1],
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let row = (extent[0] * 4).div_ceil(256) * 256;
        let readback = core.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("test-only readback"),
            size: u64::from(row) * u64::from(extent[1]),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = core.device.create_command_encoder(&Default::default());
        core.encode(
            &mut encoder,
            &texture.create_view(&Default::default()),
            extent,
            PostSettings {
                color: settings,
                bloom: strength,
                profile: false,
            },
            overlay,
            legacy,
        );
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(row),
                    rows_per_image: Some(extent[1]),
                },
            },
            texture.size(),
        );
        let submission = core.queue.submit([encoder.finish()]);
        let (tx, rx) = std::sync::mpsc::channel();
        readback
            .slice(..)
            .map_async(wgpu::MapMode::Read, move |result| {
                let _ = tx.send(result);
            });
        core.device
            .poll(wgpu::PollType::Wait {
                submission_index: Some(submission),
                timeout: Some(Duration::from_secs(10)),
            })
            .unwrap();
        rx.recv_timeout(Duration::from_secs(10)).unwrap().unwrap();
        core.check_error().unwrap();
        let bytes = readback.slice(..).get_mapped_range().unwrap();
        let mut output = Vec::new();
        for y in 0..extent[1] as usize {
            for x in 0..extent[0] as usize {
                let i = y * row as usize + x * 4;
                output.push(bytes[i..i + 4].try_into().unwrap());
            }
        }
        drop(bytes);
        readback.unmap();
        output
    }

    fn near(actual: [u8; 4], rgb: u32, tolerance: u8) {
        let expected = [(rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8];
        for (a, b) in actual[..3].iter().zip(expected) {
            assert!(
                a.abs_diff(b) <= tolerance,
                "GPU {actual:?}, CPU {expected:?}, tolerance {tolerance}"
            );
        }
        assert_eq!(actual[3], 255);
    }

    #[test]
    fn bounded_extent_and_aspect_geometry() {
        for extent in [(0, 1), (1, 0), (usize::MAX, 4), (16384, 16384)] {
            assert!(checked_extent(extent.0, extent.1).is_err());
        }
        assert_eq!(checked_extent(640, 400).unwrap(), 256000);
        assert_eq!(
            viewport(640, 400, [1920, 1080]),
            [96.0, 0.0, 1728.0, 1080.0]
        );
        assert_eq!(viewport(640, 400, [640, 480]), [0.0, 40.0, 640.0, 400.0]);
    }

    #[test]
    fn offscreen_color_bloom_ui_and_validation() {
        let Some(adapter) = adapter() else {
            return;
        };
        for format in [
            wgpu::TextureFormat::Rgba8UnormSrgb,
            wgpu::TextureFormat::Rgba8Unorm,
        ] {
            let mut core = PostProcessor::new(&adapter, 7, 5, format).unwrap();
            let mut frame = vec![LinearRgb([0.2, 0.1, 0.05]); 35];
            frame[0] = LinearRgb([0.0; 3]);
            frame[1] = LinearRgb([10.0, 0.0, 1.0]);
            frame[2] = LinearRgb([0.003, 0.04, 0.5]);
            frame[17] = LinearRgb([8.0, 4.0, 2.0]);
            let configs = [
                ColorSettings::default(),
                ColorSettings {
                    gamma: 1.5,
                    saturation: 0.7,
                    brightness: 0.8,
                    fade: 0.9,
                    tone_mapping: true,
                },
                ColorSettings {
                    tone_mapping: false,
                    ..ColorSettings::default()
                },
                ColorSettings {
                    saturation: 0.0,
                    ..ColorSettings::default()
                },
                ColorSettings {
                    fade: 0.0,
                    ..ColorSettings::default()
                },
                ColorSettings {
                    gamma: 0.05,
                    saturation: 16.0,
                    brightness: 16.0,
                    ..ColorSettings::default()
                },
                ColorSettings {
                    gamma: 16.0,
                    saturation: 0.0,
                    ..ColorSettings::default()
                },
            ];
            for settings in configs {
                frame[3] = if settings.gamma == 0.05 {
                    LinearRgb([1.0e12, 0.0, 0.001])
                } else {
                    LinearRgb([0.2, 0.1, 0.05])
                };
                for strength in [0.0, 0.35] {
                    core.prepare_hdr(&frame, settings, strength, &[]).unwrap();
                    core.upload_hdr(&frame);
                    let actual = read_frame(&core, [7, 5], settings, strength, &[], false);
                    let pipeline = ColorPipeline::new(settings).unwrap();
                    let mut reference = frame.clone();
                    Bloom::new(7, 5)
                        .unwrap()
                        .apply(&mut reference, strength, pipeline.exposure())
                        .unwrap();
                    for (actual, expected) in actual.into_iter().zip(reference) {
                        near(actual, pipeline.encode(expected), 2);
                    }
                }
            }
            let rgb = vec![0x4080c0; 35];
            let mut overlay = vec![[0.0; 4]; 35];
            overlay[0] = [0.0, 0.0, 0.0, 1.0]; // Covered black, not transparency.
            overlay[1] = [0.0, 0.0, 0.0, 0.5]; // Display-space dimming.
            overlay[2] = [0.1, 0.05, 0.0, 0.75]; // Affine add plus dim.
            core.validate_rgb(&rgb, &overlay).unwrap();
            core.upload_rgb(&rgb);
            let actual = read_frame(&core, [7, 5], ColorSettings::default(), 0.0, &overlay, true);
            near(actual[0], 0, 0);
            near(actual[1], 0x204060, 1);
            near(actual[2], 0x2a2d30, 1);
            near(actual[3], 0x4080c0, 1);
            let bars = read_frame(&core, [7, 9], ColorSettings::default(), 0.0, &[], true);
            assert!(bars[..14].iter().all(|p| *p == [0, 0, 0, 255]));
            near(bars[14], 0x4080c0, 1);
            assert!(bars[49..].iter().all(|p| *p == [0, 0, 0, 255]));
            let before = frame.clone();
            assert!(core
                .prepare_hdr(&frame[..34], configs[0], 0.0, &[])
                .is_err());
            assert!(core.prepare_hdr(&frame, configs[0], f32::NAN, &[]).is_err());
            assert_eq!(frame, before);
            for invalid in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, -0.1, 1.1e12] {
                frame[0].0[0] = invalid;
                assert!(core.prepare_hdr(&frame, configs[0], 0.0, &[]).is_err());
                assert_eq!(frame[0].0[0].to_bits(), invalid.to_bits());
            }
            frame[0] = before[0];
            for channel in 0..4 {
                for invalid in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, -0.1, 1.1] {
                    overlay[0][channel] = invalid;
                    assert!(core.validate_rgb(&rgb, &overlay).is_err());
                    assert!(core.prepare_hdr(&frame, configs[0], 0.0, &overlay).is_err());
                    assert_eq!(overlay[0][channel].to_bits(), invalid.to_bits());
                }
                overlay[0][channel] = 0.0;
            }
            frame[0] = LinearRgb([-0.0; 3]);
            overlay[0] = [-0.0; 4];
            core.prepare_hdr(&frame, configs[0], 0.0, &overlay).unwrap();
            assert!(core
                .prepare_hdr(&frame, configs[0], 0.0, &overlay[..34])
                .is_err());
            // Failed CPU preparation never submits its partially packed data.
            // The previously uploaded RGB frame remains on the GPU unchanged.
            let after_error = read_frame(&core, [7, 5], ColorSettings::default(), 0.0, &[], true);
            for pixel in after_error {
                near(pixel, 0x4080c0, 1);
            }
        }
        for (width, height) in [(1, 1), (1, 7), (7, 1)] {
            let mut core =
                PostProcessor::new(&adapter, width, height, wgpu::TextureFormat::Rgba8Unorm)
                    .unwrap();
            let frame = vec![LinearRgb([4.0, 2.0, 1.0]); width * height];
            core.upload_hdr(&frame);
            let actual = read_frame(
                &core,
                [width as u32, height as u32],
                ColorSettings::default(),
                0.35,
                &[],
                false,
            );
            let mut reference = frame.clone();
            Bloom::new(width, height)
                .unwrap()
                .apply(&mut reference, 0.35, 2.0)
                .unwrap();
            let pipeline = ColorPipeline::new(ColorSettings::default()).unwrap();
            for (a, b) in actual.into_iter().zip(reference) {
                near(a, pipeline.encode(b), 2);
            }
        }
    }
}
