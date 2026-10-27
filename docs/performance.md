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
