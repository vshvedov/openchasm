//! Presentation of projectile 3O models over the shared world depth buffer.
//!
//! This module does not create or advance projectiles, apply damage, or know
//! who owns them. The simulation provides positions and orientation per frame.
use crate::content::ResourceSet;
use crate::object_render::CameraView;
use crate::texture::Palette;
use openchasm::combat::RocketDefinitions;
use openchasm::model::{Model, SKIN_WIDTH};
use std::collections::{BTreeSet, HashMap};

const MODEL_SCALE: f32 = 1.0 / 2048.0;
const CAMERA_PLANE: f32 = 0.66;
const NEAR: f32 = 0.05;

/// A transient render snapshot supplied by the simulation. Coordinates are map
/// tiles; yaw and pitch are radians. Position, pose, and ownership remain the
/// simulation's responsibility.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ProjectileVisual {
    pub rocket_type: usize,
    pub position: [f32; 3],
    pub yaw: f32,
    pub pitch: f32,
}

pub struct ProjectileRenderer {
    /// Indexed by RocketDefinitions entry; empty instant shots have no model.
    rocket_models: Vec<Option<usize>>,
    models: Vec<Model>,
    palette: [u32; 256],
}

fn model_path(name: &str) -> Option<String> {
    let name = name.trim();
    if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\\']) {
        return None;
    }
    Some(format!("model/{}", name.to_ascii_lowercase()))
}

impl ProjectileRenderer {
    /// Load 3O meshes once per distinct path. Invalid or missing entries are
    /// skipped with a diagnostic; blank names identify instant shots.
    pub fn load(
        resources: &mut ResourceSet,
        rockets: &RocketDefinitions,
        palette: &Palette,
    ) -> Self {
        let mut models = Vec::new();
        let mut rocket_models = Vec::with_capacity(rockets.0.len());
        let mut cache: HashMap<String, Option<usize>> = HashMap::new();
        let mut warnings = BTreeSet::new();
        for definition in &rockets.0 {
            let Some(path) = model_path(&definition.model_name) else {
                if !definition.model_name.trim().is_empty() {
                    warnings.insert(format!("invalid model name {:?}", definition.model_name));
                }
                rocket_models.push(None);
                continue;
            };
            let model = *cache.entry(path.clone()).or_insert_with(|| {
                match resources.read(&path).and_then(|data| Model::parse(&data)) {
                    Ok(model) => {
                        let index = models.len();
                        models.push(model);
                        Some(index)
                    }
                    Err(error) => {
                        warnings.insert(format!("{path}: {error}"));
                        None
                    }
                }
            });
            rocket_models.push(model);
        }
        for warning in warnings {
            eprintln!("projectile preview: {warning}");
        }
        let palette = std::array::from_fn(|index| {
            let [r, g, b] = palette.rgb8(index as u8);
            (u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b)
        });
        Self {
            rocket_models,
            models,
            palette,
        }
    }

