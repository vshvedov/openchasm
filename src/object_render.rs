//! CPU renderer for original 3O map objects.
//!
//! The viewer's camera is intentionally shared with its wall projection. Meshes
//! gameplay and actor state remain owned by the simulation. The caller supplies
//! actor animation mode and elapsed fixed time for presentation.
use crate::actors::{ActorDefinitions, ActorState};
use crate::content::ResourceSet;
use crate::gltf_model::{GltfModel, Image as GltfImage};
use crate::model::{Animation, Model, Vertex as ModelVertex, SKIN_WIDTH};
use crate::objects::{self, ObjectDefinitions};
use crate::texture::Palette;
use crate::world::Player;
use std::collections::{BTreeSet, HashMap};

const MODEL_SCALE: f32 = 1.0 / 2048.0;
const NEAR: f32 = 0.05;
const CAMERA_PLANE: f32 = 0.66;
const ACTOR_ANIMATION_FPS: f64 = 18.0;
const GLTF_OBJECT_FPS: f64 = 20.0;
const GLTF_ACTOR_FPS: f64 = 1000.0 / 60.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct GltfClip {
    first: usize,
    count: usize,
}

/// Parse the remaster's `animation_id first_frame frame_count` table.
fn parse_gltf_clips(bytes: &[u8], frames: usize) -> std::io::Result<[Option<GltfClip>; 20]> {
    let text = std::str::from_utf8(bytes).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "glTF animation table is not UTF-8",
        )
    })?;
    let mut clips = [None; 20];
    for line in text.lines() {
        let line = line.split(';').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let fields: Vec<_> = line.split_whitespace().collect();
        if fields.len() != 3 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "invalid glTF animation row",
            ));
        }
        let values = fields
            .iter()
            .map(|v| v.parse::<usize>())
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "invalid glTF animation value",
                )
            })?;
        let (id, first, count) = (values[0], values[1], values[2]);
        if id >= clips.len()
            || count == 0
            || first.checked_add(count).is_none_or(|end| end > frames)
            || clips[id].is_some()
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "invalid glTF animation range or duplicate",
            ));
        }
        clips[id] = Some(GltfClip { first, count });
    }
    Ok(clips)
}

fn actor_gltf_frame(
    clips: &[Option<GltfClip>; 20],
    mode: ActorAnimationMode,
    seconds: f64,
) -> usize {
    let Some(clip) = mode.preferred_slots().iter().find_map(|&slot| clips[slot]) else {
        return 0;
    };
    let elapsed_frame = seconds.max(0.0) * GLTF_ACTOR_FPS;
    let frame = if mode.looping() {
        elapsed_frame.rem_euclid(clip.count as f64) as usize
    } else {
        elapsed_frame.min((clip.count - 1) as f64) as usize
    };
    clip.first + frame
}

fn intact_actor_primitive(name: &str, mode: ActorAnimationMode) -> bool {
    let stem = name.strip_suffix(" transparent").unwrap_or(name);
    if stem.ends_with(" removed") {
        return false;
    }
    if stem.ends_with(" not shooting") {
        return mode != ActorAnimationMode::Attack;
    }
    if stem.ends_with(" shooting") {
        return mode == ActorAnimationMode::Attack;
    }
    true
}

/// Shared camera pose for all world-space renderers.
#[derive(Clone, Copy)]
pub struct CameraView {
    pub player: Player,
    pub eye_height: f32,
}

/// Provisional mapping from simulation behavior to CAR animation slots.
#[allow(dead_code)] // Modes are supplied by the viewer once integrated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActorAnimationMode {
    Idle,
    Run,
    Attack,
    Death,
}
impl ActorAnimationMode {
    fn preferred_slots(self) -> &'static [usize] {
        match self {
            Self::Idle => &[2, 3, 0],
            Self::Run => &[0, 2, 3],
            Self::Attack => &[6, 7, 8, 5, 2, 0],
            Self::Death => &[16, 17, 2, 0],
        }
    }
    fn looping(self) -> bool {
        self != Self::Death
    }
}

struct Mesh {
    model: Model,
    radius: f32,
    /// CAR texture coordinates use 8 fractional bits unlike 3O.
    car_uv: bool,
    animations: [Option<Animation>; 20],
    /// None for original 3O/CAR geometry; glTF meshes use the original model
    /// field as an empty placeholder so existing actor behavior is unchanged.
    gltf: Option<GltfModel>,
    gltf_actor_clips: [Option<GltfClip>; 20],
}

struct Instance {
    mesh: usize,
    position: [f32; 2],
    angle: f32,
    elevation: f32,
    cell: Option<[usize; 2]>,
    actor_source_index: Option<usize>,
    object_definition_index: Option<usize>,
    object_animation_playing: bool,
    object_animation_initially_playing: bool,
    object_animation_seconds: f64,
    animation_mode: ActorAnimationMode,
    animation_seconds: f64,
    visible: bool,
}

/// Choose a pose from the available slot list. Missing slots use the base mesh.
fn actor_vertices(mesh: &Mesh, mode: ActorAnimationMode, seconds: f64) -> &[ModelVertex] {
    let animation = mode.preferred_slots().iter().find_map(|&slot| {
        mesh.animations[slot]
            .as_ref()
            .filter(|a| !a.frames.is_empty())
    });
    let Some(animation) = animation else {
        return &mesh.model.vertices;
    };
    let count = animation.frames.len();
    let frame = seconds.max(0.0) * ACTOR_ANIMATION_FPS;
    let index = if mode.looping() {
        frame.rem_euclid(count as f64) as usize
    } else {
        frame.min((count - 1) as f64) as usize
    };
    &animation.frames[index]
}

fn object_vertices(mesh: &Mesh, playing: bool, seconds: f64) -> &[ModelVertex] {
    let Some(animation) = mesh.animations[0].as_ref().filter(|a| !a.frames.is_empty()) else {
        return &mesh.model.vertices;
    };
    let index = if playing {
        (seconds.max(0.0) * ACTOR_ANIMATION_FPS).rem_euclid(animation.frames.len() as f64) as usize
    } else {
        0
    };
    &animation.frames[index]
}

fn vertex_radius(vertices: &[ModelVertex]) -> f32 {
    vertices
        .iter()
        .map(|vertex| {
            let x = f32::from(vertex.position[0]) * MODEL_SCALE;
            let y = f32::from(vertex.position[1]) * MODEL_SCALE;
            (x * x + y * y).sqrt()
        })
        .fold(0.0_f32, f32::max)
}

fn transform_gltf(matrix: [[f32; 4]; 4], pos: [f32; 3]) -> [f32; 3] {
    std::array::from_fn(|row| {
        matrix[0][row] * pos[0] + matrix[1][row] * pos[1] + matrix[2][row] * pos[2] + matrix[3][row]
    })
}

fn gltf_radius(model: &GltfModel) -> f32 {
    let mut radius = 0.0_f32;
    for primitive in &model.primitives {
        for (i, base) in primitive.positions.iter().enumerate() {
            for delta in
                std::iter::once([0.0; 3]).chain(primitive.morph_deltas.iter().map(|frame| frame[i]))
            {
                let local = transform_gltf(
                    primitive.node_transform,
                    [base[0] + delta[0], base[1] + delta[1], base[2] + delta[2]],
                );
                radius = radius.max((local[0] * local[0] + local[1] * local[1]).sqrt());
            }
        }
    }
    radius
}

/// A deduplicated collection of static object meshes and placements.
pub struct ObjectScene {
    meshes: Vec<Mesh>,
    instances: Vec<Instance>,
    palette: [u32; 256],
}

fn texture_uv(polygon: &crate::model::Polygon, corner: usize, car_uv: bool) -> [f32; 2] {
    let u = f32::from(polygon.uv[corner][0]);
    let v = f32::from(polygon.uv[corner][1]);
    if car_uv {
        [
            u / 256.0,
            (v + 4.0 * f32::from(polygon.sprite_offset)) / 256.0,
        ]
    } else {
        [u, v + f32::from(polygon.sprite_offset)]
    }
}

