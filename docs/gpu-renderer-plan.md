# Optional GPU renderer plan

Research checked 2026-10-27. The first optional post-processing increment is now implemented and tested on
Linux; the genuine 3D stage remains a proposal. Existing simulation, resource decoding,
software rendering and original-renderer research remain the reference. Shaders
would be written independently from the documented mathematics; no original
shader binaries or game assets belong in the repository.

## Recommendation and expected scope

Add an optional wgpu presenter that first consumes the existing scene-linear HDR
image, runs bloom and color conversion on the GPU, composites UI, and presents
directly. Keep SDL events, audio, relative mouse input and the software presenter.
Then replace CPU world rasterization with genuine 3D geometry in a separate
increment. The first increment cannot repair the CPU renderer's perspective or
make high-resolution world rendering inexpensive.

The reported 33 FPS is about 30.3 ms/frame. Removing a reported 6–13 ms CPU post
stage would imply about 41–58 FPS **before** charging upload, GPU work and changed
presentation behavior. This is a ceiling calculation, not a benchmark or promise.
SDL presentation already costs approximately 0.8 ms in the reported measurement;
replacing only the final RGB blit has little potential benefit.

Keep graphics appearance (`legacy`, `remaster`, optional enhancements) independent
from execution backend (`auto`, `gpu`, `software`). Original appearance retains
bloom off. A backend failure must preserve the session and its settings.

## Versioned dependencies and feature boundary

