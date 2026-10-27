//! CPU renderer for original 3O map objects.
//!
//! The viewer's camera is intentionally shared with its wall projection. Meshes
//! and instances are immutable here; gameplay movement and animation are not
//! simulated by this module.
use crate::content::ResourceSet;
use crate::model::{Model, SKIN_WIDTH};
use crate::objects::{self, ObjectDefinitions};
use crate::texture::Palette;
use crate::world::Player;
use std::collections::{BTreeSet, HashMap};

const MODEL_SCALE: f32 = 1.0 / 2048.0;
const NEAR: f32 = 0.05;
const CAMERA_HEIGHT: f32 = 1.0;
const CAMERA_PLANE: f32 = 0.66;

struct Mesh {
    model: Model,
    radius: f32,
}

struct Instance {
    mesh: usize,
    position: [f32; 2],
    angle: f32,
    cell: [usize; 2],
    visible: bool,
}

/// A deduplicated collection of static object meshes and placements.
pub struct ObjectScene {
    meshes: Vec<Mesh>,
    instances: Vec<Instance>,
    palette: [u32; 256],
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

struct Raster<'a> {
    frame: &'a mut [u32],
    depth: &'a mut [f32],
    width: usize,
    height: usize,
    palette: &'a [u32; 256],
}
impl Raster<'_> {
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
                self.frame[offset] = (channel((rgb >> 16) & 255) << 16)
                    | (channel((rgb >> 8) & 255) << 8)
                    | channel(rgb & 255);
                self.depth[offset] = distance;
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
        let mut cache: HashMap<String, Option<usize>> = HashMap::new();
        let mut warnings = BTreeSet::new();
        for instance in objects::instances(map) {
            let Some(definition) = definitions.definition(&instance) else {
                warnings.insert(format!(
                    "missing {:?} definition {}",
                    instance.kind, instance.definition_index
                ));
                continue;
            };
            let Some(path) = definition.model_3o_path() else {
                warnings.insert(format!("glTF-only model {}", definition.model_name));
                continue;
            };
            let mesh_index = *cache.entry(path.clone()).or_insert_with(|| {
                match resources.read(&path).and_then(|bytes| Model::parse(&bytes)) {
                    Ok(model) => {
                        let radius = model
                            .vertices
                            .iter()
                            .map(|vertex| {
                                let x = f32::from(vertex.position[0]) * MODEL_SCALE;
                                let y = f32::from(vertex.position[1]) * MODEL_SCALE;
                                (x * x + y * y).sqrt()
                            })
                            .fold(0.0_f32, f32::max);
                        let index = meshes.len();
                        meshes.push(Mesh { model, radius });
                        Some(index)
                    }
                    Err(error) => {
                        warnings.insert(format!("{path}: {error}"));
                        None
                    }
                }
            });
            if let Some(mesh) = mesh_index {
                instances.push(Instance {
                    mesh,
                    position: instance.position,
                    angle: instance.angle_radians,
                    cell: instance.grid_cell,
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

    /// Hide or restore an individual placement without changing shared meshes.
    /// Grid cells are stable identifiers from the source map.
    #[allow(dead_code)] // Used when pickup/game state integration is enabled.
    pub fn set_visible(&mut self, cell: [usize; 2], visible: bool) -> bool {
        if let Some(instance) = self.instances.iter_mut().find(|item| item.cell == cell) {
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
        player: Player,
    ) {
        if width.checked_mul(height) != Some(frame.len()) || frame.len() != depth.len() {
            return;
        }
        let (camera_sin, camera_cos) = player.angle.sin_cos();
        let mut raster = Raster {
            frame,
            depth,
            width,
            height,
            palette: &self.palette,
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
            let (sin, cos) = instance.angle.sin_cos();
            for polygon in &mesh.model.polygons {
                let count = polygon.vertex_count();
                let mut vertices = Vec::with_capacity(count);
                for corner in 0..count {
                    let p = mesh.model.vertices[polygon.vertices[corner] as usize].position;
                    let x = f32::from(p[0]) * MODEL_SCALE;
                    let y = f32::from(p[1]) * MODEL_SCALE;
                    let world_x = dx + x * cos - y * sin;
                    let world_y = dy + x * sin + y * cos;
                    vertices.push(Vertex {
                        side: -world_x * camera_sin + world_y * camera_cos,
                        up: f32::from(p[2]) * MODEL_SCALE - CAMERA_HEIGHT,
                        depth: world_x * camera_cos + world_y * camera_sin,
                        uv: [
                            f32::from(polygon.uv[corner][0]),
                            f32::from(polygon.uv[corner][1]) + f32::from(polygon.sprite_offset),
                        ],
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn visibility_changes_one_placement_without_hiding_shared_mesh() {
        let mut scene = ObjectScene {
            meshes: Vec::new(),
            instances: vec![
                Instance {
                    mesh: 0,
                    position: [0.0, 0.0],
                    angle: 0.0,
                    cell: [1, 2],
                    visible: true,
                },
                Instance {
                    mesh: 0,
                    position: [1.0, 1.0],
                    angle: 0.0,
                    cell: [3, 4],
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
}