#[derive(Clone, Copy)]
struct Vertex {
    side: f32,
    up: f32,
    depth: f32,
    uv: [f32; 2],
}
impl Vertex {
    fn interpolate(self, next: Self, t: f32) -> Self {
        let mix = |a, b| a + (b - a) * t;
        Self {
            side: mix(self.side, next.side),
            up: mix(self.up, next.up),
            depth: mix(self.depth, next.depth),
            uv: [mix(self.uv[0], next.uv[0]), mix(self.uv[1], next.uv[1])],
        }
    }
}

/// Clips a polygon to the near plane before perspective division.
fn clip_near(input: &[Vertex]) -> Vec<Vertex> {
    if input.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(input.len() + 1);
    let mut previous = input[input.len() - 1];
    for &current in input {
        let previous_inside = previous.depth >= NEAR;
        let current_inside = current.depth >= NEAR;
        if previous_inside != current_inside {
            let t = (NEAR - previous.depth) / (current.depth - previous.depth);
            out.push(previous.interpolate(current, t));
        }
        if current_inside {
            out.push(current);
        }
        previous = current;
    }
    out
}

#[derive(Clone, Copy)]
struct ScreenVertex {
    x: f32,
    y: f32,
    inv_depth: f32,
    u_over_depth: f32,
    v_over_depth: f32,
}
fn project(v: Vertex, width: usize, height: usize) -> ScreenVertex {
    let inv_depth = v.depth.recip();
    ScreenVertex {
        x: width as f32 * 0.5 * (1.0 + v.side * inv_depth / CAMERA_PLANE),
        y: height as f32 * 0.5 * (1.0 - v.up * inv_depth),
        inv_depth,
        u_over_depth: v.uv[0] * inv_depth,
        v_over_depth: v.uv[1] * inv_depth,
    }
}
fn edge(a: ScreenVertex, b: ScreenVertex, x: f32, y: f32) -> f32 {
    (x - a.x) * (b.y - a.y) - (y - a.y) * (b.x - a.x)
}

// Limit each pixel to its eight nearest translucent fragments. This bounds the
// temporary buffer even in maps with many intersecting effects. At overflow,
// more distant transparent layers are discarded; opaque depth remains exact.
const MAX_TRANSLUCENT_LAYERS: u8 = 8;
const NO_FRAGMENT: u32 = u32::MAX;
#[derive(Clone, Copy)]
struct Fragment {
    next: u32,
    depth: f32,
    rgb: u32,
    alpha: u8,
}
struct Translucency {
    heads: Vec<u32>,
    counts: Vec<u8>,
    fragments: Vec<Fragment>,
}
impl Translucency {
    fn new(pixels: usize) -> Self {
        Self {
            heads: vec![NO_FRAGMENT; pixels],
            counts: vec![0; pixels],
            fragments: Vec::new(),
        }
    }
    fn push(&mut self, pixel: usize, depth: f32, rgb: u32, alpha: u8) {
        let mut current = self.heads[pixel];
        let mut farthest = NO_FRAGMENT;
        while current != NO_FRAGMENT {
            let f = self.fragments[current as usize];
            // Adjacent triangles can cover the same edge sample. Avoid double
            // blending identical samples on a shared edge.
            if (f.depth - depth).abs() < 0.00001 && f.rgb == rgb && f.alpha == alpha {
                return;
            }
            if farthest == NO_FRAGMENT || f.depth > self.fragments[farthest as usize].depth {
                farthest = current;
            }
            current = f.next;
        }
        if self.counts[pixel] == MAX_TRANSLUCENT_LAYERS {
            let f = &mut self.fragments[farthest as usize];
            if depth < f.depth {
                *f = Fragment {
                    next: f.next,
                    depth,
                    rgb,
                    alpha,
                };
            }
        } else if let Ok(index) = u32::try_from(self.fragments.len()) {
            // NO_FRAGMENT is reserved as the list terminator.
            if index == NO_FRAGMENT {
                return;
            }
            self.fragments.push(Fragment {
                next: self.heads[pixel],
                depth,
                rgb,
                alpha,
            });
            self.heads[pixel] = index;
            self.counts[pixel] += 1;
        }
    }
    fn composite(&self, frame: &mut [u32], depth: &[f32]) {
        for (pixel, &head) in self.heads.iter().enumerate() {
            if head == NO_FRAGMENT {
                continue;
            }
            let mut sorted = [Fragment {
                next: NO_FRAGMENT,
                depth: 0.0,
                rgb: 0,
                alpha: 0,
            }; MAX_TRANSLUCENT_LAYERS as usize];
            let mut count = 0;
            let mut current = head;
            while current != NO_FRAGMENT {
                let f = self.fragments[current as usize];
                if f.depth < depth[pixel] {
                    sorted[count] = f;
                    count += 1;
                }
                current = f.next;
            }
            sorted[..count].sort_unstable_by(|a, b| b.depth.total_cmp(&a.depth));
            for f in &sorted[..count] {
                let dst = frame[pixel];
                let blend = |shift: u32| {
                    let src = (f.rgb >> shift) & 255_u32;
                    let back = (dst >> shift) & 255_u32;
                    (src * u32::from(f.alpha) + back * (255 - u32::from(f.alpha))) / 255
                };
                frame[pixel] = (blend(16) << 16) | (blend(8) << 8) | blend(0);
            }
        }
    }
}

struct Raster<'a> {
    frame: &'a mut [u32],
    depth: &'a mut [f32],
    width: usize,
    height: usize,
    palette: &'a [u32; 256],
    translucent: Option<Translucency>,
}
impl Raster<'_> {
    fn put(&mut self, pixel: usize, distance: f32, rgb: u32, alpha: u8) {
        if alpha == 255 {
            self.frame[pixel] = rgb;
            self.depth[pixel] = distance;
        } else if alpha != 0 {
            self.translucent
                .get_or_insert_with(|| Translucency::new(self.frame.len()))
                .push(pixel, distance, rgb, alpha);
        }
    }
    fn finish(&mut self) {
        if let Some(translucent) = &self.translucent {
            translucent.composite(self.frame, self.depth);
        }
    }
    fn triangle(&mut self, triangle: [ScreenVertex; 3], model: &Model, flags: u8) {
        let [a, b, c] = triangle;
        let area = edge(a, b, c.x, c.y);
        if area.abs() < 0.00001 || model.skin_height == 0 {
            return;
        }
        let min_x = a.x.min(b.x).min(c.x).floor().max(0.0) as usize;
        let max_x = a.x.max(b.x).max(c.x).ceil().min(self.width as f32) as usize;
        let min_y = a.y.min(b.y).min(c.y).floor().max(0.0) as usize;
        let max_y = a.y.max(b.y).max(c.y).ceil().min(self.height as f32) as usize;
        if min_x >= max_x || min_y >= max_y {
            return;
        }
        for y in min_y..max_y {
            for x in min_x..max_x {
                let px = x as f32 + 0.5;
                let py = y as f32 + 0.5;
                let w0 = edge(b, c, px, py) / area;
                let w1 = edge(c, a, px, py) / area;
                let w2 = 1.0 - w0 - w1;
                if w0 < -0.00001 || w1 < -0.00001 || w2 < -0.00001 {
                    continue;
                }
                let inverse_depth = w0 * a.inv_depth + w1 * b.inv_depth + w2 * c.inv_depth;
                if inverse_depth <= 0.0 {
                    continue;
                }
                let distance = inverse_depth.recip();
                let offset = y * self.width + x;
                if distance >= self.depth[offset] {
                    continue;
                }
                let u =
                    (w0 * a.u_over_depth + w1 * b.u_over_depth + w2 * c.u_over_depth) * distance;
                let v =
                    (w0 * a.v_over_depth + w1 * b.v_over_depth + w2 * c.v_over_depth) * distance;
                let tx = (u.floor() as i32).rem_euclid(SKIN_WIDTH as i32) as usize;
                let ty = (v.floor() as i32).rem_euclid(i32::from(model.skin_height)) as usize;
                let index = model.skin_indices[ty * SKIN_WIDTH + tx];
                if flags & 0x0e != 0 && index == 255 {
                    continue;
                }
                let rgb = self.palette[index as usize];
                let attenuation = (1.0 / (1.0 + distance * 0.08)).clamp(0.25, 1.0);
                let channel = |base: u32| (base as f32 * attenuation) as u32;
                let rgb = (channel((rgb >> 16) & 255) << 16)
                    | (channel((rgb >> 8) & 255) << 8)
                    | channel(rgb & 255);
                // Both legacy translucent flag bits use provisional half alpha.
                self.put(
                    offset,
                    distance,
                    rgb,
                    if flags & 0x0c != 0 { 128 } else { 255 },
                );
            }
        }
    }
}

