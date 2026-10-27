//! First-person weapon overlay using original 3O geometry and ANI poses.
//!
//! This renderer is presentation-only: selection, firing, animation timing and
//! weapon ownership belong to the shared simulation. See docs/weapon-rendering.md.
use crate::content::ResourceSet;
use crate::model::{Animation, Model, SKIN_WIDTH};
use crate::texture::Palette;
use std::io::{self, ErrorKind};

const WEAPON_COUNT: usize = 8;
const SCALE: f32 = 1.0 / 2048.0;
const NEAR: f32 = 0.05;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WeaponPose {
    Idle,
    Shoot,
}

/// Presentation clock at 60 simulation ticks/s. Weapon clips advance at the
/// remaster manual's 25ms/frame (40 frames/s), independent of fire cooldown.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WeaponAnimationClock {
    idle_ticks: u64,
    fire_ticks: Option<u64>,
}
impl WeaponAnimationClock {
    fn frame(ticks: u64) -> u64 {
        ticks.saturating_mul(2) / 3
    }

    /// Advance one simulation tick; a successful shot starts frame zero, and a
    /// selection change clears the old clip before a possible new shot.
    pub fn tick(&mut self, changed: bool, fired: bool, fire_frames: Option<usize>) {
        if changed {
            *self = Self::default();
        } else {
            self.idle_ticks = self.idle_ticks.saturating_add(1);
            if let Some(ticks) = &mut self.fire_ticks {
                *ticks = ticks.saturating_add(1);
                if fire_frames.is_none_or(|count| Self::frame(*ticks) >= count as u64) {
                    self.fire_ticks = None;
                    self.idle_ticks = 0;
                }
            }
        }
        if fired {
            self.fire_ticks = fire_frames.filter(|&count| count > 0).map(|_| 0);
            self.idle_ticks = 0;
        }
    }

