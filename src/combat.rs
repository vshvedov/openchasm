//! Window- and network-independent provisional instant-shot combat.
//!
//! Rocket definitions are data driven. Instant shots use one aimed ray with
//! aggregated pellets; combat timing and hit shapes remain provisional.
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

/// Nearest physical target on an enemy instant shot's horizontal ray.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnemyShotTarget {
    Player,
    Actor { source_index: usize },
    Object { cell: [usize; 2] },
    Wall { wall_index: usize },
}
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EnemyShotHit {
    pub target: EnemyShotTarget,
    pub distance: f32,
}
pub struct EnemyShotContext<'a> {
    pub world: &'a World,
    pub actors: &'a ActorState,
    pub definitions: &'a ActorDefinitions,
    pub objects: Option<&'a ObjectCombat>,
}
/// Trace up to 64 tiles in the direction of the supplied aim point, ignoring
/// the shooter. Stable ties prefer walls, then objects, then actors, then player.
pub fn enemy_hitscan<F>(
    origin: [f32; 2],
    aim: [f32; 2],
    shooter: usize,
    context: EnemyShotContext<'_>,
    blocks_shots: F,
) -> Option<EnemyShotHit>
where
    F: Fn(&Wall) -> bool,
{
    enemy_hitscan_filtered(origin, aim, shooter, context, blocks_shots, |_, _| true)
}