impl Raster<'_> {
    /// Perspective-correct RGBA rasterization into the same depth buffer used
    /// by 3O models and walls. Partial alpha blending is approximate.
    fn gltf_triangle(&mut self, triangle: [ScreenVertex; 3], image: Option<&GltfImage>) {
        let [a, b, c] = triangle;
        let area = edge(a, b, c.x, c.y);
        if area.abs() < 0.00001 {
            return;
        }
        let min_x = a.x.min(b.x).min(c.x).floor().max(0.0) as usize;
        let max_x = a.x.max(b.x).max(c.x).ceil().min(self.width as f32) as usize;
        let min_y = a.y.min(b.y).min(c.y).floor().max(0.0) as usize;
        let max_y = a.y.max(b.y).max(c.y).ceil().min(self.height as f32) as usize;
        if min_x >= max_x || min_y >= max_y {
            return;
        }
        for y in min_y..max_y {
            for x in min_x..max_x {
                let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
                let w0 = edge(b, c, px, py) / area;
                let w1 = edge(c, a, px, py) / area;
                let w2 = 1.0 - w0 - w1;
                if w0 < -0.00001 || w1 < -0.00001 || w2 < -0.00001 {
                    continue;
                }
                let inverse = w0 * a.inv_depth + w1 * b.inv_depth + w2 * c.inv_depth;
                if inverse <= 0.0 {
                    continue;
                }
                let distance = inverse.recip();
                let pixel = y * self.width + x;
                if distance >= self.depth[pixel] {
                    continue;
                }
                let u =
                    (w0 * a.u_over_depth + w1 * b.u_over_depth + w2 * c.u_over_depth) * distance;
                let v =
                    (w0 * a.v_over_depth + w1 * b.v_over_depth + w2 * c.v_over_depth) * distance;
                let rgba = if let Some(image) = image {
                    let tx = ((u.rem_euclid(1.0) * image.width as f32) as usize)
                        .min(image.width as usize - 1);
                    let ty = ((v.rem_euclid(1.0) * image.height as f32) as usize)
                        .min(image.height as usize - 1);
                    let i = (ty * image.width as usize + tx) * 4;
                    [
                        image.rgba[i],
                        image.rgba[i + 1],
                        image.rgba[i + 2],
                        image.rgba[i + 3],
                    ]
                } else {
                    [200, 200, 200, 255]
                };
                if rgba[3] == 0 {
                    continue;
                }
                let attenuation = (1.0 / (1.0 + distance * 0.08)).clamp(0.25, 1.0);
                let rgb = ((f32::from(rgba[0]) * attenuation) as u32) << 16
                    | ((f32::from(rgba[1]) * attenuation) as u32) << 8
                    | (f32::from(rgba[2]) * attenuation) as u32;
                self.put(pixel, distance, rgb, rgba[3]);
            }
        }
    }
}

fn draw_gltf(raster: &mut Raster<'_>, mesh: &Mesh, instance: &Instance, camera: CameraView) {
    let Some(model) = mesh.gltf.as_ref() else {
        return;
    };
    let frame = if instance.actor_source_index.is_some() {
        actor_gltf_frame(
            &mesh.gltf_actor_clips,
            instance.animation_mode,
            instance.animation_seconds,
        )
    } else if instance.object_animation_playing {
        (instance.object_animation_seconds.max(0.0) * GLTF_OBJECT_FPS)
            .rem_euclid(model.frame_count as f64) as usize
    } else {
        0
    };
    let (sin, cos) = instance.angle.sin_cos();
    let (camera_sin, camera_cos) = camera.player.angle.sin_cos();
    let dx = instance.position[0] - camera.player.position[0];
    let dy = instance.position[1] - camera.player.position[1];
    for primitive in &model.primitives {
        if instance.actor_source_index.is_some()
            && !intact_actor_primitive(&primitive.name, instance.animation_mode)
        {
            continue;
        }
        let delta = frame
            .checked_sub(1)
            .and_then(|i| primitive.morph_deltas.get(i));
        let image = primitive.texture.and_then(|i| model.images.get(i));
        for indices in primitive.indices.chunks(3) {
            let mut vertices = Vec::with_capacity(3);
            for &index in indices {
                let i = index as usize;
                let base = primitive.positions[i];
                let d = delta.map(|target| target[i]).unwrap_or([0.0; 3]);
                let p = transform_gltf(
                    primitive.node_transform,
                    [base[0] + d[0], base[1] + d[1], base[2] + d[2]],
                );
                if !p.iter().all(|value| value.is_finite()) {
                    vertices.clear();
                    break;
                }
                // The bundled replacement geometry matches 3O tile-space axes:
                // X/Y horizontal, Z vertical, with positions already scaled.
                let wx = dx + p[0] * cos - p[1] * sin;
                let wy = dy + p[0] * sin + p[1] * cos;
                vertices.push(Vertex {
                    side: -wx * camera_sin + wy * camera_cos,
                    up: p[2] + instance.elevation - camera.eye_height,
                    depth: wx * camera_cos + wy * camera_sin,
                    uv: primitive.uv[i],
                });
            }
            let clipped = clip_near(&vertices);
            if clipped.len() < 3 {
                continue;
            }
            let first = project(clipped[0], raster.width, raster.height);
            for i in 1..clipped.len() - 1 {
                raster.gltf_triangle(
                    [
                        first,
                        project(clipped[i], raster.width, raster.height),
                        project(clipped[i + 1], raster.width, raster.height),
                    ],
                    image,
                );
            }
        }
    }
}

