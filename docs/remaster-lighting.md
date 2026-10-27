# Lighting facts from the installed remaster

These results come from read-only analysis of the owned Steam `Chasm.exe`, SHA-256 `72c189ca808e026d59844c835fbc21a780038989fd22ff150479c0490ac5c949`. Addresses below are virtual addresses in that executable. Shader names identify its locally inspected embedded SPIR-V. No executable, shader binary, decompiled source, or game asset is included here. Mathematical descriptions below are observations, not claims that OpenChasm already implements them.

## Ambient field

The 64×64 ambient field is separate from the authored 256×256 fine light field. For an authored ambient byte `r`, the observed conversion is:

```text
b = trunc_toward_zero((35 - r) * 255 / 35)
a = 25 + trunc_toward_zero((b - 145) * 230 / 110)
ambient = byte(a) / 255
```

The first division is integer arithmetic, not a floating-point interpolation. Supported fine-light input to the first helper is 0–35; observed MAP01 ambient values are 0–15. In that ambient range, examples are 0→255, 5→177, 10→102, and 15→25. Behavior for malformed/out-of-range authored values is not a recommended public API contract.

Evidence: conversion helper `0x140091c30`; ambient loader `0x1400984e3`–`0x140098522`; texture creation `0x140043711`–`0x14004378b`; upload `0x1400461e0`. The texture is 64×64 `R8_UNORM`. The loader transposes source indexing `x*64+y` into GPU row-major `y*64+x`. OpenChasm can keep its raw x-major layout if logical lookup is consistent.

The world fragment shaders fetch ambient with integer coordinates obtained by truncating `lightmapUV*64`, at mip zero. Ambient is not bilinearly filtered. They add it to generated lighting before multiplying linear material RGB.

The fine field has a separate loader/helper call at `0x140098450`. Its use in active HD shading has not been established; substituting its converted bytes for the generated HD field is unsupported.

## Quality and generated field format

The renderer reads Shading Quality (configuration entry 9, offset `0x438`) at `0x14018831c`–`0x14018832a`. Thresholds 0.5, 1.5, and 2.5 select these variants:

| Quality | Field size | Variant | Target |
| --- | --- | --- | --- |
| 0 | 256×256 | N0 | R16_SFLOAT |
| 1 | 512×512 | N0 | R16_SFLOAT |
| 2 | 1024×1024 | N1 | R16G16B16A16_SFLOAT |
| 3 | 2048×2048 | N1 | R16G16B16A16_SFLOAT |

The normals switch is enabled at size ≥1024 (`0x140188379`–`0x14018837e`). Format selection is at `0x14004379c`–`0x1400437b1`; the original format-name mapping at `0x1401ff120` identifies enums `0x2b` and `0x6a`. The inspected user configuration selects quality 2.

The field covers the 64×64-tile map. For size `N`, texel-center world coordinates are `((i+0.5)*64/N, (j+0.5)*64/N)` when represented in conventional CPU row-major coordinates.

## Map light record conversion

Each source occupies 12 bytes, six little-endian 16-bit words. Preserve raw indices and bits. In particular, converting every field to an unsigned magnitude loses meaningful signed values.

| Offset | Observed interpretation |
| --- | --- |
| +0, +2 | signed X, Y, divided by 256 |
| +4 | signed power; use absolute value, divided by 128 |
| +6 | signed inner radius, divided by 256 if less than signed outer radius; otherwise zero |
| +8 | signed outer radius divided by 256, plus `64/N` |
| +10 | low 14 bits are subtracted from 128; divide result by 128 for maximum light level |

Equivalently, for words `w0..w5`:

```text
sourceXY = [i16(w0), i16(w1)] / 256
power = abs(i32(i16(w2))) / 128
inner = (i16(w3) < i16(w4) ? i16(w3) : 0) / 256
outer = i16(w4) / 256 + 64/N
maximum = (128 - (w5 & 0x3fff)) / 128
```

The CPU uniform construction is at `0x140044480`, specifically source position `0x140044533`/`0x140044652`, outer-radius expansion `0x140044537`/`0x140044671`, and scalar conversions `0x140044716`–`0x1400447c3`. The shader uniform carries no source Z/height.

In `lightmap_light_N1_L1024_frag` and its N0 counterpart, a negative converted inner radius enables two-sided directional lighting and is then replaced with zero. It is not a negative falloff radius. With `d=length(sourceXY-worldXY)`:

