//! Presentation of projectile 3O models over the shared world depth buffer.
//!
//! This module does not create or advance projectiles, apply damage, or know
//! who owns them. The simulation provides positions and orientation per frame.
use crate::content::ResourceSet;
use crate::object_render::CameraView;
use crate::texture::Palette;
use openchasm::combat::RocketDefinitions;
use openchasm::model::{Animation, Model, Vertex as ModelVertex, SKIN_WIDTH};
use std::collections::{BTreeSet, HashMap};

const MODEL_SCALE: f32 = 1.0 / 2048.0;
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
    /// Time since launch, in simulation seconds (unchanged by reflection).
    pub elapsed_seconds: f32,
}

struct RocketMesh {
    model: Model,
    animation: Option<Animation>,
}
impl RocketMesh {
    fn vertices(&self, elapsed_seconds: f32) -> &[ModelVertex] {
        let Some(animation) = self.animation.as_ref().filter(|a| !a.frames.is_empty()) else {
            return &self.model.vertices;
        };
        let time = if elapsed_seconds.is_finite() {
            elapsed_seconds.max(0.0) as f64
        } else {
            0.0
        };
        let frame = (time * 18.0)
            .round()
            .rem_euclid(animation.frames.len() as f64) as usize;
        &animation.frames[frame]
    }
}