impl ObjectScene {
    /// Load supported original models once, retaining only unique diagnostics.
    /// glTF-only and missing/unreadable objects are skipped for now.
    pub fn load(
        resources: &mut ResourceSet,
        map: &crate::map::Map,
        definitions: &ObjectDefinitions,
        palette: &Palette,
    ) -> Self {
        let mut meshes = Vec::new();
        let mut instances = Vec::new();
        let mut cache: HashMap<(String, Option<String>), Option<usize>> = HashMap::new();
        let mut warnings = BTreeSet::new();
        for instance in objects::instances(map) {
            let Some(definition) = definitions.definition(&instance) else {
                warnings.insert(format!(
                    "missing {:?} definition {}",
                    instance.kind, instance.definition_index
                ));
                continue;
            };
            let model_3o = definition.model_3o_path();
            let gltf_only = model_3o.is_none();
            let path = model_3o.unwrap_or_else(|| definition.model_gltf_path());
            let animation_path = if gltf_only {
                None
            } else {
                definition.animation_path()
            };
            let cache_key = (path.clone(), animation_path.clone());
            let mesh_index = *cache.entry(cache_key).or_insert_with(|| {
                if gltf_only {
                    match GltfModel::load(resources, &path) {
                        Ok(gltf) => {
                            let radius = gltf_radius(&gltf);
                            let index = meshes.len();
                            meshes.push(Mesh {
                                model: Model {
                                    vertices: vec![],
                                    polygons: vec![],
                                    skin_height: 0,
                                    skin_indices: vec![],
                                },
                                radius,
                                car_uv: false,
                                animations: std::array::from_fn(|_| None),
                                gltf: Some(gltf),
                                gltf_actor_clips: [None; 20],
                            });
                            return Some(index);
                        }
                        Err(error) => {
                            warnings.insert(format!("{path}: {error}"));
                            return None;
                        }
                    }
                }
                match resources.read(&path).and_then(|bytes| Model::parse(&bytes)) {
                    Ok(model) => {
                        let mut animations: [Option<Animation>; 20] = std::array::from_fn(|_| None);
                        if let Some(animation_path) = &animation_path {
                            match resources
                                .read(animation_path)
                                .and_then(|bytes| Animation::parse(&bytes))
                            {
                                Ok(animation) if animation.matches_model(&model) => {
                                    animations[0] = Some(animation)
                                }
                                Ok(_) => {
                                    warnings.insert(format!(
                                        "{animation_path}: vertex count mismatch for {path}"
                                    ));
                                }
                                Err(error) => {
                                    warnings.insert(format!("{animation_path}: {error}"));
                                }
                            }
                        }
                        let mut radius = vertex_radius(&model.vertices);
                        if let Some(animation) = &animations[0] {
                            for frame in &animation.frames {
                                radius = radius.max(vertex_radius(frame));
                            }
                        }
                        let index = meshes.len();
                        meshes.push(Mesh {
                            model,
                            radius,
                            car_uv: false,
                            animations,
                            gltf: None,
                            gltf_actor_clips: [None; 20],
                        });
                        Some(index)
                    }
                    Err(error) => {
                        warnings.insert(format!("{path}: {error}"));
                        None
                    }
                }
            });
            if let Some(mesh) = mesh_index {
                let initially_playing = meshes[mesh]
                    .gltf
                    .as_ref()
                    .is_some_and(|gltf| gltf.frame_count > 1)
                    && definition.action_code != 255;
                instances.push(Instance {
                    mesh,
                    position: instance.position,
                    angle: instance.angle_radians,
                    elevation: 0.0,
                    cell: Some(instance.grid_cell),
                    actor_source_index: None,
                    object_definition_index: if instance.kind == objects::ObjectKind::LevelObject {
                        Some(instance.definition_index)
                    } else {
                        None
                    },
                    object_animation_playing: initially_playing,
                    object_animation_initially_playing: initially_playing,
                    object_animation_seconds: 0.0,
                    animation_mode: ActorAnimationMode::Idle,
                    animation_seconds: 0.0,
                    visible: true,
                });
            }
        }
        for warning in warnings {
            eprintln!("object preview: {warning}");
        }
        let palette = std::array::from_fn(|index| {
            let [r, g, b] = palette.rgb8(index as u8);
            (u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b)
        });
        Self {
            meshes,
            instances,
            palette,
        }
    }

    /// Add the initial pose of each definition-backed actor, caching CAR meshes.
    /// Actor state remains separate; these are only render-side instances.
    pub fn add_actors(
        &mut self,
        resources: &mut ResourceSet,
        actors: &ActorState,
        definitions: &ActorDefinitions,
    ) {
        let mut cache: HashMap<String, Option<usize>> = HashMap::new();
        let mut warnings = BTreeSet::new();
        for actor in &actors.actors {
            let Some(definition) = definitions.entries.get(actor.definition_index) else {
                continue;
            };
            let car_path = definition.car_path();
            let gltf_only = car_path.is_none();
            let path = car_path.unwrap_or_else(|| {
                format!(
                    "monster/{}.gltf",
                    definition.model_name.to_ascii_lowercase()
                )
            });
            let mesh_index = *cache.entry(path.clone()).or_insert_with(|| {
                if gltf_only {
                    match GltfModel::load(resources, &path) {
                        Ok(gltf) => {
                            let clip_path = format!("{path}.ani");
                            let clips = match resources
                                .read(&clip_path)
                                .and_then(|data| parse_gltf_clips(&data, gltf.frame_count))
                            {
                                Ok(clips) => clips,
                                Err(error) => {
                                    warnings.insert(format!("{clip_path}: {error}"));
                                    [None; 20]
                                }
                            };
                            let radius = gltf_radius(&gltf);
                            let index = self.meshes.len();
                            self.meshes.push(Mesh {
                                model: Model {
                                    vertices: vec![],
                                    polygons: vec![],
                                    skin_height: 0,
                                    skin_indices: vec![],
                                },
                                radius,
                                car_uv: false,
                                animations: std::array::from_fn(|_| None),
                                gltf: Some(gltf),
                                gltf_actor_clips: clips,
                            });
                            return Some(index);
                        }
                        Err(error) => {
                            warnings.insert(format!("{path}: {error}"));
                            return None;
                        }
                    }
                }
                match resources
                    .read(&path)
                    .and_then(|data| crate::monster::Monster::parse(&data))
                {
                    Ok(car) => {
                        let model = car.model;
                        let animations = car.animations;
                        // Bounds include all animation frames so limbs do not
                        // disappear at the edge of the frustum while moving.
                        let mut radius = vertex_radius(&model.vertices);
                        for animation in animations.iter().flatten() {
                            for frame in &animation.frames {
                                radius = radius.max(vertex_radius(frame));
                            }
                        }
                        let index = self.meshes.len();
                        self.meshes.push(Mesh {
                            model,
                            radius,
                            car_uv: true,
                            animations,
                            gltf: None,
                            gltf_actor_clips: [None; 20],
                        });
                        Some(index)
                    }
                    Err(error) => {
                        warnings.insert(format!("{path}: {error}"));
                        None
                    }
                }
            });
            if let Some(mesh) = mesh_index {
                self.instances.push(Instance {
                    mesh,
                    position: actor.position,
                    angle: actor.angle_radians,
                    elevation: 0.0,
                    cell: None,
                    actor_source_index: Some(actor.source_index),
                    object_definition_index: None,
                    object_animation_playing: false,
                    object_animation_initially_playing: false,
                    object_animation_seconds: 0.0,
                    animation_mode: ActorAnimationMode::Idle,
                    animation_seconds: 0.0,
                    visible: true,
                });
            }
        }
        for warning in warnings {
            eprintln!("actor preview: {warning}");
        }
    }

    /// Whether a rendered object instance has this original map cell.
    #[allow(dead_code)] // Available for callers that need to detect absent assets.
    pub fn has_cell(&self, cell: [usize; 2]) -> bool {
        self.instances.iter().any(|item| item.cell == Some(cell))
    }

    /// Set an object's absolute pose; vertical process units are 64 per tile.
    pub fn set_pose(
        &mut self,
        cell: [usize; 2],
        position: [f32; 2],
        angle: f32,
        elevation_raw: f32,
    ) -> bool {
        if !position.iter().all(|v| v.is_finite())
            || !angle.is_finite()
            || !elevation_raw.is_finite()
        {
            return false;
        }
        if let Some(instance) = self
            .instances
            .iter_mut()
            .find(|item| item.cell == Some(cell))
        {
            instance.position = position;
            instance.angle = angle;
            instance.elevation = elevation_raw / 64.0;
            true
        } else {
            false
        }
    }

    /// Update one render-side actor pose by its stable source placement index.
    #[allow(dead_code)] // Used once simulation state drives actor transforms.
    pub fn set_actor_pose(
        &mut self,
        source_index: usize,
        position: [f32; 2],
        angle: f32,
        visible: bool,
    ) -> bool {
        if let Some(instance) = self
            .instances
            .iter_mut()
            .find(|item| item.actor_source_index == Some(source_index))
        {
            instance.position = position;
            instance.angle = angle;
            instance.visible = visible;
            true
        } else {
            false
        }
    }

