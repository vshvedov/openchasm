# First-person mouse input research

The current `minifb 0.29` API exposes absolute pointer position and cursor visibility, but no public relative-motion or cursor-grab API. Absolute deltas stop at screen edges; cursor warping is not a reliable cross-platform substitute, especially under Wayland.

## Recommendation

Use `sdl2 0.38` for the window and input backend, presenting the existing software framebuffer through a streaming texture. SDL2's relative mouse mode hides and confines the cursor and reports continuous motion; its [official documentation](https://wiki.libsdl.org/SDL2/SDL_SetRelativeMouseMode) describes failure behavior. In the Rust binding, `MouseUtil::set_relative_mouse_mode` returns `()`, so check `relative_mouse_mode()` afterward and retain keyboard turning when capture is unavailable.

SDL's polling event loop fits the existing fixed-step loop with a comparatively small migration. Keep simulation and rendering in the existing modules; replace only window creation, event collection, and framebuffer presentation. Use `xrel`/`yrel` from `Event::MouseMotion`, accumulate deltas until a simulation tick, then consume them once. Do **not** multiply mouse deltas by frame time: each is already displacement. Add pitch only after the world renderer and simulation support it. Disable capture and clear pending input when the window loses focus; Escape should release it, and a deliberate click can reacquire it. Don't recapture while a menu is active. Treat the capture click separately from firing.

Dependency tradeoff: `sdl2 = "0.38"` dynamically links a native SDL2 runtime. Developer machines and CI need SDL2 headers/libraries; release packages need the runtime too. The Rust binding's `bundled` feature can build SDL2 from source, but requires a working C/CMake toolchain and must be verified per target. Avoid SDL image/mixer/TTF features; existing Rust image and `rodio` audio paths can stay. Linux should build and run against both Wayland and X11; Windows and macOS need their own CI and manual input tests. See the [Rust SDL2 project](https://github.com/Rust-SDL2/rust-sdl2) for feature and linking instructions.

A viable alternative is `winit 0.30.13` plus `softbuffer 0.4.8`: `DeviceEvent::MouseMotion` supplies relative data, and `Window::set_cursor_grab` can try `Locked` then `Confined` with error handling. Softbuffer supports the current CPU framebuffer on desktop platforms. It avoids an SDL2 runtime but needs a larger event-loop rewrite; default features also include extra Linux backends that can be disabled deliberately. See [winit's window API](https://docs.rs/winit/0.30.13/winit/window/struct.Window.html) and [softbuffer](https://docs.rs/softbuffer/0.4.8/softbuffer/).

## What was tested locally

On this Omarchy machine, small Rust probes using system SDL2 and using winit+softbuffer both built and opened/presented frames under native Wayland and X11/XWayland. SDL2 reported relative mode enabled on both backends; winit reported a successful grab on both. A synthetic XTest relative move produced no observed raw delta in the winit probe, so **actual physical mouse movement, unbounded turning, focus recovery, and SDL input deltas are not verified**. Both Windows and macOS behavior and packaging remain untested. Run a human-controlled capture test on each platform before switching the default backend; verify the cursor returns on focus loss, Escape and exit.

## Minimal SDL2 probe

This standalone example was built locally with `sdl2 = "0.38.0"`. Click the window to capture, move the mouse to log yaw, and press Escape to release. Close the window to quit. It uses a solid streaming framebuffer in place of the game renderer.

```toml
[dependencies]
sdl2 = "0.38.0"
```

```rust
use std::time::{Duration, Instant};
use sdl2::{event::{Event, WindowEvent}, keyboard::Keycode, mouse::MouseButton, pixels::PixelFormatEnum};

fn main() -> Result<(), String> {
    let sdl = sdl2::init()?;
    let video = sdl.video()?;
    let window = video.window("Relative input probe", 640, 400).position_centered().build().map_err(|e| e.to_string())?;
    let mut canvas = window.into_canvas().software().build().map_err(|e| e.to_string())?;
    let creator = canvas.texture_creator();
    let mut texture = creator.create_texture_streaming(PixelFormatEnum::ARGB8888, 640, 400).map_err(|e| e.to_string())?;
    let mut events = sdl.event_pump()?;
    let mut focused = false;
    let mut pending_dx = 0_i64;
    let mut yaw = 0.0_f32;
    let mut previous = Instant::now();
    let mut accumulator = 0.0_f32;
    'running: loop {
        for event in events.poll_iter() {
            match event {
                Event::Quit { .. } => break 'running,
                Event::Window { win_event: WindowEvent::FocusGained, .. } => focused = true,
                Event::Window { win_event: WindowEvent::FocusLost, .. } => {
                    focused = false;
                    sdl.mouse().set_relative_mouse_mode(false);
                    pending_dx = 0;
                }
                Event::KeyDown { keycode: Some(Keycode::Escape), .. } => {
                    sdl.mouse().set_relative_mouse_mode(false);
                    pending_dx = 0;
                }
                Event::MouseButtonDown { mouse_btn: MouseButton::Left, .. } if focused => {
                    sdl.mouse().set_relative_mouse_mode(true);
                    if !sdl.mouse().relative_mouse_mode() { eprintln!("Relative mouse mode unavailable"); }
                }
                Event::MouseMotion { xrel, .. } if focused && sdl.mouse().relative_mouse_mode() => {
                    pending_dx = pending_dx.saturating_add(i64::from(xrel));
                }
                _ => {}
            }
        }
        let now = Instant::now();
        accumulator += now.duration_since(previous).as_secs_f32().min(0.1);
        previous = now;
        while accumulator >= 1.0 / 60.0 {
            // Relative pixels are a displacement, not a rate: don't multiply by dt.
            yaw += pending_dx as f32 * 0.002;
            if pending_dx != 0 { eprintln!("yaw={yaw:.3}"); }
            pending_dx = 0; // consume each motion event exactly once
            accumulator -= 1.0 / 60.0;
        }
        texture.with_lock(None, |bytes, pitch| {
            for row in bytes.chunks_mut(pitch).take(400) {
                for pixel in row[..640 * 4].chunks_exact_mut(4) {
                    pixel.copy_from_slice(&0xff303847_u32.to_ne_bytes());
                }
            }
        }).map_err(|e| e.to_string())?;
        canvas.copy(&texture, None, None)?;
        canvas.present();
        std::thread::sleep(Duration::from_millis(4));
    }
    sdl.mouse().set_relative_mouse_mode(false);
    Ok(())
}
```

For integration, replace the solid color with the existing `u32` image. Copy row by row respecting SDL's texture pitch and the selected pixel format. Clear held keys/buttons and accumulated mouse deltas on focus loss; route input to menus before simulation; apply sensitivity and optional invert-Y once to the tick input. Avoid storing platform window handles or OS events in the server simulation.
