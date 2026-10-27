# Rendering measurements

The visible FPS display and developer console report wall-clock FPS, average frame time, simulation time, rendering time, presentation time and longest frame in each half-second sample. `maxfps 0` removes the cap. Fixed simulation ticks continue while a lower render cap waits for its next presentation deadline.

Reproduce a release profile with installed local content:

```sh
cargo build --release --locked
OPENCHASM_PROFILE=1 OPENCHASM_UNCAPPED=1 ./target/release/openchasm --source local view 01
```

## Initial Linux measurements

The level-one spawn view was measured on the development Omarchy box using the native Wayland window, 640×400 software framebuffer, the then-current 90-degree vertical FOV, and no intentional movement. Both clone measurements used the same assets and camera configuration. Each ran for 14 seconds; the final sample after timeout-triggered pause is excluded. Values are approximate averages of the six regular samples.

| Build | FPS | Frame time | Simulation | Render | Present |
| --- | ---: | ---: | ---: | ---: | ---: |
| Before removing sky overdraw | 36.0 | 27.7 ms | 0.23 ms | 25.7 ms | 1.75 ms |
| After removing sky overdraw | 38.9 | 25.7 ms | 0.21 ms | 23.7 ms | 1.78 ms |

The optimization avoids sampling sky pixels that will be overwritten by valid solid planes. Colour and depth output were also compared independently before and after across five mixed-tile, angled, pitched, off-map and above-ceiling scenes, with byte-identical results.

The original remaster's own overlay showed approximately 60 FPS on this box in level one, with its normal GPU renderer, 1920×1080 fullscreen output, 75-degree vertical FOV and VSync enabled. This is an observed user-facing comparison, not an equal-resolution GPU-versus-CPU benchmark. The clone remains render-bound; the small optimization does not close the gap or establish performance on macOS/Windows.

Further profiling should split plane, wall, object, weapon and effect costs, measure frame pacing at the display's refresh rate, and include dense transparent and light-heavy scenes. Candidate work includes reducing per-triangle allocations/transforms, spatially filtering wall intersections and sharing transparent composition. A cross-platform GPU renderer is a candidate for the full HDR/perspective pipeline and more expensive optional effects.

The profiling output now includes separate plane, wall, model, effect and weapon
averages. The live SDL console was exercised through actual window input: typing
`echo wasd` left position unchanged; holding forward for 350 ms with `maxfps 1`
moved 0.875 tiles at the 2.5 tile/s movement speed, confirming rendering waits no
longer slow the fixed simulation. FPS/sensitivity controls and a clean console
`quit` were also exercised. This does not substitute for macOS/Windows input tests.

## Verified FOV, depth ordering and HDR

At the verified 75-degree vertical FOV and corrected world orientation, near-first wall rasterization reduced the wall pass from about 11.8 to 4.4 ms in the spawn view, raising the earlier RGB preview from about 36 to 50 FPS. Differential tests preserve exact frame/depth output across masked, tied and elevated walls. Removing redundant `floor()` operations for already-bounded plane coordinates also preserves exact RGB/HDR and depth output.

A subsequent native Wayland run at 640×400 measured these presets with the same spawn, uncapped rendering, and no concurrent builds. The table averages the first six regular samples before console interaction. Presentation was approximately 3.2–3.5 ms; desktop/window configuration can affect it.

| Preset | FPS | Render | Plane | Post-process |
| --- | ---: | ---: | ---: | ---: |
| Legacy RGB preview | 50.7 | 16.4 ms | 8.0 ms | 0.0 ms |
| Remaster linear HDR, neutral colour, bloom off | 39.9 | 21.7 ms | 7.9 ms | 6.0 ms |
| Enhanced linear HDR, bloom 0.35 | 30.6 | 28.9 ms | 7.9 ms | 12.9 ms |

The default HDR pipeline improves colour correctness at a measurable CPU cost. Enhanced bloom is optional; these numbers do not establish 60 FPS or native runtime performance on macOS/Windows. Non-neutral gamma/saturation are more expensive because their luminance/chroma powers are evaluated in the software post-process.

Live Linux checks exercised all three presets, individual colour controls, console queries and `options`, menu closing and mouse recapture, persisted cap changes, and clean console exits. A temporary SDL preference directory isolated these checks from the user's settings. Launching with `OPENCHASM_UNCAPPED=1` keeps the override in memory; unrelated graphics changes preserve the stored cap.

## Accelerated presentation

Letting SDL select its preferred presentation backend, with software fallback,
reduced final texture presentation to approximately 0.8 ms on this Linux box
(`opengl` was selected). The world remains software-rasterized. Repeating the
same uncapped preset checks showed roughly 57 FPS legacy, 44 FPS remaster HDR,
and 34 FPS enhanced. This targets scaling/blit overhead, not a GPU world
renderer, and still needs physical runtime validation on macOS/Windows.

## Directional static lighting cost

The first generated-lighting integration measured about 33.4 FPS at the same
level-one spawn (plane 11.6 ms, wall 5.8 ms, model 4.3 ms, post 6.0 ms), compared
with about 44 FPS for HDR using the former scalar field. This is an additional
CPU fidelity cost and remains a performance target. All 19 installed maps build
finite fields without skipping sources; release construction was 21 ms for
MAP01 and 5–44 ms across the maps. Moving geometry requires refreshed shadows,
so rebuild scheduling must avoid adding that construction time to every frame.