    /// Return pose and zero-based frame. Missing/empty idle animation uses
    /// frame zero, which the renderer safely resolves to its base model.
    pub fn pose(&self, idle_frames: Option<usize>) -> (WeaponPose, usize) {
        if let Some(ticks) = self.fire_ticks {
            (WeaponPose::Shoot, Self::frame(ticks) as usize)
        } else {
            let frame = idle_frames.filter(|&count| count > 0).map_or(0, |count| {
                (Self::frame(self.idle_ticks) % count as u64) as usize
            });
            (WeaponPose::Idle, frame)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct WeaponPaths {
    model: String,
    idle: String,
    shoot: String,
}
fn invalid(message: &str) -> io::Error {
    io::Error::new(ErrorKind::InvalidData, message)
}
fn filename(value: &str) -> io::Result<String> {
    let value = value.trim();
    if value.is_empty() || value == "." || value == ".." || value.contains(['/', '\\']) {
        return Err(invalid("invalid weapon resource name"));
    }
    Ok(format!("model/{}", value.to_ascii_lowercase()))
}
fn weapon_paths(inf: &[u8]) -> io::Result<Vec<WeaponPaths>> {
    let text = std::str::from_utf8(inf).map_err(|_| invalid("weapon definitions are not UTF-8"))?;
    let mut inside = false;
    let mut pending: [Option<String>; 3] = [None, None, None];
    let mut result = Vec::new();
    for raw in text.lines() {
        let line = raw.split(';').next().unwrap_or("").trim();
        if !inside {
            if line.eq_ignore_ascii_case("[WEAPONS]") {
                inside = true;
            }
            continue;
        }
        if line.starts_with('[') {
            break;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let slot = if key.trim().eq_ignore_ascii_case("MODEL") {
            0
        } else if key.trim().eq_ignore_ascii_case("STAT") {
            1
        } else if key.trim().eq_ignore_ascii_case("SHOOT") {
            2
        } else {
            continue;
        };
        if pending[slot].is_some() {
            return Err(invalid("duplicate weapon model or animation field"));
        }
        pending[slot] = Some(filename(value)?);
        if pending.iter().all(Option::is_some) {
            let [model, idle, shoot] = pending;
            result.push(WeaponPaths {
                model: model.unwrap(),
                idle: idle.unwrap(),
                shoot: shoot.unwrap(),
            });
            pending = [None, None, None];
        }
    }
    if result.len() != WEAPON_COUNT || pending.iter().any(Option::is_some) {
        return Err(invalid(
            "weapon definitions need eight complete model/STAT/SHOOT groups",
        ));
    }
    Ok(result)
}

struct WeaponModel {
    model: Model,
    idle: Option<Animation>,
    shoot: Option<Animation>,
}
/// Loaded weapon overlay assets. Missing assets leave that weapon invisible;
/// errors are reported once during loading rather than every frame.
pub struct WeaponRenderer {
    weapons: Vec<Option<WeaponModel>>,
    palette: [u32; 256],
}
impl WeaponRenderer {
    pub fn load(resources: &mut ResourceSet, inf: &[u8], palette: &Palette) -> io::Result<Self> {
        let paths = weapon_paths(inf)?;
        let mut weapons = Vec::with_capacity(paths.len());
        for paths in paths {
            let model = match resources
                .read(&paths.model)
                .and_then(|bytes| Model::parse(&bytes))
            {
                Ok(model) => model,
                Err(error) => {
                    eprintln!("weapon preview: {}: {error}", paths.model);
                    weapons.push(None);
                    continue;
                }
            };
            let mut load_animation = |path: &str| match resources
                .read(path)
                .and_then(|bytes| Animation::parse(&bytes))
            {
                Ok(animation) if animation.matches_model(&model) => Some(animation),
                Ok(_) => {
                    eprintln!("weapon preview: {path}: vertex count mismatch");
                    None
                }
                Err(error) => {
                    eprintln!("weapon preview: {path}: {error}");
                    None
                }
            };
            let idle = load_animation(&paths.idle);
            let shoot = load_animation(&paths.shoot);
            weapons.push(Some(WeaponModel { model, idle, shoot }));
        }
        let palette = std::array::from_fn(|index| {
            let [r, g, b] = palette.rgb8(index as u8);
            (u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b)
        });
        Ok(Self { weapons, palette })
    }

    /// Number of frames in the specified loaded animation, if available.
    pub fn frame_count(&self, weapon: usize, pose: WeaponPose) -> Option<usize> {
        let weapon = self.weapons.get(weapon).and_then(Option::as_ref)?;
        let animation = match pose {
            WeaponPose::Idle => &weapon.idle,
            WeaponPose::Shoot => &weapon.shoot,
        };
        animation.as_ref().map(|animation| animation.frames.len())
    }

    /// Draw over the world using a separate weapon depth buffer. Indices are
    /// zero-based, matching inventory. Frame is clamped to the animation.
    #[cfg(test)]
    pub fn render(
        &self,
        frame: &mut [u32],
        width: usize,
        height: usize,
        weapon: usize,
        pose: WeaponPose,
        frame_index: usize,
    ) {
        self.render_with_light(frame, width, height, weapon, pose, frame_index, 1.0);
    }

    /// Draw with a bounded static light factor sampled at the camera position.
    #[allow(clippy::too_many_arguments)]
    pub fn render_with_light(
        &self,
        frame: &mut [u32],
        width: usize,
        height: usize,
        weapon: usize,
        pose: WeaponPose,
        frame_index: usize,
        light: f32,
    ) {
        if width == 0 || height == 0 || width.checked_mul(height) != Some(frame.len()) {
            return;
        }
        let Some(weapon) = self.weapons.get(weapon).and_then(Option::as_ref) else {
            return;
        };
        let animation = match pose {
            WeaponPose::Idle => &weapon.idle,
            WeaponPose::Shoot => &weapon.shoot,
        };
        let vertices = animation
            .as_ref()
            .and_then(|a| a.frames.get(frame_index.min(a.frames.len() - 1)))
            .unwrap_or(&weapon.model.vertices);
        let mut depth = vec![f32::INFINITY; frame.len()];
        let mut raster = Raster {
            frame,
            depth: &mut depth,
            width,
            height,
            palette: &self.palette,
            model: &weapon.model,
            light: if light.is_finite() {
                light.clamp(0.0, 1.3)
            } else {
                1.0
            },
        };
        for polygon in &weapon.model.polygons {
            let mut input = Vec::with_capacity(polygon.vertex_count());
            for corner in 0..polygon.vertex_count() {
                let pos = vertices[polygon.vertices[corner] as usize].position;
                input.push(Vertex {
                    // The source model faces +Y. Placement is based on the
                    // published PanzerChasm camera-local offset, not a fidelity claim.
                    side: f32::from(pos[0]) * SCALE + 0.2,
                    depth: f32::from(pos[1]) * SCALE + 0.7,
                    up: f32::from(pos[2]) * SCALE - 0.45,
                    uv: [
                        f32::from(polygon.uv[corner][0]),
                        f32::from(polygon.uv[corner][1]) + f32::from(polygon.sprite_offset),
                    ],
                });
            }
            let clipped = clip_near(&input);
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
                    polygon.flags,
                );
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
    fn lerp(self, other: Self, t: f32) -> Self {
        let f = |a, b| a + t * (b - a);
        Self {
            side: f(self.side, other.side),
            depth: f(self.depth, other.depth),
            up: f(self.up, other.up),
            uv: [f(self.uv[0], other.uv[0]), f(self.uv[1], other.uv[1])],
        }
    }
}
fn clip_near(input: &[Vertex]) -> Vec<Vertex> {
    if input.is_empty() {
        return Vec::new();
    }
    let mut result = Vec::with_capacity(input.len() + 1);
    let mut prior = input[input.len() - 1];
    for &current in input {
        if (prior.depth >= NEAR) != (current.depth >= NEAR) {
            result.push(prior.lerp(
                current,
                (NEAR - prior.depth) / (current.depth - prior.depth),
            ));
        }
        if current.depth >= NEAR {
            result.push(current);
        }
        prior = current;
    }
    result
}
#[derive(Clone, Copy)]
struct Projected {
    x: f32,
    y: f32,
    inv_depth: f32,
    u_over_depth: f32,
    v_over_depth: f32,
}
// The remaster uses a distinct fixed 1.568-radian weapon FOV.
fn weapon_focal(height: usize) -> f32 {
    height as f32 * 0.5 * 1.002_800_2
}

fn project(v: Vertex, width: usize, height: usize) -> Projected {
    let inv_depth = v.depth.recip();
    Projected {
        x: width as f32 * 0.5 + weapon_focal(height) * v.side * inv_depth,
        y: height as f32 * 0.5 - weapon_focal(height) * v.up * inv_depth,
        inv_depth,
        u_over_depth: v.uv[0] * inv_depth,
        v_over_depth: v.uv[1] * inv_depth,
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
    model: &'a Model,
    light: f32,
}
impl Raster<'_> {
    fn triangle(&mut self, triangle: [Projected; 3], flags: u8) {
        let [a, b, c] = triangle;
        let area = edge(a, b, c.x, c.y);
        if area.abs() < 0.00001 || self.model.skin_height == 0 {
            return;
        }
        let x0 = a.x.min(b.x).min(c.x).floor().max(0.0) as usize;
        let x1 = a.x.max(b.x).max(c.x).ceil().min(self.width as f32) as usize;
        let y0 = a.y.min(b.y).min(c.y).floor().max(0.0) as usize;
        let y1 = a.y.max(b.y).max(c.y).ceil().min(self.height as f32) as usize;
        for y in y0..y1 {
            for x in x0..x1 {
                let px = x as f32 + 0.5;
                let py = y as f32 + 0.5;
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
                let tx = (u.floor() as i32).rem_euclid(SKIN_WIDTH as i32) as usize;
                let ty = (v.floor() as i32).rem_euclid(i32::from(self.model.skin_height)) as usize;
                let index = self.model.skin_indices[ty * SKIN_WIDTH + tx];
                if flags & 0x0e != 0 && index == 255 {
                    continue;
                }
                let rgb = self.palette[index as usize];
                let channel =
                    |shift: u32| ((((rgb >> shift) & 255u32) as f32 * self.light) as u32).min(255);
                self.frame[pixel] = channel(16) << 16 | channel(8) << 8 | channel(0);
                self.depth[pixel] = distance;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn definitions() -> String {
        let mut text = String::from("[WEAPONS]\n8\n");
        for i in 0..8 {
            text.push_str(&format!(
                "MODEL=w{i}.3o\nSTAT=w{i}.ani\nSHOOT=f{i}.ani\n 1 2 3 4 5 6 7 8\n"
            ));
        }
        text.push_str("[ROCKETS]\n");
        text
    }
    #[test]
    fn parses_weapon_paths_and_rejects_traversal() {
        let parsed = weapon_paths(definitions().as_bytes()).unwrap();
        assert_eq!(
            parsed[0],
            WeaponPaths {
                model: "model/w0.3o".into(),
                idle: "model/w0.ani".into(),
                shoot: "model/f0.ani".into()
            }
        );
        assert!(weapon_paths(definitions().replace("w0.3o", "../bad.3o").as_bytes()).is_err());
        assert!(weapon_paths(b"[WEAPONS]\nMODEL=bad.3o\n").is_err());
    }
    #[test]
    fn selected_animation_frame_changes_the_overlay() {
        use crate::model::{Polygon, Vertex};
        let vertex = |x, z| Vertex {
            position: [x, 0, z],
        };
        let positions = vec![vertex(-512, -256), vertex(512, -256), vertex(0, 512)];
        let shifted = vec![vertex(-128, -256), vertex(896, -256), vertex(384, 512)];
        let model = Model {
            vertices: positions.clone(),
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
        let renderer = WeaponRenderer {
            weapons: vec![Some(WeaponModel {
                model,
                idle: Some(Animation {
                    vertex_count: 3,
                    frames: vec![positions, shifted],
                }),
                shoot: None,
            })],
            palette,
        };
        let mut first = vec![0; 64 * 64];
        let mut second = first.clone();
        renderer.render(&mut first, 64, 64, 0, WeaponPose::Idle, 0);
        renderer.render(&mut second, 64, 64, 0, WeaponPose::Idle, 1);
        assert!(first.contains(&0xff0000));
        assert_ne!(first, second);
        assert_eq!(renderer.frame_count(0, WeaponPose::Idle), Some(2));
        assert_eq!(renderer.frame_count(0, WeaponPose::Shoot), None);
        let mut clamped = vec![0; 64 * 64];
        renderer.render(&mut clamped, 64, 64, 0, WeaponPose::Idle, usize::MAX);
        assert_eq!(second, clamped);
        let mut dark = vec![0; 64 * 64];
        renderer.render_with_light(&mut dark, 64, 64, 0, WeaponPose::Idle, 0, 0.25);
        let pixel = first.iter().position(|&rgb| rgb != 0).unwrap();
        assert_eq!(dark[pixel], 0x3f0000);
        let mut invalid = vec![0; 64 * 64];
        renderer.render_with_light(&mut invalid, 64, 64, 0, WeaponPose::Idle, 0, f32::NAN);
        assert_eq!(first, invalid);
    }

    #[test]
    fn weapon_uses_its_own_verified_fixed_field_of_view() {
        assert!((weapon_focal(400) - 200.560_04).abs() < 1e-4);
        assert!(
            (weapon_focal(400) - crate::object_render::CameraView::focal_length(400)).abs() > 50.0
        );
    }

    #[test]
    fn weapon_projection_uses_square_pixels_for_wide_and_tall_views() {
        for (width, height) in [(640, 400), (400, 640)] {
            let center = project(
                Vertex {
                    side: 0.0,
                    up: 0.0,
                    depth: 2.0,
                    uv: [0.0; 2],
                },
                width,
                height,
            );
            let side = project(
                Vertex {
                    side: 1.0,
                    up: 0.0,
                    depth: 2.0,
                    uv: [0.0; 2],
                },
                width,
                height,
            );
            let up = project(
                Vertex {
                    side: 0.0,
                    up: 1.0,
                    depth: 2.0,
                    uv: [0.0; 2],
                },
                width,
                height,
            );
            assert!(((side.x - center.x) - (center.y - up.y)).abs() < 1e-5);
        }
    }

    #[test]
    fn clips_near_plane_and_interpolates_uv() {
        let p = |depth| Vertex {
            side: 0.0,
            depth,
            up: 0.0,
            uv: [depth, 0.0],
        };
        let clipped = clip_near(&[p(-1.0), p(1.0), p(2.0)]);
        assert_eq!(clipped.len(), 4);
        assert!(clipped.iter().all(|v| v.depth >= NEAR - 1e-5));
        assert!(clipped.iter().all(|v| (v.uv[0] - v.depth).abs() < 1e-5));
    }
    #[test]
    fn rasterizes_weapon_in_front_and_skips_masked_texel() {
        let mut frame = vec![0x112233; 16];
        let mut depth = vec![f32::INFINITY; 16];
        let mut palette = [0; 256];
        palette[1] = 0xff0000;
        let mut model = Model {
            vertices: vec![],
            polygons: vec![],
            skin_height: 1,
            skin_indices: vec![255; SKIN_WIDTH],
        };
        let p = |x, y| Projected {
            x,
            y,
            inv_depth: 1.0,
            u_over_depth: 0.0,
            v_over_depth: 0.0,
        };
        {
            let mut raster = Raster {
                frame: &mut frame,
                depth: &mut depth,
                width: 4,
                height: 4,
                palette: &palette,
                model: &model,
                light: 1.0,
            };
            raster.triangle([p(0.0, 0.0), p(4.0, 0.0), p(0.0, 4.0)], 2);
        }
        assert_eq!(frame[0], 0x112233);
        model.skin_indices.fill(1);
        {
            let mut raster = Raster {
                frame: &mut frame,
                depth: &mut depth,
                width: 4,
                height: 4,
                palette: &palette,
                model: &model,
                light: 1.0,
            };
            raster.triangle([p(0.0, 0.0), p(4.0, 0.0), p(0.0, 4.0)], 0);
        }
        assert_eq!(frame[0], 0xff0000);
    }
    #[test]
    fn weapon_clock_uses_25ms_frames_and_clip_specific_duration() {
        let mut clock = WeaponAnimationClock::default();
        clock.tick(false, true, Some(24));
        assert_eq!(clock.pose(Some(3)), (WeaponPose::Shoot, 0));
        clock.tick(false, false, Some(24));
        assert_eq!(clock.pose(Some(3)), (WeaponPose::Shoot, 0));
        clock.tick(false, false, Some(24));
        assert_eq!(clock.pose(Some(3)), (WeaponPose::Shoot, 1));
        for _ in 2..35 {
            clock.tick(false, false, Some(24));
        }
        assert_eq!(clock.pose(Some(3)), (WeaponPose::Shoot, 23));
        clock.tick(false, false, Some(24));
        assert_eq!(clock.pose(Some(3)), (WeaponPose::Idle, 0));
        clock.tick(false, true, Some(14));
        for _ in 0..20 {
            clock.tick(false, false, Some(14));
        }
        assert_eq!(clock.pose(Some(3)), (WeaponPose::Shoot, 13));
        clock.tick(false, false, Some(14));
        assert_eq!(clock.pose(Some(3)), (WeaponPose::Idle, 0));
    }

    #[test]
    fn weapon_clock_handles_retrigger_switch_and_checkpoint_clone() {
        let mut clock = WeaponAnimationClock::default();
        clock.tick(false, true, Some(4));
        for _ in 0..4 {
            clock.tick(false, false, Some(4));
        }
        let checkpoint = clock;
        assert_eq!(clock.pose(Some(2)), (WeaponPose::Shoot, 2));
        clock.tick(false, true, Some(4));
        assert_eq!(clock.pose(Some(2)), (WeaponPose::Shoot, 0));
        clock = checkpoint;
        assert_eq!(clock.pose(Some(2)), (WeaponPose::Shoot, 2));
        clock.tick(true, false, Some(7));
        assert_eq!(clock.pose(Some(2)), (WeaponPose::Idle, 0));
        clock.tick(true, true, Some(7));
        assert_eq!(clock.pose(Some(2)), (WeaponPose::Shoot, 0));
        clock.tick(false, true, None);
        assert_eq!(clock.pose(None), (WeaponPose::Idle, 0));
        clock.tick(false, false, None);
        clock.tick(false, false, None);
        assert_eq!(clock.pose(Some(2)), (WeaponPose::Idle, 1));
    }
    #[test]
    fn installed_weapon_clips_produce_distinct_firing_durations() {
        let explicit = std::env::var_os("OPENCHASM_TEST_CONTENT");
        let path = explicit
            .as_ref()
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from("assets/csm.bin"));
        if !path.exists() && explicit.is_none() {
            return;
        }
        let mut resources = ResourceSet::open(&path).unwrap();
        let inf = resources.read("chasm.inf").unwrap();
        let palette = Palette::parse(&resources.read("chasm2.pal").unwrap()).unwrap();
        let renderer = WeaponRenderer::load(&mut resources, &inf, &palette).unwrap();
        assert_eq!(renderer.frame_count(5, WeaponPose::Shoot), Some(24));
        assert_eq!(renderer.frame_count(6, WeaponPose::Shoot), Some(14));
        for (slot, ticks) in [(5, 36), (6, 21)] {
            let frames = renderer.frame_count(slot, WeaponPose::Shoot);
            let mut clock = WeaponAnimationClock::default();
            clock.tick(false, true, frames);
            for _ in 0..ticks - 1 {
                clock.tick(false, false, frames);
            }
            assert_eq!(clock.pose(Some(1)).0, WeaponPose::Shoot);
            clock.tick(false, false, frames);
            assert_eq!(clock.pose(Some(1)).0, WeaponPose::Idle);
        }
    }
}
