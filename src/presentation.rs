//! Main-thread presentation with an optional GPU post-process and SDL fallback.
//! The caller retains the SDL window; backends never present simultaneously.
use crate::color_pipeline::{ColorSettings, LinearRgb};
use sdl2::{pixels::PixelFormatEnum, render::Canvas, video::Window};
use std::ptr::NonNull;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackendChoice {
    Auto,
    Gpu,
    Software,
}
impl BackendChoice {
    pub fn from_environment() -> Result<Self, String> {
        match std::env::var("OPENCHASM_RENDERER").as_deref() {
            Err(std::env::VarError::NotPresent) | Ok("auto") => Ok(Self::Auto),
            Ok("gpu") => Ok(Self::Gpu),
            Ok("software") => Ok(Self::Software),
            _ => Err("OPENCHASM_RENDERER must be auto, gpu or software".into()),
        }
    }
}

/// Own the native texture together with its renderer. This avoids a texture
/// borrowing a movable TextureCreator while allowing backend replacement.
/// SDL Canvas makes the owner main-thread-only. The texture is destroyed in
/// Drop before the renderer/window fields are released.
struct SdlPresenter {
    texture: NonNull<sdl2::sys::SDL_Texture>,
    canvas: Canvas<Window>,
    width: usize,
    height: usize,
}
impl SdlPresenter {
    fn new(window: &Window, width: usize, height: usize) -> Result<Self, String> {
        if width == 0
            || height == 0
            || width > 16384
            || height > 16384
            || width
                .checked_mul(height)
                .is_none_or(|count| count > 16 * 1024 * 1024)
        {
            return Err("presentation dimensions outside 1..=16384".into());
        }
        let mut canvas = window
            .clone()
            .into_canvas()
            .build()
            .map_err(|e| e.to_string())?;
        canvas
            .set_logical_size(width as u32, height as u32)
            .map_err(|e| e.to_string())?;
        // SAFETY: canvas owns a valid SDL renderer on this thread. Dimensions
        // are positive and bounded, and SDL copies the format/access values.
        let raw = unsafe {
            sdl2::sys::SDL_CreateTexture(
                canvas.raw(),
                PixelFormatEnum::RGB888 as u32,
                sdl2::sys::SDL_TextureAccess::SDL_TEXTUREACCESS_STREAMING as i32,
                width as i32,
                height as i32,
            )
        };
        let texture = NonNull::new(raw).ok_or_else(sdl2::get_error)?;
        // RGB888 ignores the packed pixel's high byte. SDL's default texture
        // blend mode is NONE, preserving encoded RGB including opaque black.
        Ok(Self {
            texture,
            canvas,
            width,
            height,
        })
    }
    fn present(&mut self, pixels: &[u32]) -> Result<(), String> {
        if self.width.checked_mul(self.height) != Some(pixels.len()) {
            return Err("display frame length differs from presentation dimensions".into());
        }
        // SAFETY: texture is owned by self and its renderer is alive. The
        // source slice contains height rows with width*4 readable bytes; SDL
        // copies those bytes before returning. No pointer escapes this call.
        let updated = unsafe {
            sdl2::sys::SDL_UpdateTexture(
                self.texture.as_ptr(),
                std::ptr::null(),
                pixels.as_ptr().cast(),
                (self.width * std::mem::size_of::<u32>()) as i32,
            )
        };
        if updated != 0 {
            return Err(sdl2::get_error());
        }
        self.canvas.clear();
        // SAFETY: both objects are owned here and the texture belongs to this
        // renderer. Null rectangles request its full logical viewport.
        let copied = unsafe {
            sdl2::sys::SDL_RenderCopy(
                self.canvas.raw(),
                self.texture.as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
            )
        };
        if copied != 0 {
            return Err(sdl2::get_error());
        }
        self.canvas.present();
        Ok(())
    }
}
impl Drop for SdlPresenter {
    fn drop(&mut self) {
        // SAFETY: this is the unique texture owner. Struct fields (including
        // its Canvas/renderer) remain alive until after this Drop body.
        unsafe { sdl2::sys::SDL_DestroyTexture(self.texture.as_ptr()) };
    }
}