```text
twoSided = inner < 0
inner = max(inner, 0)
falloff = 1 - min(max(d-inner, 0) / (outer-inner), 1)
L = (1-shadowMask) * min(power*falloff, maximum)
```

This is a flat inner region followed by linear radial falloff. It is not inverse-square attenuation. The source maximum is applied before accumulation, not to the finished field. There is no shader clamp of accumulated lighting to one. Malformed radii, negative maximum levels, and normalization exactly at a source center need a bounded client policy; no claim is made about useful original behavior for malformed records.

For example, MAP01 source index 3 has raw words `(7552,13952,128,193,385,0)`. At quality 2 it is at `(29.5,54.5)`, with power 1, maximum 1, inner radius 0.75390625, and outer radius 1.56640625.

## Directional accumulation and sampling

N0 writes `L` into the first channel. N1 uses `q=normalize(sourceXY-worldXY)` and writes:

```text
ordinary: S = [max(q.x*L,0), max(q.y*L,0), min(q.x*L,0), min(q.y*L,0)]
two-sided: S = [L,L,-L,-L]
```

Sources accumulate by component-wise addition in the floating-point target. A CPU boolean supplies an additional +1 or −1 multiplier, allowing existing source contributions to be removed; this is separate from the signed source-power word, whose magnitude was already taken.

Evidence: `lightmap_light_N1_L1024_frag` (embedded blob RVA `0x872450`, registration `0x14005725e`); N0 registration `0x140055e25`–`0x140055ebe`; multiplier setup `0x140044787`–`0x1400447a0`. Pipeline mode 2 maps through `0x14004ef3b` to ONE/ONE factors and ADD operation; switch table RVA `0x4f20c`.

The generated field is bilinearly sampled. The world shader uses `OpImageSampleImplicitLod` with `u_sampler_lightmap`; this alone would not prove its filtering mode. The accompanying proof is sampler descriptor VA `0x14074f520`, created at `0x1401884df`–`0x1401884f1`, and bound at `0x14018b0e7`/`0x14018b2b8`. Min/mag filters are linear, mip filtering is point with mip range zero, and address modes are repeat. Vulkan conversion is at `0x14023f75c`–`0x14023f7d6`; the D3D conversion at `0x14027fd32`–`0x14027fd83` agrees. A CPU equivalent uses coordinates `uv*N-0.5` with wrapped neighboring texels.

After sampling the accumulated four-channel field `S`, the N1 wall, floor, and model fragment variants normalize surface normal `n` and reconstruct:

```text
w = max([n.x,n.y,n.x,n.y] * S, [0,0,0,0])
generatedLight = w.x+w.y+w.z+w.w + abs(n.z)*length(S)
```

The vector length is evaluated after accumulation and filtering. Summing independent scalar per-source floor intensities is therefore not generally equivalent. For horizontal floors/ceilings the result is `length(S)`; for vertical walls it is the directional sum. This is an achromatic lighting field, not RGB light color.

## Wall normals and lookup displacement

For authored endpoints `a,b`, the main wall face uses approximately:

```text
n = normalize([a.y-b.y, b.x-a.x])
lightmapUV = worldXY/64 + normalize(n)*1.55/N
```

CPU normal construction `0x14018ebd3`–`0x14018ec81` packs components into signed normalized 16-bit values using factors 32766.5 and −32767.5. The shader renormalizes them. The displacement is explicit in `wall_N1_L1024_vert` (blob RVA `0x8378e0`). Both the generated field and ambient use this displaced coordinate. At N=1024 the world-space offset is 0.096875 tile.

For texture IDs ≥86, the wall builder emits a second face with negated normal (`0x14018ed10`–`0x14018ee46`). Use the rendered face's normal and displacement; flipping a wall normal indiscriminately toward the camera is not equivalent to reproducing authored face visibility.

This face threshold does not define texture transparency. The wall upload loop covers IDs 0..119 (`0x140187af2`–`0x140187d6e`) and passes image-conversion mode 4 for every loaded wall (`0x140187b0d`). In the indexed fallback, mode 4 selects `0x140163a62` through table RVA `0x163afc`: palette index 255 gets alpha zero, every other index gets alpha 255, including index zero. The upload does not use a texture-ID 86 or 87 cutoff. The image loader first attempts an override image; if one is found, it retains that image's RGBA data instead of using the indexed fallback (`0x1401638c5`–`0x1401638eb`). Thus texture 86 is double-sided, but whether it contains holes depends on its pixels, and authored override alpha can differ from binary indexed transparency.

