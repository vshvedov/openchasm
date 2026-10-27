# CEL textures and palettes

This describes the uncompressed CEL images observed in the 2022 Steam release's `csm.bin`. It does not claim support for every Autodesk CEL variant, the DOS archive, or custom replacements.

All 691 files under `texture/` ending in `.cel` in the inspected full-game archive have this layout:

| Offset | Size | Meaning |
| --- | --- | --- |
| 0 | 2 | Bytes `19 91` |
| 2 | 2 | Little-endian width |
| 4 | 2 | Little-endian height |
| 6 | 4 | Other image metadata, not interpreted |
| 10 | 2 | Little-endian depth, 8 in the inspected files |
| 12 | 4 | Little-endian pixel byte count |
| 16 | 16 | Other header bytes, not interpreted |
| 32 | 768 | 256 RGB triples, each component 0–63 |
| 800 | width × height | Palette indices in row-major order |

All inspected images have a declared pixel byte count equal to width × height, and their file size equals that count plus 800 bytes. Most wall images are 64 × 128 or 128 × 128; there are also sky and other image dimensions. A decoded example (`001a.cel`) was visually inspected to verify the pixel order. The standalone `chasm2.pal` contains 768 bytes with the same RGB6 layout. The 256 × 1 `palette.png` in the release confirms conversion to RGB8 by multiplying each component by four; it is not normalized to 255.

CEL palettes can differ slightly from the standalone palette, including at index 255. Transparency is supplied by the caller rather than assumed for every image. The viewer treats palette index 255 as transparent for wall IDs 87–119, and draws invisible ID 120 only for collision. `CelImage` preserves embedded palette entries, and supports optional explicit transparency when converting to RGBA.

The archive also includes PNG assets; the game's bundled modding manual documents `.cel.png` overrides. These are separate from CEL decoding and are not currently resolved by `CelImage`.

Validation: unit tests cover color conversion, transparency, bad headers and truncated files; an independent batch check decoded every CEL in the inspected full archive. The demo and other game distributions may contain additional variants.
