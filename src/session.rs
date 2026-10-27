//! Fixed-tick game session: simulation state independent of presentation and transport.
use crate::actors::{ActorDefinitions, ActorMapping, ActorState};
use crate::ai::{ActorCollisionSides, EnemyAi, EnemyAttack, EnemyAttackKind, Mode, SightCylinder};
use crate::ambient::AmbientGrid;
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
use crate::light_sources::{LightSourceError, LightSources};
use crate::mission::{MissionEvent, MissionPlan, MissionState};
use crate::motion_push::{self, EntityId, MotionProposal, MoverId, PushEntity, Translation};
use crate::object_combat::{ObjectCombat, ObjectDestructionInfo};
use crate::object_heights::ObjectHeightCatalog;
use crate::object_states::{ObjectAction, ObjectStates, ObjectTransition, TransitionKind};
use crate::objects::ObjectDefinitions;
use crate::pickups::{Difficulty, PickupEvent};
use crate::process::Script;
use crate::process_runtime::{
    ImmediateAction, ProcessEffect, ProcessEvent, ProcessObject, ProcessOutcome, ProcessRuntime,
};
use crate::process_transaction::{PresentationManifest, ProcessTransaction};
use crate::projectiles::{Impact, ImpactTarget, Launch, Owner, ProjectileSystem};
use crate::quake::{QuakeField, QuakeZone};
use crate::teleport::{RearmPolicy, Teleport, TeleportEvent, TeleportFailure, TeleportState};
use crate::vertical::{VerticalBounds, VerticalEvents, VerticalState};
use crate::wall_transitions::{prepare_wall_transitions, WallMaterialCatalog};
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

/// Matches the reconstruction's old-style camera pitch limit.
pub const MAX_PLAYER_PITCH: f32 = std::f32::consts::FRAC_PI_2 * 0.65;

#[derive(Default)]
pub struct SessionInput {
    pub movement: Input,
    /// Horizontal look displacement in radians for this tick, independent of frame rate.
    pub yaw_delta: f32,
    /// Positive radians look upward; applied once per fixed tick.
    pub pitch_delta: f32,
    pub interact: bool,
    /// One-shot jump press edge; a held button is not automatically repeated.
    pub jump: bool,
    pub fire: bool,
    pub select_weapon: Option<usize>,
}
/// A physical projectile redirected by the active player reflector.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ProjectileReflection {
    pub projectile_id: u64,
    pub rocket_id: usize,
    pub previous_owner: Owner,
    pub position: [f32; 2],
    pub time: f32,
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

/// The authoritative cause of a committed object destruction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectDestructionCause {
    PlayerHitscan,
    EnemyHitscan {
        source_index: usize,
    },
    Projectile {
        projectile_id: u64,
        owner: Owner,
    },
    Explosion {
        origin: ExplosionOrigin,
        owner: Owner,
    },
    Script {
        process_id: u16,
    },
}

/// Identity, sound and position before the destruction or replacement.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ObjectDestructionEvent {
    pub cell: [usize; 2],
    pub raw_type: u8,
    pub position: [f32; 2],
    pub sound_id: i32,
    pub cause: ObjectDestructionCause,
}
impl ObjectDestructionEvent {
    fn new(info: ObjectDestructionInfo, cause: ObjectDestructionCause) -> Self {
        Self {
            cell: info.cell,
            raw_type: info.raw_type,
            position: info.position,
            sound_id: info.sound_id,
            cause,
        }
    }
}

// An explode transition for the just-destroyed model represents the same
// destruction; other transitions, including later replacements, remain distinct.
fn deduplicate_script_destruction(events: &mut Vec<ObjectDestructionEvent>, direct: usize) {
    let first = events[direct];
    if let Some(offset) = events[direct + 1..].iter().position(|event| {
        event.cell == first.cell
            && event.raw_type == first.raw_type
            && matches!(event.cause, ObjectDestructionCause::Script { .. })
    }) {
        events.remove(direct + 1 + offset);
    }
}

#[derive(Default)]
pub struct SessionEvents {
    pub pickups: Vec<PickupEvent>,
    pub shot: Option<FireResult>,
    pub damage: u16,
    pub enemy_attacks: Vec<EnemyAttack>,
    pub enemy_hits: Vec<EnemyHitscanEvent>,
    pub projectile_impacts: Vec<Impact>,
    /// World-space impact heights aligned with `projectile_impacts`.
    pub projectile_impact_heights: Vec<f32>,
    pub projectile_reflections: Vec<ProjectileReflection>,
    /// Impact explosions and raw pre-armor damage, in deterministic order.
    pub explosions: Vec<Explosion>,
    /// One source per explosion, in the same order as `explosions`.
    pub explosion_origins: Vec<ExplosionOrigin>,
    pub explosion_damage: Vec<DamageEvent>,
    /// Stable mine lifecycle events for presentation and networking.
    pub mine_events: Vec<MineEvent>,
    /// Object cell and whether it was destroyed by this hit.
    pub object_hits: Vec<([usize; 2], bool)>,
    /// Committed destructions in simulation order, including scripted explosions.
    pub object_destructions: Vec<ObjectDestructionEvent>,
    /// Ordered committed model replacements; the host must apply before next tick.
    pub object_transitions: Vec<ObjectTransition>,
    /// Ordered committed wall material replacements.
    pub wall_transitions: Vec<crate::process_runtime::RuntimeWallTransition>,
    /// Authored quake commands for an explicitly opted-in consumer.
    pub quake_changes: Vec<QuakeZone>,
    /// Mission presentation/completion events produced during this step.
    pub mission: Vec<MissionEvent>,
    pub process_effects: Vec<ProcessEffect>,
    /// Due delayed processes that could not commit this tick and will retry.
    pub delayed_process_blocked: Vec<ProcessEvent>,
    pub hazard_damage: Vec<HazardDamage>,
    pub vertical: VerticalEvents,
    /// A committed player teleport or an explicitly rejected attempt.
    pub teleport: Option<TeleportEvent>,
    pub teleport_failure: Option<TeleportFailure>,
    /// Invalid or duplicate wind activation, if emitted by a process.
    pub wind_errors: Vec<WindError>,
    pub light_errors: Vec<LightSourceError>,
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
struct SessionObjectHeights {
    catalog: ObjectHeightCatalog,
    initial_raw_cells: Box<[u8; 4096]>,
}

/// Absolute model bounds for a combat target; missing/invalid pose or metadata
/// is unknown and must not be treated as empty space.
fn object_combat_bounds(
    heights: Option<&SessionObjectHeights>,
    processes: &ProcessRuntime,
    cell: [usize; 2],
    combat_raw: u8,
) -> Option<(f32, f32)> {
    let heights = heights?;
    let cell = [u8::try_from(cell[0]).ok()?, u8::try_from(cell[1]).ok()?];
    if cell[0] >= 64 || cell[1] >= 64 {
        return None;
    }
    let object = processes.object_pose(cell)?;
    let raw = processes.cell_type(cell).unwrap_or(combat_raw);
    let bounds = heights.catalog.get(raw)?;
    let elevation = object.elevation_raw / 64.0;
    let bottom = bounds.bottom + elevation;
    let top = bounds.top + elevation;
    (bottom.is_finite()
        && top.is_finite()
        && bottom <= top
        && bottom.abs() <= 4096.0
        && top.abs() <= 4096.0)
        .then_some((bottom, top))
}

fn object_combat_overlaps(
    heights: Option<&SessionObjectHeights>,
    processes: &ProcessRuntime,
    cell: [usize; 2],
    raw: u8,
    bottom: f32,
    top: f32,
) -> bool {
    if !bottom.is_finite() || !top.is_finite() || bottom > top {
        return true;
    }
    object_combat_bounds(heights, processes, cell, raw)
        .is_none_or(|(object_bottom, object_top)| top >= object_bottom && bottom <= object_top)
}

fn object_blocks_height(
    heights: Option<&SessionObjectHeights>,
    processes: &ProcessRuntime,
    object: &ProcessObject,
    bottom: f32,
    top: f32,
    legacy_top: f32,
) -> bool {
    if object.blocking_radius <= 0.0 {
        return false;
    }
    let Some(heights) = heights else {
        return object.elevation_raw / 64.0 < legacy_top;
    };
    let cell = object.cell;
    if cell[0] >= 64 || cell[1] >= 64 || !object.elevation_raw.is_finite() {
        return true;
    }
    let initial = heights.initial_raw_cells[usize::from(cell[0]) * 64 + usize::from(cell[1])];
    let raw = processes.cell_type(cell).unwrap_or(initial);
    if !(131..=226).contains(&raw) {
        return true;
    }
    heights
        .catalog
        .overlaps(raw, object.elevation_raw / 64.0, bottom, top)
}

/// Bounded committed process effects used to validate history-dependent grids.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub enum GridJournalEntry {
    Immediate { process_id: u16 },
    WindActivate { process_id: u16 },
    WindDeactivate { process_id: u16 },
    SourceActivate { process_id: u16, tick: u64 },
    SourceDeactivate { process_id: u16, tick: u64 },
}
const MAX_GRID_JOURNAL: usize = 4096;
// Each rectangle is conservatively charged one full map regardless of area.
const MAX_GRID_REPLAY_WORK: usize = 16 * 1024 * 1024;

#[derive(Clone)]
pub struct Session {
    pub world: World,
    wall_collision_mode: WallCollisionMode,
    prepared_motion: bool,
    object_sight: bool,
    difficulty: Difficulty,
    teleports: Option<(Vec<Teleport>, TeleportState)>,
    object_heights: Option<SessionObjectHeights>,
    pub vertical: VerticalState,
    pitch: f32,
    pub processes: ProcessRuntime,
    pub actors: ActorState,
    pub actor_definitions: ActorDefinitions,
    pub game: GameInventory,
    pub ai: EnemyAi,
    pub selected_weapon: usize,
    pub projectiles: ProjectileSystem,
    explosions_enabled: bool,
    addon_boss_rule: bool,
    projectile_heights: BTreeMap<u64, f32>,
    projectile_vertical_speeds: BTreeMap<u64, f32>,
    mines_enabled: bool,
    mines: MineSystem,
    pub objects: Option<ObjectCombat>,
    pub scripted_objects: Option<ScriptedObjects>,
    pub mission: Option<MissionState>,
    pub hazards: Hazards,
    pub wind: WindField,
    pub light_sources: Option<LightSources>,
    light_tick: u64,
    pending_light_errors: Vec<LightSourceError>,
    ambient: Option<AmbientGrid>,
    pending_wind_errors: Vec<WindError>,
    grid_journal: Vec<GridJournalEntry>,
    grid_journal_overflow: bool,
    /// Absolute offsets in original script units, indexed by wall index.
    pub wall_elevations: Vec<f32>,
    base_wall_blocking: Vec<bool>,
    wall_materials: Option<WallMaterialCatalog>,
    elevation_by_cell: Vec<f32>,
    combat: Combat,
    rockets: RocketDefinitions,
    shot_blocking: [bool; 128],
}

