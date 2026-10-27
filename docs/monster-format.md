# CAR actor model format

These observations apply to the 24 `.car` files in `monster/` in the installed 2022 Steam archive. They were checked against [Chasm-Reverse's CAR format notes](https://github.com/Panzerschrek/Chasm-Reverse/blob/master/docs/format%20.car%20modelej.txt) and the local resource corpus. The Rust reader is independently implemented. Other editions or custom resources may vary.

All known integer fields are little-endian. A CAR begins with a 102-byte header followed by a 3O-like body model, its animation segments, zero to three detached-part model blocks with their animation segments, then eight sound segments.

| Offset | Description |
| --- | --- |
| `0x00` | 20 u16 byte lengths for body animation slots |
| `0x28` | Three pairs of u16 byte lengths for detached-part animation slots |
| `0x34` | 12 bytes not interpreted; all zero in the inspected files |
| `0x40` | Three u16 fall-sound identifiers |
| `0x46` | Eight u16 sound byte lengths |
| `0x56` | Eight u16 sound volume values |
| `0x66` | Body model fixed polygon and vertex arrays and counts |
| `0x486a` | Body skin length in **bytes**, unlike 3O which stores the height |
| `0x486c` | Indexed body skin bytes, width 64 |

The model's other data fields match the 3O layout documented in [model-format.md](model-format.md). CAR skin byte length is divisible by 64 in the inspected corpus. The 20 body animation segments immediately follow the skin in header order. Each consists of consecutive frames of `vertex_count` signed i16 coordinate triples; each frame therefore occupies `vertex_count × 6` bytes. A length of zero means no animation in that slot. Frame semantics and timing have not been established by this parser.

Each detached part with either of its two declared animation lengths nonzero has a fixed `0x4806`-byte 3O-like model header, followed by its two animation segments in order. No inline part skin was present in the inspected data; the stored word at part offset `0x4804` is retained as a skin height hint, and was equal to the body skin height across this corpus. The rendering interpretation and alignment of detached parts, along with the meaning of each slot, need further investigation. A part whose two lengths are zero has no model block in the inspected files.

The eight declared sound segments follow the part blocks. They are retained as raw bytes; sound encoding, slot meanings, and playback timing are not assumed. The parser preserves the complete initial header and any trailing bytes, so currently unknown header fields and extensions are not silently discarded. It checks segment boundaries, geometry counts, and complete animation frames before accepting the file.

The standalone Rust reader successfully parsed all 24 installed CAR files, including all declared animations, part blocks, and sound segments; none had trailing bytes. Synthetic tests cover body skin conversion, animation decoding, part blocks, raw sounds, truncated data and incomplete frames. Animations, skins, detached-part behavior, and sounds still need visual and audible comparison against the game.
