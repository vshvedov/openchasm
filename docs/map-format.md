# Original map format observations

This records a subset of the DOS-era map layout used by the installed Steam release, cross-checked against [PanzerChasm's map-format notes](https://github.com/Panzerschrek/Chasm-Reverse/blob/master/docs/format%20kart.txt), [floor-format notes](https://github.com/Panzerschrek/Chasm-Reverse/blob/master/docs/floors.xx%20format.txt), and inspection of the locally installed data. The reader preserves raw fields where their interpretation is incomplete. The 2022 demo and other game versions may differ.

Integers in known structures are little-endian. Map coordinates use 256 units per map tile. Fixed grids are 64×64, stored x-major (index `x * 64 + y`).

| Offset | Contents |
| --- | --- |
| `0x00001` | 256×256 light data |
| `0x10001` | Additional wall light data |
| `0x18001` | 4096 entries of 11 bytes: type/texture ID, size, flags, x0, y0, x1, y1 (the four coordinates are u16) |
| `0x23001` | 4096 ceiling texture IDs |
| `0x24001` | 4096 floor texture IDs |
| `0x25001` | 4096 ambient-light values |
| `0x26001` | 4096 ambient-sound values |
| `0x27001` | u16 light count, then records of six u16 values: x, y, power, inner radius, outer radius, brightness |
| following lights | u16 actor count, then records of x, y, ID (u16), angle and flags (u8) |

Wall grid records describe more than walls: IDs 131–162 represent items and 163–226 represent models according to the existing reconstruction. For visible walls the ID is 1–127 and the size byte is 64 or 128; zero and other records are not treated as wall geometry. These IDs and properties need validation against scripts and texture metadata before treating every segment as solid. A wall's coordinates are independently positioned, not necessarily aligned with its owning cell.

Actor ID 100 identifies player spawn points. For those entries, the flags byte corresponds to the spawn number, with 0 used for the normal starting point in all 20 maps inspected. For other actor IDs it encodes difficulty flags. The low three angle bits give eighth-turn increments; PanzerChasm adds half a turn to obtain its world-space orientation. Gameplay behavior and coordinate-axis conventions need comparison with the original game.

Bytes after the actor array vary by level and have not been decoded. No assumption is made that the parsed structures describe the entire file; those bytes are retained.

`floors.XX` holds 64 indexed textures, not level geometry: a 64-byte header followed by 64 records of 5,504 bytes. Each record begins with 4,096 palette indices for a 64×64 texture, followed by mip levels (32×32, 16×16, 8×8) and 64 bytes of unknown purpose. The first 64×64 index image is exposed by the parser; palette interpretation belongs to the image loader.

The Rust reader was checked against all 20 map and floor files in the installed full-release archive; each parsed successfully, had the expected floor size, and included a spawn numbered zero. Synthetic tests check coordinate order, little-endian values, record counts, truncation, and floor-texture stride.

## Plane preview observations

`chasm2.pal` contains the 256 RGB triples used for `floors.XX` palette indices. Sampling the local release's map grids showed floor and ceiling IDs in `0..=63`. Across levels 01–19, ceiling ID 62 is common while floor ID 62 appears only eight times (in level 10); ID 63 dominates unused map space, and texture slot 63 is a uniform color in the inspected maps. The independent reference renderer treats both IDs 62 and 63 as absent geometry for both floors and ceilings; the preview now follows it and draws the selected sky behind those cells. The texture axes, exact vertical scale, brightness, light maps, animation, and sky behavior need visual comparison against the original game.

The first 65,536 bytes after the header contain a 256×256 authored light grid; the following 32,768 bytes carry wall lighting. `Map::static_light()` retains these raw regions and offers bounded sampling. Its four samples per tile, one-sample horizontal shift, eight wall samples per cell with a two-byte offset, `(255 - 6 * value)` conversion, and 1.3 overbright factor follow the independent PanzerChasm reconstruction. That reconstruction marks the shift, wall offset, and conversion as uncertain; values beyond its normal range clamp safely. The renderer applies this provisional static light to floors, ceilings and authored wall cells and adds its existing dynamic-light approximation. Exact remaster tone mapping and color pipeline are not yet reconstructed.
