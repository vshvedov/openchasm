//! Provisional enemy behavior operating on the server-side actor state.
//!
//! The caller advances this once at 60 Hz. Visibility, navigation, attack range,
//! cooldown and collision are initial approximations, not original AI behavior.
//! AI emits melee and projectile launch events; the owning simulation resolves damage.
use crate::actor_clips::ActorClipSet;
use crate::actors::{ActorDefinitions, ActorState};
use crate::combat::RocketDefinitions;
use crate::pickups::Difficulty;
use crate::world::{CollisionCircle, Wall, WallCollisionMode, World};
use std::collections::BTreeMap;

const DT: f32 = 1.0 / 60.0;
const SIGHT_RANGE: f32 = 8.0;
const MELEE_RANGE: f32 = 0.8;
const ATTACK_COOLDOWN_TICKS: u16 = 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
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

/// Sided actor collision is allowed only for walls marked definitely static.
/// Missing/false entries stay two-sided, including process-driven geometry.
pub struct ActorCollisionSides<'a> {
    pub objects: &'a [CollisionCircle],
    pub static_walls: &'a [bool],
}
#[derive(Clone, Copy)]
struct ActorCollision<'a> {
    objects: &'a [CollisionCircle],
    static_walls: Option<&'a [bool]>,
    sight: Option<&'a [SightCylinder]>,
    player_sight_height: f32,
}

/// Level model sight cylinder, in absolute map-tile units.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SightCylinder {
    pub center: [f32; 2],
    pub radius: f32,
    pub vertical: [f32; 2],
}

fn model_sight_clear(
    from: [f32; 2],
    to: [f32; 2],
    target_z: f32,
    models: &[SightCylinder],
) -> bool {
    if !from.iter().chain(&to).all(|value| value.is_finite()) || !target_z.is_finite() {
        return false;
    }
    let d = sub(to, from);
    let dz = target_z - 0.5;
    !models.iter().any(|model| {
        if model.radius <= 0.0 {
            return false;
        }
        if !model.center.iter().all(|v| v.is_finite())
            || !model.radius.is_finite()
            || !model.vertical.iter().all(|v| v.is_finite())
            || model.vertical[0] > model.vertical[1]
        {
            return true;
        }
        let rel = sub(from, model.center);
        let a = dot(d, d);
        let b = dot(rel, d);
        let c = dot(rel, rel) - model.radius * model.radius;
        let (near, far) = if a <= 1e-12 {
            if c > 0.0 {
                return false;
            }
            (0.0, 1.0)
        } else {
            let disc = b * b - a * c;
            if !disc.is_finite() {
                return true;
            }
            if disc < 0.0 {
                return false;
            }
            let root = disc.sqrt();
            ((-b - root) / a, (-b + root) / a)
        };
        let near = near.max(0.0);
        let far = far.min(1.0);
        near <= far
            && crate::combat::clip_aim_interval(
                near,
                far,
                0.5,
                dz,
                Some((model.vertical[0], model.vertical[1])),
            )
            .is_some()
    })
}

