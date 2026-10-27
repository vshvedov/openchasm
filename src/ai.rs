//! Provisional enemy behavior operating on the server-side actor state.
//!
//! The caller advances this once at 60 Hz. Visibility, navigation, attack range,
//! cooldown and collision are initial approximations, not original AI behavior.
//! AI emits melee and projectile launch events; the owning simulation resolves damage.
use crate::actors::{ActorDefinitions, ActorState};
use crate::combat::RocketDefinitions;
use crate::pickups::Difficulty;
use crate::world::{Wall, World};
use std::collections::BTreeMap;

const DT: f32 = 1.0 / 60.0;
const SIGHT_RANGE: f32 = 8.0;
const MELEE_RANGE: f32 = 0.8;
const ATTACK_COOLDOWN_TICKS: u16 = 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Inactive,
    Idle,
    Chasing,
    Attacking,
    Dead,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum EnemyAttackKind {
    Melee,
    /// A projectile is launched toward the target's position at attack time.
    /// A non-instant projectile must be advanced and collision-tested by the
    /// caller; do not apply its damage merely because this event was emitted.
    Ranged {
        rocket_id: usize,
        instant: bool,
        origin: [f32; 2],
        target: [f32; 2],
    },
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EnemyAttack {
    /// Stable source placement index from ActorState, not an actor array index.
    pub source_index: usize,
    pub damage: u16,
    pub kind: EnemyAttackKind,
}

#[derive(Debug, Clone, Copy)]
struct Brain {
    mode: Mode,
    last_seen: Option<[f32; 2]>,
    cooldown: u16,
}

/// Per-actor AI state remains separate from map definitions and rendering.
#[derive(Debug, Default, Clone)]
pub struct EnemyAi {
    brains: BTreeMap<usize, Brain>,
}

fn sub(a: [f32; 2], b: [f32; 2]) -> [f32; 2] {
    [a[0] - b[0], a[1] - b[1]]
}
fn dot(a: [f32; 2], b: [f32; 2]) -> f32 {
    a[0] * b[0] + a[1] * b[1]
}
fn cross(a: [f32; 2], b: [f32; 2]) -> f32 {
    a[0] * b[1] - a[1] * b[0]
}

/// Return false if an opaque level wall intersects the open sight segment.
/// Transparent walls and invisible barriers do not block visual contact.
fn line_of_sight(from: [f32; 2], to: [f32; 2], walls: &[Wall]) -> bool {
    let ray = sub(to, from);
    !walls.iter().any(|wall| {
        if !(1..=86).contains(&wall.texture) {
            return false;
        }
        let edge = sub(wall.b, wall.a);
        let divisor = cross(ray, edge);
        if divisor.abs() < 1e-8 {
            return false;
        }
        let offset = sub(wall.a, from);
        let t = cross(offset, edge) / divisor;
        let u = cross(offset, ray) / divisor;
        (1e-5..(1.0 - 1e-5)).contains(&t) && (0.0..=1.0).contains(&u)
    })
}

/// Sight remains independent of the material-specific projectile predicate.
fn shot_clear(
    from: [f32; 2],
    to: [f32; 2],
    walls: &[Wall],
    blocks_shots: &dyn Fn(&Wall) -> bool,
) -> bool {
    let ray = sub(to, from);
    !walls.iter().any(|wall| {
        if !blocks_shots(wall) {
            return false;
        }
        let edge = sub(wall.b, wall.a);
        let divisor = cross(ray, edge);
        if divisor.abs() < 1e-8 {
            return false;
        }
        let offset = sub(wall.a, from);
        let t = cross(offset, edge) / divisor;
        let u = cross(offset, ray) / divisor;
        (1e-5..(1.0 - 1e-5)).contains(&t) && (0.0..=1.0).contains(&u)
    })
}

fn distance_to_wall_sq(p: [f32; 2], wall: &Wall) -> f32 {
    let edge = sub(wall.b, wall.a);
    let length_sq = dot(edge, edge);
    let t = if length_sq < 1e-12 {
        0.0
    } else {
        (dot(sub(p, wall.a), edge) / length_sq).clamp(0.0, 1.0)
    };
    let closest = [wall.a[0] + edge[0] * t, wall.a[1] + edge[1] * t];
    let delta = sub(p, closest);
    dot(delta, delta)
}

fn can_move(from: [f32; 2], to: [f32; 2], radius: f32, walls: &[Wall]) -> bool {
    walls
        .iter()
        .filter(|wall| wall.blocks_movement)
        .all(|wall| {
            let old = distance_to_wall_sq(from, wall);
            let new = distance_to_wall_sq(to, wall);
            // Permit an actor to escape an initial overlap, but not move further in.
            new >= radius * radius || (old < radius * radius && new > old)
        })
}

/// Small substeps prevent a fast actor passing through thin walls; per-axis
/// attempts allow sliding. This does not yet include dynamic objects or actors.
fn move_actor(mut position: [f32; 2], delta: [f32; 2], radius: f32, walls: &[Wall]) -> [f32; 2] {
    let length = dot(delta, delta).sqrt();
    if length <= 1e-8 {
        return position;
    }
    let steps = (length / (radius * 0.5)).ceil().max(1.0) as usize;
    let step = [delta[0] / steps as f32, delta[1] / steps as f32];
    for _ in 0..steps {
        let x = [position[0] + step[0], position[1]];
        if can_move(position, x, radius, walls) {
            position = x;
        }
        let y = [position[0], position[1] + step[1]];
        if can_move(position, y, radius, walls) {
            position = y;
        }
    }
    position
}

fn turn_toward(current: f32, target: f32, max_turn: f32) -> f32 {
    let delta = (target - current + std::f32::consts::PI).rem_euclid(std::f32::consts::TAU)
        - std::f32::consts::PI;
    (current + delta.clamp(-max_turn, max_turn)).rem_euclid(std::f32::consts::TAU)
}

impl EnemyAi {
    /// Preserve disabled placements so their renderer can be hidden by source ID.
    pub fn new(actors: &ActorState, difficulty: Difficulty) -> Self {
        let mask = match difficulty {
            Difficulty::Easy => 1,
            Difficulty::Normal => 2,
            Difficulty::Hard => 4,
        };
        let brains = actors
            .actors
            .iter()
            .map(|actor| {
                let mode = if actor.placement.flags & mask == 0 {
                    Mode::Inactive
                } else if actor.health <= 0 {
                    Mode::Dead
                } else {
                    Mode::Idle
                };
                (
                    actor.source_index,
                    Brain {
                        mode,
                        last_seen: None,
                        cooldown: 0,
                    },
                )
            })
            .collect();
        Self { brains }
    }

    pub fn mode(&self, source_index: usize) -> Option<Mode> {
        self.brains.get(&source_index).map(|brain| brain.mode)
    }

    /// Alert an actor to a position, for example when damaged out of sight.
    /// Disabled and dead actors do not respond.
    pub fn alert(&mut self, source_index: usize, position: [f32; 2]) -> bool {
        if !position.iter().all(|value| value.is_finite()) {
            return false;
        }
        let Some(brain) = self.brains.get_mut(&source_index) else {
            return false;
        };
        if matches!(brain.mode, Mode::Inactive | Mode::Dead) {
            return false;
        }
        brain.last_seen = Some(position);
        brain.mode = Mode::Chasing;
        true
    }

    /// Run exactly one 1/60-second simulation tick. Attack events are ordered
    /// by actor placement order. Calls must be driven by the owning fixed-step
    /// simulation; no real-time clocks or random numbers are consulted here.
    pub fn tick(
        &mut self,
        actors: &mut ActorState,
        definitions: &ActorDefinitions,
        world: &World,
    ) -> Vec<EnemyAttack> {
        self.tick_inner(actors, definitions, None, world, &|wall| {
            (1..=120).contains(&wall.texture)
        })
    }

    /// As `tick`, also emitting projectile launch events from each actor's
    /// rocket definition. Missing, invalid or negative rocket IDs are ignored.
    pub fn tick_with_rockets(
        &mut self,
        actors: &mut ActorState,
        definitions: &ActorDefinitions,
        rockets: &RocketDefinitions,
        world: &World,
    ) -> Vec<EnemyAttack> {
        self.tick_inner(actors, definitions, Some(rockets), world, &|wall| {
            (1..=120).contains(&wall.texture)
        })
    }
    /// Launch attacks using the level's material-specific shot collision rules.
    pub fn tick_with_collision(
        &mut self,
        actors: &mut ActorState,
        definitions: &ActorDefinitions,
        rockets: &RocketDefinitions,
        world: &World,
        blocks_shots: &dyn Fn(&Wall) -> bool,
    ) -> Vec<EnemyAttack> {
        self.tick_inner(actors, definitions, Some(rockets), world, blocks_shots)
    }

    fn tick_inner(
        &mut self,
        actors: &mut ActorState,
        definitions: &ActorDefinitions,
        rockets: Option<&RocketDefinitions>,
        world: &World,
        blocks_shots: &dyn Fn(&Wall) -> bool,
    ) -> Vec<EnemyAttack> {
        let mut attacks = Vec::new();
        let target = world.player.position;
        if !target.iter().all(|v| v.is_finite()) {
            return attacks;
        }
        for actor in &mut actors.actors {
            let Some(brain) = self.brains.get_mut(&actor.source_index) else {
                continue;
            };
            if brain.mode == Mode::Inactive {
                continue;
            }
            if actor.health <= 0 {
                brain.mode = Mode::Dead;
                brain.last_seen = None;
                continue;
            }
            if brain.mode == Mode::Dead
                || !actor.position.iter().all(|v| v.is_finite())
                || !actor.angle_radians.is_finite()
            {
                continue;
            }
            let Some(def) = definitions.entries.get(actor.definition_index) else {
                continue;
            };
            brain.cooldown = brain.cooldown.saturating_sub(1);
            let to_player = sub(target, actor.position);
            let distance = dot(to_player, to_player).sqrt();
            let sees_player =
                distance <= SIGHT_RANGE && line_of_sight(actor.position, target, &world.walls);
            if sees_player {
                brain.last_seen = Some(target);
            }
            let Some(destination) = brain.last_seen else {
                brain.mode = Mode::Idle;
                continue;
            };
            let to_goal = sub(destination, actor.position);
            let goal_distance = dot(to_goal, to_goal).sqrt();
            if goal_distance > 1e-6 {
                let target_angle = to_goal[1].atan2(to_goal[0]);
                actor.angle_radians = turn_toward(
                    actor.angle_radians,
                    target_angle,
                    def.rotation_speed.clamp(0, 20) as f32 * DT,
                );
            }
            if sees_player && distance <= MELEE_RANGE && def.kick > 0 {
                brain.mode = Mode::Attacking;
                if brain.cooldown == 0 {
                    attacks.push(EnemyAttack {
                        source_index: actor.source_index,
                        damage: def.kick.clamp(0, u16::MAX as i32) as u16,
                        kind: EnemyAttackKind::Melee,
                    });
                    brain.cooldown = ATTACK_COOLDOWN_TICKS;
                }
                continue;
            }
            if sees_player && shot_clear(actor.position, target, &world.walls, blocks_shots) {
                let rocket = rockets.and_then(|table| {
                    usize::try_from(def.rocket)
                        .ok()
                        .and_then(|index| table.0.get(index).map(|entry| (index, entry)))
                });
                if let Some((rocket_id, rocket)) = rocket.filter(|(_, rocket)| rocket.power > 0) {
                    brain.mode = Mode::Attacking;
                    if brain.cooldown == 0 {
                        attacks.push(EnemyAttack {
                            source_index: actor.source_index,
                            damage: rocket.power.clamp(0, u16::MAX as i32) as u16,
                            kind: EnemyAttackKind::Ranged {
                                rocket_id,
                                instant: rocket.model_name.is_empty(),
                                origin: actor.position,
                                target,
                            },
                        });
                        brain.cooldown = ATTACK_COOLDOWN_TICKS;
                    }
                    continue;
                }
            }
            if !sees_player && goal_distance < 0.05 {
                brain.mode = Mode::Idle;
                brain.last_seen = None;
                continue;
            }
            brain.mode = Mode::Chasing;
            if def.speed <= 2 || goal_distance <= 1e-6 {
                continue;
            }
            // Reference implementation scales the original SPEED column by 1/10;
            // the upper bound and rest of the movement rules are provisional.
            let speed = (def.speed as f32 / 10.0).clamp(0.0, 8.0);
            let travel = (speed * DT).min(goal_distance);
            let radius = (def.wall_radius as f32 / 256.0).clamp(0.05, 0.9);
            let delta = [
                to_goal[0] / goal_distance * travel,
                to_goal[1] / goal_distance * travel,
            ];
            actor.position = move_actor(actor.position, delta, radius, &world.walls);
        }
        attacks
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actors::ActorDefinitions;
    use crate::map::Monster;
    use crate::world::Player;

    fn actor(health: i32, flags: u8) -> (ActorState, ActorDefinitions) {
        let defs = ActorDefinitions::parse(format!("[MONSTERS]\n2\nhero.car 100 100 10 5 100 0 0 0\njoker.car 52 190 10 5 {health} 20 -1 16\n").as_bytes()).unwrap();
        let records = [Monster {
            position: [0, 0],
            id: 101,
            angle: 0,
            flags,
        }];
        (ActorState::from_placements(&records, &defs), defs)
    }
    fn wall(texture: u8, blocks: bool) -> Wall {
        Wall {
            a: [1.0, -2.0],
            b: [1.0, 2.0],
            texture,
            blocks_movement: blocks,
            cell: None,
        }
    }
    fn world(position: [f32; 2], walls: Vec<Wall>) -> World {
        World {
            player: Player {
                position,
                angle: 0.0,
            },
            walls,
        }
    }
    #[test]
    fn sight_acquires_and_chases_target() {
        let (mut state, defs) = actor(100, 7);
        let mut ai = EnemyAi::new(&state, Difficulty::Normal);
        let world = world([2.0, 0.0], vec![]);
        assert!(ai.tick(&mut state, &defs, &world).is_empty());
        assert_eq!(ai.mode(0), Some(Mode::Chasing));
        assert!(state.actors[0].position[0] > 0.0);
    }
    #[test]
    fn opaque_walls_block_sight_and_barriers_block_movement() {
        let (mut state, defs) = actor(100, 7);
        let mut ai = EnemyAi::new(&state, Difficulty::Normal);
        let w = world([2.0, 0.0], vec![wall(1, true)]);
        ai.tick(&mut state, &defs, &w);
        assert_eq!(ai.mode(0), Some(Mode::Idle));
        assert!(ai.alert(0, [2.0, 0.0]));
        for _ in 0..120 {
            ai.tick(&mut state, &defs, &w);
        }
        assert!(state.actors[0].position[0] < 0.8);
        assert!(line_of_sight([0.0, 0.0], [2.0, 0.0], &[wall(87, true)]));
        assert!(line_of_sight([0.0, 0.0], [2.0, 0.0], &[wall(120, true)]));
    }
    #[test]
    fn melee_attacks_have_a_fixed_tick_cooldown_and_stop_on_death() {
        let (mut state, defs) = actor(100, 7);
        let mut ai = EnemyAi::new(&state, Difficulty::Normal);
        let w = world([0.5, 0.0], vec![]);
        assert_eq!(
            ai.tick(&mut state, &defs, &w),
            vec![EnemyAttack {
                source_index: 0,
                damage: 20,
                kind: EnemyAttackKind::Melee,
            }]
        );
        for _ in 0..59 {
            assert!(ai.tick(&mut state, &defs, &w).is_empty());
        }
        assert_eq!(ai.tick(&mut state, &defs, &w).len(), 1);
        state.actors[0].health = 0;
        assert!(ai.tick(&mut state, &defs, &w).is_empty());
        assert_eq!(ai.mode(0), Some(Mode::Dead));
        assert!(!ai.alert(0, [1.0, 1.0]));
    }
    fn rockets() -> RocketDefinitions {
        RocketDefinitions(vec![
            crate::combat::RocketDefinition {
                model_name: "".into(),
                power: 12,
                raw_fields: vec![],
            },
            crate::combat::RocketDefinition {
                model_name: "disk.3o".into(),
                power: 25,
                raw_fields: vec![],
            },
        ])
    }
    #[test]
    fn ranged_events_distinguish_projectiles_and_obey_cooldown() {
        let (mut state, mut defs) = actor(100, 7);
        defs.entries[1].rocket = 1;
        let mut ai = EnemyAi::new(&state, Difficulty::Normal);
        let w = world([3.0, 0.0], vec![]);
        let events = ai.tick_with_rockets(&mut state, &defs, &rockets(), &w);
        assert_eq!(
            events,
            vec![EnemyAttack {
                source_index: 0,
                damage: 25,
                kind: EnemyAttackKind::Ranged {
                    rocket_id: 1,
                    instant: false,
                    origin: [0.0, 0.0],
                    target: [3.0, 0.0]
                }
            }]
        );
        for _ in 0..59 {
            assert!(ai
                .tick_with_rockets(&mut state, &defs, &rockets(), &w)
                .is_empty());
        }
        assert_eq!(
            ai.tick_with_rockets(&mut state, &defs, &rockets(), &w)
                .len(),
            1
        );
        defs.entries[1].rocket = 0;
        for _ in 0..59 {
            ai.tick_with_rockets(&mut state, &defs, &rockets(), &w);
        }
        let events = ai.tick_with_rockets(&mut state, &defs, &rockets(), &w);
        assert_eq!(events[0].damage, 12);
        assert!(matches!(
            events[0].kind,
            EnemyAttackKind::Ranged {
                rocket_id: 0,
                instant: true,
                ..
            }
        ));
    }
    #[test]
    fn ranged_requires_valid_rocket_and_clear_shot_and_prefers_melee() {
        let (mut state, mut defs) = actor(100, 7);
        let mut ai = EnemyAi::new(&state, Difficulty::Normal);
        let clear = world([3.0, 0.0], vec![]);
        assert!(ai
            .tick_with_rockets(&mut state, &defs, &rockets(), &clear)
            .is_empty()); // -1
        defs.entries[1].rocket = 3;
        assert!(ai
            .tick_with_rockets(&mut state, &defs, &rockets(), &clear)
            .is_empty()); // no such rocket
        defs.entries[1].rocket = 1;
        let blocked = world([3.0, 0.0], vec![wall(87, true)]);
        assert!(ai
            .tick_with_rockets(&mut state, &defs, &rockets(), &blocked)
            .is_empty()); // sees through, but no clear shot
        let near = world([0.5, 0.0], vec![]);
        let events = ai.tick_with_rockets(&mut state, &defs, &rockets(), &near);
        assert!(matches!(events[0].kind, EnemyAttackKind::Melee));
        assert_eq!(events[0].damage, 20);
    }
    #[test]
    fn ranged_fire_respects_shot_passthrough_material() {
        let (mut state, mut defs) = actor(100, 7);
        defs.entries[1].rocket = 1;
        let mut ai = EnemyAi::new(&state, Difficulty::Normal);
        let blocked = world([3.0, 0.0], vec![wall(87, true)]);
        let events = ai.tick_with_collision(&mut state, &defs, &rockets(), &blocked, &|wall| {
            wall.texture != 87
        });
        assert!(matches!(
            events.first().map(|event| event.kind),
            Some(EnemyAttackKind::Ranged { .. })
        ));
    }
    #[test]
    fn difficulty_and_invalid_positions_do_not_activate_actors() {
        let (mut state, defs) = actor(100, 1);
        let mut ai = EnemyAi::new(&state, Difficulty::Normal);
        let w = world([0.5, 0.0], vec![]);
        assert!(ai.tick(&mut state, &defs, &w).is_empty());
        assert_eq!(ai.mode(0), Some(Mode::Inactive));
        assert!(!ai.alert(0, [1.0, 1.0]));
        let (mut state, defs) = actor(100, 7);
        let mut ai = EnemyAi::new(&state, Difficulty::Normal);
        assert!(ai
            .tick(&mut state, &defs, &world([f32::NAN, 0.0], vec![]))
            .is_empty());
        assert_eq!(ai.mode(0), Some(Mode::Idle));
    }
}
