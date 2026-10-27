//! Experimental subset of level process behavior, independent of rendering.
//!
//! Supported: floor and nearby use links, wall `move` / `rotate`, `StartDelay`, `BackWait`,
//! `Speed`, `waitout`, and initially locked/key-gated processes. Processes with other
//! state-changing actions are disabled in full. Movement timing, use range, and
//! behavior in the presence of blockers are placeholders, not verified
//! against the original game.
use crate::process::{Process, Script};
use crate::world::World;
use std::collections::BTreeMap;

const USE_RANGE: f32 = 1.25;
const MAP_UNITS_PER_TILE: f32 = 256.0;

#[derive(Clone, Debug)]
struct Motion {
    wall: usize,
    kind: MotionKind,
}
#[derive(Clone, Debug)]
enum MotionKind {
    Translate([f32; 2]),
    Rotate {
        pivot: [f32; 2],
        radians: f32,
    },
    /// Raw process height units; projection and collision are handled by host.
    Vertical(i32),
}
impl Motion {
    fn transform(&self, point: [f32; 2], progress_delta: f32) -> [f32; 2] {
        match self.kind {
            MotionKind::Translate(displacement) => [
                point[0] + displacement[0] * progress_delta,
                point[1] + displacement[1] * progress_delta,
            ],
            MotionKind::Vertical(_) => point,
            MotionKind::Rotate { pivot, radians } => {
                let (sin, cos) = (radians * progress_delta).sin_cos();
                let x = point[0] - pivot[0];
                let y = point[1] - pivot[1];
                [pivot[0] + x * cos - y * sin, pivot[1] + x * sin + y * cos]
            }
        }
    }
    /// Upper bound on endpoint travel for the whole motion, used for timing
    /// and sampling during obstruction checks.
    fn travel_bound(&self, wall: &crate::world::Wall) -> f32 {
        match self.kind {
            MotionKind::Translate(d) => d[0].hypot(d[1]),
            MotionKind::Vertical(raw) => raw.unsigned_abs() as f32 / MAP_UNITS_PER_TILE,
            MotionKind::Rotate { pivot, radians } => {
                let radius = [wall.a, wall.b]
                    .iter()
                    .map(|p| (p[0] - pivot[0]).hypot(p[1] - pivot[1]))
                    .fold(0.0f32, f32::max);
                radius * radians.abs()
            }
        }
    }
}

fn distance_to_segment(point: [f32; 2], a: [f32; 2], b: [f32; 2]) -> f32 {
    let dx = b[0] - a[0];
    let dy = b[1] - a[1];
    let length_sq = dx * dx + dy * dy;
    let along = if length_sq < 1e-12 {
        0.0
    } else {
        (((point[0] - a[0]) * dx + (point[1] - a[1]) * dy) / length_sq).clamp(0.0, 1.0)
    };
    (point[0] - (a[0] + along * dx)).hypot(point[1] - (a[1] + along * dy))
}

/// Check the swept wall against the player's preview collision circle. A short
/// travel sampling interval bounds tunnelling; excessive deltas fail closed.
fn obstructed(motions: &[Motion], world: &World, change: f32) -> bool {
    const PLAYER_RADIUS: f32 = 0.18;
    const SAMPLE_STEP: f32 = 0.04;
    const MAX_SAMPLES: usize = 128;
    if !world.player.position.iter().all(|value| value.is_finite()) {
        return true;
    }
    for motion in motions {
        let Some(wall) = world.walls.get(motion.wall) else {
            return true;
        };
        if !wall.blocks_movement || matches!(motion.kind, MotionKind::Vertical(_)) {
            continue;
        }
        let travel = motion.travel_bound(wall) * change.abs();
        if !travel.is_finite() || travel > SAMPLE_STEP * MAX_SAMPLES as f32 {
            return true;
        }
        let count = ((travel / SAMPLE_STEP).ceil() as usize).max(1);
        let mut previous = distance_to_segment(world.player.position, wall.a, wall.b);
        for sample in 1..=count {
            let fraction = change * sample as f32 / count as f32;
            let distance = distance_to_segment(
                world.player.position,
                motion.transform(wall.a, fraction),
                motion.transform(wall.b, fraction),
            );
            // Permit a wall already overlapping the player to move away.
            if distance < PLAYER_RADIUS && distance < previous - 1e-5 {
                return true;
            }
            previous = distance;
        }
    }
    false
}

/// Initial authoritative pose and conservative collision radius of one source
/// map object. Position and radius use map tiles; elevation uses raw script
/// units and orientation uses radians. Cell is a stable map-grid identifier.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ProcessObject {
    pub cell: [u8; 2],
    pub position: [f32; 2],
    pub angle_radians: f32,
    pub elevation_raw: f32,
    /// Zero denotes an object that does not block the player.
    pub blocking_radius: f32,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessObjectError {
    Invalid,
    DuplicateCell,
}
#[derive(Debug, Clone)]
struct ObjectMotion {
    cell: [u8; 2],
    raw_type: u8,
    kind: MotionKind,
}
impl ObjectMotion {
    fn proxy(&self) -> Motion {
        Motion {
            wall: 0,
            kind: self.kind.clone(),
        }
    }
    fn travel_bound(&self, pose: &ProcessObject) -> f32 {
        match self.kind {
            MotionKind::Translate(d) => d[0].hypot(d[1]),
            MotionKind::Vertical(raw) => raw.unsigned_abs() as f32 / MAP_UNITS_PER_TILE,
            MotionKind::Rotate { pivot, radians } => {
                let orbit = (pose.position[0] - pivot[0]).hypot(pose.position[1] - pivot[1]);
                (orbit + pose.blocking_radius.max(0.125)) * radians.abs()
            }
        }
    }
}

/// Check moving object circles against the player without pushing or crushing.
fn objects_obstructed(
    motions: &[ObjectMotion],
    world: &World,
    objects: &BTreeMap<[u8; 2], ProcessObject>,
    change: f32,
) -> bool {
    const PLAYER_RADIUS: f32 = 0.18;
    const SAMPLE_STEP: f32 = 0.04;
    const MAX_SAMPLES: usize = 128;
    for motion in motions {
        let Some(object) = objects.get(&motion.cell) else {
            return true;
        };
        if object.blocking_radius <= 0.0 || matches!(motion.kind, MotionKind::Vertical(_)) {
            continue;
        }
        let travel = motion.travel_bound(object) * change.abs();
        if !travel.is_finite() || travel > SAMPLE_STEP * MAX_SAMPLES as f32 {
            return true;
        }
        let count = ((travel / SAMPLE_STEP).ceil() as usize).max(1);
        let radius = PLAYER_RADIUS + object.blocking_radius;
        let mut previous = (object.position[0] - world.player.position[0])
            .hypot(object.position[1] - world.player.position[1]);
        for sample in 1..=count {
            let pos = motion
                .proxy()
                .transform(object.position, change * sample as f32 / count as f32);
            let distance =
                (pos[0] - world.player.position[0]).hypot(pos[1] - world.player.position[1]);
            if distance < radius && distance < previous - 1e-5 {
                return true;
            }
            previous = distance;
        }
    }
    false
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Phase {
    Closed,
    Delay(f32),
    Opening,
    Open,
    Closing,
}

/// Host capabilities must be opted into only when every emitted effect will
/// be applied to authoritative rendering, collision and cell state.
#[derive(Debug, Clone, Copy, Default)]
pub struct ProcessCapabilities<'a> {
    pub wall_elevation: bool,
    /// Initial raw cell types in x-major order (x * 64 + y), if cell changes
    /// can be applied by the host. Supplying None disables `change` processes.
    pub initial_cell_types: Option<&'a [u8; 4096]>,
}
#[derive(Debug, Clone, PartialEq)]
pub enum ProcessEffect {
    /// Absolute offset in raw script units from the initial wall position.
    WallElevation { wall_index: usize, offset_raw: f32 },
    /// New raw type for the grid cell; includes original type on reversal.
    CellType { cell: [u8; 2], raw_type: u8 },
    /// Absolute pose, keyed by the original stable map cell.
    ObjectPose {
        cell: [u8; 2],
        position: [f32; 2],
        angle_radians: f32,
        elevation_raw: f32,
    },
    /// Presentation request; no object animation is simulated by the runtime.
    PlayAnimation { object_id: u16 },
    /// The host must activate all zones under this process ID. Zones use
    /// inclusive map cells and raw vertical/damage units.
    HazardActivated {
        process_id: u16,
        zones: Vec<DeathZone>,
    },
    /// Remove the active hazards belonging to this process ID.
    HazardDeactivated { process_id: u16 },
}
/// A death field's authored values. Vertical bounds use 1/64 tile; negative
/// lower bounds designate fields that do not damage the player in the manual.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeathZone {
    pub min_cell: [u8; 2],
    pub max_cell: [u8; 2],
    pub z_bottom_raw: i32,
    pub z_top_raw: i32,
    pub damage_raw: u32,
}
#[derive(Debug, Clone)]
struct CellChange {
    cell: [u8; 2],
    raw_type: u8,
    previous: Option<u8>,
}

struct ProcessFrame<'a> {
    world: &'a mut World,
    elevations: &'a mut [f32],
    objects: &'a mut BTreeMap<[u8; 2], ProcessObject>,
    cell_types: &'a mut Option<Vec<u8>>,
    effects: &'a mut Vec<ProcessEffect>,
}

type Activation = (Vec<(u16, bool)>, Vec<u16>);

#[derive(Debug, Clone)]
struct RuntimeProcess {
    motions: Vec<Motion>,
    object_motions: Vec<ObjectMotion>,
    // Verified empty-cell commands still occupy a process movement stage.
    empty_motion_cells: Vec<[u8; 2]>,
    phase: Phase,
    progress: f32,
    duration: f32,
    delay: f32,
    back_wait: f32,
    open_time: f32,
    locked: bool,
    waitout: Vec<([f32; 2], f32)>,
    lock_changes: Vec<(u16, bool)>,
    cell_changes: Vec<CellChange>,
    animations: Vec<u16>,
    death_zones: Vec<DeathZone>,
    nonstop: bool,
    hazard_active: bool,
    supported: bool,
    required_keys: [bool; 3],
    mission_effect: bool,
}