pub struct Presentation<'w> {
    window: &'w Window,
    width: usize,
    height: usize,
    sdl: Option<SdlPresenter>,
    #[cfg(feature = "gpu")]
    gpu: Option<crate::gpu_present::GpuPresenter<'w>>,
    notice: Option<String>,
}
impl<'w> Presentation<'w> {
    pub fn new(
        window: &'w Window,
        width: usize,
        height: usize,
        choice: BackendChoice,
    ) -> Result<Self, String> {
        let mut result = Self {
            window,
            width,
            height,
            sdl: None,
            #[cfg(feature = "gpu")]
            gpu: None,
            notice: None,
        };
        #[cfg(feature = "gpu")]
        if choice != BackendChoice::Software {
            match crate::gpu_present::GpuPresenter::new(window, width, height) {
                Ok(gpu) => {
                    if std::env::var("OPENCHASM_PROFILE").as_deref() == Ok("1") {
                        eprintln!("presentation GPU: {}", gpu.adapter_name());
                    }
                    result.gpu = Some(gpu);
                    return Ok(result);
                }
                Err(error) => result.notice = Some(format!("GPU unavailable; using SDL: {error}")),
            }
        }
        #[cfg(not(feature = "gpu"))]
        if choice == BackendChoice::Gpu {
            result.notice = Some("GPU feature is absent from this build; using SDL".into());
        }
        result.fallback_to_sdl()?;
        Ok(result)
    }
    pub fn backend_name(&self) -> &'static str {
        if self.is_gpu() {
            "gpu"
        } else {
            "software"
        }
    }
    pub fn set_backend(&mut self, choice: BackendChoice) -> Result<(), String> {
        if choice == BackendChoice::Software {
            return self.fallback_to_sdl();
        }
        #[cfg(feature = "gpu")]
        {
            if self.gpu.is_some() {
                return Ok(());
            }
            // Destroy the SDL renderer before creating a GPU surface for the
            // same window. Session, input and decoded content remain outside.
            self.sdl = None;
            match crate::gpu_present::GpuPresenter::new(self.window, self.width, self.height) {
                Ok(gpu) => {
                    self.gpu = Some(gpu);
                    return Ok(());
                }
                Err(error) => self.notice = Some(format!("GPU unavailable; using SDL: {error}")),
            }
        }
        #[cfg(not(feature = "gpu"))]
        if choice == BackendChoice::Gpu {
            self.notice = Some("GPU feature is absent from this build; using SDL".into());
        }
        self.fallback_to_sdl()
    }
    pub fn take_notice(&mut self) -> Option<String> {
        self.notice.take()
    }
    pub fn is_gpu(&self) -> bool {
        #[cfg(feature = "gpu")]
        {
            self.gpu.is_some()
        }
        #[cfg(not(feature = "gpu"))]
        {
            false
        }
    }
    pub fn fallback_to_sdl(&mut self) -> Result<(), String> {
        #[cfg(feature = "gpu")]
        {
            self.gpu = None;
        }
        if self.sdl.is_none() {
            let sdl = SdlPresenter::new(self.window, self.width, self.height)?;
            if std::env::var("OPENCHASM_PROFILE").as_deref() == Ok("1") {
                eprintln!("presentation renderer: {}", sdl.canvas.info().name);
            }
            self.sdl = Some(sdl);
        }
        Ok(())
    }
    pub fn present_sdl(&mut self, pixels: &[u32]) -> Result<(), String> {
        self.sdl
            .as_mut()
            .ok_or("SDL presenter is inactive")?
            .present(pixels)
    }
    pub fn present_hdr(
        &mut self,
        pixels: &[LinearRgb],
        settings: ColorSettings,
        bloom: f32,
        overlay: &[[f32; 4]],
    ) -> Result<bool, String> {
        #[cfg(feature = "gpu")]
        if let Some(gpu) = &mut self.gpu {
            return gpu
                .present_hdr(pixels, settings, bloom, overlay)
                .and_then(Self::frame_status);
        }
        let _ = (pixels, settings, bloom, overlay);
        Err("GPU presenter is inactive".into())
    }
    pub fn present_rgb(&mut self, pixels: &[u32], overlay: &[[f32; 4]]) -> Result<bool, String> {
        #[cfg(feature = "gpu")]
        if let Some(gpu) = &mut self.gpu {
            return gpu
                .present_rgb(pixels, overlay)
                .and_then(Self::frame_status);
        }
        let _ = (pixels, overlay);
        Err("GPU presenter is inactive".into())
    }
    #[cfg(feature = "gpu")]
    fn frame_status(status: crate::gpu_present::FrameStatus) -> Result<bool, String> {
        match status {
            crate::gpu_present::FrameStatus::Presented => Ok(true),
            crate::gpu_present::FrameStatus::Skipped => Ok(false),
            crate::gpu_present::FrameStatus::RecreateSurface => {
                Err("GPU surface was lost; switching to SDL".into())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[ignore = "run explicitly with SDL_VIDEODRIVER=dummy and --test-threads=1"]
    fn sdl_fallback_preserves_display_channels() {
        let sdl = sdl2::init().unwrap();
        let video = sdl.video().unwrap();
        let window = video
            .window("OpenChasm presentation test", 2, 2)
            .hidden()
            .build()
            .unwrap();
        let mut presenter = SdlPresenter::new(&window, 2, 2).unwrap();
        let pixels = [0xff0000u32, 0x00ff00, 0x0000ff, 0];
        presenter.present(&pixels).unwrap();
        let mut readback = [u32::MAX; 4];
        // SAFETY: renderer is alive; output has space for two 8-byte rows.
        let read = unsafe {
            sdl2::sys::SDL_RenderReadPixels(
                presenter.canvas.raw(),
                std::ptr::null(),
                PixelFormatEnum::RGB888 as u32,
                readback.as_mut_ptr().cast(),
                8,
            )
        };
        assert_eq!(read, 0, "{}", sdl2::get_error());
        assert_eq!(readback.map(|p| p & 0xffffff), pixels);
        assert!(presenter.present(&pixels[..3]).is_err());
    }
}
