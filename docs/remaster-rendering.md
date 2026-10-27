# Rendering facts from the installed remaster

Read-only analysis used the owned Steam `Chasm.exe` with SHA-256 `72c189ca808e026d59844c835fbc21a780038989fd22ff150479c0490ac5c949`. Executable and shader data were examined locally and are not distributed here. These facts improve on assumptions drawn from the independent reconstruction; they do not prove complete renderer equivalence.

## Projection

Normal gameplay reads the `Field Of View` configuration value, whose default is 75 and range is 45–120. The projection computes `cot(fov / 2)` for its Y diagonal and divides that by width/height for X. The configured angle is therefore **vertical**, giving approximately 101.67 degrees horizontally at 640×400.

The weapon uses a separate fixed vertical angle of `1.5679999589920044` radians (float bits `0x3fc8b439`), approximately 89.84 degrees. OpenChasm now uses these defaults with square-pixel focal lengths. Its pitched world rendering still uses a horizon shift rather than a complete rotated-camera perspective.

Evidence anchors in this executable: configuration/update chain `0x1401548a0`, `0x1401532d0`, `0x14000641a`; aspect calculation `0x1400059ee`; normal projection call `0x140006f7b`; projection routine `0x140023a60`; weapon constant `0x140761f94`.

## World orientation and controls

The original preserves the first and second map position words as X and Y. In OpenChasm's conventional yaw convention its view basis is `forward=(cos(yaw), sin(yaw))`, `right=(sin(yaw), -cos(yaw))`. The former clone used the opposite right vector and therefore mirrored the world. Rendering, strafing, right-turn keys and mouse yaw have been corrected together; the camera-relative weapon and HUD are not flipped.

Original right turns and positive mouse-X deltas decrease yaw. The raw spawn angle resolves to the same conventional forward yaw used by the clone (`rawAngle * pi/4 + pi`, modulo a full turn), so the former visual difference was not a compensating map-axis swap. At the level-01 spawn `[31.5,58.5]` facing `3*pi/2`, streetlight `[29.5,54.5]` must appear on the right.

Evidence anchors: view rotation `0x140023bc0`; spawn parsing and angle conversion `0x1401a7a50`–`0x1401a7c33`; keyboard action labels `0x140085240`; keyboard turning `0x14002379c`; movement vector transform `0x1400238e9`; mouse-X subtraction `0x14008673e`.

## Colour pipeline

The executable's own format mapping identifies wall, floor and model material textures as sRGB, the scene target as `R16G16B16A16_SFLOAT`, and the output as `B8G8R8A8_SRGB`. The confirmed neutral, fully faded-in path is:

1. Decode material sRGB into linear colour.
2. Apply lighting in linear HDR space, retaining values above one.
3. Multiply by two at neutral brightness and full fade-in.
4. Apply ACES tone mapping when enabled.
5. Encode sRGB for display.

The component-wise ACES curve is `clamp(x*(2.51*x+0.03)/(x*(2.43*x+0.59)+0.14), 0, 1)`. Gamma adjustment uses luminance weights `(0.2126, 0.7152, 0.0722)`, normalized chroma and inverse gamma; saturation operates on that chroma. The brightness uniform is `2 * configured_brightness * fade^3`. Apply the world and weapon post-process before the HUD, which is drawn afterward.

Evidence anchors: gamma shader registration `0x140055140`, uniform setup `0x14018c370`, material creation `0x1401885fc`, scene target `0x14018896a`, format mapping `0x1401ff120`, swapchain format selection `0x14007bd13`. Weapon calls `0x14018bea5`/`0x14018bf16` precede gamma `0x14018bf58`; the later HUD draw is `0x1400075a8`.

Applying ACES directly to already shaded RGB bytes would be incorrect. The software renderer now has a linear HDR colour path and final post-process for this chain. Material decoding, lighting and alpha blending retain HDR values; the world and weapon share post-processing, while the HUD remains in display space. Current authored light sampling is still not equivalent to the full remaster lighting system. Damage feedback remains a provisional display-space tint.

## Light data distinction

The original conversion helper uses `trunc((35 - raw) * 255 / 35)` for supported fine raw light values 0–35. The separate 64×64 ambient field is converted and remapped; verified examples are raw 0→255, 5→177, 10→102 and 15→25. Ambient is sampled as linear `R8_UNORM` and added to a separate HD lightmap in the wall shader. That HD lightmap is generated from map light-source records. The role of the fine 256×256 authored field in active HD rendering has not yet been proven.

Consequently the current independent-reference conversion `(255 - 6*raw) * 1.3/255` is provisional. Replacing only that scalar with the fine-field helper is insufficient to reproduce the remaster's ambient, generated lighting, distance response, shadows and HDR output.

## Selectable presentation

The default `remaster` preset uses the verified colour pipeline with neutral gamma, brightness and saturation, ACES enabled, and bloom disabled. `legacy` retains the earlier RGB-byte software shading for comparison; it is not a claim to reproduce the DOS renderer. `enhanced` adds optional linear HDR bloom at strength 0.35. Bloom extracts exposed highlights before downsampling, applies a separable Gaussian blur at half resolution, and composites before tone mapping. HUD text does not bloom.

Open **Escape → Graphics Options**, or enter `options` in the tilde console. `graphics remaster`, `graphics enhanced` and `graphics legacy` switch presets; `tonemap`, `gamma`, `brightness`, `saturation` and `bloom` adjust controls individually. Explicit changes are saved in SDL's per-user OpenChasm preference directory as a versioned `graphics.json`. Presets reset visual controls and preserve the FPS cap, counter and mouse sensitivity.