fn property_float(process: &Process, name: &str, default: f32) -> f32 {
    process
        .properties
        .iter()
        .find(|cmd| cmd.name == name)
        .and_then(|cmd| cmd.arguments.first())
        .and_then(|value| value.parse::<f32>().ok())
        .filter(|value| value.is_finite() && *value >= 0.0)
        .unwrap_or(default)
}

impl RuntimeProcess {
    fn new(
        process: &Process,
        world: &World,
        capabilities: ProcessCapabilities<'_>,
        raw_cells: Option<&[u8; 4096]>,
        hazards_enabled: bool,
        objects: Option<&BTreeMap<[u8; 2], ProcessObject>>,
    ) -> Self {
        let mut motions = Vec::new();
        let mut object_motions = Vec::new();
        let mut empty_motion_cells = Vec::new();
        let mut empty_motion_distance = 0.0_f32;
        let mut cell_changes = Vec::new();
        let mut animations = Vec::new();
        let mut death_zones = Vec::new();
        let mut nonstop = false;
        let mut waitout = Vec::new();
        let mut lock_changes = Vec::new();
        let mut supported = true;
        let has_end_delay = process.properties.iter().any(|cmd| {
            cmd.name == "enddelay"
                && cmd.arguments.len() == 1
                && cmd.arguments[0]
                    .parse::<f32>()
                    .is_ok_and(|value| value.is_finite() && value > 0.0)
        });
        let has_message = process.properties.iter().any(|cmd| {
            matches!(cmd.name.as_str(), "onmessage" | "firstmessage")
                && cmd.arguments.len() == 1
                && cmd.arguments[0]
                    .parse::<u16>()
                    .is_ok_and(|value| value != 0)
        });
        for action in &process.actions {
            if action.name == "nonstop" {
                if hazards_enabled && action.arguments.is_empty() && !nonstop {
                    nonstop = true;
                    continue;
                }
                supported = false;
                break;
            }
            if action.name == "death" {
                let parsed = (|| {
                    if !hazards_enabled || action.arguments.len() != 7 {
                        return None;
                    }
                    let values: Vec<i32> = action
                        .arguments
                        .iter()
                        .map(|v| v.parse::<i32>().ok())
                        .collect::<Option<_>>()?;
                    let x1 = u8::try_from(values[0]).ok()?;
                    let y1 = u8::try_from(values[1]).ok()?;
                    let x2 = u8::try_from(values[2]).ok()?;
                    let y2 = u8::try_from(values[3]).ok()?;
                    if x1 > x2 || y1 > y2 || x2 >= 64 || y2 >= 64 || values[4] > values[5] {
                        return None;
                    }
                    Some(DeathZone {
                        min_cell: [x1, y1],
                        max_cell: [x2, y2],
                        z_bottom_raw: values[4],
                        z_top_raw: values[5],
                        damage_raw: u32::try_from(values[6]).ok()?,
                    })
                })();
                if let Some(zone) = parsed {
                    death_zones.push(zone);
                    continue;
                }
                supported = false;
                break;
            }
            if action.name == "playani" {
                if action.arguments.len() == 1 {
                    if let Ok(object_id) = action.arguments[0].parse::<u16>() {
                        animations.push(object_id);
                        continue;
                    }
                }
                supported = false;
                break;
            }
            if action.name == "change" {
                let args = &action.arguments;
                let parsed = (|| {
                    if capabilities.initial_cell_types.is_none() || args.len() != 3 {
                        return None;
                    }
                    let x = args[0].parse::<u8>().ok()?;
                    let y = args[1].parse::<u8>().ok()?;
                    let raw_type = args[2].parse::<u8>().ok()?;
                    if x >= 64
                        || y >= 64
                        || cell_changes.iter().any(|c: &CellChange| c.cell == [x, y])
                    {
                        return None;
                    }
                    Some(CellChange {
                        cell: [x, y],
                        raw_type,
                        previous: None,
                    })
                })();
                if let Some(change) = parsed {
                    cell_changes.push(change);
                } else {
                    supported = false;
                    break;
                }
                continue;
            }
            if matches!(action.name.as_str(), "lock" | "unlock") {
                if action.arguments.len() != 1 {
                    supported = false;
                    break;
                }
                if let Ok(id) = action.arguments[0].parse::<u16>() {
                    lock_changes.push((id, action.name == "lock"));
                } else {
                    supported = false;
                    break;
                }
                continue;
            }
            if action.name == "waitout" {
                let values: Option<Vec<f32>> = action
                    .arguments
                    .iter()
                    .map(|s| s.parse::<f32>().ok().filter(|x| x.is_finite()))
                    .collect();
                if let Some(values) = values.filter(|v| v.len() == 3 && v[2] >= 0.0) {
                    waitout.push((
                        [
                            values[0] / MAP_UNITS_PER_TILE,
                            values[1] / MAP_UNITS_PER_TILE,
                        ],
                        values[2] / MAP_UNITS_PER_TILE,
                    ));
                } else {
                    supported = false;
                    break;
                }
                continue;
            }
            let expected_arguments = match action.name.as_str() {
                "move" => 4,
                "rotate" => 5,
                "up" if capabilities.wall_elevation || objects.is_some() => 3,
                _ => {
                    supported = false;
                    break;
                }
            };
            if action.arguments.len() != expected_arguments {
                supported = false;
                break;
            }
            let Some(cell) = action
                .arguments
                .first()
                .and_then(|s| s.parse::<u8>().ok())
                .zip(action.arguments.get(1).and_then(|s| s.parse::<u8>().ok()))
            else {
                supported = false;
                break;
            };
            if cell.0 >= 64 || cell.1 >= 64 {
                supported = false;
                break;
            }
            let Some(dx) = action.arguments.get(2).and_then(|s| s.parse::<i32>().ok()) else {
                supported = false;
                break;
            };
            let dy = if action.name == "up" {
                0
            } else {
                let Some(value) = action.arguments.get(3).and_then(|s| s.parse::<i32>().ok())
                else {
                    supported = false;
                    break;
                };
                value
            };
            let kind = if action.name == "up" {
                MotionKind::Vertical(dx)
            } else if action.name == "rotate" {
                let Some(angle) = action.arguments.get(4).and_then(|s| s.parse::<i32>().ok())
                else {
                    supported = false;
                    break;
                };
                MotionKind::Rotate {
                    pivot: [
                        dx as f32 / MAP_UNITS_PER_TILE,
                        dy as f32 / MAP_UNITS_PER_TILE,
                    ],
                    radians: (angle as f32).to_radians(),
                }
            } else {
                MotionKind::Translate([
                    dx as f32 / MAP_UNITS_PER_TILE,
                    dy as f32 / MAP_UNITS_PER_TILE,
                ])
            };
            let before = motions.len();
            for (index, _) in world
                .walls
                .iter()
                .enumerate()
                .filter(|(_, wall)| wall.cell == Some([cell.0, cell.1]))
            {
                // Two transformations targeting the same wall would require
                // ordering semantics that have not been established.
                if motions.iter().any(|motion: &Motion| motion.wall == index) {
                    supported = false;
                    break;
                }
                motions.push(Motion {
                    wall: index,
                    kind: kind.clone(),
                });
            }
            if !supported {
                break;
            }
            if motions.len() != before && action.name == "up" && !capabilities.wall_elevation {
                supported = false;
                break;
            }
            // Missing walls could be models or malformed references. Never
            // perform only the subset of a process that happened to resolve.
            if motions.len() == before {
                let index = usize::from(cell.0) * 64 + usize::from(cell.1);
                let is_authored_object =
                    raw_cells.is_some_and(|cells| (131..=226).contains(&cells[index]));
                if is_authored_object
                    && objects.is_some_and(|poses| poses.contains_key(&[cell.0, cell.1]))
                {
                    if object_motions
                        .iter()
                        .any(|m: &ObjectMotion| m.cell == [cell.0, cell.1])
                    {
                        supported = false;
                        break;
                    }
                    object_motions.push(ObjectMotion {
                        cell: [cell.0, cell.1],
                        raw_type: raw_cells.expect("authored object has raw cells")[index],
                        kind,
                    });
                } else if raw_cells.is_some_and(|cells| cells[index] == 0) {
                    empty_motion_cells.push([cell.0, cell.1]);
                    let distance = match kind {
                        MotionKind::Translate(d) => d[0].hypot(d[1]),
                        MotionKind::Vertical(raw) => raw.unsigned_abs() as f32 / MAP_UNITS_PER_TILE,
                        MotionKind::Rotate { .. } => 0.0,
                    };
                    empty_motion_distance = empty_motion_distance.max(distance);
                } else {
                    supported = false;
                    break;
                }
            }
        }
        // `nonstop` modifies zone lifecycle. Without an understood persistent
        // effect it cannot safely make an otherwise unsupported script runnable.
        if nonstop && death_zones.is_empty() {
            supported = false;
        }
        if !object_motions.is_empty()
            && process
                .properties
                .iter()
                .any(|cmd| matches!(cmd.name.as_str(), "mortal" | "checkgo" | "checkback"))
        {
            // Object-versus-actor blocking/pushing/crushing is not implemented.
            supported = false;
        }
        if !supported {
            motions.clear();
            object_motions.clear();
            empty_motion_cells.clear();
            lock_changes.clear();
            cell_changes.clear();
            animations.clear();
            death_zones.clear();
        }
        // The manual labels Speed but does not specify units. At speed 8 we use
        // one tile per second; each process completes when its longest move does.
        let speed = property_float(process, "speed", 8.0).max(0.01) / 8.0;
        let wall_distance = motions
            .iter()
            .map(|m| m.travel_bound(&world.walls[m.wall]))
            .fold(empty_motion_distance, f32::max);
        let max_distance = object_motions
            .iter()
            .filter_map(|motion| {
                objects
                    .and_then(|poses| poses.get(&motion.cell))
                    .map(|pose| motion.travel_bound(pose))
            })
            .fold(wall_distance, f32::max);
        // The independent reconstruction advances procedure stages at
        // Speed/10 per second even for a hazard with no moving geometry.
        // Existing non-hazard movement timing is left unchanged.
        let duration = if death_zones.is_empty() {
            (max_distance / speed).max(0.001)
        } else {
            10.0 / property_float(process, "speed", 10.0).max(0.01)
        };
        let locked = process.properties.iter().any(|cmd| cmd.name == "lock");
        let required_keys = ["redkey", "greenkey", "bluekey"]
            .map(|name| process.properties.iter().any(|cmd| cmd.name == name));
        Self {
            motions,
            object_motions,
            empty_motion_cells,
            phase: Phase::Closed,
            progress: 0.0,
            duration,
            delay: property_float(process, "startdelay", 0.0),
            back_wait: property_float(process, "backwait", 0.0),
            open_time: 0.0,
            locked,
            waitout,
            lock_changes,
            cell_changes,
            animations,
            death_zones,
            nonstop,
            hazard_active: false,
            supported,
            required_keys,
            mission_effect: has_end_delay || has_message,
        }
    }
    fn trigger(&mut self, keys: [bool; 3]) -> Result<Activation, ProcessOutcome> {
        if self.locked
            || self
                .required_keys
                .iter()
                .zip(keys)
                .any(|(required, owned)| *required && !owned)
        {
            return Err(ProcessOutcome::Locked);
        }
        if !self.supported
            || (self.motions.is_empty()
                && self.object_motions.is_empty()
                && self.empty_motion_cells.is_empty()
                && self.lock_changes.is_empty()
                && self.cell_changes.is_empty()
                && self.animations.is_empty()
                && self.death_zones.is_empty()
                && !self.mission_effect)
        {
            return Err(ProcessOutcome::Unsupported);
        }
        match self.phase {
            Phase::Closed => {
                if !self.motions.is_empty()
                    || !self.object_motions.is_empty()
                    || !self.empty_motion_cells.is_empty()
                    || !self.cell_changes.is_empty()
                    || !self.death_zones.is_empty()
                {
                    self.phase = if self.delay > 0.0 {
                        Phase::Delay(self.delay)
                    } else {
                        Phase::Opening
                    };
                }
                Ok((self.lock_changes.clone(), self.animations.clone()))
            }
            Phase::Open
                if !self.motions.is_empty()
                    || !self.object_motions.is_empty()
                    || !self.empty_motion_cells.is_empty()
                    || !self.cell_changes.is_empty()
                    || !self.death_zones.is_empty() =>
            {
                self.phase = Phase::Closing;
                self.open_time = 0.0;
                Ok((Vec::new(), Vec::new()))
            }
            _ => Err(ProcessOutcome::Busy),
        }
    }
    /// Reverse an active process without re-triggering it or firing its
    /// activation side effects. An already closed or closing process is inert.
    fn start_hazard(&mut self, process_id: u16, effects: &mut Vec<ProcessEffect>) {
        if !self.locked && !self.hazard_active && !self.death_zones.is_empty() {
            self.hazard_active = true;
            effects.push(ProcessEffect::HazardActivated {
                process_id,
                zones: self.death_zones.clone(),
            });
        }
    }
    fn stop_hazard(&mut self, process_id: u16, effects: &mut Vec<ProcessEffect>) {
        if self.hazard_active {
            self.hazard_active = false;
            effects.push(ProcessEffect::HazardDeactivated { process_id });
        }
    }
    fn return_to_start(&mut self, process_id: u16, effects: &mut Vec<ProcessEffect>) {
        if self.locked || !self.supported {
            return;
        }
        match self.phase {
            Phase::Delay(_) => {
                self.phase = Phase::Closed;
                self.progress = 0.0;
            }
            Phase::Opening | Phase::Open => {
                self.phase = Phase::Closing;
                self.open_time = 0.0;
                if !self.nonstop {
                    self.stop_hazard(process_id, effects);
                }
            }
            Phase::Closed | Phase::Closing => {}
        }
    }
    fn tick(&mut self, process_id: u16, dt: f32, frame: &mut ProcessFrame<'_>) {
        let world = &mut *frame.world;
        let elevations = &mut *frame.elevations;
        let objects = &mut *frame.objects;
        let cell_types = &mut *frame.cell_types;
        let effects = &mut *frame.effects;
        if self.object_motions.iter().any(|motion| {
            !objects.contains_key(&motion.cell)
                || cell_types.as_ref().is_some_and(|types| {
                    types[usize::from(motion.cell[0]) * 64 + usize::from(motion.cell[1])]
                        != motion.raw_type
                })
        }) {
            return;
        }
        if self.phase == Phase::Closing
            && self.waitout.iter().any(|(center, radius)| {
                let dx = world.player.position[0] - center[0];
                let dy = world.player.position[1] - center[1];
                dx * dx + dy * dy <= radius * radius
            })
        {
            self.phase = Phase::Opening;
            self.start_hazard(process_id, effects);
        }
        let previous = self.progress;
        match self.phase {
            Phase::Delay(remaining) => {
                // Leftover tick time is carried into motion when delay expires.
                if remaining > dt {
                    self.phase = Phase::Delay(remaining - dt);
                } else {
                    self.phase = Phase::Opening;
                    self.start_hazard(process_id, effects);
                    self.progress = ((dt - remaining) / self.duration).min(1.0);
                }
            }
            Phase::Opening => self.progress = (self.progress + dt / self.duration).min(1.0),
            Phase::Open if self.back_wait > 0.0 => {
                self.open_time += dt;
                if self.open_time >= self.back_wait {
                    self.phase = Phase::Closing;
                }
            }
            Phase::Closing => self.progress = (self.progress - dt / self.duration).max(0.0),
            _ => {}
        }
        if matches!(self.phase, Phase::Opening) && self.progress >= 1.0 {
            self.phase = Phase::Open;
            self.open_time = 0.0;
        } else if matches!(self.phase, Phase::Closing) && self.progress <= 0.0 {
            self.phase = Phase::Closed;
        }
        let change = self.progress - previous;
        if change != 0.0 {
            if obstructed(&self.motions, world, change)
                || objects_obstructed(&self.object_motions, world, objects, change)
            {
                self.progress = previous;
                self.phase = if change > 0.0 {
                    Phase::Opening
                } else {
                    Phase::Closing
                };
                return;
            }
            for motion in &self.motions {
                match motion.kind {
                    MotionKind::Vertical(raw) => {
                        if let Some(offset) = elevations.get_mut(motion.wall) {
                            *offset += raw as f32 * change;
                            effects.push(ProcessEffect::WallElevation {
                                wall_index: motion.wall,
                                offset_raw: *offset,
                            });
                        }
                    }
                    _ => {
                        if let Some(wall) = world.walls.get_mut(motion.wall) {
                            wall.a = motion.transform(wall.a, change);
                            wall.b = motion.transform(wall.b, change);
                        }
                    }
                }
            }
            for motion in &self.object_motions {
                if let Some(object) = objects.get_mut(&motion.cell) {
                    match motion.kind {
                        MotionKind::Vertical(raw) => object.elevation_raw += raw as f32 * change,
                        MotionKind::Translate(_) => {
                            object.position = motion.proxy().transform(object.position, change)
                        }
                        MotionKind::Rotate { radians, .. } => {
                            object.position = motion.proxy().transform(object.position, change);
                            object.angle_radians += radians * change;
                        }
                    }
                    effects.push(ProcessEffect::ObjectPose {
                        cell: object.cell,
                        position: object.position,
                        angle_radians: object.angle_radians,
                        elevation_raw: object.elevation_raw,
                    });
                }
            }
            // Apply type changes only after successful motion completion. A
            // stored previous type makes reversal exact for one process.
            if let Some(types) = cell_types.as_mut() {
                for cell_change in &mut self.cell_changes {
                    let index =
                        usize::from(cell_change.cell[0]) * 64 + usize::from(cell_change.cell[1]);
                    if previous < 1.0 && self.progress >= 1.0 {
                        cell_change.previous = Some(types[index]);
                        types[index] = cell_change.raw_type;
                        effects.push(ProcessEffect::CellType {
                            cell: cell_change.cell,
                            raw_type: types[index],
                        });
                    } else if previous > 0.0 && self.progress <= 0.0 {
                        if let Some(original) = cell_change.previous.take() {
                            types[index] = original;
                            effects.push(ProcessEffect::CellType {
                                cell: cell_change.cell,
                                raw_type: original,
                            });
                        }
                    }
                }
            }
        }
        if self.phase == Phase::Open && !self.nonstop {
            self.stop_hazard(process_id, effects);
        }
    }
}

