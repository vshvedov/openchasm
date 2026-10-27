//! Original indexed impact sprites selected from committed combat events.
//!
//! Bounded visual-only particles follow the independent reference's approximate
//! burst counts, velocities and sprite metadata. Geometry remains approximate.
use crate::content::ResourceSet;
use crate::explosion_render::ExplosionRenderer;
use crate::object_render::CameraView;
use crate::texture::Palette;
use openchasm::combat::{FireResult, RocketDefinitions, ShotImpact};
use openchasm::projectiles::ImpactTarget;
use openchasm::session::SessionEvents;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Bullet,
    Blood,
    Spark,
}

fn impact_kind(actor: bool, physical: bool, blow: i32) -> Option<Kind> {
    if actor {
        Some(Kind::Blood)
    } else if !physical && blow == 1 {
        Some(Kind::Bullet)
    } else if physical && matches!(blow, 1 | 3 | 4) {
        Some(Kind::Spark)
    } else {
        None
    }
}

pub struct ImpactRenderer {
    bullet: Option<ExplosionRenderer>,
    blood: Option<ExplosionRenderer>,
    blood_b: Option<ExplosionRenderer>,
    spark: Option<ExplosionRenderer>,
    smoke: Option<ExplosionRenderer>,
    rockets: Vec<(bool, i32)>,
    random: u32,
}
impl ImpactRenderer {
    pub fn load(
        resources: &mut ResourceSet,
        palette: &Palette,
        rockets: &RocketDefinitions,
    ) -> Self {
        let mut load = |name| {
            ExplosionRenderer::load_sprite(resources, palette, name)
                .ok()
                .map(|mut sprite| {
                    sprite.set_half_size(true);
                    sprite
                })
        };
        Self {
            bullet: load("sprite_effects/bullet.obj"),
            blood: load("sprite_effects/blood.obj"),
            blood_b: load("sprite_effects/bloodb.obj"),
            spark: load("sprite_effects/s.obj"),
            smoke: load("sprite_effects/sm3.obj"),
            rockets: rockets
                .0
                .iter()
                .map(|r| {
                    (
                        !r.model_name.is_empty(),
                        r.raw_fields.first().copied().unwrap_or(0),
                    )
                })
                .collect(),
            random: 0x7a84_3f29,
        }
    }
    fn random_unit(&mut self) -> f32 {
        let mut value = self.random;
        value ^= value << 13;
        value ^= value >> 17;
        value ^= value << 5;
        self.random = value;
        (value >> 8) as f32 / 16_777_216.0
    }
    fn random_direction(&mut self) -> [f32; 3] {
        let z = self.random_unit() * 2.0 - 1.0;
        let angle = self.random_unit() * std::f32::consts::TAU;
        let radial = (1.0 - z * z).max(0.0).sqrt();
        [radial * angle.cos(), radial * angle.sin(), z]
    }
    fn smoke(&mut self, position: [f32; 3], count: usize, min_speed: f32, max_speed: f32) {
        for _ in 0..count {
            let direction = self.random_direction();
            let radius = self.random_unit().cbrt() * 0.05;
            let speed = min_speed + (max_speed - min_speed) * self.random_unit();
            let pos = [
                position[0] + radius * direction[0],
                position[1] + radius * direction[1],
                position[2] + radius * direction[2] + 0.05,
            ];
            if let Some(sprite) = &mut self.smoke {
                sprite.spawn_particle(pos, [0.0, 0.0, speed], false, false, false);
            }
        }
    }
    fn spawn(&mut self, kind: Kind, position: [f32; 3]) {
        if !position.iter().all(|p| p.is_finite()) {
            return;
        }
        match kind {
            Kind::Bullet => {
                if let Some(sprite) = &mut self.bullet {
                    sprite.spawn(position);
                }
                self.smoke(position, 3, 0.25, 0.25);
            }
            Kind::Blood => {
                for _ in 0..12 {
                    let mut direction = self.random_direction();
                    direction[2] = direction[2].abs();
                    let speed = 0.8 + 0.4 * self.random_unit();
                    let velocity = direction.map(|v| v * speed);
                    let pos = std::array::from_fn(|i| position[i] + velocity[i] / 16.0);
                    let looping = self.random_unit() < 0.5;
                    let sprite = if looping {
                        &mut self.blood_b
                    } else {
                        &mut self.blood
                    };
                    if let Some(sprite) = sprite {
                        sprite.spawn_particle(pos, velocity, true, true, looping);
                    }
                }
            }
            Kind::Spark => {
                for _ in 0..48 {
                    let mut direction = self.random_direction();
                    direction[2] *= 2.0;
                    let speed = 1.0 + self.random_unit();
                    if let Some(sprite) = &mut self.spark {
                        sprite.spawn_particle(
                            position,
                            direction.map(|v| v * speed),
                            true,
                            true,
                            false,
                        );
                    }
                }
                self.smoke(position, 5, 0.5, 1.5);
            }
        }
    }
    /// Consume committed player and physical projectile impacts only.
    /// Aim and projectile heights come from the authoritative session.
    pub fn consume(&mut self, events: &SessionEvents, camera: CameraView, rocket_id: usize) {
        if let Some(FireResult::Fired(shot)) = events.shot {
            let (actor, distance) = match shot {
                ShotImpact::Actor { distance, .. } => (true, distance),
                ShotImpact::Wall { distance, .. }
                | ShotImpact::Object { distance, .. }
                | ShotImpact::Plane { distance, .. } => (false, distance),
                _ => (false, f32::NAN),
            };
            if distance.is_finite() && distance >= 0.0 && camera.player.angle.is_finite() {
                if let Some(&(physical, blow)) = self.rockets.get(rocket_id) {
                    if !physical {
                        if let Some(kind) = impact_kind(actor, false, blow) {
                            let (sin, cos) = camera.player.angle.sin_cos();
                            self.spawn(
                                kind,
                                [
                                    camera.player.position[0] + cos * distance,
                                    camera.player.position[1] + sin * distance,
                                    camera.eye_height + distance * camera.pitch.tan(),
                                ],
                            );
                        }
                    }
                }
            }
        }
        for (hit, &height) in events
            .projectile_impacts
            .iter()
            .zip(&events.projectile_impact_heights)
        {
            if hit.target == ImpactTarget::Expired {
                continue;
            }
            let Some(&(physical, blow)) = self.rockets.get(hit.rocket_id) else {
                continue;
            };
            let actor = matches!(
                hit.target,
                ImpactTarget::Actor { .. } | ImpactTarget::Player
            );
            if let Some(kind) = impact_kind(actor, physical, blow) {
                self.spawn(kind, [hit.position[0], hit.position[1], height]);
            }
        }
    }
    pub fn tick(&mut self) {
        for sprite in [
            &mut self.bullet,
            &mut self.blood,
            &mut self.blood_b,
            &mut self.spark,
            &mut self.smoke,
        ]
        .into_iter()
        .flatten()
        {
            sprite.tick();
        }
    }
    pub fn clear(&mut self) {
        for sprite in [
            &mut self.bullet,
            &mut self.blood,
            &mut self.blood_b,
            &mut self.spark,
            &mut self.smoke,
        ]
        .into_iter()
        .flatten()
        {
            sprite.clear();
        }
        self.random = 0x7a84_3f29;
    }
    pub fn render_deferred<C: crate::color_pipeline::RenderPixel>(
        &self,
        frame: &mut [C],
        depth: &[f32],
        width: usize,
        height: usize,
        camera: CameraView,
        transparency: &mut crate::object_render::Translucency<C>,
    ) {
        for sprite in [
            &self.bullet,
            &self.blood,
            &self.blood_b,
            &self.spark,
            &self.smoke,
        ]
        .into_iter()
        .flatten()
        {
            sprite.render_deferred(frame, depth, width, height, camera, transparency);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn effect_selection_distinguishes_bullets_blood_and_physical_sparks() {
        assert_eq!(impact_kind(false, false, 1), Some(Kind::Bullet));
        assert_eq!(impact_kind(true, false, 1), Some(Kind::Blood));
        assert_eq!(impact_kind(true, true, 2), Some(Kind::Blood));
        assert_eq!(impact_kind(false, true, 1), Some(Kind::Spark));
        assert_eq!(impact_kind(false, true, 2), None);
        assert_eq!(impact_kind(false, false, 0), None);
    }
    #[test]
    fn installed_particles_spawn_bounded_groups_and_expire() {
        let explicit = std::env::var_os("OPENCHASM_TEST_CONTENT");
        let path = explicit
            .as_ref()
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| "assets/csm.bin".into());
        if !path.exists() && explicit.is_none() {
            return;
        }
        let mut resources = ResourceSet::open(&path).unwrap();
        let palette = Palette::parse(&resources.read("chasm2.pal").unwrap()).unwrap();
        let mut renderer =
            ImpactRenderer::load(&mut resources, &palette, &RocketDefinitions(vec![]));
        let count = |item: &Option<ExplosionRenderer>| item.as_ref().unwrap().effect_count();
        renderer.spawn(Kind::Bullet, [1.0, 2.0, 1.0]);
        assert_eq!(count(&renderer.bullet), 1);
        assert_eq!(count(&renderer.smoke), 3);
        renderer.spawn(Kind::Blood, [1.0, 2.0, 1.0]);
        assert_eq!(count(&renderer.blood) + count(&renderer.blood_b), 12);
        renderer.spawn(Kind::Spark, [1.0, 2.0, 1.0]);
        assert_eq!(count(&renderer.spark), 48);
        assert_eq!(count(&renderer.smoke), 8);
        for _ in 0..601 {
            renderer.tick();
        }
        assert_eq!(
            count(&renderer.smoke)
                + count(&renderer.spark)
                + count(&renderer.blood)
                + count(&renderer.blood_b)
                + count(&renderer.bullet),
            0
        );
        renderer.spawn(Kind::Bullet, [f32::NAN, 0.0, 0.0]);
        assert_eq!(count(&renderer.bullet), 0);
    }
    #[test]
    fn floor_and_ceiling_hits_emit_sprites_at_aimed_contact() {
        use openchasm::combat::{RocketDefinition, ShotPlane};
        let explicit = std::env::var_os("OPENCHASM_TEST_CONTENT");
        let path = explicit
            .as_ref()
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| "assets/csm.bin".into());
        if !path.exists() && explicit.is_none() {
            return;
        }
        let mut resources = ResourceSet::open(&path).unwrap();
        let palette = Palette::parse(&resources.read("chasm2.pal").unwrap()).unwrap();
        let rockets = RocketDefinitions(vec![RocketDefinition {
            model_name: String::new(),
            animation_name: String::new(),
            power: 1,
            raw_fields: vec![1],
        }]);
        let mut renderer = ImpactRenderer::load(&mut resources, &palette, &rockets);
        for (plane, pitch) in [
            (ShotPlane::Floor, -std::f32::consts::FRAC_PI_4),
            (ShotPlane::Ceiling, std::f32::consts::FRAC_PI_4),
        ] {
            let events = SessionEvents {
                shot: Some(FireResult::Fired(ShotImpact::Plane {
                    plane,
                    distance: 1.0,
                })),
                ..Default::default()
            };
            let camera = CameraView {
                player: crate::world::Player {
                    position: [1.0, 2.0],
                    angle: 0.0,
                },
                eye_height: 1.0,
                pitch,
            };
            renderer.consume(&events, camera, 0);
        }
        let positions = renderer.bullet.as_ref().unwrap().effect_positions();
        assert_eq!(positions.len(), 2);
        assert_eq!(positions[0], [2.0, 2.0, 0.0]);
        assert_eq!(positions[1], [2.0, 2.0, 2.0]);
        assert_eq!(renderer.smoke.as_ref().unwrap().effect_count(), 6);
    }
}