`floor_vert` (blob RVA `0x8e0620`) uses plain world XY divided by 64. `model_N1_L1024_vert` (blob RVA `0x857660`, registration `0x140056c3c`) instead uses `(rotatedLocalPositionXY + perInstanceLightmapShiftXY)/64 + rotatedNormalXY*1.5/N`. It does not normalize the XY normal before this vertex-stage displacement. The fragment subsequently normalizes the full interpolated 3D normal for lighting reconstruction. The CPU provenance of the per-instance model lightmap shift, including special weapon behavior, remains untraced; it must not automatically be equated with the wall lookup or camera-relative model translation.

## Static wall shadow mask

The source mask is a separate `R8_UNORM` target at the selected lightmap size (`0x1400438f8`–`0x140043957`). Eligible wall segments are selected by `0x140045340`:

- The source power word is nonzero.
- Candidate map cells are inside the source's raw outer-radius axis-aligned range, expanded by two cells, clamped to 0..63. The signed raw divisions truncate toward zero.
- Texture ID is 1..119 inclusive (`0x14004550c`–`0x140045516`).
- The per-cell signed vertical offset at map object `+0x6d4` is ≤80 (`0x140045536`). This is not a texture or light-type index.
- The first byte of the corresponding 64-byte texture filename is nonzero (`0x14004554c`). The filename loader is `0x140093de6`–`0x140093e9c`.

The vertical interpretation is independently established by wall geometry: the same per-cell value supplies bottom Z, clamped to zero, and top Z is bottom+512 (`0x14018ea80`–`0x14018eb60`); the vertex shader divides position by 256. Raise/lower processing adds displacement to this field at `0x14014cd07`. A wall raised more than 80/256=0.3125 tile is omitted from this mask. Negative values are sentinels for other wall state in the loader; their full state classification is not needed to establish that they pass the ≤80 comparison. Non-wall cells also use this storage differently, so the texture-ID test must precede interpreting it as wall height.

### Script elevation units

The script `up` argument uses 64 units per tile, whereas the original wall-height field uses 256 units per tile. The conversion is **four wall-height units per script unit**, not one. Consequently, OpenChasm's `Session.wall_elevations`, which is in script units, should use a shadow threshold of **20 script units**, equivalent to 80 original wall-height units or 0.3125 tile.

The complete chain is observable:

1. `up` is opcode 7 (`0x1400946b6`–`0x1400946c9`). The numeric argument parser reads signed 16-bit values without scaling at `0x140095f05`–`0x140095f16`.
2. Opcode 7 selects parser branch `0x140095fb3` through table RVA `0x96370`. Its third numeric argument is copied to action offset `+0x18` at `0x140095fcd`–`0x140095fd4` (the action base is process offset `+0x38`).
3. Runtime dispatch selects the Up branch at `0x14014c629`–`0x14014c62c`. It reads this target at `0x14014cb82`, multiplies by three and then shifts left two bits at `0x14014cb8b`–`0x14014cba4`, giving an internal target of twelve times the script argument.
4. Runtime motion keeps thirds of a wall-height unit. It divides old and new internal positions by three, truncating toward zero, at `0x14014cc33`–`0x14014cc54`, and adds their difference to the per-cell wall height at `0x14014cd07`.

At the target, the net wall displacement is therefore `12*argument/3=4*argument`, and the rendered tile displacement is `argument/64`. The original also quantizes intermediate wall displacement to integer 1/256-tile units; OpenChasm's continuous script interpolation can differ slightly near the shadow cutoff until motion quantization is matched.

The shadow vertex shader decodes raw endpoints to clip coordinates `(rawX/8192-1, 1-rawY/8192)`. For each endpoint `p`, it computes `dir=normalize(p-lightXYClip)`. The near endpoint is `p-dir/N`; the far endpoint is `p+dir`. This extrudes 32 world tiles away from the source, starting 32/N tile toward the source. CPU setup is `0x140044930`, notably `0x140044b70`–`0x140044bc7`; shader is `lightmap_shadow_vert`.

