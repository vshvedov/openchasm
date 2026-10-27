//! Headless glTF 2.0 object import prototype for the installed Chasm assets.
//!
//! Geometry remains in glTF coordinates; callers must apply the supplied node
//! transforms and a validated glTF-to-game coordinate conversion. This reader
//! supports triangle primitives and the game's basis-plus-morph frame scheme,
//! not skeletal animation or general glTF transform animation.
use crate::content::ResourceSet;
use base64::Engine;
use std::collections::HashSet;
use std::io::{self, Cursor, ErrorKind};
use std::path::{Component, Path};

const MAX_BUFFER_BYTES: usize = 64 * 1024 * 1024;
const MAX_PIXELS: usize = 4096 * 4096;
const MAX_VEC3_VALUES: usize = 4_000_000;
const MAX_INDICES: usize = 12_000_000;
const MAX_NODES: usize = 4096;

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(ErrorKind::InvalidData, message.into())
}

#[derive(Debug, Clone, PartialEq)]
pub struct Image {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Primitive {
    pub name: String,
    pub positions: Vec<[f32; 3]>,
    /// Authored unit normals, absent when the asset requests flat shading.
    pub normals: Option<Vec<[f32; 3]>>,
    pub uv: Vec<[f32; 2]>,
    pub indices: Vec<u32>,
    /// glTF position *deltas*, ordered as the game's successive frames.
    pub morph_deltas: Vec<Vec<[f32; 3]>>,
    /// Optional normal deltas, in the same frame order as position deltas.
    pub morph_normal_deltas: Vec<Option<Vec<[f32; 3]>>>,
    /// Column-major accumulated glTF scene node transform.
    pub node_transform: [[f32; 4]; 4],
    pub texture: Option<usize>,
    pub transparent: bool,
}
impl Primitive {
    /// glTF morph normals are deltas; normalize only after adding and transforming.
    pub fn frame_normal(&self, frame: usize, index: usize) -> Option<[f32; 3]> {
        let base = *self.normals.as_ref()?.get(index)?;
        let delta = frame
            .checked_sub(1)
            .and_then(|frame| self.morph_normal_deltas.get(frame))
            .and_then(Option::as_ref)
            .and_then(|values| values.get(index))
            .copied()
            .unwrap_or([0.0; 3]);
        Some(std::array::from_fn(|axis| base[axis] + delta[axis]))
    }