/// Per-actor AI state remains separate from map definitions and rendering.
#[derive(Debug, Default, Clone)]
pub struct EnemyAi {
    brains: BTreeMap<usize, Brain>,
    clips: Vec<Option<ActorClipSet>>,
    player_invisible: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttackPhaseSnapshot {
    pub slot: u8,
    pub frame_count: u32,
    pub elapsed_ticks: u32,
    pub fired: bool,
}
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrainSnapshot {
    pub source_index: u32,
    pub mode: Mode,
    pub last_seen: Option<[f32; 2]>,
    pub cooldown: u16,
    pub run_units: u64,
    pub attack_phase: Option<AttackPhaseSnapshot>,
}
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnemyAiSnapshot {
    pub brains: Vec<BrainSnapshot>,
    pub player_invisible: bool,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AiSnapshotError {
    IndexOutOfRange,
    CountMismatch,
    DuplicateSource,
    MissingSource,
    InvalidState,
    InvalidAttackClip,
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
    pub fn snapshot(&self) -> Result<EnemyAiSnapshot, AiSnapshotError> {
        let mut brains = Vec::with_capacity(self.brains.len());
        for (&id, brain) in &self.brains {
            let source_index = u32::try_from(id).map_err(|_| AiSnapshotError::IndexOutOfRange)?;
            let attack_phase = brain.attack_phase.map(|p| AttackPhaseSnapshot {
                slot: p.slot as u8,
                frame_count: p.frame_count,
                elapsed_ticks: p.elapsed_ticks,
                fired: p.fired,
            });
            brains.push(BrainSnapshot {
                source_index,
                mode: brain.mode,
                last_seen: brain.last_seen,
                cooldown: brain.cooldown,
                run_units: brain.run_units,
                attack_phase,
            });
        }
        Ok(EnemyAiSnapshot {
            brains,
            player_invisible: self.player_invisible,
        })
    }
    /// Restore brains against loaded actor IDs and clip metadata. A live brain
    /// with zero actor health is valid until the following simulation tick.
    pub fn restore(
        &mut self,
        snapshot: &EnemyAiSnapshot,
        actors: &ActorState,
    ) -> Result<(), AiSnapshotError> {
        if snapshot.brains.len() != self.brains.len() || actors.actors.len() != self.brains.len() {
            return Err(AiSnapshotError::CountMismatch);
        }
        let actor_map: BTreeMap<_, _> = actors.actors.iter().map(|a| (a.source_index, a)).collect();
        if actor_map.len() != actors.actors.len() {
            return Err(AiSnapshotError::DuplicateSource);
        }
        let mut staged = BTreeMap::new();
        for value in &snapshot.brains {
            let id = value.source_index as usize;
            if staged.contains_key(&id) {
                return Err(AiSnapshotError::DuplicateSource);
            }
            let initial = self.brains.get(&id).ok_or(AiSnapshotError::MissingSource)?;
            let actor = actor_map.get(&id).ok_or(AiSnapshotError::MissingSource)?;
            if (initial.mode == Mode::Inactive) != (value.mode == Mode::Inactive)
                || (value.mode == Mode::Dead && actor.health > 0)
                || value.cooldown > ATTACK_COOLDOWN_TICKS
                || value.run_units > 18_000_000
                || value
                    .last_seen
                    .is_some_and(|p| !p.iter().all(|v| v.is_finite() && v.abs() <= 4096.0))
            {
                return Err(AiSnapshotError::InvalidState);
            }
            let attack_phase = if let Some(phase) = value.attack_phase {
                let slot = usize::from(phase.slot);
                let clip = self
                    .clips
                    .get(actor.definition_index)
                    .and_then(Option::as_ref)
                    .and_then(|set| set.clips.get(slot))
                    .and_then(|clip| *clip)
                    .ok_or(AiSnapshotError::InvalidAttackClip)?;
                let frame = u64::from(phase.elapsed_ticks) * 5 / 18;
                if !(5..=8).contains(&slot)
                    || value.mode != Mode::Attacking
                    || phase.frame_count != clip.frame_count
                    || phase.frame_count == 0
                    || u64::from(phase.elapsed_ticks) * 5 >= u64::from(phase.frame_count) * 18
                    || (phase.elapsed_ticks > 0
                        && phase.fired != (frame >= u64::from(phase.frame_count / 2)))
                    || (phase.elapsed_ticks == 0 && phase.fired)
                {
                    return Err(AiSnapshotError::InvalidAttackClip);
                }
                Some(AttackPhase {
                    slot,
                    frame_count: phase.frame_count,
                    elapsed_ticks: phase.elapsed_ticks,
                    fired: phase.fired,
                })
            } else {
                None
            };
            staged.insert(
                id,
                Brain {
                    mode: value.mode,
                    last_seen: value.last_seen,
                    cooldown: value.cooldown,
                    run_units: value.run_units,
                    attack_phase,
                },
            );
        }
        self.brains = staged;
        self.player_invisible = snapshot.player_invisible;
        Ok(())
    }
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
            player_invisible: false,
        }
    }

