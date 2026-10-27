# Object model and animation formats

These observations apply to the `.3o` and `.ani` object resources in the installed 2022 Steam archive. They were checked against the installed corpus and published work including [PanzerChasm's format notes](https://github.com/Panzerschrek/Chasm-Reverse/blob/master/docs/format%20modelej.txt), [model loader](https://github.com/Panzerschrek/Chasm-Reverse/blob/master/src/model.cpp), and [map loader](https://github.com/Panzerschrek/Chasm-Reverse/blob/master/src/map_loader.cpp). The loader in this project is an independent implementation. CAR monster models and older demo variants are not covered.

## 3O files

All integers are little-endian. The format has no magic number and reserves fixed-size arrays, so only entries up to the stored counts are meaningful. Each file inspected has exactly the length shown below.

| Offset | Description |
| --- | --- |
| `0x0000` | 400 reserved polygon entries, 32 bytes each |
| `0x3200` | Reserved vertex data; each vertex is three signed 16-bit coordinates |
| `0x4800` | Unsigned 16-bit vertex count |
| `0x4802` | Unsigned 16-bit polygon count |
| `0x4804` | Unsigned 16-bit skin height |
| `0x4806` | `64 * height` indexed skin pixels |

A polygon contains four vertex indices (u16), four UV coordinate pairs (u16), four bytes whose role is not established, a group byte, a flags byte, and a signed 16-bit vertical texture offset. In every inspected triangle the fourth index is 255; otherwise the fourth index names a vertex. The loader validates all used vertex indices. The four uncertain bytes are retained as two raw u16 values, `next` and `distant`, without assuming semantics. Skin pixels use an external palette and may be empty. Polygon flags include possible two-sided and transparency modes; those and UV offset behavior require visual validation.

For rendering, PanzerChasm uses object vertex coordinates scaled by 1/2048 map tiles, while map-object placement uses 1/256 tile per map coordinate unit; this is an implementation reference rather than a verified fidelity claim. In map wall-cell records, types 163 through 226 are static model IDs and 131 through 162 are item IDs. Their first coordinate pair supplies placement, and the low three bits of the flags field supply rotation. Actual model names and animations come from per-level resource definitions. A general renderer will also need depth ordering, UV seams, animation, alpha modes, z offsets, and the possibility of independently replaced models/skins.

## ANI files

An ANI begins with a u16 vertex count followed by frames. Each frame contains that many triples of signed little-endian i16 coordinates, occupying six bytes per vertex. The frame count is derived from the length. The file does not by itself define playback speed or which 3O model to animate; those must be resolved from the resource definitions and game behavior. The reader checks alignment, count limits and the supplied model's vertex count when requested.

## Validation and limits

The parser passed synthetic tests for signed coordinates, triangle indices, UV/flag preservation, bounds, skin length, and animation frames. An independent batch check successfully parsed all **1,003 3O files and 155 ANI files** in the locally installed 2022 archive. A naive same-basename match is unsafe: `gateg1.ani` and `b3.ani` each differ in vertex count from the same-named 3O. Match using level/object metadata, then check counts. All 9,998 triangle markers in the corpus occurred in the fourth index slot, and observed model vertex counts did not exceed 253. Behavior for models with 255 or more vertices and fourth index 255 remains uncertain. The ANI reader caps frames at 4,096 as an explicit implementation limit.
