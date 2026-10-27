//! Fixed-tick game session: simulation state independent of presentation and transport.
use crate::actors::{ActorDefinitions, ActorState};
use crate::ai::{EnemyAi, EnemyAttack, EnemyAttackKind, Mode};
use crate::combat::{
    self, Combat, EnemyShotContext, EnemyShotHit, EnemyShotTarget, FireResult, RocketDefinitions,
    ShotContext, ShotImpact,
};
use crate::explosives::{
    self, Cylinder, DamageEvent, Explosion, ExplosionTarget, Mine, MineEvent, MineSystem,
    MineTrigger, TargetId,
};
use crate::game::GameInventory;
use crate::hazards::{HazardDamage, HazardTarget, Hazards};
use crate::mission::{MissionEvent, MissionPlan, MissionState};
use crate::object_combat::ObjectCombat;
use crate::object_states::{ObjectStates, ObjectTransition};
use crate::objects::ObjectDefinitions;
use crate::pickups::{Difficulty, PickupEvent};
use crate::process::Script;
use crate::process_runtime::{
    ImmediateAction, ProcessEffect, ProcessEvent, ProcessOutcome, ProcessRuntime,
};
use crate::process_transaction::{PresentationManifest, ProcessTransaction};
use crate::projectiles::{Impact, ImpactTarget, Launch, Owner, ProjectileSystem};
use crate::quake::{QuakeField, QuakeZone};
use crate::vertical::{VerticalBounds, VerticalEvents, VerticalState};
use crate::wind::{WindError, WindField};
use crate::world::{CollisionCircle, Input, WallCollisionMode, World};
use std::collections::{BTreeMap, BTreeSet};

pub struct LevelSetup {
    pub world: World,
    pub processes: ProcessRuntime,
    pub actors: ActorState,
    pub actor_definitions: ActorDefinitions,
    pub game: GameInventory,
    pub rockets: RocketDefinitions,
    /// Indexed by wall texture ID; false means shots pass through.
    pub shot_blocking: [bool; 128],
    pub difficulty: Difficulty,
}

#[derive(Default)]
pub struct SessionInput {
    pub movement: Input,
    /// Horizontal look displacement in radians for this tick, independent of frame rate.
    pub yaw_delta: f32,
    pub interact: bool,
    /// One-shot jump press edge; a held button is not automatically repeated.
    pub jump: bool,
    pub fire: bool,
    pub select_weapon: Option<usize>,
}
/// Result of one authoritative enemy instant-shot trace.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EnemyHitscanEvent {
    pub source_index: usize,
    pub rocket_id: usize,
    pub hit: Option<EnemyShotHit>,
}

/// The source of a committed explosion, aligned with `SessionEvents::explosions`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExplosionOrigin {
    Projectile {
        projectile_id: u64,
        rocket_id: usize,
    },
    Mine {
        mine_id: u64,
    },
}

#[derive(Default)]
pub struct SessionEvents {
    pub pickups: Vec<PickupEvent>,
    pub shot: Option<FireResult>,
    pub damage: u16,
    pub enemy_attacks: Vec<EnemyAttack>,
    pub enemy_hits: Vec<EnemyHitscanEvent>,
    pub projectile_impacts: Vec<Impact>,
    /// Impact explosions and raw pre-armor damage, in deterministic order.
    pub explosions: Vec<Explosion>,
    /// One source per explosion, in the same order as `explosions`.
    pub explosion_origins: Vec<ExplosionOrigin>,
    pub explosion_damage: Vec<DamageEvent>,
    /// Stable mine lifecycle events for presentation and networking.
    pub mine_events: Vec<MineEvent>,
    /// Object cell and whether it was destroyed by this hit.
    pub object_hits: Vec<([usize; 2], bool)>,
    /// Ordered committed model replacements; the host must apply before next tick.
    pub object_transitions: Vec<ObjectTransition>,
    /// Authored quake commands for an explicitly opted-in consumer.
    pub quake_changes: Vec<QuakeZone>,
    /// Mission presentation/completion events produced during this step.
    pub mission: Vec<MissionEvent>,
    pub process_effects: Vec<ProcessEffect>,
    pub hazard_damage: Vec<HazardDamage>,
    pub vertical: VerticalEvents,
    /// Invalid or duplicate wind activation, if emitted by a process.
    pub wind_errors: Vec<WindError>,
}

#[derive(Clone)]
pub struct ScriptedObjects {
    pub states: ObjectStates,
    pub definitions: ObjectDefinitions,
    pub manifest: PresentationManifest,
    pub quake: QuakeField,
    /// The host promises to consume and present quake changes.
    pub quake_enabled: bool,
    destroy_links: Vec<([u8; 2], u16)>,
    pickup_links: Vec<([u8; 2], u16)>,
    shoot_links: Vec<([u8; 2], u16)>,
    required_objects: Vec<u16>,
}
impl ScriptedObjects {
    pub fn new(
        states: ObjectStates,
        definitions: ObjectDefinitions,
        manifest: PresentationManifest,
        script: &Script,
        quake_enabled: bool,
    ) -> Self {
        Self {
            states,
            definitions,
            manifest,
            quake: QuakeField::new(),
            quake_enabled,
            required_objects: script
                .processes
                .iter()
                .filter_map(|(&id, process)| {
                    process
                        .actions
                        .iter()
                        .any(|a| matches!(a.name.as_str(), "explode" | "change"))
                        .then_some(id)
                })
                .collect(),
            pickup_links: script
                .triggers
                .iter()
                .filter(|t| t.kind == "link")
                .map(|t| ([t.x, t.y], t.process))
                .collect(),
            shoot_links: script
                .triggers
                .iter()
                .filter(|t| t.kind == "shoot")
                .map(|t| ([t.x, t.y], t.process))
                .collect(),
            destroy_links: script
                .triggers
                .iter()
                .filter(|t| t.kind == "destroy")
                .map(|t| ([t.x, t.y], t.process))
                .collect(),
        }
    }
}

#[derive(Clone)]
pub struct Session {
    pub world: World,
    wall_collision_mode: WallCollisionMode,
    pub vertical: VerticalState,
    pub processes: ProcessRuntime,
    pub actors: ActorState,
    pub actor_definitions: ActorDefinitions,
    pub game: GameInventory,
    pub ai: EnemyAi,
    pub selected_weapon: usize,
    pub projectiles: ProjectileSystem,
    explosions_enabled: bool,
    projectile_heights: BTreeMap<u64, f32>,
    mines_enabled: bool,
    mines: MineSystem,
    pub objects: Option<ObjectCombat>,
    pub scripted_objects: Option<ScriptedObjects>,
    pub mission: Option<MissionState>,
    pub hazards: Hazards,
    pub wind: WindField,
    pending_wind_errors: Vec<WindError>,
    /// Absolute offsets in original script units, indexed by wall index.
    pub wall_elevations: Vec<f32>,
    base_wall_blocking: Vec<bool>,
    elevation_by_cell: Vec<f32>,
    combat: Combat,
    rockets: RocketDefinitions,
    shot_blocking: [bool; 128],
}
impl Session {
    pub fn new(mut setup: LevelSetup) -> Self {
        let ai = EnemyAi::new(&setup.actors, setup.difficulty);
        // Disabled difficulty placements must not be hittable by combat.
        for actor in &mut setup.actors.actors {
            if ai.mode(actor.source_index) == Some(Mode::Inactive) {
                actor.health = 0;
            }
        }
        let wall_elevations = vec![0.0; setup.world.walls.len()];
        let base_wall_blocking = setup
            .world
            .walls
            .iter()
            .map(|wall| wall.blocks_movement)
            .collect();
        Self {
            wall_elevations,
            base_wall_blocking,
            elevation_by_cell: vec![0.0; 64 * 64],
            world: setup.world,
            wall_collision_mode: WallCollisionMode::TwoSided,
            vertical: VerticalState::new(0.0).expect("default vertical state is valid"),
            processes: setup.processes,
            actors: setup.actors,
            actor_definitions: setup.actor_definitions,
            game: setup.game,
            ai,
            selected_weapon: 0,
            projectiles: ProjectileSystem::new(),
            explosions_enabled: false,
            projectile_heights: BTreeMap::new(),
            mines_enabled: false,
            mines: MineSystem::new(),
            objects: None,
            scripted_objects: None,
            mission: None,
            hazards: Hazards::default(),
            wind: WindField::new(),
            pending_wind_errors: Vec::new(),
            combat: Combat::new(),
            rockets: setup.rockets,
            shot_blocking: setup.shot_blocking,
        }
    }
    /// Choose the wall policy for player and wind-driven movement. Defaults to
    /// two-sided; regular enemy AI and moving-wall obstruction remain separate
    /// until their owners explicitly support the same policy. Snapshot clones
    /// retain the selected policy.
    pub fn set_wall_collision_mode(&mut self, mode: WallCollisionMode) {
        self.wall_collision_mode = mode;
    }

    pub fn wall_collision_mode(&self) -> WallCollisionMode {
        self.wall_collision_mode
    }

    /// Opt into provisional straight 2D grenades and impact splash. The host
    /// must consume explosion events and synchronize resulting object changes.
    pub fn set_explosions_enabled(&mut self, enabled: bool) {
        self.explosions_enabled = enabled;
    }

    /// Enable provisional slot-6 mine placement. Restart snapshots clone mines.
    pub fn set_mines_enabled(&mut self, enabled: bool) {
        self.mines_enabled = enabled;
    }
    /// Stable authoritative snapshot for presenting or synchronizing mines.
    pub fn mines(&self) -> &[Mine] {
        self.mines.active()
    }

    /// Install shootable object state before taking the restart snapshot.
    pub fn set_objects(&mut self, objects: ObjectCombat) {
        self.objects = Some(objects);
    }

    /// Opt in to staged destroy links before taking the restart snapshot.
    /// Requires a matching staged process runtime and represented object combat.
    pub fn set_scripted_objects(&mut self, scripted: ScriptedObjects) {
        self.scripted_objects = Some(scripted);
    }
    fn sync_object_damage(&mut self, cell: [usize; 2], damage: i32) {
        if let Some(state) = self
            .scripted_objects
            .as_mut()
            .and_then(|s| s.states.get(cell).map(|state| state.health))
        {
            if let Some(scripted) = self.scripted_objects.as_mut() {
                scripted
                    .states
                    .set_health(cell, state.saturating_sub(damage.max(0)).max(0));
            }
        }
    }
    /// Execute one prepared process. The bool in an error indicates a failure
    /// during external validation, rather than a denied runtime activation.
    fn try_staged_process(
        &mut self,
        kind: &str,
        cell: [u8; 2],
        id: u16,
        events: &mut SessionEvents,
    ) -> Result<ProcessEvent, (ProcessOutcome, bool)> {
        let prepared = self
            .processes
            .prepare_at(kind, cell, id)
            .map_err(|outcome| (outcome, false))?;
        let quakes: Vec<_> = prepared
            .actions()
            .iter()
            .filter_map(|action| match action {
                ImmediateAction::Quake(zone) => Some(*zone),
                _ => None,
            })
            .collect();
        let scripted = self.scripted_objects.as_mut().expect("opted-in state");
        if !quakes.is_empty() && !scripted.quake_enabled {
            return Err((ProcessOutcome::Unsupported, true));
        }
        let combat = self
            .objects
            .as_mut()
            .ok_or((ProcessOutcome::Unsupported, true))?;
        let player = self.world.player.position;
        let actor_circles: Vec<_> = self
            .actors
            .actors
            .iter()
            .filter_map(|actor| {
                if actor.health <= 0 || self.ai.mode(actor.source_index) == Some(Mode::Inactive) {
                    return None;
                }
                let radius = self
                    .actor_definitions
                    .entries
                    .get(actor.definition_index)
                    .map(|def| (def.wall_radius as f32 / 256.0).clamp(0.05, 2.0))
                    .unwrap_or(2.0);
                Some((actor.position, radius))
            })
            .collect();
        let transaction = ProcessTransaction::prepare(
            prepared,
            &mut scripted.states,
            combat,
            &mut scripted.quake,
            &scripted.definitions,
            &scripted.manifest,
            |_, position, radius| {
                let distance = |p: [f32; 2]| (p[0] - position[0]).hypot(p[1] - position[1]);
                player.iter().all(|v| v.is_finite())
                    && distance(player) >= radius + 0.18
                    && actor_circles
                        .iter()
                        .all(|(p, r)| p.iter().all(|v| v.is_finite()) && distance(*p) >= radius + r)
            },
        )
        .map_err(|_| (ProcessOutcome::Unsupported, true))?;
        let transitions = transaction.transitions().to_vec();
        let event = transaction
            .commit()
            .map_err(|_| (ProcessOutcome::Unsupported, true))?;
        events.object_transitions.extend(transitions);
        events.quake_changes.extend(quakes);
        Ok(event)
    }
    /// Destroy-link group failure rolls back its already committed processes.
    fn activate_destroy(
        &mut self,
        cell: [u8; 2],
        events: &mut SessionEvents,
    ) -> (Vec<ProcessEvent>, bool) {
        let Some(scripted) = self.scripted_objects.as_ref() else {
            return (self.processes.activate_at("destroy", cell), true);
        };
        let required_objects = scripted.required_objects.clone();
        let ids: Vec<_> = scripted
            .destroy_links
            .iter()
            .filter_map(|(at, id)| (*at == cell).then_some(*id))
            .collect();
        let saved_processes = self.processes.clone();
        let saved_scripted = self.scripted_objects.clone();
        let saved_objects = self.objects.clone();
        let replacement_start = events.object_transitions.len();
        let quake_start = events.quake_changes.len();
        let mut attempts = Vec::new();
        for id in ids {
            match self.try_staged_process("destroy", cell, id, events) {
                Ok(event) => attempts.push(event),
                Err((outcome, preflight_failed))
                    if !preflight_failed && !required_objects.contains(&id) =>
                {
                    attempts.push(ProcessEvent {
                        process_id: id,
                        outcome,
                    })
                }
                Err(_) => {
                    self.processes = saved_processes;
                    self.scripted_objects = saved_scripted;
                    self.objects = saved_objects;
                    events.object_transitions.truncate(replacement_start);
                    events.quake_changes.truncate(quake_start);
                    return (Vec::new(), false);
                }
            }
        }
        (attempts, true)
    }

    /// Dispatch shot-contact links in authored order without publishing a
    /// partial staged action. Caller owns rollback of the surrounding hit.
    fn activate_shot_contact(
        &mut self,
        cell: [u8; 2],
        events: &mut SessionEvents,
        attempts: &mut Vec<ProcessEvent>,
    ) -> bool {
        let Some(scripted) = &self.scripted_objects else {
            attempts.extend(self.processes.activate_at("shoot", cell));
            return true;
        };
        let ids: Vec<_> = scripted
            .shoot_links
            .iter()
            .filter_map(|(at, id)| (*at == cell).then_some(*id))
            .collect();
        let required = scripted.required_objects.clone();
        for id in ids {
            match self.try_staged_process("shoot", cell, id, events) {
                Ok(event) => attempts.push(event),
                Err((outcome, preflight)) if !preflight && !required.contains(&id) => attempts
                    .push(ProcessEvent {
                        process_id: id,
                        outcome,
                    }),
                Err(_) => return false,
            }
        }
        true
    }