    pub fn mode(&self, source_index: usize) -> Option<Mode> {
        self.brains.get(&source_index).map(|brain| brain.mode)
    }

    /// Suppress visual acquisition by idle actors while the player is invisible.
    /// Already alerted actors keep tracking the player, following the reference.
    pub fn set_player_invisible(&mut self, invisible: bool) {
        self.player_invisible = invisible;
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

    /// Whether the ranged attack's muzzle geometry is visible on this frame.
    /// The local reconstruction enables it only around the midpoint of slot 5;
    /// melee, idle, unknown metadata and dead/inactive actors never show it.
    pub fn ranged_fire_visible(&self, source_index: usize) -> bool {
        let Some(brain) = self.brains.get(&source_index) else {
            return false;
        };
        let Some(phase) = brain.attack_phase else {
            return false;
        };
        if brain.mode != Mode::Attacking || phase.slot != 5 || phase.frame_count == 0 {
            return false;
        }
        let frame = ((u64::from(phase.elapsed_ticks) * 5) / 18)
            .min(u64::from(phase.frame_count - 1)) as u32;
        frame.abs_diff(phase.frame_count / 2) <= 2
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
            Some(ActorCollision {
                objects,
                static_walls: None,
                sight: None,
                player_sight_height: 0.5,
            }),
        )
    }
    /// Sweep against authored wall sides only for explicitly verified static
    /// walls. Every unmarked wall remains conservatively two-sided. Shot and
    /// sight tests always use the original material and are unchanged.
    pub fn tick_with_authored_sides(
        &mut self,
        actors: &mut ActorState,
        definitions: &ActorDefinitions,
        rockets: &RocketDefinitions,
        world: &World,
        blocks_shots: &dyn Fn(&Wall) -> bool,
        sides: ActorCollisionSides<'_>,
    ) -> Vec<EnemyAttack> {
        self.tick_inner(
            actors,
            definitions,
            Some(rockets),
            world,
            blocks_shots,
            Some(ActorCollision {
                objects: sides.objects,
                static_walls: Some(sides.static_walls),
                sight: None,
                player_sight_height: 0.5,
            }),
        )
    }

    /// Opt into reference-style level-model sight cylinders independently of movement.
    #[allow(clippy::too_many_arguments)]
    pub fn tick_with_object_sight(
        &mut self,
        actors: &mut ActorState,
        definitions: &ActorDefinitions,
        rockets: &RocketDefinitions,
        world: &World,
        blocks_shots: &dyn Fn(&Wall) -> bool,
        objects: &[CollisionCircle],
        static_walls: Option<&[bool]>,
        sight: &[SightCylinder],
        player_sight_height: f32,
    ) -> Vec<EnemyAttack> {
        self.tick_inner(
            actors,
            definitions,
            Some(rockets),
            world,
            blocks_shots,
            Some(ActorCollision {
                objects,
                static_walls,
                sight: Some(sight),
                player_sight_height,
            }),
        )
    }

