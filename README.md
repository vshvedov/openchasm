# OpenChasm

An experimental Rust reimplementation of **Chasm: The Rift**, with Linux, Windows and macOS as target platforms. The current build is an **early gameplay preview**; the full campaign is not yet playable. It loads the original map wall segments, CEL textures, floor and ceiling textures, 3O objects and animated monster models, starts at the player spawn, and allows basic movement. Default walking/running, acceleration, mouse input and jump forces follow verified remaster calculations; collision geometry and stepping remain incomplete. A limited subset of map processes can move walls, supported items can be collected, and enemies can chase and attack. Enemy behavior and most weapon mechanics are provisional; some audio, grenade and mine behavior, many level scripts and multiplayer remain incomplete.

The name OpenChasm is already used by another independent project; this repository is a separate Rust implementation. See [research notes](docs/research.md) and [map format notes](docs/map-format.md) for references.

## Try the preview

Install a stable Rust toolchain and CMake, then run from this directory. Linux builds also need ALSA development headers:

```sh
cargo run --release -- sources
cargo run --release -- launcher
cargo run --release -- --source full view 01
```

The launcher lets you choose a discovered full, demo or local installation, or enter a path to an archive or loose-resource directory, then select a level and difficulty. Use up/down to select a row, left/right to change it, Enter to confirm, and Escape to quit. For a custom path, type or paste it in the path row and press Enter to load its levels. The launcher returns after you leave a level. Its labels describe locations, not demo entitlements.

The game captures the mouse when its window is focused; move it to look and aim, or use **left/right arrows** to turn. If capture is unavailable, click the window to retry. Use **WASD** to move; autorun defaults on, and **Shift** temporarily switches between walking and running. Use **E** to use nearby linked objects, **Space** or the right mouse button to jump, the left mouse button to fire, **1–8** to select owned weapons or the **mouse wheel** to cycle through owned weapons with ammunition, **Tab** to toggle the provisional explored-wall map, **R** to request a restart, **F5** to save an in-memory checkpoint, **F9** to restore it, and **Escape** to open the pause menu. Use the arrow keys and Enter in the menu; restart and exit require confirmation. The map reveals opaque walls seen by the player; its visibility and wall-occlusion rules are approximate. The checkpoint lasts only for the current level and is lost on exit. After a supported level exit, press **Enter** to continue. Change `01` to another two-digit map ID shown by `cargo run --release -- levels`. Select `demo`, `full`, or `local` with `--source`, or supply a path to a compatible archive or directory of loose resources. Select `easy`, `normal`, or `hard` with `--difficulty` before `view`; the default is `normal`. Without an option the program chooses the copied local archive first, then the full Steam installation, then the demo. `--archive PATH` remains an alias for `--source PATH`.

The FPS counter is shown during gameplay. Press **backtick/tilde** to open the
developer console; `help` lists commands, `status` shows frame timing and player
position, `maxfps 0` removes the frame cap, and `maxfps 60` restores it. The console
pauses gameplay and releases the mouse. See [console controls](docs/developer-console.md).

Open **Escape → Options** to change the visual preset, tone mapping,
colour controls, bloom and frame cap. `remaster` is the default; `enhanced` adds
optional bloom. The tilde console also accepts `graphics enhanced`,
`graphics remaster` and `options`. Press **Tab** in Options to switch to Controls, where you can adjust mouse sensitivity, autorun and Original/Quake 1 movement. Quake mode uses Quake ground friction, acceleration, air movement and jumping on Chasm collision geometry; it does not implement Quake BSP slopes, water or stair traversal. Changes are saved per user. The graphical status bar uses your game content;
`showhelp on` restores the development control overlay.

Examples:

```sh
cargo run --release -- --source demo --difficulty easy view 01
cargo run --release -- --difficulty hard --source full view 01
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

Next work includes validating floor and ceiling rendering, materials and transparency, original collision and movement, 3D objects, object animation and collision, the remaining map scripts, enemies and combat, and audio. Cross-platform CI is configured for synthetic tests; game-data checks require a locally installed copy. Cross-platform behavior has not yet been tested on Windows or macOS.

An experimental GPU post-processing backend is available with
`cargo build --release --locked --features gpu`. It moves colour correction, bloom
and UI composition to Vulkan/Metal/DX12; world rasterization remains on the CPU.
Use `OPENCHASM_RENDERER=software` to choose SDL, or `renderer software` /
`renderer gpu` in the console to switch during play. Default builds retain SDL.