    /// Update an actor's presentation timer once per simulation tick. When the
    /// supplied mode changes its animation starts from frame zero. Invalid or
    /// negative deltas are rejected without changing the instance.
    #[allow(dead_code)] // Called by the viewer once integrated.
    pub fn set_actor_animation(
        &mut self,
        source_index: usize,
        mode: ActorAnimationMode,
        delta_seconds: f32,
    ) -> bool {
        if !delta_seconds.is_finite() || delta_seconds < 0.0 {
            return false;
        }
        let Some(instance) = self
            .instances
            .iter_mut()
            .find(|item| item.actor_source_index == Some(source_index))
        else {
            return false;
        };
        if instance.animation_mode != mode {
            instance.animation_mode = mode;
            instance.animation_seconds = 0.0;
        } else {
            instance.animation_seconds += f64::from(delta_seconds);
        }
        true
    }

    /// Start the matching level-object animations after a `playani` effect.
    /// Script values >= 33 identify the `#newobjects` entry `value - 33`;
    /// unknown lower values and entries without compatible ANI data are no-ops.
    /// Returns the number of placements restarted.
    #[allow(dead_code)] // Viewer integrates ProcessEffect separately.
    pub fn play_object_animation(&mut self, script_id: u16) -> usize {
        let Some(definition_index) = script_id.checked_sub(33).map(usize::from) else {
            return 0;
        };
        let mut started = 0;
        for instance in &mut self.instances {
            if instance.object_definition_index != Some(definition_index)
                || (self.meshes[instance.mesh].animations[0].is_none()
                    && self.meshes[instance.mesh]
                        .gltf
                        .as_ref()
                        .is_none_or(|model| model.frame_count <= 1))
            {
                continue;
            }
            instance.object_animation_playing = true;
            instance.object_animation_seconds = 0.0;
            started += 1;
        }
        started
    }

    /// Advance active object presentation timers once per fixed simulation tick.
    /// Invalid or negative elapsed time is rejected without changing any state.
    #[allow(dead_code)] // Viewer integrates fixed-tick presentation separately.
    pub fn tick_object_animations(&mut self, delta_seconds: f32) -> bool {
        if !delta_seconds.is_finite() || delta_seconds < 0.0 {
            return false;
        }
        for instance in &mut self.instances {
            if instance.object_animation_playing {
                instance.object_animation_seconds += f64::from(delta_seconds);
            }
        }
        true
    }

    /// Restore the initial stopped object poses when a level is restarted.
    #[allow(dead_code)] // Viewer integrates restart separately.
    pub fn reset_object_animations(&mut self) {
        for instance in &mut self.instances {
            instance.object_animation_playing = instance.object_animation_initially_playing;
            instance.object_animation_seconds = 0.0;
        }
    }

    /// Hide or restore an individual placement without changing shared meshes.
    /// Grid cells are stable identifiers from the source map.
    #[allow(dead_code)] // Used when pickup/game state integration is enabled.
    pub fn set_visible(&mut self, cell: [usize; 2], visible: bool) -> bool {
        if let Some(instance) = self
            .instances
            .iter_mut()
            .find(|item| item.cell == Some(cell))
        {
            instance.visible = visible;
            true
        } else {
            false
        }
    }

