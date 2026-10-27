//! Window- and network-independent provisional instant-shot combat.
//!
//! Rocket definitions are data driven. Instant shots are currently a single
//! horizontal ray, with all pellets aggregated, and combat timing/radii are
//! provisional. Physical projectiles, vertical aim and effects are unsupported.
use crate::actors::{ActorDefinitions, ActorState};
use crate::inventory::{Inventory, WeaponDefinitions};
use crate::map::UNITS_PER_TILE;
use crate::object_combat::ObjectCombat;
use crate::world::{Wall, World};
use std::io::{self, ErrorKind};

const MAX_RANGE: f32 = 64.0;
const MIN_COOLDOWN: f32 = 1.0 / 60.0;
const RELOAD_UNITS_PER_SECOND: f32 = 256.0;

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(ErrorKind::InvalidData, message.into())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RocketDefinition {
    /// An empty model field indicates an instant projectile in the game's manual.
    pub model_name: String,
    pub power: i32,
    /// The complete numeric row is retained for future projectile behavior.
    pub raw_fields: Vec<i32>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RocketDefinitions(pub Vec<RocketDefinition>);
impl RocketDefinitions {
    pub fn parse(inf: &[u8]) -> io::Result<Self> {
        let text = std::str::from_utf8(inf).map_err(|_| invalid("chasm.inf is not UTF-8"))?;
        let mut lines = text.lines().enumerate();
        if !lines
            .by_ref()
            .any(|(_, line)| line.trim().eq_ignore_ascii_case("[ROCKETS]"))
        {
            return Err(invalid("missing [ROCKETS] section"));
        }
        let (_, count_line) = lines
            .by_ref()
            .find(|(_, line)| !line.split(';').next().unwrap_or("").trim().is_empty())
            .ok_or_else(|| invalid("missing rocket count"))?;
        let count: usize = count_line
            .split_whitespace()
            .next()
            .ok_or_else(|| invalid("missing rocket count"))?
            .parse()
            .map_err(|_| invalid("invalid rocket count"))?;
        if count > 1024 {
            return Err(invalid("rocket count exceeds limit"));
        }
        let mut rows = Vec::with_capacity(count);
        let mut model: Option<String> = None;
        for (line_number, raw) in lines {
            if rows.len() == count {
                break;
            }
            let line = raw.split(';').next().unwrap_or("").trim();
            if line.is_empty() || line.starts_with('*') || line.starts_with('=') {
                continue;
            }
            if line.starts_with('[') {
                break;
            }
            if let Some((name, value)) = line.split_once('=') {
                if name.trim().eq_ignore_ascii_case("3d_model") {
                    model = Some(value.trim().to_owned());
                }
                continue;
            }
            let fields: Vec<_> = line.split_whitespace().collect();
            if fields.first().and_then(|x| x.parse::<i32>().ok()).is_none() {
                continue;
            }
            let values: Vec<i32> = fields
                .iter()
                .map(|value| {
                    value.parse::<i32>().map_err(|_| {
                        invalid(format!("invalid rocket field on line {}", line_number + 1))
                    })
                })
                .collect::<io::Result<_>>()?;
            if values.len() < 5 {
                return Err(invalid(format!(
                    "short rocket row on line {}",
                    line_number + 1
                )));
            }
            let model_name = model.take().ok_or_else(|| {
                invalid(format!("missing rocket model on line {}", line_number + 1))
            })?;
            rows.push(RocketDefinition {
                model_name,
                power: values[4],
                raw_fields: values,
            });
        }
        if rows.len() != count {
            return Err(invalid("[ROCKETS] ended before declared count"));
        }
        Ok(Self(rows))
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ShotImpact {
    Actor {
        source_index: usize,
        damage: i32,
        killed: bool,
        distance: f32,
    },
    Wall {
        wall_index: usize,
        distance: f32,
    },
    Object {
        cell: [usize; 2],
        damage: i32,
        destroyed: bool,
        distance: f32,
    },
    Miss,
    /// Spawn a physical projectile with this rocket definition.
    Launch {
        rocket_id: usize,
    },
}
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FireResult {
    Fired(ShotImpact),
    CoolingDown,
    NotOwned,
    OutOfAmmo,
    Unsupported,
    InvalidWeapon,
    InvalidShooter,
}

/// Mutable simulation state and immutable level data needed to resolve a shot.
pub struct ShotContext<'a> {
    pub inventory: &'a mut Inventory,
    pub weapons: &'a WeaponDefinitions,
    pub rockets: &'a RocketDefinitions,
    pub actors: &'a mut ActorState,
    pub definitions: &'a ActorDefinitions,
    pub world: &'a World,
    pub objects: Option<&'a mut ObjectCombat>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Combat {
    cooldown: f32,
}
impl Combat {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn cooldown(&self) -> f32 {
        self.cooldown
    }
    /// Revert cooldown if the session could not spawn a projectile.
    pub fn cancel_shot(&mut self) {
        self.cooldown = 0.0;
    }
    /// Advance once per fixed simulation step. Invalid times have no effect.
    pub fn tick(&mut self, dt: f32) {
        if dt.is_finite() && dt > 0.0 {
            self.cooldown = (self.cooldown - dt).max(0.0);
        }
    }

    /// Resolve a first-pass horizontal hitscan shot from the player. The caller
    /// supplies material-specific shot collision rules independently of movement.
    /// A shot consumes one ammo; pellets hit the same ray for now. Cooldown is
    /// shared across weapon changes. Unsupported weapon kinds consume nothing.
    pub fn fire<F>(
        &mut self,
        weapon: usize,
        context: ShotContext<'_>,
        blocks_shots: F,
    ) -> FireResult
    where
        F: Fn(&Wall) -> bool,
    {
        self.fire_with_explosions(weapon, context, blocks_shots, false)
    }

    /// Enable authored grenade firing only when the owning session handles splash.
    pub fn fire_with_explosions<F>(
        &mut self,
        weapon: usize,
        context: ShotContext<'_>,
        blocks_shots: F,
        explosions: bool,
    ) -> FireResult
    where
        F: Fn(&Wall) -> bool,
    {
        let ShotContext {
            inventory,
            weapons,
            rockets,
            actors,
            definitions,
            world,
            objects,
        } = context;
        let Some(spec) = weapons.0.get(weapon) else {
            return FireResult::InvalidWeapon;
        };
        if !inventory.weapons[weapon] {
            return FireResult::NotOwned;
        }
        if self.cooldown > 0.0 {
            return FireResult::CoolingDown;
        }
        let Some(rocket) = usize::try_from(spec.rocket_type)
            .ok()
            .and_then(|i| rockets.0.get(i))
        else {
            return FireResult::Unsupported;
        };
        if rocket.power <= 0 || spec.projectile_count == 0 {
            return FireResult::Unsupported;
        }
        let physical = !rocket.model_name.is_empty();
        // Mines require placement. Grenades require an explicit splash host.
        if physical && !matches!(weapon, 3 | 4 | 7) && !(weapon == 5 && explosions) {
            return FireResult::Unsupported;
        }
        if inventory.ammo[weapon] == 0 {
            return FireResult::OutOfAmmo;
        }
        let player = &world.player;
        if !player.angle.is_finite() || !player.position.iter().all(|value| value.is_finite()) {
            return FireResult::InvalidShooter;
        }
        inventory.ammo[weapon] -= 1;
        self.cooldown = (spec.reload_time as f32 / RELOAD_UNITS_PER_SECOND).max(MIN_COOLDOWN);
        if physical {
            return FireResult::Fired(ShotImpact::Launch {
                rocket_id: spec.rocket_type as usize,
            });
        }
        let (sin, cos) = player.angle.sin_cos();
        let direction = [cos, sin];
        let mut hits = Vec::new();
        world.ray_hits(direction, &mut hits);
        let wall_hit = hits
            .into_iter()
            .filter(|(_, index, _)| blocks_shots(&world.walls[*index]))
            .map(|(distance, index, _)| (distance, index))
            .min_by(|a, b| a.0.total_cmp(&b.0));
        let mut nearest = wall_hit.map_or(MAX_RANGE, |(distance, _)| distance.min(MAX_RANGE));
        let mut selected = None;
        let mut object_hit = objects
            .as_deref()
            .and_then(|objects| objects.raycast(player.position, direction, nearest, 0.0));
        if let Some(hit) = object_hit {
            nearest = hit.distance;
        }
        for (index, actor) in actors.actors.iter().enumerate() {
            if actor.health <= 0 || !actor.position.iter().all(|x| x.is_finite()) {
                continue;
            }
            let Some(definition) = definitions.entries.get(actor.definition_index) else {
                continue;
            };
            // The documented wall radius is a starting approximation for the
            // horizontal hit circle. Actor/projectile radii need verification.
            let radius = (definition.wall_radius as f32 / UNITS_PER_TILE).clamp(0.05, 2.0);
            let offset = [
                actor.position[0] - player.position[0],
                actor.position[1] - player.position[1],
            ];
            let projection = offset[0] * direction[0] + offset[1] * direction[1];
            let perpendicular_sq =
                (offset[0] * offset[0] + offset[1] * offset[1] - projection * projection).max(0.0);
            let radius_sq = radius * radius;
            if perpendicular_sq > radius_sq {
                continue;
            }
            let half = (radius_sq - perpendicular_sq).sqrt();
            let near = projection - half;
            let far = projection + half;
            if far < 0.0 {
                continue;
            }
            let distance = near.max(0.0);
            if distance < nearest {
                nearest = distance;
                selected = Some(index);
                object_hit = None;
            }
        }
        if let Some(hit) = object_hit {
            let damage = rocket
                .power
                .saturating_mul(i32::from(spec.projectile_count));
            if let Some(event) = objects.and_then(|objects| objects.apply_hit(hit.cell, damage)) {
                return FireResult::Fired(ShotImpact::Object {
                    cell: event.cell,
                    damage,
                    destroyed: event.destroyed,
                    distance: hit.distance,
                });
            }
        }
        if let Some(index) = selected {
            let actor = &mut actors.actors[index];
            let damage = rocket
                .power
                .saturating_mul(i32::from(spec.projectile_count));
            actor.health = actor.health.saturating_sub(damage).max(0);
            FireResult::Fired(ShotImpact::Actor {
                source_index: actor.source_index,
                damage,
                killed: actor.health == 0,
                distance: nearest,
            })
        } else if let Some((distance, index)) =
            wall_hit.filter(|(distance, _)| *distance <= MAX_RANGE)
        {
            FireResult::Fired(ShotImpact::Wall {
                wall_index: index,
                distance,
            })
        } else {
            FireResult::Fired(ShotImpact::Miss)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actors::ActorDefinitions;
    use crate::inventory::WeaponDefinition;
    use crate::map::Monster as MapActor;
    use crate::world::Player;

    fn setup() -> (
        Inventory,
        WeaponDefinitions,
        RocketDefinitions,
        ActorDefinitions,
        ActorState,
        World,
    ) {
        let weapon = WeaponDefinition {
            rocket_type: 0,
            reload_time: 128,
            vertical_shift: 0,
            projectile_height: 0,
            ammo_per_portion: 1,
            ammo_limit: 5,
            ammo_on_weapon_pickup: 1,
            projectile_count: 1,
        };
        let weapons = WeaponDefinitions([weapon; 8]);
        let rockets = RocketDefinitions::parse(b"[ROCKETS]\n2\n3d_MODEL=\nANIMATION=\n1 0 50 42 25 0\n3d_MODEL=arrow.3o\nANIMATION=a.ani\n1 0 50 42 50 0\n").unwrap();
        let definitions = ActorDefinitions::parse(
            b"[MONSTERS]\n2\nhero.car 100 100 10 5 100 0 0 0\njoker.car 52 190 10 5 100 20 5 16\n",
        )
        .unwrap();
        let placements = [
            MapActor {
                id: 101,
                position: [512, 0],
                angle: 0,
                flags: 7,
            },
            MapActor {
                id: 101,
                position: [1024, 0],
                angle: 0,
                flags: 7,
            },
        ];
        let actors = ActorState::from_placements(&placements, &definitions);
        let world = World {
            walls: vec![],
            player: Player {
                position: [0.0, 0.0],
                angle: 0.0,
            },
        };
        (
            Inventory::new(&weapons),
            weapons,
            rockets,
            definitions,
            actors,
            world,
        )
    }
    fn fire(
        combat: &mut Combat,
        inv: &mut Inventory,
        weapons: &WeaponDefinitions,
        rockets: &RocketDefinitions,
        definitions: &ActorDefinitions,
        actors: &mut ActorState,
        world: &World,
    ) -> FireResult {
        combat.fire(
            0,
            ShotContext {
                inventory: inv,
                weapons,
                rockets,
                actors,
                definitions,
                world,
                objects: None,
            },
            |_| true,
        )
    }
    fn wall(x: f32) -> Wall {
        Wall {
            a: [x, -1.0],
            b: [x, 1.0],
            texture: 1,
            blocks_movement: true,
            cell: None,
        }
    }
    #[test]
    fn instant_shot_hits_nearest_actor_consumes_ammo_and_obeys_cooldown() {
        let (mut inv, weapons, rockets, defs, mut actors, world) = setup();
        let mut combat = Combat::new();
        let result = fire(
            &mut combat,
            &mut inv,
            &weapons,
            &rockets,
            &defs,
            &mut actors,
            &world,
        );
        assert!(matches!(
            result,
            FireResult::Fired(ShotImpact::Actor {
                source_index: 0,
                damage: 25,
                killed: false,
                ..
            })
        ));
        assert_eq!(actors.actors[0].health, 75);
        assert_eq!(inv.ammo[0], 4);
        assert_eq!(
            fire(
                &mut combat,
                &mut inv,
                &weapons,
                &rockets,
                &defs,
                &mut actors,
                &world
            ),
            FireResult::CoolingDown
        );
        combat.tick(0.49);
        assert_eq!(
            fire(
                &mut combat,
                &mut inv,
                &weapons,
                &rockets,
                &defs,
                &mut actors,
                &world
            ),
            FireResult::CoolingDown
        );
        combat.tick(0.02);
        assert!(matches!(
            fire(
                &mut combat,
                &mut inv,
                &weapons,
                &rockets,
                &defs,
                &mut actors,
                &world
            ),
            FireResult::Fired(_)
        ));
    }
    #[test]
    fn walls_occlude_and_caller_can_mark_them_passable_for_shots() {
        let (mut inv, weapons, rockets, defs, mut actors, mut world) = setup();
        world.walls.push(wall(1.0));
        let mut combat = Combat::new();
        assert!(matches!(
            fire(
                &mut combat,
                &mut inv,
                &weapons,
                &rockets,
                &defs,
                &mut actors,
                &world
            ),
            FireResult::Fired(ShotImpact::Wall { wall_index: 0, .. })
        ));
        assert_eq!(actors.actors[0].health, 100);
        combat.tick(1.0);
        assert!(matches!(
            combat.fire(
                0,
                ShotContext {
                    inventory: &mut inv,
                    weapons: &weapons,
                    rockets: &rockets,
                    actors: &mut actors,
                    definitions: &defs,
                    world: &world,
                    objects: None,
                },
                |_| false
            ),
            FireResult::Fired(ShotImpact::Actor {
                source_index: 0,
                ..
            })
        ));
    }
    #[test]
    fn dead_targets_do_not_block_later_shots() {
        let (mut inv, weapons, rockets, defs, mut actors, world) = setup();
        actors.actors[0].health = 25;
        let mut combat = Combat::new();
        assert!(matches!(
            fire(
                &mut combat,
                &mut inv,
                &weapons,
                &rockets,
                &defs,
                &mut actors,
                &world
            ),
            FireResult::Fired(ShotImpact::Actor {
                killed: true,
                source_index: 0,
                ..
            })
        ));
        assert_eq!(actors.actors[0].health, 0);
        combat.tick(1.0);
        assert!(matches!(
            fire(
                &mut combat,
                &mut inv,
                &weapons,
                &rockets,
                &defs,
                &mut actors,
                &world
            ),
            FireResult::Fired(ShotImpact::Actor {
                source_index: 1,
                ..
            })
        ));
    }
    #[test]
    fn invalid_or_unsupported_shots_do_not_consume_ammo() {
        let (mut inv, mut weapons, rockets, defs, mut actors, world) = setup();
        let mut combat = Combat::new();
        assert_eq!(
            combat.fire(
                8,
                ShotContext {
                    inventory: &mut inv,
                    weapons: &weapons,
                    rockets: &rockets,
                    actors: &mut actors,
                    definitions: &defs,
                    world: &world,
                    objects: None,
                },
                |_| true
            ),
            FireResult::InvalidWeapon
        );
        assert_eq!(
            combat.fire(
                1,
                ShotContext {
                    inventory: &mut inv,
                    weapons: &weapons,
                    rockets: &rockets,
                    actors: &mut actors,
                    definitions: &defs,
                    world: &world,
                    objects: None,
                },
                |_| true
            ),
            FireResult::NotOwned
        );
        weapons.0[0].rocket_type = 1;
        assert_eq!(
            fire(
                &mut combat,
                &mut inv,
                &weapons,
                &rockets,
                &defs,
                &mut actors,
                &world
            ),
            FireResult::Unsupported
        );
        assert_eq!(inv.ammo[0], 5);
        weapons.0[0].rocket_type = 0;
        inv.ammo[0] = 0;
        assert_eq!(
            fire(
                &mut combat,
                &mut inv,
                &weapons,
                &rockets,
                &defs,
                &mut actors,
                &world
            ),
            FireResult::OutOfAmmo
        );
    }
    #[test]
    fn rocket_parser_rejects_incomplete_or_invalid_rows() {
        assert!(RocketDefinitions::parse(b"[ROCKETS]\n1\n1 2 3 4 5\n").is_err());
        assert!(RocketDefinitions::parse(b"[ROCKETS]\n1\n3d_MODEL=\n1 2 3 bad 5\n").is_err());
        assert!(RocketDefinitions::parse(b"[ROCKETS]\n2\n3d_MODEL=\n1 2 3 4 5\n").is_err());
    }
}