/// A real activation attempt, classified so callers can distinguish a locked
/// message from an unsupported script or a process that is still moving.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessOutcome {
    Accepted,
    Locked,
    Unsupported,
    Busy,
    Missing,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessEvent {
    pub process_id: u16,
    pub outcome: ProcessOutcome,
}

/// Process state can be ticked by a client or a headless authoritative server.
/// The owning world's wall vector must retain its original order and length.
#[derive(Clone)]
pub struct ProcessRuntime {
    processes: BTreeMap<u16, RuntimeProcess>,
    links: Vec<crate::process::Trigger>,
    previous_floor: Option<[u8; 2]>,
    keys: [bool; 3],
    elevations: Vec<f32>,
    objects: BTreeMap<[u8; 2], ProcessObject>,
    cell_types: Option<Vec<u8>>,
    effects: Vec<ProcessEffect>,
}

impl ProcessRuntime {
    pub fn new(script: &Script, world: &World) -> Self {
        Self::with_capabilities(script, world, ProcessCapabilities::default())
    }
    /// Opt in only when the host applies every effect after each tick.
    pub fn with_capabilities(
        script: &Script,
        world: &World,
        capabilities: ProcessCapabilities<'_>,
    ) -> Self {
        Self::build(
            script,
            world,
            capabilities,
            capabilities.initial_cell_types,
            false,
            None,
        )
    }
    /// Supply x-major map cell types for validating empty motion targets without
    /// opting into applying `change` effects. Non-empty cells without supported
    /// wall geometry still reject the entire process.
    pub fn with_cell_data(
        script: &Script,
        world: &World,
        capabilities: ProcessCapabilities<'_>,
        raw_cells: &[u8; 4096],
    ) -> Self {
        Self::build(
            script,
            world,
            capabilities,
            capabilities.initial_cell_types.or(Some(raw_cells)),
            false,
            None,
        )
    }
    /// Enable death-zone effects only when the authoritative host consumes
    /// and simulates every emitted hazard activation/deactivation.
    pub fn with_hazards(
        script: &Script,
        world: &World,
        capabilities: ProcessCapabilities<'_>,
        raw_cells: &[u8; 4096],
    ) -> Self {
        Self::build(
            script,
            world,
            capabilities,
            capabilities.initial_cell_types.or(Some(raw_cells)),
            true,
            None,
        )
    }
    /// Enable object poses and hazard events only when the host can apply
    /// every resulting pose to presentation, collision and pickup state.
    pub fn with_objects_and_hazards(
        script: &Script,
        world: &World,
        capabilities: ProcessCapabilities<'_>,
        raw_cells: &[u8; 4096],
        object_poses: &[ProcessObject],
    ) -> Result<Self, ProcessObjectError> {
        let mut objects = BTreeMap::new();
        for &object in object_poses {
            if object.cell[0] >= 64
                || object.cell[1] >= 64
                || !object.position.iter().all(|v| v.is_finite())
                || !object.angle_radians.is_finite()
                || !object.elevation_raw.is_finite()
                || !object.blocking_radius.is_finite()
                || !(0.0..=64.0).contains(&object.blocking_radius)
            {
                return Err(ProcessObjectError::Invalid);
            }
            let index = usize::from(object.cell[0]) * 64 + usize::from(object.cell[1]);
            if !(131..=226).contains(&raw_cells[index])
                || world
                    .walls
                    .iter()
                    .any(|wall| wall.cell == Some(object.cell))
            {
                return Err(ProcessObjectError::Invalid);
            }
            if objects.insert(object.cell, object).is_some() {
                return Err(ProcessObjectError::DuplicateCell);
            }
        }
        Ok(Self::build(
            script,
            world,
            capabilities,
            capabilities.initial_cell_types.or(Some(raw_cells)),
            true,
            Some(objects),
        ))
    }
    /// Mark a collected, destroyed or replaced object unavailable so later
    /// processes fail closed and any active movement stops emitting effects.
    pub fn remove_object(&mut self, cell: [u8; 2]) -> bool {
        self.objects.remove(&cell).is_some()
    }