    /// Render snapshots after walls and models, and before the HUD/weapon, with
    /// the same depth array so both world geometry and projectiles occlude them.
    pub fn render(
        &self,
        frame: &mut [u32],
        depth: &mut [f32],
        width: usize,
        height: usize,
        camera: CameraView,
        projectiles: &[ProjectileVisual],
    ) {
        let player = camera.player;
        if !camera.eye_height.is_finite()
            || width == 0
            || height == 0
            || width.checked_mul(height) != Some(frame.len())
            || frame.len() != depth.len()
            || !player.position.iter().all(|x| x.is_finite())
            || !player.angle.is_finite()
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
        };
        for projectile in projectiles {
            if !projectile.position.iter().all(|v| v.is_finite())
                || !projectile.yaw.is_finite()
                || !projectile.pitch.is_finite()
            {
                continue;
            }
            let Some(mesh) = self
                .rocket_models
                .get(projectile.rocket_type)
                .and_then(|id| *id)
                .and_then(|i| self.models.get(i))
            else {
                continue;
            };
            let (yaw_sin, yaw_cos) = (projectile.yaw - std::f32::consts::FRAC_PI_2).sin_cos();
            let (pitch_sin, pitch_cos) = projectile.pitch.sin_cos();
            for polygon in &mesh.polygons {
                let mut vertices = Vec::with_capacity(polygon.vertex_count());
                for corner in 0..polygon.vertex_count() {
                    let [x, y, z] = mesh.vertices[polygon.vertices[corner] as usize]
                        .position
                        .map(|p| f32::from(p) * MODEL_SCALE);
                    // Source rocket meshes face +Y. Rotate pitch about local X,
                    // then orient +Y to the supplied horizontal yaw.
                    let py = y * pitch_cos - z * pitch_sin;
                    let pz = y * pitch_sin + z * pitch_cos;
                    let wx =
                        projectile.position[0] + x * yaw_cos - py * yaw_sin - player.position[0];
                    let wy =
                        projectile.position[1] + x * yaw_sin + py * yaw_cos - player.position[1];
                    vertices.push(Vertex {
                        side: -wx * camera_sin + wy * camera_cos,
                        depth: wx * camera_cos + wy * camera_sin,
                        up: projectile.position[2] + pz - camera.eye_height,
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
                let first = project(clipped[0], width, height, camera.horizon(height));
                for i in 1..clipped.len() - 1 {
                    raster.triangle(
                        [
                            first,
                            project(clipped[i], width, height, camera.horizon(height)),
                            project(clipped[i + 1], width, height, camera.horizon(height)),
                        ],
                        mesh,
                        polygon.flags,
                    );
                }
            }
        }
    }
}

#[derive(Clone, Copy)]
struct Vertex {
    side: f32,
    depth: f32,
    up: f32,
    uv: [f32; 2],
}
impl Vertex {
    fn lerp(self, next: Self, t: f32) -> Self {
        let mix = |a, b| a + t * (b - a);
        Self {
            side: mix(self.side, next.side),
            depth: mix(self.depth, next.depth),
            up: mix(self.up, next.up),
            uv: [mix(self.uv[0], next.uv[0]), mix(self.uv[1], next.uv[1])],
        }
    }
}
fn clip_near(input: &[Vertex]) -> Vec<Vertex> {
    if input.is_empty() {
        return Vec::new();
    }
    let mut output = Vec::with_capacity(input.len() + 1);
    let mut prior = input[input.len() - 1];
    for &current in input {
        if (prior.depth >= NEAR) != (current.depth >= NEAR) {
            output.push(prior.lerp(
                current,
                (NEAR - prior.depth) / (current.depth - prior.depth),
            ));
        }
        if current.depth >= NEAR {
            output.push(current);
        }
        prior = current;
    }
    output
}
#[derive(Clone, Copy)]
struct Projected {
    x: f32,
    y: f32,
    inverse: f32,
    u_inverse: f32,
    v_inverse: f32,
}
fn project(vertex: Vertex, width: usize, height: usize, horizon: f32) -> Projected {
    let inverse = vertex.depth.recip();
    Projected {
        x: width as f32 * 0.5 * (1.0 + vertex.side * inverse / CAMERA_PLANE),
        y: horizon - height as f32 * 0.5 * vertex.up * inverse,
        inverse,
        u_inverse: vertex.uv[0] * inverse,
        v_inverse: vertex.uv[1] * inverse,
    }
}
fn edge(a: Projected, b: Projected, x: f32, y: f32) -> f32 {
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
    fn triangle(&mut self, triangle: [Projected; 3], model: &Model, flags: u8) {
        let [a, b, c] = triangle;
        let area = edge(a, b, c.x, c.y);
        if area.abs() < 0.00001 || model.skin_height == 0 {
            return;
        }
        let x0 = a.x.min(b.x).min(c.x).floor().max(0.0) as usize;
        let x1 = a.x.max(b.x).max(c.x).ceil().min(self.width as f32) as usize;
        let y0 = a.y.min(b.y).min(c.y).floor().max(0.0) as usize;
        let y1 = a.y.max(b.y).max(c.y).ceil().min(self.height as f32) as usize;
        for y in y0..y1 {
            for x in x0..x1 {
                let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
                let w0 = edge(b, c, px, py) / area;
                let w1 = edge(c, a, px, py) / area;
                let w2 = 1.0 - w0 - w1;
                if w0 < -0.00001 || w1 < -0.00001 || w2 < -0.00001 {
                    continue;
                }
                let inverse = w0 * a.inverse + w1 * b.inverse + w2 * c.inverse;
                if inverse <= 0.0 {
                    continue;
                }
                let distance = inverse.recip();
                let pixel = y * self.width + x;
                if distance >= self.depth[pixel] {
                    continue;
                }
                let u = (w0 * a.u_inverse + w1 * b.u_inverse + w2 * c.u_inverse) * distance;
                let v = (w0 * a.v_inverse + w1 * b.v_inverse + w2 * c.v_inverse) * distance;
                let tx = (u.floor() as i32).rem_euclid(SKIN_WIDTH as i32) as usize;
                let ty = (v.floor() as i32).rem_euclid(i32::from(model.skin_height)) as usize;
                let index = model.skin_indices[ty * SKIN_WIDTH + tx];
                if flags & 0x0e != 0 && index == 255 {
                    continue;
                }
                let rgb = self.palette[index as usize];
                let attenuation = (1.0 / (1.0 + distance * 0.08)).clamp(0.25, 1.0);
                let channel = |c: u32| (c as f32 * attenuation) as u32;
                self.frame[pixel] = (channel((rgb >> 16) & 255) << 16)
                    | (channel((rgb >> 8) & 255) << 8)
                    | channel(rgb & 255);
                self.depth[pixel] = distance;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pitched_projectile_uses_shared_world_horizon() {
        let point = Vertex {
            side: 0.0,
            up: 2.0,
            depth: 4.0,
            uv: [0.0, 0.0],
        };
        let camera = CameraView {
            player: crate::world::Player {
                position: [0.0, 0.0],
                angle: 0.0,
            },
            eye_height: 1.0,
            pitch: 0.5_f32.atan(),
        };
        let result = project(point, 640, 400, camera.horizon(400));
        assert!((result.y - 200.0).abs() < 0.001);
        assert_eq!(result.inverse, 0.25);
    }
    use openchasm::model::{Polygon, Vertex as ModelVertex};
    use openchasm::world::Player;

    fn renderer() -> ProjectileRenderer {
        let vertex = |y, z| ModelVertex {
            position: [0, y, z],
        };
        let model = Model {
            vertices: vec![vertex(-512, -512), vertex(512, -512), vertex(0, 512)],
            polygons: vec![Polygon {
                vertices: [0, 1, 2, 255],
                uv: [[0; 2]; 4],
                next: 0,
                distant: 0,
                group: 0,
                flags: 0,
                sprite_offset: 0,
            }],
            skin_height: 1,
            skin_indices: vec![1; SKIN_WIDTH],
        };
        let mut palette = [0; 256];
        palette[1] = 0xff0000;
        ProjectileRenderer {
            rocket_models: vec![Some(0), None],
            models: vec![model],
            palette,
        }
    }
    #[test]
    fn projectile_uses_world_depth_and_ignores_instant_or_invalid_types() {
        let renderer = renderer();
        let mut frame = vec![0; 64 * 64];
        let mut depth = vec![0.5; 64 * 64];
        let player = Player {
            position: [0.0, 0.0],
            angle: 0.0,
        };
        let mut shot = ProjectileVisual {
            rocket_type: 0,
            position: [1.0, 0.0, 1.0],
            yaw: std::f32::consts::FRAC_PI_2,
            pitch: 0.0,
        };
        renderer.render(
            &mut frame,
            &mut depth,
            64,
            64,
            CameraView {
                player,
                eye_height: 1.0,
                pitch: 0.0,
            },
            &[shot],
        );
        assert!(frame.iter().all(|&pixel| pixel == 0));
        depth.fill(f32::INFINITY);
        renderer.render(
            &mut frame,
            &mut depth,
            64,
            64,
            CameraView {
                player,
                eye_height: 1.0,
                pitch: 0.0,
            },
            &[shot],
        );
        assert!(frame.iter().any(|&pixel| pixel != 0));
        frame.fill(0);
        depth.fill(f32::INFINITY);
        shot.rocket_type = 1;
        renderer.render(
            &mut frame,
            &mut depth,
            64,
            64,
            CameraView {
                player,
                eye_height: 1.0,
                pitch: 0.0,
            },
            &[shot],
        );
        shot.rocket_type = usize::MAX;
        renderer.render(
            &mut frame,
            &mut depth,
            64,
            64,
            CameraView {
                player,
                eye_height: 1.0,
                pitch: 0.0,
            },
            &[shot],
        );
        assert!(frame.iter().all(|&pixel| pixel == 0));
    }
    #[test]
    fn rising_camera_moves_projectile_projection_down() {
        let renderer = renderer();
        let player = Player {
            position: [0.0, 0.0],
            angle: 0.0,
        };
        let shot = ProjectileVisual {
            rocket_type: 0,
            position: [1.0, 0.0, 1.0],
            yaw: std::f32::consts::FRAC_PI_2,
            pitch: 0.0,
        };
        let mut frame = vec![0; 64 * 64];
        let mut depth = vec![f32::INFINITY; 64 * 64];
        let top = |frame: &[u32]| {
            (0..64)
                .find(|&y| frame[y * 64..(y + 1) * 64].iter().any(|&pixel| pixel != 0))
                .unwrap()
        };
        renderer.render(
            &mut frame,
            &mut depth,
            64,
            64,
            CameraView {
                player,
                eye_height: 0.75,
                pitch: 0.0,
            },
            &[shot],
        );
        let lower_top = top(&frame);
        frame.fill(0);
        depth.fill(f32::INFINITY);
        renderer.render(
            &mut frame,
            &mut depth,
            64,
            64,
            CameraView {
                player,
                eye_height: 1.15,
                pitch: 0.0,
            },
            &[shot],
        );
        assert!(top(&frame) > lower_top + 5);
    }
    #[test]
    fn skips_invalid_coordinates_and_rejects_unsafe_paths() {
        assert_eq!(model_path("Arrow.3O").as_deref(), Some("model/arrow.3o"));
        for name in ["", ".", "..", "../evil", "dir\\evil"] {
            assert!(model_path(name).is_none());
        }
        let mut frame = vec![0; 16];
        let mut depth = vec![f32::INFINITY; 16];
        let shot = ProjectileVisual {
            rocket_type: 0,
            position: [f32::NAN, 0.0, 0.0],
            yaw: 0.0,
            pitch: 0.0,
        };
        renderer().render(
            &mut frame,
            &mut depth,
            4,
            4,
            CameraView {
                player: Player {
                    position: [0.0, 0.0],
                    angle: 0.0,
                },
                eye_height: 1.0,
                pitch: 0.0,
            },
            &[shot],
        );
        assert_eq!(frame, vec![0; 16]);
    }
    #[test]
    fn clips_at_near_plane_before_projection() {
        let vertex = |depth| Vertex {
            side: 0.0,
            depth,
            up: 0.0,
            uv: [depth, 0.0],
        };
        let result = clip_near(&[vertex(-1.0), vertex(1.0), vertex(2.0)]);
        assert_eq!(result.len(), 4);
        assert!(result.iter().all(|v| v.depth >= NEAR - 1e-5));
    }
}