    /// Paint visible objects against per-pixel wall/object depth.
    pub fn render(
        &self,
        frame: &mut [u32],
        depth: &mut [f32],
        width: usize,
        height: usize,
        camera: CameraView,
    ) {
        let player = camera.player;
        if !camera.eye_height.is_finite()
            || width.checked_mul(height) != Some(frame.len())
            || frame.len() != depth.len()
        {
            return;
        }
        let (camera_sin, camera_cos) = player.angle.sin_cos();
        let mut raster = Raster {
            frame,
            depth,
            width,
            height,
            palette: &self.palette,
            translucent: None,
        };
        for instance in &self.instances {
            if !instance.visible {
                continue;
            }
            let mesh = &self.meshes[instance.mesh];
            let dx = instance.position[0] - player.position[0];
            let dy = instance.position[1] - player.position[1];
            let center_depth = dx * camera_cos + dy * camera_sin;
            let center_side = -dx * camera_sin + dy * camera_cos;
            if center_depth + mesh.radius < NEAR
                || center_side.abs() > center_depth.max(0.0) * CAMERA_PLANE + mesh.radius * 1.5
            {
                continue;
            }
            if mesh.gltf.is_some() {
                draw_gltf(&mut raster, mesh, instance, camera);
                continue;
            }
            let (sin, cos) = instance.angle.sin_cos();
            let pose_vertices = if mesh.car_uv {
                actor_vertices(mesh, instance.animation_mode, instance.animation_seconds)
            } else {
                object_vertices(
                    mesh,
                    instance.object_animation_playing,
                    instance.object_animation_seconds,
                )
            };
            for polygon in &mesh.model.polygons {
                // The default pose shows intact body/head/hands, excluding
                // detached parts and attack-only effects.
                if mesh.car_uv && polygon.group != 0 && polygon.group & (1 | 4 | 16) == 0 {
                    continue;
                }
                let count = polygon.vertex_count();
                let mut vertices = Vec::with_capacity(count);
                for corner in 0..count {
                    let p = pose_vertices[polygon.vertices[corner] as usize].position;
                    let x = f32::from(p[0]) * MODEL_SCALE;
                    let y = f32::from(p[1]) * MODEL_SCALE;
                    let world_x = dx + x * cos - y * sin;
                    let world_y = dy + x * sin + y * cos;
                    vertices.push(Vertex {
                        side: -world_x * camera_sin + world_y * camera_cos,
                        up: f32::from(p[2]) * MODEL_SCALE + instance.elevation - camera.eye_height,
                        depth: world_x * camera_cos + world_y * camera_sin,
                        uv: texture_uv(polygon, corner, mesh.car_uv),
                    });
                }
                let clipped = clip_near(&vertices);
                if clipped.len() < 3 {
                    continue;
                }
                let first = project(clipped[0], width, height);
                for i in 1..clipped.len() - 1 {
                    raster.triangle(
                        [
                            first,
                            project(clipped[i], width, height),
                            project(clipped[i + 1], width, height),
                        ],
                        &mesh.model,
                        polygon.flags,
                    );
                }
            }
        }
        raster.finish();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn gltf_scene(alpha: u8) -> ObjectScene {
        let image = GltfImage {
            width: 1,
            height: 1,
            rgba: vec![255, 0, 0, alpha],
        };
        let primitive = crate::gltf_model::Primitive {
            name: "triangle".into(),
            positions: vec![[0.0, -0.5, 0.5], [0.0, 0.5, 0.5], [0.0, 0.0, 1.5]],
            uv: vec![[0.0, 0.0]; 3],
            indices: vec![0, 1, 2],
            morph_deltas: vec![vec![[0.0, 0.4, 0.0]; 3]],
            node_transform: [
                [1., 0., 0., 0.],
                [0., 1., 0., 0.],
                [0., 0., 1., 0.],
                [0., 0., 0., 1.],
            ],
            texture: Some(0),
            transparent: false,
        };
        let gltf = GltfModel {
            primitives: vec![primitive],
            images: vec![image],
            frame_count: 2,
        };
        let mesh = Mesh {
            model: Model {
                vertices: vec![],
                polygons: vec![],
                skin_height: 0,
                skin_indices: vec![],
            },
            radius: gltf_radius(&gltf),
            car_uv: false,
            animations: std::array::from_fn(|_| None),
            gltf: Some(gltf),
            gltf_actor_clips: [None; 20],
        };
        let instance = Instance {
            mesh: 0,
            position: [2.0, 0.0],
            angle: 0.0,
            elevation: 0.0,
            cell: Some([1, 1]),
            actor_source_index: None,
            object_definition_index: Some(0),
            object_animation_playing: false,
            object_animation_initially_playing: false,
            object_animation_seconds: 0.0,
            animation_mode: ActorAnimationMode::Idle,
            animation_seconds: 0.0,
            visible: true,
        };
        ObjectScene {
            meshes: vec![mesh],
            instances: vec![instance],
            palette: [0; 256],
        }
    }
    #[test]
    fn parses_gltf_actor_clips_and_selects_mode_frames() {
        let clips = parse_gltf_clips(b"0 0 2\n2 2 3\n16 5 2\n", 7).unwrap();
        assert_eq!(actor_gltf_frame(&clips, ActorAnimationMode::Idle, 0.0), 2);
        assert_eq!(actor_gltf_frame(&clips, ActorAnimationMode::Idle, 0.061), 3);
        assert_eq!(actor_gltf_frame(&clips, ActorAnimationMode::Run, 0.0), 0);
        assert_eq!(actor_gltf_frame(&clips, ActorAnimationMode::Death, 5.0), 6);
        assert!(parse_gltf_clips(b"0 6 2\n", 7).is_err());
        assert!(parse_gltf_clips(b"0 0 1\n0 1 1\n", 7).is_err());
        assert!(intact_actor_primitive(
            "faust hand left",
            ActorAnimationMode::Idle
        ));
        assert!(!intact_actor_primitive(
            "faust hand left removed",
            ActorAnimationMode::Idle
        ));
        assert!(!intact_actor_primitive(
            "faust shooting transparent",
            ActorAnimationMode::Idle
        ));
        assert!(intact_actor_primitive(
            "faust shooting transparent",
            ActorAnimationMode::Attack
        ));
    }
    #[test]
    fn installed_faust_actor_is_visible_and_uses_original_animations() {
        let path = std::path::Path::new("assets/csm.bin");
        if !path.is_file() {
            return;
        }
        let mut resources = ResourceSet::open(path).unwrap();
        let map = crate::map::Map::parse(&resources.read("map/map.03").unwrap()).unwrap();
        let defs = ActorDefinitions::parse(&resources.read("chasm.inf").unwrap()).unwrap();
        let mut actors = ActorState::from_placements(&map.monsters, &defs);
        actors
            .actors
            .retain(|a| defs.entries[a.definition_index].model_name == "faust");
        assert!(!actors.actors.is_empty());
        let mut scene = ObjectScene {
            meshes: vec![],
            instances: vec![],
            palette: [0; 256],
        };
        scene.add_actors(&mut resources, &actors, &defs);
        assert_eq!(scene.meshes.len(), 1); // shared mesh
        assert_eq!(scene.instances.len(), actors.actors.len());
        let mesh = &scene.meshes[0];
        let model = mesh.gltf.as_ref().unwrap();
        assert_eq!(model.frame_count, 281);
        assert_eq!(
            mesh.gltf_actor_clips[2],
            Some(GltfClip {
                first: 15,
                count: 17
            })
        );
        assert_eq!(
            actor_gltf_frame(&mesh.gltf_actor_clips, ActorAnimationMode::Idle, 0.0),
            15
        );
        assert_eq!(
            actor_gltf_frame(&mesh.gltf_actor_clips, ActorAnimationMode::Run, 0.0),
            0
        );
        assert_eq!(
            actor_gltf_frame(&mesh.gltf_actor_clips, ActorAnimationMode::Death, 100.0),
            256
        );
        // Render one real actor in isolation and confirm the decoded texture is visible.
        let actor = &actors.actors[0];
        scene.instances.truncate(1);
        let camera = CameraView {
            player: Player {
                position: [actor.position[0] - 2.0, actor.position[1]],
                angle: 0.0,
            },
            eye_height: 1.0,
        };
        let mut frame = vec![0; 160 * 100];
        let mut depth = vec![f32::INFINITY; frame.len()];
        scene.render(&mut frame, &mut depth, 160, 100, camera);
        assert!(frame.iter().any(|&pixel| pixel != 0));
    }

    #[test]
    fn gltf_depth_alpha_and_per_instance_animation() {
        let mut scene = gltf_scene(255);
        let camera = CameraView {
            player: Player {
                position: [0.0, 0.0],
                angle: 0.0,
            },
            eye_height: 1.0,
        };
        let mut frame = vec![0; 64 * 64];
        let mut depth = vec![1.0; 64 * 64];
        scene.render(&mut frame, &mut depth, 64, 64, camera);
        assert!(frame.iter().all(|&p| p == 0));
        depth.fill(f32::INFINITY);
        scene.render(&mut frame, &mut depth, 64, 64, camera);
        assert!(frame.iter().any(|&p| p & 0xff0000 != 0));
        let base = frame.clone();
        assert_eq!(scene.play_object_animation(33), 1);
        assert!(scene.tick_object_animations(0.051));
        frame.fill(0);
        depth.fill(f32::INFINITY);
        scene.render(&mut frame, &mut depth, 64, 64, camera);
        assert_ne!(frame, base);
        let invisible = gltf_scene(0);
        frame.fill(0);
        depth.fill(f32::INFINITY);
        invisible.render(&mut frame, &mut depth, 64, 64, camera);
        assert!(frame.iter().all(|&p| p == 0));
        assert!(depth.iter().all(|&d| d == f32::INFINITY));
    }
    #[test]
    fn installed_modern_only_objects_load_and_render_when_available() {
        let path = std::path::Path::new("assets/csm.bin");
        if !path.is_file() {
            return;
        }
        let mut resources = ResourceSet::open(path).unwrap();
        let inf = resources.read("chasm.inf").unwrap();
        let palette = Palette::parse(&resources.read("chasm2.pal").unwrap()).unwrap();
        for (level, index, expected_name) in [("01", 17, "vent1"), ("10", 33, "bridge_2")] {
            let map = crate::map::Map::parse(&resources.read(&format!("map/map.{level}")).unwrap())
                .unwrap();
            let resource = resources.read(&format!("map/resource.{level}")).unwrap();
            let definitions = ObjectDefinitions::parse(&inf, &resource).unwrap();
            assert_eq!(definitions.level_objects[index].model_name, expected_name);
            let mut scene = ObjectScene::load(&mut resources, &map, &definitions, &palette);
            scene
                .instances
                .retain(|instance| instance.object_definition_index == Some(index));
            assert!(!scene.instances.is_empty());
            let instance = &scene.instances[0];
            assert!(scene.meshes[instance.mesh].gltf.is_some());
            let camera = CameraView {
                player: Player {
                    position: [instance.position[0] - 2.0, instance.position[1]],
                    angle: 0.0,
                },
                eye_height: 1.0,
            };
            let mut frame = vec![0; 160 * 100];
            let mut depth = vec![f32::INFINITY; frame.len()];
            scene.render(&mut frame, &mut depth, 160, 100, camera);
            assert!(
                frame.iter().any(|&p| p != 0),
                "{expected_name} produced no pixels"
            );
        }
    }

    #[test]
    fn playani_targets_all_level_objects_and_preserves_instance_state() {
        use std::time::{SystemTime, UNIX_EPOCH};
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("openchasm-playani-{}-{unique}", std::process::id()));
        std::fs::create_dir_all(dir.join("model")).unwrap();
        let mut model = vec![0_u8; 0x4806 + 64];
        for (i, index) in [0_u16, 1, 2, 255].iter().enumerate() {
            model[i * 2..i * 2 + 2].copy_from_slice(&index.to_le_bytes());
        }
        model[0x4800..0x4802].copy_from_slice(&3_u16.to_le_bytes());
        model[0x4802..0x4804].copy_from_slice(&1_u16.to_le_bytes());
        model[0x4804..0x4806].copy_from_slice(&1_u16.to_le_bytes());
        std::fs::write(dir.join("model/a.3o"), model).unwrap();
        let mut animation = vec![0_u8; 2 + 3 * 6 * 2];
        animation[..2].copy_from_slice(&3_u16.to_le_bytes());
        animation[2 + 3 * 6..4 + 3 * 6].copy_from_slice(&512_i16.to_le_bytes());
        std::fs::write(dir.join("model/a.ani"), animation).unwrap();
        let mut map_bytes = vec![0; 0x27001 + 4];
        for (x, y, id) in [(1, 1, 163), (2, 2, 163), (3, 3, 164)] {
            map_bytes[0x18001 + (x * 64 + y) * 11] = id;
        }
        let map = crate::map::Map::parse(&map_bytes).unwrap();
        let def = |animation_name| crate::objects::ObjectDefinition {
            collision_radius: 0,
            shadow: 0,
            bmp_object: 0,
            bmp_z: 0,
            action_code: 0,
            blow_up: 0,
            break_limit: 0,
            ambient_sound: 0,
            break_sound: 0,
            model_name: "a.3o".into(),
            animation_name,
        };
        let definitions = crate::objects::ObjectDefinitions {
            items: vec![],
            level_objects: vec![def(Some("%a.ani".into())), def(None)],
        };
        let palette = Palette::parse(&[0; 768]).unwrap();
        let mut resources = ResourceSet::open(&dir).unwrap();
        let mut scene = ObjectScene::load(&mut resources, &map, &definitions, &palette);
        assert_eq!(scene.instances.len(), 3);
        assert_eq!(scene.meshes.len(), 2); // same model, different animation association
        assert_eq!(scene.play_object_animation(1), 0); // unknown legacy special ID
        assert_eq!(scene.play_object_animation(34), 0); // no ANI
        assert_eq!(scene.play_object_animation(33), 2);
        assert!(scene.tick_object_animations(1.0 / 18.0 + 0.001));
        for instance in &scene.instances[..2] {
            assert!(instance.object_animation_playing);
            assert_eq!(
                object_vertices(
                    &scene.meshes[instance.mesh],
                    true,
                    instance.object_animation_seconds
                )[0]
                .position[0],
                512
            );
        }
        assert!(!scene.instances[2].object_animation_playing);
        assert!(!scene.tick_object_animations(f32::NAN));
        scene.reset_object_animations();
        assert!(scene
            .instances
            .iter()
            .all(|i| !i.object_animation_playing && i.object_animation_seconds == 0.0));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn actor_animation_selects_slots_loops_and_clamps_death() {
        let vertex = |x| ModelVertex {
            position: [x, 0, 0],
        };
        let mut animations: [Option<Animation>; 20] = std::array::from_fn(|_| None);
        let frames = |values: &[i16]| Animation {
            vertex_count: 1,
            frames: values.iter().map(|&v| vec![vertex(v)]).collect(),
        };
        animations[0] = Some(frames(&[0, 1, 2]));
        animations[2] = Some(frames(&[10, 11]));
        animations[16] = Some(frames(&[20, 21]));
        let mesh = Mesh {
            model: Model {
                vertices: vec![vertex(-1)],
                polygons: vec![],
                skin_height: 0,
                skin_indices: vec![],
            },
            radius: 1.0,
            car_uv: true,
            animations,
            gltf: None,
            gltf_actor_clips: [None; 20],
        };
        let x = |mode, time| actor_vertices(&mesh, mode, time)[0].position[0];
        assert_eq!(x(ActorAnimationMode::Idle, 0.0), 10);
        assert_eq!(x(ActorAnimationMode::Idle, 1.0 / ACTOR_ANIMATION_FPS), 11);
        assert_eq!(x(ActorAnimationMode::Run, 3.0 / ACTOR_ANIMATION_FPS), 0);
        assert_eq!(x(ActorAnimationMode::Attack, 0.0), 10); // idle fallback
        assert_eq!(x(ActorAnimationMode::Death, 100.0), 21); // hold last frame
    }
    #[test]
    fn actor_timers_are_independent_and_reset_on_mode_change() {
        let instance = |id| Instance {
            mesh: 0,
            position: [0.0, 0.0],
            angle: 0.0,
            elevation: 0.0,
            cell: None,
            actor_source_index: Some(id),
            object_definition_index: None,
            object_animation_playing: false,
            object_animation_initially_playing: false,
            object_animation_seconds: 0.0,
            animation_mode: ActorAnimationMode::Idle,
            animation_seconds: 0.0,
            visible: true,
        };
        let mut scene = ObjectScene {
            meshes: vec![],
            instances: vec![instance(7), instance(8)],
            palette: [0; 256],
        };
        assert!(scene.set_actor_animation(7, ActorAnimationMode::Idle, 0.25));
        assert_eq!(scene.instances[0].animation_seconds, 0.25);
        assert_eq!(scene.instances[1].animation_seconds, 0.0);
        assert!(scene.set_actor_animation(7, ActorAnimationMode::Run, 0.25));
        assert_eq!(scene.instances[0].animation_seconds, 0.0);
        assert!(scene.set_actor_animation(7, ActorAnimationMode::Run, 0.25));
        assert_eq!(scene.instances[0].animation_seconds, 0.25);
        assert!(!scene.set_actor_animation(7, ActorAnimationMode::Attack, f32::NAN));
        assert_eq!(scene.instances[0].animation_mode, ActorAnimationMode::Run);
        assert!(!scene.set_actor_animation(9, ActorAnimationMode::Attack, 0.0));
    }

    #[test]
    fn visibility_changes_one_placement_without_hiding_shared_mesh() {
        let mut scene = ObjectScene {
            meshes: Vec::new(),
            instances: vec![
                Instance {
                    mesh: 0,
                    position: [0.0, 0.0],
                    angle: 0.0,
                    elevation: 0.0,
                    cell: Some([1, 2]),
                    actor_source_index: None,
                    object_definition_index: None,
                    object_animation_playing: false,
                    object_animation_initially_playing: false,
                    object_animation_seconds: 0.0,
                    animation_mode: ActorAnimationMode::Idle,
                    animation_seconds: 0.0,
                    visible: true,
                },
                Instance {
                    mesh: 0,
                    position: [1.0, 1.0],
                    angle: 0.0,
                    elevation: 0.0,
                    cell: Some([3, 4]),
                    actor_source_index: None,
                    object_definition_index: None,
                    object_animation_playing: false,
                    object_animation_initially_playing: false,
                    object_animation_seconds: 0.0,
                    animation_mode: ActorAnimationMode::Idle,
                    animation_seconds: 0.0,
                    visible: true,
                },
            ],
            palette: [0; 256],
        };
        assert!(scene.set_visible([1, 2], false));
        assert!(!scene.instances[0].visible);
        assert!(scene.instances[1].visible);
        assert!(!scene.set_visible([9, 9], false));
        assert!(scene.set_visible([1, 2], true));
        assert!(scene.instances[0].visible);
    }

    #[test]
    fn car_uv_uses_fixed_point_coordinates_and_distinct_offset() {
        let polygon = crate::model::Polygon {
            vertices: [0, 1, 2, 255],
            uv: [[256, 512]; 4],
            next: 0,
            distant: 0,
            group: 0,
            flags: 0,
            sprite_offset: 64,
        };
        assert_eq!(texture_uv(&polygon, 0, true), [1.0, 3.0]);
        assert_eq!(texture_uv(&polygon, 0, false), [256.0, 576.0]);
    }

    #[test]
    fn loads_shared_car_mesh_using_first_pose_and_updates_one_actor() {
        use std::time::{SystemTime, UNIX_EPOCH};
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "openchasm-actor-render-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(dir.join("monster")).unwrap();
        let mut car = vec![0_u8; 0x66];
        car[..2].copy_from_slice(&18_u16.to_le_bytes());
        let mut body = vec![0_u8; 0x4806 + 64];
        body[0x4800..0x4802].copy_from_slice(&3_u16.to_le_bytes());
        body[0x4802..0x4804].copy_from_slice(&1_u16.to_le_bytes());
        body[0x4804..0x4806].copy_from_slice(&64_u16.to_le_bytes());
        for (i, index) in [0_u16, 1, 2, 255].iter().enumerate() {
            body[i * 2..i * 2 + 2].copy_from_slice(&index.to_le_bytes());
        }
        body[0x4806..].fill(1);
        car.extend(body);
        let mut animation = [0_u8; 18];
        animation[2..4].copy_from_slice(&(-512_i16).to_le_bytes());
        animation[8..10].copy_from_slice(&512_i16.to_le_bytes());
        animation[16..18].copy_from_slice(&2048_i16.to_le_bytes());
        car.extend(animation);
        std::fs::write(dir.join("monster/joker.car"), car).unwrap();
        let mut resources = ResourceSet::open(&dir).unwrap();
        let definitions = ActorDefinitions::parse(
            b"[MONSTERS]\n2\nhero.car 1 1 1 1 100 0 0 0\njoker.car 1 1 1 1 100 0 0 0\n",
        )
        .unwrap();
        let map_actor = |x| crate::map::Monster {
            position: [x, 256],
            id: 101,
            angle: 0,
            flags: 0,
        };
        let actors = ActorState::from_placements(&[map_actor(256), map_actor(512)], &definitions);
        let mut scene = ObjectScene {
            meshes: vec![],
            instances: vec![],
            palette: [0; 256],
        };
        scene.add_actors(&mut resources, &actors, &definitions);
        assert_eq!(scene.meshes.len(), 1);
        assert_eq!(scene.instances.len(), 2);
        assert!(scene.meshes[0].car_uv);
        assert_eq!(
            actor_vertices(&scene.meshes[0], ActorAnimationMode::Idle, 0.0)[0].position[1],
            -512
        );
        scene.palette[1] = 0xffffff;
        let mut frame = vec![0; 64 * 64];
        let mut depth = vec![f32::INFINITY; 64 * 64];
        scene.render(
            &mut frame,
            &mut depth,
            64,
            64,
            CameraView {
                player: Player {
                    position: [0.0, 1.0],
                    angle: 0.0,
                },
                eye_height: 1.0,
            },
        );
        assert!(frame.iter().any(|&pixel| pixel != 0));
        let top = |pixels: &[u32]| {
            (0..64)
                .find(|&y| pixels[y * 64..(y + 1) * 64].iter().any(|&p| p != 0))
                .unwrap()
        };
        let low_eye_top = top(&frame);
        frame.fill(0);
        depth.fill(f32::INFINITY);
        scene.render(
            &mut frame,
            &mut depth,
            64,
            64,
            CameraView {
                player: Player {
                    position: [0.0, 1.0],
                    angle: 0.0,
                },
                eye_height: 1.2,
            },
        );
        assert!(top(&frame) > low_eye_top + 2);
        assert!(scene.set_actor_pose(0, [3.0, 4.0], 1.0, false));
        assert!(!scene.instances[0].visible);
        assert!(scene.instances[1].visible);
        assert_eq!(scene.instances[0].position, [3.0, 4.0]);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn clips_polygon_crossing_near_plane() {
        let vertex = |depth| Vertex {
            side: 0.0,
            up: 0.0,
            depth,
            uv: [depth, 0.0],
        };
        let out = clip_near(&[vertex(-1.0), vertex(1.0), vertex(2.0)]);
        assert_eq!(out.len(), 4);
        assert!(out.iter().all(|v| v.depth >= NEAR - 0.00001));
        assert!(
            out.iter()
                .filter(|v| (v.depth - NEAR).abs() < 0.00001)
                .count()
                == 2
        );
    }
    #[test]
    fn object_triangles_respect_wall_depth_and_masked_texels() {
        let mut frame = vec![0x123456; 16];
        let mut depth = vec![2.0; 16];
        let mut palette = [0; 256];
        palette[1] = 0xff0000;
        let mut model = Model {
            vertices: vec![],
            polygons: vec![],
            skin_height: 1,
            skin_indices: vec![1; SKIN_WIDTH],
        };
        let make = |x: f32, y: f32, z: f32| ScreenVertex {
            x,
            y,
            inv_depth: 1.0 / z,
            u_over_depth: 0.0,
            v_over_depth: 0.0,
        };
        let triangle = [
            make(0.0, 0.0, 3.0),
            make(4.0, 0.0, 3.0),
            make(0.0, 4.0, 3.0),
        ];
        let mut raster = Raster {
            frame: &mut frame,
            depth: &mut depth,
            width: 4,
            height: 4,
            palette: &palette,
            translucent: None,
        };
        raster.triangle(triangle, &model, 0);
        assert_eq!(raster.frame[0], 0x123456);
        let triangle = [
            make(0.0, 0.0, 1.0),
            make(4.0, 0.0, 1.0),
            make(0.0, 4.0, 1.0),
        ];
        model.skin_indices.fill(255);
        raster.triangle(triangle, &model, 2);
        assert_eq!(raster.frame[0], 0x123456);
        model.skin_indices.fill(1);
        raster.triangle(triangle, &model, 0);
        assert_ne!(raster.frame[0], 0x123456);
        assert_eq!(raster.depth[0], 1.0);
    }
    #[test]
    fn translucent_fragments_sort_per_pixel_and_respect_final_opaque_depth() {
        let vertex = |x, y, depth: f32| ScreenVertex {
            x,
            y,
            inv_depth: depth.recip(),
            u_over_depth: 0.0,
            v_over_depth: 0.0,
        };
        let tri = |a, b, c| {
            [
                vertex(0.0, 0.0, a),
                vertex(4.0, 0.0, b),
                vertex(0.0, 4.0, c),
            ]
        };
        let red = GltfImage {
            width: 1,
            height: 1,
            rgba: vec![255, 0, 0, 128],
        };
        let blue = GltfImage {
            width: 1,
            height: 1,
            rgba: vec![0, 0, 255, 128],
        };
        let white = GltfImage {
            width: 1,
            height: 1,
            rgba: vec![255, 255, 255, 255],
        };
        let palette = [0; 256];
        let render = |reverse: bool, opaque: bool| {
            let mut frame = vec![0; 16];
            let mut depth = vec![10.0; 16];
            let mut raster = Raster {
                frame: &mut frame,
                depth: &mut depth,
                width: 4,
                height: 4,
                palette: &palette,
                translucent: None,
            };
            // The two surfaces cross: their order reverses across the triangle.
            let draws = [(tri(1.0, 4.0, 1.0), &red), (tri(4.0, 1.0, 4.0), &blue)];
            for i in if reverse { [1, 0] } else { [0, 1] } {
                raster.gltf_triangle(draws[i].0, Some(draws[i].1));
            }
            if opaque {
                raster.gltf_triangle(tri(1.1, 1.1, 1.1), Some(&white));
            }
            raster.finish();
            (frame, depth)
        };
        let (a, depth) = render(false, false);
        let (b, _) = render(true, false);
        assert_eq!(a, b);
        assert_ne!(a[0], a[2]);
        assert_eq!(depth[0], 10.0);
        let (a, depth) = render(false, true);
        let (b, _) = render(true, true);
        assert_eq!(a, b);
        assert!((depth[0] - 1.1).abs() < 0.0001);
        assert_ne!(a[0], 0);
    }

    #[test]
    fn translucent_layers_are_bounded_and_keep_nearest() {
        let mut fragments = Translucency::new(1);
        for i in (1..=12).rev() {
            fragments.push(0, i as f32, i as u32, 128);
        }
        assert_eq!(fragments.fragments.len(), MAX_TRANSLUCENT_LAYERS as usize);
        assert!(fragments.fragments.iter().all(|f| f.depth <= 8.0));
    }
    #[test]
    fn translucent_samples_do_not_hide_later_opaque_objects() {
        let palette = [0; 256];
        let mut frame = [0];
        let mut depth = [10.0];
        let mut raster = Raster {
            frame: &mut frame,
            depth: &mut depth,
            width: 1,
            height: 1,
            palette: &palette,
            translucent: None,
        };
        raster.put(0, 3.0, 0xff0000, 128);
        raster.put(0, 1.0, 0x0000ff, 255);
        raster.finish();
        assert_eq!(raster.frame[0], 0x0000ff);
        assert_eq!(raster.depth[0], 1.0);
    }
}