    fn tick_inner(
        &mut self,
        actors: &mut ActorState,
        definitions: &ActorDefinitions,
        rockets: Option<&RocketDefinitions>,
        world: &World,
        blocks_shots: &dyn Fn(&Wall) -> bool,
        collision: Option<ActorCollision<'_>>,
    ) -> Vec<EnemyAttack> {
        let mut attacks = Vec::new();
        let target = world.player.position;
        if !target.iter().all(|v| v.is_finite()) {
            return attacks;
        }
        // The sweep uses a private world copy so conservative wall-side
        // overrides do not affect visibility, shots, or the owning world.
        let movement_world = collision.and_then(|config| {
            config.static_walls.map(|mask| {
                let mut view = world.clone();
                for (index, wall) in view.walls.iter_mut().enumerate() {
                    if !mask.get(index).copied().unwrap_or(false)
                        && (1..=86).contains(&wall.texture)
                    {
                        wall.texture = 120;
                    }
                }
                view
            })
        });
        let mut circles = collision.map(|_| {
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
            // Visibility follows the material's shot-pass flag, independently
            // of walk collision and of whether the wall is drawn transparent.
            let sees_player = !(self.player_invisible && brain.mode == Mode::Idle)
                && distance <= SIGHT_RANGE
                && shot_clear(actor.position, target, &world.walls, blocks_shots)
                && collision.is_none_or(|config| {
                    config.sight.is_none_or(|models| {
                        model_sight_clear(
                            actor.position,
                            target,
                            config.player_sight_height,
                            models,
                        )
                    })
                });
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
            if let (Some(circles), Some(config)) = (&mut circles, collision) {
                let objects = config.objects;
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
                actor.position = if let Some(view) = &movement_world {
                    view.move_circle_with_mode(
                        actor.position,
                        delta,
                        radius,
                        &obstacles,
                        WallCollisionMode::AuthoredSides,
                    )
                } else {
                    world.move_circle(actor.position, delta, radius, &obstacles)
                };
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

    #[test]
    fn ai_snapshot_validates_clips_and_continues_attack() {
        let (mut state, mut defs) = actor(100, 7);
        defs.entries[1].speed = 0;
        let clips = ActorClipSet::parse_gltf(b"0 0 1\n6 1 4\n", Some(5)).unwrap();
        let mut source = EnemyAi::new(&state, Difficulty::Normal);
        source.set_clip_metadata(vec![None, Some(clips.clone())]);
        let w = world([0.5, 0.0], vec![]);
        for _ in 0..30 {
            source.tick(&mut state, &defs, &w);
            if source.attack_animation(0).is_some() {
                break;
            }
        }
        assert!(source.attack_animation(0).is_some());
        let snapshot = source.snapshot().unwrap();
        let mut restored_state = state.clone();
        let mut restored = EnemyAi::new(&restored_state, Difficulty::Normal);
        restored.set_clip_metadata(vec![None, Some(clips)]);
        restored.restore(&snapshot, &restored_state).unwrap();
        for _ in 0..20 {
            assert_eq!(
                source.tick(&mut state, &defs, &w),
                restored.tick(&mut restored_state, &defs, &w)
            );
        }
        assert_eq!(source.snapshot().unwrap(), restored.snapshot().unwrap());
        let before = restored.snapshot().unwrap();
        let mut invalid = snapshot.clone();
        invalid.brains[0].attack_phase.as_mut().unwrap().frame_count += 1;
        assert_eq!(
            restored.restore(&invalid, &restored_state),
            Err(AiSnapshotError::InvalidAttackClip)
        );
        assert_eq!(restored.snapshot().unwrap(), before);
        let mut transient = snapshot;
        transient.brains[0].mode = Mode::Attacking;
        restored_state.actors[0].health = 0;
        assert!(
            restored.restore(&transient, &restored_state).is_ok(),
            "damage can precede AI death update"
        );
    }

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
    fn invisible_player_is_not_acquired_by_idle_enemy_but_alerted_enemy_tracks() {
        let (mut actors, defs) = actor(100, 7);
        let mut ai = EnemyAi::new(&actors, Difficulty::Normal);
        let w = world([3.0, 0.0], vec![]);
        ai.set_player_invisible(true);
        for _ in 0..10 {
            ai.tick(&mut actors, &defs, &w);
        }
        assert_eq!(ai.mode(0), Some(Mode::Idle));
        assert!(ai.alert(0, w.player.position));
        ai.tick(&mut actors, &defs, &w);
        assert_eq!(ai.mode(0), Some(Mode::Chasing));
        let (mut actors, defs) = actor(100, 7);
        let mut ai = EnemyAi::new(&actors, Difficulty::Normal);
        ai.set_player_invisible(true);
        ai.tick(&mut actors, &defs, &w);
        ai.set_player_invisible(false);
        ai.tick(&mut actors, &defs, &w);
        assert_eq!(ai.mode(0), Some(Mode::Chasing));
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
    fn opt_in_model_sight_uses_height_and_rechecks_attacks() {
        let (mut state, defs) = actor(100, 7);
        let mut ai = EnemyAi::new(&state, Difficulty::Normal);
        let w = world([0.6, 0.0], vec![]);
        let mut cover = SightCylinder {
            center: [0.3, 0.0],
            radius: 0.08,
            vertical: [0.0, 1.0],
        };
        let attacks = ai.tick_with_object_sight(
            &mut state,
            &defs,
            &rockets(),
            &w,
            &|_| true,
            &[],
            None,
            &[cover],
            0.5,
        );
        assert!(attacks.is_empty());
        assert_eq!(ai.mode(0), Some(Mode::Idle));
        cover.vertical = [2.0, 3.0];
        assert_eq!(
            ai.tick_with_object_sight(
                &mut state,
                &defs,
                &rockets(),
                &w,
                &|_| true,
                &[],
                None,
                &[cover],
                0.5
            )
            .len(),
            1
        );
        assert!(model_sight_clear(
            [0.0, 0.0],
            [3.0, 0.0],
            0.5,
            &[SightCylinder {
                center: [1.0, 0.0],
                radius: 0.0,
                vertical: [0.0, 1.0]
            }]
        ));
        // Beginning inside a cylinder and exact tangencies both occlude.
        let solid = SightCylinder {
            center: [0.0, 0.0],
            radius: 0.5,
            vertical: [0.0, 1.0],
        };
        assert!(!model_sight_clear([0.0, 0.0], [3.0, 0.0], 0.5, &[solid]));
        assert!(!model_sight_clear(
            [0.0, 0.5],
            [3.0, 0.5],
            0.5,
            &[SightCylinder {
                center: [1.0, 0.0],
                ..solid
            }]
        ));
        assert!(!model_sight_clear([0.0, 0.0], [0.0, 0.0], 0.5, &[solid]));
        assert!(!model_sight_clear(
            [0.0, 0.0],
            [3.0, 0.0],
            2.5,
            &[SightCylinder {
                center: [1.0, 0.0],
                radius: 0.4,
                vertical: [1.0, 1.2]
            }]
        ));
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
        assert!(shot_clear(
            [0.0, 0.0],
            [2.0, 0.0],
            &[wall(87, true)],
            &|_| false
        ));
        assert!(shot_clear(
            [0.0, 0.0],
            [2.0, 0.0],
            &[wall(120, true)],
            &|_| false
        ));
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
                animation_name: String::new(),
                model_name: "".into(),
                power: 12,
                raw_fields: vec![],
            },
            crate::combat::RocketDefinition {
                animation_name: String::new(),
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
    fn ranged_fire_geometry_only_shows_near_ranged_clip_midpoint() {
        let (state, _) = actor(100, 7);
        let mut ai = EnemyAi::new(&state, Difficulty::Normal);
        assert!(!ai.ranged_fire_visible(0));
        assert!(!ai.ranged_fire_visible(99));
        let brain = ai.brains.get_mut(&0).unwrap();
        brain.mode = Mode::Attacking;
        brain.attack_phase = Some(AttackPhase {
            slot: 5,
            frame_count: 12,
            elapsed_ticks: 0,
            fired: false,
        });
        assert!(!ai.ranged_fire_visible(0));
        ai.brains
            .get_mut(&0)
            .unwrap()
            .attack_phase
            .as_mut()
            .unwrap()
            .elapsed_ticks = 15; // frame 4
        assert!(ai.ranged_fire_visible(0));
        ai.brains
            .get_mut(&0)
            .unwrap()
            .attack_phase
            .as_mut()
            .unwrap()
            .elapsed_ticks = 33; // frame 9
        assert!(!ai.ranged_fire_visible(0));
        ai.brains
            .get_mut(&0)
            .unwrap()
            .attack_phase
            .as_mut()
            .unwrap()
            .slot = 6;
        assert!(!ai.ranged_fire_visible(0));
        let brain = ai.brains.get_mut(&0).unwrap();
        brain.attack_phase.as_mut().unwrap().slot = 5;
        brain.mode = Mode::Dead;
        assert!(!ai.ranged_fire_visible(0));
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
    #[test]
    fn visibility_uses_shot_material_independent_of_walk_collision() {
        let (state, mut defs) = actor(100, 7);
        defs.entries[1].kick = 0;
        let rockets = rockets();
        let cases = [
            (false, true, false),
            (false, false, true),
            (true, false, true),
            (true, true, false),
        ];
        for (walk_blocking, shot_blocking, visible) in cases {
            let mut actors = state.clone();
            let mut ai = EnemyAi::new(&actors, Difficulty::Normal);
            let w = world([2.0, 0.0], vec![wall(87, walk_blocking)]);
            ai.tick_with_actor_collision(&mut actors, &defs, &rockets, &w, &|_| shot_blocking, &[]);
            assert_eq!(
                ai.mode(0) == Some(Mode::Chasing),
                visible,
                "walk={walk_blocking} shot={shot_blocking}"
            );
            if visible && walk_blocking {
                for _ in 0..120 {
                    ai.tick_with_actor_collision(&mut actors, &defs, &rockets, &w, &|_| false, &[]);
                }
                assert!(
                    actors.actors[0].position[0] < 0.8,
                    "visible barrier must still block walking"
                );
            }
        }
    }
    #[test]
    fn authored_actor_sides_require_explicit_static_proof() {
        fn run(from: f32, target: f32, texture: u8, static_wall: &[bool], legacy: bool) -> f32 {
            let (mut state, mut defs) = actor(100, 7);
            defs.entries[1].kick = 0;
            state.actors[0].position = [from, 0.0];
            let mut ai = EnemyAi::new(&state, Difficulty::Normal);
            let w = world([target, 0.0], vec![wall(texture, true)]);
            for _ in 0..150 {
                if legacy {
                    ai.tick_with_actor_collision(
                        &mut state,
                        &defs,
                        &rockets(),
                        &w,
                        &|_| false,
                        &[],
                    );
                } else {
                    ai.tick_with_authored_sides(
                        &mut state,
                        &defs,
                        &rockets(),
                        &w,
                        &|_| false,
                        ActorCollisionSides {
                            objects: &[],
                            static_walls: static_wall,
                        },
                    );
                }
            }
            state.actors[0].position[0]
        }
        let front = run(0.0, 2.0, 1, &[true], false);
        assert!(front < 0.8, "opaque front blocks: {front}");
        let back = run(2.0, 0.0, 1, &[true], false);
        assert!(back < 1.0, "verified static back passes: {back}");
        for (texture, mask, legacy) in [
            (1, &[][..], false),
            (1, &[false][..], false),
            (87, &[true][..], false),
            (120, &[true][..], false),
            (1, &[true][..], true),
        ] {
            let position = run(2.0, 0.0, texture, mask, legacy);
            assert!(
                position > 1.2,
                "unverified, transparent or default wall blocks both sides: {position}"
            );
        }
    }
    #[test]
    fn authored_movement_does_not_change_wall_material_for_sight() {
        let (mut state, mut defs) = actor(100, 7);
        defs.entries[1].kick = 0;
        state.actors[0].position = [2.0, 0.0];
        let mut ai = EnemyAi::new(&state, Difficulty::Normal);
        let w = world([0.0, 0.0], vec![wall(1, true)]);
        ai.tick_with_authored_sides(
            &mut state,
            &defs,
            &rockets(),
            &w,
            &|wall| wall.texture == 1,
            ActorCollisionSides {
                objects: &[],
                static_walls: &[],
            },
        );
        assert_eq!(ai.mode(0), Some(Mode::Idle));
        assert_eq!(w.walls[0].texture, 1);
    }
}