pub struct ProjectileRenderer {
    /// Indexed by RocketDefinitions entry; empty instant shots have no model.
    rocket_models: Vec<Option<usize>>,
    fullbright: Vec<bool>,
    models: Vec<RocketMesh>,
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
        let mut cache: HashMap<(String, Option<String>), Option<usize>> = HashMap::new();
        let mut warnings = BTreeSet::new();
        for definition in &rockets.0 {
            let Some(path) = model_path(&definition.model_name) else {
                if !definition.model_name.trim().is_empty() {
                    warnings.insert(format!("invalid model name {:?}", definition.model_name));
                }
                rocket_models.push(None);
                continue;
            };
            let animation_path = if definition.animation_name.trim().is_empty() {
                None
            } else {
                let path = model_path(&definition.animation_name);
                if path.is_none() {
                    warnings.insert(format!(
                        "invalid projectile animation name {:?}",
                        definition.animation_name
                    ));
                }
                path
            };
            let model = *cache
                .entry((path.clone(), animation_path.clone()))
                .or_insert_with(|| {
                    match resources.read(&path).and_then(|data| Model::parse(&data)) {
                        Ok(model) => {
                            let animation =
                                animation_path.as_ref().and_then(|path| {
                                    match resources
                                        .read(path)
                                        .and_then(|bytes| Animation::parse(&bytes))
                                    {
                                        Ok(animation) if animation.matches_model(&model) => {
                                            Some(animation)
                                        }
                                        Ok(_) => {
                                            warnings
                                                .insert(format!("{path}: vertex count mismatch"));
                                            None
                                        }
                                        Err(error) => {
                                            warnings.insert(format!("{path}: {error}"));
                                            None
                                        }
                                    }
                                });
                            let index = models.len();
                            models.push(RocketMesh { model, animation });
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
            fullbright: rockets
                .0
                .iter()
                .map(|r| r.raw_fields.get(6).copied().unwrap_or(0) != 0)
                .collect(),
            models,
            palette,
        }
    }

    /// Render snapshots after walls and models, and before the HUD/weapon, with
    /// the same depth array so both world geometry and projectiles occlude them.
    #[cfg(test)]
    pub fn render(
        &self,
        frame: &mut [u32],
        depth: &mut [f32],
        width: usize,
        height: usize,
        camera: CameraView,
        projectiles: &[ProjectileVisual],
    ) {
        self.render_with_static_light(frame, depth, width, height, camera, projectiles, None);
    }

    /// Sample authored light per projectile; fullbright definitions bypass it.
    #[allow(clippy::too_many_arguments)]
    pub fn render_with_static_light(
        &self,
        frame: &mut [u32],
        depth: &mut [f32],
        width: usize,
        height: usize,
        camera: CameraView,
        projectiles: &[ProjectileVisual],
        static_light: Option<&crate::map::StaticLightMap>,
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
            static_light: None,
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
            raster.static_light = static_light
                .and_then(|light| light.sample([projectile.position[0], projectile.position[1]]));
            let (yaw_sin, yaw_cos) = (projectile.yaw - std::f32::consts::FRAC_PI_2).sin_cos();
            let (pitch_sin, pitch_cos) = projectile.pitch.sin_cos();
            let pose = mesh.vertices(projectile.elapsed_seconds);
            for polygon in &mesh.model.polygons {
                let mut vertices = Vec::with_capacity(polygon.vertex_count());
                for corner in 0..polygon.vertex_count() {
                    let [x, y, z] = pose[polygon.vertices[corner] as usize]
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
                        &mesh.model,
                        polygon.flags,
                        self.fullbright
                            .get(projectile.rocket_type)
                            .copied()
                            .unwrap_or(false),
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
        x: CameraView::project_x(width, height, vertex.side * inverse),
        y: horizon - CameraView::focal_length(height) * vertex.up * inverse,
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
    static_light: Option<f32>,
}
impl Raster<'_> {
    fn triangle(&mut self, triangle: [Projected; 3], model: &Model, flags: u8, fullbright: bool) {
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
                let attenuation = if fullbright {
                    1.0
                } else {
                    self.static_light
                        .unwrap_or_else(|| (1.0 / (1.0 + distance * 0.08)).clamp(0.25, 1.0))
                };
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

    #[test]
    fn projectile_animations_follow_launch_age_and_cache_animation_identity() {
        let path =
            std::env::temp_dir().join(format!("openchasm-rocket-anim-{}", std::process::id()));
        std::fs::create_dir_all(path.join("model")).unwrap();
        let mut model = vec![0u8; 0x4806 + 64];
        for (i, value) in [0u16, 1, 2, 255].iter().enumerate() {
            model[i * 2..i * 2 + 2].copy_from_slice(&value.to_le_bytes());
        }
        model[0x4800..0x4802].copy_from_slice(&3u16.to_le_bytes());
        model[0x4802..0x4804].copy_from_slice(&1u16.to_le_bytes());
        model[0x4804..0x4806].copy_from_slice(&1u16.to_le_bytes());
        std::fs::write(path.join("model/a.3o"), model).unwrap();
        let mut animation = vec![0u8; 2 + 3 * 6 * 2];
        animation[..2].copy_from_slice(&3u16.to_le_bytes());
        animation[20..22].copy_from_slice(&512i16.to_le_bytes());
        std::fs::write(path.join("model/a.ani"), animation).unwrap();
        let info = b"[ROCKETS]\n3\n3d_MODEL=a.3o\nANIMATION=a.ani\n1 0 0 1 1\n3d_MODEL=a.3o\nANIMATION=missing.ani\n1 0 0 1 1\n3d_MODEL=a.3o\nANIMATION=../bad.ani\n1 0 0 1 1\n";
        let rockets = RocketDefinitions::parse(info).unwrap();
        let mut resources = ResourceSet::open(&path).unwrap();
        let palette = Palette::parse(&[0; 768]).unwrap();
        let renderer = ProjectileRenderer::load(&mut resources, &rockets, &palette);
        let animated = &renderer.models[renderer.rocket_models[0].unwrap()];
        let static_fallback = &renderer.models[renderer.rocket_models[1].unwrap()];
        assert_ne!(renderer.rocket_models[0], renderer.rocket_models[1]);
        assert_eq!(animated.vertices(0.0)[0].position[0], 0);
        assert_eq!(animated.vertices(1.0 / 18.0)[0].position[0], 512);
        assert_eq!(animated.vertices(2.0 / 18.0)[0].position[0], 0);
        assert_eq!(animated.vertices(f32::NAN)[0].position[0], 0);
        assert_eq!(
            static_fallback.vertices(1.0)[0],
            static_fallback.model.vertices[0]
        );
        assert!(renderer.models[renderer.rocket_models[2].unwrap()]
            .animation
            .is_none());
        std::fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn projectile_animation_changes_rendered_pose_and_keeps_wall_occlusion() {
        let mut renderer = renderer();
        let first = renderer.models[0].model.vertices.clone();
        let mut second = first.clone();
        for vertex in &mut second {
            vertex.position[2] += 512;
        }
        renderer.models[0].animation = Some(Animation {
            vertex_count: first.len() as u16,
            frames: vec![first, second],
        });
        let camera = CameraView {
            player: Player {
                position: [0.0, 0.0],
                angle: 0.0,
            },
            eye_height: 1.0,
            pitch: 0.0,
        };
        let mut shot = ProjectileVisual {
            rocket_type: 0,
            position: [1.0, 0.0, 1.0],
            yaw: std::f32::consts::FRAC_PI_2,
            pitch: 0.0,
            elapsed_seconds: 0.0,
        };
        let mut first_frame = vec![0; 64 * 64];
        let mut second_frame = first_frame.clone();
        let mut depth = vec![f32::INFINITY; first_frame.len()];
        renderer.render(&mut first_frame, &mut depth, 64, 64, camera, &[shot]);
        shot.elapsed_seconds = 1.0 / 18.0;
        depth.fill(f32::INFINITY);
        renderer.render(&mut second_frame, &mut depth, 64, 64, camera, &[shot]);
        assert_ne!(first_frame, second_frame);
        second_frame.fill(0);
        depth.fill(0.1);
        renderer.render(&mut second_frame, &mut depth, 64, 64, camera, &[shot]);
        assert!(second_frame.iter().all(|pixel| *pixel == 0));
    }

    #[test]
    fn installed_projectile_animation_assets_load_when_available() {
        let explicit = std::env::var_os("OPENCHASM_TEST_CONTENT");
        let path = explicit
            .as_ref()
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from("assets/csm.bin"));
        if !path.exists() && explicit.is_none() {
            return;
        }
        let mut resources = ResourceSet::open(&path).unwrap();
        let rockets = RocketDefinitions::parse(&resources.read("chasm.inf").unwrap()).unwrap();
        assert_eq!(rockets.0[0].animation_name, "disk.ani");
        assert_eq!(rockets.0[6].animation_name, "3arow_.ani");
        assert_eq!(rockets.0[4].raw_fields[6], 1);
        assert_eq!(rockets.0[0].raw_fields[6], 0);
        let palette = Palette::parse(&resources.read("chasm2.pal").unwrap()).unwrap();
        let renderer = ProjectileRenderer::load(&mut resources, &rockets, &palette);
        assert!(renderer.fullbright[4]);
        assert!(!renderer.fullbright[0]);
        let mesh = &renderer.models[renderer.rocket_models[0].unwrap()];
        assert!(mesh.animation.as_ref().unwrap().frames.len() > 1);
        assert_ne!(mesh.vertices(0.0), mesh.vertices(1.0 / 18.0));
    }

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
            fullbright: vec![false, false],
            models: vec![RocketMesh {
                model,
                animation: None,
            }],
            palette,
        }
    }
    #[test]
    fn pitched_floor_depth_occludes_below_floor_projectile_but_not_open_tile() {
        use crate::map::{Floors, Map};
        use crate::planes::PlaneRenderer;
        let make_planes = |floor_id: u8| {
            let mut map_bytes = vec![0; 0x27001 + 4];
            map_bytes[0x23001..0x24001].fill(63);
            map_bytes[0x24001..0x25001].fill(floor_id);
            let map = Map::parse(&map_bytes).unwrap();
            let floors = Floors::parse(&vec![0; 64 + 64 * 64 * 86]).unwrap();
            let palette = Palette::parse(&[0; 768]).unwrap();
            PlaneRenderer::new(&map, &floors, &palette)
        };
        let camera = CameraView {
            player: Player {
                position: [16.5, 16.5],
                angle: 0.0,
            },
            eye_height: 1.0,
            pitch: -0.5_f32.atan(),
        };
        let solid = make_planes(0);
        let open = make_planes(63);
        let renderer = renderer();
        let mut shot = ProjectileVisual {
            rocket_type: 0,
            position: [19.5, 16.5, 0.5],
            yaw: std::f32::consts::FRAC_PI_2,
            pitch: 0.0,
            elapsed_seconds: 0.0,
        };
        let mut frame = vec![0; 64 * 64];
        let mut depth = vec![f32::INFINITY; frame.len()];
        solid.render_with_depth(&mut frame, &mut depth, 64, 64, camera, None);
        let baseline = frame.clone();
        renderer.render(&mut frame, &mut depth, 64, 64, camera, &[shot]);
        assert!(
            frame != baseline,
            "projectile above floor should be visible"
        );
        shot.position[2] = -0.5;
        depth.fill(f32::INFINITY);
        solid.render_with_depth(&mut frame, &mut depth, 64, 64, camera, None);
        let baseline = frame.clone();
        renderer.render(&mut frame, &mut depth, 64, 64, camera, &[shot]);
        assert!(frame == baseline, "floor must hide projectile below it");
        depth.fill(f32::INFINITY);
        open.render_with_depth(&mut frame, &mut depth, 64, 64, camera, None);
        let background = frame.clone();
        renderer.render(&mut frame, &mut depth, 64, 64, camera, &[shot]);
        assert!(
            frame
                .iter()
                .zip(background)
                .any(|(&pixel, old)| pixel != old && pixel & 0xff0000 != 0),
            "open floor should reveal the same projectile"
        );
    }

    #[test]
    fn fullbright_projectile_keeps_color_and_obeys_depth() {
        let mut renderer = renderer();
        renderer.rocket_models.push(Some(0));
        renderer.fullbright.push(true);
        let camera = CameraView {
            player: Player {
                position: [0.0, 0.0],
                angle: 0.0,
            },
            eye_height: 1.0,
            pitch: 0.0,
        };
        let mut shot = ProjectileVisual {
            rocket_type: 0,
            position: [3.0, 0.0, 1.0],
            yaw: std::f32::consts::FRAC_PI_2,
            pitch: 0.0,
            elapsed_seconds: 0.0,
        };
        let mut frame = vec![0; 64 * 64];
        let mut depth = vec![f32::INFINITY; frame.len()];
        renderer.render(&mut frame, &mut depth, 64, 64, camera, &[shot]);
        let pixel = frame.iter().position(|&color| color != 0).unwrap();
        assert!(frame[pixel] < 0xff0000);
        frame.fill(0);
        depth.fill(f32::INFINITY);
        let mut map_bytes = vec![0; 0x27001 + 4];
        map_bytes[1..0x10001].fill(35);
        let map = crate::map::Map::parse(&map_bytes).unwrap();
        renderer.render_with_static_light(
            &mut frame,
            &mut depth,
            64,
            64,
            camera,
            &[shot],
            Some(map.static_light()),
        );
        assert!(frame[pixel] < 0x800000);
        frame.fill(0);
        depth.fill(f32::INFINITY);
        shot.rocket_type = 2;
        renderer.render_with_static_light(
            &mut frame,
            &mut depth,
            64,
            64,
            camera,
            &[shot],
            Some(map.static_light()),
        );
        assert_eq!(frame[pixel], 0xff0000);
        frame.fill(0);
        depth.fill(1.0);
        renderer.render(&mut frame, &mut depth, 64, 64, camera, &[shot]);
        assert!(frame.iter().all(|&color| color == 0));
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
            elapsed_seconds: 0.0,
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
            elapsed_seconds: 0.0,
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
            elapsed_seconds: 0.0,
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