The two triangles use near endpoint pair `v0,v1` and far pair `v2,v3`: `(v0,v1,v2)` and `(v1,v3,v2)`. UVs have a width correction: let `nearWidth=length(v1-v0)` and `farWidth=length(v3-v2)`. Emit `(u*width,width,textureID)` at each vertex, interpolate, then recover `u=first/second` in the fragment. The segment's authored first endpoint has U=`sizeByte/128`, and the second has U=0.

The fragment samples wall alpha at `(u,0.7)` and `(u,0.3)` in the selected texture-array layer, and writes their minimum. Overlapping masks use MAX blending, established by pipeline mode 3 and its mapping at `0x14004ef5b`–`0x14004ef80`. The light shader reads mask texels directly at integer `worldXY*N/64`, and multiplies lighting by `1-mask`.

This pass uses 2D wall segments and two alpha strips; it does not trace a 3D ray to a source height. There are no model triangles in this particular static wall-mask builder. That observation does not establish that the original has no other model-shadow pass. Exact shadow texture filtering, triangle-edge coverage, and all moving-geometry update paths remain to be matched before claiming pixel equivalence. A CPU ray/segment occlusion approximation should be labeled as such.

## Camera distance response

The N0 and N1 world fragment shaders apply:

```text
F = (toneMappingEnabled ? 10 : 5) * distanceFadeSetting
D = clamp(F/(length(worldPosition-cameraPosition)+F), F/50, 1)^2
linearSceneRGB = linearMaterialRGB * (ambient + generatedLight) * D
```

The scalar uses full 3D distance, not projected forward depth. Source radial falloff and camera distance fade are different operations. Material alpha is retained independently. The wall fragment additionally rejects texture alpha <0.5 and world Z>2; these are geometry/material rules, not lighting attenuation.

CPU `0x14018b8f4`–`0x14018b947` selects 5 or 10 from Tone Mapping and multiplies by developer setting index 4, “Distance Fade”. Its default string is `1`, confirmed at `0x140155eff`–`0x140155f45`, with declared range 0.05..2. With the observed neutral configuration and tone mapping enabled, `F=10` and the minimum multiplier is 0.04.

Tone mapping follows this linear lighting stage; see [the rendering pipeline](remaster-rendering.md). Do not saturate lighting or material-light multiplication at one before HDR post-processing.

## Suggested client implementation boundary

This is a proposed design, not an observed original API:

```rust
RemasterLightmap::build(map: &Map, world: &World, textures: &WallTextures,
                       quality: ShadingQuality) -> Result<RemasterLightmap, LightingError>
RemasterLightmap::sample(position_xy: [f32; 2], normal: [f32; 3],
                        wall_face: bool) -> f32
camera_distance_multiplier(world_xyz: [f32; 3], camera_xyz: [f32; 3],
                           tone_mapping: bool) -> f32
```

The client owns the decoded ambient table, generated field, reusable shadow scratch buffer, and wall/source dirty-state tracking. Build or update source contributions over their bounded texel rectangles; do not iterate all lights for each rendered pixel. `sample` returns ambient plus reconstructed directional lighting, using the wall-face displacement when requested. Keep camera distance fade outside the precomputed field because it changes with the camera and surface height. Apply the result to decoded linear material RGB before the common HDR post-process.

A lower-level `sample_uv(lightmap_uv, normal)` is useful for models, whose vertex lookup differs from both wall and floor lookup. The caller can interpolate the model vertex lighting coordinates along with the normal.

`World` currently supplies wall segments and authored cells but no vertical wall offset. A height-aware shadow update needs that offset from client/session render state; do not infer it from movement-blocking flags. A method accepting `Session.wall_elevations` must explicitly document script units and convert by four to original wall-height units, or by 1/64 to tiles. Preserve raw light order and signed reinterpretation for script references. Bounds, allocation failure, degenerate segments, malformed radii, and exact-center normalization require explicit finite behavior.

Keep this field exclusive to remaster/enhanced presentation; Legacy can retain its existing light path. Initial implementation may use a CPU f32 field rather than original half-float render-target accumulation, but must record the rounding difference. Exact projectile, muzzle, explosion and other transient-source construction, shadow refresh timing, and CPU model-lighting shift setup are not fully established by this investigation. Applying a convenient dynamic-light formula remains provisional and should be documented independently of the verified static source formulas.
