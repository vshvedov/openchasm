//! Provisional server-independent 2D projectile simulation.
//!
//! Motion is finite-lifetime and uses continuous swept circles against current
//! walls and actor/player hit circles. Collision reports events; the owning
//! session applies damage and effects. There is no vertical or splash physics.
use crate::actors::{ActorDefinitions, ActorState};
use crate::combat::RocketDefinitions;
use crate::map::UNITS_PER_TILE;
use crate::object_combat::ObjectCombat;
use crate::world::{Wall, World};
use std::collections::BTreeSet;

const DEFAULT_SPEED: f32 = 10.0; // tiles per second
const FAST_SPEED: f32 = 20.0;
const DEFAULT_LIFETIME: f32 = 10.0; // seconds: provisional
const PLAYER_RADIUS: f32 = 0.18; // matches the preview movement collision
const MAX_PROJECTILES: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Owner {
    Player,
    /// Stable map placement index; the emitting actor is ignored on impact.
    Enemy {
        source_index: usize,
    },
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Launch {
    pub owner: Owner,
    pub rocket_id: usize,
    pub origin: [f32; 2],
    /// Any finite, nonzero vector; the projectile uses its direction only.
    pub direction: [f32; 2],
}

#[derive(Debug, Clone, PartialEq)]
pub struct Projectile {
    pub id: u64,
    pub owner: Owner,
    pub rocket_id: usize,
    pub position: [f32; 2],
    pub velocity: [f32; 2],
    pub radius: f32,
    pub damage: i32,
    pub remaining: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpawnError {
    UnknownRocket,
    InstantRocket,
    InvalidRocket,
    InvalidLaunch,
    Capacity,
    IdExhausted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImpactTarget {
    Wall { wall_index: usize },
    Actor { source_index: usize },
    Player,
    Object { cell: [usize; 2] },
    Expired,
}
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Impact {
    pub projectile_id: u64,
    pub owner: Owner,
    pub rocket_id: usize,
    pub target: ImpactTarget,
    pub position: [f32; 2],
    pub damage: i32,
    /// Seconds after the beginning of the supplied tick.
    pub time: f32,
}

/// State sampled when tracing the next projectile event.
pub struct TraceContext<'a> {
    pub world: &'a World,
    pub actors: &'a ActorState,
    pub definitions: &'a ActorDefinitions,
    pub objects: Option<&'a ObjectCombat>,
}

#[derive(Debug, Default, Clone)]
pub struct ProjectileSystem {
    next_id: u64,
    projectiles: Vec<Projectile>,
}

fn dot(a: [f32; 2], b: [f32; 2]) -> f32 {
    a[0] * b[0] + a[1] * b[1]
}
fn sub(a: [f32; 2], b: [f32; 2]) -> [f32; 2] {
    [a[0] - b[0], a[1] - b[1]]
}
fn add_scaled(a: [f32; 2], b: [f32; 2], t: f32) -> [f32; 2] {
    [a[0] + b[0] * t, a[1] + b[1] * t]
}

/// Earliest time a moving point enters a circle, over [0, 1] of displacement.
fn sweep_circle(origin: [f32; 2], delta: [f32; 2], center: [f32; 2], radius: f32) -> Option<f32> {
    let offset = sub(origin, center);
    let c = dot(offset, offset) - radius * radius;
    if c <= 0.0 {
        return Some(0.0);
    }
    let speed_sq = dot(delta, delta);
    if speed_sq < 1e-12 {
        return None;
    }
    let b = dot(offset, delta);
    let discriminant = b * b - speed_sq * c;
    if discriminant < 0.0 {
        return None;
    }
    let t = (-b - discriminant.sqrt()) / speed_sq;
    (0.0..=1.0).contains(&t).then_some(t)
}

/// Swept circle against the finite wall segment, including rounded endpoints.
fn sweep_wall(origin: [f32; 2], delta: [f32; 2], radius: f32, wall: &Wall) -> Option<f32> {
    let edge = sub(wall.b, wall.a);
    let length_sq = dot(edge, edge);
    if length_sq < 1e-12 {
        return sweep_circle(origin, delta, wall.a, radius);
    }
    let length = length_sq.sqrt();
    let tangent = [edge[0] / length, edge[1] / length];
    let normal = [-tangent[1], tangent[0]];
    let relative = sub(origin, wall.a);
    let along = dot(relative, tangent).clamp(0.0, length);
    let closest = add_scaled(wall.a, tangent, along);
    let separation = sub(origin, closest);
    if dot(separation, separation) <= radius * radius {
        return Some(0.0);
    }
    let mut hit: Option<f32> = None;
    let normal_speed = dot(delta, normal);
    if normal_speed.abs() > 1e-8 {
        let start = dot(relative, normal);
        for side in [-1.0, 1.0] {
            let t = (side * radius - start) / normal_speed;
            if !(0.0..=1.0).contains(&t) {
                continue;
            }
            let projected = dot(add_scaled(relative, delta, t), tangent);
            if (0.0..=length).contains(&projected) && hit.is_none_or(|h| t < h) {
                hit = Some(t);
            }
        }
    }
    for endpoint in [wall.a, wall.b] {
        if let Some(t) = sweep_circle(origin, delta, endpoint, radius) {
            if hit.is_none_or(|h| t < h) {
                hit = Some(t);
            }
        }
    }
    hit
}

impl ProjectileSystem {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn active(&self) -> &[Projectile] {
        &self.projectiles
    }

    /// Restore an impacted projectile from a pre-tick snapshot after the host
    /// rejects its effect transaction. Existing projectiles remain unchanged.
    pub fn restore_impact(&mut self, before: &Self, id: u64) -> bool {
        if self
            .projectiles
            .iter()
            .any(|projectile| projectile.id == id)
        {
            return false;
        }
        let Some(projectile) = before
            .projectiles
            .iter()
            .find(|projectile| projectile.id == id)
        else {
            return false;
        };
        self.projectiles.push(projectile.clone());
        true
    }

    /// Predict the next impact from current state without changing it. The
    /// host must retrace after committing any world or actor state change.
    pub fn trace_next<F>(
        &self,
        horizon: f32,
        context: TraceContext<'_>,
        blocks_shots: F,
        frozen: &BTreeSet<u64>,
    ) -> Option<Impact>
    where
        F: Fn(&Wall) -> bool,
    {
        if !horizon.is_finite() || horizon < 0.0 {
            return None;
        }
        let mut probe = self.clone();
        probe
            .projectiles
            .retain(|projectile| !frozen.contains(&projectile.id));
        // A tiny positive horizon allows same-time overlaps and expiry to be
        // considered at the end of the tick without moving past that end.
        let probe_horizon = if horizon == 0.0 {
            f32::MIN_POSITIVE
        } else {
            horizon
        };
        probe
            .tick_with_objects(
                probe_horizon,
                context.world,
                context.actors,
                context.definitions,
                context.objects,
                blocks_shots,
            )
            .into_iter()
            .find(|impact| horizon > 0.0 || impact.time == 0.0)
    }
    /// Advance all unfrozen projectiles by a previously traced safe interval.
    /// On invalid input no projectile is modified.
    pub fn advance_traced(&mut self, dt: f32, frozen: &BTreeSet<u64>) -> bool {
        if !dt.is_finite() || dt < 0.0 {
            return false;
        }
        if self
            .projectiles
            .iter()
            .filter(|p| !frozen.contains(&p.id))
            .any(|p| {
                dt > p.remaining + 1e-6
                    || !p
                        .position
                        .iter()
                        .zip(p.velocity)
                        .all(|(x, v)| (x + v * dt).is_finite())
            })
        {
            return false;
        }
        for p in self
            .projectiles
            .iter_mut()
            .filter(|p| !frozen.contains(&p.id))
        {
            p.position = add_scaled(p.position, p.velocity, dt);
            p.remaining = (p.remaining - dt).max(0.0);
        }
        true
    }
    /// Remove one resolved candidate without affecting the ID generator.
    pub fn take(&mut self, id: u64) -> Option<Projectile> {
        let index = self.projectiles.iter().position(|p| p.id == id)?;
        Some(self.projectiles.remove(index))
    }

    /// Only non-instant, positive-power rocket definitions can be spawned.
    pub fn spawn(
        &mut self,
        launch: Launch,
        rockets: &RocketDefinitions,
    ) -> Result<u64, SpawnError> {
        let rocket = rockets
            .0
            .get(launch.rocket_id)
            .ok_or(SpawnError::UnknownRocket)?;
        if rocket.model_name.is_empty() {
            return Err(SpawnError::InstantRocket);
        }
        let Some(&collision_radius) = rocket.raw_fields.get(3) else {
            return Err(SpawnError::InvalidRocket);
        };
        if rocket.power <= 0 || collision_radius < 0 {
            return Err(SpawnError::InvalidRocket);
        }
        if !launch
            .origin
            .iter()
            .chain(launch.direction.iter())
            .all(|x| x.is_finite())
        {
            return Err(SpawnError::InvalidLaunch);
        }
        let magnitude = launch.direction[0].hypot(launch.direction[1]);
        if !magnitude.is_finite() || magnitude < 1e-6 {
            return Err(SpawnError::InvalidLaunch);
        }
        if self.projectiles.len() >= MAX_PROJECTILES {
            return Err(SpawnError::Capacity);
        }
        let next = self.next_id.checked_add(1).ok_or(SpawnError::IdExhausted)?;
        let id = self.next_id;
        self.next_id = next;
        // Fast is the eleventh numeric rocket field. Short synthetic/custom
        // rows retain the ordinary speed until that field is supplied.
        let speed = if rocket.raw_fields.get(10).copied().unwrap_or(0) != 0 {
            FAST_SPEED
        } else {
            DEFAULT_SPEED
        };
        self.projectiles.push(Projectile {
            id,
            owner: launch.owner,
            rocket_id: launch.rocket_id,
            position: launch.origin,
            velocity: [
                speed * launch.direction[0] / magnitude,
                speed * launch.direction[1] / magnitude,
            ],
            radius: (collision_radius as f32 / UNITS_PER_TILE).clamp(0.0, 2.0),
            damage: rocket.power,
            remaining: DEFAULT_LIFETIME,
        });
        Ok(id)
    }

    /// Conservative opt-in path: no gravity, bounce, or homing physics.
    pub fn spawn_straight(
        &mut self,
        launch: Launch,
        rockets: &RocketDefinitions,
    ) -> Result<u64, SpawnError> {
        let rocket = rockets
            .0
            .get(launch.rocket_id)
            .ok_or(SpawnError::UnknownRocket)?;
        if [1, 5, 9]
            .into_iter()
            .any(|index| rocket.raw_fields.get(index).copied().unwrap_or(0) != 0)
        {
            return Err(SpawnError::InvalidRocket);
        }
        self.spawn(launch, rockets)
    }

    /// Advance using the current level and actor state. The wall predicate is
    /// the caller's material-specific shooting collision rule. The returned
    /// impacts are sorted by their time in the tick and then projectile ID.
    pub fn tick<F>(
        &mut self,
        dt: f32,
        world: &World,
        actors: &ActorState,
        definitions: &ActorDefinitions,
        blocks_shots: F,
    ) -> Vec<Impact>
    where
        F: Fn(&Wall) -> bool,
    {
        self.tick_with_objects(dt, world, actors, definitions, None, blocks_shots)
    }
    /// Like tick, also colliding with active shootable level objects.
    pub fn tick_with_objects<F>(
        &mut self,
        dt: f32,
        world: &World,
        actors: &ActorState,
        definitions: &ActorDefinitions,
        objects: Option<&ObjectCombat>,
        blocks_shots: F,
    ) -> Vec<Impact>
    where
        F: Fn(&Wall) -> bool,
    {
        if !dt.is_finite() || dt <= 0.0 {
            return Vec::new();
        }
        let mut events = Vec::new();
        self.projectiles.retain_mut(|projectile| {
            let duration = dt.min(projectile.remaining);
            let origin = projectile.position;
            let delta = [projectile.velocity[0] * duration, projectile.velocity[1] * duration];
            let mut nearest: Option<(f32, ImpactTarget)> = None;
            for (index, wall) in world.walls.iter().enumerate().filter(|(_, wall)| blocks_shots(wall)) {
                if !wall.a.iter().chain(wall.b.iter()).all(|x| x.is_finite()) { continue; }
                if let Some(t) = sweep_wall(origin, delta, projectile.radius, wall) {
                    if nearest.is_none_or(|(best, _)| t < best) { nearest = Some((t, ImpactTarget::Wall { wall_index: index })); }
                }
            }
            let length = delta[0].hypot(delta[1]);
            if let Some(hit) = objects.and_then(|objects| objects.raycast(origin, projectile.velocity, length, projectile.radius)) {
                let t = if length > 1e-8 { hit.distance / length } else { 0.0 };
                if nearest.is_none_or(|(best, _)| t < best) { nearest = Some((t, ImpactTarget::Object { cell: hit.cell })); }
            }
            for actor in &actors.actors {
                if actor.health <= 0 || !actor.position.iter().all(|x| x.is_finite()) { continue; }
                if matches!(projectile.owner, Owner::Enemy { source_index } if source_index == actor.source_index) { continue; }
                let Some(definition) = definitions.entries.get(actor.definition_index) else { continue; };
                let radius = (definition.wall_radius as f32 / UNITS_PER_TILE).clamp(0.05, 2.0) + projectile.radius;
                if let Some(t) = sweep_circle(origin, delta, actor.position, radius) {
                    if nearest.is_none_or(|(best, _)| t < best) { nearest = Some((t, ImpactTarget::Actor { source_index: actor.source_index })); }
                }
            }
            if matches!(projectile.owner, Owner::Enemy { .. }) && world.player.position.iter().all(|x| x.is_finite()) {
                if let Some(t) = sweep_circle(origin, delta, world.player.position, PLAYER_RADIUS + projectile.radius) {
                    if nearest.is_none_or(|(best, _)| t < best) { nearest = Some((t, ImpactTarget::Player)); }
                }
            }
            if let Some((t, target)) = nearest {
                events.push(Impact { projectile_id: projectile.id, owner: projectile.owner, rocket_id: projectile.rocket_id,
                    target, position: add_scaled(origin, delta, t), damage: projectile.damage, time: duration * t });
                return false;
            }
            projectile.position = add_scaled(origin, delta, 1.0);
            projectile.remaining = (projectile.remaining - duration).max(0.0);
            if projectile.remaining <= 0.0 {
                events.push(Impact { projectile_id: projectile.id, owner: projectile.owner, rocket_id: projectile.rocket_id,
                    target: ImpactTarget::Expired, position: projectile.position, damage: 0, time: duration });
                false
            } else { true }
        });
        events.sort_by(|a, b| {
            a.time
                .total_cmp(&b.time)
                .then(a.projectile_id.cmp(&b.projectile_id))
        });
        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::map::Monster as MapActor;
    use crate::world::Player;

    fn setup() -> (RocketDefinitions, ActorDefinitions, ActorState, World) {
        let rockets = RocketDefinitions::parse(b"[ROCKETS]\n2\n3d_MODEL=arrow.3o\nANIMATION=a.ani\n1 0 120 42 40 0\n3d_MODEL=\nANIMATION=\n1 0 50 42 25 0\n").unwrap();
        let defs = ActorDefinitions::parse(
            b"[MONSTERS]\n2\nhero.car 100 100 10 5 100 0 0 0\njoker.car 52 190 10 5 100 20 5 16\n",
        )
        .unwrap();
        let map_actor = MapActor {
            id: 101,
            position: [512, 0],
            angle: 0,
            flags: 7,
        };
        let actors = ActorState::from_placements(&[map_actor], &defs);
        let world = World {
            player: Player {
                position: [0.0, 0.0],
                angle: 0.0,
            },
            walls: Vec::new(),
        };
        (rockets, defs, actors, world)
    }
    fn launch(owner: Owner, origin: [f32; 2], direction: [f32; 2]) -> Launch {
        Launch {
            owner,
            rocket_id: 0,
            origin,
            direction,
        }
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
    fn rejects_instant_invalid_and_unknown_rockets_without_spawning() {
        let (rockets, ..) = setup();
        let mut system = ProjectileSystem::new();
        let mut shot = launch(Owner::Player, [0., 0.], [1., 0.]);
        shot.rocket_id = 1;
        assert_eq!(system.spawn(shot, &rockets), Err(SpawnError::InstantRocket));
        shot.rocket_id = 2;
        assert_eq!(system.spawn(shot, &rockets), Err(SpawnError::UnknownRocket));
        shot.rocket_id = 0;
        shot.direction = [0., 0.];
        assert_eq!(system.spawn(shot, &rockets), Err(SpawnError::InvalidLaunch));
        shot.direction = [f32::NAN, 0.];
        assert_eq!(system.spawn(shot, &rockets), Err(SpawnError::InvalidLaunch));
        assert!(system.active().is_empty());
    }
    #[test]
    fn fast_flag_selects_the_authored_projectile_speed() {
        let (mut rockets, ..) = setup();
        rockets.0[0].raw_fields.resize(11, 0);
        let mut system = ProjectileSystem::new();
        system
            .spawn(launch(Owner::Player, [0., 0.], [0., 2.]), &rockets)
            .unwrap();
        assert_eq!(system.active()[0].velocity, [0., DEFAULT_SPEED]);
        rockets.0[0].raw_fields[10] = 1;
        system
            .spawn(launch(Owner::Player, [0., 0.], [0., 2.]), &rockets)
            .unwrap();
        assert_eq!(system.active()[1].velocity, [0., FAST_SPEED]);
    }
    #[test]
    fn moves_over_time_then_hits_nearest_actor() {
        let (rockets, defs, actors, world) = setup();
        let mut system = ProjectileSystem::new();
        system
            .spawn(launch(Owner::Player, [0., 0.], [2., 0.]), &rockets)
            .unwrap();
        assert!(system
            .tick(0.1, &world, &actors, &defs, |_| true)
            .is_empty());
        assert!((system.active()[0].position[0] - 1.0).abs() < 1e-5);
        let event = system.tick(0.5, &world, &actors, &defs, |_| true)[0];
        assert_eq!(event.target, ImpactTarget::Actor { source_index: 0 });
        assert_eq!(event.damage, 40);
        assert!(event.position[0] < 2.0);
        assert_eq!(actors.actors[0].health, 100, "caller applies event damage");
        assert!(system.active().is_empty());
    }
    #[test]
    fn long_tick_does_not_tunnel_through_wall_and_respects_shot_material() {
        let (rockets, defs, actors, mut world) = setup();
        world.walls.push(wall(1.0));
        let mut system = ProjectileSystem::new();
        system
            .spawn(launch(Owner::Player, [0., 0.], [1., 0.]), &rockets)
            .unwrap();
        let impact = system.tick(2.0, &world, &actors, &defs, |_| true)[0];
        assert_eq!(impact.target, ImpactTarget::Wall { wall_index: 0 });
        assert!((impact.position[0] - (1.0 - 42.0 / 256.0)).abs() < 0.001);
        system
            .spawn(launch(Owner::Player, [0., 0.], [1., 0.]), &rockets)
            .unwrap();
        assert_eq!(
            system.tick(2.0, &world, &actors, &defs, |_| false)[0].target,
            ImpactTarget::Actor { source_index: 0 }
        );
    }
    #[test]
    fn enemy_shot_ignores_owner_and_hits_player() {
        let (rockets, defs, actors, world) = setup();
        let mut system = ProjectileSystem::new();
        system
            .spawn(
                launch(Owner::Enemy { source_index: 0 }, [2., 0.], [-1., 0.]),
                &rockets,
            )
            .unwrap();
        let hit = system.tick(1.0, &world, &actors, &defs, |_| true)[0];
        assert_eq!(hit.target, ImpactTarget::Player);
        assert_eq!(hit.owner, Owner::Enemy { source_index: 0 });
        assert!(hit.position[0] > 0.0);
    }
    #[test]
    fn lifetime_limits_travel_and_produces_expiry() {
        let (rockets, defs, mut actors, mut world) = setup();
        actors.actors.clear();
        world.player.position = [20., 20.];
        let mut system = ProjectileSystem::new();
        system
            .spawn(launch(Owner::Player, [0., 0.], [1., 0.]), &rockets)
            .unwrap();
        let event = system.tick(15.0, &world, &actors, &defs, |_| true)[0];
        assert_eq!(event.target, ImpactTarget::Expired);
        assert!((event.position[0] - 100.0).abs() < 0.001);
        assert_eq!(event.damage, 0);
        assert!(system.active().is_empty());
    }
    #[test]
    fn nearer_endpoints_and_event_order_are_respected() {
        let (rockets, defs, mut actors, mut world) = setup();
        actors.actors.clear();
        world.walls.push(Wall {
            a: [1.0, 0.1],
            b: [1.0, 1.0],
            texture: 1,
            blocks_movement: true,
            cell: None,
        });
        let mut system = ProjectileSystem::new();
        let farther = system
            .spawn(launch(Owner::Player, [0., 0.], [1., 0.]), &rockets)
            .unwrap();
        let nearer = system
            .spawn(launch(Owner::Player, [0.5, 0.], [1., 0.]), &rockets)
            .unwrap();
        let events = system.tick(1.0, &world, &actors, &defs, |_| true);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].projectile_id, nearer);
        assert_eq!(events[1].projectile_id, farther);
        assert!(events[0].time < events[1].time);
    }
    #[test]
    fn chronological_zero_time_ties_and_expiry_are_stable() {
        let (rockets, defs, actors, mut world) = setup();
        world.walls.push(wall(0.0));
        let mut system = ProjectileSystem::new();
        for _ in 0..2 {
            system
                .spawn(launch(Owner::Player, [0.0, 0.0], [1.0, 0.0]), &rockets)
                .unwrap();
        }
        for p in &mut system.projectiles {
            p.remaining = 0.0;
        }
        let frozen = BTreeSet::new();
        let first = system
            .trace_next(
                0.0,
                TraceContext {
                    world: &world,
                    actors: &actors,
                    definitions: &defs,
                    objects: None,
                },
                |_| true,
                &frozen,
            )
            .unwrap();
        assert_eq!(
            (first.projectile_id, first.target, first.time),
            (0, ImpactTarget::Wall { wall_index: 0 }, 0.0)
        );
        assert!(system.advance_traced(0.0, &frozen));
        system.take(first.projectile_id).unwrap();
        let second = system
            .trace_next(
                0.0,
                TraceContext {
                    world: &world,
                    actors: &actors,
                    definitions: &defs,
                    objects: None,
                },
                |_| true,
                &frozen,
            )
            .unwrap();
        assert_eq!(
            (second.projectile_id, second.target),
            (1, ImpactTarget::Wall { wall_index: 0 })
        );
    }
    #[test]
    fn traced_motion_preserves_distance_and_freezes_selected_ids() {
        let (rockets, ..) = setup();
        let mut system = ProjectileSystem::new();
        system
            .spawn(launch(Owner::Player, [0.0, 0.0], [1.0, 0.0]), &rockets)
            .unwrap();
        system
            .spawn(launch(Owner::Player, [0.0, 2.0], [1.0, 0.0]), &rockets)
            .unwrap();
        let frozen = BTreeSet::from([0]);
        assert!(system.advance_traced(0.01, &frozen));
        assert!(system.advance_traced(0.01, &frozen));
        assert_eq!(system.active()[0].position, [0.0, 0.0]);
        assert!((system.active()[1].position[0] - 0.2).abs() < 1e-6);
        let snapshot = system.clone();
        assert!(!system.advance_traced(f32::NAN, &frozen));
        assert_eq!(system.active(), snapshot.active());
    }
}