    fn resolve_enemy_instant(
        &mut self,
        attack: EnemyAttack,
        rocket_id: usize,
        origin: [f32; 2],
        target: [f32; 2],
        events: &mut SessionEvents,
        attempts: &mut Vec<ProcessEvent>,
    ) {
        let hit = combat::enemy_hitscan(
            origin,
            target,
            attack.source_index,
            EnemyShotContext {
                world: &self.world,
                actors: &self.actors,
                definitions: &self.actor_definitions,
                objects: self.objects.as_ref(),
            },
            |wall| Self::blocks_shot(wall, &self.shot_blocking, &self.elevation_by_cell, 1.0),
        );
        let before = hit.and_then(|hit| {
            matches!(
                hit.target,
                EnemyShotTarget::Object { .. } | EnemyShotTarget::Wall { .. }
            )
            .then(|| self.clone())
        });
        let attempt_count = attempts.len();
        let hit_count = events.object_hits.len();
        let transition_count = events.object_transitions.len();
        let quake_count = events.quake_changes.len();
        let mut accepted = true;
        if let Some(hit) = hit {
            let cell = match hit.target {
                EnemyShotTarget::Object { cell } => Some([cell[0] as u8, cell[1] as u8]),
                EnemyShotTarget::Wall { wall_index } => {
                    self.world.walls.get(wall_index).and_then(|wall| wall.cell)
                }
                _ => None,
            };
            if let Some(cell) = cell {
                accepted = self.activate_shot_contact(cell, events, attempts);
            }
            if accepted {
                match hit.target {
                    EnemyShotTarget::Player => {
                        events.damage = events
                            .damage
                            .saturating_add(self.apply_player_damage(attack.damage));
                    }
                    EnemyShotTarget::Actor { source_index } => {
                        if let Some(actor) = self
                            .actors
                            .actors
                            .iter_mut()
                            .find(|actor| actor.source_index == source_index)
                        {
                            actor.health =
                                actor.health.saturating_sub(i32::from(attack.damage)).max(0);
                            if actor.health > 0 {
                                self.ai.alert(source_index, origin);
                            }
                        }
                    }
                    EnemyShotTarget::Object { cell } => {
                        if let Some(object_hit) = self
                            .objects
                            .as_mut()
                            .and_then(|objects| objects.apply_hit(cell, i32::from(attack.damage)))
                        {
                            self.sync_object_damage(cell, i32::from(attack.damage));
                            if object_hit.destroyed {
                                let (destroy_attempts, committed) =
                                    self.activate_destroy([cell[0] as u8, cell[1] as u8], events);
                                if committed {
                                    attempts.extend(destroy_attempts);
                                    if !events.object_transitions[transition_count..]
                                        .iter()
                                        .any(|t| t.cell == cell)
                                    {
                                        self.invalidate_process_object(cell);
                                    }
                                } else {
                                    accepted = false;
                                }
                            }
                            if accepted {
                                events.object_hits.push((cell, object_hit.destroyed));
                            }
                        }
                    }
                    EnemyShotTarget::Wall { .. } => {}
                }
            }
        }
        if !accepted {
            if let Some(before) = before {
                *self = before;
            }
            attempts.truncate(attempt_count);
            events.object_hits.truncate(hit_count);
            events.object_transitions.truncate(transition_count);
            events.quake_changes.truncate(quake_count);
            return;
        }
        events.enemy_hits.push(EnemyHitscanEvent {
            source_index: attack.source_index,
            rocket_id,
            hit,
        });
    }

    /// Apply one impact as a transaction. Damage is only emitted after every
    /// object destruction script has succeeded; direct impact is not applied.
    fn apply_explosion(
        &mut self,
        explosion: Explosion,
        origin: ExplosionOrigin,
        impact: Option<ImpactTarget>,
        events: &mut SessionEvents,
        attempts: &mut Vec<ProcessEvent>,
    ) -> bool {
        let mut targets = vec![ExplosionTarget {
            id: TargetId::Player,
            cylinder: Cylinder {
                center: self.world.player.position,
                radius: 0.18,
                bottom: self.vertical.feet(),
                top: self.vertical.head(),
            },
        }];
        targets.extend(self.actors.actors.iter().filter_map(|actor| {
            if actor.health <= 0 || self.ai.mode(actor.source_index) == Some(Mode::Inactive) {
                return None;
            }
            let def = self.actor_definitions.entries.get(actor.definition_index)?;
            Some(ExplosionTarget {
                id: TargetId::Actor {
                    source_index: actor.source_index,
                },
                cylinder: Cylinder {
                    center: actor.position,
                    radius: (def.wall_radius as f32 / 256.0).clamp(0.05, 2.0),
                    bottom: 0.0,
                    top: 1.0,
                },
            })
        }));
        if let Some(objects) = &self.objects {
            targets.extend(objects.explosion_targets().map(|(cell, center, radius)| {
                ExplosionTarget {
                    id: TargetId::Object { cell },
                    cylinder: Cylinder {
                        center,
                        radius,
                        bottom: 0.0,
                        top: 1.0,
                    },
                }
            }));
        }
        let Ok(damage) = explosives::resolve_explosion(explosion, &targets) else {
            return false;
        };
        let before = self.clone();
        let attempt_count = attempts.len();
        let hit_count = events.object_hits.len();
        let transition_count = events.object_transitions.len();
        let quake_count = events.quake_changes.len();
        let previous_damage = events.damage;
        // Contact still dispatches shoot links, independently of blast targets.
        let hit_cell = match impact {
            Some(ImpactTarget::Object { cell }) => Some([cell[0] as u8, cell[1] as u8]),
            Some(ImpactTarget::Wall { wall_index }) => {
                self.world.walls.get(wall_index).and_then(|wall| wall.cell)
            }
            _ => None,
        };
        if let Some(cell) = hit_cell {
            if !self.activate_shot_contact(cell, events, attempts) {
                *self = before;
                attempts.truncate(attempt_count);
                events.object_hits.truncate(hit_count);
                events.object_transitions.truncate(transition_count);
                events.quake_changes.truncate(quake_count);
                events.damage = previous_damage;
                return false;
            }
        }
        let mut applied = Vec::new();
        for item in &damage {
            match item.target {
                TargetId::Player => {
                    if self.game.inventory.health == 0 {
                        continue;
                    }
                    applied.push(*item);
                    events.damage = events.damage.saturating_add(
                        self.apply_player_damage(item.damage.clamp(0, u16::MAX as i32) as u16),
                    );
                }
                TargetId::Actor { source_index } => {
                    if let Some(actor) = self
                        .actors
                        .actors
                        .iter_mut()
                        .find(|actor| actor.source_index == source_index)
                    {
                        if actor.health <= 0 {
                            continue;
                        }
                        applied.push(*item);
                        actor.health = actor.health.saturating_sub(item.damage).max(0);
                        if actor.health > 0 {
                            self.ai.alert(
                                source_index,
                                explosion.center[..2].try_into().expect("two coordinates"),
                            );
                        }
                    }
                }
                TargetId::Object { cell } => {
                    let Some(hit) = self
                        .objects
                        .as_mut()
                        .and_then(|objects| objects.apply_hit(cell, item.damage))
                    else {
                        continue;
                    };
                    applied.push(*item);
                    self.sync_object_damage(cell, item.damage);
                    if hit.destroyed {
                        let (destroy_attempts, accepted) =
                            self.activate_destroy([cell[0] as u8, cell[1] as u8], events);
                        if !accepted {
                            *self = before;
                            attempts.truncate(attempt_count);
                            events.object_hits.truncate(hit_count);
                            events.object_transitions.truncate(transition_count);
                            events.quake_changes.truncate(quake_count);
                            events.damage = previous_damage;
                            return false;
                        }
                        attempts.extend(destroy_attempts);
                        if !events.object_transitions[transition_count..]
                            .iter()
                            .any(|t| t.cell == cell)
                        {
                            self.invalidate_process_object(cell);
                        }
                    }
                    events.object_hits.push((cell, hit.destroyed));
                }
            }
        }
        events.explosions.push(explosion);
        events.explosion_origins.push(origin);
        events.explosion_damage.extend(applied);
        true
    }

    fn tick_mines(&mut self, events: &mut SessionEvents, attempts: &mut Vec<ProcessEvent>) {
        if !self.mines_enabled || self.mines.active().is_empty() {
            return;
        }
        let mut triggers = Vec::new();
        if self.game.inventory.health > 0 {
            triggers.push(MineTrigger {
                id: TargetId::Player,
                position: self.world.player.position,
                radius: 0.18,
            });
        }
        triggers.extend(self.actors.actors.iter().filter_map(|actor| {
            if actor.health <= 0 || self.ai.mode(actor.source_index) == Some(Mode::Inactive) {
                return None;
            }
            let definition = self.actor_definitions.entries.get(actor.definition_index)?;
            Some(MineTrigger {
                id: TargetId::Actor {
                    source_index: actor.source_index,
                },
                position: actor.position,
                radius: (definition.wall_radius as f32 / 256.0).clamp(0.05, 2.0),
            })
        }));
        let mut staged = self.mines.clone();
        let Ok(mine_events) = staged.tick(&triggers) else {
            return;
        };
        if mine_events
            .iter()
            .any(|event| matches!(event, MineEvent::Detonated { .. }))
        {
            let before = self.clone();
            let attempt_count = attempts.len();
            let hit_count = events.object_hits.len();
            let transition_count = events.object_transitions.len();
            let quake_count = events.quake_changes.len();
            let explosions_count = events.explosions.len();
            let damage_count = events.explosion_damage.len();
            let previous_damage = events.damage;
            for event in &mine_events {
                if let MineEvent::Detonated { id, explosion } = event {
                    if !self.apply_explosion(
                        *explosion,
                        ExplosionOrigin::Mine { mine_id: *id },
                        None,
                        events,
                        attempts,
                    ) {
                        *self = before;
                        attempts.truncate(attempt_count);
                        events.object_hits.truncate(hit_count);
                        events.object_transitions.truncate(transition_count);
                        events.quake_changes.truncate(quake_count);
                        events.explosions.truncate(explosions_count);
                        events.explosion_origins.truncate(explosions_count);
                        events.explosion_damage.truncate(damage_count);
                        events.damage = previous_damage;
                        return;
                    }
                }
            }
        }
        self.mines = staged;
        events.mine_events.extend(mine_events);
    }