    /// Frame zero is the base mesh; frame n > 0 applies morph target n - 1.
    pub fn frame_positions(&self, frame: usize) -> Option<Vec<[f32; 3]>> {
        if frame == 0 {
            return Some(self.positions.clone());
        }
        let delta = self.morph_deltas.get(frame - 1)?;
        Some(
            self.positions
                .iter()
                .zip(delta)
                .map(|(position, delta)| {
                    [
                        position[0] + delta[0],
                        position[1] + delta[1],
                        position[2] + delta[2],
                    ]
                })
                .collect(),
        )
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct GltfModel {
    pub primitives: Vec<Primitive>,
    pub images: Vec<Image>,
    /// Basis plus the most morph frames found in any primitive.
    pub frame_count: usize,
}

fn relative_resource(model_path: &str, uri: &str) -> io::Result<String> {
    if uri.is_empty() || uri.contains(['\\', ':', '?', '#', '%']) || uri.starts_with('/') {
        return Err(invalid("unsupported or unsafe glTF resource URI"));
    }
    let path = Path::new(uri);
    if path
        .components()
        .any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err(invalid("glTF resource escapes its containing directory"));
    }
    let directory = model_path
        .rsplit_once('/')
        .map(|(dir, _)| dir)
        .unwrap_or("");
    Ok(if directory.is_empty() {
        uri.to_owned()
    } else {
        format!("{directory}/{uri}")
    })
}
fn resource_bytes(resources: &mut ResourceSet, model_path: &str, uri: &str) -> io::Result<Vec<u8>> {
    if let Some(data) = uri.strip_prefix("data:") {
        let (metadata, encoded) = data
            .split_once(',')
            .ok_or_else(|| invalid("malformed glTF data URI"))?;
        if !metadata.ends_with(";base64") || encoded.len() > MAX_BUFFER_BYTES * 4 / 3 + 8 {
            return Err(invalid("unsupported or excessive glTF data URI"));
        }
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|_| invalid("invalid glTF base64 data"))?;
        if bytes.len() > MAX_BUFFER_BYTES {
            return Err(invalid("glTF buffer exceeds size limit"));
        }
        Ok(bytes)
    } else {
        resources.read(&relative_resource(model_path, uri)?)
    }
}
fn decode_png(data: &[u8]) -> io::Result<Image> {
    let mut decoder = png::Decoder::new(Cursor::new(data));
    decoder.set_limits(png::Limits {
        bytes: MAX_BUFFER_BYTES,
    });
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder
        .read_info()
        .map_err(|e| invalid(format!("invalid PNG header: {e}")))?;
    let width = reader.info().width;
    let height = reader.info().height;
    let pixels = (width as usize)
        .checked_mul(height as usize)
        .filter(|&n| n > 0 && n <= MAX_PIXELS)
        .ok_or_else(|| invalid("invalid PNG dimensions"))?;
    let size = reader
        .output_buffer_size()
        .filter(|&n| n <= MAX_BUFFER_BYTES)
        .ok_or_else(|| invalid("PNG output exceeds size limit"))?;
    let mut raw = vec![0; size];
    let info = reader
        .next_frame(&mut raw)
        .map_err(|e| invalid(format!("invalid PNG pixels: {e}")))?;
    let raw = &raw[..info.buffer_size()];
    let channels = match info.color_type {
        png::ColorType::Rgba => 4,
        png::ColorType::Rgb => 3,
        png::ColorType::Grayscale => 1,
        png::ColorType::GrayscaleAlpha => 2,
        png::ColorType::Indexed => return Err(invalid("PNG palette expansion failed")),
    };
    if raw.len() != pixels * channels {
        return Err(invalid("unexpected PNG channel data length"));
    }
    let mut rgba = Vec::with_capacity(pixels * 4);
    match info.color_type {
        png::ColorType::Rgba => rgba.extend_from_slice(raw),
        png::ColorType::Rgb => {
            for p in raw.chunks(3) {
                rgba.extend_from_slice(&[p[0], p[1], p[2], 255]);
            }
        }
        png::ColorType::Grayscale => {
            for &p in raw {
                rgba.extend_from_slice(&[p, p, p, 255]);
            }
        }
        png::ColorType::GrayscaleAlpha => {
            for p in raw.chunks(2) {
                rgba.extend_from_slice(&[p[0], p[0], p[0], p[1]]);
            }
        }
        png::ColorType::Indexed => return Err(invalid("PNG palette expansion failed")),
    }
    if rgba.len() != pixels * 4 {
        return Err(invalid("unexpected PNG output size"));
    }
    Ok(Image {
        width,
        height,
        rgba,
    })
}
fn identity() -> [[f32; 4]; 4] {
    [
        [1.0, 0.0, 0.0, 0.0],
        [0.0, 1.0, 0.0, 0.0],
        [0.0, 0.0, 1.0, 0.0],
        [0.0, 0.0, 0.0, 1.0],
    ]
}
fn multiply(a: [[f32; 4]; 4], b: [[f32; 4]; 4]) -> [[f32; 4]; 4] {
    let mut result = [[0.0; 4]; 4];
    for column in 0..4 {
        for row in 0..4 {
            result[column][row] = (0..4).map(|i| a[i][row] * b[column][i]).sum();
        }
    }
    result
}

impl GltfModel {
    /// Read a glTF asset and its local resources from the game's resource set.
    /// This deliberately does not use gltf's filesystem-based import helper.
    pub fn load(resources: &mut ResourceSet, path: &str) -> io::Result<Self> {
        if !path.ends_with(".gltf") {
            return Err(invalid("expected a .gltf resource path"));
        }
        let data = resources.read(path)?;
        if data.len() > MAX_BUFFER_BYTES {
            return Err(invalid("glTF JSON exceeds size limit"));
        }
        let document =
            gltf::Gltf::from_slice(&data).map_err(|e| invalid(format!("invalid glTF: {e}")))?;
        if document.skins().next().is_some() {
            return Err(invalid("skinned glTF is not supported by object importer"));
        }
        let mut buffers = Vec::new();
        let mut total = 0usize;
        for buffer in document.buffers() {
            let bytes = match buffer.source() {
                gltf::buffer::Source::Uri(uri) => resource_bytes(resources, path, uri)?,
                gltf::buffer::Source::Bin => document
                    .blob
                    .clone()
                    .ok_or_else(|| invalid("missing binary glTF buffer"))?,
            };
            total = total
                .checked_add(bytes.len())
                .ok_or_else(|| invalid("glTF buffer size overflow"))?;
            if total > MAX_BUFFER_BYTES || bytes.len() < buffer.length() {
                return Err(invalid("glTF buffer is too large or truncated"));
            }
            buffers.push(bytes);
        }
        let companion = format!("{}.png", path.trim_end_matches(".gltf"));
        let mut companion_index = None;
        let mut images = Vec::new();
        for image in document.images() {
            let bytes = match image.source() {
                gltf::image::Source::Uri { uri, .. } => {
                    if !uri.starts_with("data:") && relative_resource(path, uri)? == companion {
                        companion_index = Some(images.len());
                    }
                    resource_bytes(resources, path, uri)?
                }
                gltf::image::Source::View { view, .. } => {
                    let buffer = &buffers[view.buffer().index()];
                    let end = view
                        .offset()
                        .checked_add(view.length())
                        .ok_or_else(|| invalid("image offset overflow"))?;
                    buffer
                        .get(view.offset()..end)
                        .ok_or_else(|| invalid("truncated embedded image"))?
                        .to_vec()
                }
            };
            images.push(decode_png(&bytes)?);
        }
        // The remaster also uses a filename-based replacement even when the
        // glTF has no material/image entry, e.g. vent1.gltf + vent1.png.
        let fallback = if companion_index.is_some() {
            companion_index
        } else if let Ok(bytes) = resources.read(&companion) {
            let index = images.len();
            images.push(decode_png(&bytes)?);
            Some(index)
        } else {
            None
        };
        let scene = document
            .default_scene()
            .or_else(|| document.scenes().next())
            .ok_or_else(|| invalid("glTF has no scene"))?;
        let mut stack: Vec<_> = scene.nodes().map(|n| (n, identity())).collect();
        let mut seen = HashSet::new();
        let mut primitives = Vec::new();
        let mut total_vec3 = 0usize;
        let mut frame_count = 1;
        while let Some((node, parent)) = stack.pop() {
            if !seen.insert(node.index()) || seen.len() > MAX_NODES {
                return Err(invalid("cyclic or excessive glTF nodes"));
            }
            let transform = multiply(parent, node.transform().matrix());
            if !transform.iter().flatten().all(|value| value.is_finite()) {
                return Err(invalid("non-finite glTF transform"));
            }
            for child in node.children() {
                stack.push((child, transform));
            }
            let Some(mesh) = node.mesh() else {
                continue;
            };
            let name = mesh.name().unwrap_or("").to_owned();
            for primitive in mesh.primitives() {
                if primitive.mode() != gltf::mesh::Mode::Triangles {
                    return Err(invalid("non-triangle glTF primitive"));
                }
                let vertex_count = primitive
                    .get(&gltf::Semantic::Positions)
                    .ok_or_else(|| invalid("missing glTF position accessor"))?
                    .count();
                if vertex_count == 0
                    || vertex_count > MAX_VEC3_VALUES
                    || total_vec3
                        .checked_add(vertex_count)
                        .is_none_or(|n| n > MAX_VEC3_VALUES)
                {
                    return Err(invalid("glTF exceeds geometry limit"));
                }
                if primitive.indices().is_some_and(|a| a.count() > MAX_INDICES) {
                    return Err(invalid("glTF exceeds index limit"));
                }
                if primitive
                    .get(&gltf::Semantic::TexCoords(0))
                    .is_none_or(|a| a.count() != vertex_count)
                {
                    return Err(invalid("missing or invalid glTF UV accessor"));
                }
                let reader =
                    primitive.reader(|buffer| buffers.get(buffer.index()).map(Vec::as_slice));
                let positions: Vec<_> = reader
                    .read_positions()
                    .ok_or_else(|| invalid("missing or invalid glTF positions"))?
                    .collect();
                if positions.is_empty()
                    || positions.len() > MAX_VEC3_VALUES
                    || positions.iter().flatten().any(|v| !v.is_finite())
                {
                    return Err(invalid("invalid glTF positions"));
                }
                total_vec3 = total_vec3
                    .checked_add(positions.len())
                    .ok_or_else(|| invalid("glTF vertex count overflow"))?;
                let normals = if let Some(accessor) = primitive.get(&gltf::Semantic::Normals) {
                    if accessor.count() != vertex_count {
                        return Err(invalid("invalid glTF normal count"));
                    }
                    total_vec3 = total_vec3
                        .checked_add(vertex_count)
                        .filter(|&n| n <= MAX_VEC3_VALUES)
                        .ok_or_else(|| invalid("glTF exceeds geometry limit"))?;
                    let values: Vec<_> = reader
                        .read_normals()
                        .ok_or_else(|| invalid("invalid glTF normal accessor"))?
                        .collect();
                    if values.len() != vertex_count
                        || values.iter().any(|n| {
                            !n.iter().all(|v| v.is_finite())
                                || n.iter().map(|v| v * v).sum::<f32>() <= 0.0
                        })
                    {
                        return Err(invalid("invalid glTF normals"));
                    }
                    Some(values)
                } else {
                    None
                };
                let uv: Vec<_> = reader
                    .read_tex_coords(0)
                    .ok_or_else(|| invalid("missing or invalid glTF UVs"))?
                    .into_f32()
                    .collect();
                if uv.len() != positions.len() || uv.iter().flatten().any(|v| !v.is_finite()) {
                    return Err(invalid("invalid glTF UVs"));
                }
                let indices: Vec<u32> = match reader.read_indices() {
                    Some(indices) => indices.into_u32().collect(),
                    None if primitive.indices().is_none() => {
                        (0..positions.len()).map(|i| i as u32).collect()
                    }
                    None => return Err(invalid("invalid glTF index accessor")),
                };
                if !indices.len().is_multiple_of(3)
                    || indices.iter().any(|&i| i as usize >= positions.len())
                {
                    return Err(invalid("invalid glTF triangle indices"));
                }
                let mut morph_deltas = Vec::new();
                let mut morph_normal_deltas = Vec::new();
                for (target, (position_delta, normal_delta, _)) in
                    primitive.morph_targets().zip(reader.read_morph_targets())
                {
                    if target.positions().is_some() && position_delta.is_none() {
                        return Err(invalid("invalid glTF morph accessor"));
                    }
                    if total_vec3
                        .checked_add(positions.len())
                        .is_none_or(|n| n > MAX_VEC3_VALUES)
                    {
                        return Err(invalid("glTF exceeds geometry limit"));
                    }
                    let values: Vec<_> = match position_delta {
                        Some(values) if values.len() == positions.len() => values.collect(),
                        Some(_) => return Err(invalid("invalid glTF morph count")),
                        None => vec![[0.0; 3]; positions.len()],
                    };
                    if values.len() != positions.len()
                        || values.iter().flatten().any(|v| !v.is_finite())
                    {
                        return Err(invalid("invalid glTF morph target"));
                    }
                    total_vec3 = total_vec3
                        .checked_add(values.len())
                        .ok_or_else(|| invalid("glTF morph count overflow"))?;
                    if total_vec3 > MAX_VEC3_VALUES {
                        return Err(invalid("glTF exceeds geometry limit"));
                    }
                    morph_deltas.push(values);
                    let normal_values = if let Some(accessor) = target.normals() {
                        if normals.is_none() || accessor.count() != vertex_count {
                            return Err(invalid(
                                "invalid glTF morph normal count or missing base normals",
                            ));
                        }
                        total_vec3 = total_vec3
                            .checked_add(vertex_count)
                            .filter(|&n| n <= MAX_VEC3_VALUES)
                            .ok_or_else(|| invalid("glTF exceeds geometry limit"))?;
                        let values: Vec<_> = normal_delta
                            .ok_or_else(|| invalid("invalid glTF morph normal accessor"))?
                            .collect();
                        if values.len() != vertex_count
                            || values.iter().flatten().any(|v| !v.is_finite())
                        {
                            return Err(invalid("invalid glTF morph normals"));
                        }
                        Some(values)
                    } else {
                        None
                    };
                    morph_normal_deltas.push(normal_values);
                }
                frame_count = frame_count.max(1 + morph_deltas.len());
                let texture = primitive
                    .material()
                    .pbr_metallic_roughness()
                    .base_color_texture()
                    .map(|info| info.texture().source().index())
                    .or(fallback);
                primitives.push(Primitive {
                    name: name.clone(),
                    positions,
                    normals,
                    uv,
                    indices,
                    morph_deltas,
                    morph_normal_deltas,
                    node_transform: transform,
                    texture,
                    transparent: name.ends_with(" transparent"),
                });
            }
        }
        if primitives.is_empty() {
            return Err(invalid("glTF scene contains no renderable meshes"));
        }
        Ok(Self {
            primitives,
            images,
            frame_count,
        })
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn fixture(invalid_index: bool) -> (Vec<u8>, Vec<u8>) {
        let mut bin = Vec::new();
        for value in [
            0_f32, 0., 0., 1., 0., 0., 0., 1., 0., 0., 0., 1., 0., 0., 1.,
        ] {
            bin.extend_from_slice(&value.to_le_bytes());
        }
        for value in [0_u16, 1, if invalid_index { 9 } else { 2 }] {
            bin.extend_from_slice(&value.to_le_bytes());
        }
        bin.extend_from_slice(&[0, 0]);
        for value in [0_f32, 0., 1., 0., 0., 1., 0., 0., 1.] {
            bin.extend_from_slice(&value.to_le_bytes());
        }
        assert_eq!(bin.len(), 104);
        let encoded = base64::engine::general_purpose::STANDARD.encode(bin);
        let json = format!(r#"{{
          "asset":{{"version":"2.0"}},"scene":0,"scenes":[{{"nodes":[0]}}],
          "nodes":[{{"mesh":0,"translation":[1,2,3]}}],
          "buffers":[{{"byteLength":104,"uri":"data:application/octet-stream;base64,{encoded}"}}],
          "bufferViews":[{{"buffer":0,"byteOffset":0,"byteLength":36}},{{"buffer":0,"byteOffset":36,"byteLength":24}},{{"buffer":0,"byteOffset":60,"byteLength":6}},{{"buffer":0,"byteOffset":68,"byteLength":36}}],
          "accessors":[{{"bufferView":0,"componentType":5126,"count":3,"type":"VEC3","min":[0,0,0],"max":[1,1,0]}},{{"bufferView":1,"componentType":5126,"count":3,"type":"VEC2"}},{{"bufferView":2,"componentType":5123,"count":3,"type":"SCALAR"}},{{"bufferView":3,"componentType":5126,"count":3,"type":"VEC3"}}],
          "meshes":[{{"name":"fixture","primitives":[{{"attributes":{{"POSITION":0,"TEXCOORD_0":1}},"indices":2,"targets":[{{"POSITION":3}}]}}]}}]
        }}"#).into_bytes();
        let mut png = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut png, 1, 1);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().unwrap();
            writer.write_image_data(&[255, 0, 128, 64]).unwrap();
        }
        (json, png)
    }
    fn tempdir() -> std::path::PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("openchasm-gltf-{}-{unique}", std::process::id()))
    }
    #[test]
    fn imports_basis_morph_texture_and_node_transform() {
        let dir = tempdir();
        std::fs::create_dir_all(dir.join("model")).unwrap();
        let (json, png) = fixture(false);
        std::fs::write(dir.join("model/fixture.gltf"), json).unwrap();
        std::fs::write(dir.join("model/fixture.png"), png).unwrap();
        let mut source = ResourceSet::open(&dir).unwrap();
        let model = GltfModel::load(&mut source, "model/fixture.gltf").unwrap();
        assert_eq!(model.frame_count, 2);
        assert_eq!(model.primitives[0].indices, [0, 1, 2]);
        assert_eq!(
            model.primitives[0].frame_positions(0).unwrap()[0],
            [0.0, 0.0, 0.0]
        );
        assert_eq!(
            model.primitives[0].frame_positions(1).unwrap()[0],
            [0.0, 0.0, 1.0]
        );
        assert!(model.primitives[0].frame_positions(2).is_none());
        assert_eq!(model.primitives[0].node_transform[3], [1.0, 2.0, 3.0, 1.0]);
        assert_eq!(model.primitives[0].texture, Some(0));
        assert_eq!(model.images[0].rgba, [255, 0, 128, 64]);
        std::fs::remove_dir_all(dir).unwrap();
    }
    fn normal_fixture(normal: [f32; 3], delta: [f32; 3]) -> serde_json::Value {
        let (json, _) = fixture(false);
        let mut json: serde_json::Value = serde_json::from_slice(&json).unwrap();
        let mut bytes = Vec::new();
        for vector in [normal, delta] {
            for _ in 0..3 {
                for value in vector {
                    bytes.extend_from_slice(&value.to_le_bytes());
                }
            }
        }
        json["buffers"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({
                "byteLength": 72, "uri": format!("data:application/octet-stream;base64,{}",
                    base64::engine::general_purpose::STANDARD.encode(bytes))
            }));
        for offset in [0, 36] {
            let view = json["bufferViews"].as_array().unwrap().len();
            json["bufferViews"]
                .as_array_mut()
                .unwrap()
                .push(serde_json::json!({
                    "buffer": 1, "byteOffset": offset, "byteLength": 36
                }));
            json["accessors"]
                .as_array_mut()
                .unwrap()
                .push(serde_json::json!({
                    "bufferView": view, "componentType": 5126, "count": 3, "type": "VEC3"
                }));
        }
        json["meshes"][0]["primitives"][0]["attributes"]["NORMAL"] = 4.into();
        json["meshes"][0]["primitives"][0]["targets"][0]["NORMAL"] = 5.into();
        json
    }
    fn load_normal_fixture(json: &serde_json::Value) -> io::Result<GltfModel> {
        let dir = tempdir();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("fixture.gltf"), serde_json::to_vec(json).unwrap()).unwrap();
        let mut source = ResourceSet::open(&dir).unwrap();
        let result = GltfModel::load(&mut source, "fixture.gltf");
        std::fs::remove_dir_all(dir).unwrap();
        result
    }
    #[test]
    fn imports_normal_deltas_in_matching_morph_frame() {
        let json = normal_fixture([0.0, 0.0, 1.0], [1.0, 0.0, -1.0]);
        let model = load_normal_fixture(&json).unwrap();
        let primitive = &model.primitives[0];
        assert_eq!(primitive.frame_normal(0, 1), Some([0.0, 0.0, 1.0]));
        assert_eq!(primitive.frame_normal(1, 1), Some([1.0, 0.0, 0.0]));
        let mut no_delta = json;
        no_delta["meshes"][0]["primitives"][0]["targets"][0]
            .as_object_mut()
            .unwrap()
            .remove("NORMAL");
        let model = load_normal_fixture(&no_delta).unwrap();
        assert_eq!(
            model.primitives[0].frame_normal(1, 1),
            Some([0.0, 0.0, 1.0])
        );
    }
    #[test]
    fn rejects_invalid_base_and_morph_normals() {
        for (normal, delta) in [
            ([0.0; 3], [0.0; 3]),
            ([f32::NAN, 0.0, 1.0], [0.0; 3]),
            ([0.0, 0.0, 1.0], [f32::INFINITY, 0.0, 0.0]),
        ] {
            assert!(load_normal_fixture(&normal_fixture(normal, delta)).is_err());
        }
        let good = normal_fixture([0.0, 0.0, 1.0], [0.0; 3]);
        for accessor in [4, 5] {
            let mut json = good.clone();
            json["accessors"][accessor]["count"] = 2.into();
            assert!(load_normal_fixture(&json).is_err());
        }
        let mut missing_base = good;
        missing_base["meshes"][0]["primitives"][0]["attributes"]
            .as_object_mut()
            .unwrap()
            .remove("NORMAL");
        assert!(load_normal_fixture(&missing_base).is_err());
    }
    #[test]
    fn rejects_bad_indices_and_unsafe_resources() {
        let dir = tempdir();
        std::fs::create_dir_all(dir.join("model")).unwrap();
        let (json, _) = fixture(true);
        std::fs::write(dir.join("model/bad.gltf"), json).unwrap();
        let mut source = ResourceSet::open(&dir).unwrap();
        assert!(GltfModel::load(&mut source, "model/bad.gltf").is_err());
        for uri in [
            "../bad",
            "/absolute",
            "a\\b",
            "https://host/file",
            "%2e%2e/file",
        ] {
            assert!(relative_resource("model/file.gltf", uri).is_err(), "{uri}");
        }
        assert_eq!(
            relative_resource("model/file.gltf", "texture.png").unwrap(),
            "model/texture.png"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
