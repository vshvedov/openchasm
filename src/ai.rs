//! Provisional enemy behavior operating on the server-side actor state.
//!
//! The caller advances this once at 60 Hz. Visibility, navigation, attack range,
//! cooldown and collision are initial approximations, not original AI behavior.
//! AI emits melee and projectile launch events; the owning simulation resolves damage.
use crate::actor_clips::ActorClipSet;
use crate::actors::{ActorDefinitions, ActorState};
use crate::combat::RocketDefinitions;
use crate::pickups::Difficulty;
use crate::world::{CollisionCircle, Wall, World};
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

/// The authoritative local frame of the currently active attack clip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttackAnimation {
    pub slot: usize,
    pub frame: u32,
}
#[derive(Debug, Clone, Copy)]
struct AttackPhase {
    slot: usize,
    frame_count: u32,
    elapsed_ticks: u32,
    fired: bool,
}

#[derive(Debug, Clone, Copy)]
struct Brain {
    mode: Mode,
    last_seen: Option<[f32; 2]>,
    cooldown: u16,
    run_units: u64,
    attack_phase: Option<AttackPhase>,
}

/// Per-actor AI state remains separate from map definitions and rendering.
#[derive(Debug, Default, Clone)]
pub struct EnemyAi {
    brains: BTreeMap<usize, Brain>,
    clips: Vec<Option<ActorClipSet>>,
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
        if !(1..=86).contains(&wall.texture) || !wall.blocks_movement {
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
                        run_units: 0,
                        attack_phase: None,
                    },
                )
            })
            .collect();
        Self {
            brains,
            clips: Vec::new(),
        }
    }

    pub fn mode(&self, source_index: usize) -> Option<Mode> {
        self.brains.get(&source_index).map(|brain| brain.mode)
    }

    /// Supply one optional clip set per monster definition before simulation.
    /// A missing entry explicitly retains the legacy provisional attack logic;
    /// a present set with absent slots disables the corresponding attack.
    pub fn set_clip_metadata(&mut self, clips: Vec<Option<ActorClipSet>>) {
        self.clips = clips;
        for brain in self.brains.values_mut() {
            brain.run_units = 0;
            brain.attack_phase = None;
            if brain.mode == Mode::Attacking {
                brain.mode = Mode::Chasing;
            }
        }
    }
    /// Local attack frame uses the same deterministic 60ms/frame clock as AI.
    pub fn attack_animation(&self, source_index: usize) -> Option<AttackAnimation> {
        let phase = self.brains.get(&source_index)?.attack_phase?;
        let frame = ((u64::from(phase.elapsed_ticks) * 5) / 18)
            .min(u64::from(phase.frame_count - 1)) as u32;
        Some(AttackAnimation {
            slot: phase.slot,
            frame,
        })
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
        self.tick_inner(
            actors,
            definitions,
            None,
            world,
            &|wall| (1..=120).contains(&wall.texture),
            None,
        )
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
        self.tick_inner(
            actors,
            definitions,
            Some(rockets),
            world,
            &|wall| (1..=120).contains(&wall.texture),
            None,
        )
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
        self.tick_inner(
            actors,
            definitions,
            Some(rockets),
            world,
            blocks_shots,
            None,
        )
    }
    /// As `tick_with_collision`, sweep chase movement against the player,
    /// living difficulty-enabled actors and the supplied represented objects.
    pub fn tick_with_actor_collision(
        &mut self,
        actors: &mut ActorState,
        definitions: &ActorDefinitions,
        rockets: &RocketDefinitions,
        world: &World,
        blocks_shots: &dyn Fn(&Wall) -> bool,
        objects: &[CollisionCircle],
    ) -> Vec<EnemyAttack> {
        self.tick_inner(
            actors,
            definitions,
            Some(rockets),
            world,
            blocks_shots,
            Some(objects),
        )
    }

    fn tick_inner(
        &mut self,
        actors: &mut ActorState,
        definitions: &ActorDefinitions,
        rockets: Option<&RocketDefinitions>,
        world: &World,
        blocks_shots: &dyn Fn(&Wall) -> bool,
        objects: Option<&[CollisionCircle]>,
    ) -> Vec<EnemyAttack> {
        let mut attacks = Vec::new();
        let target = world.player.position;
        if !target.iter().all(|v| v.is_finite()) {
            return attacks;
        }
        let mut circles = objects.map(|_| {
            actors
                .actors
                .iter()
                .map(|actor| {
                    let mode = self
                        .brains
                        .get(&actor.source_index)
                        .map(|brain| brain.mode)?;
                    if actor.health <= 0
                        || matches!(mode, Mode::Inactive | Mode::Dead)
                        || !actor.position.iter().all(|v| v.is_finite())
                    {
                        return None;
                    }
                    let def = definitions.entries.get(actor.definition_index)?;
                    Some(CollisionCircle {
                        center: actor.position,
                        radius: (def.wall_radius as f32 / 256.0).clamp(0.05, 2.0),
                    })
                })
                .collect::<Vec<_>>()
        });
        for (index, actor) in actors.actors.iter_mut().enumerate() {
            let Some(brain) = self.brains.get_mut(&actor.source_index) else {
                continue;
            };
            if brain.mode == Mode::Inactive {
                continue;
            }
            if actor.health <= 0 {
                brain.mode = Mode::Dead;
                brain.last_seen = None;
                brain.attack_phase = None;
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
            if let Some(mut phase) = brain.attack_phase {
                phase.elapsed_ticks = phase.elapsed_ticks.saturating_add(1);
                let frame = u64::from(phase.elapsed_ticks) * 5 / 18;
                if !phase.fired && frame >= u64::from(phase.frame_count / 2) {
                    phase.fired = true;
                    if phase.slot == 5 {
                        if sees_player
                            && shot_clear(actor.position, target, &world.walls, blocks_shots)
                        {
                            if let Some((rocket_id, rocket)) = rockets
                                .and_then(|table| {
                                    usize::try_from(def.rocket).ok().and_then(|index| {
                                        table.0.get(index).map(|entry| (index, entry))
                                    })
                                })
                                .filter(|(_, rocket)| rocket.power > 0)
                            {
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
                            }
                        }
                    } else if sees_player && distance <= MELEE_RANGE && def.kick > 0 {
                        attacks.push(EnemyAttack {
                            source_index: actor.source_index,
                            damage: def.kick.clamp(0, u16::MAX as i32) as u16,
                            kind: EnemyAttackKind::Melee,
                        });
                    }
                }
                if u64::from(phase.elapsed_ticks) * 5 >= u64::from(phase.frame_count) * 18 {
                    brain.attack_phase = None;
                    brain.mode = Mode::Chasing;
                } else {
                    brain.attack_phase = Some(phase);
                    brain.mode = Mode::Attacking;
                }
                continue;
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
            if let Some(clips) = self
                .clips
                .get(actor.definition_index)
                .and_then(Option::as_ref)
            {
                let boundary = if let Some(run) = clips.clips[0]
                    .filter(|clip| clip.frame_count > 0 && clip.frame_count <= 1_000_000)
                {
                    let duration = u64::from(run.frame_count) * 18;
                    brain.run_units = brain.run_units.saturating_add(5);
                    if brain.run_units >= duration {
                        brain.run_units %= duration;
                        true
                    } else {
                        false
                    }
                } else {
                    false
                };
                if boundary && sees_player {
                    let melee = distance <= MELEE_RANGE && def.kick > 0;
                    let slot = if melee {
                        (6..=8).find(|&slot| {
                            clips.clips[slot].is_some_and(|clip| {
                                clip.frame_count > 0 && clip.frame_count <= 1_000_000
                            })
                        })
                    } else {
                        None
                    };
                    let ranged = clips.clips[5]
                        .is_some_and(|clip| clip.frame_count > 0 && clip.frame_count <= 1_000_000)
                        && shot_clear(actor.position, target, &world.walls, blocks_shots)
                        && rockets
                            .and_then(|table| {
                                usize::try_from(def.rocket)
                                    .ok()
                                    .and_then(|id| table.0.get(id))
                            })
                            .is_some_and(|rocket| rocket.power > 0);
                    if let Some(slot) = slot.or_else(|| ranged.then_some(5)) {
                        let clip = clips.clips[slot].expect("selected present clip");
                        brain.run_units = 0;
                        brain.attack_phase = Some(AttackPhase {
                            slot,
                            frame_count: clip.frame_count,
                            elapsed_ticks: 0,
                            fired: false,
                        });
                        brain.mode = Mode::Attacking;
                        continue;
                    }
                }
                // Known metadata must not fall back to nonexistent attack slots.
            } else {
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
                    if let Some((rocket_id, rocket)) = rocket.filter(|(_, rocket)| rocket.power > 0)
                    {
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
            let delta = [
                to_goal[0] / goal_distance * travel,
                to_goal[1] / goal_distance * travel,
            ];
            if let (Some(circles), Some(objects)) = (&mut circles, objects) {
                let radius = (def.wall_radius as f32 / 256.0).clamp(0.05, 2.0);
                let mut obstacles = Vec::with_capacity(circles.len() + objects.len() + 1);
                obstacles.push(CollisionCircle {
                    center: target,
                    radius: 0.18,
                });
                obstacles.extend(
                    circles.iter().enumerate().filter_map(|(other, circle)| {
                        (other != index).then_some(*circle).flatten()
                    }),
                );
                obstacles.extend(objects.iter().copied());
                actor.position = world.move_circle(actor.position, delta, radius, &obstacles);
                circles[index] = Some(CollisionCircle {
                    center: actor.position,
                    radius,
                });
            } else {
                let radius = (def.wall_radius as f32 / 256.0).clamp(0.05, 0.9);
                actor.position = move_actor(actor.position, delta, radius, &world.walls);
            }
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
    #[test]
    fn metadata_drives_run_then_melee_midpoint_and_completion() {
        let (mut state, mut defs) = actor(100, 7);
        defs.entries[1].speed = 0;
        let clips = ActorClipSet::parse_gltf(b"0 0 1\n6 1 4\n", Some(5)).unwrap();
        let mut ai = EnemyAi::new(&state, Difficulty::Normal);
        ai.set_clip_metadata(vec![None, Some(clips)]);
        let w = world([0.5, 0.0], vec![]);
        for _ in 0..3 {
            assert!(ai.tick(&mut state, &defs, &w).is_empty());
            assert!(ai.attack_animation(0).is_none());
        }
        assert!(ai.tick(&mut state, &defs, &w).is_empty());
        assert_eq!(
            ai.attack_animation(0),
            Some(AttackAnimation { slot: 6, frame: 0 })
        );
        for _ in 0..7 {
            assert!(ai.tick(&mut state, &defs, &w).is_empty());
        }
        let mut restart_ai = ai.clone();
        let mut restart_state = state.clone();
        assert_eq!(
            ai.tick(&mut state, &defs, &w),
            vec![EnemyAttack {
                source_index: 0,
                damage: 20,
                kind: EnemyAttackKind::Melee
            }]
        );
        assert_eq!(
            restart_ai.tick(&mut restart_state, &defs, &w),
            vec![EnemyAttack {
                source_index: 0,
                damage: 20,
                kind: EnemyAttackKind::Melee
            }]
        );
        assert_eq!(
            ai.attack_animation(0),
            Some(AttackAnimation { slot: 6, frame: 2 })
        );
        for _ in 0..6 {
            assert!(ai.tick(&mut state, &defs, &w).is_empty());
        }
        assert!(ai.attack_animation(0).is_some());
        assert!(ai.tick(&mut state, &defs, &w).is_empty());
        assert!(ai.attack_animation(0).is_none());
    }
    #[test]
    fn metadata_selects_ranged_and_rechecks_target_at_fire_frame() {
        let (mut state, mut defs) = actor(100, 7);
        defs.entries[1].speed = 0;
        defs.entries[1].rocket = 0;
        let clips = ActorClipSet::parse_gltf(b"0 0 1\n5 1 4\n6 5 4\n", Some(9)).unwrap();
        let mut ai = EnemyAi::new(&state, Difficulty::Normal);
        ai.set_clip_metadata(vec![None, Some(clips)]);
        let visible = world([2.0, 0.0], vec![]);
        for _ in 0..4 {
            assert!(ai
                .tick_with_rockets(&mut state, &defs, &rockets(), &visible)
                .is_empty());
        }
        assert_eq!(
            ai.attack_animation(0),
            Some(AttackAnimation { slot: 5, frame: 0 })
        );
        for _ in 0..7 {
            assert!(ai
                .tick_with_rockets(&mut state, &defs, &rockets(), &visible)
                .is_empty());
        }
        let blocked = world([2.0, 0.0], vec![wall(1, true)]);
        assert!(ai
            .tick_with_rockets(&mut state, &defs, &rockets(), &blocked)
            .is_empty());
        for _ in 0..6 {
            assert!(ai
                .tick_with_rockets(&mut state, &defs, &rockets(), &visible)
                .is_empty());
        }
        state.actors[0].health = 0;
        assert!(ai
            .tick_with_rockets(&mut state, &defs, &rockets(), &visible)
            .is_empty());
        assert_eq!(ai.mode(0), Some(Mode::Dead));
        assert!(ai.attack_animation(0).is_none());
    }
    #[test]
    fn known_missing_attack_or_run_slots_do_not_use_legacy_fallback() {
        let (mut state, mut defs) = actor(100, 7);
        defs.entries[1].speed = 0;
        let w = world([0.5, 0.0], vec![]);
        for data in [b"0 0 1\n".as_slice(), b"6 0 1\n".as_slice()] {
            let mut ai = EnemyAi::new(&state, Difficulty::Normal);
            ai.set_clip_metadata(vec![
                None,
                Some(ActorClipSet::parse_gltf(data, None).unwrap()),
            ]);
            for _ in 0..60 {
                assert!(ai.tick(&mut state, &defs, &w).is_empty());
            }
            assert!(ai.attack_animation(0).is_none());
        }
        let mut legacy = EnemyAi::new(&state, Difficulty::Normal);
        legacy.set_clip_metadata(vec![None, None]);
        assert_eq!(legacy.tick(&mut state, &defs, &w).len(), 1);
    }
    #[test]
    fn single_frame_attack_fires_once_and_completes_without_underflow() {
        let (mut state, mut defs) = actor(100, 7);
        defs.entries[1].speed = 0;
        let clips = ActorClipSet::parse_gltf(b"0 0 1\n6 1 1\n", Some(2)).unwrap();
        let mut ai = EnemyAi::new(&state, Difficulty::Normal);
        ai.set_clip_metadata(vec![None, Some(clips)]);
        let w = world([0.5, 0.0], vec![]);
        for _ in 0..4 {
            assert!(ai.tick(&mut state, &defs, &w).is_empty());
        }
        assert_eq!(
            ai.attack_animation(0),
            Some(AttackAnimation { slot: 6, frame: 0 })
        );
        assert_eq!(ai.tick(&mut state, &defs, &w).len(), 1);
        for _ in 0..2 {
            assert!(ai.tick(&mut state, &defs, &w).is_empty());
        }
        assert!(ai.attack_animation(0).is_some());
        assert!(ai.tick(&mut state, &defs, &w).is_empty());
        assert!(ai.attack_animation(0).is_none());
    }
    #[test]
    fn swept_chase_respects_player_and_represented_objects() {
        let (mut state, mut defs) = actor(100, 7);
        defs.entries[1].kick = 0;
        let mut ai = EnemyAi::new(&state, Difficulty::Normal);
        let w = world([3.0, 0.0], vec![]);
        let objects = [CollisionCircle {
            center: [1.0, 0.0],
            radius: 0.2,
        }];
        for _ in 0..180 {
            ai.tick_with_actor_collision(&mut state, &defs, &rockets(), &w, &|_| true, &objects);
        }
        let radius = defs.entries[1].wall_radius as f32 / 256.0;
        assert!(state.actors[0].position[0] <= 1.0 - radius - 0.2 + 1e-4);
        for _ in 0..180 {
            ai.tick_with_actor_collision(&mut state, &defs, &rockets(), &w, &|_| true, &[]);
        }
        assert!(state.actors[0].position[0] <= 3.0 - radius - 0.18 + 1e-4);
        assert!(state.actors[0].position[0] > 2.0);
    }
    #[test]
    fn swept_chase_blocks_living_peers_but_excludes_dead_and_disabled() {
        let (mut state, mut defs) = actor(100, 7);
        defs.entries[1].kick = 0;
        let mut stationary = defs.entries[1].clone();
        stationary.speed = 0;
        defs.entries.push(stationary);
        let mut other = state.actors[0].clone();
        other.source_index = 1;
        other.definition_index = 2;
        other.position = [1.0, 0.0];
        state.actors.push(other);
        let w = world([3.0, 0.0], vec![]);
        let mut ai = EnemyAi::new(&state, Difficulty::Normal);
        let mut dead_state = state.clone();
        let mut dead_ai = ai.clone();
        dead_state.actors[1].health = 0;
        let mut disabled_state = state.clone();
        disabled_state.actors[1].placement.flags = 1;
        let mut disabled_ai = EnemyAi::new(&disabled_state, Difficulty::Normal);
        for _ in 0..100 {
            ai.tick_with_actor_collision(&mut state, &defs, &rockets(), &w, &|_| true, &[]);
            dead_ai.tick_with_actor_collision(
                &mut dead_state,
                &defs,
                &rockets(),
                &w,
                &|_| true,
                &[],
            );
            disabled_ai.tick_with_actor_collision(
                &mut disabled_state,
                &defs,
                &rockets(),
                &w,
                &|_| true,
                &[],
            );
        }
        let radius = defs.entries[1].wall_radius as f32 / 256.0;
        assert!(state.actors[0].position[0] <= 1.0 - radius * 2.0 + 1e-4);
        assert!(dead_state.actors[0].position[0] > 1.0);
        assert!(disabled_state.actors[0].position[0] > 1.0);
    }
    #[test]
    fn swept_chase_allows_escape_from_existing_object_overlap() {
        let (mut state, mut defs) = actor(100, 7);
        defs.entries[1].kick = 0;
        state.actors[0].position = [0.9, 0.0];
        let mut ai = EnemyAi::new(&state, Difficulty::Normal);
        let w = world([-1.0, 0.0], vec![]);
        let object = CollisionCircle {
            center: [1.0, 0.0],
            radius: 0.3,
        };
        for _ in 0..10 {
            ai.tick_with_actor_collision(&mut state, &defs, &rockets(), &w, &|_| true, &[object]);
        }
        assert!(state.actors[0].position[0] < 0.8);
    }
}
