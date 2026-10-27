# Initial resource research

The Steam installation at `steamapps/common/Chasm The Rift` contains a 2022 Windows executable, a `csm.bin` archive, a modding manual, and a separate `Chasm The Rift Original` DOS installation. The archive is an uncompressed GNU tar, 373,039,104 bytes in this installation. Its SHA-256 is `a8d0a4c292861e82627bab160af2b4fc7da58a0a3bf3cd030ece6a3979b4b261`.

The archive contains 2,970 regular files and 22 directories. Notable paths include `chasm.inf`, `chasm2.pal`, `palette.png`, `model/`, `monster/`, `texture/`, `sound/`, `sound_new/`, `music/`, and `map/`. The archive contains `map/resource.01` through `.19`, plus `.99`; the first is named Power Station and `.99` is Brif_Room. Corresponding map and floor files are present. The modding manual says map files contain walls, monsters, objects, and lights; floor files contain floor/ceiling maps; process files describe map logic. These descriptions are leads, not yet decoded formats.

`chasm.inf` contains textual monster, object, weapon, and other definitions. For example, its `MONSTERS` section lists 24 entries and columns labeled radius, speed, life, attack values, and other fields. Their units and behavior must be verified rather than inferred solely from labels.

The 2022 archive also includes modern replacement models and textures. The DOS installation has another `CSM.BIN` and loose add-on/editor data; the initial reader does not support that separate archive. No executable code has been copied or incorporated.

## Reader boundaries

The current indexer accepts ordinary files and directories with octal sizes in the observed tar archive, validates checksums, rejects duplicates and unsupported types, and reads by exact archive path. It does not implement GNU long-name, PAX, links, compression, or asset decoding. Parsing is independent of archive extraction, so archive paths are never written to disk.

## Verification on the initial machine

The copied archive matches the Steam file's SHA-256. Python's standard `tarfile` independently enumerated all entries and 20 level resource files. A separate scan checked all 2,992 tar header checksums and padded sizes, and confirmed that their GNU prefix fields are empty. After Rust was installed, `cargo test` passed all three tests and `cargo run -- levels` listed all 20 level resource files.