/// Variant that filters each object candidate before nearest-hit selection.
pub fn enemy_hitscan_filtered<F>(
    origin: [f32; 2],
    aim: [f32; 2],
    shooter: usize,
    context: EnemyShotContext<'_>,
    blocks_shots: F,
    accepts_object: impl Fn([usize; 2], u8) -> bool,
) -> Option<EnemyShotHit>
where
    F: Fn(&Wall) -> bool,
{
    if !origin.iter().chain(aim.iter()).all(|v| v.is_finite()) {
        return None;
    }
    let delta = [aim[0] - origin[0], aim[1] - origin[1]];
    let length = delta[0].hypot(delta[1]);
    if length < 1e-6 || !length.is_finite() {
        return None;
    }
    let direction = [delta[0] / length, delta[1] / length];
    let mut nearest = MAX_RANGE;
    let mut selected = None;
    let cross = |a: [f32; 2], b: [f32; 2]| a[0] * b[1] - a[1] * b[0];
    for (index, wall) in context
        .world
        .walls
        .iter()
        .enumerate()
        .filter(|(_, wall)| blocks_shots(wall))
    {
        if !wall.a.iter().chain(wall.b.iter()).all(|v| v.is_finite()) {
            continue;
        }
        let edge = [wall.b[0] - wall.a[0], wall.b[1] - wall.a[1]];
        let denominator = cross(direction, edge);
        if denominator.abs() < 1e-5 {
            continue;
        }
        let offset = [wall.a[0] - origin[0], wall.a[1] - origin[1]];
        let distance = cross(offset, edge) / denominator;
        let along = cross(offset, direction) / denominator;
        if distance > 0.001 && distance < nearest && (0.0..=1.0).contains(&along) {
            nearest = distance;
            selected = Some(EnemyShotTarget::Wall { wall_index: index });
        }
    }
    if let Some(hit) = context.objects.and_then(|objects| {
        objects.raycast_filtered(origin, direction, nearest, 0.0, accepts_object)
    }) {
        if hit.distance < nearest {
            nearest = hit.distance;
            selected = Some(EnemyShotTarget::Object { cell: hit.cell });
        }
    }
    let circle_distance = |center: [f32; 2], radius: f32| {
        if !center.iter().all(|v| v.is_finite()) {
            return None;
        }
        let offset = [center[0] - origin[0], center[1] - origin[1]];
        let projection = offset[0] * direction[0] + offset[1] * direction[1];
        let perpendicular =
            (offset[0] * offset[0] + offset[1] * offset[1] - projection * projection).max(0.0);
        if perpendicular > radius * radius {
            return None;
        }
        let half = (radius * radius - perpendicular).sqrt();
        if projection + half < 0.0 {
            return None;
        }
        Some((projection - half).max(0.0))
    };
    for actor in &context.actors.actors {
        if actor.source_index == shooter || actor.health <= 0 {
            continue;
        }
        let Some(definition) = context.definitions.entries.get(actor.definition_index) else {
            continue;
        };
        let radius = (definition.wall_radius as f32 / UNITS_PER_TILE).clamp(0.05, 2.0);
        if let Some(distance) = circle_distance(actor.position, radius) {
            if distance < nearest {
                nearest = distance;
                selected = Some(EnemyShotTarget::Actor {
                    source_index: actor.source_index,
                });
            }
        }
    }
    if let Some(distance) = circle_distance(context.world.player.position, 0.18) {
        if distance < nearest {
            nearest = distance;
            selected = Some(EnemyShotTarget::Player);
        }
    }
    selected.map(|target| EnemyShotHit {
        target,
        distance: nearest,
    })
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ShotPlane {
    Floor,
    Ceiling,
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
    /// Contact with the host-supplied provisional room plane. Distance is the
    /// horizontal ray distance, as for wall, object, and actor hits.
    Plane {
        plane: ShotPlane,
        distance: f32,
    },
    Miss,
    /// A mine placement was requested; the session validates and plants it.
    MinePlaced,
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

/// Camera ray in map tiles; pitch is positive upward. Room planes are the
/// current provisional flat floor/ceiling used by the session.
#[derive(Debug, Clone, Copy)]
pub struct ShotAim {
    pub eye: f32,
    pub pitch: f32,
    pub floor: f32,
    pub ceiling: f32,
}

/// Clip an XY intersection interval against a vertical cylinder slab.
pub fn clip_aim_interval(
    near: f32,
    far: f32,
    eye: f32,
    slope: f32,
    bounds: Option<(f32, f32)>,
) -> Option<f32> {
    let Some((bottom, top)) = bounds else {
        return Some(near);
    }; // unknown stays blocking
    if !bottom.is_finite() || !top.is_finite() || bottom > top {
        return Some(near);
    }
    if slope.abs() < 1e-7 {
        return (eye >= bottom && eye <= top).then_some(near);
    }
    let a = (bottom - eye) / slope;
    let b = (top - eye) / slope;
    let enter = near.max(a.min(b));
    (enter <= far.min(a.max(b))).then_some(enter)
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Combat {
    cooldown: f32,
}
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CombatSnapshot {
    pub cooldown: f32,
}
impl Combat {
    pub fn snapshot(&self) -> CombatSnapshot {
        CombatSnapshot {
            cooldown: self.cooldown,
        }
    }
    pub fn restore(
        &mut self,
        value: CombatSnapshot,
        weapons: &WeaponDefinitions,
    ) -> Result<(), &'static str> {
        let max = weapons
            .0
            .iter()
            .map(|spec| (spec.reload_time as f32 / RELOAD_UNITS_PER_SECOND).max(MIN_COOLDOWN))
            .fold(0.0_f32, f32::max);
        if !value.cooldown.is_finite() || value.cooldown < 0.0 || value.cooldown > max {
            return Err("invalid combat cooldown");
        }
        self.cooldown = value.cooldown;
        Ok(())
    }
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

    /// Consume one mine and start its cooldown before session placement. The
    /// session must refund ammo and cancel cooldown if placement fails.
    pub fn plant_mine(
        &mut self,
        inventory: &mut Inventory,
        weapons: &WeaponDefinitions,
        position: [f32; 2],
    ) -> FireResult {
        const SLOT: usize = 6;
        if !inventory.weapons[SLOT] {
            return FireResult::NotOwned;
        }
        if self.cooldown > 0.0 {
            return FireResult::CoolingDown;
        }
        if inventory.ammo[SLOT] == 0 {
            return FireResult::OutOfAmmo;
        }
        if !position.iter().all(|v| v.is_finite()) {
            return FireResult::InvalidShooter;
        }
        inventory.ammo[SLOT] -= 1;
        self.cooldown =
            (weapons.0[SLOT].reload_time as f32 / RELOAD_UNITS_PER_SECOND).max(MIN_COOLDOWN);
        FireResult::Fired(ShotImpact::MinePlaced)
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
        self.fire_with_explosions_filtered(weapon, context, blocks_shots, explosions, |_, _| true)
    }

    /// Variant filtering object candidates before nearest-hit selection.
    pub fn fire_with_explosions_filtered<F>(
        &mut self,
        weapon: usize,
        context: ShotContext<'_>,
        blocks_shots: F,
        explosions: bool,
        accepts_object: impl Fn([usize; 2], u8) -> bool,
    ) -> FireResult
    where
        F: Fn(&Wall) -> bool,
    {
        self.fire_impl(
            weapon,
            context,
            |wall, _| blocks_shots(wall),
            explosions,
            None,
            |cell, raw, near, _| accepts_object(cell, raw).then_some(near),
        )
    }

    /// Resolve a pitched hitscan; physical launches are returned to the host.
    pub fn fire_with_aim(
        &mut self,
        weapon: usize,
        context: ShotContext<'_>,
        explosions: bool,
        aim: ShotAim,
        blocks_shots: impl Fn(&Wall, f32) -> bool,
        object_bounds: impl Fn([usize; 2], u8) -> Option<(f32, f32)>,
    ) -> FireResult {
        if !aim.eye.is_finite()
            || !aim.pitch.is_finite()
            || !aim.floor.is_finite()
            || !aim.ceiling.is_finite()
            || aim.floor > aim.ceiling
            || aim.pitch.abs() >= std::f32::consts::FRAC_PI_2
        {
            return FireResult::InvalidShooter;
        }
        let slope = aim.pitch.tan();
        self.fire_impl(
            weapon,
            context,
            blocks_shots,
            explosions,
            Some(aim),
            |cell, raw, near, far| {
                clip_aim_interval(near, far, aim.eye, slope, object_bounds(cell, raw))
            },
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn fire_impl(
        &mut self,
        weapon: usize,
        context: ShotContext<'_>,
        blocks_shots: impl Fn(&Wall, f32) -> bool,
        explosions: bool,
        aim: Option<ShotAim>,
        contact_object: impl Fn([usize; 2], u8, f32, f32) -> Option<f32>,
    ) -> FireResult {
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
        if weapon == 6 {
            return FireResult::Unsupported;
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
            .filter(|(distance, index, _)| {
                blocks_shots(
                    &world.walls[*index],
                    aim.map_or(1.0, |a| a.eye + distance * a.pitch.tan()),
                )
            })
            .map(|(distance, index, _)| (distance, index))
            .min_by(|a, b| a.0.total_cmp(&b.0));
        let plane_hit = aim.and_then(|a| {
            let slope = a.pitch.tan();
            let (plane, distance) = if slope > 1e-7 {
                (ShotPlane::Ceiling, (a.ceiling - a.eye) / slope)
            } else if slope < -1e-7 {
                (ShotPlane::Floor, (a.floor - a.eye) / slope)
            } else {
                return None;
            };
            (distance.is_finite() && (0.0..=MAX_RANGE).contains(&distance))
                .then_some((plane, distance))
        });
        let room_limit = aim.map_or(MAX_RANGE, |a| {
            let slope = a.pitch.tan();
            if slope > 1e-7 {
                ((a.ceiling - a.eye) / slope).clamp(0.0, MAX_RANGE)
            } else if slope < -1e-7 {
                ((a.floor - a.eye) / slope).clamp(0.0, MAX_RANGE)
            } else {
                MAX_RANGE
            }
        });
        let mut nearest = wall_hit.map_or(room_limit, |(distance, _)| distance.min(room_limit));
        let mut selected = None;
        let mut object_hit = objects.as_deref().and_then(|objects| {
            objects.raycast_intervals(player.position, direction, nearest, 0.0, contact_object)
        });
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
            let Some(distance) = aim.map_or(Some(near.max(0.0)), |a| {
                clip_aim_interval(
                    near.max(0.0),
                    far.min(nearest),
                    a.eye,
                    a.pitch.tan(),
                    Some((0.0, 2.0)),
                )
            }) else {
                continue;
            };
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
            wall_hit.filter(|(distance, _)| *distance <= nearest)
        {
            FireResult::Fired(ShotImpact::Wall {
                wall_index: index,
                distance,
            })
        } else if let Some((plane, distance)) =
            plane_hit.filter(|(_, distance)| *distance <= nearest)
        {
            FireResult::Fired(ShotImpact::Plane { plane, distance })
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
    fn aimed_shots_report_room_planes_only_when_nearest_and_in_range() {
        let (mut inventory, weapons, rockets, definitions, mut actors, mut world) = setup();
        actors.actors.clear();
        let mut shoot = |pitch: f32, world: &World| {
            Combat::new().fire_with_aim(
                0,
                ShotContext {
                    inventory: &mut inventory,
                    weapons: &weapons,
                    rockets: &rockets,
                    actors: &mut actors,
                    definitions: &definitions,
                    world,
                    objects: None,
                },
                false,
                ShotAim {
                    eye: 1.0,
                    pitch,
                    floor: 0.0,
                    ceiling: 2.0,
                },
                |_, _| true,
                |_, _| None,
            )
        };
        for (pitch, expected) in [
            (-std::f32::consts::FRAC_PI_4, ShotPlane::Floor),
            (std::f32::consts::FRAC_PI_4, ShotPlane::Ceiling),
        ] {
            assert!(
                matches!(shoot(pitch, &world), FireResult::Fired(ShotImpact::Plane { plane, distance }) if plane == expected && (distance - 1.0).abs() < 1e-5)
            );
        }
        assert_eq!(shoot(0.0, &world), FireResult::Fired(ShotImpact::Miss));
        assert_eq!(shoot(0.001, &world), FireResult::Fired(ShotImpact::Miss));
        world.walls.push(wall(0.5));
        assert!(
            matches!(shoot(-std::f32::consts::FRAC_PI_4, &world), FireResult::Fired(ShotImpact::Wall { distance, .. }) if (distance - 0.5).abs() < 1e-5)
        );
    }

    #[test]
    fn nearer_actor_wins_over_floor_plane() {
        let (mut inventory, weapons, rockets, definitions, mut actors, world) = setup();
        let result = Combat::new().fire_with_aim(
            0,
            ShotContext {
                inventory: &mut inventory,
                weapons: &weapons,
                rockets: &rockets,
                actors: &mut actors,
                definitions: &definitions,
                world: &world,
                objects: None,
            },
            false,
            ShotAim {
                eye: 1.0,
                pitch: -(1.0_f32 / 3.0).atan(),
                floor: 0.0,
                ceiling: 2.0,
            },
            |_, _| true,
            |_, _| None,
        );
        assert!(matches!(
            result,
            FireResult::Fired(ShotImpact::Actor {
                source_index: 0,
                ..
            })
        ));
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