    /// Retrieve current process-owned pose by original stable cell.
    pub fn object_pose(&self, cell: [u8; 2]) -> Option<ProcessObject> {
        self.objects.get(&cell).copied()
    }

    fn build(
        script: &Script,
        world: &World,
        capabilities: ProcessCapabilities<'_>,
        raw_cells: Option<&[u8; 4096]>,
        hazards_enabled: bool,
        objects: Option<BTreeMap<[u8; 2], ProcessObject>>,
    ) -> Self {
        let enabled = objects.is_some();
        let objects = objects.unwrap_or_default();
        Self {
            processes: script
                .processes
                .iter()
                .map(|(&id, process)| {
                    (
                        id,
                        RuntimeProcess::new(
                            process,
                            world,
                            capabilities,
                            raw_cells,
                            hazards_enabled,
                            enabled.then_some(&objects),
                        ),
                    )
                })
                .collect(),
            links: script.triggers.clone(),
            previous_floor: None,
            keys: [false; 3],
            elevations: vec![0.0; world.walls.len()],
            objects,
            cell_types: capabilities.initial_cell_types.map(|x| x.to_vec()),
            effects: Vec::new(),
        }
    }
    /// Drain effects accumulated since the last call, including direct activations.
    /// The authoritative host must consume these before advancing another step.
    pub fn take_effects(&mut self) -> Vec<ProcessEffect> {
        std::mem::take(&mut self.effects)
    }
    pub fn wall_elevation(&self, index: usize) -> Option<f32> {
        self.elevations.get(index).copied()
    }
    pub fn cell_type(&self, cell: [u8; 2]) -> Option<u8> {
        if cell[0] >= 64 || cell[1] >= 64 {
            return None;
        }
        self.cell_types
            .as_ref()
            .map(|types| types[usize::from(cell[0]) * 64 + usize::from(cell[1])])
    }
    /// Number of initially unlocked processes with fully supported wall motion.
    pub fn usable_count(&self) -> usize {
        self.processes
            .values()
            .filter(|p| {
                !p.locked
                    && !p.required_keys.iter().any(|&key| key)
                    && (!p.motions.is_empty() || !p.object_motions.is_empty())
            })
            .count()
    }
    /// Synchronize the owning player's red, green and blue key state.
    pub fn set_keys(&mut self, keys: [bool; 3]) {
        self.keys = keys;
    }

