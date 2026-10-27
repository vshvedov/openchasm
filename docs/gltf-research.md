# glTF object research and importer prototype

The installed 2022 resource archive contains 160 `.gltf` files: 156 under `model/` and four under `monster/`. All inspected mesh primitives are triangles with position, normal and first UV attributes. Twenty-one files have animations, using 29 channels that target morph weights with STEP interpolation. No skeletal skins were found. This is an observation about this archive, not a guarantee for custom content.

The level definition resources contain two object names with no `.3o` suffix: `vent1` and `bridge_2`. `model/vent1.gltf` has 324 vertices, 420 triangle indices, 31 morph targets, and a companion `model/vent1.png` even though the glTF has no material/image declaration. `model/bridge_2.gltf` has 96 vertices and 144 indices and explicitly references `bridge_2.png`. Its name is also identified in the bundled modding manual as an example of an object without a 3O file. glTF model buffers in this installation are base64 data URIs; textures can be companion PNGs or glTF-referenced PNGs.

The bundled modding manual describes object animation as a base geometry frame followed by shape keys in index order, with a nominal 50 ms object frame time. Standard glTF describes morph targets as *deltas* relative to the base. These rules differ from treating an arbitrary glTF animation timeline as game animation. The manual also calls out companion texture naming, mesh-name conventions and a separate transparent mesh. The importer deliberately implements this object-frame interpretation; it is not a general glTF animation player.

## Prototype

`src/gltf_model.rs` is a headless importer used by the `gltf-info` inspection command. The client feature enables these dependencies:

```toml
gltf = { version = "1.4.1", default-features = false, features = ["utils", "names"] }
base64 = "0.22"
png = "0.18"
```

`GltfModel::load(&mut ResourceSet, path)` returns triangles, normalized UVs, indexed vertices, local-to-scene glTF node matrices, morph position deltas, decoded RGBA PNG images, per-primitive texture indices and a transparency name hint. `Primitive::frame_positions(frame)` computes base positions or base plus the selected target delta. Buffers and images come through the existing resource abstraction, including local data URIs; no working-directory filesystem importer is used. The reader bounds JSON/buffer/geometry/image sizes, rejects unsafe relative references, and validates index and accessor bounds. Tests use synthetic binary geometry and PNG fixtures.

An isolated Rust harness with the proposed dependencies passed tests and strict Clippy. Both glTF-only resources loaded; vent frame 1 changed 156 vertices relative to its basis, and both PNGs decoded. Across all 156 model glTF resources, 155 parsed successfully. `model/wp_st2.3o.gltf` is rejected because its scene refers to nonexistent node indices; integration should report this and fall back to the valid original 3O. The importer is integrated for inspection; rendering integration remains in progress.

## Integration boundaries

The importer preserves standard glTF coordinates and column-major node transforms; the correct conversion to Chasm's tile-space axes and orientation needs verification by rendering and comparing against the installed game. It exposes RGBA images rather than palette indices; the renderer needs a texture representation that supports both. A static primitive with no morph targets should remain on its base pose when another primitive in the same model advances. The animation clock should belong to each instance, support `playani`/`stopani`, and select frames using the original game rules.

The prototype does not implement skinning, animated node transforms, general glTF sampler interpolation, animation channels, material factors, alpha blending, texture sampler modes, normals/lighting, JPEG, percent-encoded URIs, arbitrary external URLs, or 3O texture/geometry replacement priority. It currently rejects skins and non-triangle geometry; some unsupported material and animation properties are retained only indirectly or ignored. File names without a `.gltf` suffix, traversal, nonlocal URI schemes, oversized data, and invalid triangle references are rejected. Custom content may need a more complete importer or explicit compatibility errors before release.
