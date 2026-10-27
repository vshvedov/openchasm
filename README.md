# OpenChasm

An experimental Rust reimplementation of **Chasm: The Rift**, with Linux, Windows and macOS as target platforms. The current build is a **geometry preview**, not a playable game. It loads the original map wall segments, CEL textures, and floor and ceiling textures, starts at the player spawn, and allows basic movement. Movement and collision are provisional. A limited subset of map processes can move walls; enemies, weapons, audio and multiplayer have not been implemented.

The name OpenChasm is already used by another independent project; this repository is a separate Rust implementation. See [research notes](docs/research.md) and [map format notes](docs/map-format.md) for references.

## Try the preview

Install a stable Rust toolchain, then run from this directory:

```sh
cargo run --release -- sources
cargo run --release -- --source full view 01
```

Use **WASD** to move, **left/right arrows** to turn, **E** to use nearby linked objects, and **Escape** to quit. Change `01` to another two-digit map ID shown by `cargo run --release -- levels`. Select `demo`, `full`, or `local` with `--source`, or supply a path to a compatible archive or directory of loose resources. Without an option the program chooses the copied local archive first, then the full Steam installation, then the demo. `--archive PATH` remains an alias for `--source PATH`.

Examples:

```sh
cargo run --release -- --source demo view 01
cargo run --release -- --source /path/to/csm.bin levels
cargo run --release -- --source /path/to/loose-data view 01
cargo run --release -- --source full check
cargo test --locked
```

`check` validates map and floor structures and starting positions, and reports the number of initially unlocked moving processes supported in each level. `list [PREFIX]` browses resource names, and `cat RESOURCE` writes one resource to standard output.

## Game data

A legitimately obtained copy of the game is required. The engine code is MIT licensed; game assets and the manual remain the property of their respective owners and are not covered by that license. Do not commit or redistribute them with this repository. Files in `assets/` are ignored by Git.

The 2022 Steam release archive is an uncompressed tar file; the earlier DOS installation's differently formatted `CSM.BIN` is not yet supported. On the initial machine, the Steam demo and full game archives were byte-for-byte identical; this does not establish demo entitlements or reproduce any restrictions imposed by the original demo executable. A source label describes where data was found, not what a user is entitled to access.

## Architecture and direction

Resource containers, map/image decoders, the movement/geometry prototype, and the window/renderer live in separate modules. The movement update is driven by inputs on a fixed tick and does not depend on window APIs. This is a starting separation for a future authoritative server; there is **no networking protocol or multiplayer implementation yet**, and deterministic cross-platform simulation has not been established. Loose-directory loading gives future original content a development path, though it currently expects the same resource layout and formats.

Next work includes validating floor and ceiling rendering, materials and transparency, original collision and movement, 3D objects, the remaining map scripts, enemies and combat, and audio. Cross-platform CI is configured for synthetic tests; game-data checks require a locally installed copy. Cross-platform behavior has not yet been tested on Windows or macOS.