    fn trigger(&mut self, id: u16) -> ProcessEvent {
        let result = match self.processes.get_mut(&id) {
            Some(process)
                if process
                    .object_motions
                    .iter()
                    .any(|motion| !self.objects.contains_key(&motion.cell))
                    || self.cell_types.as_ref().is_some_and(|types| {
                        process.empty_motion_cells.iter().any(|cell| {
                            types[usize::from(cell[0]) * 64 + usize::from(cell[1])] != 0
                        }) || process.object_motions.iter().any(|motion| {
                            types[usize::from(motion.cell[0]) * 64 + usize::from(motion.cell[1])]
                                != motion.raw_type
                        })
                    }) =>
            {
                Err(ProcessOutcome::Unsupported)
            }
            Some(process) => process.trigger(self.keys),
            None => Err(ProcessOutcome::Missing),
        };
        let outcome = match result {
            Ok((changes, animations)) => {
                if let Some(process) = self.processes.get_mut(&id) {
                    if process.phase == Phase::Opening {
                        process.start_hazard(id, &mut self.effects);
                    }
                }
                self.effects.extend(
                    animations
                        .into_iter()
                        .map(|object_id| ProcessEffect::PlayAnimation { object_id }),
                );
                for (target, locked) in changes {
                    if let Some(process) = self.processes.get_mut(&target) {
                        process.locked = locked;
                        if locked {
                            process.stop_hazard(target, &mut self.effects);
                        }
                    }
                }
                ProcessOutcome::Accepted
            }
            Err(outcome) => outcome,
        };
        ProcessEvent {
            process_id: id,
            outcome,
        }
    }
    /// Activate links of a given kind for a cell in script order.
    /// Destroy links first unlock the referenced process, as described in the
    /// bundled modding manual.
    pub fn activate_at(&mut self, kind: &str, cell: [u8; 2]) -> Vec<ProcessEvent> {
        let ids: Vec<u16> = self
            .links
            .iter()
            .filter(|link| link.kind == kind && [link.x, link.y] == cell)
            .map(|link| link.process)
            .collect();
        if matches!(kind, "return" | "returnf") {
            for id in ids {
                if let Some(process) = self.processes.get_mut(&id) {
                    process.return_to_start(id, &mut self.effects);
                }
            }
            return Vec::new();
        }
        ids.into_iter()
            .map(|id| {
                if kind == "destroy" {
                    if let Some(process) = self.processes.get_mut(&id) {
                        process.locked = false;
                    }
                }
                self.trigger(id)
            })
            .collect()
    }
    pub fn activate_links_at(&mut self, cell: [u8; 2]) -> Vec<ProcessEvent> {
        self.activate_at("link", cell)
    }
    /// Apply use and reverse links at one cell in source order. Reverse links
    /// do not emit activation events or schedule mission completion.
    fn use_at(&mut self, cell: [u8; 2]) -> Vec<ProcessEvent> {
        let links: Vec<(String, u16)> = self
            .links
            .iter()
            .filter(|link| {
                matches!(link.kind.as_str(), "link" | "return") && [link.x, link.y] == cell
            })
            .map(|link| (link.kind.clone(), link.process))
            .collect();
        let mut events = Vec::new();
        for (kind, id) in links {
            if kind == "return" {
                if let Some(process) = self.processes.get_mut(&id) {
                    process.return_to_start(id, &mut self.effects);
                }
            } else {
                events.push(self.trigger(id));
            }
        }
        events
    }
    /// Call after movement once per simulation step. `interact_pressed` must be
    /// true on the press edge only, not every frame while the key is held.
    pub fn tick(
        &mut self,
        world: &mut World,
        dt: f32,
        interact_pressed: bool,
    ) -> Vec<ProcessEvent> {
        if !dt.is_finite() || dt <= 0.0 {
            return Vec::new();
        }
        let mut events = Vec::new();
        let p = world.player.position;
        let floor = if p[0].is_finite()
            && p[1].is_finite()
            && p[0] >= 0.0
            && p[0] < 64.0
            && p[1] >= 0.0
            && p[1] < 64.0
        {
            Some([p[0].floor() as u8, p[1].floor() as u8])
        } else {
            None
        };
        if floor != self.previous_floor {
            if let Some(cell) = floor {
                let links: Vec<(String, u16)> = self
                    .links
                    .iter()
                    .filter(|trigger| {
                        matches!(trigger.kind.as_str(), "floor" | "returnf")
                            && [trigger.x, trigger.y] == cell
                    })
                    .map(|trigger| (trigger.kind.clone(), trigger.process))
                    .collect();
                for (kind, id) in links {
                    if kind == "returnf" {
                        if let Some(process) = self.processes.get_mut(&id) {
                            process.return_to_start(id, &mut self.effects);
                        }
                    } else {
                        events.push(self.trigger(id));
                    }
                }
            }
            self.previous_floor = floor;
        }
        if interact_pressed && p[0].is_finite() && p[1].is_finite() {
            // A linked cell may contain an object, which the current preview has
            // not loaded. Cell-center distance is a provisional interaction test.
            let nearest = self
                .links
                .iter()
                .filter(|link| matches!(link.kind.as_str(), "link" | "return"))
                .map(|link| {
                    let dx = f32::from(link.x) + 0.5 - p[0];
                    let dy = f32::from(link.y) + 0.5 - p[1];
                    ([link.x, link.y], dx * dx + dy * dy)
                })
                .filter(|(_, distance)| *distance <= USE_RANGE * USE_RANGE)
                .min_by(|a, b| a.1.total_cmp(&b.1));
            if let Some((cell, _)) = nearest {
                events.extend(self.use_at(cell));
            }
        }
        let mut frame = ProcessFrame {
            world,
            elevations: &mut self.elevations,
            objects: &mut self.objects,
            cell_types: &mut self.cell_types,
            effects: &mut self.effects,
        };
        for (&id, process) in &mut self.processes {
            process.tick(id, dt, &mut frame);
        }
        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::{Player, Wall};

    fn world() -> World {
        World {
            walls: vec![Wall {
                a: [1.0, 0.0],
                b: [1.0, 1.0],
                texture: 1,
                blocks_movement: true,
                cell: Some([1, 0]),
            }],
            player: Player {
                position: [0.5, 0.5],
                angle: 0.0,
            },
        }
    }
    fn tick_for(runtime: &mut ProcessRuntime, world: &mut World, time: f32) {
        for _ in 0..(time * 60.0).round() as usize {
            runtime.tick(world, 1.0 / 60.0, false);
        }
    }
    #[test]
    fn reports_accepted_locked_unsupported_and_busy_attempts() {
        let script = Script::parse("#proc1\nEndDelay 1\n#end\n#proc2\nLock\nEndDelay 1\n#end\n#proc3\nEndDelay 1\n#action\nrotate 1 0 1 1\n#end\n#proc4\n#action\nmove 1 0 256 0\n#end\n#links\nlink 0 0 1\nlink 1 0 2\nlink 2 0 3\nlink 3 0 4\n#end\n");
        let mut world = world();
        let mut runtime = ProcessRuntime::new(&script, &world);
        world.player.position = [0.5, 0.5];
        assert_eq!(
            runtime.tick(&mut world, 1.0 / 60.0, true)[0].outcome,
            ProcessOutcome::Accepted
        );
        world.player.position = [1.5, 0.5];
        assert_eq!(
            runtime.tick(&mut world, 1.0 / 60.0, true)[0].outcome,
            ProcessOutcome::Locked
        );
        world.player.position = [2.5, 0.5];
        assert_eq!(
            runtime.tick(&mut world, 1.0 / 60.0, true)[0].outcome,
            ProcessOutcome::Unsupported
        );
        world.player.position = [3.5, 0.5];
        assert_eq!(
            runtime.tick(&mut world, 1.0 / 60.0, true)[0].outcome,
            ProcessOutcome::Accepted
        );
        assert_eq!(
            runtime.tick(&mut world, 1.0 / 60.0, true)[0].outcome,
            ProcessOutcome::Busy
        );
    }
    #[test]
    fn playani_emits_events_only_for_supported_actions() {
        for (body, expected) in [
            ("EndDelay 1\n#action\nplayani 8", ProcessOutcome::Accepted),
            (
                "EndDelay 1\n#action\nplayani 8\nchange 1 0 2",
                ProcessOutcome::Unsupported,
            ),
            ("#action\nplayani 8", ProcessOutcome::Accepted),
            (
                "EndDelay 1\n#action\nplayani invalid",
                ProcessOutcome::Unsupported,
            ),
        ] {
            let script =
                Script::parse(&format!("#proc1\n{body}\n#end\n#links\nlink 0 0 1\n#end\n"));
            let mut world = world();
            let mut runtime = ProcessRuntime::new(&script, &world);
            assert_eq!(
                runtime.tick(&mut world, 1.0 / 60.0, true)[0].outcome,
                expected,
                "{body}"
            );
            let effects = runtime.take_effects();
            if expected == ProcessOutcome::Accepted {
                assert_eq!(effects, [ProcessEffect::PlayAnimation { object_id: 8 }]);
            } else {
                assert!(effects.is_empty());
            }
        }
    }
    #[test]
    fn vertical_walls_are_opt_in_emit_every_step_and_reverse() {
        let script = Script::parse(
            "#proc1\nBackWait 0.1\nSpeed 8\n#action\nup 1 0 96\n#end\n#links\nfloor 0 0 1\n#end\n",
        );
        let mut world = world();
        let mut disabled = ProcessRuntime::new(&script, &world);
        assert_eq!(
            disabled.tick(&mut world, 1.0 / 60.0, false)[0].outcome,
            ProcessOutcome::Unsupported
        );
        assert!(disabled.take_effects().is_empty());
        let original = world.walls[0];
        let mut runtime = ProcessRuntime::with_capabilities(
            &script,
            &world,
            ProcessCapabilities {
                wall_elevation: true,
                initial_cell_types: None,
            },
        );
        let mut offsets = Vec::new();
        for _ in 0..100 {
            runtime.tick(&mut world, 1.0 / 60.0, false);
            for effect in runtime.take_effects() {
                let ProcessEffect::WallElevation {
                    wall_index,
                    offset_raw,
                } = effect
                else {
                    panic!("unexpected effect")
                };
                assert_eq!(wall_index, 0);
                offsets.push(offset_raw);
            }
        }
        assert!(offsets.iter().any(|x| (*x - 96.0).abs() < 0.001));
        assert!(runtime.wall_elevation(0).unwrap().abs() < 0.001);
        assert_eq!(world.walls[0].a, original.a);
        assert_eq!(world.walls[0].b, original.b);
    }
    #[test]
    fn cell_change_is_opt_in_and_restores_original_type() {
        let script = Script::parse(
            "#proc1\nBackWait 0.1\n#action\nchange 1 0 184\n#end\n#links\nfloor 0 0 1\n#end\n",
        );
        let mut world = world();
        let mut disabled = ProcessRuntime::new(&script, &world);
        assert_eq!(
            disabled.tick(&mut world, 1.0 / 60.0, false)[0].outcome,
            ProcessOutcome::Unsupported
        );
        let mut cells = [0u8; 4096];
        cells[64] = 92;
        let mut runtime = ProcessRuntime::with_capabilities(
            &script,
            &world,
            ProcessCapabilities {
                wall_elevation: false,
                initial_cell_types: Some(&cells),
            },
        );
        assert_eq!(
            runtime.tick(&mut world, 1.0 / 60.0, false)[0].outcome,
            ProcessOutcome::Accepted
        );
        assert_eq!(
            runtime.take_effects(),
            [ProcessEffect::CellType {
                cell: [1, 0],
                raw_type: 184
            }]
        );
        assert_eq!(runtime.cell_type([1, 0]), Some(184));
        tick_for(&mut runtime, &mut world, 0.3);
        assert_eq!(
            runtime.take_effects(),
            [ProcessEffect::CellType {
                cell: [1, 0],
                raw_type: 92
            }]
        );
        assert_eq!(runtime.cell_type([1, 0]), Some(92));
    }
    #[test]
    fn unsupported_mixed_process_never_emits_partial_effects() {
        let script = Script::parse(
            "#proc1\n#action\nplayani 58\nup 1 0 96\nunknown 1\n#end\n#links\nlink 0 0 1\n#end\n",
        );
        let mut world = world();
        let mut runtime = ProcessRuntime::with_capabilities(
            &script,
            &world,
            ProcessCapabilities {
                wall_elevation: true,
                initial_cell_types: None,
            },
        );
        assert_eq!(
            runtime.tick(&mut world, 1.0 / 60.0, true)[0].outcome,
            ProcessOutcome::Unsupported
        );
        assert!(runtime.take_effects().is_empty());
        assert_eq!(runtime.wall_elevation(0), Some(0.0));
    }
    #[test]
    fn direct_activation_effects_survive_until_drained() {
        let script = Script::parse("#proc1\n#action\nplayani 58\n#end\n#links\nlink 0 0 1\n#end\n");
        let mut world = world();
        let mut runtime = ProcessRuntime::new(&script, &world);
        assert_eq!(
            runtime.activate_links_at([0, 0])[0].outcome,
            ProcessOutcome::Accepted
        );
        runtime.tick(&mut world, 1.0 / 60.0, false);
        assert_eq!(
            runtime.take_effects(),
            [ProcessEffect::PlayAnimation { object_id: 58 }]
        );
        assert!(runtime.take_effects().is_empty());
    }
    fn object_fixture(cell: [u8; 2]) -> ProcessObject {
        ProcessObject {
            cell,
            position: [f32::from(cell[0]) + 0.5, f32::from(cell[1]) + 0.5],
            angle_radians: 0.0,
            elevation_raw: 0.0,
            blocking_radius: 0.2,
        }
    }
    #[test]
    fn object_move_is_opt_in_emits_absolute_pose_and_reverses() {
        let script = Script::parse(
            "#proc1\nBackWait 0.1\n#action\nmove 2 0 256 0\n#end\n#links\nfloor 0 0 1\n#end\n",
        );
        let mut world = world();
        let mut cells = [0u8; 4096];
        cells[2 * 64] = 163;
        let mut disabled =
            ProcessRuntime::with_cell_data(&script, &world, ProcessCapabilities::default(), &cells);
        assert_eq!(
            disabled.tick(&mut world, 1.0 / 60.0, false)[0].outcome,
            ProcessOutcome::Unsupported
        );
        let mut runtime = ProcessRuntime::with_objects_and_hazards(
            &script,
            &world,
            ProcessCapabilities::default(),
            &cells,
            &[object_fixture([2, 0])],
        )
        .unwrap();
        assert_eq!(
            runtime.tick(&mut world, 1.0 / 60.0, false)[0].outcome,
            ProcessOutcome::Accepted
        );
        assert!(matches!(
            runtime.take_effects().as_slice(),
            [ProcessEffect::ObjectPose { cell: [2, 0], .. }]
        ));
        for _ in 0..60 {
            runtime.tick(&mut world, 1.0 / 60.0, false);
            runtime.take_effects();
        }
        assert!((runtime.object_pose([2, 0]).unwrap().position[0] - 3.5).abs() < 0.001);
        for _ in 0..70 {
            runtime.tick(&mut world, 1.0 / 60.0, false);
            runtime.take_effects();
        }
        assert!((runtime.object_pose([2, 0]).unwrap().position[0] - 2.5).abs() < 0.001);
        assert_eq!(runtime.object_pose([2, 0]).unwrap().cell, [2, 0]);
    }
    #[test]
    fn object_rotation_and_vertical_actions_emit_complete_poses() {
        for (action, expected) in [("rotate 2 0 512 0 90", true), ("up 2 0 96", false)] {
            let script = Script::parse(&format!(
                "#proc1\n#action\n{action}\n#end\n#links\nfloor 0 0 1\n#end\n"
            ));
            let mut world = world();
            let mut cells = [0u8; 4096];
            cells[128] = 163;
            let mut runtime = ProcessRuntime::with_objects_and_hazards(
                &script,
                &world,
                ProcessCapabilities::default(),
                &cells,
                &[object_fixture([2, 0])],
            )
            .unwrap();
            assert_eq!(
                runtime.tick(&mut world, 1.0 / 60.0, false)[0].outcome,
                ProcessOutcome::Accepted
            );
            for _ in 0..240 {
                runtime.tick(&mut world, 1.0 / 60.0, false);
            }
            let pose = runtime.object_pose([2, 0]).unwrap();
            if expected {
                assert!((pose.position[0] - 1.5).abs() < 0.002, "{pose:?}");
                assert!((pose.position[1] - 0.5).abs() < 0.002, "{pose:?}");
                assert!((pose.angle_radians - std::f32::consts::FRAC_PI_2).abs() < 0.002);
            } else {
                assert!((pose.elevation_raw - 96.0).abs() < 0.002);
            }
            assert!(runtime
                .take_effects()
                .iter()
                .any(|effect| matches!(effect, ProcessEffect::ObjectPose { cell: [2, 0], .. })));
        }
    }
    #[test]
    fn moving_object_stops_before_player_and_resumes() {
        let script =
            Script::parse("#proc1\n#action\nmove 2 0 -512 0\n#end\n#links\nfloor 0 0 1\n#end\n");
        let mut world = world();
        let mut cells = [0u8; 4096];
        cells[128] = 163;
        let mut runtime = ProcessRuntime::with_objects_and_hazards(
            &script,
            &world,
            ProcessCapabilities::default(),
            &cells,
            &[object_fixture([2, 0])],
        )
        .unwrap();
        tick_for(&mut runtime, &mut world, 3.0);
        let x = runtime.object_pose([2, 0]).unwrap().position[0];
        assert!((0.5 + 0.18 + 0.2 - 0.001..0.95).contains(&x), "{x}");
        world.player.position = [0.0, 2.0];
        tick_for(&mut runtime, &mut world, 3.0);
        assert!((runtime.object_pose([2, 0]).unwrap().position[0] - 0.5).abs() < 0.001);
    }
    #[test]
    fn object_motion_fails_closed_for_invalid_missing_or_mortal_targets() {
        let mut world = world();
        let mut cells = [0u8; 4096];
        cells[128] = 163;
        let script =
            Script::parse("#proc1\n#action\nmove 2 0 256 0\n#end\n#links\nfloor 0 0 1\n#end\n");
        let mut missing = ProcessRuntime::with_objects_and_hazards(
            &script,
            &world,
            ProcessCapabilities::default(),
            &cells,
            &[],
        )
        .unwrap();
        assert_eq!(
            missing.tick(&mut world, 1.0 / 60.0, false)[0].outcome,
            ProcessOutcome::Unsupported
        );
        let object = object_fixture([2, 0]);
        assert!(matches!(
            ProcessRuntime::with_objects_and_hazards(
                &script,
                &world,
                ProcessCapabilities::default(),
                &cells,
                &[object, object]
            ),
            Err(ProcessObjectError::DuplicateCell)
        ));
        let mut bad = object;
        bad.position[0] = f32::NAN;
        assert!(matches!(
            ProcessRuntime::with_objects_and_hazards(
                &script,
                &world,
                ProcessCapabilities::default(),
                &cells,
                &[bad]
            ),
            Err(ProcessObjectError::Invalid)
        ));
        let lethal = Script::parse(
            "#proc1\nMortal\n#action\nmove 2 0 256 0\n#end\n#links\nfloor 0 0 1\n#end\n",
        );
        let mut runtime = ProcessRuntime::with_objects_and_hazards(
            &lethal,
            &world,
            ProcessCapabilities::default(),
            &cells,
            &[object],
        )
        .unwrap();
        assert_eq!(
            runtime.tick(&mut world, 1.0 / 60.0, false)[0].outcome,
            ProcessOutcome::Unsupported
        );
        assert!(runtime.take_effects().is_empty());
    }
    #[test]
    fn changed_cell_and_missing_object_disable_whole_mixed_process() {
        let script = Script::parse("#proc1\n#action\nmove 1 0 256 0\nmove 2 0 256 0\n#end\n#proc2\n#action\nchange 2 0 184\n#end\n#links\nlink 0 0 1\nlink 2 1 2\n#end\n");
        let mut world = world();
        let mut cells = [0u8; 4096];
        cells[64] = 1;
        cells[128] = 163;
        let mut missing = ProcessRuntime::with_objects_and_hazards(
            &script,
            &world,
            ProcessCapabilities {
                wall_elevation: true,
                initial_cell_types: Some(&cells),
            },
            &cells,
            &[],
        )
        .unwrap();
        assert_eq!(
            missing.activate_at("link", [0, 0])[0].outcome,
            ProcessOutcome::Unsupported
        );
        tick_for(&mut missing, &mut world, 1.5);
        assert_eq!(world.walls[0].a, [1.0, 0.0]);
        let mut runtime = ProcessRuntime::with_objects_and_hazards(
            &script,
            &world,
            ProcessCapabilities {
                wall_elevation: true,
                initial_cell_types: Some(&cells),
            },
            &cells,
            &[object_fixture([2, 0])],
        )
        .unwrap();
        assert_eq!(
            runtime.activate_at("link", [2, 1])[0].outcome,
            ProcessOutcome::Accepted
        );
        runtime.tick(&mut world, 1.0 / 60.0, false);
        assert_eq!(runtime.cell_type([2, 0]), Some(184));
        runtime.take_effects();
        assert_eq!(
            runtime.activate_at("link", [0, 0])[0].outcome,
            ProcessOutcome::Unsupported
        );
        tick_for(&mut runtime, &mut world, 1.5);
        assert_eq!(world.walls[0].a, [1.0, 0.0]);
        assert!(runtime.take_effects().is_empty());
    }
    #[test]
    fn removing_object_stops_active_motion_and_rejects_future_triggers() {
        let script =
            Script::parse("#proc1\n#action\nmove 2 0 256 0\n#end\n#links\nlink 0 0 1\n#end\n");
        let mut world = world();
        let mut cells = [0u8; 4096];
        cells[128] = 163;
        let mut runtime = ProcessRuntime::with_objects_and_hazards(
            &script,
            &world,
            ProcessCapabilities::default(),
            &cells,
            &[object_fixture([2, 0])],
        )
        .unwrap();
        assert_eq!(
            runtime.activate_at("link", [0, 0])[0].outcome,
            ProcessOutcome::Accepted
        );
        runtime.tick(&mut world, 1.0 / 60.0, false);
        runtime.take_effects();
        assert!(runtime.remove_object([2, 0]));
        runtime.tick(&mut world, 1.0 / 60.0, false);
        assert!(runtime.take_effects().is_empty());
        assert_eq!(
            runtime.activate_at("link", [0, 0])[0].outcome,
            ProcessOutcome::Unsupported
        );
    }
    #[test]
    fn floor_trigger_opens_and_automatically_returns_wall() {
        let script = Script::parse("#proc1\nSpeed 8\nBackWait 1\n#action\nmove 1 0 256 0\n#end\n#links\nfloor 0 0 1\n#end\n");
        let mut world = world();
        let mut runtime = ProcessRuntime::new(&script, &world);
        tick_for(&mut runtime, &mut world, 1.0);
        assert!((world.walls[0].a[0] - 2.0).abs() < 0.001);
        tick_for(&mut runtime, &mut world, 2.1);
        assert!((world.walls[0].a[0] - 1.0).abs() < 0.001);
        tick_for(&mut runtime, &mut world, 1.0);
        assert!(
            (world.walls[0].a[0] - 1.0).abs() < 0.001,
            "floor contact should not retrigger every tick"
        );
    }
    #[test]
    fn link_requires_press_and_locked_or_keyed_processes_do_not_move() {
        for property in ["Lock", "RedKey", "GreenKey", "BlueKey"] {
            let script = Script::parse(&format!(
                "#proc1\n{property}\n#action\nmove 1 0 256 0\n#end\n#links\nlink 1 0 1\n#end\n"
            ));
            let mut world = world();
            let mut runtime = ProcessRuntime::new(&script, &world);
            runtime.tick(&mut world, 1.0 / 60.0, true);
            tick_for(&mut runtime, &mut world, 2.0);
            assert_eq!(world.walls[0].a[0], 1.0);
        }
        let script =
            Script::parse("#proc1\n#action\nmove 1 0 256 0\n#end\n#links\nlink 1 0 1\n#end\n");
        let mut world = world();
        let mut runtime = ProcessRuntime::new(&script, &world);
        tick_for(&mut runtime, &mut world, 1.0);
        assert_eq!(world.walls[0].a[0], 1.0);
        runtime.tick(&mut world, 1.0 / 60.0, true);
        tick_for(&mut runtime, &mut world, 1.0);
        assert!((world.walls[0].a[0] - 2.0).abs() < 0.001);
    }
    #[test]
    fn use_activates_all_processes_bound_to_nearest_cell() {
        let script = Script::parse("#proc1\n#action\nunlock 3\n#end\n#proc2\n#action\nlock 3\n#end\n#proc3\nLock\n#action\nmove 1 0 256 0\n#end\n#links\nlink 1 0 1\nlink 1 0 2\n#end\n");
        let mut world = world();
        let mut runtime = ProcessRuntime::new(&script, &world);
        let events = runtime.tick(&mut world, 1.0 / 60.0, true);
        assert_eq!(
            events
                .iter()
                .map(|event| event.process_id)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert!(events
            .iter()
            .all(|event| event.outcome == ProcessOutcome::Accepted));
        assert!(runtime.processes[&3].locked);
    }
    #[test]
    fn use_chooses_nearest_link() {
        let script = Script::parse("#proc1\n#action\nmove 1 0 256 0\n#end\n#proc2\n#action\nmove 1 0 512 0\n#end\n#links\nlink 1 0 1\nlink 0 1 2\n#end\n");
        let mut world = world();
        world.player.position = [1.2, 0.5];
        let mut runtime = ProcessRuntime::new(&script, &world);
        runtime.tick(&mut world, 1.0 / 60.0, true);
        // Step away so the wall can move after the chosen link is activated.
        world.player.position = [0.0, 2.0];
        tick_for(&mut runtime, &mut world, 2.1);
        assert!((world.walls[0].a[0] - 2.0).abs() < 0.001);
    }
    #[test]
    fn unsupported_or_missing_targets_disable_the_entire_process() {
        for action in [
            "rotate 1 0 1 1", // missing angle
            "move 2 0 256 0",
            "move 1 0 128 0 s:2",
            "unknown 1 0",
        ] {
            let script = Script::parse(&format!(
                "#proc1\n#action\nmove 1 0 256 0\n{action}\n#end\n#links\nfloor 0 0 1\n#end\n"
            ));
            let mut world = world();
            let mut runtime = ProcessRuntime::new(&script, &world);
            tick_for(&mut runtime, &mut world, 2.0);
            assert_eq!(world.walls[0].a[0], 1.0, "{action}");
        }
    }
    #[test]
    fn rotates_wall_around_pivot_and_returns_to_initial_geometry() {
        let script = Script::parse("#proc1\nSpeed 8\nBackWait 0.1\n#action\nrotate 1 0 256 0 90\n#end\n#links\nfloor 0 0 1\n#end\n");
        let mut world = world();
        world.player.position = [0.0, 0.9];
        let original = world.walls[0];
        let mut runtime = ProcessRuntime::new(&script, &world);
        tick_for(&mut runtime, &mut world, 1.6);
        assert!((world.walls[0].a[0] - 1.0).abs() < 0.001);
        assert!(
            (world.walls[0].b[0] - 0.0).abs() < 0.02,
            "{:?}",
            world.walls[0]
        );
        assert!(world.walls[0].b[1].abs() < 0.02);
        tick_for(&mut runtime, &mut world, 2.0);
        assert!((world.walls[0].a[0] - original.a[0]).abs() < 0.001);
        assert!((world.walls[0].b[0] - original.b[0]).abs() < 0.001);
        assert!((world.walls[0].b[1] - original.b[1]).abs() < 0.001);
    }
    #[test]
    fn a_rotating_wall_stops_before_overlapping_player() {
        let script = Script::parse(
            "#proc1\nSpeed 8\n#action\nrotate 1 0 256 0 90\n#end\n#links\nlink 1 0 1\n#end\n",
        );
        let mut world = world();
        world.player.position = [0.5, 0.5];
        let mut runtime = ProcessRuntime::new(&script, &world);
        runtime.tick(&mut world, 1.0 / 60.0, true);
        tick_for(&mut runtime, &mut world, 3.0);
        let wall = world.walls[0];
        assert!(distance_to_segment(world.player.position, wall.a, wall.b) >= 0.18 - 0.001);
        assert!(
            wall.b[0] > 0.4,
            "blocked before complete rotation: {wall:?}"
        );
        world.player.position = [0.0, 1.5];
        tick_for(&mut runtime, &mut world, 2.0);
        assert!(world.walls[0].b[0].abs() < 0.02);
    }
    #[test]
    fn negative_rotation_turns_opposite_direction() {
        let script = Script::parse(
            "#proc1\n#action\nrotate 1 0 256 0 -90\n#end\n#links\nfloor 0 0 1\n#end\n",
        );
        let mut world = world();
        world.player.position = [0.0, 0.9];
        let mut runtime = ProcessRuntime::new(&script, &world);
        tick_for(&mut runtime, &mut world, 2.0);
        assert!((world.walls[0].b[0] - 2.0).abs() < 0.02);
        assert!(world.walls[0].b[1].abs() < 0.02);
    }
    #[test]
    fn translating_door_stops_before_player() {
        let script =
            Script::parse("#proc1\n#action\nmove 1 0 -256 0\n#end\n#links\nfloor 0 0 1\n#end\n");
        let mut world = world();
        let mut runtime = ProcessRuntime::new(&script, &world);
        tick_for(&mut runtime, &mut world, 2.0);
        assert!(world.walls[0].a[0] >= world.player.position[0] + 0.18 - 0.001);
        assert!(world.walls[0].a[0] < 0.7);
        world.player.position = [2.0, 2.0];
        tick_for(&mut runtime, &mut world, 2.0);
        assert!(world.walls[0].a[0].abs() < 0.02);
    }
    #[test]
    fn duplicate_target_actions_disable_process() {
        let script = Script::parse("#proc1\n#action\nmove 1 0 256 0\nrotate 1 0 256 0 90\n#end\n#links\nfloor 0 0 1\n#end\n");
        let mut world = world();
        let mut runtime = ProcessRuntime::new(&script, &world);
        tick_for(&mut runtime, &mut world, 2.0);
        assert_eq!(world.walls[0].a, [1.0, 0.0]);
        assert_eq!(world.walls[0].b, [1.0, 1.0]);
    }
    #[test]
    fn required_key_allows_a_door_to_open() {
        let script = Script::parse(
            "#proc1\nGreenKey\n#action\nmove 1 0 256 0\n#end\n#links\nlink 1 0 1\n#end\n",
        );
        let mut world = world();
        let mut runtime = ProcessRuntime::new(&script, &world);
        runtime.tick(&mut world, 1.0 / 60.0, true);
        tick_for(&mut runtime, &mut world, 1.0);
        assert_eq!(world.walls[0].a[0], 1.0);
        runtime.set_keys([false, true, false]);
        runtime.tick(&mut world, 1.0 / 60.0, true);
        tick_for(&mut runtime, &mut world, 1.0);
        assert!((world.walls[0].a[0] - 2.0).abs() < 0.001);
    }
    #[test]
    fn a_linked_switch_can_unlock_a_separate_door() {
        let script = Script::parse("#proc1\nLock\n#action\nmove 1 0 256 0\n#end\n#proc2\n#action\nunlock 1\n#end\n#links\nlink 1 0 2\nfloor 0 0 1\n#end\n");
        let mut world = world();
        let mut runtime = ProcessRuntime::new(&script, &world);
        runtime.tick(&mut world, 1.0 / 60.0, true);
        world.player.position = [2.5, 0.5];
        runtime.tick(&mut world, 1.0 / 60.0, false);
        world.player.position = [0.5, 0.5];
        tick_for(&mut runtime, &mut world, 1.1);
        assert!((world.walls[0].a[0] - 2.0).abs() < 0.001);
    }
    #[test]
    fn waitout_reopens_a_closing_door_when_player_enters() {
        let script = Script::parse("#proc1\nBackWait 0.1\n#action\nwaitout 128 128 100\nmove 1 0 256 0\n#end\n#links\nfloor 0 0 1\n#end\n");
        let mut world = world();
        let mut runtime = ProcessRuntime::new(&script, &world);
        tick_for(&mut runtime, &mut world, 1.8);
        assert!(
            world.walls[0].a[0] > 1.85,
            "door should reopen within waitout radius"
        );
    }
    #[test]
    fn waits_for_start_delay_and_rejects_nonfinite_time() {
        let script = Script::parse(
            "#proc1\nStartDelay 1\n#action\nmove 1 0 256 0\n#end\n#links\nfloor 0 0 1\n#end\n",
        );
        let mut world = world();
        let mut runtime = ProcessRuntime::new(&script, &world);
        runtime.tick(&mut world, f32::NAN, false);
        tick_for(&mut runtime, &mut world, 0.5);
        assert_eq!(world.walls[0].a[0], 1.0);
        tick_for(&mut runtime, &mut world, 1.6);
        assert!((world.walls[0].a[0] - 2.0).abs() < 0.001);
    }

    #[test]
    fn empty_motion_targets_require_raw_cell_data_and_preserve_stage() {
        let script = Script::parse("#proc1\nSpeed 8\n#action\nmove 0 0 256 0\n#end\n#proc2\n#action\nchange 0 0 5\n#end\n#links\nlink 0 0 1\nlink 2 0 2\n#end\n");
        let mut world = world();
        let mut old = ProcessRuntime::new(&script, &world);
        assert_eq!(
            old.activate_at("link", [0, 0])[0].outcome,
            ProcessOutcome::Unsupported
        );
        let cells = [0; 4096];
        let mut runtime =
            ProcessRuntime::with_cell_data(&script, &world, ProcessCapabilities::default(), &cells);
        assert_eq!(
            runtime.activate_at("link", [2, 0])[0].outcome,
            ProcessOutcome::Unsupported
        );
        assert_eq!(
            runtime.activate_at("link", [0, 0])[0].outcome,
            ProcessOutcome::Accepted
        );
        assert_eq!(
            runtime.activate_at("link", [0, 0])[0].outcome,
            ProcessOutcome::Busy
        );
        world.player.position = [-10.0, -10.0];
        tick_for(&mut runtime, &mut world, 1.1);
        assert_eq!(
            runtime.activate_at("link", [0, 0])[0].outcome,
            ProcessOutcome::Accepted
        );
        let mut object_cells = cells;
        object_cells[0] = 163;
        let mut object_target = ProcessRuntime::with_cell_data(
            &script,
            &world,
            ProcessCapabilities::default(),
            &object_cells,
        );
        assert_eq!(
            object_target.activate_at("link", [0, 0])[0].outcome,
            ProcessOutcome::Unsupported
        );
    }

    #[test]
    fn changing_an_empty_target_prevents_a_stale_noop() {
        let script = Script::parse("#proc1\n#action\nmove 0 0 256 0\n#end\n#proc2\n#action\nchange 0 0 163\n#end\n#links\nlink 0 0 1\nlink 2 0 2\n#end\n");
        let mut world = world();
        let cells = [0; 4096];
        let mut runtime = ProcessRuntime::with_capabilities(
            &script,
            &world,
            ProcessCapabilities {
                wall_elevation: false,
                initial_cell_types: Some(&cells),
            },
        );
        assert_eq!(
            runtime.activate_at("link", [2, 0])[0].outcome,
            ProcessOutcome::Accepted
        );
        world.player.position = [-10.0, -10.0];
        tick_for(&mut runtime, &mut world, 0.1);
        assert_eq!(runtime.cell_type([0, 0]), Some(163));
        assert_eq!(
            runtime.activate_at("link", [0, 0])[0].outcome,
            ProcessOutcome::Unsupported
        );
    }

    #[test]
    fn return_floor_reverses_in_progress_without_activation_event() {
        let script = Script::parse(
            "#proc1\n#action\nmove 1 0 256 0\n#end\n#links\nfloor 0 0 1\nreturnf 2 0 1\n#end\n",
        );
        let mut world = world();
        let mut runtime = ProcessRuntime::new(&script, &world);
        tick_for(&mut runtime, &mut world, 0.3);
        let opening = world.walls[0].a[0];
        assert!(opening > 1.2 && opening < 1.4);
        world.player.position = [2.5, 0.5];
        assert!(runtime.tick(&mut world, 1.0 / 60.0, false).is_empty());
        assert!(world.walls[0].a[0] < opening);
        tick_for(&mut runtime, &mut world, 0.4);
        assert!((world.walls[0].a[0] - 1.0).abs() < 0.001);
    }

    #[test]
    fn return_use_does_not_open_closed_process_and_cancels_start_delay() {
        let script = Script::parse("#proc1\nStartDelay 1\n#action\nmove 1 0 256 0\n#end\n#links\nlink 0 0 1\nreturn 2 0 1\n#end\n");
        let mut world = world();
        let mut runtime = ProcessRuntime::new(&script, &world);
        assert!(runtime.activate_at("return", [2, 0]).is_empty());
        assert_eq!(
            runtime.activate_at("link", [0, 0])[0].outcome,
            ProcessOutcome::Accepted
        );
        world.player.position = [2.5, 0.5];
        assert!(runtime.tick(&mut world, 1.0 / 60.0, true).is_empty());
        tick_for(&mut runtime, &mut world, 1.2);
        assert_eq!(world.walls[0].a[0], 1.0);
        assert_eq!(
            runtime.activate_at("link", [0, 0])[0].outcome,
            ProcessOutcome::Accepted
        );
    }

    #[test]
    fn locked_process_ignores_return_request() {
        let script = Script::parse("#proc1\n#action\nmove 1 0 256 0\n#end\n#proc2\n#action\nlock 1\n#end\n#links\nlink 0 0 1\nlink 3 0 2\nreturn 2 0 1\n#end\n");
        let mut world = world();
        world.player.position = [-10.0, -10.0];
        let mut runtime = ProcessRuntime::new(&script, &world);
        assert_eq!(
            runtime.activate_at("link", [0, 0])[0].outcome,
            ProcessOutcome::Accepted
        );
        tick_for(&mut runtime, &mut world, 0.2);
        assert_eq!(
            runtime.activate_at("link", [3, 0])[0].outcome,
            ProcessOutcome::Accepted
        );
        let before = world.walls[0].a[0];
        assert!(runtime.activate_at("return", [2, 0]).is_empty());
        tick_for(&mut runtime, &mut world, 0.1);
        assert!(world.walls[0].a[0] > before);
    }

    #[test]
    fn nonstop_hazard_starts_after_delay_and_stops_when_locked() {
        let script = Script::parse("#proc1\nLock\nStartDelay 0.5\n#action\nnonstop\ndeath 4 4 60 60 -1 128 100\n#end\n#proc2\n#action\nunlock 1\n#end\n#proc3\n#action\nlock 1\n#end\n#links\nfloor 0 0 1\nlink 1 0 2\nlink 2 0 3\n#end\n");
        let mut world = world();
        let cells = [0; 4096];
        let mut default =
            ProcessRuntime::with_cell_data(&script, &world, ProcessCapabilities::default(), &cells);
        default.activate_at("link", [1, 0]);
        assert_eq!(
            default.activate_at("floor", [0, 0])[0].outcome,
            ProcessOutcome::Unsupported
        );

        let mut runtime =
            ProcessRuntime::with_hazards(&script, &world, ProcessCapabilities::default(), &cells);
        assert_eq!(
            runtime.activate_at("floor", [0, 0])[0].outcome,
            ProcessOutcome::Locked
        );
        assert!(runtime.take_effects().is_empty());
        assert_eq!(
            runtime.activate_at("link", [1, 0])[0].outcome,
            ProcessOutcome::Accepted
        );
        assert_eq!(
            runtime.activate_at("floor", [0, 0])[0].outcome,
            ProcessOutcome::Accepted
        );
        assert!(runtime.take_effects().is_empty());
        world.player.position = [-10.0, -10.0];
        runtime.tick(&mut world, 0.25, false);
        assert!(runtime.take_effects().is_empty());
        runtime.tick(&mut world, 0.3, false);
        assert_eq!(
            runtime.take_effects(),
            vec![ProcessEffect::HazardActivated {
                process_id: 1,
                zones: vec![DeathZone {
                    min_cell: [4, 4],
                    max_cell: [60, 60],
                    z_bottom_raw: -1,
                    z_top_raw: 128,
                    damage_raw: 100
                }]
            }]
        );
        runtime.tick(&mut world, 2.0, false);
        assert!(
            runtime.take_effects().is_empty(),
            "nonstop survives the opening stage"
        );
        assert_eq!(
            runtime.activate_at("link", [2, 0])[0].outcome,
            ProcessOutcome::Accepted
        );
        assert_eq!(
            runtime.take_effects(),
            vec![ProcessEffect::HazardDeactivated { process_id: 1 }]
        );
        assert_eq!(
            runtime.activate_at("floor", [0, 0])[0].outcome,
            ProcessOutcome::Locked
        );
        assert!(runtime.take_effects().is_empty());
    }

    #[test]
    fn finite_hazard_stays_active_during_opening_then_deactivates() {
        let script = Script::parse(
            "#proc1\nSpeed 10\n#action\ndeath 1 2 3 4 0 24 500\n#end\n#links\nfloor 0 0 1\n#end\n",
        );
        let mut world = world();
        world.player.position = [-10.0, -10.0];
        let cells = [0; 4096];
        let mut runtime =
            ProcessRuntime::with_hazards(&script, &world, ProcessCapabilities::default(), &cells);
        assert_eq!(
            runtime.activate_at("floor", [0, 0])[0].outcome,
            ProcessOutcome::Accepted
        );
        assert!(
            matches!(&runtime.take_effects()[0], ProcessEffect::HazardActivated { process_id: 1, zones } if zones[0].damage_raw == 500)
        );
        runtime.tick(&mut world, 0.5, false);
        assert!(runtime.take_effects().is_empty());
        runtime.tick(&mut world, 0.5, false);
        assert_eq!(
            runtime.take_effects(),
            vec![ProcessEffect::HazardDeactivated { process_id: 1 }]
        );
    }

    #[test]
    fn malformed_and_unrelated_hazard_actions_do_not_emit_partial_effects() {
        for action in [
            "death 1 1 1 1 -1 128 -1",
            "death 4 4 1 1 0 128 100",
            "nonstop 1",
            "nonstop\nwind 1 1 2 2 5 5",
            "death 1 1 1 1 0 128 100\nquake 1 1 2 2 10",
        ] {
            let script = Script::parse(&format!(
                "#proc1\n#action\n{action}\n#end\n#links\nlink 0 0 1\n#end\n"
            ));
            let world = world();
            let mut runtime = ProcessRuntime::with_hazards(
                &script,
                &world,
                ProcessCapabilities::default(),
                &[0; 4096],
            );
            assert_eq!(
                runtime.activate_at("link", [0, 0])[0].outcome,
                ProcessOutcome::Unsupported,
                "{action}"
            );
            assert!(runtime.take_effects().is_empty());
        }
    }
}
