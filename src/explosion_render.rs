//! Client-only explosion sprites. No simulation, network, or audio state.
//!
//! The original indexed OBJ sprite is played at a provisional 15 frames/s.
//! One billboard per explosion event approximates the original's burst.
//! Palette index 255 is transparent; depth testing uses the supplied world
//! buffer, which is never modified. Fullbright, light flashes, particles and
//! sprite-to-translucent-object ordering are not reconstructed here.
//!
//! Committed explosion events include source identity. Mine detonations and
//! projectiles whose authored BW is 2 use the explosion sprite; other blow
//! effects need their own presentation.
use crate::content::ResourceSet;
use crate::object_render::CameraView;
use crate::texture::Palette;
use std::io::{self, ErrorKind};

const MAX_FRAMES: usize = 256;
const MAX_SIDE: usize = 512;
const MAX_PIXELS: usize = 8 * 1024 * 1024;
const MAX_EFFECTS: usize = 256;
const NEAR: f32 = 0.05;
const TICKS_PER_FRAME: u32 = 4; // 15 FPS at 60 simulation ticks/s.

// Only sample pixels whose centers lie inside the projected image. A direct
// float-to-usize cast saturates negative coordinates to zero and paints a
// one-pixel fringe when the billboard starts within a pixel.
fn texel_coordinate(pixel: usize, start: f32, scale: f32, limit: usize) -> Option<usize> {
    let coordinate = (pixel as f32 + 0.5 - start) / scale;
    if coordinate >= 0.0 && coordinate < limit as f32 {
        Some(coordinate as usize)
    } else {
        None
    }
}

struct Frame {
    width: usize,
    height: usize,
    center: usize,
    pixels: Vec<u8>,
}
struct Sprite {
    frames: Vec<Frame>,
    height: usize,
}
impl Sprite {
    fn parse(bytes: &[u8]) -> io::Result<Self> {
        let invalid = || io::Error::new(ErrorKind::InvalidData, "invalid explosion sprite");
        let word = |offset: usize| -> io::Result<usize> {
            let data: [u8; 2] = bytes
                .get(offset..offset + 2)
                .ok_or_else(invalid)?
                .try_into()
                .map_err(|_| invalid())?;
            Ok(usize::from(u16::from_le_bytes(data)))
        };
        let count = word(0)?;
        if count == 0 || count > MAX_FRAMES {
            return Err(invalid());
        }
        let mut frames = Vec::with_capacity(count);
        let mut cursor = 2;
        let mut total = 0usize;
        let mut max_height = 0;
        for _ in 0..count {
            let width = word(cursor)?;
            let height = word(cursor + 2)?;
            let center = word(cursor + 4)?;
            if width == 0 || height == 0 || width > MAX_SIDE || height > MAX_SIDE || center > width
            {
                return Err(invalid());
            }
            let size = width.checked_mul(height).ok_or_else(invalid)?;
            total = total
                .checked_add(size)
                .filter(|&n| n <= MAX_PIXELS)
                .ok_or_else(invalid)?;
            cursor += 6;
            let source = bytes.get(cursor..cursor + size).ok_or_else(invalid)?;
            let mut pixels = vec![0; size];
            for y in 0..height {
                for x in 0..width {
                    pixels[y * width + x] = source[height - 1 - y + x * height];
                }
            }
            cursor += size;
            max_height = max_height.max(height);
            frames.push(Frame {
                width,
                height,
                center,
                pixels,
            });
        }
        Ok(Self {
            frames,
            height: max_height,
        })
    }
}

#[derive(Clone, Copy)]
struct Effect {
    position: [f32; 3],
    velocity: [f32; 3],
    age_ticks: u32,
    gravity: bool,
    bounce: bool,
    looping: bool,
}