The current stable release found is **wgpu 30.0.1**, published 2026-08-22, with
raw-window-handle `^0.6.2`. Its package declares Rust 1.87.0. SDL2 0.38.0's optional
raw-window-handle `0.6.0` requirement is semver-compatible with 0.6.2. Its bundled
wgpu example targets **wgpu 0.20**, so it is useful for SDL window construction,
not current wgpu API syntax. [wgpu release and dependencies](https://docs.rs/crate/wgpu/30.0.1),
[wgpu package manifest](https://docs.rs/crate/wgpu/30.0.1/source/Cargo.toml),
[SDL2 0.38.0 manifest](https://github.com/Rust-SDL2/rust-sdl2/blob/0.38.0/Cargo.toml).

Manifest feature boundary used by the implementation:

```toml
[features]
gpu = ["client", "dep:wgpu", "dep:pollster", "dep:bytemuck", "sdl2/raw-window-handle"]

[dependencies]
wgpu = { version = "30.0.1", optional = true, default-features = false, features = ["std", "parking_lot", "vulkan", "metal", "dx12", "wgsl"] }
pollster = { version = "1.0.1", optional = true }
bytemuck = { version = "1.25.2", optional = true }
```

These wgpu features are published for 30.0.1. Pollster provides a small blocking
executor for initialization; bytemuck can safely expose explicitly laid out
upload data as bytes. These helper versions were also checked against their
published crates. [wgpu features](https://docs.rs/crate/wgpu/30.0.1/features),
[pollster 1.0.1](https://docs.rs/crate/pollster/1.0.1),
[bytemuck 1.25.2](https://docs.rs/crate/bytemuck/1.25.2).

Initially retain the existing default `client` feature. Opt in with
`--features gpu`; keep GPU modules behind client-side `cfg` declarations. The
headless library, deterministic simulation, resource traces and server builds
must not acquire SDL/wgpu imports or a display requirement. Commit the resolved
lockfile when implementation is authorized. Adding the GPU feature to release
defaults should follow native validation.

Use Vulkan on Linux, Metal on macOS and DX12 on Windows initially. Exclude GLES
from this first feature set: in wgpu 30 its instance needs a display handle for
presentation, especially on Wayland. Vulkan, Metal and DX12 currently do not use
that instance field. This keeps SDL ownership adaptation smaller; existing SDL
presentation remains the fallback. This is a scope choice, not a statement that
wgpu cannot support GLES. [InstanceDescriptor 30.0.1](https://docs.rs/wgpu/30.0.1/wgpu/struct.InstanceDescriptor.html).

## SDL window ownership and native surfaces

Build an ordinary resizable SDL window, enabling `.metal_view()` and
`.allow_highdpi()`. The former is essential on macOS: SDL2 0.38's AppKit handle
implementation obtains the NSView from its stored Metal view and panics when it
is missing. On Wayland the implementation supplies `wl_surface` and `wl_display`;
on Windows it supplies HWND and instance; X11 uses Xlib handles. The SDL source
is more precise here than examples written for older releases.
[SDL raw handle implementation](https://github.com/Rust-SDL2/rust-sdl2/blob/0.38.0/src/sdl2/raw_window_handle.rs),
[SDL window builder source](https://github.com/Rust-SDL2/rust-sdl2/blob/0.38.0/src/sdl2/video.rs).

SDL `Window` is cloneable through an Rc-backed context and is explicitly
`!Send`/`!Sync`. Wgpu's safe native window target requires its window-handle
trait's thread-safety bounds. Wrapping SDL's window in `Arc` does not satisfy
them. Keep SDL ownership and surface creation on the main thread, including
macOS. Do not invent unsafe `Send`/`Sync` implementations.
[SDL Window](https://docs.rs/sdl2/0.38.0/sdl2/video/struct.Window.html),
[wgpu WindowHandle](https://docs.rs/wgpu/30.0.1/wgpu/trait.WindowHandle.html),
[wgpu SurfaceTarget](https://docs.rs/wgpu/30.0.1/wgpu/enum.SurfaceTarget.html).

Use a small, documented unsafe boundary around
`SurfaceTargetUnsafe::from_display_and_window(window, window)` and
`Instance::create_surface_unsafe`. In 30.0.1, `from_window` alone omits the display
and requires it to be supplied through the instance instead. Both underlying
handles must remain valid until after the surface is dropped.
[SurfaceTargetUnsafe 30.0.1](https://docs.rs/wgpu/30.0.1/wgpu/enum.SurfaceTargetUnsafe.html).

Prefer an explicit borrowed owner:

```text
main-thread SDL Window
    -> GpuPresenter<'window> { Surface<'window>, &'window Window, GPU resources }
```

Keep the SDL/video context alive for that scope. This ties the otherwise unsafe
surface lifetime to a real window borrow and makes the presenter main-thread
bound. Drop acquired images and the presenter before consuming the window into
an SDL Canvas. Never let SDL Canvas and wgpu both present to the same window.
On a driver that cannot transition cleanly, recreate the presentation window
while preserving the Session; validate this fallback on each platform.

## Current initialization and frame API

Create an `InstanceDescriptor::new_without_display_handle()`, set the selected
backends and pass it by value to `Instance::new`. Create the surface, then request
an adapter with `compatible_surface`. `request_adapter` returns a **Result**.
Request only supported limits/features; initial post processing needs no optional
timestamp or float32-filtering feature. `request_device` takes one device
descriptor, without the older second trace argument. Use the current descriptor's
defaults for fields not intentionally changed.
[Instance API](https://docs.rs/wgpu/30.0.1/wgpu/struct.Instance.html),
[Adapter API](https://docs.rs/wgpu/30.0.1/wgpu/struct.Adapter.html),
[DeviceDescriptor](https://docs.rs/wgpu/30.0.1/wgpu/type.DeviceDescriptor.html).

Query capabilities; select a supported format and composite-alpha mode. Start
with an SDR sRGB format and `SurfaceColorSpace::Auto`, not HDR-monitor output.
Use FIFO or AutoVsync initially; only FIFO is guaranteed. Set nonzero physical
pixel dimensions, `RENDER_ATTACHMENT`, and a conservative frame-latency hint.
The descriptor now includes `color_space`; older example struct literals miss
it. [SurfaceConfiguration 30.0.1](https://docs.rs/wgpu/30.0.1/wgpu/type.SurfaceConfiguration.html).

Query physical dimensions with SDL's `SDL_GetWindowSizeInPixels` through the
existing sys bindings, rather than assuming logical window size equals drawable
size. SDL documents this API from 2.26.0; bundled SDL and generated bindings must
be checked in the actual build. Handle minimize/zero sizes without configuring
or drawing. Requery on resize and display changes, checking adapter limits and
allocation bounds before allocating. [SDL pixel-size API](https://wiki.libsdl.org/SDL2/SDL_GetWindowSizeInPixels).

Wgpu 30's acquisition returns `CurrentSurfaceTexture`, not the older Result:

| Outcome | Application action |
| --- | --- |
| `Success(image)` | Encode, submit, present. |
| `Suboptimal(image)` | Render/present it, then reconfigure after releasing it. |
| `Timeout` / `Occluded` | Skip presentation; wait for a later useful frame/event. |
| `Outdated` | Requery dimensions and reconfigure. |
| `Lost` | Recreate surface; if device also lost, rebuild device/resources. |
| `Validation` | Report the validation failure; do not spin retrying unchanged state. |

This follows the versioned enum's documented distinctions.
[CurrentSurfaceTexture 30.0.1](https://docs.rs/wgpu/30.0.1/wgpu/enum.CurrentSurfaceTexture.html).

Call `queue.submit(...)`, then **`queue.present(image)`**. Do not transplant an
old `image.present()` example. Wgpu presentation performs the Wayland surface
attachment/commit itself. Reconfigure only on relevant changes, with no live
acquired image; configuration may wait for GPU idle.
[Queue presentation](https://docs.rs/wgpu/30.0.1/wgpu/struct.Queue.html#method.present),
[Surface configuration](https://docs.rs/wgpu/30.0.1/wgpu/struct.Surface.html#method.configure).

Initialization failure returns a useful reason and chooses the SDL presenter in
auto mode. Runtime recovery should be bounded, retaining Session and rebuilding
only renderer state. Rebase frame timing after a long rebuild. Adapter diagnostics
should identify backend/device and a software-adapter selection; do not silently
assume a CPU Vulkan adapter will accelerate this workload.

## Increment one: GPU post processing without readback

Suggested client interface, independent of SDL event handling:

```text
GpuPresenter::new(&Window, internal_extent, settings) -> Result<Self, InitError>
resize_surface(physical_extent) -> Result<(), PresentError>
present_hdr(&[LinearRgb], &UiOverlay, post_settings) -> Result<FrameStatus, PresentError>
present_legacy(&[u32], &UiOverlay) -> Result<FrameStatus, PresentError>
```

Keep CPU world and weapon rendering unchanged, but bypass CPU bloom and
`ColorPipeline::encode_frame` for GPU HDR presentation. Reuse bounded GPU input,
half-resolution bright/blur storage, uniforms and UI storage across frames. The
legacy path uploads already encoded RGB and bypasses HDR correction.

For the first prototype, a flat RGB `array<f32>` storage buffer avoids half-float
range loss and filterable-float-texture requirements. A WGSL `array<vec3<f32>>`
has a different array stride: do not upload tightly packed RGB into it. Existing
`LinearRgb` has Rust's default representation and is not declared Pod. Either
pack into a reusable `Vec<f32>` and safely cast that slice, or separately audit
and declare a stable upload representation. No unchecked cast of the current
struct is justified merely by observing its current size.

RGB float upload is 3,072,000 bytes at 640×400, approximately 184 MB/s at 60 FPS
before staging copies. RGBA32F uses 4,096,000 bytes; RGBA16F uses 2,048,000 but
requires conversion and has a finite range far below current HDR input bounds.
Measure packing and transfer time before choosing a narrower representation.
Validate byte counts, storage-buffer limits, dispatch dimensions and image sizes.

`Queue::write_buffer` copies into staging memory immediately and transfers on
submission; it is sufficient for a correctness prototype. Its native staging
allocation means reusing application buffers alone does **not** prove a
zero-allocation frame. If profiling warrants it, use a bounded staging ring or
StagingBelt. Never wait synchronously for completion every gameplay frame.
[Queue upload behavior](https://docs.rs/wgpu/30.0.1/wgpu/struct.Queue.html#method.write_buffer).

If using textures, `Queue::write_texture` does not impose the buffer-copy
256-byte row alignment; explicit buffer-to-texture copies do. Check format
features before requesting float texture filtering; manual loads/interpolation
can avoid that optional requirement.
[TexelCopyBufferLayout](https://docs.rs/wgpu/30.0.1/wgpu/struct.TexelCopyBufferLayout.html),
[TextureFormat capabilities](https://docs.rs/wgpu/30.0.1/wgpu/enum.TextureFormat.html).

The GPU pass order should follow existing code:

1. World and weapon scene-linear HDR input.
2. Optional bright extraction and half-resolution separable Gaussian bloom.
3. Add upsampled bloom to scene-linear HDR.
4. Luminance-based gamma/chroma saturation, exposure/fade, optional ACES curve,
   and display encoding, matching `color_pipeline.rs` and its research evidence.
5. Damage tint and UI in their existing order, after world correction.
6. Presentation, preserving the internal aspect ratio.

Reproduce the current bloom's threshold after exposure, chroma-preserving
extraction, valid-pixel average on odd-sized edges, clamped five-tap convolution
and bilinear upsample coordinates. Strength zero bypasses all bloom passes.
Keep HDR values above one until the tone stage. Match validated settings and
finite-input handling rather than introducing a different automatic exposure.

GPU analytic ACES/sRGB and the CPU's interpolated lookup table can differ near
8-bit boundaries. Compare against both the analytic reference and current CPU
output; document tolerances instead of requiring an unjustified bitwise match.

## HUD and menus: avoid the readback trap

The present `Hud::render` writes `0x00RRGGBB`, discarding alpha. Drawing it onto
an initially transparent image and uploading that as RGBA would leave its pixels
transparent. Black is valid artwork, so black cannot serve as a transparency key.
The graphics menu also halves the existing destination color, and the console
uses destination/16 plus a color bias. These are background-dependent operations,
not just opaque sprite writes. This was verified in `hud.rs`,
`graphics_settings.rs`, `developer_console.rs` and `viewer.rs`.

A second UI texture is a reasonable first implementation, but requires an
alpha-aware canvas or explicit coverage. Keep opaque black pixels covered,
transparent atlas pixels uncovered, and preserve draw ordering. Never read the
post-processed GPU image back just so the current CPU UI can modify it.

One design is an overlay that represents a per-pixel display-space operation
`result = background * gain + add_rgb`. Initially `gain=1, add=0`; an opaque write
sets `gain=0, add=color`. A dim multiplies both existing terms, and console bias
adds to `add`. The current operations have scalar gain, so RGBA can store
`add_rgb` and `alpha=1-gain`, with the final shader evaluating the expression.
This composes damage tint, HUD, menu and console without knowing the world pixel.
Test quantization: collapsing multiple byte-rounded operations into one float
expression need not be bit-exact. A float overlay can reduce packing error, but
RGBA8 is cheaper if measured differences are acceptable.

Alternatively, emit ordered UI commands and upload atlases once: screen dim,
sprite/text quads, rectangles and console dim/bias. This is a larger initial UI
refactor but eliminates a full overlay upload and can be reused by the 3D backend.
At 640×400 an RGBA8 overlay adds 1,024,000 bytes/frame, still far preferable to a
synchronous readback. Neither option requires gameplay readback.

Color-space contract must be explicit. Current UI colors are display-encoded
bytes and its dimming is performed in that space. For a compatibility overlay,
sample UI data without automatic sRGB decoding, encode the tone-mapped world,
compose in encoded space, then decode the combined result before writing an
sRGB surface attachment that performs hardware encoding. A supported unorm
surface can instead receive the encoded result directly. Do not encode twice,
apply ACES to the HUD, or accidentally change existing menu dimming to linear
blending. Later physically linear UI blending is a separate visual change.

Initially keep the same 640×400 internal viewport and aspect-preserving bars as
the software path. Draw the normal HUD at that internal resolution and preserve
nearest integer atlas scaling. Changing internal resolution, window aspect and
HUD scaling simultaneously would make the first comparison ambiguous.

## Increment two: genuine 3D world rendering

Reuse the post/UI stage while replacing the HDR producer. Build static floor,
ceiling and wall meshes, with GPU depth testing, perspective-correct UVs, proper
near clipping and a full view/projection matrix. Preserve the verified world
orientation, vertical FOV and distinct weapon projection; do not inherit the CPU
column renderer's pitch approximation. Check winding/culling against the map's
coordinate convention before enabling back-face culling.

Upload the precomputed directional light field and ambient data once per rebuild,
preserving signed components, ambient indexing, bilinear repeat sampling and
normal/distance response from `remaster_light.rs`. Decode material sRGB before
lighting. Keep world and weapon in HDR until the shared tone stage. Update moving
walls/doors and animation buffers incrementally. Handle alpha-cutout materials,
double-sided surfaces, sprites and transparent sorting/depth behavior explicitly.

This phase should add a configurable internal resolution and real aspect-aware
projection. Keep optional enhancement bloom and future filtering/antialiasing
separate from original appearance. Higher resolution should not silently change
simulation, lighting resource interpretation or authored HUD placement.

## Implementation checkpoints and validation

1. Add feature-gated presenter and a colored test frame; verify window ownership,
   adapter selection, resize, physical sizing and software fallback first.
2. Upload HDR and port color mathematics; compare fixed synthetic images and a
   stationary scene with bloom off before adding other effects.
3. Port bloom, then explicit UI coverage/composition. Remove CPU post work only
   from the GPU path. Keep identical settings and gameplay data.
4. Profile complete frames and bounded resource reuse, including upload/packing,
   submission, presentation wait and CPU world work. Optional timestamp queries
   require capability checks; CPU submission timing alone is not GPU duration.
5. Validate native platforms and recovery. Only then consider default enablement
   and the separate 3D geometry implementation.

Run the existing headless tests with no default features, the default client
checks, and the GPU feature checks independently. Add GPU offscreen tests for
black, gray, saturated channels, HDR highlights, nonneutral gamma/saturation,
fade, bloom impulses/uniform fields, odd/tiny sizes and disabled bloom. Readback
is appropriate in these tests and explicit screenshot export, not in play.
Exercise UI opaque black, transparent holes, ordering, dimming, channel order
and packing. Define tolerances before comparing GPU and CPU outputs.

Native validation remains required; successful cross-compilation is insufficient:

| Platform | Initial backend and necessary checks |
| --- | --- |
| Linux Wayland | Vulkan; relative mouse/focus, compositor scaling, resize, fullscreen, minimize/occlusion and fallback. |
| Linux X11 | Vulkan; same frame/color comparison and SDL handle/surface lifetime checks. |
| Windows 10/11 | DX12 on real hardware; resize, Alt-Tab, DPI changes, adapter failure and device/surface recovery. |
| macOS Apple Silicon | Metal; main-thread construction, Metal view, Retina physical sizing and monitor changes. |
| macOS Intel, if supported | Separate native check before claiming support for this build. |

Also test explicit software mode, GPU-unavailable startup, forced recovery paths,
unsupported limits, repeated window creation/destruction, and no per-frame
unbounded allocation. Coordinate FIFO with the existing frame limiter to avoid
double throttling. Compare paired captures with the same camera, dimensions,
settings and simulation time; different original-game poses cannot establish
pixel parity.

The optional GPU presenter, independent WGSL post/bloom shaders and affine UI
canvas are implemented. Native Linux testing selected Intel UHD 620 Vulkan and
exercised all three appearance presets, colour controls, menus, mouse capture,
clean quit and live GPU↔SDL switching. Offscreen tests ran on that hardware
with sRGB/unorm targets, extreme settings, tiny/odd images, bloom, opaque black
coverage and invalid-input state preservation. Native macOS/Windows GPU runtime
validation remains outstanding. CI now checks both feature configurations on
all three platforms. The backend still consumes a CPU-rasterized world image;
it is not a high-resolution 3D world renderer.