/// Explicit mutable-state aggregate. The content digest is supplied and
/// verified by the host; the DTO contains no resource bytes or paths.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionSnapshot {
    pub content: crate::save_format::ContentDigest,
    pub capabilities: SessionSnapshotCapabilities,
    pub world: crate::world::WorldSnapshot,
    pub vertical: crate::vertical::VerticalSnapshot,
    #[serde(default)]
    pub pitch: f32,
    pub processes: crate::process_runtime::ProcessRuntimeSnapshot,
    pub actors: crate::actors::ActorStateSnapshot,
    pub ai: crate::ai::EnemyAiSnapshot,
    pub inventory: crate::inventory::InventorySnapshot,
    pub powerups: crate::inventory::ProtectivePowerupsSnapshot,
    pub pickups: crate::pickups::PickupsSnapshot,
    pub selected_weapon: u8,
    pub combat: crate::combat::CombatSnapshot,
    pub projectiles: crate::projectiles::ProjectileSystemSnapshot,
    pub projectile_heights: Vec<(u64, f32)>,
    #[serde(default)]
    pub projectile_vertical_speeds: Vec<(u64, f32)>,
    pub mines: crate::explosives::MineSystemSnapshot,
    pub object_states: Option<crate::object_states::ObjectStatesSnapshot>,
    pub object_combat: Option<crate::object_combat::ObjectCombatSnapshot>,
    pub quake: Option<crate::quake::QuakeSnapshot>,
    pub mission: Option<crate::mission::MissionSnapshot>,
    pub hazards: crate::hazards::HazardsSnapshot,
    pub wind: crate::wind::WindSnapshot,
    pub grid_journal: Vec<GridJournalEntry>,
    #[serde(default)]
    pub light_sources: Option<crate::light_sources::LightSourcesSnapshot>,
    #[serde(default)]
    pub light_tick: u64,
    pub ambient: Option<crate::ambient::AmbientSnapshot>,
    pub teleport: Option<crate::teleport::TeleportSnapshot>,
    pub wall_elevations: Vec<f32>,
    pub elevation_by_cell: Vec<f32>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionSnapshotCapabilities {
    pub wall_collision_mode: WallCollisionMode,
    pub wall_materials: bool,
    pub teleports: bool,
    pub object_heights: bool,
    pub explosions: bool,
    pub addon_boss: bool,
    pub mines: bool,
    pub objects: bool,
    pub scripted_objects: bool,
    pub mission: bool,
    pub ambient: bool,
    pub powerups: bool,
    pub quake_presentation: bool,
    pub source_lights: bool,
    #[serde(default)]
    pub prepared_motion: bool,
    #[serde(default)]
    pub object_sight: bool,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionSnapshotError {
    Content,
    Capabilities,
    PendingEffects,
    Component(&'static str),
    Inconsistent(&'static str),
    Unverifiable(&'static str),
}
impl Session {
    fn snapshot_capabilities(&self) -> SessionSnapshotCapabilities {
        SessionSnapshotCapabilities {
            wall_collision_mode: self.wall_collision_mode,
            prepared_motion: self.prepared_motion,
            object_sight: self.object_sight,
            wall_materials: self.wall_materials.is_some(),
            teleports: self.teleports.is_some(),
            object_heights: self.object_heights.is_some(),
            explosions: self.explosions_enabled,
            addon_boss: self.addon_boss_rule,
            mines: self.mines_enabled,
            objects: self.objects.is_some(),
            scripted_objects: self.scripted_objects.is_some(),
            mission: self.mission.is_some(),
            ambient: self.ambient.is_some(),
            powerups: self.game.protective_powerups.enabled,
            source_lights: self.light_sources.is_some(),
            quake_presentation: self
                .scripted_objects
                .as_ref()
                .is_some_and(|s| s.quake_enabled),
        }
    }

    /// Capture at a completed simulation tick after all effects are consumed.
    pub fn snapshot(
        &self,
        content: crate::save_format::ContentDigest,
    ) -> Result<SessionSnapshot, SessionSnapshotError> {
        if !self.pending_wind_errors.is_empty() || !self.pending_light_errors.is_empty() {
            return Err(SessionSnapshotError::PendingEffects);
        }
        if self.grid_journal_overflow || !self.grid_journal_within_work_limit() {
            return Err(SessionSnapshotError::Unverifiable("grid journal overflow"));
        }
        if self.light_sources.is_some() != self.processes.source_lights_enabled() {
            return Err(SessionSnapshotError::Inconsistent(
                "source light capability",
            ));
        }
        let processes = self
            .processes
            .snapshot(&self.world)
            .map_err(|_| SessionSnapshotError::PendingEffects)?;
        if self.objects.is_some() != self.scripted_objects.is_some() {
            return Err(SessionSnapshotError::Unverifiable("object definitions"));
        }
        Ok(SessionSnapshot {
            content,
            capabilities: self.snapshot_capabilities(),
            world: self.world.snapshot(),
            vertical: self.vertical.snapshot(),
            pitch: self.pitch,
            processes,
            actors: self
                .actors
                .snapshot()
                .map_err(|_| SessionSnapshotError::Component("actors"))?,
            ai: self
                .ai
                .snapshot()
                .map_err(|_| SessionSnapshotError::Component("ai"))?,
            inventory: self.game.inventory.snapshot(),
            powerups: self.game.protective_powerups.snapshot(),
            pickups: self
                .game
                .pickups
                .snapshot()
                .map_err(|_| SessionSnapshotError::Component("pickups"))?,
            selected_weapon: u8::try_from(self.selected_weapon)
                .map_err(|_| SessionSnapshotError::Component("weapon"))?,
            combat: self.combat.snapshot(),
            projectiles: self
                .projectiles
                .snapshot()
                .map_err(|_| SessionSnapshotError::Component("projectiles"))?,
            projectile_heights: self
                .projectile_heights
                .iter()
                .map(|(&id, &height)| (id, height))
                .collect(),
            projectile_vertical_speeds: self
                .projectile_vertical_speeds
                .iter()
                .map(|(&id, &speed)| (id, speed))
                .collect(),
            mines: self
                .mines
                .snapshot()
                .map_err(|_| SessionSnapshotError::Component("mines"))?,
            object_states: self.scripted_objects.as_ref().map(|s| s.states.snapshot()),
            object_combat: self.objects.as_ref().map(|o| o.snapshot()),
            quake: self.scripted_objects.as_ref().map(|s| s.quake.snapshot()),
            mission: self.mission.as_ref().map(|m| m.snapshot()),
            hazards: self.hazards.snapshot(),
            wind: self.wind.snapshot(),
            grid_journal: self.grid_journal.clone(),
            light_sources: self.light_sources.as_ref().map(LightSources::snapshot),
            light_tick: self.light_tick,
            ambient: self.ambient.as_ref().map(|a| a.snapshot()),
            teleport: self.teleports.as_ref().map(|(_, state)| state.snapshot()),
            wall_elevations: self.wall_elevations.clone(),
            elevation_by_cell: self.elevation_by_cell.clone(),
        })
    }

    /// Build a candidate from a caller-supplied fresh same-content baseline.
    /// The caller must verify the digest from actual resolved inputs before
    /// passing it, and publish the returned Session only after presentation is
    /// rebuilt. Ambiguous rotation histories are rejected.
    pub fn restore_from_baseline(
        &self,
        snapshot: &SessionSnapshot,
        verified_content: crate::save_format::ContentDigest,
    ) -> Result<Self, SessionSnapshotError> {
        use crate::process_runtime::ProcessPhaseSnapshot;
        if snapshot.content != verified_content {
            return Err(SessionSnapshotError::Content);
        }
        if snapshot.capabilities != self.snapshot_capabilities() {
            return Err(SessionSnapshotError::Capabilities);
        }
        if !self.pending_wind_errors.is_empty() || !self.pending_light_errors.is_empty() {
            return Err(SessionSnapshotError::PendingEffects);
        }
        if self.light_tick != 0
            || self.light_sources.is_some() != self.processes.source_lights_enabled()
            || self
                .light_sources
                .as_ref()
                .is_some_and(|lights| lights.active().next().is_some())
        {
            return Err(SessionSnapshotError::Unverifiable("source light baseline"));
        }
        let baseline = self
            .processes
            .snapshot(&self.world)
            .map_err(|_| SessionSnapshotError::PendingEffects)?;
        if baseline.processes.iter().any(|p| {
            p.phase != ProcessPhaseSnapshot::Closed
                || p.progress != 0.0
                || p.wall_progress
                    .iter()
                    .chain(&p.object_progress)
                    .any(|v| *v != 0.0)
        }) {
            return Err(SessionSnapshotError::Unverifiable("baseline is not fresh"));
        }
        if snapshot.wall_elevations != snapshot.processes.elevations
            || snapshot.wall_elevations.len() != self.world.walls.len()
            || snapshot.elevation_by_cell.len() != 4096
        {
            return Err(SessionSnapshotError::Inconsistent("wall elevation"));
        }
        let mut cell_elevations = BTreeMap::new();
        for (wall, &offset) in self.world.walls.iter().zip(&snapshot.wall_elevations) {
            if let Some(cell) = wall.cell.filter(|[x, y]| *x < 64 && *y < 64) {
                if cell_elevations
                    .insert(cell, offset)
                    .is_some_and(|previous| previous != offset)
                {
                    return Err(SessionSnapshotError::Unverifiable(
                        "shared wall cell elevation",
                    ));
                }
            }
        }
        for x in 0..64 {
            for y in 0..64 {
                let expected = cell_elevations
                    .get(&[x as u8, y as u8])
                    .copied()
                    .unwrap_or(0.0);
                if snapshot.elevation_by_cell[x * 64 + y] != expected {
                    return Err(SessionSnapshotError::Inconsistent("cell elevation"));
                }
            }
        }
        if snapshot.processes.keys != snapshot.inventory.keys {
            return Err(SessionSnapshotError::Inconsistent("keys"));
        }
        if snapshot.powerups.enabled != snapshot.capabilities.powerups
            || snapshot.ai.player_invisible != (snapshot.inventory.invisibility_ticks_remaining > 0)
        {
            return Err(SessionSnapshotError::Inconsistent("powerup state"));
        }
        let (replayed_world, replayed_blocking) = self.validate_grid_journal(snapshot)?;
        if snapshot.selected_weapon as usize >= snapshot.inventory.weapons.len()
            || !snapshot.inventory.weapons[snapshot.selected_weapon as usize]
        {
            return Err(SessionSnapshotError::Inconsistent("selected weapon"));
        }
        if !snapshot.pitch.is_finite() || snapshot.pitch.abs() > MAX_PLAYER_PITCH {
            return Err(SessionSnapshotError::Component("pitch"));
        }
        let mut candidate = self.clone();
        candidate.pitch = snapshot.pitch;
        candidate.world = replayed_world;
        candidate.base_wall_blocking = replayed_blocking;
        candidate
            .world
            .restore(&snapshot.world)
            .map_err(|_| SessionSnapshotError::Component("world"))?;
        candidate
            .processes
            .restore(&snapshot.processes, &candidate.world)
            .map_err(|_| SessionSnapshotError::Component("processes"))?;
        candidate
            .processes
            .validate_snapshot_geometry(&self.processes, &self.world, &candidate.world)
            .map_err(|error| match error {
                crate::process_runtime::ProcessGeometryError::AmbiguousRotation => {
                    SessionSnapshotError::Unverifiable("rotation history")
                }
                _ => SessionSnapshotError::Inconsistent("process geometry"),
            })?;
        if self.wall_materials.is_some() {
            if let Some(types) = &snapshot.processes.cell_types {
                for wall in &snapshot.world.walls {
                    if let Some([x, y]) = wall.cell {
                        if x >= 64
                            || y >= 64
                            || types.get(usize::from(x) * 64 + usize::from(y))
                                != Some(&wall.texture)
                        {
                            return Err(SessionSnapshotError::Inconsistent("wall cell type"));
                        }
                    }
                }
            } else {
                return Err(SessionSnapshotError::Unverifiable("wall cell types"));
            }
        }
        candidate
            .wall_elevations
            .clone_from(&snapshot.wall_elevations);
        candidate
            .elevation_by_cell
            .clone_from(&snapshot.elevation_by_cell);
        candidate
            .vertical
            .restore(snapshot.vertical, candidate.player_bounds())
            .map_err(|_| SessionSnapshotError::Component("vertical"))?;
        candidate
            .game
            .inventory
            .restore(&snapshot.inventory, &candidate.game.weapons)
            .map_err(|_| SessionSnapshotError::Component("inventory"))?;
        candidate
            .game
            .protective_powerups
            .restore(snapshot.powerups)
            .map_err(|_| SessionSnapshotError::Component("powerups"))?;
        candidate
            .game
            .pickups
            .restore(&snapshot.pickups)
            .map_err(|_| SessionSnapshotError::Component("pickups"))?;
        candidate
            .actors
            .restore(&snapshot.actors, &candidate.actor_definitions)
            .map_err(|_| SessionSnapshotError::Component("actors"))?;
        candidate
            .ai
            .restore(&snapshot.ai, &candidate.actors)
            .map_err(|_| SessionSnapshotError::Component("ai"))?;
        if candidate.actors.actors.iter().any(|actor| {
            candidate.ai.mode(actor.source_index) == Some(Mode::Inactive) && actor.health != 0
        }) {
            return Err(SessionSnapshotError::Inconsistent("inactive actor health"));
        }
        candidate
            .projectiles
            .restore_with_vertical(
                &snapshot.projectiles,
                &candidate.rockets,
                &candidate.actors,
                &snapshot.projectile_vertical_speeds,
            )
            .map_err(|_| SessionSnapshotError::Component("projectiles"))?;
        candidate
            .mines
            .restore(&snapshot.mines, &candidate.actors)
            .map_err(|_| SessionSnapshotError::Component("mines"))?;
        candidate
            .combat
            .restore(snapshot.combat, &candidate.game.weapons)
            .map_err(|_| SessionSnapshotError::Component("combat"))?;
        candidate.selected_weapon = snapshot.selected_weapon as usize;
        let ids: BTreeSet<_> = candidate
            .projectiles
            .active()
            .iter()
            .map(|p| p.id)
            .collect();
        let mut heights = BTreeMap::new();
        for &(id, height) in &snapshot.projectile_heights {
            if !ids.contains(&id)
                || !height.is_finite()
                || height.abs() > 4096.0
                || heights.insert(id, height).is_some()
            {
                return Err(SessionSnapshotError::Inconsistent("projectile heights"));
            }
        }
        candidate.projectile_heights = heights;
        let mut speeds = BTreeMap::new();
        for &(id, speed) in &snapshot.projectile_vertical_speeds {
            let Some(projectile) = candidate.projectiles.active().iter().find(|p| p.id == id)
            else {
                return Err(SessionSnapshotError::Inconsistent(
                    "projectile vertical speeds",
                ));
            };
            let horizontal = projectile.velocity[0].hypot(projectile.velocity[1]);
            let player_launch =
                projectile.owner == Owner::Player && projectile.reflected_from.is_none();
            if (player_launch && speed.abs() > horizontal * MAX_PLAYER_PITCH.tan() + 0.001)
                || !ids.contains(&id)
                || !speed.is_finite()
                || speed.abs() > 64.0
                || speeds.insert(id, speed).is_some()
                || !candidate.projectile_heights.contains_key(&id)
            {
                return Err(SessionSnapshotError::Inconsistent(
                    "projectile vertical speeds",
                ));
            }
        }
        candidate.projectile_vertical_speeds = speeds;
        match (
            &mut candidate.scripted_objects,
            &snapshot.object_states,
            &snapshot.quake,
        ) {
            (Some(scripted), Some(states), Some(quake)) => {
                scripted
                    .states
                    .restore(states)
                    .map_err(|_| SessionSnapshotError::Component("object states"))?;
                scripted
                    .quake
                    .restore(quake)
                    .map_err(|_| SessionSnapshotError::Component("quake"))?;
            }
            (None, None, None) => {}
            _ => return Err(SessionSnapshotError::Capabilities),
        }
        match (
            &mut candidate.objects,
            &snapshot.object_combat,
            &candidate.scripted_objects,
        ) {
            (Some(objects), Some(saved), Some(scripted)) => objects
                .restore(saved, &scripted.definitions)
                .map_err(|_| SessionSnapshotError::Component("object combat"))?,
            (None, None, _) => {}
            _ => return Err(SessionSnapshotError::Unverifiable("object definitions")),
        }
        match (&mut candidate.mission, &snapshot.mission) {
            (Some(target), Some(saved)) => target
                .restore(saved)
                .map_err(|_| SessionSnapshotError::Component("mission"))?,
            (None, None) => {}
            _ => return Err(SessionSnapshotError::Capabilities),
        }
        candidate
            .hazards
            .restore(&snapshot.hazards)
            .map_err(|_| SessionSnapshotError::Component("hazards"))?;
        match (&mut candidate.light_sources, &snapshot.light_sources) {
            (Some(target), Some(saved)) => {
                target
                    .restore(saved)
                    .map_err(|_| SessionSnapshotError::Component("source lights"))?;
                if !candidate.processes.snapshot_sources_match(saved) {
                    return Err(SessionSnapshotError::Inconsistent("source lights"));
                }
            }
            (None, None) if !candidate.processes.has_active_source_lights() => {}
            _ => return Err(SessionSnapshotError::Capabilities),
        }
        candidate.light_tick = snapshot.light_tick;
        candidate.grid_journal.clone_from(&snapshot.grid_journal);
        candidate.grid_journal_overflow = false;
        candidate
            .wind
            .restore(&snapshot.wind)
            .map_err(|_| SessionSnapshotError::Component("wind"))?;
        if !candidate
            .processes
            .snapshot_effects_match(&snapshot.hazards, &snapshot.wind)
        {
            return Err(SessionSnapshotError::Inconsistent("hazards or wind"));
        }
        match (&mut candidate.ambient, &snapshot.ambient) {
            (Some(target), Some(saved)) => target
                .restore(saved)
                .map_err(|_| SessionSnapshotError::Component("ambient"))?,
            (None, None) => {}
            _ => return Err(SessionSnapshotError::Capabilities),
        }
        match (&mut candidate.teleports, snapshot.teleport) {
            (Some((_, state)), Some(saved)) if state.snapshot().policy == saved.policy => state
                .restore(saved)
                .map_err(|_| SessionSnapshotError::Component("teleport"))?,
            (None, None) => {}
            _ => return Err(SessionSnapshotError::Capabilities),
        }
        if let (Some(states), Some(objects)) = (&snapshot.object_states, &snapshot.object_combat) {
            for object in &objects.entries {
                let state = states
                    .entries
                    .iter()
                    .find(|s| s.cell == object.cell)
                    .ok_or(SessionSnapshotError::Inconsistent("object identity"))?;
                if object.raw_type != state.raw_type
                    || object.position != state.position
                    || object.health.is_some_and(|health| health != state.health)
                {
                    return Err(SessionSnapshotError::Inconsistent("object state"));
                }
            }
        }
        for object in &snapshot.processes.objects {
            let cell = [usize::from(object.cell[0]), usize::from(object.cell[1])];
            if let Some(state) = snapshot
                .object_states
                .as_ref()
                .and_then(|states| states.entries.iter().find(|s| s.cell == cell))
            {
                if object.position != state.position
                    || object.angle_radians != state.angle_radians
                    || candidate
                        .processes
                        .cell_type(object.cell)
                        .is_some_and(|raw| raw != state.raw_type)
                {
                    return Err(SessionSnapshotError::Inconsistent("process object state"));
                }
                let scripted = candidate
                    .scripted_objects
                    .as_ref()
                    .ok_or(SessionSnapshotError::Capabilities)?;
                let def = scripted
                    .definitions
                    .level_objects
                    .get(usize::from(state.raw_type - 163))
                    .ok_or(SessionSnapshotError::Component("object definition"))?;
                let radius = if def.model_name.eq_ignore_ascii_case("dummy.3o")
                    || (141..=143).contains(&def.action_code)
                {
                    0.0
                } else {
                    (def.collision_radius.max(0) as f32 / 256.0).min(2.0)
                };
                if object.blocking_radius != radius {
                    return Err(SessionSnapshotError::Inconsistent("object collision"));
                }
            } else if baseline
                .objects
                .iter()
                .find(|o| o.cell == object.cell)
                .is_some_and(|base| base.blocking_radius != object.blocking_radius)
            {
                return Err(SessionSnapshotError::Inconsistent("pickup collision"));
            }
            if let Some(pickup) = snapshot
                .pickups
                .entries
                .iter()
                .find(|p| p.cell == object.cell)
            {
                if pickup.position != object.position || pickup.collected {
                    return Err(SessionSnapshotError::Inconsistent("process pickup"));
                }
            }
        }
        for original in &baseline.objects {
            if snapshot
                .processes
                .objects
                .iter()
                .any(|object| object.cell == original.cell)
            {
                continue;
            }
            let cell = [usize::from(original.cell[0]), usize::from(original.cell[1])];
            let collected = snapshot
                .pickups
                .entries
                .iter()
                .any(|p| p.cell == original.cell && p.collected);
            let removed = snapshot.object_combat.as_ref().is_some_and(|objects| {
                objects
                    .entries
                    .iter()
                    .any(|o| o.cell == cell && (o.destroyed || !o.enabled))
            });
            if !collected && !removed {
                return Err(SessionSnapshotError::Inconsistent("removed process object"));
            }
        }
        let needed = (candidate.vertical.head() * 64.0).max(80.0);
        for ((wall, original), elevation) in candidate
            .world
            .walls
            .iter()
            .zip(&candidate.base_wall_blocking)
            .zip(&candidate.wall_elevations)
        {
            if wall.blocks_movement != (*original && *elevation < needed) {
                return Err(SessionSnapshotError::Inconsistent("wall collision"));
            }
        }
        Ok(candidate)
    }
    pub fn new(mut setup: LevelSetup) -> Self {
        let ai = EnemyAi::new(&setup.actors, setup.difficulty);
        // Disabled difficulty placements must not be hittable by combat.
        for actor in &mut setup.actors.actors {
            if ai.mode(actor.source_index) == Some(Mode::Inactive) {
                actor.health = 0;
            }
        }
        let source_lights_enabled = setup.processes.source_lights_enabled();
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
            wall_materials: None,
            elevation_by_cell: vec![0.0; 64 * 64],
            world: setup.world,
            wall_collision_mode: WallCollisionMode::TwoSided,
            prepared_motion: false,
            object_sight: false,
            difficulty: setup.difficulty,
            teleports: None,
            object_heights: None,
            vertical: VerticalState::new(0.0).expect("default vertical state is valid"),
            pitch: 0.0,
            processes: setup.processes,
            actors: setup.actors,
            actor_definitions: setup.actor_definitions,
            game: setup.game,
            ai,
            selected_weapon: 0,
            projectiles: ProjectileSystem::new(),
            explosions_enabled: false,
            addon_boss_rule: false,
            projectile_heights: BTreeMap::new(),
            projectile_vertical_speeds: BTreeMap::new(),
            mines_enabled: false,
            mines: MineSystem::new(),
            objects: None,
            scripted_objects: None,
            mission: None,
            hazards: Hazards::default(),
            wind: WindField::new(),
            light_sources: source_lights_enabled.then(LightSources::default),
            light_tick: 0,
            pending_light_errors: Vec::new(),
            ambient: None,
            pending_wind_errors: Vec::new(),
            grid_journal: Vec::new(),
            grid_journal_overflow: false,
            combat: Combat::new(),
            rockets: setup.rockets,
            shot_blocking: setup.shot_blocking,
        }
    }
    /// Enable the provisional collision-checked process translation path.
    /// Configure before making a restart or durable snapshot.
    pub fn set_prepared_motion(&mut self, enabled: bool) {
        self.prepared_motion = enabled;
    }
    /// Opt into provisional reference-style model cylinders for enemy sight.
    /// Configure before taking restart or durable snapshots.
    pub fn set_object_sight(&mut self, enabled: bool) -> bool {
        if enabled && (self.scripted_objects.is_none() || self.objects.is_none()) {
            return false;
        }
        self.object_sight = enabled;
        true
    }
    /// Opt into model-height collision using original x-major map cells.
    /// Missing heights remain blocking. Call before the restart snapshot.
    /// Reject raw cells that disagree with represented runtime objects.
    pub fn set_object_heights(
        &mut self,
        catalog: ObjectHeightCatalog,
        raw_cells: [u8; 4096],
    ) -> bool {
        for object in self.processes.objects() {
            let [x, y] = object.cell;
            if x >= 64 || y >= 64 {
                return false;
            }
            let raw = raw_cells[usize::from(x) * 64 + usize::from(y)];
            if !(131..=226).contains(&raw)
                || self
                    .processes
                    .cell_type(object.cell)
                    .is_some_and(|current| current != raw)
            {
                return false;
            }
        }
        self.object_heights = Some(SessionObjectHeights {
            catalog,
            initial_raw_cells: Box::new(raw_cells),
        });
        true
    }

    /// Opt into player teleports; configure before taking a restart snapshot.
    /// The host must present teleport events and keep floor geometry in sync.
    pub fn set_teleports(&mut self, definitions: Vec<Teleport>, policy: RearmPolicy) {
        self.teleports = Some((definitions, TeleportState::new(policy)));
    }
    /// Install an authoritative ambient map before taking the restart snapshot.
    pub fn set_ambient_grid(&mut self, grid: AmbientGrid) {
        self.ambient = Some(grid);
    }
    /// The presentation layer can sample the current sound ID at a cell.
    pub fn ambient_grid(&self) -> Option<&AmbientGrid> {
        self.ambient.as_ref()
    }
    /// Enable the documented addon boss rule only for an explicitly selected
    /// addon-to-remaster actor mapping. Configure before the restart snapshot.
    pub fn set_addon_boss_rule(&mut self, mapping: ActorMapping) {
        self.addon_boss_rule = mapping == ActorMapping::AddonToRemaster;
    }

    /// Enable staged wall replacements using the host's complete GFX flags and
    /// presentation availability. Existing shot flags must match the catalog.
    pub fn set_wall_material_catalog(&mut self, catalog: WallMaterialCatalog) -> bool {
        if (1..=127u8).any(|raw| {
            catalog
                .get(raw)
                .is_some_and(|m| m.blocks_shots != self.shot_blocking[usize::from(raw)])
        }) {
            return false;
        }
        self.wall_materials = Some(catalog);
        true
    }
    /// Authoritative view pitch in radians, positive upward.
    pub fn pitch(&self) -> f32 {
        self.pitch
    }
    /// Current projectile height in map tiles, including pitched flight.
    pub fn projectile_height(&self, id: u64) -> Option<f32> {
        self.projectile_heights.get(&id).copied()
    }
    /// Current flight pitch in radians, positive upward.
    pub fn projectile_pitch(&self, id: u64) -> Option<f32> {
        let projectile = self
            .projectiles
            .active()
            .iter()
            .find(|projectile| projectile.id == id)?;
        let vertical = self
            .projectile_vertical_speeds
            .get(&id)
            .copied()
            .unwrap_or(0.0);
        Some(vertical.atan2(projectile.velocity[0].hypot(projectile.velocity[1])))
    }
    /// Choose the wall policy for player and wind-driven movement. Defaults to
    /// two-sided. Authored sides apply only to walls the process runtime proves
    /// static; moving-wall obstruction remains conservative. Snapshot clones
    /// retain the selected policy. Replacing or reordering walls externally
    /// invalidates the runtime's mask and must not be done while opted in.
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

    /// Enable provisional reflector/chojin pickups and effects. Disabling
    /// clears both timers so an unavailable presentation cannot hide them.
    pub fn set_protective_powerups_enabled(&mut self, enabled: bool) {
        self.game.protective_powerups.enabled = enabled;
        if !enabled {
            self.game.protective_powerups.reflector_ticks_remaining = 0;
            self.game.protective_powerups.chojin_ticks_remaining = 0;
        }
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
    fn grid_journal_within_work_limit(&self) -> bool {
        let mut rectangles = 0usize;
        for entry in &self.grid_journal {
            let count = match *entry {
                GridJournalEntry::Immediate { process_id } => {
                    match self.processes.journal_actions(process_id) {
                        Some(actions) => actions
                            .iter()
                            .filter(|a| {
                                matches!(
                                    a,
                                    ImmediateAction::Ambient(_)
                                        | ImmediateAction::Quake(_)
                                        | ImmediateAction::WallChange { .. }
                                )
                            })
                            .count(),
                        None => return false,
                    }
                }
                GridJournalEntry::SourceActivate { process_id, .. }
                | GridJournalEntry::SourceDeactivate { process_id, .. } => {
                    match self.processes.journal_sources(process_id) {
                        Some(sources) => sources.len(),
                        None => return false,
                    }
                }
                GridJournalEntry::WindActivate { process_id }
                | GridJournalEntry::WindDeactivate { process_id } => {
                    match self.processes.journal_wind(process_id) {
                        Some(zones) => zones.len(),
                        None => return false,
                    }
                }
            };
            let Some(next) = rectangles.checked_add(count) else {
                return false;
            };
            rectangles = next;
            if rectangles > MAX_GRID_REPLAY_WORK / 4096 {
                return false;
            }
        }
        true
    }
    fn journal_checkpoint(&self) -> (usize, bool) {
        (self.grid_journal.len(), self.grid_journal_overflow)
    }
    fn journal_rollback(&mut self, checkpoint: (usize, bool)) {
        self.grid_journal.truncate(checkpoint.0);
        self.grid_journal_overflow = checkpoint.1;
    }
    fn journal_push(&mut self, entry: GridJournalEntry) {
        if self.grid_journal.len() < MAX_GRID_JOURNAL {
            self.grid_journal.push(entry);
        } else {
            self.grid_journal_overflow = true;
        }
    }
    /// Verify grid values and wind membership from authored operations, not
    /// untrusted snapshot rectangles. This does not establish gameplay reachability.
    fn validate_grid_journal(
        &self,
        snapshot: &SessionSnapshot,
    ) -> Result<(World, Vec<bool>), SessionSnapshotError> {
        let invalid = || SessionSnapshotError::Unverifiable("grid journal");
        if !self.grid_journal.is_empty()
            || self.grid_journal_overflow
            || snapshot.grid_journal.len() > MAX_GRID_JOURNAL
        {
            return Err(invalid());
        }
        let mut wind = self.wind.clone();
        let mut ambient = self.ambient.clone();
        let mut quake = self.scripted_objects.as_ref().map(|s| s.quake.clone());
        let mut walls = self.world.clone();
        let mut base_blocking = self.base_wall_blocking.clone();
        if base_blocking.len() != walls.walls.len() {
            return Err(invalid());
        }
        let mut work = 0usize;
        let mut source_active =
            BTreeMap::<u16, (u64, Vec<crate::light_sources::SourceLight>)>::new();
        let mut previous_source_tick = 0;
        for entry in &snapshot.grid_journal {
            match *entry {
                GridJournalEntry::Immediate { process_id } => {
                    let actions = self
                        .processes
                        .journal_actions(process_id)
                        .ok_or_else(invalid)?;
                    let mut found = false;
                    for action in actions {
                        match action {
                            ImmediateAction::Quake(zone) => {
                                work = work.checked_add(4096).ok_or_else(invalid)?;
                                if work > MAX_GRID_REPLAY_WORK {
                                    return Err(invalid());
                                }
                                quake
                                    .as_mut()
                                    .ok_or_else(invalid)?
                                    .apply_batch(&[*zone])
                                    .map_err(|_| invalid())?;
                                found = true;
                            }
                            ImmediateAction::Ambient(region) => {
                                work = work.checked_add(4096).ok_or_else(invalid)?;
                                if work > MAX_GRID_REPLAY_WORK {
                                    return Err(invalid());
                                }
                                ambient
                                    .as_mut()
                                    .ok_or_else(invalid)?
                                    .apply(&[*region])
                                    .map_err(|_| invalid())?;
                                found = true;
                            }
                            ImmediateAction::WallChange { cell, raw_type } => {
                                work = work.checked_add(4096).ok_or_else(invalid)?;
                                if work > MAX_GRID_REPLAY_WORK {
                                    return Err(invalid());
                                }
                                if walls.walls.len() > 4096 {
                                    return Err(invalid());
                                }
                                let catalog = self.wall_materials.as_ref().ok_or_else(invalid)?;
                                let mut matching = walls
                                    .walls
                                    .iter()
                                    .enumerate()
                                    .filter(|(_, wall)| wall.cell == Some(*cell));
                                let (index, wall) = matching.next().ok_or_else(invalid)?;
                                if matching.next().is_some()
                                    || self.processes.cell_type(*cell)
                                        != Some(self.world.walls[index].texture)
                                {
                                    return Err(invalid());
                                }
                                let old = catalog.get(wall.texture).ok_or_else(invalid)?;
                                let new = catalog.get(*raw_type).ok_or_else(invalid)?;
                                if !(1..=127).contains(raw_type)
                                    || !(1..=127).contains(&wall.texture)
                                    || !new.presentation_available
                                    || old.blocks_movement != base_blocking[index]
                                    || old.blocks_shots
                                        != self.shot_blocking[usize::from(wall.texture)]
                                    || new.blocks_shots
                                        != self.shot_blocking[usize::from(*raw_type)]
                                    || wall.a == wall.b
                                    || wall
                                        .a
                                        .iter()
                                        .chain(&wall.b)
                                        .any(|v| !v.is_finite() || v.abs() > 4096.0)
                                {
                                    return Err(invalid());
                                }
                                walls.walls[index].texture = *raw_type;
                                base_blocking[index] = new.blocks_movement;
                                found = true;
                            }
                            ImmediateAction::Object(_) => {}
                        }
                    }
                    if !found {
                        return Err(invalid());
                    }
                }
                GridJournalEntry::SourceActivate { process_id, tick } => {
                    if self.light_sources.is_none()
                        || tick < previous_source_tick
                        || tick > snapshot.light_tick
                    {
                        return Err(invalid());
                    }
                    previous_source_tick = tick;
                    let sources = self
                        .processes
                        .journal_sources(process_id)
                        .ok_or_else(invalid)?;
                    work = work
                        .checked_add(sources.len().checked_mul(4096).ok_or_else(invalid)?)
                        .ok_or_else(invalid)?;
                    if work > MAX_GRID_REPLAY_WORK
                        || source_active
                            .insert(process_id, (tick, sources.to_vec()))
                            .is_some()
                    {
                        return Err(invalid());
                    }
                }
                GridJournalEntry::SourceDeactivate { process_id, tick } => {
                    if self.light_sources.is_none()
                        || tick < previous_source_tick
                        || tick > snapshot.light_tick
                    {
                        return Err(invalid());
                    }
                    previous_source_tick = tick;
                    let sources = self
                        .processes
                        .journal_sources(process_id)
                        .ok_or_else(invalid)?;
                    work = work
                        .checked_add(sources.len().checked_mul(4096).ok_or_else(invalid)?)
                        .ok_or_else(invalid)?;
                    if work > MAX_GRID_REPLAY_WORK || source_active.remove(&process_id).is_none() {
                        return Err(invalid());
                    }
                }
                GridJournalEntry::WindActivate { process_id } => {
                    let zones = self
                        .processes
                        .journal_wind(process_id)
                        .ok_or_else(invalid)?;
                    work = work
                        .checked_add(zones.len().checked_mul(4096).ok_or_else(invalid)?)
                        .ok_or_else(invalid)?;
                    if work > MAX_GRID_REPLAY_WORK {
                        return Err(invalid());
                    }
                    wind.activate(process_id, zones).map_err(|_| invalid())?;
                }
                GridJournalEntry::WindDeactivate { process_id } => {
                    let zones = self
                        .processes
                        .journal_wind(process_id)
                        .ok_or_else(invalid)?;
                    work = work
                        .checked_add(zones.len().checked_mul(4096).ok_or_else(invalid)?)
                        .ok_or_else(invalid)?;
                    if work > MAX_GRID_REPLAY_WORK || !wind.deactivate(process_id) {
                        return Err(invalid());
                    }
                }
            }
        }
        let expected_sources =
            self.light_sources
                .as_ref()
                .map(|_| crate::light_sources::LightSourcesSnapshot {
                    active: source_active
                        .into_iter()
                        .map(|(process_id, (tick, sources))| {
                            crate::light_sources::LightSourceProcessSnapshot {
                                process_id,
                                sources,
                                elapsed_ticks: snapshot.light_tick - tick,
                            }
                        })
                        .collect(),
                });
        if expected_sources != snapshot.light_sources
            || (self.light_sources.is_none() && snapshot.light_tick != 0)
        {
            return Err(invalid());
        }
        if wind.snapshot() != snapshot.wind
            || ambient.as_ref().map(AmbientGrid::snapshot) != snapshot.ambient
            || quake.as_ref().map(QuakeField::snapshot) != snapshot.quake
        {
            return Err(invalid());
        }
        Ok((walls, base_blocking))
    }
    /// Provisional generic muzzle; reference target is player feet + 0.5.
    fn spawn_enemy_projectile(
        &mut self,
        source_index: usize,
        rocket_id: usize,
        origin: [f32; 2],
        target: [f32; 2],
    ) -> Option<u64> {
        let launch = Launch {
            owner: Owner::Enemy { source_index },
            rocket_id,
            origin,
            direction: [target[0] - origin[0], target[1] - origin[1]],
        };
        let id = self.projectiles.spawn(launch, &self.rockets).ok()?;
        let muzzle = 0.5;
        let horizontal = (target[0] - origin[0]).hypot(target[1] - origin[1]);
        let pitch = (self.vertical.feet() + 0.5 - muzzle).atan2(horizontal);
        let Some(speed) = self.projectiles.apply_launch_pitch(id, pitch) else {
            self.projectiles.take(id);
            return None;
        };
        self.projectile_heights.insert(id, muzzle);
        self.projectile_vertical_speeds.insert(id, speed);
        Some(id)
    }

    fn advance_projectile_heights(&mut self, dt: f32, frozen: &BTreeSet<u64>) {
        for projectile in self.projectiles.active() {
            if frozen.contains(&projectile.id) {
                continue;
            }
            if let (Some(height), Some(speed)) = (
                self.projectile_heights.get_mut(&projectile.id),
                self.projectile_vertical_speeds.get(&projectile.id),
            ) {
                *height += *speed * dt;
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
        let prepared = if kind == "due" {
            self.processes.prepare_due(id)
        } else {
            self.processes.prepare_at(kind, cell, id)
        }
        .map_err(|outcome| (outcome, false))?;
        let delayed = prepared.is_delayed();
        let grid_immediate = prepared.actions().iter().any(|a| {
            matches!(
                a,
                ImmediateAction::Ambient(_)
                    | ImmediateAction::Quake(_)
                    | ImmediateAction::WallChange { .. }
            )
        });
        let quakes: Vec<_> = prepared
            .actions()
            .iter()
            .filter_map(|action| match action {
                ImmediateAction::Quake(zone) => Some(*zone),
                _ => None,
            })
            .collect();
        let scripted = self.scripted_objects.as_mut().expect("opted-in state");
        let positions: BTreeMap<_, _> = prepared
            .actions()
            .iter()
            .filter_map(|action| {
                let ImmediateAction::Object(ObjectAction::Explode { cell }) = action else {
                    return None;
                };
                scripted
                    .states
                    .get(*cell)
                    .map(|state| (*cell, state.placement.position))
            })
            .collect();
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
        let mut wall_transitions = Vec::new();
        let mut wall_types = BTreeMap::new();
        for action in prepared.actions() {
            if let ImmediateAction::WallChange { cell, raw_type } = *action {
                let mut matches = self
                    .world
                    .walls
                    .iter()
                    .enumerate()
                    .filter(|(_, wall)| wall.cell == Some(cell));
                let (wall_index, wall) =
                    matches.next().ok_or((ProcessOutcome::Unsupported, true))?;
                if matches.next().is_some() {
                    return Err((ProcessOutcome::Unsupported, true));
                }
                let previous_type = wall_types.get(&wall_index).copied().unwrap_or(wall.texture);
                wall_types.insert(wall_index, raw_type);
                wall_transitions.push(crate::process_runtime::RuntimeWallTransition {
                    cell,
                    wall_index,
                    previous_type,
                    new_type: raw_type,
                });
            }
        }
        let staged_walls = if wall_transitions.is_empty() {
            None
        } else {
            let catalog = self
                .wall_materials
                .as_ref()
                .ok_or((ProcessOutcome::Unsupported, true))?;
            let needed = (self.vertical.head() * 64.0).max(80.0);
            let clear = |wall: &crate::world::Wall, position: [f32; 2], radius: f32| {
                if !position.iter().all(|v| v.is_finite()) || !radius.is_finite() {
                    return false;
                }
                let d = [wall.b[0] - wall.a[0], wall.b[1] - wall.a[1]];
                let length = d[0] * d[0] + d[1] * d[1];
                if !length.is_finite() || length <= 1e-12 {
                    return false;
                }
                let t = (((position[0] - wall.a[0]) * d[0] + (position[1] - wall.a[1]) * d[1])
                    / length)
                    .clamp(0.0, 1.0);
                let dx = position[0] - wall.a[0] - d[0] * t;
                let dy = position[1] - wall.a[1] - d[1] * t;
                dx * dx + dy * dy >= radius * radius
            };
            Some(
                prepare_wall_transitions(
                    &self.world,
                    &self.base_wall_blocking,
                    catalog,
                    &wall_transitions,
                    |index, blocks| {
                        blocks && self.wall_elevations.get(index).is_some_and(|e| *e < needed)
                    },
                    |_, wall| {
                        clear(wall, player, 0.18)
                            && actor_circles.iter().all(|(p, r)| clear(wall, *p, *r))
                    },
                )
                .map_err(|_| (ProcessOutcome::Unsupported, true))?,
            )
        };
        let transaction = ProcessTransaction::prepare_with_wall_transitions(
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
            self.ambient.as_mut(),
            staged_walls
                .as_ref()
                .map(|staged| staged.transitions.as_slice()),
        )
        .map_err(|_| (ProcessOutcome::Unsupported, true))?;
        if delayed {
            return transaction
                .schedule()
                .map_err(|_| (ProcessOutcome::Unsupported, true));
        }
        let transitions = transaction.transitions().to_vec();
        let event = transaction
            .commit()
            .map_err(|_| (ProcessOutcome::Unsupported, true))?;
        for transition in &transitions {
            if transition.kind == TransitionKind::Explode {
                let position = positions[&transition.cell];
                let definition = &scripted.definitions.level_objects
                    [usize::from(transition.previous_type - 163)];
                events.object_destructions.push(ObjectDestructionEvent {
                    cell: transition.cell,
                    raw_type: transition.previous_type,
                    position,
                    sound_id: definition.break_sound,
                    cause: ObjectDestructionCause::Script { process_id: id },
                });
            }
        }
        events.object_transitions.extend(transitions);
        if let Some(staged) = staged_walls {
            self.world = staged.world;
            self.base_wall_blocking = staged.base_blocking;
            events.wall_transitions.extend(staged.transitions);
        }
        events.quake_changes.extend(quakes);
        if grid_immediate {
            self.journal_push(GridJournalEntry::Immediate { process_id: id });
        }
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
        let saved_ambient = self.ambient.clone();
        let saved_walls = self
            .wall_materials
            .as_ref()
            .map(|_| (self.world.clone(), self.base_wall_blocking.clone()));
        let wall_start = events.wall_transitions.len();
        let saved_journal = self.journal_checkpoint();
        let replacement_start = events.object_transitions.len();
        let destruction_start = events.object_destructions.len();
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
                    self.ambient = saved_ambient;
                    if let Some((world, blocking)) = saved_walls {
                        self.world = world;
                        self.base_wall_blocking = blocking;
                    }
                    events.wall_transitions.truncate(wall_start);
                    self.journal_rollback(saved_journal);
                    events.object_transitions.truncate(replacement_start);
                    events.object_destructions.truncate(destruction_start);
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
        let hit = combat::enemy_hitscan_filtered(
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
            |cell, raw| {
                object_combat_overlaps(
                    self.object_heights.as_ref(),
                    &self.processes,
                    cell,
                    raw,
                    1.0,
                    1.0,
                )
            },
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
        let wall_count = events.wall_transitions.len();
        let destruction_count = events.object_destructions.len();
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
                                let direct = events.object_destructions.len();
                                if let Some(info) =
                                    self.objects.as_ref().and_then(|o| o.destruction_info(cell))
                                {
                                    events.object_destructions.push(ObjectDestructionEvent::new(
                                        info,
                                        ObjectDestructionCause::EnemyHitscan {
                                            source_index: attack.source_index,
                                        },
                                    ));
                                }
                                let (destroy_attempts, committed) =
                                    self.activate_destroy([cell[0] as u8, cell[1] as u8], events);
                                if committed {
                                    if direct < events.object_destructions.len() {
                                        deduplicate_script_destruction(
                                            &mut events.object_destructions,
                                            direct,
                                        );
                                    }
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
            events.wall_transitions.truncate(wall_count);
            events.object_destructions.truncate(destruction_count);
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
            targets.extend(objects.explosion_targets_with_types().map(
                |(cell, center, radius, raw)| {
                    let (bottom, top) = object_combat_bounds(
                        self.object_heights.as_ref(),
                        &self.processes,
                        cell,
                        raw,
                    )
                    .unwrap_or(if self.object_heights.is_some() {
                        (-4096.0, 4096.0)
                    } else {
                        (0.0, 1.0)
                    });
                    ExplosionTarget {
                        id: TargetId::Object { cell },
                        cylinder: Cylinder {
                            center,
                            radius,
                            bottom,
                            top,
                        },
                    }
                },
            ));
        }
        let Ok(damage) = explosives::resolve_explosion(explosion, &targets) else {
            return false;
        };
        let before = self.clone();
        let attempt_count = attempts.len();
        let hit_count = events.object_hits.len();
        let transition_count = events.object_transitions.len();
        let wall_count = events.wall_transitions.len();
        let destruction_count = events.object_destructions.len();
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
                events.wall_transitions.truncate(wall_count);
                events.object_destructions.truncate(destruction_count);
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
                        let direct = events.object_destructions.len();
                        if let Some(info) =
                            self.objects.as_ref().and_then(|o| o.destruction_info(cell))
                        {
                            events.object_destructions.push(ObjectDestructionEvent::new(
                                info,
                                ObjectDestructionCause::Explosion {
                                    origin,
                                    owner: explosion.owner,
                                },
                            ));
                        }
                        let (destroy_attempts, accepted) =
                            self.activate_destroy([cell[0] as u8, cell[1] as u8], events);
                        if !accepted {
                            *self = before;
                            attempts.truncate(attempt_count);
                            events.object_hits.truncate(hit_count);
                            events.object_transitions.truncate(transition_count);
                            events.wall_transitions.truncate(wall_count);
                            events.object_destructions.truncate(destruction_count);
                            events.quake_changes.truncate(quake_count);
                            events.damage = previous_damage;
                            return false;
                        }
                        if direct < events.object_destructions.len() {
                            deduplicate_script_destruction(&mut events.object_destructions, direct);
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
            let wall_count = events.wall_transitions.len();
            let destruction_count = events.object_destructions.len();
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
                        events.wall_transitions.truncate(wall_count);
                        events.object_destructions.truncate(destruction_count);
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
    /// Stage one process's geometry and all affected entity positions. Every
    /// interval must succeed before any authoritative state is published.
    fn advance_prepared_motion(&mut self, id: u16, dt: f32) {
        let mut collision_world = self.world.clone();
        if collision_world.walls.len() != self.base_wall_blocking.len() {
            return;
        }
        for (wall, base) in collision_world
            .walls
            .iter_mut()
            .zip(&self.base_wall_blocking)
        {
            // This also protects actors when the player's elevation has made
            // the corresponding live wall temporarily nonblocking.
            wall.blocks_movement = *base;
        }
        let objects: Vec<_> = self.processes.objects().copied().collect();
        let mut cells = BTreeMap::new();
        let mut circles = Vec::new();
        let mut object_z = BTreeMap::new();
        for object in &objects {
            if object.blocking_radius <= 0.0 {
                continue;
            }
            if object.cell[0] >= 64
                || object.cell[1] >= 64
                || cells.insert(object.cell, circles.len()).is_some()
            {
                return;
            }
            circles.push(CollisionCircle {
                center: object.position,
                radius: object.blocking_radius,
            });
            let bounds = self
                .object_heights
                .as_ref()
                .and_then(|heights| {
                    let raw = self.processes.cell_type(object.cell).unwrap_or(
                        heights.initial_raw_cells
                            [usize::from(object.cell[0]) * 64 + usize::from(object.cell[1])],
                    );
                    let bounds = heights.catalog.get(raw)?;
                    let z = [
                        bounds.bottom + object.elevation_raw / 64.0,
                        bounds.top + object.elevation_raw / 64.0,
                    ];
                    (z[0].is_finite()
                        && z[1].is_finite()
                        && z[0] >= -4096.0
                        && z[1] <= 4096.0
                        && z[0] < z[1])
                        .then_some(z)
                })
                .unwrap_or([-4096.0, 4096.0]);
            object_z.insert(object.cell, bounds);
        }
        let mut entities = vec![PushEntity {
            id: EntityId::Player,
            position: self.world.player.position,
            radius: 0.18,
            vertical: [self.vertical.feet(), self.vertical.head()],
        }];
        let mut actor_indices = BTreeMap::new();
        for (index, actor) in self.actors.actors.iter().enumerate() {
            if actor.health <= 0 || self.ai.mode(actor.source_index) == Some(Mode::Inactive) {
                continue;
            }
            let Ok(source) = u32::try_from(actor.source_index) else {
                return;
            };
            let Some(definition) = self.actor_definitions.entries.get(actor.definition_index)
            else {
                return;
            };
            if actor_indices.insert(source, index).is_some() {
                return;
            }
            entities.push(PushEntity {
                id: EntityId::Actor(source),
                position: actor.position,
                radius: (definition.wall_radius as f32 / 256.0).clamp(0.05, 2.0),
                vertical: [0.0, 1.0],
            });
        }
        let Ok(prepared) = self.processes.prepare_motion_step(&mut self.world, id, dt) else {
            return;
        };
        if prepared
            .new_effects()
            .iter()
            .any(|effect| !matches!(effect, ProcessEffect::ObjectPose { .. }))
        {
            return;
        }
        for interval in prepared.intervals() {
            let mut translations = Vec::new();
            for &(index, delta) in &interval.walls {
                translations.push(Translation {
                    mover: MoverId::Wall(index),
                    delta,
                    vertical: [-4096.0, 4096.0],
                });
            }
            for &(cell, delta) in &interval.objects {
                if let Some(&index) = cells.get(&cell) {
                    translations.push(Translation {
                        mover: MoverId::Circle(index),
                        delta,
                        vertical: object_z[&cell],
                    });
                }
            }
            let Ok(MotionProposal::Committed(result)) =
                motion_push::propose(&collision_world, &circles, &entities, &translations)
            else {
                return;
            };
            collision_world = result.world;
            circles = result.circles;
            entities = result.entities;
        }
        let close = |a: [f32; 2], b: [f32; 2]| (a[0] - b[0]).hypot(a[1] - b[1]) <= 0.001;
        if collision_world
            .walls
            .iter()
            .zip(&prepared.proposed_world().walls)
            .any(|(a, b)| !close(a.a, b.a) || !close(a.b, b.b))
        {
            return;
        }
        for (&cell, &index) in &cells {
            let Some(object) = prepared.proposed_object(cell) else {
                return;
            };
            if !close(circles[index].center, object.position) {
                return;
            }
        }
        let mut player = None;
        let mut moved_actors = Vec::new();
        for entity in entities {
            match entity.id {
                EntityId::Player => player = Some(entity.position),
                EntityId::Actor(source) => {
                    let Some(&index) = actor_indices.get(&source) else {
                        return;
                    };
                    moved_actors.push((index, entity.position));
                }
            }
        }
        let Some(player) = player else {
            return;
        };
        if prepared.commit(player).is_err() {
            return;
        }
        for (index, position) in moved_actors {
            self.actors.actors[index].position = position;
        }
    }
    fn apply_process_effects(&mut self) -> Vec<ProcessEffect> {
        let effects = self.processes.take_effects();
        for effect in &effects {
            self.hazards.apply_effect(effect);
            match effect {
                ProcessEffect::WindActivated { process_id, zones } => {
                    match self.wind.activate(*process_id, zones) {
                        Ok(()) => self.journal_push(GridJournalEntry::WindActivate {
                            process_id: *process_id,
                        }),
                        Err(error) => self.pending_wind_errors.push(error),
                    }
                }
                ProcessEffect::WindDeactivated { process_id } => {
                    if self.wind.deactivate(*process_id) {
                        self.journal_push(GridJournalEntry::WindDeactivate {
                            process_id: *process_id,
                        });
                    }
                }
                ProcessEffect::SourceLightsActivated {
                    process_id,
                    sources,
                } => match self.light_sources.as_mut() {
                    Some(lights) => match lights.activate(*process_id, sources) {
                        Ok(()) => self.journal_push(GridJournalEntry::SourceActivate {
                            process_id: *process_id,
                            tick: self.light_tick,
                        }),
                        Err(error) => self.pending_light_errors.push(error),
                    },
                    None => self
                        .pending_light_errors
                        .push(LightSourceError::InvalidDefinition),
                },
                ProcessEffect::SourceLightsDeactivated { process_id } => {
                    if let Some(lights) = &mut self.light_sources {
                        if lights.deactivate(*process_id) {
                            self.journal_push(GridJournalEntry::SourceDeactivate {
                                process_id: *process_id,
                                tick: self.light_tick,
                            });
                        }
                    }
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
        // Follows the independent reconstruction; original-game rounding and
        // protection mechanics still require direct validation.
        if self.game.inventory.health == 0
            || self.game.protective_powerups.chojin_ticks_remaining > 0
        {
            return 0;
        }
        let absorbed = amount.min(self.game.inventory.armor);
        self.game.inventory.armor -= absorbed;
        let damage = (amount - absorbed) + absorbed / 4;
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
        // Existing effects age once per live tick. Pickups collected below
        // begin at full duration and affect AI on this same tick.
        if let Some(lights) = &mut self.light_sources {
            if let Some(next) = self.light_tick.checked_add(1) {
                self.light_tick = next;
            } else {
                self.grid_journal_overflow = true;
            }
            lights.tick();
        }
        self.game.inventory.tick_effects();
        self.game.protective_powerups.tick();
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
        if input.pitch_delta.is_finite() {
            self.pitch = (self.pitch
                + input
                    .pitch_delta
                    .clamp(-MAX_PLAYER_PITCH * 2.0, MAX_PLAYER_PITCH * 2.0))
            .clamp(-MAX_PLAYER_PITCH, MAX_PLAYER_PITCH);
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
        // Radii are provisional circles; optional mesh heights filter
        // vertically separated objects, with conservative unknown fallback.
        circles.extend(
            self.processes
                .objects()
                .filter(|object| {
                    object_blocks_height(
                        self.object_heights.as_ref(),
                        &self.processes,
                        object,
                        self.vertical.feet(),
                        self.vertical.head(),
                        self.vertical.head(),
                    )
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
        if self.wall_collision_mode == WallCollisionMode::AuthoredSides {
            self.world.step_with_external_static_sides(
                input.movement,
                DT,
                &circles,
                displacement,
                self.processes.static_wall_mask(),
            );
        } else {
            self.world
                .step_with_external(input.movement, DT, &circles, displacement);
        }
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
            let displacement = self
                .wind
                .actor_displacement(position, DT)
                .unwrap_or([0.0, 0.0]);
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
                        object_blocks_height(
                            self.object_heights.as_ref(),
                            &self.processes,
                            object,
                            0.0,
                            1.0,
                            self.vertical.head(),
                        )
                    })
                    .map(|object| CollisionCircle {
                        center: object.position,
                        radius: object.blocking_radius,
                    }),
            );
            self.actors.actors[index].position =
                if self.wall_collision_mode == WallCollisionMode::AuthoredSides {
                    self.world.move_circle_with_static_sides(
                        position,
                        displacement,
                        radius,
                        &obstacles,
                        self.processes.static_wall_mask(),
                    )
                } else {
                    self.world
                        .move_circle(position, displacement, radius, &obstacles)
                };
        }
        // Teleport after movement and before pickup, floor links and combat.
        // Destination clearance is conservative and uses provisional flat floors.
        if let Some((definitions, state)) = &mut self.teleports {
            let mut obstacles: Vec<CollisionCircle> = self
                .actors
                .actors
                .iter()
                .filter_map(|actor| {
                    if actor.health <= 0 || self.ai.mode(actor.source_index) == Some(Mode::Inactive)
                    {
                        return None;
                    }
                    let radius = self
                        .actor_definitions
                        .entries
                        .get(actor.definition_index)
                        .map(|def| (def.wall_radius as f32 / 256.0).clamp(0.05, 2.0))
                        .unwrap_or(2.0);
                    Some(CollisionCircle {
                        center: actor.position,
                        radius,
                    })
                })
                .collect();
            let destination_head = self.vertical.config().body_height;
            obstacles.extend(
                self.processes
                    .objects()
                    .filter(|object| {
                        object_blocks_height(
                            self.object_heights.as_ref(),
                            &self.processes,
                            object,
                            0.0,
                            destination_head,
                            destination_head,
                        )
                    })
                    .map(|object| CollisionCircle {
                        center: object.position,
                        radius: object.blocking_radius,
                    }),
            );
            let world = &self.world;
            let base = &self.base_wall_blocking;
            let elevations = &self.wall_elevations;
            let outcome = state.tick(definitions, world.player.position, |destination| {
                const RADIUS: f32 = 0.18;
                destination
                    .iter()
                    .all(|v| *v >= RADIUS && *v <= 64.0 - RADIUS)
                    && destination_head <= 2.0
                    && world.disc_is_clear(destination, RADIUS, &obstacles, |index, _| {
                        base.get(index).copied().unwrap_or(true)
                            && elevations.get(index).copied().unwrap_or(0.0)
                                < (destination_head * 64.0).max(80.0)
                    })
            });
            match outcome {
                Ok(Some(event)) => {
                    self.world.player.position = event.destination;
                    self.world.player.angle = event.angle_radians;
                    self.vertical = VerticalState::with_config(0.0, self.vertical.config())
                        .expect("existing vertical configuration is valid");
                    self.sync_wall_blocking();
                    // A teleport is a new floor entry, even when both positions
                    // happen to be in the same cell. No source floor is fired.
                    self.processes
                        .collect_tick_commands([f32::NAN, f32::NAN], false);
                    events.teleport = Some(event);
                }
                Ok(None) => {}
                Err(error) => events.teleport_failure = Some(error),
            }
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
                    self.ambient.clone(),
                    self.journal_checkpoint(),
                    self.wall_materials
                        .as_ref()
                        .map(|_| (self.world.clone(), self.base_wall_blocking.clone())),
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
            if let Some((game, processes, objects, scripted, ambient, journal, walls)) =
                pickup_backup
            {
                self.game = game;
                self.processes = processes;
                self.objects = objects;
                self.scripted_objects = scripted;
                self.ambient = ambient;
                self.journal_rollback(journal);
                if let Some((world, blocking)) = walls {
                    self.world = world;
                    self.base_wall_blocking = blocking;
                }
            }
            events.pickups.clear();
            events.object_transitions.clear();
            events.wall_transitions.clear();
            events.object_destructions.clear();
            events.quake_changes.clear();
            attempts.clear();
        }
        if self.scripted_objects.is_none() && !self.prepared_motion {
            attempts.extend(self.processes.tick(&mut self.world, DT, input.interact));
        } else {
            let commands = self
                .processes
                .collect_tick_commands(self.world.player.position, input.interact);
            for command in commands {
                if self.scripted_objects.is_none()
                    || matches!(command.kind.as_str(), "return" | "returnf")
                {
                    if let Some(event) = self.processes.dispatch_tick_command(&command) {
                        attempts.push(event);
                    }
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
            if self.prepared_motion {
                let ids: Vec<_> = self.processes.process_ids().collect();
                for id in ids {
                    self.advance_prepared_motion(id, DT);
                }
            } else {
                self.processes.advance_motions(&mut self.world, DT);
            }
            for id in self.processes.ready_delayed() {
                // Failed revalidation leaves the deadline pending for retry.
                if let Err((outcome, _)) = self.try_staged_process("due", [0, 0], id, &mut events) {
                    events.delayed_process_blocked.push(ProcessEvent {
                        process_id: id,
                        outcome,
                    });
                }
            }
        }
        // Process-owned object positions must be applied before this tick's
        // hitscan or projectile queries and before enemy simulation.
        events.process_effects.extend(self.apply_process_effects());
        self.tick_mines(&mut events, &mut attempts);
        self.combat.tick(DT);
        if input.fire {
            let object_before_hit = self.scripted_objects.as_ref().and(self.objects.clone());
            let scripted_before_hit = self.scripted_objects.clone();
            let ambient_before_hit = self.ambient.clone();
            let journal_before_hit = self.journal_checkpoint();
            let walls_before_hit = self
                .wall_materials
                .as_ref()
                .map(|_| (self.world.clone(), self.base_wall_blocking.clone()));
            let wall_events_before_hit = events.wall_transitions.len();
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
                let room = self.player_bounds();
                self.combat.fire_with_aim(
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
                    self.explosions_enabled,
                    combat::ShotAim {
                        eye: self.vertical.eye(),
                        pitch: self.pitch,
                        floor: room.floor,
                        ceiling: room.ceiling.unwrap_or(2.0),
                    },
                    |wall, height| {
                        let offset = wall
                            .cell
                            .and_then(|[x, y]| {
                                self.elevation_by_cell
                                    .get(usize::from(x) * 64 + usize::from(y))
                            })
                            .copied()
                            .unwrap_or(0.0)
                            / 64.0;
                        Self::blocks_shot(
                            wall,
                            &self.shot_blocking,
                            &self.elevation_by_cell,
                            height,
                        ) && height <= offset + 2.0
                    },
                    |cell, raw| {
                        object_combat_bounds(
                            self.object_heights.as_ref(),
                            &self.processes,
                            cell,
                            raw,
                        )
                    },
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
                    let direct = events.object_destructions.len();
                    if destroyed {
                        if let Some(info) =
                            self.objects.as_ref().and_then(|o| o.destruction_info(cell))
                        {
                            events.object_destructions.push(ObjectDestructionEvent::new(
                                info,
                                ObjectDestructionCause::PlayerHitscan,
                            ));
                        }
                    }
                    if let (Ok(x), Ok(y)) = (u8::try_from(cell[0]), u8::try_from(cell[1])) {
                        attempts.extend(self.processes.activate_at("shoot", [x, y]));
                        if destroyed {
                            let (destroy_attempts, accepted) =
                                self.activate_destroy([x, y], &mut events);
                            if accepted {
                                if direct < events.object_destructions.len() {
                                    deduplicate_script_destruction(
                                        &mut events.object_destructions,
                                        direct,
                                    );
                                }
                                attempts.extend(destroy_attempts);
                            } else {
                                events.object_destructions.truncate(direct);
                                self.objects = object_before_hit;
                                self.scripted_objects = scripted_before_hit;
                                self.ambient = ambient_before_hit;
                                self.journal_rollback(journal_before_hit);
                                if let Some((world, blocking)) = walls_before_hit {
                                    self.world = world;
                                    self.base_wall_blocking = blocking;
                                }
                                events.wall_transitions.truncate(wall_events_before_hit);
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
                    if self.pitch != 0.0 {
                        let speed = self
                            .projectiles
                            .apply_launch_pitch(id, self.pitch)
                            .expect("spawned projectile and validated pitch");
                        self.projectile_vertical_speeds.insert(id, speed);
                    }
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
            .filter(|object| {
                object_blocks_height(
                    self.object_heights.as_ref(),
                    &self.processes,
                    object,
                    0.0,
                    1.0,
                    1.0,
                )
            })
            .map(|object| CollisionCircle {
                center: object.position,
                radius: object.blocking_radius,
            })
            .collect();
        self.ai
            .set_player_invisible(self.game.inventory.invisibility_ticks_remaining > 0);
        let blocks_shots = |wall: &crate::world::Wall| {
            Self::blocks_shot(wall, &self.shot_blocking, &self.elevation_by_cell, 1.0)
        };
        let sight_models = if self.object_sight {
            let hidden: BTreeSet<_> = self.game.pickups.hidden_cells().collect();
            let mask = match self.difficulty {
                Difficulty::Easy => 1,
                Difficulty::Normal => 2,
                Difficulty::Hard => 4,
            };
            self.scripted_objects
                .as_ref()
                .map(|scripted| {
                    scripted
                        .states
                        .iter()
                        .filter_map(|state| {
                            let cell = state.placement.grid_cell;
                            if state.placement.difficulty_flags & mask == 0
                                || hidden.contains(&cell)
                                || self
                                    .objects
                                    .as_ref()
                                    .and_then(|objects| objects.is_active(cell))
                                    == Some(false)
                            {
                                return None;
                            }
                            let def = scripted
                                .definitions
                                .level_objects
                                .get(usize::from(state.raw_type.checked_sub(163)?))?;
                            let radius = def.collision_radius as f32 / 256.0;
                            if radius <= 0.0 {
                                return None;
                            }
                            let pose = self.processes.object_pose([cell[0] as u8, cell[1] as u8]);
                            let position =
                                pose.map_or(state.placement.position, |object| object.position);
                            let elevation = pose.map_or(0.0, |object| object.elevation_raw / 64.0);
                            let vertical = self
                                .object_heights
                                .as_ref()
                                .and_then(|heights| heights.catalog.get(state.raw_type))
                                .map(|bounds| [bounds.bottom + elevation, bounds.top + elevation])
                                .filter(|bounds| {
                                    bounds.iter().all(|value| value.is_finite())
                                        && bounds[0] <= bounds[1]
                                })
                                .unwrap_or([-4096.0, 4096.0]);
                            Some(SightCylinder {
                                center: position,
                                radius,
                                vertical,
                            })
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        events.enemy_attacks = if self.object_sight {
            self.ai.tick_with_object_sight(
                &mut self.actors,
                &self.actor_definitions,
                &self.rockets,
                &self.world,
                &blocks_shots,
                &enemy_objects,
                (self.wall_collision_mode == WallCollisionMode::AuthoredSides)
                    .then(|| self.processes.static_wall_mask()),
                &sight_models,
                self.vertical.feet() + 0.5,
            )
        } else if self.wall_collision_mode == WallCollisionMode::AuthoredSides {
            self.ai.tick_with_authored_sides(
                &mut self.actors,
                &self.actor_definitions,
                &self.rockets,
                &self.world,
                &blocks_shots,
                ActorCollisionSides {
                    objects: &enemy_objects,
                    static_walls: self.processes.static_wall_mask(),
                },
            )
        } else {
            self.ai.tick_with_actor_collision(
                &mut self.actors,
                &self.actor_definitions,
                &self.rockets,
                &self.world,
                &blocks_shots,
                &enemy_objects,
            )
        };
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
                self.spawn_enemy_projectile(attack.source_index, rocket_id, origin, target);
                continue;
            }
            events.damage = events
                .damage
                .saturating_add(self.apply_player_damage(attack.damage));
        }
        let pre_tick_projectiles = (self.explosions_enabled
            || self.game.protective_powerups.reflector_ticks_remaining > 0
            || self.addon_boss_rule)
            .then(|| self.projectiles.clone());
        let mut frozen = BTreeSet::new();
        let mut elapsed = 0.0_f32;
        loop {
            let remaining = (DT - elapsed).max(0.0);
            let mut legacy_frozen = frozen.clone();
            legacy_frozen.extend(self.projectile_vertical_speeds.keys().copied());
            let legacy = self.projectiles.trace_next_filtered(
                remaining,
                crate::projectiles::TraceContext {
                    world: &self.world,
                    actors: &self.actors,
                    definitions: &self.actor_definitions,
                    objects: self.objects.as_ref(),
                },
                |wall| Self::blocks_shot(wall, &self.shot_blocking, &self.elevation_by_cell, 1.0),
                &legacy_frozen,
                |projectile, cell, raw| {
                    let height = self
                        .projectile_heights
                        .get(&projectile.id)
                        .copied()
                        .unwrap_or(0.75);
                    object_combat_overlaps(
                        self.object_heights.as_ref(),
                        &self.processes,
                        cell,
                        raw,
                        height - projectile.radius,
                        height + projectile.radius,
                    )
                },
            );
            let mut pitched_frozen = frozen.clone();
            pitched_frozen.extend(
                self.projectiles
                    .active()
                    .iter()
                    .filter(|p| !self.projectile_vertical_speeds.contains_key(&p.id))
                    .map(|p| p.id),
            );
            let aimed = self.projectiles.trace_next_3d(
                remaining,
                crate::projectiles::TraceContext {
                    world: &self.world,
                    actors: &self.actors,
                    definitions: &self.actor_definitions,
                    objects: self.objects.as_ref(),
                },
                &pitched_frozen,
                |id| {
                    (
                        self.projectile_heights.get(&id).copied().unwrap_or(0.75),
                        self.projectile_vertical_speeds
                            .get(&id)
                            .copied()
                            .unwrap_or(0.0),
                    )
                },
                |wall| {
                    if !self
                        .shot_blocking
                        .get(wall.texture as usize)
                        .copied()
                        .unwrap_or(true)
                    {
                        return None;
                    }
                    let offset = wall
                        .cell
                        .and_then(|[x, y]| {
                            self.elevation_by_cell
                                .get(usize::from(x) * 64 + usize::from(y))
                        })
                        .copied()
                        .unwrap_or(0.0)
                        / 64.0;
                    Some((offset, offset + 2.0))
                },
                |cell, raw| {
                    object_combat_bounds(self.object_heights.as_ref(), &self.processes, cell, raw)
                },
                (self.vertical.feet(), self.vertical.head()),
            );
            let candidate = match (legacy, aimed) {
                (Some(a), Some(b)) => Some(
                    if a.time < b.time || (a.time == b.time && a.projectile_id < b.projectile_id) {
                        a
                    } else {
                        b
                    },
                ),
                (a, b) => a.or(b),
            };
            let Some(mut impact) = candidate else {
                if self.projectiles.advance_traced(remaining, &frozen) {
                    self.advance_projectile_heights(remaining, &frozen);
                }
                break;
            };
            if !self.projectiles.advance_traced(impact.time, &frozen) {
                break;
            }
            self.advance_projectile_heights(impact.time, &frozen);
            elapsed = (elapsed + impact.time).min(DT);
            impact.time = elapsed;
            let Some(projectile) = self.projectiles.take(impact.projectile_id) else {
                break;
            };
            events.projectile_impacts.push(impact);
            let height = self
                .projectile_heights
                .remove(&impact.projectile_id)
                .unwrap_or(0.75);
            events.projectile_impact_heights.push(height);
            let vertical_speed = self
                .projectile_vertical_speeds
                .remove(&impact.projectile_id);
            if impact.target == ImpactTarget::Player
                && self.game.protective_powerups.reflector_ticks_remaining > 0
            {
                if self
                    .projectiles
                    .reflect_from_player(projectile, self.world.player.position)
                {
                    self.projectile_heights.insert(impact.projectile_id, height);
                    if let Some(speed) = vertical_speed {
                        self.projectile_vertical_speeds
                            .insert(impact.projectile_id, speed);
                    }
                    events.projectile_impacts.pop();
                    events.projectile_impact_heights.pop();
                    events.projectile_reflections.push(ProjectileReflection {
                        projectile_id: impact.projectile_id,
                        rocket_id: impact.rocket_id,
                        previous_owner: impact.owner,
                        position: impact.position,
                        time: impact.time,
                    });
                } else if pre_tick_projectiles.as_ref().is_some_and(|snapshot| {
                    self.projectiles
                        .restore_impact(snapshot, impact.projectile_id)
                }) {
                    self.projectile_heights.insert(impact.projectile_id, height);
                    if let Some(speed) = vertical_speed {
                        self.projectile_vertical_speeds
                            .insert(impact.projectile_id, speed);
                    }
                    events.projectile_impacts.pop();
                    events.projectile_impact_heights.pop();
                    if let Some(speed) = vertical_speed {
                        self.projectile_heights
                            .insert(impact.projectile_id, height - speed * elapsed);
                    }
                    frozen.insert(impact.projectile_id);
                }
                continue;
            }
            let boss_hit = self.addon_boss_rule
                && impact.rocket_id == 20
                && matches!(projectile.reflected_from, Some(Owner::Enemy { .. }))
                && matches!(impact.target, ImpactTarget::Actor { source_index } if self.actors.actors.iter().any(|actor| actor.source_index == source_index && actor.placement.id == 118 && actor.health > 0));
            if boss_hit {
                let valid_mission = self.mission.as_ref().is_some_and(|mission| {
                    !mission.is_complete()
                        && mission
                            .plan
                            .processes
                            .get(&30)
                            .is_some_and(|process| process.end_delay_seconds.is_some())
                });
                let accepted = valid_mission
                    && self
                        .processes
                        .prepare_forced_mission(30)
                        .is_ok_and(|prepared| {
                            let event = prepared.commit();
                            if event.outcome == ProcessOutcome::Accepted {
                                attempts.push(event);
                                true
                            } else {
                                false
                            }
                        });
                if accepted {
                    if let ImpactTarget::Actor { source_index } = impact.target {
                        if let Some(actor) = self
                            .actors
                            .actors
                            .iter_mut()
                            .find(|actor| actor.source_index == source_index)
                        {
                            actor.health = 0;
                        }
                    }
                } else if pre_tick_projectiles.as_ref().is_some_and(|snapshot| {
                    self.projectiles
                        .restore_impact(snapshot, impact.projectile_id)
                }) {
                    self.projectile_heights.insert(impact.projectile_id, height);
                    if let Some(speed) = vertical_speed {
                        self.projectile_vertical_speeds
                            .insert(impact.projectile_id, speed);
                    }
                    events.projectile_impacts.pop();
                    events.projectile_impact_heights.pop();
                    events
                        .projectile_reflections
                        .retain(|event| event.projectile_id != impact.projectile_id);
                    if let Some(speed) = vertical_speed {
                        self.projectile_heights
                            .insert(impact.projectile_id, height - speed * elapsed);
                    }
                    frozen.insert(impact.projectile_id);
                }
                continue;
            }
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
                    if let Some(speed) = vertical_speed {
                        self.projectile_vertical_speeds
                            .insert(impact.projectile_id, speed);
                    }
                    events.projectile_impacts.pop();
                    events.projectile_impact_heights.pop();
                    if let Some(speed) = vertical_speed {
                        self.projectile_heights
                            .insert(impact.projectile_id, height - speed * elapsed);
                    }
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
                    let ambient_before_hit = self.ambient.clone();
                    let journal_before_hit = self.journal_checkpoint();
                    let walls_before_hit = self
                        .wall_materials
                        .as_ref()
                        .map(|_| (self.world.clone(), self.base_wall_blocking.clone()));
                    let wall_events_before_hit = events.wall_transitions.len();
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
                        let direct = events.object_destructions.len();
                        if destroyed {
                            if let Some(info) =
                                self.objects.as_ref().and_then(|o| o.destruction_info(cell))
                            {
                                events.object_destructions.push(ObjectDestructionEvent::new(
                                    info,
                                    ObjectDestructionCause::Projectile {
                                        projectile_id: impact.projectile_id,
                                        owner: impact.owner,
                                    },
                                ));
                            }
                        }
                        if impact.owner == Owner::Player {
                            if let (Ok(x), Ok(y)) = (u8::try_from(cell[0]), u8::try_from(cell[1])) {
                                attempts.extend(self.processes.activate_at("shoot", [x, y]));
                                if destroyed {
                                    let (destroy_attempts, accepted) =
                                        self.activate_destroy([x, y], &mut events);
                                    if accepted {
                                        if direct < events.object_destructions.len() {
                                            deduplicate_script_destruction(
                                                &mut events.object_destructions,
                                                direct,
                                            );
                                        }
                                        attempts.extend(destroy_attempts);
                                    } else {
                                        events.object_destructions.truncate(direct);
                                        self.objects = object_before_hit;
                                        self.scripted_objects = scripted_before_hit;
                                        self.ambient = ambient_before_hit;
                                        self.journal_rollback(journal_before_hit);
                                        if let Some((world, blocking)) = walls_before_hit {
                                            self.world = world;
                                            self.base_wall_blocking = blocking;
                                        }
                                        events.wall_transitions.truncate(wall_events_before_hit);
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
        events.light_errors = std::mem::take(&mut self.pending_light_errors);
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

    fn push_session(property: &str, action: &str, pin: bool) -> Session {
        let text =
            format!("#proc1\n{property}\n#action\n{action}\n#end\n#links\nlink 0 0 1\n#end\n");
        let mut session = mission_session(&text);
        session.world.player.position = [1.3, 1.0];
        session.world.walls = vec![crate::world::Wall {
            a: [1.0, 0.0],
            b: [1.0, 2.0],
            texture: 1,
            blocks_movement: true,
            cell: Some([1, 0]),
        }];
        if pin {
            session.world.walls.push(crate::world::Wall {
                a: [1.8, 0.0],
                b: [1.8, 2.0],
                texture: 1,
                blocks_movement: true,
                cell: Some([2, 0]),
            });
        }
        session.base_wall_blocking = vec![true; session.world.walls.len()];
        session.wall_elevations = vec![0.0; session.world.walls.len()];
        session.processes = ProcessRuntime::new(&Script::parse(&text), &session.world);
        session.set_prepared_motion(true);
        assert_eq!(
            session.processes.activate_at("link", [0, 0])[0].outcome,
            ProcessOutcome::Accepted
        );
        session
    }

    #[test]
    fn prepared_motion_pushes_player_and_rolls_back_pinned_step() {
        let mut session = push_session("", "move 1 0 128 0", false);
        session.advance_prepared_motion(1, 1.0);
        assert!(session.world.player.position[0] > 1.67);
        assert!((session.world.walls[0].a[0] - 1.5).abs() < 0.001);
        let mut pinned = push_session("", "move 1 0 128 0", true);
        let before = pinned.processes.snapshot(&pinned.world).unwrap();
        pinned.advance_prepared_motion(1, 1.0);
        assert_eq!(pinned.world.player.position, [1.3, 1.0]);
        assert_eq!(pinned.world.walls[0].a, [1.0, 0.0]);
        assert_eq!(pinned.processes.snapshot(&pinned.world).unwrap(), before);
    }

    #[test]
    fn prepared_motion_snapshot_preserves_mode_and_moved_geometry() {
        let mut baseline = push_session("", "move 1 0 128 0", false);
        let text = "#proc1\n#action\nmove 1 0 128 0\n#end\n#links\nlink 0 0 1\n#end\n";
        baseline.processes = ProcessRuntime::new(&Script::parse(text), &baseline.world);
        let mut source = baseline.clone();
        source.processes.activate_at("link", [0, 0]);
        source.advance_prepared_motion(1, 0.2);
        let digest = crate::save_format::ContentDigest([32; 32]);
        let saved = source.snapshot(digest).unwrap();
        assert!(saved.capabilities.prepared_motion);
        let restored = baseline.restore_from_baseline(&saved, digest).unwrap();
        assert_eq!(restored.snapshot(digest).unwrap(), saved);
        baseline.set_prepared_motion(false);
        assert_eq!(
            baseline.restore_from_baseline(&saved, digest).err(),
            Some(SessionSnapshotError::Capabilities)
        );
    }

    #[test]
    fn prepared_motion_pushes_actor_and_freezes_unsupported_processes() {
        let mut session = push_session("", "move 1 0 128 0", false);
        session.world.player.position = [5.0, 5.0];
        session.actor_definitions.entries.push(ActorDefinition {
            model_name: "a.car".into(),
            wall_radius: 64,
            actor_radius: 64,
            speed: 0,
            rotation_speed: 0,
            health: 100,
            kick: 0,
            rocket: -1,
            separation_limit: 0,
            extra_fields: vec![],
            raw_line: String::new(),
        });
        session.actors.actors.push(Actor {
            source_index: 7,
            definition_index: 0,
            placement: Monster {
                position: [333, 256],
                id: 101,
                angle: 0,
                flags: 2,
            },
            position: [1.3, 1.0],
            angle_radians: 0.0,
            health: 100,
        });
        session.ai = EnemyAi::new(&session.actors, Difficulty::Normal);
        session.advance_prepared_motion(1, 1.0);
        assert!(session.actors.actors[0].position[0] > 1.74);
        for (property, action) in [
            ("Mortal", "move 1 0 128 0"),
            ("CheckGo", "move 1 0 128 0"),
            ("", "rotate 1 0 0 0 90"),
        ] {
            let mut frozen = push_session(property, action, false);
            frozen.step(SessionInput::default());
            assert_eq!(frozen.world.walls[0].a, [1.0, 0.0]);
        }
    }

    #[test]
    fn session_codec_roundtrip_and_rejects_untrusted_data() {
        use crate::{save_format, session_codec};
        let baseline = mission_session("");
        let digest = save_format::ContentDigest([55; 32]);
        let snapshot = baseline.snapshot(digest).unwrap();
        let encoded = session_codec::encode(&snapshot).unwrap();
        let decoded = session_codec::decode(&encoded, digest).unwrap();
        assert_eq!(decoded, snapshot);
        let restored = baseline.restore_from_baseline(&decoded, digest).unwrap();
        assert_eq!(restored.snapshot(digest).unwrap(), snapshot);
        assert!(session_codec::decode(&encoded, save_format::ContentDigest([56; 32])).is_err());
        let mut corrupted = encoded.clone();
        corrupted[60] ^= 1;
        assert!(session_codec::decode(&corrupted, digest).is_err());
        assert!(session_codec::decode(&encoded[..encoded.len() - 1], digest).is_err());
        let envelope = save_format::decode(&encoded).unwrap();
        let original: serde_json::Value = serde_json::from_slice(envelope.payload).unwrap();
        let mut future = original.clone();
        future["version"] = 5.into();
        let envelope = save_format::encode(digest, &serde_json::to_vec(&future).unwrap()).unwrap();
        assert!(matches!(
            session_codec::decode(&envelope, digest),
            Err(session_codec::SessionCodecError::UnsupportedVersion(5))
        ));
        let mut legacy = original.clone();
        legacy["version"] = 1.into();
        legacy["snapshot"]
            .as_object_mut()
            .unwrap()
            .remove("light_sources");
        legacy["snapshot"]
            .as_object_mut()
            .unwrap()
            .remove("light_tick");
        for process in legacy["snapshot"]["processes"]["processes"]
            .as_array_mut()
            .unwrap()
        {
            process.as_object_mut().unwrap().remove("source_active");
        }
        let envelope = save_format::encode(digest, &serde_json::to_vec(&legacy).unwrap()).unwrap();
        assert_eq!(session_codec::decode(&envelope, digest).unwrap(), snapshot);
        let mut legacy_v2 = original.clone();
        legacy_v2["version"] = 2.into();
        legacy_v2["snapshot"]
            .as_object_mut()
            .unwrap()
            .remove("pitch");
        legacy_v2["snapshot"]
            .as_object_mut()
            .unwrap()
            .remove("projectile_vertical_speeds");
        let envelope =
            save_format::encode(digest, &serde_json::to_vec(&legacy_v2).unwrap()).unwrap();
        assert_eq!(session_codec::decode(&envelope, digest).unwrap(), snapshot);
        let mut legacy_v3 = original.clone();
        legacy_v3["version"] = 3.into();
        let envelope =
            save_format::encode(digest, &serde_json::to_vec(&legacy_v3).unwrap()).unwrap();
        assert_eq!(session_codec::decode(&envelope, digest).unwrap(), snapshot);
        let mut unknown = original.clone();
        unknown["snapshot"]["world"]["future"] = true.into();
        let envelope = save_format::encode(digest, &serde_json::to_vec(&unknown).unwrap()).unwrap();
        assert!(session_codec::decode(&envelope, digest).is_err());
        let mut bad_content = original.clone();
        bad_content["snapshot"]["content"][0] = 0.into();
        let envelope =
            save_format::encode(digest, &serde_json::to_vec(&bad_content).unwrap()).unwrap();
        assert!(session_codec::decode(&envelope, digest).is_err());
        let giant = vec![b' '; session_codec::MAX_SESSION_PAYLOAD + 1];
        let giant = save_format::encode(digest, &giant).unwrap();
        assert!(matches!(
            session_codec::decode(&giant, digest),
            Err(session_codec::SessionCodecError::PayloadTooLarge)
        ));
        let mut oversized_json = original.clone();
        oversized_json["snapshot"]["grid_journal"] = serde_json::Value::Array(vec![
            serde_json::json!({"Immediate": {"process_id": 1}});
            4097
        ]);
        let envelope =
            save_format::encode(digest, &serde_json::to_vec(&oversized_json).unwrap()).unwrap();
        assert!(matches!(
            session_codec::decode(&envelope, digest),
            Err(session_codec::SessionCodecError::InvalidCardinality)
        ));
        let mut oversized = snapshot.clone();
        oversized
            .grid_journal
            .resize(4097, GridJournalEntry::Immediate { process_id: 1 });
        assert!(matches!(
            session_codec::encode(&oversized),
            Err(session_codec::SessionCodecError::InvalidCardinality)
        ));
        let mut not_finite = snapshot.clone();
        not_finite.world.player_position[0] = f32::NAN;
        assert!(session_codec::encode(&not_finite).is_err());
        let mut illegal = snapshot.clone();
        illegal.inventory.health = 201;
        let encoded = session_codec::encode(&illegal).unwrap();
        let decoded = session_codec::decode(&encoded, digest).unwrap();
        assert!(baseline.restore_from_baseline(&decoded, digest).is_err());
    }

    #[test]
    fn aggregate_snapshot_restores_from_fresh_baseline_atomically() {
        let mut baseline = mission_session("");
        baseline.rockets = RocketDefinitions(vec![RocketDefinition {
            animation_name: String::new(),
            model_name: String::new(),
            power: 10,
            raw_fields: vec![0, 0, 0, 0, 10],
        }]);
        let mut source = baseline.clone();
        source.game.inventory.health = 73;
        source.step(SessionInput {
            fire: true,
            movement: Input {
                forward: 1.0,
                ..Default::default()
            },
            ..Default::default()
        });
        let digest = crate::save_format::ContentDigest([9; 32]);
        let snapshot = source.snapshot(digest).unwrap();
        let mut restored = baseline.restore_from_baseline(&snapshot, digest).unwrap();
        assert_eq!(restored.snapshot(digest).unwrap(), snapshot);
        assert_eq!(
            source
                .step(SessionInput {
                    fire: true,
                    ..Default::default()
                })
                .shot,
            restored
                .step(SessionInput {
                    fire: true,
                    ..Default::default()
                })
                .shot
        );
        assert_eq!(
            source.snapshot(digest).unwrap(),
            restored.snapshot(digest).unwrap()
        );
        let mut invalid = snapshot.clone();
        invalid.inventory.health = 201;
        assert_eq!(
            baseline.restore_from_baseline(&invalid, digest).err(),
            Some(SessionSnapshotError::Component("inventory"))
        );
        assert_eq!(baseline.game.inventory.health, 100);
        assert_eq!(
            baseline
                .restore_from_baseline(&snapshot, crate::save_format::ContentDigest([8; 32]))
                .err(),
            Some(SessionSnapshotError::Content)
        );
        invalid = snapshot.clone();
        invalid.projectile_heights.push((999, 1.0));
        assert_eq!(
            baseline.restore_from_baseline(&invalid, digest).err(),
            Some(SessionSnapshotError::Inconsistent("projectile heights"))
        );
        invalid = snapshot.clone();
        invalid.wind.cells[0] = [1, 0];
        assert_eq!(
            baseline.restore_from_baseline(&invalid, digest).err(),
            Some(SessionSnapshotError::Unverifiable("grid journal"))
        );
        invalid = snapshot.clone();
        invalid.capabilities.explosions = !invalid.capabilities.explosions;
        assert_eq!(
            baseline.restore_from_baseline(&invalid, digest).err(),
            Some(SessionSnapshotError::Capabilities)
        );
        invalid = snapshot.clone();
        invalid.selected_weapon = 7;
        assert_eq!(
            baseline.restore_from_baseline(&invalid, digest).err(),
            Some(SessionSnapshotError::Inconsistent("selected weapon"))
        );
    }

    #[test]
    fn aggregate_snapshot_validates_moving_wall_and_rejects_corruption() {
        let digest = crate::save_format::ContentDigest([1; 32]);
        let text = "#proc1\n#action\nplayani 1\n#end\n#links\nlink 0 0 1\n#end\n";
        let mut pending = mission_session(text);
        assert_eq!(
            pending.processes.activate_at("link", [0, 0])[0].outcome,
            ProcessOutcome::Accepted
        );
        assert_eq!(
            pending.snapshot(digest).err(),
            Some(SessionSnapshotError::PendingEffects)
        );
        let text = "#proc1\n#action\nmove 1 0 256 0\n#end\n#links\nfloor 0 0 1\n#end\n";
        let mut baseline = mission_session(text);
        baseline.world.walls.push(crate::world::Wall {
            a: [1.0, 0.0],
            b: [1.0, 1.0],
            texture: 1,
            blocks_movement: true,
            cell: Some([1, 0]),
        });
        baseline.processes = ProcessRuntime::new(&Script::parse(text), &baseline.world);
        baseline.wall_elevations = vec![0.0];
        baseline.base_wall_blocking = vec![true];
        let mut moved = baseline.clone();
        moved.step(SessionInput::default());
        let snapshot = moved.snapshot(digest).unwrap();
        let mut restored = baseline.restore_from_baseline(&snapshot, digest).unwrap();
        assert_eq!(restored.world.snapshot(), moved.world.snapshot());
        restored.step(SessionInput::default());
        moved.step(SessionInput::default());
        assert_eq!(restored.world.snapshot(), moved.world.snapshot());
        let mut corrupted = snapshot.clone();
        corrupted.world.walls[0].a[0] += 0.1;
        assert_eq!(
            baseline.restore_from_baseline(&corrupted, digest).err(),
            Some(SessionSnapshotError::Inconsistent("process geometry"))
        );
    }

    #[test]
    fn aggregate_snapshot_preserves_delayed_retry_on_fresh_session() {
        let text =
            "#proc1\nstartdelay 0.05\n#action\nambient 0 0 2 2 5\n#end\n#links\nlink 0 0 1\n#end\n";
        let baseline = ambient_session(text);
        let mut source = baseline.clone();
        source
            .try_staged_process("link", [0, 0], 1, &mut SessionEvents::default())
            .unwrap();
        source.step(SessionInput::default());
        let digest = crate::save_format::ContentDigest([4; 32]);
        let snapshot = source.snapshot(digest).unwrap();
        let mut restored = baseline.restore_from_baseline(&snapshot, digest).unwrap();
        for _ in 0..5 {
            let a = source.step(SessionInput::default());
            let b = restored.step(SessionInput::default());
            assert_eq!(a.delayed_process_blocked, b.delayed_process_blocked);
            assert_eq!(source.ambient_grid(), restored.ambient_grid());
        }
        assert_eq!(restored.ambient_grid().unwrap().cell([0, 0]), Some(5));
        let committed = restored.snapshot(digest).unwrap();
        assert_eq!(
            baseline
                .restore_from_baseline(&committed, digest)
                .unwrap()
                .snapshot(digest)
                .unwrap(),
            committed
        );
    }

    #[test]
    fn grid_journal_restores_ordered_overlapping_authored_effects_and_rejects_forgery() {
        let text = "#proc1\n#action\nambient 0 0 2 2 5\nambient 1 1 3 3 7\nquake 0 0 2 2 30\nquake 1 1 2 2 70\n#end\n#proc2\n#action\nnonstop\nwind 0 0 3 3 20 0\n#end\n#proc3\n#action\nnonstop\nwind 1 1 4 4 0 30 1\n#end\n#proc4\n#action\nlock 2\n#end\n#links\nlink 0 0 1\nlink 0 0 2\nlink 0 0 3\nlink 0 0 4\n#end\n";
        let mut baseline = ambient_session(text);
        baseline.scripted_objects.as_mut().unwrap().quake_enabled = true;
        let mut source = baseline.clone();
        let digest = crate::save_format::ContentDigest([21; 32]);
        for id in [1, 2, 3, 4] {
            source
                .try_staged_process("link", [0, 0], id, &mut SessionEvents::default())
                .unwrap();
            source.apply_process_effects();
        }
        assert_eq!(source.wind.cell([1, 1]), Some([0, 0]));
        assert_eq!(source.ambient_grid().unwrap().cell([1, 1]), Some(7));
        assert_eq!(
            source.scripted_objects.as_ref().unwrap().quake.cell([1, 1]),
            Some(70)
        );
        let snapshot = source.snapshot(digest).unwrap();
        let restored = baseline.restore_from_baseline(&snapshot, digest).unwrap();
        assert_eq!(restored.snapshot(digest).unwrap(), snapshot);
        assert!(snapshot.grid_journal.len() >= 4);
        let mut forged = snapshot.clone();
        forged.wind.cells[0] = [100, 0];
        assert!(baseline.restore_from_baseline(&forged, digest).is_err());
        let mut forged = snapshot.clone();
        forged.grid_journal[0] = GridJournalEntry::Immediate { process_id: 999 };
        assert!(baseline.restore_from_baseline(&forged, digest).is_err());
        let mut forged = snapshot.clone();
        forged
            .grid_journal
            .push(GridJournalEntry::WindActivate { process_id: 3 });
        assert!(baseline.restore_from_baseline(&forged, digest).is_err());
        let mut forged = snapshot.clone();
        forged.ambient.as_mut().unwrap().cells[0] = 42;
        assert!(baseline.restore_from_baseline(&forged, digest).is_err());
        let mut forged = snapshot.clone();
        forged.quake.as_mut().unwrap().cells[0] = 999;
        assert!(baseline.restore_from_baseline(&forged, digest).is_err());
        let mut forged = snapshot.clone();
        forged.grid_journal.resize(
            MAX_GRID_JOURNAL + 1,
            GridJournalEntry::Immediate { process_id: 1 },
        );
        assert!(baseline.restore_from_baseline(&forged, digest).is_err());
    }

    #[test]
    fn aggregate_snapshot_validates_raised_wall_elevation() {
        use crate::process_runtime::ProcessCapabilities;
        let text = "#proc1\n#action\nup 1 0 80\n#end\n#links\nfloor 0 0 1\n#end\n";
        let mut baseline = mission_session(text);
        baseline.world.walls.push(crate::world::Wall {
            a: [1.0, 0.0],
            b: [1.0, 1.0],
            texture: 1,
            blocks_movement: true,
            cell: Some([1, 0]),
        });
        baseline.processes = ProcessRuntime::with_capabilities(
            &Script::parse(text),
            &baseline.world,
            ProcessCapabilities {
                wall_elevation: true,
                initial_cell_types: None,
            },
        );
        baseline.wall_elevations = vec![0.0];
        baseline.base_wall_blocking = vec![true];
        let mut source = baseline.clone();
        for _ in 0..30 {
            source.step(SessionInput::default());
        }
        assert!(source.wall_elevations[0] > 0.0);
        let digest = crate::save_format::ContentDigest([7; 32]);
        let snapshot = source.snapshot(digest).unwrap();
        let restored = baseline.restore_from_baseline(&snapshot, digest).unwrap();
        assert_eq!(restored.wall_elevations, source.wall_elevations);
        let mut forged = snapshot.clone();
        forged.wall_elevations[0] += 1.0;
        forged.processes.elevations[0] += 1.0;
        forged.elevation_by_cell[64] += 1.0;
        assert_eq!(
            baseline.restore_from_baseline(&forged, digest).err(),
            Some(SessionSnapshotError::Inconsistent("process geometry"))
        );
    }

    fn wall_change_session(text: &str) -> Session {
        use crate::process_runtime::ProcessCapabilities;
        use crate::wall_transitions::WallMaterial;
        let mut session = ambient_session(text);
        session.world.walls.push(crate::world::Wall {
            a: [1.0, 0.0],
            b: [1.0, 1.0],
            texture: 87,
            blocks_movement: false,
            cell: Some([1, 0]),
        });
        session.base_wall_blocking.push(false);
        session.wall_elevations.push(0.0);
        let mut cells = [0; 4096];
        cells[64] = 87;
        session.processes = ProcessRuntime::with_wall_changes(
            &Script::parse(text),
            &session.world,
            ProcessCapabilities::default(),
            &cells,
            &[],
        )
        .unwrap();
        session.light_sources = Some(LightSources::default());
        session.shot_blocking[2] = false;
        let mut catalog = WallMaterialCatalog::default();
        assert!(catalog.set(
            87,
            WallMaterial {
                blocks_movement: false,
                blocks_shots: true,
                presentation_available: true
            }
        ));
        assert!(catalog.set(
            2,
            WallMaterial {
                blocks_movement: true,
                blocks_shots: false,
                presentation_available: true
            }
        ));
        assert!(session.set_wall_material_catalog(catalog));
        session
    }

    #[test]
    fn snapshot_restores_source_lifecycle_elapsed_time_and_rejects_forgery() {
        let text = "#proc1\n#action\nnonstop\nsource 1 1 5 28 160\n#end\n#proc2\n#action\nlock 1\n#end\n#links\nlink 0 0 1\nlink 0 0 2\n#end\n";
        let baseline = wall_change_session(text);
        let mut source = baseline.clone();
        source
            .try_staged_process("link", [0, 0], 1, &mut SessionEvents::default())
            .unwrap();
        source.apply_process_effects();
        for _ in 0..5 {
            source.step(SessionInput::default());
        }
        let digest = crate::save_format::ContentDigest([42; 32]);
        let snapshot = source.snapshot(digest).unwrap();
        assert_eq!(
            snapshot.light_sources.as_ref().unwrap().active[0].elapsed_ticks,
            5
        );
        let encoded = crate::session_codec::encode(&snapshot).unwrap();
        let snapshot = crate::session_codec::decode(&encoded, digest).unwrap();
        let mut restored = baseline.restore_from_baseline(&snapshot, digest).unwrap();
        assert_eq!(snapshot, restored.snapshot(digest).unwrap());
        source.step(SessionInput::default());
        restored.step(SessionInput::default());
        assert_eq!(
            source.light_sources.as_ref().unwrap().snapshot(),
            restored.light_sources.as_ref().unwrap().snapshot()
        );
        let mut forged = snapshot.clone();
        forged.light_sources.as_mut().unwrap().active[0].elapsed_ticks += 1;
        assert!(baseline.restore_from_baseline(&forged, digest).is_err());
        let mut forged = snapshot.clone();
        forged.light_sources.as_mut().unwrap().active[0].sources[0].brightness += 1;
        assert!(baseline.restore_from_baseline(&forged, digest).is_err());
        let mut forged = snapshot.clone();
        forged.processes.processes[0].source_active = false;
        assert!(baseline.restore_from_baseline(&forged, digest).is_err());
        let mut forged = snapshot.clone();
        forged.grid_journal.clear();
        assert!(baseline.restore_from_baseline(&forged, digest).is_err());
        let mut forged = snapshot.clone();
        forged
            .grid_journal
            .push(GridJournalEntry::SourceDeactivate {
                process_id: 1,
                tick: forged.light_tick + 1,
            });
        assert!(baseline.restore_from_baseline(&forged, digest).is_err());
        restored
            .try_staged_process("link", [0, 0], 2, &mut SessionEvents::default())
            .unwrap();
        restored.apply_process_effects();
        assert!(restored
            .light_sources
            .as_ref()
            .unwrap()
            .active()
            .next()
            .is_none());
        let ended = restored.snapshot(digest).unwrap();
        assert_eq!(
            baseline
                .restore_from_baseline(&ended, digest)
                .unwrap()
                .snapshot(digest)
                .unwrap(),
            ended
        );
    }

    #[test]
    fn wall_change_commits_material_and_blocks_player_overlap_atomically() {
        let text =
            "#proc1\n#action\nchange 1 0 2\nambient 0 0 0 0 5\n#end\n#links\nlink 0 0 1\n#end\n";
        let baseline = wall_change_session(text);
        let mut source = baseline.clone();
        source.world.player.position = [1.0, 0.5];
        let mut events = SessionEvents::default();
        assert!(source
            .try_staged_process("link", [0, 0], 1, &mut events)
            .is_err());
        assert!(events.wall_transitions.is_empty());
        assert_eq!(source.world.walls[0].texture, 87);
        assert_eq!(source.processes.cell_type([1, 0]), Some(87));
        assert_eq!(source.ambient_grid().unwrap().cell([0, 0]), Some(0));
        source.world.player.position = [0.5, 0.5];
        source
            .try_staged_process("link", [0, 0], 1, &mut events)
            .unwrap();
        assert_eq!(events.wall_transitions.len(), 1);
        assert_eq!(source.world.walls[0].texture, 2);
        assert!(source.world.walls[0].blocks_movement);
        assert!(source.base_wall_blocking[0]);
        assert_eq!(source.processes.cell_type([1, 0]), Some(2));
        assert_eq!(source.ambient_grid().unwrap().cell([0, 0]), Some(5));
        assert!(!Session::blocks_shot(
            &source.world.walls[0],
            &source.shot_blocking,
            &source.elevation_by_cell,
            1.0
        ));
        source.apply_process_effects();
        let digest = crate::save_format::ContentDigest([12; 32]);
        let snapshot = source.snapshot(digest).unwrap();
        let encoded = crate::session_codec::encode(&snapshot).unwrap();
        let snapshot = crate::session_codec::decode(&encoded, digest).unwrap();
        let restored = baseline.restore_from_baseline(&snapshot, digest).unwrap();
        assert_eq!(restored.snapshot(digest).unwrap(), snapshot);
        let mut forged = snapshot.clone();
        forged.grid_journal.clear();
        assert!(baseline.restore_from_baseline(&forged, digest).is_err());
        let mut forged = snapshot.clone();
        forged.world.walls[0].texture = 87;
        assert!(baseline.restore_from_baseline(&forged, digest).is_err());
        let mut forged = snapshot.clone();
        forged.processes.cell_types.as_mut().unwrap()[64] = 87;
        assert!(baseline.restore_from_baseline(&forged, digest).is_err());
        let mut forged = snapshot.clone();
        forged.processes.wall_identity[0].1 = 87;
        assert!(baseline.restore_from_baseline(&forged, digest).is_err());
        let mut forged = snapshot.clone();
        forged.world.walls[0].blocks_movement = false;
        assert!(baseline.restore_from_baseline(&forged, digest).is_err());
        let mut missing = baseline.clone();
        let mut catalog = missing.wall_materials.clone().unwrap();
        catalog.set(
            2,
            crate::wall_transitions::WallMaterial {
                blocks_movement: true,
                blocks_shots: false,
                presentation_available: false,
            },
        );
        assert!(missing.set_wall_material_catalog(catalog));
        assert!(missing.restore_from_baseline(&snapshot, digest).is_err());
    }

    #[test]
    fn wall_journal_roundtrips_wall_only_reversal_and_continues() {
        let text = "#proc1\n#action\nchange 1 0 2\n#end\n#proc2\n#action\nchange 1 0 87\n#end\n#links\nlink 0 0 1\nlink 0 0 2\n#end\n";
        let baseline = wall_change_session(text);
        let mut source = baseline.clone();
        let digest = crate::save_format::ContentDigest([19; 32]);
        for id in [1, 2] {
            source
                .try_staged_process("link", [0, 0], id, &mut SessionEvents::default())
                .unwrap();
            source.apply_process_effects();
        }
        assert_eq!(
            source.grid_journal,
            vec![
                GridJournalEntry::Immediate { process_id: 1 },
                GridJournalEntry::Immediate { process_id: 2 }
            ]
        );
        let snapshot = source.snapshot(digest).unwrap();
        let mut restored = baseline.restore_from_baseline(&snapshot, digest).unwrap();
        assert_eq!(restored.snapshot(digest).unwrap(), snapshot);
        source.step(SessionInput::default());
        restored.step(SessionInput::default());
        assert_eq!(
            restored.snapshot(digest).unwrap(),
            source.snapshot(digest).unwrap()
        );
        let mut forged = snapshot.clone();
        forged.grid_journal.reverse();
        assert!(baseline.restore_from_baseline(&forged, digest).is_err());
    }

    #[test]
    fn wall_journal_restores_a_changed_moving_wall() {
        let text = "#proc1\nspeed 1\n#action\nchange 1 0 2\nmove 1 0 64 0\n#end\n#links\nlink 0 0 1\n#end\n";
        let baseline = wall_change_session(text);
        let mut source = baseline.clone();
        source
            .try_staged_process("link", [0, 0], 1, &mut SessionEvents::default())
            .unwrap();
        for _ in 0..8 {
            source.step(SessionInput::default());
        }
        let digest = crate::save_format::ContentDigest([20; 32]);
        let snapshot = source.snapshot(digest).unwrap();
        let mut restored = baseline.restore_from_baseline(&snapshot, digest).unwrap();
        source.step(SessionInput::default());
        restored.step(SessionInput::default());
        assert_eq!(source.world.snapshot(), restored.world.snapshot());
    }

    #[test]
    fn wall_change_checks_living_actor_and_presentation() {
        use crate::wall_transitions::WallMaterial;
        let text = "#proc1\n#action\nchange 1 0 2\n#end\n#links\nlink 0 0 1\n#end\n";
        let mut session = wall_change_session(text);
        session.actor_definitions.entries.push(ActorDefinition {
            model_name: "a.car".into(),
            wall_radius: 64,
            actor_radius: 64,
            speed: 0,
            rotation_speed: 0,
            health: 100,
            kick: 0,
            rocket: -1,
            separation_limit: 0,
            extra_fields: vec![],
            raw_line: String::new(),
        });
        session.actors.actors.push(Actor {
            source_index: 7,
            definition_index: 0,
            placement: Monster {
                position: [256, 128],
                id: 101,
                angle: 0,
                flags: 2,
            },
            position: [1.0, 0.5],
            angle_radians: 0.0,
            health: 100,
        });
        session.ai = EnemyAi::new(&session.actors, Difficulty::Normal);
        assert!(session
            .try_staged_process("link", [0, 0], 1, &mut SessionEvents::default())
            .is_err());
        session.actors.actors[0].health = 0;
        let mut unavailable = WallMaterialCatalog::default();
        unavailable.set(
            87,
            WallMaterial {
                blocks_movement: false,
                blocks_shots: true,
                presentation_available: true,
            },
        );
        unavailable.set(
            2,
            WallMaterial {
                blocks_movement: true,
                blocks_shots: false,
                presentation_available: false,
            },
        );
        assert!(session.set_wall_material_catalog(unavailable));
        assert!(session
            .try_staged_process("link", [0, 0], 1, &mut SessionEvents::default())
            .is_err());
        assert_eq!(session.world.walls[0].texture, 87);
        let mut available = WallMaterialCatalog::default();
        available.set(
            87,
            WallMaterial {
                blocks_movement: false,
                blocks_shots: true,
                presentation_available: true,
            },
        );
        available.set(
            2,
            WallMaterial {
                blocks_movement: true,
                blocks_shots: false,
                presentation_available: true,
            },
        );
        assert!(session.set_wall_material_catalog(available));
        session
            .try_staged_process("link", [0, 0], 1, &mut SessionEvents::default())
            .unwrap();
        assert_eq!(session.world.walls[0].texture, 2);
    }

    #[test]
    fn delayed_wall_change_revalidates_and_retries_clearance() {
        let text =
            "#proc1\nstartdelay 0.05\n#action\nchange 1 0 2\n#end\n#links\nlink 0 0 1\n#end\n";
        let mut session = wall_change_session(text);
        let mut events = SessionEvents::default();
        session
            .try_staged_process("link", [0, 0], 1, &mut events)
            .unwrap();
        assert!(events.wall_transitions.is_empty());
        session.world.player.position = [1.0, 0.5];
        for _ in 0..5 {
            session.step(SessionInput::default());
        }
        assert_eq!(session.processes.ready_delayed(), vec![1]);
        assert_eq!(session.world.walls[0].texture, 87);
        session.world.player.position = [0.5, 0.5];
        let result = session.step(SessionInput::default());
        assert_eq!(result.wall_transitions.len(), 1);
        assert_eq!(session.world.walls[0].texture, 2);
        assert!(session.processes.ready_delayed().is_empty());
    }

    #[test]
    fn failed_destroy_group_rolls_back_wall_and_events() {
        let text = "#proc1\n#action\nchange 1 0 2\n#end\n#proc2\n#action\nchange 3 0 163\n#end\n#links\ndestroy 4 0 1\ndestroy 4 0 2\n#end\n";
        let mut session = wall_change_session(text);
        let mut events = SessionEvents::default();
        let (_, accepted) = session.activate_destroy([4, 0], &mut events);
        assert!(!accepted);
        assert!(events.wall_transitions.is_empty());
        assert_eq!(session.world.walls[0].texture, 87);
        assert!(!session.base_wall_blocking[0]);
        assert_eq!(session.processes.cell_type([1, 0]), Some(87));
    }

    fn ambient_session(text: &str) -> Session {
        use crate::process_runtime::ProcessCapabilities;
        let script = Script::parse(text);
        let mut session = mission_session(text);
        let cells = [0; 4096];
        session.processes = ProcessRuntime::with_staged_actions(
            &script,
            &session.world,
            ProcessCapabilities::default(),
            &cells,
            &[],
        )
        .unwrap();
        let defs = ObjectDefinitions {
            items: vec![],
            level_objects: vec![],
        };
        let states = ObjectStates::new(&[], &defs).unwrap();
        session.set_objects(ObjectCombat::new(&[], &defs, &script, Difficulty::Normal));
        session.set_scripted_objects(ScriptedObjects::new(
            states,
            defs,
            PresentationManifest::default(),
            &script,
            false,
        ));
        let map = crate::map::Map::parse(&vec![0; 0x27005]).unwrap();
        session.set_ambient_grid(AmbientGrid::from_map(&map));
        session
    }

    #[test]
    fn staged_ambient_requires_capability_and_preserves_action_order() {
        let text = "#proc1\n#action\nambient 0 0 2 2 5\nambient 1 1 3 3 7\n#end\n#links\nlink 0 0 1\n#end\n";
        let mut session = ambient_session(text);
        let mut no_grid = session.clone();
        no_grid.ambient = None;
        assert!(no_grid
            .try_staged_process("link", [0, 0], 1, &mut SessionEvents::default())
            .is_err());
        let mut direct = session.processes.clone();
        assert_eq!(
            direct
                .prepare_at("link", [0, 0], 1)
                .unwrap()
                .commit()
                .outcome,
            ProcessOutcome::Unsupported
        );
        let event = session
            .try_staged_process("link", [0, 0], 1, &mut SessionEvents::default())
            .unwrap();
        assert_eq!(event.outcome, ProcessOutcome::Accepted);
        assert_eq!(session.ambient_grid().unwrap().cell([0, 0]), Some(5));
        assert_eq!(session.ambient_grid().unwrap().cell([1, 1]), Some(7));
        assert_eq!(session.ambient_grid().unwrap().cell([3, 3]), Some(7));
        assert_eq!(session.ambient_grid().unwrap().cell([4, 4]), Some(0));
    }

    #[test]
    fn delayed_ambient_revalidates_at_deadline_and_retries_atomically() {
        let text = "#proc1\nstartdelay 0.05\n#action\nambient 0 0 2 2 5\nunlock 2\n#end\n#proc2\nlock\nonmessage 1\n#end\n#links\nlink 0 0 1\nlink 3 0 2\n#end\n";
        let mut session = ambient_session(text);
        let before = session.ambient.clone().unwrap();
        let mut no_grid = session.clone();
        no_grid.ambient = None;
        assert!(no_grid
            .try_staged_process("link", [0, 0], 1, &mut SessionEvents::default())
            .is_err());
        assert!(no_grid.processes.ready_delayed().is_empty());
        assert!(session
            .try_staged_process("link", [0, 0], 1, &mut SessionEvents::default())
            .is_ok());
        assert_eq!(session.ambient.as_ref().unwrap(), &before);
        assert!(session.grid_journal.is_empty());
        assert_eq!(
            session.processes.activate_at("link", [3, 0])[0].outcome,
            ProcessOutcome::Locked
        );
        session.ambient = None;
        let mut blocked = false;
        for _ in 0..5 {
            let events = session.step(SessionInput::default());
            assert!(events.process_effects.is_empty());
            blocked |= events
                .delayed_process_blocked
                .iter()
                .any(|event| event.process_id == 1 && event.outcome == ProcessOutcome::Unsupported);
        }
        assert!(blocked);
        assert!(session.grid_journal.is_empty());
        assert_eq!(session.processes.ready_delayed(), vec![1]);
        assert_eq!(
            session.processes.activate_at("link", [3, 0])[0].outcome,
            ProcessOutcome::Locked
        );
        let mut restored = session.clone();
        session.ambient = Some(before.clone());
        restored.ambient = Some(before);
        session.step(SessionInput::default());
        restored.step(SessionInput::default());
        assert_eq!(session.ambient_grid().unwrap().cell([0, 0]), Some(5));
        assert_eq!(
            session.grid_journal,
            vec![GridJournalEntry::Immediate { process_id: 1 }]
        );
        assert_eq!(session.ambient, restored.ambient);
        assert!(session.processes.ready_delayed().is_empty());
        assert_ne!(
            session.processes.activate_at("link", [3, 0])[0].outcome,
            ProcessOutcome::Locked
        );
        session.step(SessionInput::default());
        assert!(session.processes.ready_delayed().is_empty());
    }

    #[test]
    fn failed_multi_destroy_rolls_back_ambient_with_other_processes() {
        let text = "#proc1\n#action\nambient 0 0 2 2 7\nunlock 3\n#end\n#proc2\n#action\nchange 3 0 163\n#end\n#proc3\nlock\nonmessage 1\n#end\n#links\ndestroy 1 1 1\ndestroy 1 1 2\nlink 0 0 3\n#end\n";
        let mut session = ambient_session(text);
        let before = session.ambient_grid().unwrap().clone();
        let (_, accepted) = session.activate_destroy([1, 1], &mut SessionEvents::default());
        assert!(!accepted);
        assert!(session.grid_journal.is_empty());
        assert_eq!(session.ambient_grid().unwrap(), &before);
        assert_eq!(
            session.processes.activate_at("link", [0, 0])[0].outcome,
            ProcessOutcome::Locked
        );
    }

    fn height_combat_fixture() -> Session {
        use crate::object_heights::{HeightBounds, ObjectHeightCatalog};
        use crate::objects::{ObjectDefinition, ObjectInstance, ObjectKind};
        use crate::process_runtime::{ProcessCapabilities, ProcessObject};
        let mut session = mission_session("");
        let def = |name: &str| ObjectDefinition {
            collision_radius: 40,
            shadow: 0,
            bmp_object: 0,
            bmp_z: 0,
            action_code: 0,
            blow_up: 1,
            break_limit: 100,
            ambient_sound: 0,
            break_sound: 0,
            model_name: name.into(),
            animation_name: None,
        };
        let defs = ObjectDefinitions {
            items: vec![],
            level_objects: vec![def("roof.3o"), def("floor.3o")],
        };
        let instance = |x, index| ObjectInstance {
            kind: ObjectKind::LevelObject,
            definition_index: index,
            grid_cell: [x, 0],
            position: [x as f32 + 0.5, 0.5],
            angle_radians: 0.0,
            flags: 0,
            difficulty_flags: 7,
            extra: 0,
            size: 0,
        };
        let instances = [instance(2, 0), instance(3, 1)];
        let mut cells = [0; 4096];
        cells[2 * 64] = 163;
        cells[3 * 64] = 164;
        let poses = instances.clone().map(|i| ProcessObject {
            cell: [i.grid_cell[0] as u8, 0],
            position: i.position,
            angle_radians: 0.0,
            elevation_raw: 0.0,
            blocking_radius: 0.0,
        });
        let script = Script::default();
        session.processes = ProcessRuntime::with_wind(
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
        let mut catalog = ObjectHeightCatalog::new();
        assert!(catalog.set(163, HeightBounds::new(2.0, 3.0).unwrap()));
        assert!(catalog.set(164, HeightBounds::new(0.0, 1.0).unwrap()));
        assert!(session.set_object_heights(catalog, cells));
        session.rockets = RocketDefinitions(vec![
            RocketDefinition {
                animation_name: String::new(),
                model_name: "".into(),
                power: 10,
                raw_fields: vec![0, 0, 0, 0, 10],
            },
            RocketDefinition {
                animation_name: String::new(),
                model_name: "rocket.3o".into(),
                power: 10,
                raw_fields: vec![0, 0, 0, 1, 10],
            },
        ]);
        session.game.inventory.weapons[0] = true;
        session.game.inventory.ammo[0] = 5;
        session
    }

    #[test]
    fn object_sight_uses_live_height_and_disabled_state() {
        use crate::objects::{ObjectDefinition, ObjectInstance, ObjectKind};
        let mut session = height_combat_fixture();
        assert!(!session.set_object_sight(true));
        let def = |name: &str| ObjectDefinition {
            collision_radius: 40,
            shadow: 0,
            bmp_object: 0,
            bmp_z: 0,
            action_code: 0,
            blow_up: 1,
            break_limit: 100,
            ambient_sound: 0,
            break_sound: 0,
            model_name: name.into(),
            animation_name: None,
        };
        let defs = ObjectDefinitions {
            items: vec![],
            level_objects: vec![def("roof.3o"), def("floor.3o")],
        };
        let instance = |x, definition_index| ObjectInstance {
            kind: ObjectKind::LevelObject,
            definition_index,
            grid_cell: [x, 0],
            position: [x as f32 + 0.5, 0.5],
            angle_radians: 0.0,
            flags: 0,
            difficulty_flags: 7,
            extra: 0,
            size: 0,
        };
        let instances = [instance(2, 0), instance(3, 1)];
        let script = Script::default();
        session.set_scripted_objects(ScriptedObjects::new(
            ObjectStates::new(&instances, &defs).unwrap(),
            defs,
            PresentationManifest::default(),
            &script,
            false,
        ));
        session.actor_definitions.entries.push(ActorDefinition {
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
        session.actors.actors.push(Actor {
            source_index: 7,
            definition_index: 0,
            placement: Monster {
                position: [1152, 128],
                id: 101,
                angle: 0,
                flags: 2,
            },
            position: [4.5, 0.5],
            angle_radians: 0.0,
            health: 100,
        });
        session.ai = EnemyAi::new(&session.actors, Difficulty::Normal);
        assert!(session.set_object_sight(true));
        session.step(SessionInput::default());
        assert_eq!(session.ai.mode(7), Some(Mode::Idle));
        session.objects.as_mut().unwrap().set_enabled([3, 0], false);
        session.step(SessionInput::default());
        assert_eq!(session.ai.mode(7), Some(Mode::Chasing));
    }

    #[test]
    fn pitched_hitscan_respects_model_heights_and_room_planes() {
        let mut straight = height_combat_fixture();
        let hit = straight.step(SessionInput {
            fire: true,
            ..Default::default()
        });
        assert!(matches!(
            hit.shot,
            Some(FireResult::Fired(ShotImpact::Object { cell: [3, 0], .. }))
        ));
        let mut upward = height_combat_fixture();
        let miss = upward.step(SessionInput {
            pitch_delta: 0.3,
            fire: true,
            ..Default::default()
        });
        assert!(matches!(
            miss.shot,
            Some(FireResult::Fired(ShotImpact::Plane {
                plane: combat::ShotPlane::Ceiling,
                ..
            }))
        ));
        assert!((upward.pitch() - 0.3).abs() < 1e-6);
        let mut down = height_combat_fixture();
        let miss = down.step(SessionInput {
            pitch_delta: -0.5,
            fire: true,
            ..Default::default()
        });
        assert!(matches!(
            miss.shot,
            Some(FireResult::Fired(ShotImpact::Plane {
                plane: combat::ShotPlane::Floor,
                ..
            }))
        ));
    }

    #[test]
    fn pitched_projectile_hits_ceiling_and_preserves_height_in_event() {
        let mut session = mission_session("");
        session.game.inventory.weapons[3] = true;
        session.game.inventory.ammo[3] = 1;
        session.game.weapons.0[3].rocket_type = 0;
        session.rockets.0.push(RocketDefinition {
            animation_name: String::new(),
            model_name: "rocket.3o".into(),
            power: 10,
            raw_fields: vec![0, 0, 0, 20, 10],
        });
        let baseline = session.clone();
        session.step(SessionInput {
            pitch_delta: 0.4,
            fire: true,
            select_weapon: Some(3),
            ..Default::default()
        });
        let id = session.projectiles.active()[0].id;
        assert!(session.projectile_height(id).unwrap() > session.vertical.eye());
        assert!((session.projectile_pitch(id).unwrap() - 0.4).abs() < 1e-5);
        let digest = crate::save_format::ContentDigest([17; 32]);
        let snapshot = session.snapshot(digest).unwrap();
        let encoded = crate::session_codec::encode(&snapshot).unwrap();
        let decoded = crate::session_codec::decode(&encoded, digest).unwrap();
        let mut restored = baseline.restore_from_baseline(&decoded, digest).unwrap();
        let mut ceiling = None;
        for _ in 0..60 {
            let left = session.step(SessionInput::default());
            let right = restored.step(SessionInput::default());
            assert_eq!(left.projectile_impacts, right.projectile_impacts);
            assert_eq!(
                left.projectile_impact_heights,
                right.projectile_impact_heights
            );
            if !left.projectile_impacts.is_empty() {
                ceiling = Some((
                    left.projectile_impacts[0].target,
                    left.projectile_impact_heights[0],
                ));
                break;
            }
        }
        let (target, height) = ceiling.unwrap();
        assert_eq!(target, ImpactTarget::Plane);
        assert!((height - 2.0).abs() < 1e-4);
    }

    #[test]
    fn enemy_projectile_tracks_jumping_player_and_reflection_preserves_z() {
        let mut session = mission_session("");
        session.actor_definitions.entries.push(ActorDefinition {
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
        });
        session.actors.actors.push(Actor {
            source_index: 7,
            definition_index: 0,
            placement: Monster {
                position: [256, 128],
                id: 101,
                angle: 0,
                flags: 2,
            },
            position: [1.0, 0.5],
            angle_radians: 0.0,
            health: 100,
        });
        session.ai = EnemyAi::new(&session.actors, Difficulty::Normal);
        session.rockets.0.push(RocketDefinition {
            animation_name: String::new(),
            model_name: "rocket.3o".into(),
            power: 10,
            raw_fields: vec![0, 0, 0, 20, 10],
        });
        session.world.player.position = [3.0, 0.5];
        let baseline = session.clone();
        session
            .vertical
            .step(0.1, true, VerticalBounds::flat_room())
            .unwrap();
        assert!(session.vertical.feet() > 0.0);
        let id = session
            .spawn_enemy_projectile(7, 0, [1.0, 0.5], [3.0, 0.5])
            .unwrap();
        let zspeed = session.projectile_vertical_speeds[&id];
        assert!(zspeed > 0.0);
        let projectile = session.projectiles.take(id).unwrap();
        let horizontal = projectile.velocity[0].hypot(projectile.velocity[1]);
        assert!((horizontal.hypot(zspeed) - 10.0).abs() < 1e-4);
        assert!((zspeed / horizontal - session.vertical.feet() / 2.0).abs() < 1e-5);
        assert!(session
            .projectiles
            .reflect_from_player(projectile, [0.0, 0.5]));
        assert_eq!(session.projectile_vertical_speeds[&id], zspeed);
        let digest = crate::save_format::ContentDigest([31; 32]);
        let snapshot = session.snapshot(digest).unwrap();
        let restored = baseline.restore_from_baseline(&snapshot, digest).unwrap();
        assert_eq!(restored.projectile_vertical_speeds[&id], zspeed);
        assert_eq!(restored.snapshot(digest).unwrap(), snapshot);
    }

    #[test]
    fn pitch_clamps_and_snapshot_rejects_invalid_values() {
        let baseline = mission_session("");
        let mut session = baseline.clone();
        session.step(SessionInput {
            pitch_delta: 100.0,
            ..Default::default()
        });
        assert_eq!(session.pitch(), MAX_PLAYER_PITCH);
        session.step(SessionInput {
            pitch_delta: f32::NAN,
            ..Default::default()
        });
        assert_eq!(session.pitch(), MAX_PLAYER_PITCH);
        let digest = crate::save_format::ContentDigest([22; 32]);
        let snapshot = session.snapshot(digest).unwrap();
        let restored = baseline.restore_from_baseline(&snapshot, digest).unwrap();
        assert_eq!(restored.pitch(), MAX_PLAYER_PITCH);
        let mut invalid = snapshot;
        invalid.pitch = MAX_PLAYER_PITCH + 0.1;
        assert!(baseline.restore_from_baseline(&invalid, digest).is_err());
    }

    #[test]
    fn shot_and_projectile_skip_overhead_object_before_nearest_selection() {
        let mut instant = height_combat_fixture();
        let shot = instant.step(SessionInput {
            fire: true,
            ..Default::default()
        });
        assert!(matches!(
            shot.shot,
            Some(FireResult::Fired(ShotImpact::Object { cell: [3, 0], .. }))
        ));
        let mut physical = height_combat_fixture();
        let id = physical
            .projectiles
            .spawn(
                Launch {
                    owner: Owner::Player,
                    rocket_id: 1,
                    origin: [0.5, 0.5],
                    direction: [1.0, 0.0],
                },
                &physical.rockets,
            )
            .unwrap();
        physical.projectile_heights.insert(id, 0.75);
        let mut hit = None;
        for _ in 0..30 {
            let events = physical.step(SessionInput::default());
            if let Some(impact) = events.projectile_impacts.first() {
                hit = Some(impact.target);
                break;
            }
        }
        assert_eq!(hit, Some(ImpactTarget::Object { cell: [3, 0] }));
        let mut high = height_combat_fixture();
        let id = high
            .projectiles
            .spawn(
                Launch {
                    owner: Owner::Player,
                    rocket_id: 1,
                    origin: [0.5, 0.5],
                    direction: [1.0, 0.0],
                },
                &high.rockets,
            )
            .unwrap();
        high.projectile_heights.insert(id, 2.5);
        let mut hit = None;
        for _ in 0..30 {
            let events = high.step(SessionInput::default());
            if let Some(impact) = events.projectile_impacts.first() {
                hit = Some(impact.target);
                break;
            }
        }
        assert_eq!(hit, Some(ImpactTarget::Object { cell: [2, 0] }));
        let mut unknown = height_combat_fixture();
        unknown.object_heights.as_mut().unwrap().catalog =
            crate::object_heights::ObjectHeightCatalog::new();
        let shot = unknown.step(SessionInput {
            fire: true,
            ..Default::default()
        });
        assert!(matches!(
            shot.shot,
            Some(FireResult::Fired(ShotImpact::Object { cell: [2, 0], .. }))
        ));
    }

    #[test]
    fn object_blast_uses_model_bounds_and_unknown_is_conservative() {
        let mut session = height_combat_fixture();
        let explosion = Explosion {
            owner: Owner::Player,
            center: [2.5, 0.5, 0.75],
            radius: 0.5,
            damage: 100,
        };
        let mut events = SessionEvents::default();
        assert!(session.apply_explosion(
            explosion,
            ExplosionOrigin::Projectile {
                projectile_id: 1,
                rocket_id: 1
            },
            None,
            &mut events,
            &mut Vec::new()
        ));
        assert!(!events
            .explosion_damage
            .iter()
            .any(|item| item.target == TargetId::Object { cell: [2, 0] }));
        let mut high = height_combat_fixture();
        let mut high_events = SessionEvents::default();
        let high_blast = Explosion {
            center: [2.5, 0.5, 2.5],
            ..explosion
        };
        assert!(high.apply_explosion(
            high_blast,
            ExplosionOrigin::Projectile {
                projectile_id: 2,
                rocket_id: 1
            },
            None,
            &mut high_events,
            &mut Vec::new()
        ));
        assert!(high_events
            .explosion_damage
            .iter()
            .any(|item| item.target == TargetId::Object { cell: [2, 0] }));
        let mut unknown = height_combat_fixture();
        unknown.object_heights.as_mut().unwrap().catalog =
            crate::object_heights::ObjectHeightCatalog::new();
        let mut events = SessionEvents::default();
        assert!(unknown.apply_explosion(
            explosion,
            ExplosionOrigin::Projectile {
                projectile_id: 1,
                rocket_id: 1
            },
            None,
            &mut events,
            &mut Vec::new()
        ));
        assert!(events
            .explosion_damage
            .iter()
            .any(|item| item.target == TargetId::Object { cell: [2, 0] }));
    }

    #[test]
    fn enemy_hitscan_skips_overhead_objects() {
        use crate::object_heights::HeightBounds;
        let mut session = height_combat_fixture();
        session
            .object_heights
            .as_mut()
            .unwrap()
            .catalog
            .set(164, HeightBounds::new(2.0, 3.0).unwrap());
        let attack = EnemyAttack {
            source_index: 42,
            damage: 20,
            kind: EnemyAttackKind::Ranged {
                rocket_id: 0,
                instant: true,
                origin: [4.5, 0.5],
                target: [0.5, 0.5],
            },
        };
        let mut events = SessionEvents::default();
        session.resolve_enemy_instant(
            attack,
            0,
            [4.5, 0.5],
            [0.5, 0.5],
            &mut events,
            &mut Vec::new(),
        );
        assert_eq!(
            events.enemy_hits[0].hit.unwrap().target,
            EnemyShotTarget::Player
        );
        assert!(session.game.inventory.health < 100);
    }

    fn addon_boss_fixture(script: &str, raw_id: u16, rocket: usize, reflected: bool) -> Session {
        let mut session = mission_session(script);
        session.actor_definitions.entries.push(ActorDefinition {
            model_name: "wizard.car".into(),
            wall_radius: 20,
            actor_radius: 20,
            speed: 0,
            rotation_speed: 0,
            health: 1000,
            kick: 0,
            rocket: -1,
            separation_limit: 0,
            extra_fields: vec![],
            raw_line: String::new(),
        });
        session.actors.actors.push(Actor {
            source_index: 7,
            definition_index: 0,
            placement: Monster {
                position: [256, 128],
                id: raw_id,
                angle: 0,
                flags: 7,
            },
            position: [1.0, 0.5],
            angle_radians: 0.0,
            health: 1000,
        });
        session.ai = EnemyAi::new(&session.actors, Difficulty::Normal);
        session.rockets = RocketDefinitions(vec![
            RocketDefinition {
                animation_name: String::new(),
                model_name: "bomb.3o".into(),
                power: 10,
                raw_fields: vec![1, 0, 0, 10, 10, 0, 0, 0, 0, 0, 0],
            };
            21
        ]);
        let id = session
            .projectiles
            .spawn(
                Launch {
                    owner: if reflected {
                        Owner::Enemy { source_index: 7 }
                    } else {
                        Owner::Player
                    },
                    rocket_id: rocket,
                    origin: [0.7, 0.5],
                    direction: [1.0, 0.0],
                },
                &session.rockets,
            )
            .unwrap();
        if reflected {
            let shot = session.projectiles.take(id).unwrap();
            assert!(session
                .projectiles
                .reflect_from_player(shot, session.world.player.position));
        }
        session
    }

    #[test]
    fn addon_boss_requires_context_authored_id_and_actual_reflection() {
        let script = "#proc30\nlock\nenddelay 13\n#action\n#end\n";
        for (mapping, raw_id, rocket, reflected, should_kill) in [
            (ActorMapping::AddonToRemaster, 118, 20, true, true),
            (ActorMapping::Identity, 118, 20, true, false),
            (ActorMapping::AddonToRemaster, 119, 20, true, false),
            (ActorMapping::AddonToRemaster, 118, 19, true, false),
            (ActorMapping::AddonToRemaster, 118, 20, false, false),
        ] {
            let mut session = addon_boss_fixture(script, raw_id, rocket, reflected);
            session.set_addon_boss_rule(mapping);
            for _ in 0..5 {
                session.step(SessionInput::default());
            }
            assert_eq!(
                session.actors.actors[0].health == 0,
                should_kill,
                "{mapping:?} {raw_id} {rocket} {reflected}"
            );
            assert_eq!(
                session.mission.as_ref().unwrap().end_remaining().is_some(),
                should_kill
            );
        }
    }

    #[test]
    fn rejected_addon_boss_trigger_retries_without_impact_or_kill() {
        let bad = "#proc30\nlock\nenddelay 13\n#action\nunsupported 1\n#end\n";
        let mut session = addon_boss_fixture(bad, 118, 20, true);
        session.set_addon_boss_rule(ActorMapping::AddonToRemaster);
        for _ in 0..5 {
            let events = session.step(SessionInput::default());
            assert!(events.projectile_impacts.is_empty());
            assert_eq!(session.actors.actors[0].health, 1000);
        }
        assert_eq!(session.projectiles.active().len(), 1);
        assert!(session.mission.as_ref().unwrap().end_remaining().is_none());
        let good = Script::parse("#proc30\nlock\nenddelay 13\n#action\n#end\n");
        session.processes = ProcessRuntime::new(&good, &session.world);
        let events = session.step(SessionInput::default());
        assert_eq!(events.projectile_impacts.len(), 1);
        assert_eq!(session.actors.actors[0].health, 0);
        assert!(session.mission.as_ref().unwrap().end_remaining().is_some());
    }
    #[test]
    fn object_height_filters_teleport_clearance_and_unknown_stays_solid() {
        use crate::object_heights::{HeightBounds, ObjectHeightCatalog};
        use crate::process_runtime::{ProcessCapabilities, ProcessObject};
        use crate::teleport::{self, RearmPolicy, TeleportFailure};
        let mut session = mission_session("");
        let mut cells = [0u8; 4096];
        cells[2 * 64] = 165;
        let roof = ProcessObject {
            cell: [2, 0],
            position: [2.5, 0.5],
            angle_radians: 0.0,
            elevation_raw: 0.0,
            blocking_radius: 0.5,
        };
        session.processes = ProcessRuntime::with_wind(
            &Script::default(),
            &session.world,
            ProcessCapabilities::default(),
            &cells,
            &[roof],
        )
        .unwrap();
        session.set_teleports(
            teleport::parse("#teleports\ntcenter 0 0 2 0 3\n#end\n").unwrap(),
            RearmPolicy::ReferenceContinuous,
        );
        let mut unknown = session.clone();
        assert!(unknown.set_object_heights(ObjectHeightCatalog::new(), cells));
        assert!(matches!(
            unknown.step(SessionInput::default()).teleport_failure,
            Some(TeleportFailure::DestinationBlocked { .. })
        ));
        let mut catalog = ObjectHeightCatalog::new();
        assert!(catalog.set(165, HeightBounds::new(2.0, 4.7).unwrap()));
        let mut invalid = cells;
        invalid[2 * 64] = 0;
        assert!(!session.set_object_heights(catalog.clone(), invalid));
        assert!(session.set_object_heights(catalog, cells));
        let mut snapshot = session.clone();
        assert!(session.step(SessionInput::default()).teleport.is_some());
        assert!(snapshot.step(SessionInput::default()).teleport.is_some());
    }

    #[test]
    fn object_height_lookup_uses_live_changed_type_and_pose_elevation() {
        use crate::object_heights::{HeightBounds, ObjectHeightCatalog};
        use crate::process_runtime::{ProcessCapabilities, ProcessObject};
        let text = "#proc1\n#action\nchange 2 0 166\n#end\n#links\nlink 0 0 1\n#end\n";
        let world = mission_session("").world;
        let mut cells = [0u8; 4096];
        cells[2 * 64] = 165;
        let roof = ProcessObject {
            cell: [2, 0],
            position: [2.5, 0.5],
            angle_radians: 0.0,
            elevation_raw: 0.0,
            blocking_radius: 0.5,
        };
        let mut runtime = ProcessRuntime::with_wind(
            &Script::parse(text),
            &world,
            ProcessCapabilities {
                wall_elevation: false,
                initial_cell_types: Some(&cells),
            },
            &cells,
            &[roof],
        )
        .unwrap();
        let mut catalog = ObjectHeightCatalog::new();
        catalog.set(165, HeightBounds::new(2.0, 4.0).unwrap());
        catalog.set(166, HeightBounds::new(0.0, 1.0).unwrap());
        let heights = SessionObjectHeights {
            catalog,
            initial_raw_cells: Box::new(cells),
        };
        assert!(!object_blocks_height(
            Some(&heights),
            &runtime,
            &roof,
            0.0,
            0.9,
            0.9
        ));
        assert_eq!(
            object_combat_bounds(Some(&heights), &runtime, [2, 0], 165),
            Some((2.0, 4.0))
        );
        let mut moved = roof;
        moved.elevation_raw = -128.0;
        let moved_runtime = ProcessRuntime::with_wind(
            &Script::parse(text),
            &world,
            ProcessCapabilities::default(),
            &cells,
            &[moved],
        )
        .unwrap();
        assert_eq!(
            object_combat_bounds(Some(&heights), &moved_runtime, [2, 0], 165),
            Some((0.0, 2.0))
        );
        assert!(object_blocks_height(
            Some(&heights),
            &runtime,
            &moved,
            0.0,
            0.9,
            0.9
        ));
        assert_eq!(
            runtime.activate_at("link", [0, 0])[0].outcome,
            ProcessOutcome::Accepted
        );
        let mut world = world;
        runtime.tick(&mut world, 1.0 / 60.0, false);
        assert_eq!(runtime.cell_type([2, 0]), Some(166));
        assert_eq!(
            object_combat_bounds(Some(&heights), &runtime, [2, 0], 165),
            Some((0.0, 1.0))
        );
        assert!(object_blocks_height(
            Some(&heights),
            &runtime,
            &roof,
            0.0,
            0.9,
            0.9
        ));
    }

    #[test]
    fn teleport_resets_vertical_and_dispatches_only_destination_floor() {
        use crate::teleport::{self, RearmPolicy};
        let text="#mess1\nDelay 1\nTextA: 0 0 \"Source\"\n#end\n#mess2\nDelay 1\nTextA: 0 0 \"Destination\"\n#end\n#proc1\nOnMessage 1\n#end\n#proc2\nOnMessage 2\n#end\n#links\nfloor 0 0 1\nfloor 2 0 2\n#end\n";
        let mut session = mission_session(text);
        session.set_teleports(
            teleport::parse("#teleports\ntcenter 0 0 2 0 3\n#end\n").unwrap(),
            RearmPolicy::ReferenceContinuous,
        );
        let events = session.step(SessionInput {
            jump: true,
            ..Default::default()
        });
        assert!(events.teleport.is_some());
        assert_eq!(session.world.player.position, [2.5, 0.5]);
        assert!((session.world.player.angle - std::f32::consts::PI).abs() < 1e-5);
        assert_eq!(session.vertical.feet(), 0.0);
        assert_eq!(session.vertical.velocity(), 0.0);
        assert!(session.vertical.grounded());
        assert_eq!(events.mission, vec![MissionEvent::ShowMessage(2)]);
        assert!(session.step(SessionInput::default()).teleport.is_none());
    }

    #[test]
    fn teleport_collects_destination_pickup_and_snapshot_preserves_rearm() {
        use crate::inventory::Pickup;
        use crate::objects::{ObjectDefinition, ObjectInstance, ObjectKind};
        use crate::teleport::{self, RearmPolicy};
        let mut session = mission_session("");
        let def = ObjectDefinition {
            collision_radius: 0,
            shadow: 0,
            bmp_object: 0,
            bmp_z: 0,
            action_code: 128,
            blow_up: 0,
            break_limit: 0,
            ambient_sound: 0,
            break_sound: 0,
            model_name: "armor.3o".into(),
            animation_name: None,
        };
        let obj = ObjectInstance {
            kind: ObjectKind::Item,
            definition_index: 0,
            grid_cell: [2, 0],
            position: [2.5, 0.5],
            angle_radians: 0.0,
            flags: 0,
            difficulty_flags: 2,
            extra: 0,
            size: 0,
        };
        session.game = GameInventory::new(
            session.game.weapons.clone(),
            &[obj],
            &ObjectDefinitions {
                items: vec![def],
                level_objects: vec![],
            },
            Difficulty::Normal,
        );
        // The second source is at the destination; the opt-in policy gates it.
        session.set_teleports(
            teleport::parse("#teleports\ntcenter 0 0 2 0 3\ntcenter 2 0 3 0 3\n#end\n").unwrap(),
            RearmPolicy::AfterLeavingSources,
        );
        let events = session.step(SessionInput::default());
        assert!(events.teleport.is_some());
        assert!(events
            .pickups
            .iter()
            .any(|pickup| pickup.pickup == Pickup::Armor));
        let mut restart = session.clone();
        assert!(session.step(SessionInput::default()).teleport.is_none());
        assert!(restart.step(SessionInput::default()).teleport.is_none());
        assert_eq!(session.world.player.position, [2.5, 0.5]);
    }

    #[test]
    fn teleport_destination_collision_fails_and_extra_field_is_ignored() {
        use crate::teleport::{self, RearmPolicy, TeleportFailure};
        use crate::world::Wall;
        let mut session = mission_session("");
        session.world.walls.push(Wall {
            a: [2.5, 0.0],
            b: [2.5, 1.0],
            texture: 1,
            blocks_movement: true,
            cell: None,
        });
        session.base_wall_blocking.push(true);
        session.wall_elevations.push(0.0);
        session.set_teleports(
            teleport::parse("#teleports\ntcenter 0 0 2 0 3\n#end\n").unwrap(),
            RearmPolicy::AfterLeavingSources,
        );
        let before = session.clone();
        let e = session.step(SessionInput::default());
        assert_eq!(
            e.teleport_failure,
            Some(TeleportFailure::DestinationBlocked { index: 0 })
        );
        assert_eq!(session.world.player.position, before.world.player.position);
        session.world.walls[0].blocks_movement = false;
        session.base_wall_blocking[0] = false;
        assert!(
            session.step(SessionInput::default()).teleport.is_some(),
            "blocked attempt must not consume trigger"
        );
        let mut unknown = mission_session("");
        unknown.set_teleports(
            teleport::parse("#teleports\ntcenter 0 0 2 0 3 2\n#end\n").unwrap(),
            RearmPolicy::ReferenceContinuous,
        );
        let e = unknown.step(SessionInput::default());
        assert!(e.teleport_failure.is_none());
        assert!(e.teleport.is_some());
        assert_eq!(unknown.world.player.position, [2.5, 0.5]);
    }

    #[test]
    fn invisibility_pickup_suppresses_idle_acquisition_until_expiry() {
        use crate::inventory::Pickup;
        use crate::objects::{ObjectDefinition, ObjectInstance, ObjectKind};
        let mut session = mission_session("");
        let def = ObjectDefinition {
            collision_radius: 0,
            shadow: 0,
            bmp_object: 0,
            bmp_z: 0,
            action_code: 131,
            blow_up: 0,
            break_limit: 0,
            ambient_sound: 0,
            break_sound: 0,
            model_name: "invis.3o".into(),
            animation_name: None,
        };
        let placement = ObjectInstance {
            kind: ObjectKind::Item,
            definition_index: 0,
            grid_cell: [0, 0],
            position: session.world.player.position,
            angle_radians: 0.0,
            flags: 0,
            difficulty_flags: 2,
            extra: 0,
            size: 0,
        };
        let definitions = ObjectDefinitions {
            items: vec![def],
            level_objects: vec![],
        };
        session.game = GameInventory::new(
            session.game.weapons.clone(),
            &[placement],
            &definitions,
            Difficulty::Normal,
        );
        session.actor_definitions.entries.push(ActorDefinition {
            model_name: "test.car".into(),
            wall_radius: 64,
            actor_radius: 64,
            speed: 30,
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
                position: [512, 128],
                id: 101,
                angle: 0,
                flags: 2,
            },
            position: [2.0, 0.5],
            angle_radians: 0.0,
            health: 30,
        });
        session.ai = EnemyAi::new(&session.actors, Difficulty::Normal);
        let e = session.step(SessionInput::default());
        assert!(e.pickups.iter().any(|p| p.pickup == Pickup::Invisibility));
        assert_eq!(session.game.inventory.invisibility_ticks_remaining, 2400);
        assert_eq!(session.ai.mode(0), Some(Mode::Idle));
        let mut snapshot = session.clone();
        session.step(SessionInput::default());
        assert_eq!(session.game.inventory.invisibility_ticks_remaining, 2399);
        assert_eq!(snapshot.game.inventory.invisibility_ticks_remaining, 2400);
        snapshot.game.inventory.invisibility_ticks_remaining = 1;
        snapshot.step(SessionInput::default());
        assert_eq!(snapshot.game.inventory.invisibility_ticks_remaining, 0);
        assert_eq!(snapshot.ai.mode(0), Some(Mode::Chasing));
        assert!(session.ai.alert(0, session.world.player.position));
        session.step(SessionInput::default());
        assert_eq!(session.ai.mode(0), Some(Mode::Chasing));
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
            cell: Some([1, 0]),
        });
        session.base_wall_blocking.push(true);
        session.wall_elevations.push(0.0);
        session.processes = ProcessRuntime::new(&Script::default(), &session.world);
        session
            .wind
            .activate(
                1,
                &[WindZone {
                    min_cell: [0, 0],
                    max_cell: [5, 5],
                    direction: [-64, 0],
                    actor_only: false,
                }],
            )
            .unwrap();
        let mut dynamic = session.clone();
        dynamic.processes = ProcessRuntime::new(
            &Script::parse("#proc1\n#action\nmove 1 0 256 0\n#end\n"),
            &dynamic.world,
        );
        dynamic.set_wall_collision_mode(WallCollisionMode::AuthoredSides);
        for _ in 0..3 {
            dynamic.step(SessionInput::default());
        }
        assert!(dynamic.world.player.position[0] > 1.179);
        let mut unknown = session.clone();
        unknown.processes = ProcessRuntime::new(
            &Script::parse("#proc1\n#action\nunknown 1 0\n#end\n"),
            &unknown.world,
        );
        unknown.set_wall_collision_mode(WallCollisionMode::AuthoredSides);
        for _ in 0..3 {
            unknown.step(SessionInput::default());
        }
        assert!(unknown.world.player.position[0] > 1.179);
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
                    actor_only: false,
                }],
            )
            .unwrap();
        for _ in 0..3 {
            restarted.step(SessionInput::default());
        }
        assert!(restarted.world.player.position[0] < 0.821);
    }

    #[test]
    fn enemy_chase_uses_authored_sides_only_for_static_walls() {
        use crate::world::Wall;
        fn fixture(script: &str) -> Session {
            let mut session = mission_session("");
            session.world.player.position = [0.1, 0.5];
            session.world.walls.push(Wall {
                a: [1.0, 0.0],
                b: [1.0, 1.0],
                texture: 1,
                blocks_movement: true,
                cell: Some([1, 0]),
            });
            session.base_wall_blocking.push(true);
            session.wall_elevations.push(0.0);
            session.processes = ProcessRuntime::new(&Script::parse(script), &session.world);
            session.shot_blocking[1] = false;
            session.actor_definitions.entries.push(ActorDefinition {
                model_name: "test.car".into(),
                wall_radius: 64,
                actor_radius: 64,
                speed: 30,
                rotation_speed: 180,
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
                    position: [512, 128],
                    id: 101,
                    angle: 0,
                    flags: 2,
                },
                position: [2.0, 0.5],
                angle_radians: 0.0,
                health: 30,
            });
            session.ai = EnemyAi::new(&session.actors, Difficulty::Normal);
            session
        }
        let mut normal = fixture("");
        let mut authored = normal.clone();
        authored.set_wall_collision_mode(WallCollisionMode::AuthoredSides);
        let mut dynamic = fixture("#proc1\n#action\nmove 1 0 256 0\n#end\n");
        dynamic.set_wall_collision_mode(WallCollisionMode::AuthoredSides);
        for _ in 0..150 {
            normal.step(SessionInput::default());
            authored.step(SessionInput::default());
            dynamic.step(SessionInput::default());
        }
        assert!(normal.actors.actors[0].position[0] > 1.24);
        assert!(authored.actors.actors[0].position[0] < 1.0);
        assert!(dynamic.actors.actors[0].position[0] > 1.24);
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
                    actor_only: true,
                }],
            )
            .unwrap();
        for _ in 0..10 {
            session.step(SessionInput::default());
        }
        assert_eq!(session.world.player.position, [5.5, 5.5]);
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
            cell: Some([1, 0]),
        });
        session.base_wall_blocking.push(true);
        session.wall_elevations.push(0.0);
        session.processes = ProcessRuntime::new(&Script::default(), &session.world);
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
                    actor_only: false,
                }],
            )
            .unwrap();
        let mut dynamic = session.clone();
        dynamic.processes = ProcessRuntime::new(
            &Script::parse("#proc1\n#action\nmove 1 0 256 0\n#end\n"),
            &dynamic.world,
        );
        dynamic.set_wall_collision_mode(WallCollisionMode::AuthoredSides);
        for _ in 0..3 {
            dynamic.step(SessionInput::default());
        }
        assert!(dynamic.actors.actors[0].position[0] > 1.249);
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
            animation_name: String::new(),
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
            animation_name: String::new(),
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
            collision_radius: 32,
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
            animation_name: String::new(),
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
            animation_name: String::new(),
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
    fn destruction_deduplicates_only_first_matching_script_transition() {
        let base = ObjectDestructionEvent {
            cell: [1, 2],
            raw_type: 163,
            position: [1.5, 2.5],
            sound_id: 5,
            cause: ObjectDestructionCause::PlayerHitscan,
        };
        let script = ObjectDestructionEvent {
            cause: ObjectDestructionCause::Script { process_id: 1 },
            ..base
        };
        let replacement = ObjectDestructionEvent {
            raw_type: 164,
            ..script
        };
        let mut events = vec![base, script, replacement, script];
        deduplicate_script_destruction(&mut events, 0);
        assert_eq!(events, vec![base, replacement, script]);
    }
    #[test]
    fn staged_destroy_replaces_object_and_failed_preflight_restores_hit() {
        use crate::objects::{ObjectDefinition, ObjectInstance, ObjectKind};
        use crate::process_runtime::{ProcessCapabilities, ProcessObject};
        let text = "#proc1\n#action\nexplode 3 0\nquake 2 0 3 1 10\n#end\n#proc2\n#action\nplayani 99\nunlock 3\n#end\n#proc3\nLock\nEndDelay 1\n#end\n#proc4\nLock\nEndDelay 1\n#end\n#links\nshoot 1 0 2\ndestroy 1 0 1\ndestroy 1 0 4\nlink 5 0 3\nlink 6 0 4\n#end\n";
        let script = Script::parse(text);
        let mut session = mission_session(text);
        session.rockets = RocketDefinitions(vec![RocketDefinition {
            animation_name: String::new(),
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
            break_sound: if name == "heart.3o" { 23 } else { 41 },
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
            animation_name: String::new(),
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
                assert!(events.object_destructions.is_empty());
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
        assert!(blocked.object_destructions.is_empty());
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
        let mut projectile_success = session.clone();
        projectile_success.game.inventory.weapons[3] = true;
        projectile_success.game.inventory.ammo[3] = 1;
        projectile_success.game.weapons.0[3].rocket_type = 1;
        projectile_success.rockets.0.push(RocketDefinition {
            animation_name: String::new(),
            model_name: "disk.3o".into(),
            power: 10,
            raw_fields: vec![0, 0, 0, 20, 10],
        });
        projectile_success.step(SessionInput {
            fire: true,
            select_weapon: Some(3),
            ..Default::default()
        });
        let mut committed = Vec::new();
        for _ in 0..30 {
            committed.extend(
                projectile_success
                    .step(SessionInput::default())
                    .object_destructions,
            );
        }
        assert_eq!(committed.len(), 2);
        assert_eq!(committed[0].cell, [1, 0]);
        assert_eq!(committed[0].raw_type, 163);
        assert_eq!(committed[0].sound_id, 23);
        assert!(matches!(
            committed[0].cause,
            ObjectDestructionCause::Projectile {
                owner: Owner::Player,
                ..
            }
        ));
        assert_eq!(
            committed[1].cause,
            ObjectDestructionCause::Script { process_id: 1 }
        );
        let result = session.step(SessionInput {
            fire: true,
            ..Default::default()
        });
        assert_eq!(result.object_hits, vec![([1, 0], true)]);
        assert_eq!(
            result.object_destructions,
            vec![
                ObjectDestructionEvent {
                    cell: [1, 0],
                    raw_type: 163,
                    position: [1.5, 0.5],
                    sound_id: 23,
                    cause: ObjectDestructionCause::PlayerHitscan
                },
                ObjectDestructionEvent {
                    cell: [3, 0],
                    raw_type: 164,
                    position: [3.5, 0.5],
                    sound_id: 41,
                    cause: ObjectDestructionCause::Script { process_id: 1 }
                },
            ]
        );
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
        for pickup_code in [142, 132, 133] {
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
                items: vec![def("pickup.3o", pickup_code)],
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
            session.set_protective_powerups_enabled(true);
            let rejected = session.step(SessionInput::default());
            assert!(rejected.pickups.is_empty());
            assert!(!session.game.inventory.keys[1]);
            assert_eq!(
                session.game.protective_powerups.reflector_ticks_remaining,
                0
            );
            assert_eq!(session.game.protective_powerups.chojin_ticks_remaining, 0);
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
            assert_eq!(session.game.inventory.keys[1], pickup_code == 142);
            assert_eq!(
                session.game.protective_powerups.reflector_ticks_remaining,
                if pickup_code == 132 { 2400 } else { 0 }
            );
            assert_eq!(
                session.game.protective_powerups.chojin_ticks_remaining,
                if pickup_code == 133 { 1800 } else { 0 }
            );
            assert_eq!(session.game.pickups.is_visible([0, 0]), Some(false));
            assert_eq!(session.processes.cell_type([3, 0]), Some(164));
            assert_eq!(accepted.object_transitions.len(), 1);
        }
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
            animation_name: String::new(),
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
                animation_name: String::new(),
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
            break_sound: 47,
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
        assert!(mine_events.object_destructions.is_empty());
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
        assert_eq!(
            mine_events.object_destructions,
            vec![ObjectDestructionEvent {
                cell: [1, 0],
                raw_type: 163,
                position: [1.5, 0.5],
                sound_id: 47,
                cause: ObjectDestructionCause::Explosion {
                    origin: ExplosionOrigin::Mine { mine_id: 1 },
                    owner: Owner::Player
                }
            }]
        );
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
        assert!(events.object_destructions.is_empty());
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
        assert_eq!(
            events.object_destructions,
            vec![ObjectDestructionEvent {
                cell: [1, 0],
                raw_type: 163,
                position: [1.5, 0.5],
                sound_id: 47,
                cause: ObjectDestructionCause::Explosion {
                    origin: ExplosionOrigin::Projectile {
                        projectile_id: 0,
                        rocket_id: 0
                    },
                    owner: Owner::Player
                }
            }]
        );
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
            animation_name: String::new(),
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
            animation_name: String::new(),
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
            animation_name: String::new(),
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
            animation_name: String::new(),
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
            animation_name: String::new(),
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
            break_sound: 19,
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
        assert!(events.object_destructions.is_empty());
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
            events.object_destructions,
            vec![ObjectDestructionEvent {
                cell: [2, 0],
                raw_type: 163,
                position: [2.5, 0.5],
                sound_id: 19,
                cause: ObjectDestructionCause::EnemyHitscan { source_index: 7 }
            }]
        );
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
    #[test]
    fn armor_absorbs_full_hit_and_uses_quarter_health_rounding() {
        for (health, armor, amount, expected_health, expected_armor, expected_damage) in [
            (100, 0, 20, 80, 0, 20),
            (100, 100, 20, 95, 80, 5),
            (100, 5, 20, 84, 0, 16),
            (100, 3, 3, 100, 0, 0),
            (100, 7, 7, 99, 0, 1),
            (2, 8, 20, 0, 0, 14),
            (0, 50, 10, 0, 50, 0),
        ] {
            let mut session = mission_session("");
            session.game.inventory.health = health;
            session.game.inventory.armor = armor;
            assert_eq!(session.apply_player_damage(amount), expected_damage);
            assert_eq!(
                (session.game.inventory.health, session.game.inventory.armor),
                (expected_health, expected_armor)
            );
        }
    }
    #[test]
    fn armor_rules_apply_to_enemy_hitscan_projectiles_and_hazards() {
        use crate::process_runtime::ProcessCapabilities;
        let mut instant = mission_session("");
        instant.game.inventory.armor = 100;
        let attack = EnemyAttack {
            source_index: 7,
            damage: 20,
            kind: EnemyAttackKind::Ranged {
                rocket_id: 0,
                instant: true,
                origin: [2.5, 0.5],
                target: [0.5, 0.5],
            },
        };
        let mut events = SessionEvents::default();
        let mut attempts = Vec::new();
        instant.resolve_enemy_instant(
            attack,
            0,
            [2.5, 0.5],
            [0.5, 0.5],
            &mut events,
            &mut attempts,
        );
        assert_eq!(
            (
                instant.game.inventory.health,
                instant.game.inventory.armor,
                events.damage
            ),
            (95, 80, 5)
        );
        let mut projectile = mission_session("");
        projectile.game.inventory.armor = 100;
        projectile.rockets = RocketDefinitions(vec![RocketDefinition {
            animation_name: String::new(),
            model_name: "arrow.3o".into(),
            power: 20,
            raw_fields: vec![1, 0, 120, 0, 20, 0],
        }]);
        projectile
            .projectiles
            .spawn(
                Launch {
                    owner: Owner::Enemy { source_index: 7 },
                    rocket_id: 0,
                    origin: [1.5, 0.5],
                    direction: [-1.0, 0.0],
                },
                &projectile.rockets,
            )
            .unwrap();
        let mut seen = 0;
        for _ in 0..15 {
            seen += u32::from(projectile.step(SessionInput::default()).damage);
        }
        assert_eq!(
            (
                projectile.game.inventory.health,
                projectile.game.inventory.armor,
                seen
            ),
            (95, 80, 5)
        );
        let text =
            "#proc1\n#action\nnonstop\ndeath 0 0 0 0 0 128 30\n#end\n#links\nfloor 0 0 1\n#end\n";
        let mut hazard = mission_session(text);
        hazard.game.inventory.armor = 100;
        hazard.processes = ProcessRuntime::with_hazards(
            &Script::parse(text),
            &hazard.world,
            ProcessCapabilities::default(),
            &[0; 4096],
        );
        let mut seen = 0;
        for _ in 0..20 {
            seen += u32::from(hazard.step(SessionInput::default()).damage);
        }
        assert_eq!(
            (
                hazard.game.inventory.health,
                hazard.game.inventory.armor,
                seen
            ),
            (93, 70, 7)
        );
    }
    #[test]
    fn simultaneous_mines_do_not_damage_or_consume_armor_after_death() {
        let mut session = mission_session("");
        session.set_mines_enabled(true);
        session.game.inventory.health = 10;
        session.game.inventory.armor = 200;
        session.mines.plant(Owner::Player, [0.5, 0.5, 0.0]).unwrap();
        session.mines.plant(Owner::Player, [0.5, 0.5, 0.0]).unwrap();
        for _ in 1..explosives::MINE_ARM_TICKS {
            session.step(SessionInput::default());
        }
        let events = session.step(SessionInput::default());
        assert_eq!(events.explosions.len(), 2);
        assert_eq!(events.damage, 35);
        assert_eq!(
            (session.game.inventory.health, session.game.inventory.armor),
            (0, 60)
        );
        assert_eq!(
            events
                .explosion_damage
                .iter()
                .filter(|event| event.target == TargetId::Player)
                .count(),
            1
        );
    }
    #[test]
    fn reflector_redirects_and_retraces_enemy_projectile_in_same_tick() {
        let mut session = mission_session("");
        session.actor_definitions.entries.push(ActorDefinition {
            model_name: "a.car".into(),
            wall_radius: 0,
            actor_radius: 0,
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
            position: [195, 128],
            id: 101,
            angle: 0,
            flags: 2,
        };
        session.actors.actors.push(Actor {
            source_index: 7,
            definition_index: 0,
            placement,
            position: [0.76, 0.5],
            angle_radians: 0.0,
            health: 100,
        });
        session.ai = EnemyAi::new(&session.actors, Difficulty::Normal);
        session.rockets = RocketDefinitions(vec![RocketDefinition {
            animation_name: String::new(),
            model_name: "arrow.3o".into(),
            power: 20,
            raw_fields: vec![1, 0, 120, 0, 20, 0],
        }]);
        session.set_protective_powerups_enabled(true);
        session.game.protective_powerups.reflector_ticks_remaining = 100;
        session
            .projectiles
            .spawn(
                Launch {
                    owner: Owner::Enemy { source_index: 7 },
                    rocket_id: 0,
                    origin: [0.8, 0.5],
                    direction: [-1.0, 0.0],
                },
                &session.rockets,
            )
            .unwrap();
        let events = session.step(SessionInput::default());
        assert_eq!(session.game.inventory.health, 100);
        assert_eq!(events.projectile_reflections.len(), 1);
        assert_eq!(
            events.projectile_reflections[0].previous_owner,
            Owner::Enemy { source_index: 7 }
        );
        assert!(events
            .projectile_impacts
            .iter()
            .all(|impact| impact.target != ImpactTarget::Player));
        assert!(events.projectile_impacts.iter().any(|impact| impact.target
            == ImpactTarget::Actor { source_index: 7 }
            && impact.owner == Owner::Player));
        assert_eq!(session.actors.actors[0].health, 80);
    }
    #[test]
    fn chojin_preserves_health_and_armor_across_damage_and_expiry() {
        let mut session = mission_session("");
        session.set_protective_powerups_enabled(true);
        session.game.inventory.armor = 50;
        session.game.protective_powerups.chojin_ticks_remaining = 2;
        assert_eq!(session.apply_player_damage(20), 0);
        assert_eq!(
            (session.game.inventory.health, session.game.inventory.armor),
            (100, 50)
        );
        let mut events = SessionEvents::default();
        let mut attempts = Vec::new();
        let attack = EnemyAttack {
            source_index: 7,
            damage: 20,
            kind: EnemyAttackKind::Ranged {
                rocket_id: 0,
                instant: true,
                origin: [2.5, 0.5],
                target: [0.5, 0.5],
            },
        };
        session.resolve_enemy_instant(
            attack,
            0,
            [2.5, 0.5],
            [0.5, 0.5],
            &mut events,
            &mut attempts,
        );
        assert_eq!(events.damage, 0);
        let explosion = Explosion {
            owner: Owner::Player,
            center: [0.5, 0.5, 0.5],
            radius: 2.0,
            damage: 100,
        };
        assert!(session.apply_explosion(
            explosion,
            ExplosionOrigin::Mine { mine_id: 0 },
            None,
            &mut events,
            &mut attempts
        ));
        assert_eq!(
            (
                session.game.inventory.health,
                session.game.inventory.armor,
                events.damage
            ),
            (100, 50, 0)
        );
        session.step(SessionInput::default());
        assert_eq!(session.game.protective_powerups.chojin_ticks_remaining, 1);
        session.step(SessionInput::default());
        assert_eq!(session.game.protective_powerups.chojin_ticks_remaining, 0);
        assert_eq!(session.apply_player_damage(20), 5);
        assert_eq!(
            (session.game.inventory.health, session.game.inventory.armor),
            (95, 30)
        );
    }
    #[test]
    fn reflector_suppresses_player_contact_explosion_but_not_instant_hits() {
        let mut session = mission_session("");
        session.set_protective_powerups_enabled(true);
        session.game.protective_powerups.reflector_ticks_remaining = 100;
        session.set_explosions_enabled(true);
        session.rockets = RocketDefinitions(vec![RocketDefinition {
            animation_name: String::new(),
            model_name: "grenade.3o".into(),
            power: 20,
            raw_fields: vec![2, 0, 424, 0, 20, 0],
        }]);
        session
            .projectiles
            .spawn(
                Launch {
                    owner: Owner::Enemy { source_index: 7 },
                    rocket_id: 0,
                    origin: [0.8, 0.5],
                    direction: [-1.0, 0.0],
                },
                &session.rockets,
            )
            .unwrap();
        let events = session.step(SessionInput::default());
        assert_eq!(events.projectile_reflections.len(), 1);
        assert!(events.projectile_impacts.is_empty());
        assert!(events.explosions.is_empty());
        assert_eq!(session.game.inventory.health, 100);
        assert_eq!(session.projectiles.active()[0].owner, Owner::Player);
        let attack = EnemyAttack {
            source_index: 7,
            damage: 20,
            kind: EnemyAttackKind::Ranged {
                rocket_id: 1,
                instant: true,
                origin: [2.5, 0.5],
                target: [0.5, 0.5],
            },
        };
        let mut events = SessionEvents::default();
        let mut attempts = Vec::new();
        session.resolve_enemy_instant(
            attack,
            1,
            [2.5, 0.5],
            [0.5, 0.5],
            &mut events,
            &mut attempts,
        );
        assert_eq!(events.damage, 20);
        assert_eq!(session.game.inventory.health, 80);
        let blast = Explosion {
            owner: Owner::Enemy { source_index: 7 },
            center: [0.5, 0.5, 0.5],
            radius: 1.0,
            damage: 20,
        };
        assert!(session.apply_explosion(
            blast,
            ExplosionOrigin::Mine { mine_id: 9 },
            None,
            &mut events,
            &mut attempts
        ));
        assert_eq!(
            session.game.inventory.health, 60,
            "reflector does not prevent nearby blast damage"
        );
    }
    #[test]
    fn disabled_protection_items_remain_available_and_enabled_collection_is_authoritative() {
        use crate::objects::{ObjectDefinition, ObjectInstance, ObjectKind};
        let mut session = mission_session("");
        let definition = |code| ObjectDefinition {
            collision_radius: 0,
            shadow: 0,
            bmp_object: 0,
            bmp_z: 0,
            action_code: code,
            blow_up: 0,
            break_limit: 0,
            ambient_sound: 0,
            break_sound: 0,
            model_name: "power.3o".into(),
            animation_name: None,
        };
        let defs = ObjectDefinitions {
            items: vec![definition(132), definition(133)],
            level_objects: vec![],
        };
        let placement = |index| ObjectInstance {
            kind: ObjectKind::Item,
            definition_index: index,
            grid_cell: [index, 0],
            position: [0.5, 0.5],
            angle_radians: 0.0,
            flags: 0,
            difficulty_flags: 2,
            extra: 0,
            size: 0,
        };
        session.game = GameInventory::new(
            session.game.weapons.clone(),
            &[placement(0), placement(1)],
            &defs,
            Difficulty::Normal,
        );
        assert!(session.step(SessionInput::default()).pickups.is_empty());
        assert_eq!(session.game.pickups.is_visible([0, 0]), Some(true));
        assert_eq!(session.game.pickups.is_visible([1, 0]), Some(true));
        session.set_protective_powerups_enabled(true);
        let events = session.step(SessionInput::default());
        assert_eq!(events.pickups.len(), 2);
        assert!(events
            .pickups
            .iter()
            .any(|event| event.pickup == crate::inventory::Pickup::Reflector));
        assert!(events
            .pickups
            .iter()
            .any(|event| event.pickup == crate::inventory::Pickup::Chojin));
        assert_eq!(
            (
                session.game.protective_powerups.reflector_ticks_remaining,
                session.game.protective_powerups.chojin_ticks_remaining
            ),
            (2400, 1800)
        );
        let snapshot = session.clone();
        session.step(SessionInput::default());
        assert_eq!(
            session.game.protective_powerups.reflector_ticks_remaining,
            2399
        );
        assert_eq!(
            snapshot.game.protective_powerups.reflector_ticks_remaining,
            2400
        );
        session.set_protective_powerups_enabled(false);
        assert_eq!(
            (
                session.game.protective_powerups.reflector_ticks_remaining,
                session.game.protective_powerups.chojin_ticks_remaining
            ),
            (0, 0)
        );
    }
    #[test]
    fn reflector_expiry_precedes_physical_contact_on_same_tick() {
        let mut session = mission_session("");
        session.set_protective_powerups_enabled(true);
        session.game.protective_powerups.reflector_ticks_remaining = 1;
        session.rockets = RocketDefinitions(vec![RocketDefinition {
            animation_name: String::new(),
            model_name: "arrow.3o".into(),
            power: 20,
            raw_fields: vec![1, 0, 120, 0, 20, 0],
        }]);
        session
            .projectiles
            .spawn(
                Launch {
                    owner: Owner::Enemy { source_index: 7 },
                    rocket_id: 0,
                    origin: [0.8, 0.5],
                    direction: [-1.0, 0.0],
                },
                &session.rockets,
            )
            .unwrap();
        let events = session.step(SessionInput::default());
        assert!(events.projectile_reflections.is_empty());
        assert!(events
            .projectile_impacts
            .iter()
            .any(|impact| impact.target == ImpactTarget::Player));
        assert_eq!(events.damage, 20);
        assert_eq!(session.game.inventory.health, 80);
    }
}