pub struct ExplosionRenderer {
    sprite: Sprite,
    palette: [u32; 256],
    effects: Vec<Effect>,
    projectile_explosions: Vec<bool>,
    half_size: bool,
}
impl ExplosionRenderer {
    #[cfg(test)]
    pub(crate) fn effect_count(&self) -> usize {
        self.effects.len()
    }
    #[cfg(test)]
    pub(crate) fn effect_positions(&self) -> Vec<[f32; 3]> {
        self.effects.iter().map(|effect| effect.position).collect()
    }
    /// Load an indexed world-effect sprite with the shared palette and depth
    /// renderer. Other effect kinds provide their own event selection.
    pub fn load_sprite(
        resources: &mut ResourceSet,
        palette: &Palette,
        path: &str,
    ) -> io::Result<Self> {
        let sprite = Sprite::parse(&resources.read(path)?)?;
        Ok(Self {
            sprite,
            palette: std::array::from_fn(|index| {
                let [r, g, b] = palette.rgb8(index as u8);
                (u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b)
            }),
            effects: Vec::new(),
            projectile_explosions: Vec::new(),
            half_size: false,
        })
    }
    /// Load the original effect; unavailable or malformed content returns an
    /// error so callers can disable presentation without affecting gameplay.
    pub fn load(
        resources: &mut ResourceSet,
        palette: &Palette,
        rockets: &openchasm::combat::RocketDefinitions,
    ) -> io::Result<Self> {
        let mut renderer = Self::load_sprite(resources, palette, "sprite_effects/exp.obj")?;
        renderer.projectile_explosions = rockets
            .0
            .iter()
            .map(|rocket| !rocket.model_name.is_empty() && rocket.raw_fields.first() == Some(&2))
            .collect();
        Ok(renderer)
    }
    /// Add a committed visual event in map tiles. Reject invalid positions and
    /// capacity overflow, leaving all existing effects untouched.
    pub fn spawn(&mut self, position: [f32; 3]) -> bool {
        self.spawn_particle(position, [0.0; 3], false, false, false)
    }
    /// Configure authored HalfSize for the sprite before spawning particles.
    pub fn set_half_size(&mut self, half_size: bool) {
        self.half_size = half_size;
    }
    pub fn spawn_particle(
        &mut self,
        position: [f32; 3],
        velocity: [f32; 3],
        gravity: bool,
        bounce: bool,
        looping: bool,
    ) -> bool {
        if !position
            .iter()
            .chain(velocity.iter())
            .all(|x| x.is_finite())
            || self.effects.len() >= MAX_EFFECTS
        {
            return false;
        }
        self.effects.push(Effect {
            position,
            velocity,
            age_ticks: 0,
            gravity,
            bounce,
            looping,
        });
        true
    }
    /// Consume matching committed explosions in authored event order. Refuse
    /// mismatched metadata before changing state; unsupported projectile blow
    /// types are ignored, and capacity overflow drops only visual effects.
    pub fn consume(
        &mut self,
        events: &openchasm::session::SessionEvents,
    ) -> Result<usize, &'static str> {
        use openchasm::session::ExplosionOrigin;
        if events.explosions.len() != events.explosion_origins.len() {
            return Err("explosion origin count does not match explosion count");
        }
        let mut spawned = 0;
        for (explosion, origin) in events.explosions.iter().zip(&events.explosion_origins) {
            let supported = match *origin {
                ExplosionOrigin::Mine { .. } => true,
                ExplosionOrigin::Projectile { rocket_id, .. } => self
                    .projectile_explosions
                    .get(rocket_id)
                    .copied()
                    .unwrap_or(false),
            };
            if supported && self.spawn(explosion.center) {
                spawned += 1;
            }
        }
        Ok(spawned)
    }

    /// Advance one 60 Hz simulation tick.
    pub fn tick(&mut self) {
        let duration = self.sprite.frames.len() as u32 * TICKS_PER_FRAME;
        for effect in &mut self.effects {
            effect.age_ticks += 1;
            if effect.gravity {
                effect.velocity[2] -= 5.0 / 60.0;
            }
            for (position, velocity) in effect.position.iter_mut().zip(effect.velocity) {
                *position += velocity / 60.0;
            }
            if effect.position[2] < 0.0 {
                if effect.bounce {
                    effect.position[2] = 0.0;
                    effect.velocity[0] *= 0.5;
                    effect.velocity[1] *= 0.5;
                    effect.velocity[2] *= -0.5;
                }
                if !effect.looping || effect.velocity[2].abs() < 0.2 {
                    effect.age_ticks = 601;
                }
            }
        }
        self.effects.retain(|effect| {
            effect.age_ticks <= 600 && (effect.looping || effect.age_ticks < duration)
        });
    }
    pub fn clear(&mut self) {
        self.effects.clear();
    }
    /// Draw back-to-front without modifying opaque depth. `depth` must contain
    /// perpendicular camera-space depths from walls/objects/projectiles.
    pub fn render(
        &self,
        frame: &mut [u32],
        depth: &[f32],
        width: usize,
        height: usize,
        camera: CameraView,
    ) {
        if width == 0
            || height == 0
            || width.checked_mul(height) != Some(frame.len())
            || frame.len() != depth.len()
            || !camera.eye_height.is_finite()
            || !camera.pitch.is_finite()
            || !camera.player.angle.is_finite()
            || !camera.player.position.iter().all(|x| x.is_finite())
        {
            return;
        }
        let (sin, cos) = camera.player.angle.sin_cos();
        let mut sorted = Vec::with_capacity(self.effects.len());
        for effect in &self.effects {
            let dx = effect.position[0] - camera.player.position[0];
            let dy = effect.position[1] - camera.player.position[1];
            let distance = dx * cos + dy * sin;
            if distance > NEAR && distance.is_finite() {
                sorted.push((distance, -dx * sin + dy * cos, effect));
            }
        }
        sorted.sort_unstable_by(|a, b| b.0.total_cmp(&a.0));
        for (distance, side, effect) in sorted {
            let image = &self.sprite.frames
                [(effect.age_ticks / TICKS_PER_FRAME) as usize % self.sprite.frames.len()];
            let size_factor = if self.half_size { 0.5 } else { 1.0 };
            let scale_x = CameraView::focal_length(height) * size_factor / (distance * 64.0);
            let scale_y = height as f32 * 0.5 * size_factor / (distance * 64.0);
            let center_x = CameraView::project_x(width, height, side / distance);
            let center_y = camera.horizon(height)
                - height as f32 * 0.5 * (effect.position[2] - camera.eye_height) / distance;
            let left = center_x - image.center as f32 * scale_x;
            let top = center_y - self.sprite.height as f32 * 0.5 * scale_y;
            let right = left + image.width as f32 * scale_x;
            let bottom = top + image.height as f32 * scale_y;
            let x0 = left.floor().max(0.0).min(width as f32) as usize;
            let x1 = right.ceil().max(0.0).min(width as f32) as usize;
            let y0 = top.floor().max(0.0).min(height as f32) as usize;
            let y1 = bottom.ceil().max(0.0).min(height as f32) as usize;
            for y in y0..y1 {
                let Some(ty) = texel_coordinate(y, top, scale_y, image.height) else {
                    continue;
                };
                for x in x0..x1 {
                    let Some(tx) = texel_coordinate(x, left, scale_x, image.width) else {
                        continue;
                    };
                    let offset = y * width + x;
                    // Small contact tolerance allows impacts on a wall to be visible.
                    if distance > depth[offset] + 1.0 / 32.0 {
                        continue;
                    }
                    let index = image.pixels[ty * image.width + tx];
                    if index != 255 {
                        frame[offset] = self.palette[index as usize];
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::Player;
    fn sprite_bytes() -> Vec<u8> {
        // One 2x2 frame, source is column-major bottom-to-top.
        vec![1, 0, 2, 0, 2, 0, 1, 0, 1, 2, 3, 255]
    }
    fn renderer() -> ExplosionRenderer {
        let mut palette = [0; 256];
        palette[1] = 0xff0000;
        palette[2] = 0x00ff00;
        palette[3] = 0x0000ff;
        ExplosionRenderer {
            sprite: Sprite::parse(&sprite_bytes()).unwrap(),
            palette,
            effects: Vec::new(),
            projectile_explosions: vec![true, false],
            half_size: false,
        }
    }
    #[test]
    fn sprite_parser_decodes_columns_and_rejects_bad_input() {
        let sprite = Sprite::parse(&sprite_bytes()).unwrap();
        assert_eq!(sprite.frames[0].pixels, [2, 255, 1, 3]);
        assert!(Sprite::parse(&sprite_bytes()[..11]).is_err());
        assert!(Sprite::parse(&[0, 0]).is_err());
        assert!(Sprite::parse(&[1, 0, 1, 0, 1, 0, 2, 0, 1]).is_err());
    }
    #[test]
    fn effect_bounds_animation_and_wall_depth() {
        let mut renderer = renderer();
        assert!(!renderer.spawn([f32::NAN, 0.0, 0.0]));
        assert!(renderer.spawn([0.2, 0.0, 1.0]));
        let camera = CameraView {
            player: Player {
                position: [0.0, 0.0],
                angle: 0.0,
            },
            eye_height: 1.0,
            pitch: 0.0,
        };
        let mut frame = vec![0; 64 * 64];
        let depth = vec![f32::INFINITY; frame.len()];
        renderer.render(&mut frame, &depth, 64, 64, camera);
        assert!(frame.iter().any(|&color| color != 0));
        frame.fill(0);
        renderer.render(&mut frame, &vec![0.1; depth.len()], 64, 64, camera);
        assert!(frame.iter().all(|&color| color == 0));
        for _ in 0..TICKS_PER_FRAME {
            renderer.tick();
        }
        assert!(renderer.effects.is_empty());
        for _ in 0..MAX_EFFECTS {
            assert!(renderer.spawn([1.0, 1.0, 1.0]));
        }
        assert!(!renderer.spawn([1.0, 1.0, 1.0]));
        renderer.clear();
        assert!(renderer.effects.is_empty());
    }
    #[test]
    fn sprite_pixels_remain_square_across_viewport_aspects() {
        let mut renderer = renderer();
        renderer.sprite.frames[0].pixels.fill(1);
        assert!(renderer.spawn([0.125, 0.0, 1.0]));
        let camera = CameraView {
            player: crate::world::Player {
                position: [0.0, 0.0],
                angle: 0.0,
            },
            eye_height: 1.0,
            pitch: 0.0,
        };
        for (width, height) in [(128, 64), (64, 128)] {
            let mut frame = vec![0; width * height];
            let depth = vec![f32::INFINITY; frame.len()];
            renderer.render(&mut frame, &depth, width, height, camera);
            let points: Vec<_> = frame
                .iter()
                .enumerate()
                .filter(|(_, c)| **c != 0)
                .map(|(i, _)| (i % width, i / width))
                .collect();
            let min_x = points.iter().map(|p| p.0).min().unwrap();
            let max_x = points.iter().map(|p| p.0).max().unwrap();
            let min_y = points.iter().map(|p| p.1).min().unwrap();
            let max_y = points.iter().map(|p| p.1).max().unwrap();
            assert_eq!(max_x - min_x, max_y - min_y);
        }
    }

    #[test]
    fn moving_particles_apply_gravity_floor_bounce_and_expiry() {
        let mut renderer = renderer();
        assert!(renderer.spawn_particle([0.0, 0.0, 0.001], [0.6, 0.0, -0.5], true, true, true));
        renderer.tick();
        let particle = renderer.effects[0];
        assert_eq!(particle.position[2], 0.0);
        assert!((particle.position[0] - 0.01).abs() < 0.00001);
        assert!((particle.velocity[0] - 0.3).abs() < 0.00001);
        assert!((particle.velocity[2] - (0.5 + 5.0 / 60.0) / 2.0).abs() < 0.00001);
        for _ in 0..100 {
            renderer.tick();
        }
        assert!(renderer.effects.is_empty());
        assert!(!renderer.spawn_particle([0.0; 3], [f32::NAN, 0.0, 0.0], true, true, true));
    }
    #[test]
    fn installed_explosion_sprite_decodes_when_available() {
        let explicit = std::env::var_os("OPENCHASM_TEST_CONTENT");
        let path = explicit
            .as_ref()
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from("assets/csm.bin"));
        if !path.exists() && explicit.is_none() {
            return;
        }
        let mut resources = ResourceSet::open(&path).unwrap();
        for name in ["exp", "bullet", "blood", "s"] {
            let sprite = Sprite::parse(
                &resources
                    .read(&format!("sprite_effects/{name}.obj"))
                    .unwrap(),
            )
            .unwrap();
            assert!(sprite.frames.len() > 1, "{name}");
            assert!(
                sprite
                    .frames
                    .iter()
                    .any(|frame| frame.pixels.iter().any(|&pixel| pixel != 255)),
                "{name}"
            );
        }
    }
    #[test]
    fn consumes_only_typed_supported_committed_explosions_in_order() {
        use openchasm::explosives::Explosion;
        use openchasm::projectiles::Owner;
        use openchasm::session::{ExplosionOrigin, SessionEvents};
        let event = |x| Explosion {
            owner: Owner::Player,
            center: [x, 0.0, 1.0],
            radius: 1.0,
            damage: 1,
        };
        let mut events = SessionEvents {
            explosions: vec![event(1.0), event(2.0), event(3.0), event(4.0)],
            explosion_origins: vec![
                ExplosionOrigin::Projectile {
                    projectile_id: 1,
                    rocket_id: 1,
                },
                ExplosionOrigin::Mine { mine_id: 0 },
                ExplosionOrigin::Projectile {
                    projectile_id: 2,
                    rocket_id: 0,
                },
                ExplosionOrigin::Projectile {
                    projectile_id: 3,
                    rocket_id: 10,
                },
            ],
            ..SessionEvents::default()
        };
        let mut renderer = renderer();
        assert_eq!(renderer.consume(&events), Ok(2));
        assert_eq!(
            renderer
                .effects
                .iter()
                .map(|effect| effect.position[0])
                .collect::<Vec<_>>(),
            vec![2.0, 3.0]
        );
        events.explosion_origins.pop();
        assert!(renderer.consume(&events).is_err());
        assert_eq!(renderer.effects.len(), 2);
    }
    #[test]
    fn projected_texel_lookup_excludes_outside_pixel_centers() {
        assert_eq!(texel_coordinate(2, 2.6, 1.0, 2), None);
        assert_eq!(texel_coordinate(3, 2.6, 1.0, 2), Some(0));
        assert_eq!(texel_coordinate(4, 2.6, 1.0, 2), Some(1));
        assert_eq!(texel_coordinate(5, 2.6, 1.0, 2), None);
    }
}