    /// Attach a freshly parsed mission before taking the session's restart snapshot.
    pub fn set_mission(&mut self, plan: MissionPlan) {
        self.mission = Some(MissionState::new(plan));
    }
    fn blocks_shot(
        wall: &crate::world::Wall,
        shot_blocking: &[bool; 128],
        elevation_by_cell: &[f32],
        shot_height: f32,
    ) -> bool {
        let elevated = wall.cell.is_some_and(|[x, y]| {
            x < 64
                && y < 64
                && elevation_by_cell
                    .get(usize::from(x) * 64 + usize::from(y))
                    .is_some_and(|offset| *offset >= shot_height * 64.0)
        });
        !elevated
            && shot_blocking
                .get(wall.texture as usize)
                .copied()
                .unwrap_or(true)
    }
    /// Conservative clearance under nearby elevated solid walls. Flat map
    /// floor/ceiling assumptions remain provisional until plane geometry exists.
    fn player_bounds(&self) -> VerticalBounds {
        let p = self.world.player.position;
        let mut bounds = VerticalBounds::flat_room();
        for (i, wall) in self.world.walls.iter().enumerate() {
            if !self.base_wall_blocking.get(i).copied().unwrap_or(false) {
                continue;
            }
            let offset = self.wall_elevations.get(i).copied().unwrap_or(0.0) / 64.0;
            if offset <= self.vertical.feet() || offset >= bounds.ceiling.unwrap_or(f32::INFINITY) {
                continue;
            }
            let d = [wall.b[0] - wall.a[0], wall.b[1] - wall.a[1]];
            let len = d[0] * d[0] + d[1] * d[1];
            let t = if len <= 1e-12 {
                0.0
            } else {
                (((p[0] - wall.a[0]) * d[0] + (p[1] - wall.a[1]) * d[1]) / len).clamp(0.0, 1.0)
            };
            let dx = p[0] - (wall.a[0] + d[0] * t);
            let dy = p[1] - (wall.a[1] + d[1] * t);
            if dx * dx + dy * dy < 0.18 * 0.18 {
                bounds.ceiling = Some(offset);
            }
        }
        bounds
    }
    fn sync_wall_blocking(&mut self) {
        let needed = (self.vertical.head() * 64.0).max(80.0);
        for (i, wall) in self.world.walls.iter_mut().enumerate() {
            wall.blocks_movement = self.base_wall_blocking.get(i).copied().unwrap_or(false)
                && self.wall_elevations.get(i).copied().unwrap_or(0.0) < needed;
        }
    }
    fn apply_process_effects(&mut self) -> Vec<ProcessEffect> {
        let effects = self.processes.take_effects();
        for effect in &effects {
            self.hazards.apply_effect(effect);
            match effect {
                ProcessEffect::WindActivated { process_id, zones } => {
                    if let Err(error) = self.wind.activate(*process_id, zones) {
                        self.pending_wind_errors.push(error);
                    }
                }
                ProcessEffect::WindDeactivated { process_id } => {
                    self.wind.deactivate(*process_id);
                }
                _ => {}
            }
            if let ProcessEffect::ObjectPose {
                cell,
                position,
                angle_radians,
                ..
            } = *effect
            {
                let cell = [usize::from(cell[0]), usize::from(cell[1])];
                self.game.pickups.set_position(cell, position);
                if let Some(scripted) = &mut self.scripted_objects {
                    scripted.states.set_pose(cell, position, angle_radians);
                }
                if let Some(objects) = &mut self.objects {
                    objects.set_position(cell, position);
                }
            }
            if let ProcessEffect::WallElevation {
                wall_index,
                offset_raw,
            } = *effect
            {
                if let Some(wall) = self.world.walls.get_mut(wall_index) {
                    self.wall_elevations[wall_index] = offset_raw;
                    if let Some([x, y]) = wall.cell.filter(|[x, y]| *x < 64 && *y < 64) {
                        self.elevation_by_cell[usize::from(x) * 64 + usize::from(y)] = offset_raw;
                    }
                }
            }
        }
        self.sync_wall_blocking();
        effects
    }
    fn invalidate_process_object(&mut self, cell: [usize; 2]) {
        if let (Ok(x), Ok(y)) = (u8::try_from(cell[0]), u8::try_from(cell[1])) {
            self.processes.remove_object([x, y]);
        }
    }
    fn apply_player_damage(&mut self, amount: u16) -> u16 {
        // Provisional armor: absorbs at most half of each incoming hit.
        let absorbed = (amount / 2).min(self.game.inventory.armor);
        self.game.inventory.armor -= absorbed;
        let damage = amount - absorbed;
        self.game.inventory.health = self.game.inventory.health.saturating_sub(damage);
        damage
    }
    /// Advance exactly one 60 Hz simulation tick, returning presentation events.
    pub fn step(&mut self, input: SessionInput) -> SessionEvents {
        const DT: f32 = 1.0 / 60.0;
        let mut events = SessionEvents::default();
        if self.game.inventory.health == 0 {
            return events;
        }
        if let Some(mission) = &mut self.mission {
            events.mission.extend(mission.advance(DT));
            if mission.is_complete() {
                return events;
            }
        }
        if let Some(index) = input.select_weapon.filter(|&index| {
            index < self.game.inventory.weapons.len() && self.game.inventory.weapons[index]
        }) {
            self.selected_weapon = index;
        }
        // Process look as simulation input so a future server can own the view
        // direction. Discard non-finite values and bound a single tick's turn.
        if input.yaw_delta.is_finite() && self.world.player.angle.is_finite() {
            let delta = input
                .yaw_delta
                .clamp(-std::f32::consts::PI, std::f32::consts::PI);
            self.world.player.angle =
                (self.world.player.angle + delta).rem_euclid(std::f32::consts::TAU);
        }
        let bounds = self.player_bounds();
        // A closing wall can temporarily leave insufficient clearance; retain
        // the last valid vertical state until the player can move clear.
        events.vertical = self
            .vertical
            .step(DT, input.jump, bounds)
            .unwrap_or_default();
        self.sync_wall_blocking();
        // Actors currently use a provisional full-room vertical extent.
        let mut circles: Vec<_> = self
            .actors
            .actors
            .iter()
            .filter_map(|actor| {
                if actor.health <= 0 || self.ai.mode(actor.source_index) == Some(Mode::Inactive) {
                    return None;
                }
                let definition = self.actor_definitions.entries.get(actor.definition_index)?;
                Some(crate::world::CollisionCircle {
                    center: actor.position,
                    radius: (definition.wall_radius as f32 / 256.0).clamp(0.05, 2.0),
                })
            })
            .collect();
        // Only opt-in, live, represented objects have a process-owned pose.
        // Their authored 2D radii are provisional circular approximations;
        // object height is not yet known, so do not allow jumping over them.
        circles.extend(
            self.processes
                .objects()
                .filter(|object| {
                    object.blocking_radius > 0.0
                        && object.elevation_raw / 64.0 < self.vertical.head()
                })
                .map(|object| crate::world::CollisionCircle {
                    center: object.position,
                    radius: object.blocking_radius,
                }),
        );
        let displacement = self
            .wind
            .displacement(self.world.player.position, DT)
            .unwrap_or([0.0, 0.0]);
        self.world.step_with_external_mode(
            input.movement,
            DT,
            &circles,
            displacement,
            self.wall_collision_mode,
        );
        // Move live actors in stable order using the same tick-start field.
        // Rebuild obstacles after each move, so subsequent actors see new positions.
        for index in 0..self.actors.actors.len() {
            let actor = &self.actors.actors[index];
            if actor.health <= 0 || self.ai.mode(actor.source_index) == Some(Mode::Inactive) {
                continue;
            }
            let Some(definition) = self.actor_definitions.entries.get(actor.definition_index)
            else {
                continue;
            };
            let radius = (definition.wall_radius as f32 / 256.0).clamp(0.05, 2.0);
            let position = actor.position;
            let displacement = self.wind.displacement(position, DT).unwrap_or([0.0, 0.0]);
            if displacement == [0.0, 0.0] {
                continue;
            }
            let mut obstacles = vec![CollisionCircle {
                center: self.world.player.position,
                radius: 0.18,
            }];
            obstacles.extend(self.actors.actors.iter().enumerate().filter_map(
                |(other_index, other)| {
                    if other_index == index
                        || other.health <= 0
                        || self.ai.mode(other.source_index) == Some(Mode::Inactive)
                    {
                        return None;
                    }
                    let definition = self.actor_definitions.entries.get(other.definition_index)?;
                    Some(CollisionCircle {
                        center: other.position,
                        radius: (definition.wall_radius as f32 / 256.0).clamp(0.05, 2.0),
                    })
                },
            ));
            obstacles.extend(
                self.processes
                    .objects()
                    .filter(|object| {
                        object.blocking_radius > 0.0
                            && object.elevation_raw / 64.0 < self.vertical.head()
                    })
                    .map(|object| CollisionCircle {
                        center: object.position,
                        radius: object.blocking_radius,
                    }),
            );
            self.actors.actors[index].position = self.world.move_circle_with_mode(
                position,
                displacement,
                radius,
                &obstacles,
                self.wall_collision_mode,
            );
        }
        // First stage only inventory/pickup data. The larger simulation snapshot
        // is needed only on ticks where a collection actually occurs.
        let pickup_backup = if self.scripted_objects.is_some() {
            let mut staged_game = self.game.clone();
            events.pickups = staged_game.collect_near(self.world.player.position);
            if events.pickups.is_empty() {
                None
            } else {
                let game = std::mem::replace(&mut self.game, staged_game);
                Some((
                    game,
                    self.processes.clone(),
                    self.objects.clone(),
                    self.scripted_objects.clone(),
                ))
            }
        } else {
            events.pickups = self.game.collect_near(self.world.player.position);
            None
        };
        self.processes.set_keys(self.game.inventory.keys);
        let mut attempts = Vec::new();
        let mut pickup_failed = false;
        for pickup in events.pickups.clone() {
            self.invalidate_process_object(pickup.cell);
            if let Some(objects) = &mut self.objects {
                objects.set_enabled(pickup.cell, false);
            }
            if let (Ok(x), Ok(y)) = (u8::try_from(pickup.cell[0]), u8::try_from(pickup.cell[1])) {
                if let Some(scripted) = &self.scripted_objects {
                    let ids: Vec<_> = scripted
                        .pickup_links
                        .iter()
                        .filter_map(|(at, id)| (*at == [x, y]).then_some(*id))
                        .collect();
                    let required = scripted.required_objects.clone();
                    for id in ids {
                        match self.try_staged_process("link", [x, y], id, &mut events) {
                            Ok(event) => attempts.push(event),
                            Err((outcome, preflight)) if !preflight && !required.contains(&id) => {
                                attempts.push(ProcessEvent {
                                    process_id: id,
                                    outcome,
                                })
                            }
                            Err(_) => {
                                pickup_failed = true;
                                break;
                            }
                        }
                    }
                } else {
                    attempts.extend(self.processes.activate_links_at([x, y]));
                }
            }
            if pickup_failed {
                break;
            }
        }
        if pickup_failed {
            if let Some((game, processes, objects, scripted)) = pickup_backup {
                self.game = game;
                self.processes = processes;
                self.objects = objects;
                self.scripted_objects = scripted;
            }
            events.pickups.clear();
            events.object_transitions.clear();
            events.quake_changes.clear();
            attempts.clear();
        }
        if self.scripted_objects.is_none() {
            attempts.extend(self.processes.tick(&mut self.world, DT, input.interact));
        } else {
            let commands = self
                .processes
                .collect_tick_commands(self.world.player.position, input.interact);
            for command in commands {
                if matches!(command.kind.as_str(), "return" | "returnf") {
                    self.processes.dispatch_tick_command(&command);
                } else {
                    let event = self
                        .try_staged_process(
                            &command.kind,
                            command.cell,
                            command.process_id,
                            &mut events,
                        )
                        .unwrap_or_else(|(outcome, _)| ProcessEvent {
                            process_id: command.process_id,
                            outcome,
                        });
                    attempts.push(event);
                }
            }
            self.processes.advance_motions(&mut self.world, DT);
        }
        // Process-owned object positions must be applied before this tick's
        // hitscan or projectile queries and before enemy simulation.
        events.process_effects.extend(self.apply_process_effects());
        self.tick_mines(&mut events, &mut attempts);
        self.combat.tick(DT);
        if input.fire {
            let object_before_hit = self.scripted_objects.as_ref().and(self.objects.clone());
            let scripted_before_hit = self.scripted_objects.clone();
            let processes_before_hit = self
                .scripted_objects
                .as_ref()
                .map(|_| self.processes.clone());
            let attempts_before_hit = attempts.len();
            let mut shot = if self.mines_enabled && self.selected_weapon == 6 {
                self.combat.plant_mine(
                    &mut self.game.inventory,
                    &self.game.weapons,
                    self.world.player.position,
                )
            } else {
                self.combat.fire_with_explosions(
                    self.selected_weapon,
                    ShotContext {
                        inventory: &mut self.game.inventory,
                        weapons: &self.game.weapons,
                        rockets: &self.rockets,
                        actors: &mut self.actors,
                        definitions: &self.actor_definitions,
                        world: &self.world,
                        objects: self.objects.as_mut(),
                    },
                    |wall| {
                        Self::blocks_shot(
                            wall,
                            &self.shot_blocking,
                            &self.elevation_by_cell,
                            self.vertical.eye(),
                        )
                    },
                    self.explosions_enabled,
                )
            };
            if matches!(shot, FireResult::Fired(ShotImpact::MinePlaced)) {
                let floor = self.player_bounds().floor;
                let position = [
                    self.world.player.position[0],
                    self.world.player.position[1],
                    floor,
                ];
                match self.mines.plant(Owner::Player, position) {
                    Ok(event) => events.mine_events.push(event),
                    Err(_) => {
                        self.game.inventory.ammo[6] = self.game.inventory.ammo[6].saturating_add(1);
                        self.combat.cancel_shot();
                        shot = FireResult::Unsupported;
                    }
                }
            }
            match shot {
                FireResult::Fired(ShotImpact::Object {
                    cell,
                    damage,
                    destroyed,
                    distance,
                }) => {
                    self.sync_object_damage(cell, damage);
                    let mut destroyed = destroyed;
                    if let (Ok(x), Ok(y)) = (u8::try_from(cell[0]), u8::try_from(cell[1])) {
                        attempts.extend(self.processes.activate_at("shoot", [x, y]));
                        if destroyed {
                            let (destroy_attempts, accepted) =
                                self.activate_destroy([x, y], &mut events);
                            if accepted {
                                attempts.extend(destroy_attempts);
                            } else {
                                self.objects = object_before_hit;
                                self.scripted_objects = scripted_before_hit;
                                if let Some(processes) = processes_before_hit {
                                    self.processes = processes;
                                }
                                attempts.truncate(attempts_before_hit);
                                destroyed = false;
                                shot = FireResult::Fired(ShotImpact::Object {
                                    cell,
                                    damage,
                                    destroyed,
                                    distance,
                                });
                            }
                        }
                    }
                    if destroyed && !events.object_transitions.iter().any(|t| t.cell == cell) {
                        self.invalidate_process_object(cell);
                    }
                    events.object_hits.push((cell, destroyed));
                }
                FireResult::Fired(ShotImpact::Wall { wall_index, .. }) => {
                    if let Some(cell) = self.world.walls.get(wall_index).and_then(|wall| wall.cell)
                    {
                        attempts.extend(self.processes.activate_at("shoot", cell));
                    }
                }
                _ => {}
            }
            if let FireResult::Fired(ShotImpact::Launch { rocket_id }) = shot {
                let (sin, cos) = self.world.player.angle.sin_cos();
                let launch = Launch {
                    owner: Owner::Player,
                    rocket_id,
                    origin: self.world.player.position,
                    direction: [cos, sin],
                };
                let spawned = if self.explosions_enabled && self.selected_weapon == 5 {
                    self.projectiles.spawn_straight(launch, &self.rockets)
                } else {
                    self.projectiles.spawn(launch, &self.rockets)
                };
                if let Ok(id) = spawned {
                    self.projectile_heights.insert(id, self.vertical.eye());
                } else {
                    self.game.inventory.ammo[self.selected_weapon] =
                        self.game.inventory.ammo[self.selected_weapon].saturating_add(1);
                    self.combat.cancel_shot();
                    shot = FireResult::Unsupported;
                }
            }
            if let FireResult::Fired(ShotImpact::Actor {
                source_index,
                killed: false,
                ..
            }) = shot
            {
                self.ai.alert(source_index, self.world.player.position);
            }
            events.shot = Some(shot);
        }
        events.process_effects.extend(self.apply_process_effects());
        // Enemy height is not yet modeled; one tile is a provisional body
        // height, so overhead raised objects are excluded from the sweep.
        let enemy_objects: Vec<_> = self
            .processes
            .objects()
            .filter(|object| object.blocking_radius > 0.0 && object.elevation_raw < 64.0)
            .map(|object| CollisionCircle {
                center: object.position,
                radius: object.blocking_radius,
            })
            .collect();
        events.enemy_attacks = self.ai.tick_with_actor_collision(
            &mut self.actors,
            &self.actor_definitions,
            &self.rockets,
            &self.world,
            &|wall| Self::blocks_shot(wall, &self.shot_blocking, &self.elevation_by_cell, 1.0),
            &enemy_objects,
        );
        for index in 0..events.enemy_attacks.len() {
            let attack = events.enemy_attacks[index];
            if let EnemyAttackKind::Ranged {
                rocket_id,
                instant: true,
                origin,
                target,
            } = attack.kind
            {
                self.resolve_enemy_instant(
                    attack,
                    rocket_id,
                    origin,
                    target,
                    &mut events,
                    &mut attempts,
                );
                continue;
            }
            if let EnemyAttackKind::Ranged {
                rocket_id,
                instant: false,
                origin,
                target,
            } = attack.kind
            {
                let launch = Launch {
                    owner: Owner::Enemy {
                        source_index: attack.source_index,
                    },
                    rocket_id,
                    origin,
                    direction: [target[0] - origin[0], target[1] - origin[1]],
                };
                let spawned = self.projectiles.spawn(launch, &self.rockets);
                if let Ok(id) = spawned {
                    self.projectile_heights.insert(id, 1.0);
                }
                continue;
            }
            events.damage = events
                .damage
                .saturating_add(self.apply_player_damage(attack.damage));
        }
        let pre_tick_projectiles = self.explosions_enabled.then(|| self.projectiles.clone());
        let mut frozen = BTreeSet::new();
        let mut elapsed = 0.0_f32;
        loop {
            let remaining = (DT - elapsed).max(0.0);
            let candidate = self.projectiles.trace_next(
                remaining,
                crate::projectiles::TraceContext {
                    world: &self.world,
                    actors: &self.actors,
                    definitions: &self.actor_definitions,
                    objects: self.objects.as_ref(),
                },
                |wall| Self::blocks_shot(wall, &self.shot_blocking, &self.elevation_by_cell, 1.0),
                &frozen,
            );
            let Some(mut impact) = candidate else {
                self.projectiles.advance_traced(remaining, &frozen);
                break;
            };
            if !self.projectiles.advance_traced(impact.time, &frozen) {
                break;
            }
            elapsed = (elapsed + impact.time).min(DT);
            impact.time = elapsed;
            if self.projectiles.take(impact.projectile_id).is_none() {
                break;
            }
            events.projectile_impacts.push(impact);
            let height = self
                .projectile_heights
                .remove(&impact.projectile_id)
                .unwrap_or(0.75);
            let radius_raw = self
                .rockets
                .0
                .get(impact.rocket_id)
                .and_then(|rocket| rocket.raw_fields.get(2))
                .copied()
                .unwrap_or(0);
            if self.explosions_enabled && radius_raw > 200 && impact.target != ImpactTarget::Expired
            {
                let explosion = Explosion {
                    owner: impact.owner,
                    center: [impact.position[0], impact.position[1], height],
                    radius: radius_raw as f32 / 256.0,
                    damage: impact.damage,
                };
                if !self.apply_explosion(
                    explosion,
                    ExplosionOrigin::Projectile {
                        projectile_id: impact.projectile_id,
                        rocket_id: impact.rocket_id,
                    },
                    Some(impact.target),
                    &mut events,
                    &mut attempts,
                ) && pre_tick_projectiles.as_ref().is_some_and(|snapshot| {
                    self.projectiles
                        .restore_impact(snapshot, impact.projectile_id)
                }) {
                    self.projectile_heights.insert(impact.projectile_id, height);
                    events.projectile_impacts.pop();
                    frozen.insert(impact.projectile_id);
                }
                continue;
            }
            match impact.target {
                ImpactTarget::Player => {
                    events.damage = events.damage.saturating_add(
                        self.apply_player_damage(impact.damage.clamp(0, u16::MAX as i32) as u16),
                    )
                }
                ImpactTarget::Object { cell } => {
                    let object_before_hit =
                        self.scripted_objects.as_ref().and(self.objects.clone());
                    let scripted_before_hit = self.scripted_objects.clone();
                    let processes_before_hit = self
                        .scripted_objects
                        .as_ref()
                        .map(|_| self.processes.clone());
                    let attempts_before_hit = attempts.len();
                    if let Some(hit) = self
                        .objects
                        .as_mut()
                        .and_then(|objects| objects.apply_hit(cell, impact.damage))
                    {
                        self.sync_object_damage(cell, impact.damage);
                        let mut destroyed = hit.destroyed;
                        if impact.owner == Owner::Player {
                            if let (Ok(x), Ok(y)) = (u8::try_from(cell[0]), u8::try_from(cell[1])) {
                                attempts.extend(self.processes.activate_at("shoot", [x, y]));
                                if destroyed {
                                    let (destroy_attempts, accepted) =
                                        self.activate_destroy([x, y], &mut events);
                                    if accepted {
                                        attempts.extend(destroy_attempts);
                                    } else {
                                        self.objects = object_before_hit;
                                        self.scripted_objects = scripted_before_hit;
                                        if let Some(processes) = processes_before_hit {
                                            self.processes = processes;
                                        }
                                        attempts.truncate(attempts_before_hit);
                                        destroyed = false;
                                    }
                                }
                            }
                        }
                        if destroyed && !events.object_transitions.iter().any(|t| t.cell == cell) {
                            self.invalidate_process_object(cell);
                        }
                        events.object_hits.push((cell, destroyed));
                    }
                }
                ImpactTarget::Wall { wall_index } if impact.owner == Owner::Player => {
                    if let Some(cell) = self.world.walls.get(wall_index).and_then(|wall| wall.cell)
                    {
                        attempts.extend(self.processes.activate_at("shoot", cell));
                    }
                }
                ImpactTarget::Actor { source_index } => {
                    if let Some(actor) = self
                        .actors
                        .actors
                        .iter_mut()
                        .find(|actor| actor.source_index == source_index)
                    {
                        actor.health = actor.health.saturating_sub(impact.damage).max(0);
                        if actor.health > 0 {
                            self.ai.alert(source_index, self.world.player.position);
                        }
                    }
                }
                _ => {}
            }
        }
        if let Some(mission) = &mut self.mission {
            for attempt in attempts {
                match attempt.outcome {
                    ProcessOutcome::Accepted => events
                        .mission
                        .extend(mission.attempt(attempt.process_id, true)),
                    ProcessOutcome::Locked => events
                        .mission
                        .extend(mission.attempt(attempt.process_id, false)),
                    ProcessOutcome::Unsupported
                    | ProcessOutcome::Busy
                    | ProcessOutcome::Missing => {}
                }
            }
        }
        events.process_effects.extend(self.apply_process_effects());
        events.hazard_damage = self.hazards.tick(
            self.world.player.position,
            self.vertical.feet(),
            self.vertical.head(),
            &self.actors,
        );
        for hazard in &events.hazard_damage {
            match hazard.target {
                HazardTarget::Player => {
                    events.damage = events.damage.saturating_add(
                        self.apply_player_damage(hazard.damage.min(u16::MAX as u32) as u16),
                    )
                }
                HazardTarget::Actor { source_index } => {
                    if let Some(actor) = self
                        .actors
                        .actors
                        .iter_mut()
                        .find(|actor| actor.source_index == source_index)
                    {
                        actor.health = actor
                            .health
                            .saturating_sub(hazard.damage.min(i32::MAX as u32) as i32)
                            .max(0);
                    }
                }
            }
        }
        events.wind_errors = std::mem::take(&mut self.pending_wind_errors);
        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actors::{Actor, ActorDefinition};
    use crate::combat::RocketDefinition;
    use crate::inventory::{WeaponDefinition, WeaponDefinitions};
    use crate::map::Monster;
    use crate::objects::ObjectDefinitions;
    use crate::process::Script;
    use crate::world::Player;

    fn mission_session(text: &str) -> Session {
        let world = World {
            walls: vec![],
            player: Player {
                position: [0.5, 0.5],
                angle: 0.0,
            },
        };
        let script = Script::parse(text);
        let processes = ProcessRuntime::new(&script, &world);
        let weapon = WeaponDefinition {
            rocket_type: 0,
            reload_time: 1,
            vertical_shift: 0,
            projectile_height: 0,
            ammo_per_portion: 1,
            ammo_limit: 10,
            ammo_on_weapon_pickup: 1,
            projectile_count: 1,
        };
        let game = GameInventory::new(
            WeaponDefinitions([weapon; 8]),
            &[],
            &ObjectDefinitions {
                items: vec![],
                level_objects: vec![],
            },
            Difficulty::Normal,
        );
        let mut session = Session::new(LevelSetup {
            world,
            processes,
            actors: ActorState {
                player_spawns: vec![],
                actors: vec![],
                unresolved: vec![],
            },
            actor_definitions: ActorDefinitions { entries: vec![] },
            game,
            rockets: RocketDefinitions(vec![]),
            shot_blocking: [true; 128],
            difficulty: Difficulty::Normal,
        });
        session.set_mission(MissionPlan::parse(&script, text));
        session
    }
    #[test]
    fn authored_wall_mode_is_opt_in_and_restart_snapshot_retains_it() {
        use crate::wind::WindZone;
        use crate::world::Wall;
        let mut session = mission_session("");
        session.world.player.position = [2.0, 0.5];
        session.world.walls.push(Wall {
            a: [1.0, 0.0],
            b: [1.0, 1.0],
            texture: 1,
            blocks_movement: true,
            cell: None,
        });
        session.base_wall_blocking.push(true);
        session.wall_elevations.push(0.0);
        session
            .wind
            .activate(
                1,
                &[WindZone {
                    min_cell: [0, 0],
                    max_cell: [5, 5],
                    direction: [-64, 0],
                }],
            )
            .unwrap();
        let mut default_session = session.clone();
        for _ in 0..3 {
            default_session.step(SessionInput::default());
        }
        assert!(default_session.world.player.position[0] > 1.179);
        session.set_wall_collision_mode(WallCollisionMode::AuthoredSides);
        let restart = session.clone();
        for _ in 0..3 {
            session.step(SessionInput::default());
        }
        assert!(session.world.player.position[0] < 1.0);
        let mut restarted = restart;
        assert_eq!(
            restarted.wall_collision_mode(),
            WallCollisionMode::AuthoredSides
        );
        for _ in 0..3 {
            restarted.step(SessionInput::default());
        }
        assert_eq!(
            restarted.world.player.position,
            session.world.player.position
        );
        restarted.world.player.position = [0.5, 0.5];
        restarted.wind.deactivate(1);
        restarted
            .wind
            .activate(
                2,
                &[WindZone {
                    min_cell: [0, 0],
                    max_cell: [5, 5],
                    direction: [64, 0],
                }],
            )
            .unwrap();
        for _ in 0..3 {
            restarted.step(SessionInput::default());
        }
        assert!(restarted.world.player.position[0] < 0.821);
    }

    #[test]
    fn scripted_wind_starts_next_tick_and_cannot_push_player_through_wall() {
        use crate::process_runtime::ProcessCapabilities;
        use crate::world::Wall;
        let text = "#proc1\n#action\nnonstop\nwind 0 0 5 5 64 0\n#end\n#links\nfloor 0 0 1\n#end\n";
        let mut session = mission_session(text);
        session.world.walls.push(Wall {
            a: [1.0, 0.0],
            b: [1.0, 1.0],
            texture: 1,
            blocks_movement: true,
            cell: None,
        });
        session.base_wall_blocking.push(true);
        session.wall_elevations.push(0.0);
        session.processes = ProcessRuntime::with_wind(
            &Script::parse(text),
            &session.world,
            ProcessCapabilities::default(),
            &[0; 4096],
            &[],
        )
        .unwrap();
        let first = session.step(SessionInput::default());
        assert!(first.wind_errors.is_empty());
        assert!(session.wind.is_active(1));
        assert_eq!(session.world.player.position, [0.5, 0.5]);
        for _ in 0..20 {
            assert!(session.step(SessionInput::default()).wind_errors.is_empty());
        }
        assert!(session.world.player.position[0] > 0.8);
        assert!(session.world.player.position[0] <= 0.8201);
    }

    #[test]
    fn wind_moves_live_actor_with_its_own_collision_radius() {
        use crate::wind::WindZone;
        use crate::world::Wall;
        let mut session = mission_session("");
        session.world.player.position = [5.5, 5.5];
        session.world.walls.push(Wall {
            a: [2.0, 0.0],
            b: [2.0, 2.0],
            texture: 1,
            blocks_movement: true,
            cell: None,
        });
        session.base_wall_blocking.push(true);
        session.wall_elevations.push(0.0);
        session.actor_definitions.entries.push(ActorDefinition {
            model_name: "test.car".into(),
            wall_radius: 64,
            actor_radius: 64,
            speed: 0,
            rotation_speed: 0,
            health: 30,
            kick: 0,
            rocket: -1,
            separation_limit: 0,
            extra_fields: vec![],
            raw_line: "".into(),
        });
        session.actors.actors.push(Actor {
            source_index: 0,
            definition_index: 0,
            placement: Monster {
                position: [256, 128],
                id: 100,
                angle: 0,
                flags: 2,
            },
            position: [1.0, 0.5],
            angle_radians: 0.0,
            health: 30,
        });
        session.ai = EnemyAi::new(&session.actors, Difficulty::Normal);
        session
            .wind
            .activate(
                1,
                &[WindZone {
                    min_cell: [0, 0],
                    max_cell: [5, 5],
                    direction: [64, 0],
                }],
            )
            .unwrap();
        for _ in 0..10 {
            session.step(SessionInput::default());
        }
        let x = session.actors.actors[0].position[0];
        assert!(x > 1.7 && x <= 1.7501, "{x}");
        session.actors.actors[0].health = 0;
        session.step(SessionInput::default());
        assert_eq!(session.actors.actors[0].position[0], x);
    }

    #[test]
    fn wind_actor_obeys_opt_in_opaque_sides() {
        use crate::wind::WindZone;
        use crate::world::Wall;
        let mut session = mission_session("");
        session.world.player.position = [5.5, 5.5];
        session.world.walls.push(Wall {
            a: [1.0, 0.0],
            b: [1.0, 2.0],
            texture: 1,
            blocks_movement: true,
            cell: None,
        });
        session.base_wall_blocking.push(true);
        session.wall_elevations.push(0.0);
        session.actor_definitions.entries.push(ActorDefinition {
            model_name: "test.car".into(),
            wall_radius: 64,
            actor_radius: 64,
            speed: 0,
            rotation_speed: 0,
            health: 30,
            kick: 0,
            rocket: -1,
            separation_limit: 0,
            extra_fields: vec![],
            raw_line: "".into(),
        });
        session.actors.actors.push(Actor {
            source_index: 0,
            definition_index: 0,
            placement: Monster {
                position: [256, 128],
                id: 100,
                angle: 0,
                flags: 2,
            },
            position: [2.0, 0.5],
            angle_radians: 0.0,
            health: 30,
        });
        session.ai = EnemyAi::new(&session.actors, Difficulty::Normal);
        session
            .wind
            .activate(
                1,
                &[WindZone {
                    min_cell: [0, 0],
                    max_cell: [5, 5],
                    direction: [-64, 0],
                }],
            )
            .unwrap();
        let mut baseline = session.clone();
        for _ in 0..3 {
            baseline.step(SessionInput::default());
        }
        assert!(baseline.actors.actors[0].position[0] > 1.249);
        session.set_wall_collision_mode(WallCollisionMode::AuthoredSides);
        for _ in 0..3 {
            session.step(SessionInput::default());
        }
        assert!(session.actors.actors[0].position[0] < 1.0);
    }

    #[test]
    fn look_input_is_finite_bounded_and_applied_before_movement() {
        let mut session = mission_session("");
        session.step(SessionInput {
            yaw_delta: std::f32::consts::FRAC_PI_2,
            movement: Input {
                forward: 1.0,
                ..Input::default()
            },
            ..Default::default()
        });
        assert!((session.world.player.angle - std::f32::consts::FRAC_PI_2).abs() < 1e-6);
        assert!(session.world.player.position[1] > 0.5);
        let angle = session.world.player.angle;
        session.step(SessionInput {
            yaw_delta: f32::NAN,
            ..Default::default()
        });
        assert_eq!(session.world.player.angle, angle);
        session.step(SessionInput {
            yaw_delta: 100_000.0,
            ..Default::default()
        });
        assert!((session.world.player.angle - (angle + std::f32::consts::PI)).abs() < 1e-6);
    }

    #[test]
    fn end_process_emits_message_and_completion_once_and_restart_restores_state() {
        let text = "#mess1\nDelay 3\nTextA: 0 0 \"Done!\"\n#end\n#proc1\nEndDelay 0.05\nOnMessage 1\n#action\nplayani 8\n#end\n#links\nfloor 0 0 1\n#end\n";
        let mut session = mission_session(text);
        let restart = session.clone();
        assert_eq!(
            session.step(SessionInput::default()).mission,
            [MissionEvent::ShowMessage(1)]
        );
        let mut completion_count = 0;
        for _ in 0..10 {
            completion_count += session
                .step(SessionInput::default())
                .mission
                .iter()
                .filter(|event| **event == MissionEvent::LevelComplete)
                .count();
        }
        assert_eq!(completion_count, 1);
        assert!(session.mission.as_ref().unwrap().is_complete());
        let mut restarted = restart;
        assert!(!restarted.mission.as_ref().unwrap().is_complete());
        assert_eq!(
            restarted.step(SessionInput::default()).mission,
            [MissionEvent::ShowMessage(1)]
        );
    }
    #[test]
    fn locked_exit_messages_but_never_completes_and_unsupported_is_not_accepted() {
        let text = "#mess2\nDelay 1\nTextA: 0 0 \"Locked\"\n#end\n#proc1\nLock\nLockMessage 2\nEndDelay 0.01\n#end\n#links\nlink 0 0 1\n#end\n";
        let mut session = mission_session(text);
        assert_eq!(
            session
                .step(SessionInput {
                    interact: true,
                    ..Default::default()
                })
                .mission,
            [MissionEvent::ShowMessage(2)]
        );
        for _ in 0..60 {
            assert!(!session
                .step(SessionInput::default())
                .mission
                .contains(&MissionEvent::LevelComplete));
        }
        assert_eq!(session.mission.as_ref().unwrap().end_remaining(), None);
        let text =
            "#proc1\nEndDelay 0.01\n#action\nchange 0 0 3\n#end\n#links\nfloor 0 0 1\n#end\n";
        let mut session = mission_session(text);
        for _ in 0..60 {
            assert!(!session
                .step(SessionInput::default())
                .mission
                .contains(&MissionEvent::LevelComplete));
        }
        assert_eq!(session.mission.as_ref().unwrap().end_remaining(), None);
    }
    #[test]
    fn collecting_a_linked_key_activates_its_process() {
        use crate::objects::{ObjectDefinition, ObjectInstance, ObjectKind};
        let text = "#mess1\nDelay 1\nTextA: 0 0 \"Key acquired\"\n#end\n#proc1\nOnMessage 1\nEndDelay 1\n#end\n#links\nlink 0 0 1\n#end\n";
        let mut session = mission_session(text);
        let definition = ObjectDefinition {
            collision_radius: 0,
            shadow: 0,
            bmp_object: 0,
            bmp_z: 0,
            action_code: 142,
            blow_up: 0,
            break_limit: 0,
            ambient_sound: 0,
            break_sound: 0,
            model_name: "key.3o".into(),
            animation_name: None,
        };
        let placement = ObjectInstance {
            kind: ObjectKind::Item,
            definition_index: 0,
            grid_cell: [0, 0],
            position: [0.5, 0.5],
            angle_radians: 0.0,
            flags: 0,
            difficulty_flags: 2,
            extra: 0,
            size: 0,
        };
        session.game = GameInventory::new(
            session.game.weapons.clone(),
            &[placement],
            &ObjectDefinitions {
                items: vec![definition],
                level_objects: vec![],
            },
            Difficulty::Normal,
        );
        let events = session.step(SessionInput::default());
        assert_eq!(events.pickups.len(), 1);
        assert!(session.game.inventory.keys[1]);
        assert_eq!(events.mission, vec![MissionEvent::ShowMessage(1)]);
    }
    #[test]
    fn destroying_a_linked_lock_unlocks_its_process_and_ends_level() {
        use crate::objects::{ObjectDefinition, ObjectInstance, ObjectKind};
        let text = "#proc1\nLock\nEndDelay 0.05\n#end\n#links\ndestroy 1 0 1\n#end\n";
        let mut session = mission_session(text);
        session.rockets = RocketDefinitions(vec![RocketDefinition {
            model_name: "".into(),
            power: 10,
            raw_fields: vec![],
        }]);
        let definition = ObjectDefinition {
            collision_radius: 32,
            shadow: 0,
            bmp_object: 0,
            bmp_z: 0,
            action_code: 0,
            blow_up: 0,
            break_limit: 10,
            ambient_sound: 0,
            break_sound: 0,
            model_name: "lock.3o".into(),
            animation_name: None,
        };
        let placement = ObjectInstance {
            kind: ObjectKind::LevelObject,
            definition_index: 0,
            grid_cell: [1, 0],
            position: [1.5, 0.5],
            angle_radians: 0.0,
            flags: 0,
            difficulty_flags: 2,
            extra: 0,
            size: 0,
        };
        session.set_objects(ObjectCombat::new(
            &[placement],
            &ObjectDefinitions {
                items: vec![],
                level_objects: vec![definition],
            },
            &Script::parse(text),
            Difficulty::Normal,
        ));
        let mut raw = [0u8; 4096];
        raw[64] = 163;
        session.processes = ProcessRuntime::with_objects_and_hazards(
            &Script::parse(text),
            &session.world,
            crate::process_runtime::ProcessCapabilities::default(),
            &raw,
            &[crate::process_runtime::ProcessObject {
                cell: [1, 0],
                position: [1.5, 0.5],
                angle_radians: 0.0,
                elevation_raw: 0.0,
                blocking_radius: 0.125,
            }],
        )
        .unwrap();
        let mut projectile_session = session.clone();
        projectile_session.game.inventory.weapons[3] = true;
        projectile_session.game.inventory.ammo[3] = 1;
        projectile_session.game.weapons.0[3].rocket_type = 1;
        projectile_session.rockets.0.push(RocketDefinition {
            model_name: "disk.3o".into(),
            power: 10,
            raw_fields: vec![0, 0, 0, 20, 10],
        });
        let first = projectile_session.step(SessionInput {
            fire: true,
            select_weapon: Some(3),
            ..SessionInput::default()
        });
        assert!(matches!(
            first.shot,
            Some(FireResult::Fired(ShotImpact::Launch { .. }))
        ));
        let mut projectile_destroyed = false;
        let mut projectile_completed = false;
        for _ in 0..30 {
            let event = projectile_session.step(SessionInput::default());
            projectile_destroyed |= event.object_hits.contains(&([1, 0], true));
            projectile_completed |= event.mission.contains(&MissionEvent::LevelComplete);
        }
        assert!(projectile_destroyed && projectile_completed);
        assert!(projectile_session.processes.object_pose([1, 0]).is_none());
        let events = session.step(SessionInput {
            fire: true,
            ..SessionInput::default()
        });
        assert_eq!(events.object_hits, vec![([1, 0], true)]);
        assert!(session.processes.object_pose([1, 0]).is_none());
        assert!(matches!(
            events.shot,
            Some(FireResult::Fired(ShotImpact::Object {
                destroyed: true,
                ..
            }))
        ));
        let mut completed = false;
        for _ in 0..10 {
            completed |= session
                .step(SessionInput::default())
                .mission
                .contains(&MissionEvent::LevelComplete);
        }
        assert!(completed);
    }
    #[test]
    fn process_object_updates_shootable_and_pickup_positions_then_invalidates_on_collection() {
        use crate::objects::{ObjectDefinition, ObjectInstance, ObjectKind};
        use crate::process_runtime::{ProcessCapabilities, ProcessObject};
        let text =
            "#proc1\n#action\nmove 2 0 256 0\n#end\n#links\nfloor 0 0 1\nshoot 2 0 1\n#end\n";
        let script = Script::parse(text);
        let mut session = mission_session(text);
        let mut raw = [0u8; 4096];
        raw[128] = 163;
        let pose = ProcessObject {
            cell: [2, 0],
            position: [2.5, 0.5],
            angle_radians: 0.0,
            elevation_raw: 0.0,
            blocking_radius: 0.0,
        };
        session.processes = ProcessRuntime::with_objects_and_hazards(
            &script,
            &session.world,
            ProcessCapabilities::default(),
            &raw,
            &[pose],
        )
        .unwrap();
        let definition = ObjectDefinition {
            collision_radius: 0,
            shadow: 0,
            bmp_object: -1,
            bmp_z: 0,
            action_code: 141,
            blow_up: 0,
            break_limit: 0,
            ambient_sound: 0,
            break_sound: 0,
            model_name: "key.3o".into(),
            animation_name: None,
        };
        let instance = ObjectInstance {
            kind: ObjectKind::LevelObject,
            definition_index: 0,
            grid_cell: [2, 0],
            position: [2.5, 0.5],
            angle_radians: 0.0,
            flags: 0,
            difficulty_flags: 2,
            extra: 0,
            size: 0,
        };
        let definitions = ObjectDefinitions {
            items: vec![],
            level_objects: vec![definition],
        };
        session.game = GameInventory::new(
            session.game.weapons.clone(),
            std::slice::from_ref(&instance),
            &definitions,
            Difficulty::Normal,
        );
        session.set_objects(ObjectCombat::new(
            &[instance],
            &definitions,
            &script,
            Difficulty::Normal,
        ));
        let mut saw_effect = false;
        for _ in 0..80 {
            saw_effect |= session
                .step(SessionInput::default())
                .process_effects
                .iter()
                .any(|event| matches!(event, ProcessEffect::ObjectPose { cell: [2, 0], .. }));
        }
        assert!(saw_effect);
        assert!((session.processes.object_pose([2, 0]).unwrap().position[0] - 3.5).abs() < 0.001);
        assert!(session
            .objects
            .as_ref()
            .unwrap()
            .raycast([3.0, 0.5], [1.0, 0.0], 1.0, 0.0)
            .is_some());
        session.world.player.position = [3.5, 0.5];
        let event = session.step(SessionInput::default());
        assert_eq!(event.pickups.len(), 1);
        assert!(session.game.inventory.keys[0]);
        assert!(session.processes.object_pose([2, 0]).is_none());
        assert_eq!(
            session.objects.as_ref().unwrap().is_active([2, 0]),
            Some(false)
        );
    }
    #[test]
    fn process_object_radius_blocks_player_until_removed() {
        use crate::process_runtime::{ProcessCapabilities, ProcessObject};
        let mut session = mission_session("");
        let mut raw = [0u8; 4096];
        raw[64] = 163;
        let pose = ProcessObject {
            cell: [1, 0],
            position: [1.5, 0.5],
            angle_radians: 0.0,
            elevation_raw: 0.0,
            blocking_radius: 0.3,
        };
        session.processes = ProcessRuntime::with_objects_and_hazards(
            &Script::default(),
            &session.world,
            ProcessCapabilities::default(),
            &raw,
            &[pose],
        )
        .unwrap();
        for _ in 0..120 {
            session.step(SessionInput {
                movement: Input {
                    forward: 1.0,
                    ..Default::default()
                },
                ..Default::default()
            });
        }
        assert!((session.world.player.position[0] - (1.5 - 0.3 - 0.18)).abs() < 0.002);
        assert!(session.processes.remove_object([1, 0]));
        for _ in 0..40 {
            session.step(SessionInput {
                movement: Input {
                    forward: 1.0,
                    ..Default::default()
                },
                ..Default::default()
            });
        }
        assert!(session.world.player.position[0] > 1.5);
    }
    #[test]
    fn raised_objects_allow_clearance_but_low_objects_block() {
        use crate::process_runtime::{ProcessCapabilities, ProcessObject};
        let mut session = mission_session("");
        let mut raw = [0u8; 4096];
        raw[64] = 163;
        let mut pose = ProcessObject {
            cell: [1, 0],
            position: [1.5, 0.5],
            angle_radians: 0.0,
            elevation_raw: 96.0,
            blocking_radius: 0.3,
        };
        session.processes = ProcessRuntime::with_objects_and_hazards(
            &Script::default(),
            &session.world,
            ProcessCapabilities::default(),
            &raw,
            &[pose],
        )
        .unwrap();
        for _ in 0..60 {
            session.step(SessionInput {
                movement: Input {
                    forward: 1.0,
                    ..Default::default()
                },
                ..Default::default()
            });
        }
        assert!(session.world.player.position[0] > 1.5);
        session.world.player.position = [0.5, 0.5];
        pose.elevation_raw = 32.0;
        session.processes = ProcessRuntime::with_objects_and_hazards(
            &Script::default(),
            &session.world,
            ProcessCapabilities::default(),
            &raw,
            &[pose],
        )
        .unwrap();
        for _ in 0..60 {
            session.step(SessionInput {
                movement: Input {
                    forward: 1.0,
                    ..Default::default()
                },
                ..Default::default()
            });
        }
        assert!((session.world.player.position[0] - 1.02).abs() < 0.002);
    }
    #[test]
    fn player_can_escape_initial_wall_and_object_overlap() {
        use crate::process_runtime::{ProcessCapabilities, ProcessObject};
        use crate::world::Wall;
        let mut session = mission_session("");
        session.world.player.position = [0.9, 0.5];
        session.world.walls.push(Wall {
            a: [1.0, 0.0],
            b: [1.0, 1.0],
            texture: 1,
            blocks_movement: true,
            cell: Some([1, 0]),
        });
        session.base_wall_blocking.push(true);
        session.wall_elevations.push(0.0);
        let mut raw = [0u8; 4096];
        raw[128] = 163;
        let pose = ProcessObject {
            cell: [2, 0],
            position: [0.9, 0.5],
            angle_radians: 0.0,
            elevation_raw: 0.0,
            blocking_radius: 0.3,
        };
        session.processes = ProcessRuntime::with_objects_and_hazards(
            &Script::default(),
            &session.world,
            ProcessCapabilities::default(),
            &raw,
            &[pose],
        )
        .unwrap();
        for _ in 0..20 {
            session.step(SessionInput {
                movement: Input {
                    forward: -1.0,
                    ..Default::default()
                },
                ..Default::default()
            });
        }
        assert!(session.world.player.position[0] < 0.3);
    }
    #[test]
    fn session_jump_and_restart_clone_preserve_vertical_state() {
        let mut session = mission_session("");
        let initial = session.clone();
        let first = session.step(SessionInput {
            jump: true,
            ..Default::default()
        });
        assert!(first.vertical.jumped);
        assert!(session.vertical.feet() > 0.0);
        let airborne = session.step(SessionInput {
            jump: true,
            ..Default::default()
        });
        assert!(!airborne.vertical.jumped);
        for _ in 0..120 {
            session.step(SessionInput::default());
        }
        assert!(session.vertical.grounded());
        assert!((session.vertical.feet() - 0.0).abs() < 1e-6);
        assert!(initial.vertical.grounded());
        assert_eq!(initial.vertical.feet(), 0.0);
    }
    #[test]
    fn jumping_under_raised_solid_gate_hits_its_underside() {
        use crate::world::Wall;
        let mut session = mission_session("");
        session.world.player.position = [1.0, 0.5];
        session.world.walls.push(Wall {
            a: [1.0, 0.0],
            b: [1.0, 1.0],
            texture: 1,
            blocks_movement: false,
            cell: Some([1, 0]),
        });
        session.base_wall_blocking.push(true);
        session.wall_elevations.push(80.0);
        session.elevation_by_cell[64] = 80.0;
        let first = session.step(SessionInput {
            jump: true,
            ..Default::default()
        });
        assert!(first.vertical.jumped);
        let mut hit = false;
        for _ in 0..100 {
            let event = session.step(SessionInput::default());
            hit |= event.vertical.hit_ceiling;
            assert!(session.vertical.head() <= 1.25 + 1e-5);
        }
        assert!(hit);
        assert!(session.vertical.grounded());
    }
    #[test]
    fn raised_wall_becomes_passable_and_remains_visible_as_elevated_geometry() {
        use crate::process_runtime::ProcessCapabilities;
        use crate::world::Wall;
        let text = "#proc1\n#action\nup 1 0 80\n#end\n#links\nfloor 0 0 1\n#end\n";
        let old = mission_session(text);
        let world = World {
            player: Player {
                position: [0.5, 0.5],
                angle: 0.0,
            },
            walls: vec![Wall {
                a: [1.0, 0.0],
                b: [1.0, 1.0],
                texture: 1,
                blocks_movement: true,
                cell: Some([1, 0]),
            }],
        };
        let script = Script::parse(text);
        let processes = ProcessRuntime::with_capabilities(
            &script,
            &world,
            ProcessCapabilities {
                wall_elevation: true,
                initial_cell_types: None,
            },
        );
        let mut session = Session::new(LevelSetup {
            world,
            processes,
            actors: old.actors,
            actor_definitions: old.actor_definitions,
            game: old.game,
            rockets: RocketDefinitions(vec![]),
            shot_blocking: [true; 128],
            difficulty: Difficulty::Normal,
        });
        for _ in 0..120 {
            session.step(SessionInput::default());
        }
        assert!((session.wall_elevations[0] - 80.0).abs() < 0.001);
        assert!(!session.world.walls[0].blocks_movement);
        assert!(!Session::blocks_shot(
            &session.world.walls[0],
            &session.shot_blocking,
            &session.elevation_by_cell,
            1.0
        ));
        assert!(Session::blocks_shot(
            &session.world.walls[0],
            &session.shot_blocking,
            &session.elevation_by_cell,
            1.3
        ));
    }
    #[test]
    fn script_hazard_applies_periodic_damage_to_player() {
        use crate::process_runtime::ProcessCapabilities;
        let text =
            "#proc1\n#action\nnonstop\ndeath 0 0 0 0 0 128 30\n#end\n#links\nfloor 0 0 1\n#end\n";
        let mut session = mission_session(text);
        let cells = [0u8; 4096];
        session.processes = ProcessRuntime::with_hazards(
            &Script::parse(text),
            &session.world,
            ProcessCapabilities::default(),
            &cells,
        );
        for _ in 0..20 {
            session.step(SessionInput::default());
        }
        assert_eq!(session.game.inventory.health, 70);
    }
    #[test]
    fn session_fires_damages_enemies_and_consumes_ammunition() {
        let world = World {
            walls: vec![],
            player: Player {
                position: [0.0, 0.0],
                angle: 0.0,
            },
        };
        let process = ProcessRuntime::new(&Script::default(), &world);
        let placement = Monster {
            position: [512, 0],
            id: 101,
            angle: 0,
            flags: 2,
        };
        let actors = ActorState {
            player_spawns: vec![],
            actors: vec![Actor {
                source_index: 0,
                definition_index: 1,
                placement,
                position: [2.0, 0.0],
                angle_radians: 0.0,
                health: 30,
            }],
            unresolved: vec![],
        };
        let definition = ActorDefinition {
            model_name: "test.car".into(),
            wall_radius: 64,
            actor_radius: 64,
            speed: 0,
            rotation_speed: 0,
            health: 30,
            kick: 0,
            rocket: -1,
            separation_limit: 0,
            extra_fields: vec![],
            raw_line: "".into(),
        };
        let actor_definitions = ActorDefinitions {
            entries: vec![definition.clone(), definition],
        };
        let weapon = WeaponDefinition {
            rocket_type: 0,
            reload_time: 1,
            vertical_shift: 0,
            projectile_height: 0,
            ammo_per_portion: 1,
            ammo_limit: 10,
            ammo_on_weapon_pickup: 1,
            projectile_count: 1,
        };
        let game = GameInventory::new(
            WeaponDefinitions([weapon; 8]),
            &[],
            &ObjectDefinitions {
                items: vec![],
                level_objects: vec![],
            },
            Difficulty::Normal,
        );
        let rockets = RocketDefinitions(vec![RocketDefinition {
            model_name: "".into(),
            power: 10,
            raw_fields: vec![],
        }]);
        let mut session = Session::new(LevelSetup {
            world,
            processes: process,
            actors,
            actor_definitions,
            game,
            rockets,
            shot_blocking: [true; 128],
            difficulty: Difficulty::Normal,
        });
        let initial = session.clone();
        let mut collision_session = initial.clone();
        for _ in 0..120 {
            collision_session.step(SessionInput {
                movement: Input {
                    forward: 1.0,
                    ..Input::default()
                },
                ..SessionInput::default()
            });
        }
        assert!(
            collision_session.world.player.position[0] < 1.6,
            "player should not pass through the living enemy"
        );
        let events = session.step(SessionInput {
            fire: true,
            ..SessionInput::default()
        });
        assert!(matches!(
            events.shot,
            Some(FireResult::Fired(ShotImpact::Actor { damage: 10, .. }))
        ));
        assert_eq!(session.actors.actors[0].health, 20);
        assert_eq!(session.game.inventory.ammo[0], 9);
        assert_eq!(initial.game.inventory.ammo[0], 10);
        assert_eq!(initial.actors.actors[0].health, 30);
        session.actor_definitions.entries[1].rocket = 0;
        let events = session.step(SessionInput::default());
        assert!(matches!(
            events.enemy_attacks.first().map(|attack| attack.kind),
            Some(EnemyAttackKind::Ranged { instant: true, .. })
        ));
        assert_eq!(session.game.inventory.health, 90);
        let mut projectile_session = initial;
        projectile_session.game.inventory.weapons[3] = true;
        projectile_session.game.inventory.ammo[3] = 2;
        projectile_session.game.weapons.0[3].rocket_type = 1;
        projectile_session.rockets.0.push(RocketDefinition {
            model_name: "disk.3o".into(),
            power: 12,
            raw_fields: vec![0, 0, 0, 20, 12],
        });
        let event = projectile_session.step(SessionInput {
            fire: true,
            select_weapon: Some(3),
            ..SessionInput::default()
        });
        assert!(matches!(
            event.shot,
            Some(FireResult::Fired(ShotImpact::Launch { rocket_id: 1 }))
        ));
        assert_eq!(projectile_session.game.inventory.ammo[3], 1);
        for _ in 0..40 {
            projectile_session.step(SessionInput::default());
        }
        assert_eq!(projectile_session.actors.actors[0].health, 18);
    }
    #[test]
    fn staged_destroy_replaces_object_and_failed_preflight_restores_hit() {
        use crate::objects::{ObjectDefinition, ObjectInstance, ObjectKind};
        use crate::process_runtime::{ProcessCapabilities, ProcessObject};
        let text = "#proc1\n#action\nexplode 3 0\nquake 2 0 3 1 10\n#end\n#proc2\n#action\nplayani 99\nunlock 3\n#end\n#proc3\nLock\nEndDelay 1\n#end\n#proc4\nLock\nEndDelay 1\n#end\n#links\nshoot 1 0 2\ndestroy 1 0 1\ndestroy 1 0 4\nlink 5 0 3\nlink 6 0 4\n#end\n";
        let script = Script::parse(text);
        let mut session = mission_session(text);
        session.rockets = RocketDefinitions(vec![RocketDefinition {
            model_name: "".into(),
            power: 10,
            raw_fields: vec![],
        }]);
        let def = |name: &str, health: i32| ObjectDefinition {
            collision_radius: 32,
            shadow: 0,
            bmp_object: 0,
            bmp_z: 0,
            action_code: 0,
            blow_up: 0,
            break_limit: health,
            ambient_sound: 0,
            break_sound: 0,
            model_name: name.into(),
            animation_name: None,
        };
        let definitions = ObjectDefinitions {
            items: vec![],
            level_objects: vec![def("heart.3o", 10), def("gate.3o", 0), def("dummy.3o", 0)],
        };
        let placement = |cell: [usize; 2], definition_index| ObjectInstance {
            kind: ObjectKind::LevelObject,
            definition_index,
            grid_cell: cell,
            position: [cell[0] as f32 + 0.5, cell[1] as f32 + 0.5],
            angle_radians: 0.0,
            flags: 0,
            difficulty_flags: 2,
            extra: 0,
            size: 0,
        };
        let objects = vec![placement([1, 0], 0), placement([3, 0], 1)];
        let mut cells = [0; 4096];
        cells[64] = 163;
        cells[192] = 164;
        let poses: Vec<_> = objects
            .iter()
            .map(|o| ProcessObject {
                cell: [o.grid_cell[0] as u8, o.grid_cell[1] as u8],
                position: o.position,
                angle_radians: 0.0,
                elevation_raw: 0.0,
                blocking_radius: 0.125,
            })
            .collect();
        session.processes = ProcessRuntime::with_staged_actions(
            &script,
            &session.world,
            ProcessCapabilities::default(),
            &cells,
            &poses,
        )
        .unwrap();
        session.set_objects(ObjectCombat::new(
            &objects,
            &definitions,
            &script,
            Difficulty::Normal,
        ));
        let states = ObjectStates::new(&objects, &definitions).unwrap();
        session.set_scripted_objects(ScriptedObjects::new(
            states,
            definitions,
            PresentationManifest::default(),
            &script,
            true,
        ));
        let mut projectile_retry = session.clone();
        projectile_retry.game.inventory.weapons[3] = true;
        projectile_retry.game.inventory.ammo[3] = 1;
        projectile_retry.game.weapons.0[3].rocket_type = 1;
        projectile_retry.rockets.0.push(RocketDefinition {
            model_name: "disk.3o".into(),
            power: 10,
            raw_fields: vec![0, 0, 0, 20, 10],
        });
        projectile_retry.step(SessionInput {
            fire: true,
            select_weapon: Some(3),
            ..Default::default()
        });
        let mut projectile_hit = false;
        for _ in 0..30 {
            let events = projectile_retry.step(SessionInput::default());
            if events.object_hits.contains(&([1, 0], false)) {
                projectile_hit = true;
                assert!(!events
                    .process_effects
                    .iter()
                    .any(|e| matches!(e, ProcessEffect::PlayAnimation { object_id: 99 })));
            }
        }
        assert!(projectile_hit);
        assert_eq!(
            projectile_retry.processes.activate_at("link", [5, 0])[0].outcome,
            ProcessOutcome::Locked
        );
        assert_eq!(
            projectile_retry.objects.as_ref().unwrap().is_active([1, 0]),
            Some(true)
        );
        let mut retry = session.clone();
        let blocked = retry.step(SessionInput {
            fire: true,
            ..Default::default()
        });
        assert_eq!(blocked.object_hits, vec![([1, 0], false)]);
        assert!(blocked.object_transitions.is_empty());
        assert!(!blocked
            .process_effects
            .iter()
            .any(|e| matches!(e, ProcessEffect::PlayAnimation { object_id: 99 })));
        assert_eq!(
            retry.processes.activate_at("link", [5, 0])[0].outcome,
            ProcessOutcome::Locked
        );
        assert_eq!(
            retry.processes.activate_at("link", [6, 0])[0].outcome,
            ProcessOutcome::Locked
        );
        assert_eq!(
            retry
                .scripted_objects
                .as_ref()
                .unwrap()
                .states
                .get([1, 0])
                .unwrap()
                .health,
            10
        );
        assert_eq!(
            retry.objects.as_ref().unwrap().is_active([1, 0]),
            Some(true)
        );
        assert_eq!(retry.processes.cell_type([3, 0]), Some(164));
        session
            .scripted_objects
            .as_mut()
            .unwrap()
            .manifest
            .allow([3, 0], 165);
        let result = session.step(SessionInput {
            fire: true,
            ..Default::default()
        });
        assert_eq!(result.object_hits, vec![([1, 0], true)]);
        assert_eq!(result.object_transitions.len(), 1);
        assert_eq!(result.object_transitions[0].new_type, 165);
        assert_eq!(result.quake_changes.len(), 1);
        assert!(result
            .process_effects
            .iter()
            .any(|e| matches!(e, ProcessEffect::PlayAnimation { object_id: 99 })));
        assert_eq!(
            session.processes.activate_at("link", [5, 0])[0].outcome,
            ProcessOutcome::Accepted
        );
        assert_eq!(
            session.processes.activate_at("link", [6, 0])[0].outcome,
            ProcessOutcome::Accepted
        );
        assert_eq!(session.processes.cell_type([3, 0]), Some(165));
        assert_eq!(
            session
                .processes
                .object_pose([3, 0])
                .unwrap()
                .blocking_radius,
            0.0
        );
        assert_eq!(
            session
                .scripted_objects
                .as_ref()
                .unwrap()
                .quake
                .cell([2, 0]),
            Some(10)
        );
    }
    #[test]
    fn staged_floor_and_return_preserve_order_and_atomic_wall_motion() {
        use crate::objects::{ObjectDefinition, ObjectInstance, ObjectKind};
        use crate::process_runtime::{ProcessCapabilities, ProcessObject};
        use crate::world::Wall;
        let text = "#proc1\n#action\nchange 3 0 164\nmove 2 0 256 0\nunlock 2\n#end\n#proc2\nLock\n#action\nplayani 55\n#end\n#links\nfloor 0 0 1\nfloor 0 0 2\nreturn 0 0 1\n#end\n";
        let script = Script::parse(text);
        let mut session = mission_session(text);
        session.world.walls.push(Wall {
            a: [2.0, 0.0],
            b: [2.0, 1.0],
            texture: 1,
            blocks_movement: true,
            cell: Some([2, 0]),
        });
        session.base_wall_blocking.push(true);
        session.wall_elevations.push(0.0);
        let def = |name: &str| ObjectDefinition {
            collision_radius: 32,
            shadow: 0,
            bmp_object: 0,
            bmp_z: 0,
            action_code: 0,
            blow_up: 0,
            break_limit: 0,
            ambient_sound: 0,
            break_sound: 0,
            model_name: name.into(),
            animation_name: None,
        };
        let defs = ObjectDefinitions {
            items: vec![],
            level_objects: vec![def("a.3o"), def("b.3o")],
        };
        let instance = ObjectInstance {
            kind: ObjectKind::LevelObject,
            definition_index: 0,
            grid_cell: [3, 0],
            position: [3.5, 0.5],
            angle_radians: 0.0,
            flags: 0,
            difficulty_flags: 2,
            extra: 0,
            size: 0,
        };
        let mut cells = [0; 4096];
        cells[3 * 64] = 163;
        let pose = ProcessObject {
            cell: [3, 0],
            position: instance.position,
            angle_radians: 0.0,
            elevation_raw: 0.0,
            blocking_radius: 0.125,
        };
        session.processes = ProcessRuntime::with_staged_actions(
            &script,
            &session.world,
            ProcessCapabilities::default(),
            &cells,
            &[pose],
        )
        .unwrap();
        session.set_objects(ObjectCombat::new(
            std::slice::from_ref(&instance),
            &defs,
            &script,
            Difficulty::Normal,
        ));
        let states = ObjectStates::new(&[instance], &defs).unwrap();
        session.set_scripted_objects(ScriptedObjects::new(
            states,
            defs,
            PresentationManifest::default(),
            &script,
            false,
        ));
        let mut failed = session.clone();
        let rejected = failed.step(SessionInput::default());
        assert!(rejected.object_transitions.is_empty());
        assert!(!rejected
            .process_effects
            .iter()
            .any(|e| matches!(e, ProcessEffect::PlayAnimation { object_id: 55 })));
        assert_eq!(failed.world.walls[0].a[0], 2.0);
        assert_eq!(failed.processes.cell_type([3, 0]), Some(163));
        session
            .scripted_objects
            .as_mut()
            .unwrap()
            .manifest
            .allow([3, 0], 164);
        let use_text = text.replace("floor 0 0 1\nfloor 0 0 2", "link 0 0 1\nlink 0 0 2");
        let use_script = Script::parse(&use_text);
        let mut use_session = session.clone();
        use_session.processes = ProcessRuntime::with_staged_actions(
            &use_script,
            &use_session.world,
            ProcessCapabilities::default(),
            &cells,
            &[pose],
        )
        .unwrap();
        let previous = use_session.scripted_objects.take().unwrap();
        use_session.set_scripted_objects(ScriptedObjects::new(
            previous.states,
            previous.definitions,
            previous.manifest,
            &use_script,
            false,
        ));
        // Avoid the same-cell return here, which is intentionally processed in source order.
        use_session.processes = ProcessRuntime::with_staged_actions(
            &Script::parse(&use_text.replace("return 0 0 1\n", "")),
            &use_session.world,
            ProcessCapabilities::default(),
            &cells,
            &[pose],
        )
        .unwrap();
        let use_events = use_session.step(SessionInput {
            interact: true,
            ..Default::default()
        });
        assert_eq!(use_events.object_transitions.len(), 1);
        assert!(use_events
            .process_effects
            .iter()
            .any(|e| matches!(e, ProcessEffect::PlayAnimation { object_id: 55 })));
        assert!(use_session.world.walls[0].a[0] > 2.0);
        let accepted = session.step(SessionInput::default());
        assert_eq!(accepted.object_transitions.len(), 1);
        assert!(accepted
            .process_effects
            .iter()
            .any(|e| matches!(e, ProcessEffect::PlayAnimation { object_id: 55 })));
        assert!(session.world.walls[0].a[0] > 2.0);
        session.step(SessionInput {
            interact: true,
            ..Default::default()
        });
        for _ in 0..10 {
            session.step(SessionInput::default());
        }
        assert!((session.world.walls[0].a[0] - 2.0).abs() < 0.001);
        assert_eq!(session.processes.cell_type([3, 0]), Some(164));
    }
    #[test]
    fn staged_pickup_rolls_back_on_missing_replacement_then_retries() {
        use crate::objects::{ObjectDefinition, ObjectInstance, ObjectKind};
        use crate::process_runtime::{ProcessCapabilities, ProcessObject};
        let text = "#proc1\n#action\nchange 3 0 164\n#end\n#links\nlink 0 0 1\n#end\n";
        let script = Script::parse(text);
        let mut session = mission_session(text);
        let def = |name: &str, code| ObjectDefinition {
            collision_radius: 0,
            shadow: 0,
            bmp_object: 0,
            bmp_z: 0,
            action_code: code,
            blow_up: 0,
            break_limit: 0,
            ambient_sound: 0,
            break_sound: 0,
            model_name: name.into(),
            animation_name: None,
        };
        let defs = ObjectDefinitions {
            items: vec![def("key.3o", 142)],
            level_objects: vec![def("a.3o", 0), def("b.3o", 0)],
        };
        let key = ObjectInstance {
            kind: ObjectKind::Item,
            definition_index: 0,
            grid_cell: [0, 0],
            position: [0.5, 0.5],
            angle_radians: 0.0,
            flags: 0,
            difficulty_flags: 2,
            extra: 0,
            size: 0,
        };
        let object = ObjectInstance {
            kind: ObjectKind::LevelObject,
            definition_index: 0,
            grid_cell: [3, 0],
            position: [3.5, 0.5],
            ..key.clone()
        };
        let instances = [key, object];
        session.game = GameInventory::new(
            session.game.weapons.clone(),
            &instances,
            &defs,
            Difficulty::Normal,
        );
        let mut cells = [0; 4096];
        cells[3 * 64] = 163;
        let pose = ProcessObject {
            cell: [3, 0],
            position: [3.5, 0.5],
            angle_radians: 0.0,
            elevation_raw: 0.0,
            blocking_radius: 0.0,
        };
        session.processes = ProcessRuntime::with_staged_actions(
            &script,
            &session.world,
            ProcessCapabilities::default(),
            &cells,
            &[pose],
        )
        .unwrap();
        session.set_objects(ObjectCombat::new(
            &instances,
            &defs,
            &script,
            Difficulty::Normal,
        ));
        let states = ObjectStates::new(&instances, &defs).unwrap();
        session.set_scripted_objects(ScriptedObjects::new(
            states,
            defs,
            PresentationManifest::default(),
            &script,
            false,
        ));
        let rejected = session.step(SessionInput::default());
        assert!(rejected.pickups.is_empty());
        assert!(!session.game.inventory.keys[1]);
        assert_eq!(session.game.pickups.is_visible([0, 0]), Some(true));
        assert_eq!(session.processes.cell_type([3, 0]), Some(163));
        session
            .scripted_objects
            .as_mut()
            .unwrap()
            .manifest
            .allow([3, 0], 164);
        let accepted = session.step(SessionInput::default());
        assert_eq!(accepted.pickups.len(), 1);
        assert!(session.game.inventory.keys[1]);
        assert_eq!(session.game.pickups.is_visible([0, 0]), Some(false));
        assert_eq!(session.processes.cell_type([3, 0]), Some(164));
        assert_eq!(accepted.object_transitions.len(), 1);
    }
    #[test]
    fn grenade_opt_in_explodes_once_and_hurts_owner() {
        use crate::world::Wall;
        let mut session = mission_session("");
        session.world.walls.push(Wall {
            a: [1.5, 0.0],
            b: [1.5, 1.0],
            texture: 1,
            blocks_movement: true,
            cell: None,
        });
        session.base_wall_blocking.push(true);
        session.wall_elevations.push(0.0);
        session.rockets = RocketDefinitions(vec![RocketDefinition {
            model_name: "grenade.3o".into(),
            power: 100,
            raw_fields: vec![2, 0, 424, 42, 100, 0, 0, 1, 0, 0, 0, 18],
        }]);
        session.game.inventory.weapons[5] = true;
        session.game.inventory.ammo[5] = 1;
        let mut disabled = session.clone();
        assert_eq!(
            disabled
                .step(SessionInput {
                    fire: true,
                    select_weapon: Some(5),
                    ..Default::default()
                })
                .shot,
            Some(FireResult::Unsupported)
        );
        assert_eq!(disabled.game.inventory.ammo[5], 1);
        let mut below_threshold = session.clone();
        below_threshold.rockets.0[0].raw_fields[2] = 200;
        below_threshold.set_explosions_enabled(true);
        below_threshold.step(SessionInput {
            fire: true,
            select_weapon: Some(5),
            ..Default::default()
        });
        let mut impacts = 0;
        for _ in 0..20 {
            let events = below_threshold.step(SessionInput::default());
            impacts += events.projectile_impacts.len();
            assert!(events.explosions.is_empty());
        }
        assert_eq!(impacts, 1);
        assert_eq!(below_threshold.game.inventory.health, 100);
        session.set_explosions_enabled(true);
        let fire = session.step(SessionInput {
            fire: true,
            select_weapon: Some(5),
            ..Default::default()
        });
        assert!(matches!(
            fire.shot,
            Some(FireResult::Fired(ShotImpact::Launch { rocket_id: 0 }))
        ));
        let mut seen = 0;
        for _ in 0..20 {
            let events = session.step(SessionInput::default());
            seen += events.explosions.len();
            if !events.explosions.is_empty() {
                assert!(events
                    .explosion_damage
                    .iter()
                    .any(|e| e.target == TargetId::Player
                        && e.owner == Owner::Player
                        && e.damage > 0));
                assert!(events.damage > 0);
            }
        }
        assert_eq!(seen, 1);
        assert!(session.game.inventory.health < 100);
    }
    #[test]
    fn gravity_bounce_and_homing_grenades_are_rejected_without_ammo_loss() {
        for index in [1, 5, 9] {
            let mut session = mission_session("");
            let mut raw = vec![2, 0, 424, 42, 100, 0, 0, 0, 0, 0, 0];
            raw[index] = 1;
            session.rockets = RocketDefinitions(vec![RocketDefinition {
                model_name: "grenade.3o".into(),
                power: 100,
                raw_fields: raw,
            }]);
            session.game.inventory.weapons[5] = true;
            session.game.inventory.ammo[5] = 1;
            session.set_explosions_enabled(true);
            assert_eq!(
                session
                    .step(SessionInput {
                        fire: true,
                        select_weapon: Some(5),
                        ..Default::default()
                    })
                    .shot,
                Some(FireResult::Unsupported)
            );
            assert_eq!(session.game.inventory.ammo[5], 1);
            assert!(session.projectiles.active().is_empty());
        }
    }
    #[test]
    fn explosion_applies_only_one_hit_to_direct_actor_and_skips_zero_damage() {
        let mut session = mission_session("");
        let definition = ActorDefinition {
            model_name: "a.car".into(),
            wall_radius: 32,
            actor_radius: 32,
            speed: 0,
            rotation_speed: 0,
            health: 100,
            kick: 0,
            rocket: -1,
            separation_limit: 0,
            extra_fields: vec![],
            raw_line: String::new(),
        };
        session.actor_definitions.entries.push(definition);
        let placement = Monster {
            position: [512, 0],
            id: 101,
            angle: 0,
            flags: 2,
        };
        session.actors.actors.push(Actor {
            source_index: 7,
            definition_index: 0,
            placement,
            position: [2.0, 0.5],
            angle_radians: 0.0,
            health: 100,
        });
        let explosion = Explosion {
            owner: Owner::Player,
            center: [2.0, 0.5, 0.75],
            radius: 1.0,
            damage: 30,
        };
        let impact = Impact {
            projectile_id: 0,
            owner: Owner::Player,
            rocket_id: 0,
            target: ImpactTarget::Actor { source_index: 7 },
            position: [2.0, 0.5],
            damage: 30,
            time: 0.0,
        };
        let mut events = SessionEvents::default();
        let mut attempts = Vec::new();
        session.apply_explosion(
            explosion,
            ExplosionOrigin::Projectile {
                projectile_id: impact.projectile_id,
                rocket_id: impact.rocket_id,
            },
            Some(impact.target),
            &mut events,
            &mut attempts,
        );
        assert_eq!(session.actors.actors[0].health, 70);
        assert_eq!(
            events
                .explosion_damage
                .iter()
                .filter(|event| event.target == TargetId::Actor { source_index: 7 })
                .count(),
            1
        );
        assert!(!events
            .explosion_damage
            .iter()
            .any(|event| event.target == TargetId::Player));
    }
    #[test]
    fn explosion_object_script_failure_rolls_back_all_damage_and_contact_links() {
        use crate::objects::{ObjectDefinition, ObjectInstance, ObjectKind};
        use crate::process_runtime::{ProcessCapabilities, ProcessObject};
        let text = "#proc1\n#action\nchange 3 0 165\n#end\n#proc2\n#action\nplayani 99\n#end\n#links\nshoot 1 0 2\ndestroy 1 0 1\n#end\n";
        let script = Script::parse(text);
        let mut session = mission_session(text);
        let def = |name: &str, health, blow_up| ObjectDefinition {
            collision_radius: 32,
            shadow: 0,
            bmp_object: 0,
            bmp_z: 0,
            action_code: 0,
            blow_up,
            break_limit: health,
            ambient_sound: 0,
            break_sound: 0,
            model_name: name.into(),
            animation_name: None,
        };
        let defs = ObjectDefinitions {
            items: vec![],
            level_objects: vec![
                def("barrel.3o", 10, 1),
                def("switch.3o", 0, 0),
                def("dummy.3o", 0, 0),
            ],
        };
        let object = |cell: [usize; 2], definition_index| ObjectInstance {
            kind: ObjectKind::LevelObject,
            definition_index,
            grid_cell: cell,
            position: [cell[0] as f32 + 0.5, 0.5],
            angle_radians: 0.0,
            flags: 0,
            difficulty_flags: 2,
            extra: 0,
            size: 0,
        };
        let instances = [object([1, 0], 0), object([3, 0], 1)];
        let mut cells = [0; 4096];
        cells[64] = 163;
        cells[192] = 164;
        let poses: Vec<_> = instances
            .iter()
            .map(|o| ProcessObject {
                cell: [o.grid_cell[0] as u8, 0],
                position: o.position,
                angle_radians: 0.0,
                elevation_raw: 0.0,
                blocking_radius: 0.125,
            })
            .collect();
        session.processes = ProcessRuntime::with_staged_actions(
            &script,
            &session.world,
            ProcessCapabilities::default(),
            &cells,
            &poses,
        )
        .unwrap();
        session.set_objects(ObjectCombat::new(
            &instances,
            &defs,
            &script,
            Difficulty::Normal,
        ));
        session.set_scripted_objects(ScriptedObjects::new(
            ObjectStates::new(&instances, &defs).unwrap(),
            defs,
            PresentationManifest::default(),
            &script,
            false,
        ));
        let mut mined = session.clone();
        mined.set_mines_enabled(true);
        mined.world.player.position = [1.5, 0.5];
        mined.actor_definitions.entries.push(ActorDefinition {
            model_name: "a.car".into(),
            wall_radius: 32,
            actor_radius: 32,
            speed: 0,
            rotation_speed: 0,
            health: 100,
            kick: 0,
            rocket: -1,
            separation_limit: 0,
            extra_fields: vec![],
            raw_line: String::new(),
        });
        let placement = Monster {
            position: [2560, 2560],
            id: 101,
            angle: 0,
            flags: 2,
        };
        mined.actors.actors.push(Actor {
            source_index: 7,
            definition_index: 0,
            placement,
            position: [10.0, 10.0],
            angle_radians: 0.0,
            health: 100,
        });
        mined.ai = EnemyAi::new(&mined.actors, Difficulty::Normal);
        mined.mines.plant(Owner::Player, [10.0, 10.0, 0.0]).unwrap();
        mined.mines.plant(Owner::Player, [1.5, 0.5, 0.0]).unwrap();
        let mut mine_events = SessionEvents::default();
        let mut mine_attempts = Vec::new();
        for _ in 0..explosives::MINE_ARM_TICKS {
            mined.tick_mines(&mut mine_events, &mut mine_attempts);
        }
        assert_eq!(mined.mines()[0].age_ticks, explosives::MINE_ARM_TICKS - 1);
        assert_eq!(mined.game.inventory.health, 100);
        assert_eq!(mined.actors.actors[0].health, 100);
        assert_eq!(mined.mines().len(), 2);
        assert!(mine_events.mine_events.is_empty());
        assert!(mine_events.explosions.is_empty());
        assert!(mine_events.explosion_origins.is_empty());
        assert_eq!(
            mined.objects.as_ref().unwrap().is_active([1, 0]),
            Some(true)
        );
        mined
            .scripted_objects
            .as_mut()
            .unwrap()
            .manifest
            .allow([3, 0], 165);
        mined.tick_mines(&mut mine_events, &mut mine_attempts);
        assert!(mined.mines().is_empty());
        assert_eq!(mine_events.explosions.len(), 2);
        assert_eq!(
            mine_events.explosion_origins,
            vec![
                ExplosionOrigin::Mine { mine_id: 0 },
                ExplosionOrigin::Mine { mine_id: 1 }
            ]
        );
        assert_eq!(mine_events.object_hits, vec![([1, 0], true)]);
        assert!(!mined
            .processes
            .take_effects()
            .iter()
            .any(|effect| matches!(effect, ProcessEffect::PlayAnimation { object_id: 99 })));
        let explosion = Explosion {
            owner: Owner::Player,
            center: [1.5, 0.5, 0.75],
            radius: 2.0,
            damage: 100,
        };
        let impact = Impact {
            projectile_id: 0,
            owner: Owner::Player,
            rocket_id: 0,
            target: ImpactTarget::Object { cell: [1, 0] },
            position: [1.5, 0.5],
            damage: 100,
            time: 0.0,
        };
        let mut events = SessionEvents::default();
        let mut attempts = Vec::new();
        session.apply_explosion(
            explosion,
            ExplosionOrigin::Projectile {
                projectile_id: impact.projectile_id,
                rocket_id: impact.rocket_id,
            },
            Some(impact.target),
            &mut events,
            &mut attempts,
        );
        assert_eq!(session.game.inventory.health, 100);
        assert_eq!(
            session.objects.as_ref().unwrap().is_active([1, 0]),
            Some(true)
        );
        assert_eq!(session.processes.cell_type([3, 0]), Some(164));
        assert!(events.explosions.is_empty());
        assert!(events.explosion_damage.is_empty());
        assert!(session.processes.take_effects().is_empty());
        assert!(attempts.is_empty());
        session
            .scripted_objects
            .as_mut()
            .unwrap()
            .manifest
            .allow([3, 0], 165);
        session.apply_explosion(
            explosion,
            ExplosionOrigin::Projectile {
                projectile_id: impact.projectile_id,
                rocket_id: impact.rocket_id,
            },
            Some(impact.target),
            &mut events,
            &mut attempts,
        );
        assert!(session.game.inventory.health < 100);
        assert_eq!(
            session.objects.as_ref().unwrap().is_active([1, 0]),
            Some(false)
        );
        assert_eq!(session.processes.cell_type([3, 0]), Some(165));
        assert_eq!(events.explosions.len(), 1);
        assert_eq!(events.object_hits, vec![([1, 0], true)]);
        assert!(session
            .processes
            .take_effects()
            .iter()
            .any(|effect| matches!(effect, ProcessEffect::PlayAnimation { object_id: 99 })));
    }
    #[test]
    fn explosion_mode_preserves_legacy_enemy_and_mega_projectile_launches() {
        let mut session = mission_session("");
        let definition = ActorDefinition {
            model_name: "gross.car".into(),
            wall_radius: 32,
            actor_radius: 32,
            speed: 0,
            rotation_speed: 0,
            health: 100,
            kick: 0,
            rocket: 0,
            separation_limit: 0,
            extra_fields: vec![],
            raw_line: String::new(),
        };
        session.actor_definitions.entries.push(definition);
        let placement = Monster {
            position: [4 * 256, 128],
            id: 101,
            angle: 0,
            flags: 2,
        };
        session.actors.actors.push(Actor {
            source_index: 7,
            definition_index: 0,
            placement,
            position: [4.5, 0.5],
            angle_radians: 0.0,
            health: 100,
        });
        session.ai = EnemyAi::new(&session.actors, Difficulty::Normal);
        session.rockets = RocketDefinitions(vec![RocketDefinition {
            model_name: "gravity.3o".into(),
            power: 100,
            raw_fields: vec![4, 3, 640, 42, 100, 1, 0, 0, 0, 1, 0],
        }]);
        session.game.inventory.weapons[7] = true;
        session.game.inventory.ammo[7] = 1;
        session.set_explosions_enabled(true);
        let events = session.step(SessionInput {
            fire: true,
            select_weapon: Some(7),
            ..Default::default()
        });
        assert!(matches!(
            events.shot,
            Some(FireResult::Fired(ShotImpact::Launch { rocket_id: 0 }))
        ));
        assert!(events
            .enemy_attacks
            .iter()
            .any(|attack| matches!(attack.kind, EnemyAttackKind::Ranged { instant: false, .. })));
        assert!(session
            .projectiles
            .active()
            .iter()
            .any(|p| p.owner == Owner::Player));
        assert!(session
            .projectiles
            .active()
            .iter()
            .any(|p| p.owner == Owner::Enemy { source_index: 7 }));
    }
    #[test]
    fn opt_in_mine_places_arms_detonates_and_hurts_owner() {
        let mut session = mission_session("");
        session.game.inventory.weapons[6] = true;
        session.game.inventory.ammo[6] = 2;
        session.game.weapons.0[6].reload_time = 112;
        session.set_mines_enabled(true);
        let placed = session.step(SessionInput {
            fire: true,
            select_weapon: Some(6),
            ..Default::default()
        });
        assert_eq!(placed.shot, Some(FireResult::Fired(ShotImpact::MinePlaced)));
        assert!(matches!(
            &placed.mine_events[..],
            [MineEvent::Placed(Mine {
                id: 0,
                age_ticks: 0,
                ..
            })]
        ));
        assert_eq!(session.game.inventory.ammo[6], 1);
        let cooling = session.step(SessionInput {
            fire: true,
            ..Default::default()
        });
        assert_eq!(cooling.shot, Some(FireResult::CoolingDown));
        assert_eq!(session.mines()[0].age_ticks, 1);
        for _ in 1..explosives::MINE_ARM_TICKS - 1 {
            assert!(session.step(SessionInput::default()).mine_events.is_empty());
        }
        let detonation = session.step(SessionInput::default());
        assert!(matches!(
            &detonation.mine_events[..],
            [
                MineEvent::Armed { id: 0 },
                MineEvent::Detonated { id: 0, .. }
            ]
        ));
        assert_eq!(detonation.explosions.len(), 1);
        assert!(detonation.damage > 0 && session.game.inventory.health < 100);
        assert!(session.mines().is_empty());
        assert!(session.step(SessionInput::default()).mine_events.is_empty());
    }
    #[test]
    fn rejected_mine_placement_refunds_ammo_and_cooldown() {
        let mut invalid = mission_session("");
        invalid.set_mines_enabled(true);
        invalid.game.inventory.weapons[6] = true;
        invalid.game.inventory.ammo[6] = 1;
        invalid.world.player.position = [64.0, 1.0];
        let event = invalid.step(SessionInput {
            fire: true,
            select_weapon: Some(6),
            ..Default::default()
        });
        assert_eq!(event.shot, Some(FireResult::Unsupported));
        assert_eq!(invalid.game.inventory.ammo[6], 1);
        assert_eq!(invalid.combat.cooldown(), 0.0);
        assert!(invalid.mines().is_empty());
        let mut full = mission_session("");
        full.set_mines_enabled(true);
        full.game.inventory.weapons[6] = true;
        full.game.inventory.ammo[6] = 1;
        for _ in 0..explosives::MAX_MINES {
            full.mines.plant(Owner::Player, [0.0, 0.0, 0.0]).unwrap();
        }
        let event = full.step(SessionInput {
            fire: true,
            select_weapon: Some(6),
            ..Default::default()
        });
        assert_eq!(event.shot, Some(FireResult::Unsupported));
        assert_eq!(full.game.inventory.ammo[6], 1);
        assert_eq!(full.combat.cooldown(), 0.0);
        assert_eq!(full.mines().len(), explosives::MAX_MINES);
    }
    #[test]
    fn mine_expires_without_trigger_and_restart_clone_retains_snapshot() {
        let mut session = mission_session("");
        session.set_mines_enabled(true);
        session
            .mines
            .plant(Owner::Player, [10.0, 10.0, 0.0])
            .unwrap();
        let snapshot = session.clone();
        let mut events = SessionEvents::default();
        let mut attempts = Vec::new();
        for _ in 0..explosives::MINE_EXPIRE_TICKS {
            session.tick_mines(&mut events, &mut attempts);
        }
        assert_eq!(session.mines()[0].age_ticks, explosives::MINE_EXPIRE_TICKS);
        session.tick_mines(&mut events, &mut attempts);
        assert!(events.mine_events.contains(&MineEvent::Expired { id: 0 }));
        assert!(session.mines().is_empty());
        assert_eq!(snapshot.mines()[0].age_ticks, 0);
    }
    #[test]
    fn simultaneous_explosion_origins_match_committed_event_order() {
        let mut session = mission_session("");
        session.set_mines_enabled(true);
        session.mines.plant(Owner::Player, [0.5, 0.5, 0.0]).unwrap();
        session.mines.plant(Owner::Player, [0.6, 0.5, 0.0]).unwrap();
        let mut events = SessionEvents::default();
        let mut attempts = Vec::new();
        for _ in 0..explosives::MINE_ARM_TICKS {
            session.tick_mines(&mut events, &mut attempts);
        }
        assert_eq!(
            events.explosion_origins,
            vec![
                ExplosionOrigin::Mine { mine_id: 0 },
                ExplosionOrigin::Mine { mine_id: 1 }
            ]
        );
        assert_eq!(events.explosions.len(), 2);
        assert_eq!(events.explosions[0].center[0], 0.5);
        assert_eq!(events.explosions[1].center[0], 0.6);
        let explosion = Explosion {
            owner: Owner::Player,
            center: [2.0, 2.0, 0.75],
            radius: 1.0,
            damage: 10,
        };
        assert!(session.apply_explosion(
            explosion,
            ExplosionOrigin::Projectile {
                projectile_id: 42,
                rocket_id: 10
            },
            None,
            &mut events,
            &mut attempts
        ));
        assert_eq!(
            events.explosion_origins[2],
            ExplosionOrigin::Projectile {
                projectile_id: 42,
                rocket_id: 10
            }
        );
        assert_eq!(events.explosions[2], explosion);
        assert_eq!(events.explosions.len(), events.explosion_origins.len());
    }
    #[test]
    fn grenade_retries_staged_contact_and_reports_only_applied_blast_damage() {
        use crate::objects::{ObjectDefinition, ObjectInstance, ObjectKind};
        use crate::process_runtime::{ProcessCapabilities, ProcessObject};
        let text = "#proc1\n#action\nchange 2 0 165\n#end\n#links\nshoot 1 0 1\n#end\n";
        let script = Script::parse(text);
        let mut session = mission_session(text);
        let def = |name: &str, health, blow_up| ObjectDefinition {
            collision_radius: 32,
            shadow: 0,
            bmp_object: 0,
            bmp_z: 0,
            action_code: 0,
            blow_up,
            break_limit: health,
            ambient_sound: 0,
            break_sound: 0,
            model_name: name.into(),
            animation_name: None,
        };
        let defs = ObjectDefinitions {
            items: vec![],
            level_objects: vec![
                def("a.3o", 1000, 1),
                def("b.3o", 1000, 1),
                def("dummy.3o", 0, 0),
            ],
        };
        let object = |x: usize, definition_index| ObjectInstance {
            kind: ObjectKind::LevelObject,
            definition_index,
            grid_cell: [x, 0],
            position: [x as f32 + 0.5, 0.5],
            angle_radians: 0.0,
            flags: 0,
            difficulty_flags: 2,
            extra: 0,
            size: 0,
        };
        let instances = [object(1, 0), object(2, 1)];
        let mut cells = [0; 4096];
        cells[64] = 163;
        cells[128] = 164;
        let poses: Vec<_> = instances
            .iter()
            .map(|o| ProcessObject {
                cell: [o.grid_cell[0] as u8, 0],
                position: o.position,
                angle_radians: 0.0,
                elevation_raw: 0.0,
                blocking_radius: 0.125,
            })
            .collect();
        session.processes = ProcessRuntime::with_staged_actions(
            &script,
            &session.world,
            ProcessCapabilities::default(),
            &cells,
            &poses,
        )
        .unwrap();
        session.set_objects(ObjectCombat::new(
            &instances,
            &defs,
            &script,
            Difficulty::Normal,
        ));
        session.set_scripted_objects(ScriptedObjects::new(
            ObjectStates::new(&instances, &defs).unwrap(),
            defs,
            PresentationManifest::default(),
            &script,
            false,
        ));
        session.rockets = RocketDefinitions(vec![RocketDefinition {
            model_name: "grenade.3o".into(),
            power: 100,
            raw_fields: vec![2, 0, 424, 42, 100, 0, 0, 1, 0, 0, 0, 18],
        }]);
        session.game.inventory.weapons[5] = true;
        session.game.inventory.ammo[5] = 1;
        session.set_explosions_enabled(true);
        session.step(SessionInput {
            fire: true,
            select_weapon: Some(5),
            ..Default::default()
        });
        session.rockets.0.push(RocketDefinition {
            model_name: "arrow.3o".into(),
            power: 10,
            raw_fields: vec![1, 0, 120, 0, 10, 0],
        });
        let independent = session
            .projectiles
            .spawn(
                Launch {
                    owner: Owner::Player,
                    rocket_id: 1,
                    origin: [5.0, 5.0],
                    direction: [1.0, 0.0],
                },
                &session.rockets,
            )
            .unwrap();
        for _ in 0..15 {
            let events = session.step(SessionInput::default());
            assert!(events.projectile_impacts.is_empty());
            assert!(events.explosions.is_empty());
        }
        assert_eq!(
            session.projectiles.active().len(),
            2,
            "failed staged contact must retry while the other projectile advances"
        );
        let other = session
            .projectiles
            .active()
            .iter()
            .find(|p| p.id == independent)
            .unwrap();
        assert!((other.position[0] - 7.5).abs() < 1e-4);
        assert_eq!(session.game.inventory.health, 100);
        assert_eq!(session.processes.cell_type([2, 0]), Some(164));
        session
            .scripted_objects
            .as_mut()
            .unwrap()
            .manifest
            .allow([2, 0], 165);
        let events = session.step(SessionInput::default());
        assert_eq!(events.projectile_impacts.len(), 1);
        assert_eq!(events.explosions.len(), 1);
        assert_eq!(session.processes.cell_type([2, 0]), Some(165));
        assert!(events
            .explosion_damage
            .iter()
            .any(|d| d.target == TargetId::Object { cell: [1, 0] }));
        assert!(!events
            .explosion_damage
            .iter()
            .any(|d| d.target == TargetId::Object { cell: [2, 0] }));
        assert!(!events.object_hits.iter().any(|(cell, _)| *cell == [2, 0]));
        assert_eq!(session.projectiles.active().len(), 1);
        assert_eq!(session.projectiles.active()[0].id, independent);
    }
    #[test]
    fn chronological_projectiles_retrace_after_first_kills_actor() {
        let mut session = mission_session("");
        session.actor_definitions.entries.push(ActorDefinition {
            model_name: "a.car".into(),
            wall_radius: 0,
            actor_radius: 0,
            speed: 0,
            rotation_speed: 0,
            health: 10,
            kick: 0,
            rocket: -1,
            separation_limit: 0,
            extra_fields: vec![],
            raw_line: String::new(),
        });
        let placement = Monster {
            position: [256, 128],
            id: 101,
            angle: 0,
            flags: 2,
        };
        session.actors.actors.push(Actor {
            source_index: 7,
            definition_index: 0,
            placement,
            position: [1.0, 0.5],
            angle_radians: 0.0,
            health: 10,
        });
        session.ai = EnemyAi::new(&session.actors, Difficulty::Normal);
        session.rockets = RocketDefinitions(vec![RocketDefinition {
            model_name: "arrow.3o".into(),
            power: 20,
            raw_fields: vec![1, 0, 120, 0, 20, 0],
        }]);
        let spawn = |origin| Launch {
            owner: Owner::Player,
            rocket_id: 0,
            origin,
            direction: [1.0, 0.0],
        };
        session
            .projectiles
            .spawn(spawn([0.9, 0.5]), &session.rockets)
            .unwrap();
        session
            .projectiles
            .spawn(spawn([0.8, 0.5]), &session.rockets)
            .unwrap();
        let events = session.step(SessionInput::default());
        assert_eq!(events.projectile_impacts.len(), 1);
        assert_eq!(events.projectile_impacts[0].projectile_id, 0);
        assert_eq!(
            events.projectile_impacts[0].target,
            ImpactTarget::Actor { source_index: 7 }
        );
        assert_eq!(session.actors.actors[0].health, 0);
        assert_eq!(session.projectiles.active().len(), 1);
        assert_eq!(session.projectiles.active()[0].id, 1);
        assert!((session.projectiles.active()[0].position[0] - (0.8 + 10.0 / 60.0)).abs() < 1e-5);
    }
    #[test]
    fn enemy_instant_shot_selects_nearest_actor_wall_and_player() {
        use crate::world::Wall;
        let mut session = mission_session("");
        let def = ActorDefinition {
            model_name: "a.car".into(),
            wall_radius: 32,
            actor_radius: 32,
            speed: 0,
            rotation_speed: 0,
            health: 100,
            kick: 0,
            rocket: 0,
            separation_limit: 0,
            extra_fields: vec![],
            raw_line: String::new(),
        };
        session.actor_definitions.entries.push(def);
        for (source_index, x) in [(7, 4.5), (8, 2.5)] {
            session.actors.actors.push(Actor {
                source_index,
                definition_index: 0,
                placement: Monster {
                    position: [(x * 256.0) as u16, 128],
                    id: 101,
                    angle: 0,
                    flags: 2,
                },
                position: [x, 0.5],
                angle_radians: 0.0,
                health: 100,
            });
        }
        let mut passive = session.actor_definitions.entries[0].clone();
        passive.rocket = -1;
        session.actor_definitions.entries.push(passive);
        session.actors.actors[1].definition_index = 1;
        session.ai = EnemyAi::new(&session.actors, Difficulty::Normal);
        session.rockets = RocketDefinitions(vec![RocketDefinition {
            model_name: String::new(),
            power: 20,
            raw_fields: vec![],
        }]);
        let mut actual_tick = session.clone();
        let tick = actual_tick.step(SessionInput::default());
        assert!(tick.enemy_hits.iter().any(|event| event.source_index == 7
            && matches!(
                event.hit,
                Some(EnemyShotHit {
                    target: EnemyShotTarget::Actor { source_index: 8 },
                    ..
                })
            )));
        assert_eq!(actual_tick.actors.actors[1].health, 80);
        assert_eq!(actual_tick.game.inventory.health, 100);
        let attack = EnemyAttack {
            source_index: 7,
            damage: 20,
            kind: EnemyAttackKind::Ranged {
                rocket_id: 11,
                instant: true,
                origin: [4.5, 0.5],
                target: [0.5, 0.5],
            },
        };
        let mut events = SessionEvents::default();
        let mut attempts = Vec::new();
        session.resolve_enemy_instant(
            attack,
            11,
            [4.5, 0.5],
            [0.5, 0.5],
            &mut events,
            &mut attempts,
        );
        assert_eq!(
            events.enemy_hits[0].hit.unwrap().target,
            EnemyShotTarget::Actor { source_index: 8 }
        );
        assert_eq!(session.actors.actors[0].health, 100);
        assert_eq!(session.actors.actors[1].health, 80);
        assert_eq!(session.game.inventory.health, 100);
        session.actors.actors[1].health = 0;
        session.world.walls.push(Wall {
            a: [2.0, 0.0],
            b: [2.0, 1.0],
            texture: 1,
            blocks_movement: true,
            cell: None,
        });
        let mut events = SessionEvents::default();
        session.resolve_enemy_instant(
            attack,
            11,
            [4.5, 0.5],
            [0.5, 0.5],
            &mut events,
            &mut attempts,
        );
        assert_eq!(
            events.enemy_hits[0].hit.unwrap().target,
            EnemyShotTarget::Wall { wall_index: 0 }
        );
        assert_eq!(session.game.inventory.health, 100);
        session.shot_blocking[1] = false;
        let mut events = SessionEvents::default();
        session.resolve_enemy_instant(
            attack,
            11,
            [4.5, 0.5],
            [0.5, 0.5],
            &mut events,
            &mut attempts,
        );
        assert_eq!(
            events.enemy_hits[0].hit.unwrap().target,
            EnemyShotTarget::Player
        );
        assert_eq!(session.game.inventory.health, 80);
        let mut events = SessionEvents::default();
        session.resolve_enemy_instant(
            attack,
            11,
            [4.5, 0.5],
            [4.5, 5.0],
            &mut events,
            &mut attempts,
        );
        assert_eq!(events.enemy_hits[0].hit, None);
        assert_eq!(session.game.inventory.health, 80);
    }
    #[test]
    fn enemy_instant_object_hit_stages_contact_and_destroy_atomically() {
        use crate::objects::{ObjectDefinition, ObjectInstance, ObjectKind};
        use crate::process_runtime::{ProcessCapabilities, ProcessObject};
        let text = "#proc1\n#action\nchange 3 0 165\n#end\n#proc2\nEndDelay 1\n#end\n#links\nshoot 2 0 1\ndestroy 2 0 2\n#end\n";
        let script = Script::parse(text);
        let mut session = mission_session(text);
        let def = |name: &str, health| ObjectDefinition {
            collision_radius: 32,
            shadow: 0,
            bmp_object: 0,
            bmp_z: 0,
            action_code: 0,
            blow_up: 0,
            break_limit: health,
            ambient_sound: 0,
            break_sound: 0,
            model_name: name.into(),
            animation_name: None,
        };
        let defs = ObjectDefinitions {
            items: vec![],
            level_objects: vec![
                def("barrel.3o", 10),
                def("switch.3o", 0),
                def("dummy.3o", 0),
            ],
        };
        let object = |x: usize, definition_index| ObjectInstance {
            kind: ObjectKind::LevelObject,
            definition_index,
            grid_cell: [x, 0],
            position: [x as f32 + 0.5, 0.5],
            angle_radians: 0.0,
            flags: 0,
            difficulty_flags: 2,
            extra: 0,
            size: 0,
        };
        let instances = [object(2, 0), object(3, 1)];
        let mut cells = [0; 4096];
        cells[128] = 163;
        cells[192] = 164;
        let poses: Vec<_> = instances
            .iter()
            .map(|o| ProcessObject {
                cell: [o.grid_cell[0] as u8, 0],
                position: o.position,
                angle_radians: 0.0,
                elevation_raw: 0.0,
                blocking_radius: 0.125,
            })
            .collect();
        session.processes = ProcessRuntime::with_staged_actions(
            &script,
            &session.world,
            ProcessCapabilities::default(),
            &cells,
            &poses,
        )
        .unwrap();
        session.set_objects(ObjectCombat::new(
            &instances,
            &defs,
            &script,
            Difficulty::Normal,
        ));
        session.set_scripted_objects(ScriptedObjects::new(
            ObjectStates::new(&instances, &defs).unwrap(),
            defs,
            PresentationManifest::default(),
            &script,
            false,
        ));
        let attack = EnemyAttack {
            source_index: 7,
            damage: 20,
            kind: EnemyAttackKind::Ranged {
                rocket_id: 11,
                instant: true,
                origin: [4.5, 0.5],
                target: [0.5, 0.5],
            },
        };
        let mut events = SessionEvents::default();
        let mut attempts = Vec::new();
        session.resolve_enemy_instant(
            attack,
            11,
            [4.5, 0.5],
            [0.5, 0.5],
            &mut events,
            &mut attempts,
        );
        assert!(events.enemy_hits.is_empty());
        assert!(attempts.is_empty());
        assert_eq!(session.game.inventory.health, 100);
        assert_eq!(
            session.objects.as_ref().unwrap().is_active([2, 0]),
            Some(true)
        );
        assert_eq!(session.processes.cell_type([3, 0]), Some(164));
        session
            .scripted_objects
            .as_mut()
            .unwrap()
            .manifest
            .allow([3, 0], 165);
        session.resolve_enemy_instant(
            attack,
            11,
            [4.5, 0.5],
            [0.5, 0.5],
            &mut events,
            &mut attempts,
        );
        assert_eq!(
            events.enemy_hits[0].hit.unwrap().target,
            EnemyShotTarget::Object { cell: [2, 0] }
        );
        assert_eq!(events.object_hits, vec![([2, 0], true)]);
        assert_eq!(
            attempts.iter().map(|a| a.process_id).collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert!(attempts
            .iter()
            .all(|a| a.outcome == ProcessOutcome::Accepted));
        assert_eq!(session.processes.cell_type([3, 0]), Some(165));
        assert_eq!(session.game.inventory.health, 100);
    }
    #[test]
    fn chase_obeys_represented_object_pose_and_elevation() {
        use crate::process_runtime::{ProcessCapabilities, ProcessObject};
        let mut session = mission_session("");
        session.world.player.position = [3.0, 0.5];
        session.actor_definitions.entries.push(ActorDefinition {
            model_name: "a.car".into(),
            wall_radius: 52,
            actor_radius: 52,
            speed: 10,
            rotation_speed: 5,
            health: 100,
            kick: 0,
            rocket: -1,
            separation_limit: 0,
            extra_fields: vec![],
            raw_line: String::new(),
        });
        let placement = Monster {
            position: [128, 128],
            id: 101,
            angle: 0,
            flags: 2,
        };
        session.actors.actors.push(Actor {
            source_index: 7,
            definition_index: 0,
            placement,
            position: [0.5, 0.5],
            angle_radians: 0.0,
            health: 100,
        });
        session.ai = EnemyAi::new(&session.actors, Difficulty::Normal);
        let mut cells = [0; 4096];
        cells[64] = 163;
        let pose = ProcessObject {
            cell: [1, 0],
            position: [1.5, 0.5],
            angle_radians: 0.0,
            elevation_raw: 0.0,
            blocking_radius: 0.2,
        };
        let script = Script::default();
        session.processes = ProcessRuntime::with_objects_and_hazards(
            &script,
            &session.world,
            ProcessCapabilities::default(),
            &cells,
            &[pose],
        )
        .unwrap();
        let mut overhead = session.clone();
        overhead.processes = ProcessRuntime::with_objects_and_hazards(
            &script,
            &overhead.world,
            ProcessCapabilities::default(),
            &cells,
            &[ProcessObject {
                elevation_raw: 64.0,
                ..pose
            }],
        )
        .unwrap();
        for _ in 0..90 {
            session.step(SessionInput::default());
            overhead.step(SessionInput::default());
        }
        assert!(session.actors.actors[0].position[0] <= 1.5 - 52.0 / 256.0 - 0.2 + 1e-4);
        assert!(overhead.actors.actors[0].position[0] > 1.6);
    }
    #[test]
    fn enemy_sight_uses_raised_wall_height_without_changing_movement_collision() {
        use crate::world::Wall;
        let mut session = mission_session("");
        session.world.player.position = [3.0, 0.5];
        session.actor_definitions.entries.push(ActorDefinition {
            model_name: "a.car".into(),
            wall_radius: 52,
            actor_radius: 52,
            speed: 10,
            rotation_speed: 5,
            health: 100,
            kick: 0,
            rocket: -1,
            separation_limit: 0,
            extra_fields: vec![],
            raw_line: String::new(),
        });
        let placement = Monster {
            position: [128, 128],
            id: 101,
            angle: 0,
            flags: 2,
        };
        session.actors.actors.push(Actor {
            source_index: 7,
            definition_index: 0,
            placement,
            position: [0.5, 0.5],
            angle_radians: 0.0,
            health: 100,
        });
        session.ai = EnemyAi::new(&session.actors, Difficulty::Normal);
        session.world.walls.push(Wall {
            a: [1.0, 0.0],
            b: [1.0, 1.0],
            texture: 87,
            blocks_movement: true,
            cell: Some([1, 0]),
        });
        session.base_wall_blocking.push(true);
        session.wall_elevations.push(0.0);
        let mut raised = session.clone();
        raised.wall_elevations[0] = 64.0;
        raised.elevation_by_cell[64] = 64.0;
        session.step(SessionInput::default());
        raised.step(SessionInput::default());
        assert_eq!(session.ai.mode(7), Some(Mode::Idle));
        assert_eq!(raised.ai.mode(7), Some(Mode::Chasing));
        assert!(raised.world.walls[0].blocks_movement);
    }
}
