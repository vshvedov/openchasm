//! Experimental subset of level process behavior, independent of rendering.
//!
//! Supported: floor and nearby use links, wall `move` / `rotate`, `StartDelay`, `BackWait`,
//! `Speed`, `waitout`, and initially locked/key-gated processes. Processes with other
//! state-changing actions are disabled in full. Movement timing, use range, and
//! behavior in the presence of blockers are placeholders, not verified
//! against the original game.
use crate::ambient::AmbientRegion;
use crate::light_sources::SourceLight;
use crate::object_states::ObjectAction;
use crate::process::{Process, Script};
use crate::quake::QuakeZone;
use crate::wind::WindZone;
use crate::world::World;
use std::collections::{BTreeMap, BTreeSet};

const USE_RANGE: f32 = 1.25;
const MAP_UNITS_PER_TILE: f32 = 256.0;

#[derive(Clone, Debug)]
struct Motion {
    wall: usize,
    kind: MotionKind,
    speed_override: Option<f32>,
    duration: f32,
    progress: f32,
}
#[derive(Clone, Debug)]
enum MotionKind {
    Translate([f32; 2]),
    AxisTranslate {
        displacement: [f32; 2],
        x_first: bool,
    },
    Rotate {
        pivot: [f32; 2],
        radians: f32,
    },
    /// Raw process height units; projection and collision are handled by host.
    Vertical(i32),
}
impl Motion {
    fn transform(&self, point: [f32; 2], from: f32, to: f32) -> [f32; 2] {
        let progress_delta = to - from;
        match self.kind {
            MotionKind::AxisTranslate {
                displacement,
                x_first,
            } => {
                let stage = |p: f32| {
                    let first = (p * 2.0).min(1.0);
                    let second = (p * 2.0 - 1.0).max(0.0);
                    if x_first {
                        [first, second]
                    } else {
                        [second, first]
                    }
                };
                let a = stage(from);
                let b = stage(to);
                [
                    point[0] + displacement[0] * (b[0] - a[0]),
                    point[1] + displacement[1] * (b[1] - a[1]),
                ]
            }
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
            MotionKind::AxisTranslate {
                displacement: d, ..
            } => d[0].abs() + d[1].abs(),
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
fn obstructed(motions: &[Motion], world: &World, elapsed: f32, direction: f32) -> bool {
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
        let from = motion.progress;
        let to = (from + direction * elapsed / motion.duration).clamp(0.0, 1.0);
        let travel = motion.travel_bound(wall)
            * (to - from).abs()
            * if matches!(motion.kind, MotionKind::AxisTranslate { .. }) {
                2.0
            } else {
                1.0
            };
        if !travel.is_finite() || travel > SAMPLE_STEP * MAX_SAMPLES as f32 {
            return true;
        }
        let count = ((travel / SAMPLE_STEP).ceil() as usize).max(1);
        let mut previous = distance_to_segment(world.player.position, wall.a, wall.b);
        for sample in 1..=count {
            let progress = from + (to - from) * sample as f32 / count as f32;
            let distance = distance_to_segment(
                world.player.position,
                motion.transform(wall.a, from, progress),
                motion.transform(wall.b, from, progress),
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
impl std::fmt::Display for ProcessObjectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid => f.write_str("invalid scripted object pose or cell"),
            Self::DuplicateCell => f.write_str("duplicate scripted object cell"),
        }
    }
}
impl std::error::Error for ProcessObjectError {}
#[derive(Debug, Clone)]
struct ObjectMotion {
    cell: [u8; 2],
    raw_type: u8,
    kind: MotionKind,
    speed_override: Option<f32>,
    duration: f32,
    progress: f32,
}
impl ObjectMotion {
    fn proxy(&self) -> Motion {
        Motion {
            wall: 0,
            kind: self.kind.clone(),
            speed_override: self.speed_override,
            duration: self.duration,
            progress: self.progress,
        }
    }
    fn travel_bound(&self, pose: &ProcessObject) -> f32 {
        match self.kind {
            MotionKind::Translate(d) => d[0].hypot(d[1]),
            MotionKind::AxisTranslate {
                displacement: d, ..
            } => d[0].abs() + d[1].abs(),
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
    elapsed: f32,
    direction: f32,
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
        let from = motion.progress;
        let to = (from + direction * elapsed / motion.duration).clamp(0.0, 1.0);
        let travel = motion.travel_bound(object)
            * (to - from).abs()
            * if matches!(motion.kind, MotionKind::AxisTranslate { .. }) {
                2.0
            } else {
                1.0
            };
        if !travel.is_finite() || travel > SAMPLE_STEP * MAX_SAMPLES as f32 {
            return true;
        }
        let count = ((travel / SAMPLE_STEP).ceil() as usize).max(1);
        let radius = PLAYER_RADIUS + object.blocking_radius;
        let mut previous = (object.position[0] - world.player.position[0])
            .hypot(object.position[1] - world.player.position[1]);
        for sample in 1..=count {
            let pos = motion.proxy().transform(
                object.position,
                from,
                from + (to - from) * sample as f32 / count as f32,
            );
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
    WallElevation {
        wall_index: usize,
        offset_raw: f32,
    },
    /// New raw type for the grid cell; includes original type on reversal.
    CellType {
        cell: [u8; 2],
        raw_type: u8,
    },
    /// Absolute pose, keyed by the original stable map cell.
    ObjectPose {
        cell: [u8; 2],
        position: [f32; 2],
        angle_radians: f32,
        elevation_raw: f32,
    },
    /// Presentation request; no object animation is simulated by the runtime.
    PlayAnimation {
        object_id: u16,
    },
    /// The host must activate all zones under this process ID. Zones use
    /// inclusive map cells and raw vertical/damage units.
    HazardActivated {
        process_id: u16,
        zones: Vec<DeathZone>,
    },
    /// Remove the active hazards belonging to this process ID.
    HazardDeactivated {
        process_id: u16,
    },
    /// Activate the validated signed wind rectangles for this process.
    WindActivated {
        process_id: u16,
        zones: Vec<WindZone>,
    },
    /// Remove the wind rectangles owned by this process.
    WindDeactivated {
        process_id: u16,
    },
    SourceLightsActivated {
        process_id: u16,
        sources: Vec<SourceLight>,
    },
    SourceLightsDeactivated {
        process_id: u16,
    },
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

/// Ordered commands that require host validation before process acceptance.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ImmediateAction {
    Object(ObjectAction),
    Quake(QuakeZone),
    Ambient(AmbientRegion),
}

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
    wind_zones: Vec<WindZone>,
    sources: Vec<SourceLight>,
    immediate_actions: Vec<ImmediateAction>,
    nonstop: bool,
    hazard_active: bool,
    wind_active: bool,
    source_active: bool,
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
        modes: (bool, bool, bool),
    ) -> Self {
        let (wind_enabled, immediate_enabled, source_enabled) = modes;
        let mut motions = Vec::new();
        let mut object_motions = Vec::new();
        let mut empty_motion_cells = Vec::new();
        let mut empty_motion_distance = 0.0_f32;
        let mut empty_speed_overrides = Vec::new();
        let mut cell_changes = Vec::new();
        let mut animations = Vec::new();
        let mut death_zones = Vec::new();
        let mut wind_zones = Vec::new();
        let mut sources = Vec::new();
        let mut immediate_actions = Vec::new();
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
                if (hazards_enabled || wind_enabled) && action.arguments.is_empty() && !nonstop {
                    nonstop = true;
                    continue;
                }
                supported = false;
                break;
            }
            if action.name == "source" {
                if !source_enabled {
                    supported = false;
                    break;
                }
                match SourceLight::parse(&action.arguments) {
                    Ok(source) => sources.push(source),
                    Err(_) => {
                        supported = false;
                        break;
                    }
                }
                continue;
            }
            if action.name == "wind" {
                let parsed = (|| {
                    if !wind_enabled || !(6..=7).contains(&action.arguments.len()) {
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
                    if x1 > x2
                        || y1 > y2
                        || x2 >= 64
                        || y2 >= 64
                        || !(-128..=127).contains(&values[4])
                        || !(-128..=127).contains(&values[5])
                    {
                        return None;
                    }
                    let actor_only = match values.get(6).copied().unwrap_or(0) {
                        0 => false,
                        1 => true,
                        _ => return None,
                    };
                    Some(WindZone {
                        actor_only,
                        min_cell: [x1, y1],
                        max_cell: [x2, y2],
                        direction: [values[4] as i16, values[5] as i16],
                    })
                })();
                if let Some(zone) = parsed {
                    wind_zones.push(zone);
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
            if immediate_enabled && action.name == "explode" {
                let args = &action.arguments;
                let parsed = (|| {
                    if args.len() != 2 {
                        return None;
                    }
                    let x = args[0].parse::<usize>().ok()?;
                    let y = args[1].parse::<usize>().ok()?;
                    if x >= 64 || y >= 64 {
                        return None;
                    }
                    Some(ObjectAction::Explode { cell: [x, y] })
                })();
                if let Some(object) = parsed {
                    immediate_actions.push(ImmediateAction::Object(object));
                    continue;
                }
                supported = false;
                break;
            }
            if immediate_enabled && action.name == "ambient" {
                let parsed = (|| {
                    if action.arguments.len() != 5 {
                        return None;
                    }
                    let values: Vec<i16> = action
                        .arguments
                        .iter()
                        .map(|v| v.parse().ok())
                        .collect::<Option<_>>()?;
                    let sound_id = u8::try_from(values[4]).ok().filter(|id| *id < 32)?;
                    Some(AmbientRegion {
                        min_cell: [values[0], values[1]],
                        max_cell: [values[2], values[3]],
                        sound_id,
                    })
                })();
                if let Some(region) = parsed {
                    immediate_actions.push(ImmediateAction::Ambient(region));
                    continue;
                }
                supported = false;
                break;
            }
            if immediate_enabled && action.name == "quake" {
                let args = &action.arguments;
                let parsed = (|| {
                    if args.len() != 5 {
                        return None;
                    }
                    let x1 = args[0].parse::<u8>().ok()?;
                    let y1 = args[1].parse::<u8>().ok()?;
                    let x2 = args[2].parse::<u8>().ok()?;
                    let y2 = args[3].parse::<u8>().ok()?;
                    let power = args[4].parse::<u32>().ok()?;
                    if x1 > x2 || y1 > y2 || x2 >= 64 || y2 >= 64 {
                        return None;
                    }
                    Some(QuakeZone {
                        min_cell: [x1, y1],
                        max_cell: [x2, y2],
                        power,
                    })
                })();
                if let Some(zone) = parsed {
                    immediate_actions.push(ImmediateAction::Quake(zone));
                    continue;
                }
                supported = false;
                break;
            }
            if immediate_enabled && action.name == "change" {
                let args = &action.arguments;
                let parsed = (|| {
                    if args.len() != 3 {
                        return None;
                    }
                    let x = args[0].parse::<usize>().ok()?;
                    let y = args[1].parse::<usize>().ok()?;
                    let raw_type = args[2].parse::<u8>().ok()?;
                    if x >= 64 || y >= 64 || !(163..=226).contains(&raw_type) {
                        return None;
                    }
                    Some(ObjectAction::Change {
                        cell: [x, y],
                        raw_type,
                    })
                })();
                if let Some(object) = parsed {
                    immediate_actions.push(ImmediateAction::Object(object));
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
                "move" | "xmove" | "ymove" => 4,
                "rotate" => 5,
                "up" if capabilities.wall_elevation || objects.is_some() => 3,
                _ => {
                    supported = false;
                    break;
                }
            };
            let speed_override = if action.arguments.len() == expected_arguments {
                None
            } else if action.arguments.len() == expected_arguments + 1 {
                let suffix = &action.arguments[expected_arguments];
                // The remaster stores the fifth numeric move argument, but its
                // opcode-4 motion and lifecycle paths do not read that field.
                if action.name == "move" && suffix.parse::<i16>().is_ok() {
                    None
                } else {
                    let value = suffix
                        .strip_prefix("s:")
                        .and_then(|value| value.parse::<f32>().ok())
                        .filter(|value| value.is_finite() && *value > 0.0);
                    if value.is_none() {
                        supported = false;
                        break;
                    }
                    value
                }
            } else {
                supported = false;
                break;
            };
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
                let displacement = [
                    dx as f32 / MAP_UNITS_PER_TILE,
                    dy as f32 / MAP_UNITS_PER_TILE,
                ];
                if action.name == "move" {
                    MotionKind::Translate(displacement)
                } else {
                    MotionKind::AxisTranslate {
                        displacement,
                        x_first: action.name == "xmove",
                    }
                }
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
                    speed_override,
                    duration: 0.0,
                    progress: 0.0,
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
                        speed_override,
                        duration: 0.0,
                        progress: 0.0,
                    });
                } else if raw_cells.is_some_and(|cells| cells[index] == 0) {
                    empty_motion_cells.push([cell.0, cell.1]);
                    empty_speed_overrides.push(speed_override);
                    let distance = match kind {
                        MotionKind::Translate(d) => d[0].hypot(d[1]),
                        MotionKind::AxisTranslate {
                            displacement: d, ..
                        } => d[0].abs() + d[1].abs(),
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
        if nonstop
            && death_zones.is_empty()
            && wind_zones.is_empty()
            && sources.is_empty()
            && immediate_actions.is_empty()
        {
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
            wind_zones.clear();
            sources.clear();
            immediate_actions.clear();
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
        let base_duration = if death_zones.is_empty() && wind_zones.is_empty() {
            (max_distance / speed).max(0.001)
        } else {
            10.0 / property_float(process, "speed", 10.0).max(0.01)
        };
        let base_speed = speed * 8.0;
        let mut duration = if motions.is_empty()
            && object_motions.is_empty()
            && empty_speed_overrides.is_empty()
        {
            base_duration
        } else {
            0.0_f32
        };
        for override_speed in empty_speed_overrides {
            duration =
                duration.max(base_duration * base_speed / override_speed.unwrap_or(base_speed));
        }
        for motion in &mut motions {
            motion.duration =
                base_duration * base_speed / motion.speed_override.unwrap_or(base_speed);
            duration = duration.max(motion.duration);
        }
        for motion in &mut object_motions {
            motion.duration =
                base_duration * base_speed / motion.speed_override.unwrap_or(base_speed);
            duration = duration.max(motion.duration);
        }
        if !duration.is_finite()
            || duration <= 0.0
            || motions
                .iter()
                .any(|m| !m.duration.is_finite() || m.duration <= 0.0)
            || object_motions
                .iter()
                .any(|m| !m.duration.is_finite() || m.duration <= 0.0)
        {
            supported = false;
        }
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
            wind_zones,
            sources,
            immediate_actions,
            nonstop,
            hazard_active: false,
            wind_active: false,
            source_active: false,
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
                && self.wind_zones.is_empty()
                && self.sources.is_empty()
                && self.immediate_actions.is_empty()
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
                    || !self.wind_zones.is_empty()
                    || !self.sources.is_empty()
                    || (self.delay > 0.0 && !self.immediate_actions.is_empty())
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
                    || !self.death_zones.is_empty()
                    || !self.wind_zones.is_empty()
                    || !self.sources.is_empty() =>
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
        if !self.locked && !self.wind_active && !self.wind_zones.is_empty() {
            self.wind_active = true;
            effects.push(ProcessEffect::WindActivated {
                process_id,
                zones: self.wind_zones.clone(),
            });
        }
        if !self.locked && !self.source_active && !self.sources.is_empty() {
            self.source_active = true;
            effects.push(ProcessEffect::SourceLightsActivated {
                process_id,
                sources: self.sources.clone(),
            });
        }
    }
    fn stop_sources(&mut self, process_id: u16, effects: &mut Vec<ProcessEffect>) {
        if self.source_active {
            self.source_active = false;
            effects.push(ProcessEffect::SourceLightsDeactivated { process_id });
        }
    }
    fn stop_hazard(&mut self, process_id: u16, effects: &mut Vec<ProcessEffect>) {
        if self.hazard_active {
            self.hazard_active = false;
            effects.push(ProcessEffect::HazardDeactivated { process_id });
        }
        if self.wind_active {
            self.wind_active = false;
            effects.push(ProcessEffect::WindDeactivated { process_id });
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
                self.stop_sources(process_id, effects);
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
                    if !self.immediate_actions.is_empty() {
                        self.phase = Phase::Delay(0.0);
                        return;
                    }
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
                    self.stop_sources(process_id, effects);
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
            let elapsed = change.abs() * self.duration;
            let direction = change.signum();
            if obstructed(&self.motions, world, elapsed, direction)
                || objects_obstructed(&self.object_motions, world, objects, elapsed, direction)
            {
                self.progress = previous;
                self.phase = if change > 0.0 {
                    Phase::Opening
                } else {
                    Phase::Closing
                };
                return;
            }
            for motion in &mut self.motions {
                let from = motion.progress;
                let to = (from + direction * elapsed / motion.duration).clamp(0.0, 1.0);
                let motion_change = to - from;
                match motion.kind {
                    MotionKind::Vertical(raw) => {
                        if let Some(offset) = elevations.get_mut(motion.wall) {
                            *offset += raw as f32 * motion_change;
                            effects.push(ProcessEffect::WallElevation {
                                wall_index: motion.wall,
                                offset_raw: *offset,
                            });
                        }
                    }
                    _ => {
                        if let Some(wall) = world.walls.get_mut(motion.wall) {
                            wall.a = motion.transform(wall.a, from, to);
                            wall.b = motion.transform(wall.b, from, to);
                        }
                    }
                }
                motion.progress = to;
            }
            for motion in &mut self.object_motions {
                let from = motion.progress;
                let to = (from + direction * elapsed / motion.duration).clamp(0.0, 1.0);
                let motion_change = to - from;
                if let Some(object) = objects.get_mut(&motion.cell) {
                    match motion.kind {
                        MotionKind::Vertical(raw) => {
                            object.elevation_raw += raw as f32 * motion_change
                        }
                        MotionKind::Translate(_) | MotionKind::AxisTranslate { .. } => {
                            object.position = motion.proxy().transform(object.position, from, to)
                        }
                        MotionKind::Rotate { radians, .. } => {
                            object.position = motion.proxy().transform(object.position, from, to);
                            object.angle_radians += radians * motion_change;
                        }
                    }
                    motion.progress = to;
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

/// One floor, use, or reverse command selected for a simulation tick.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessCommand {
    pub kind: String,
    pub cell: [u8; 2],
    pub process_id: u16,
}

/// Identify walls not referenced by any potentially geometry-changing script
/// action. Unknown action names or ambiguous coordinates make every wall
/// unverified; runtime action support does not affect this classification.
fn static_wall_mask(script: &Script, world: &World) -> Vec<bool> {
    let mut targets = BTreeSet::new();
    let mut unknown = !script.diagnostics.is_empty();
    for action in script
        .processes
        .values()
        .flat_map(|process| &process.actions)
    {
        match action.name.as_str() {
            "move" | "xmove" | "ymove" | "rotate" | "up" | "change" | "explode" => {
                let cell = action
                    .arguments
                    .first()
                    .and_then(|value| value.parse::<u8>().ok())
                    .zip(
                        action
                            .arguments
                            .get(1)
                            .and_then(|value| value.parse::<u8>().ok()),
                    );
                match cell {
                    Some((x, y)) if x < 64 && y < 64 => {
                        targets.insert([x, y]);
                    }
                    _ => unknown = true,
                }
            }
            // These known commands do not transform or replace walls.
            "lock" | "unlock" | "playani" | "stopani" | "light" | "death" | "quake" | "ambient"
            | "wind" | "source" | "waitout" | "nonstop" => {}
            _ => unknown = true,
        }
    }
    world
        .walls
        .iter()
        .map(|wall| {
            !unknown
                && wall
                    .a
                    .iter()
                    .chain(wall.b.iter())
                    .all(|value| value.is_finite())
                && wall
                    .cell
                    .is_some_and(|cell| cell[0] < 64 && cell[1] < 64 && !targets.contains(&cell))
        })
        .collect()
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
    static_walls: Vec<bool>,
    objects: BTreeMap<[u8; 2], ProcessObject>,
    cell_types: Option<Vec<u8>>,
    effects: Vec<ProcessEffect>,
    staged_authorized: bool,
    source_enabled: bool,
}

/// Durable process phase; delay is measured in seconds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ProcessPhaseSnapshot {
    Closed,
    Delay(f32),
    Opening,
    Open,
    Closing,
}
#[derive(Debug, Clone, PartialEq)]
pub struct ProcessEntrySnapshot {
    pub id: u16,
    pub phase: ProcessPhaseSnapshot,
    pub progress: f32,
    pub open_time: f32,
    pub locked: bool,
    pub hazard_active: bool,
    pub wind_active: bool,
    pub wall_progress: Vec<f32>,
    pub object_progress: Vec<f32>,
    pub previous_cell_types: Vec<Option<u8>>,
}
#[derive(Debug, Clone, PartialEq)]
pub struct ProcessRuntimeSnapshot {
    pub processes: Vec<ProcessEntrySnapshot>,
    pub previous_floor: Option<[u8; 2]>,
    pub keys: [bool; 3],
    pub wall_identity: Vec<(Option<[u8; 2]>, u8)>,
    pub elevations: Vec<f32>,
    pub objects: Vec<ProcessObject>,
    pub cell_types: Option<Vec<u8>>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessSnapshotError {
    ActiveSourceLights,
    PendingEffects,
    WallIdentity,
    ProcessIdentity,
    ObjectIdentity,
    CellTypes,
    InvalidValue,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessGeometryError {
    Identity,
    InvalidValue,
    PoseMismatch,
    AmbiguousRotation,
}

impl ProcessRuntime {
    /// Verify restored process motion against a fresh runtime/world baseline.
    /// Translations commute; a rotation sharing its target with any other
    /// horizontal motion has history-dependent order and is rejected.
    pub fn validate_snapshot_geometry(
        &self,
        baseline: &ProcessRuntime,
        base_world: &World,
        saved_world: &World,
    ) -> Result<(), ProcessGeometryError> {
        const EPS: f32 = 0.002;
        let near = |a: f32, b: f32| a.is_finite() && b.is_finite() && (a - b).abs() <= EPS;
        for process in self.processes.values() {
            let motion_progress = process
                .motions
                .iter()
                .map(|m| m.progress)
                .chain(process.object_motions.iter().map(|m| m.progress));
            match process.phase {
                Phase::Closed | Phase::Delay(_)
                    if process.progress != 0.0 || motion_progress.clone().any(|p| p != 0.0) =>
                {
                    return Err(ProcessGeometryError::InvalidValue)
                }
                Phase::Open
                    if process.progress != 1.0 || motion_progress.clone().any(|p| p != 1.0) =>
                {
                    return Err(ProcessGeometryError::InvalidValue)
                }
                _ => {}
            }
        }
        if base_world.walls.len() != saved_world.walls.len()
            || self.elevations.len() != base_world.walls.len()
            || baseline.elevations.len() != base_world.walls.len()
        {
            return Err(ProcessGeometryError::Identity);
        }
        for (index, (base, saved)) in base_world.walls.iter().zip(&saved_world.walls).enumerate() {
            let motions: Vec<_> = self
                .processes
                .values()
                .flat_map(|p| &p.motions)
                .filter(|m| m.wall == index)
                .collect();
            let horizontal: Vec<_> = motions
                .iter()
                .copied()
                .filter(|m| !matches!(m.kind, MotionKind::Vertical(_)))
                .collect();
            if horizontal.len() > 1
                && horizontal
                    .iter()
                    .any(|m| matches!(m.kind, MotionKind::Rotate { .. }))
                && (horizontal.iter().any(|m| m.progress != 0.0)
                    || !near(base.a[0], saved.a[0])
                    || !near(base.a[1], saved.a[1])
                    || !near(base.b[0], saved.b[0])
                    || !near(base.b[1], saved.b[1]))
            {
                return Err(ProcessGeometryError::AmbiguousRotation);
            }
            let mut a = base.a;
            let mut b = base.b;
            let mut elevation = baseline.elevations[index];
            for motion in motions {
                if !motion.progress.is_finite() || !(0.0..=1.0).contains(&motion.progress) {
                    return Err(ProcessGeometryError::InvalidValue);
                }
                match motion.kind {
                    MotionKind::Vertical(raw) => elevation += raw as f32 * motion.progress,
                    _ => {
                        a = motion.transform(a, 0.0, motion.progress);
                        b = motion.transform(b, 0.0, motion.progress);
                    }
                }
            }
            if !near(a[0], saved.a[0])
                || !near(a[1], saved.a[1])
                || !near(b[0], saved.b[0])
                || !near(b[1], saved.b[1])
                || !near(elevation, self.elevations[index])
            {
                return Err(ProcessGeometryError::PoseMismatch);
            }
        }
        for (&cell, object) in &self.objects {
            let base = baseline
                .objects
                .get(&cell)
                .ok_or(ProcessGeometryError::Identity)?;
            let motions: Vec<_> = self
                .processes
                .values()
                .flat_map(|p| &p.object_motions)
                .filter(|m| m.cell == cell)
                .collect();
            let horizontal: Vec<_> = motions
                .iter()
                .copied()
                .filter(|m| !matches!(m.kind, MotionKind::Vertical(_)))
                .collect();
            if horizontal.len() > 1
                && horizontal
                    .iter()
                    .any(|m| matches!(m.kind, MotionKind::Rotate { .. }))
                && (horizontal.iter().any(|m| m.progress != 0.0)
                    || !near(base.position[0], object.position[0])
                    || !near(base.position[1], object.position[1])
                    || !near(base.angle_radians, object.angle_radians))
            {
                return Err(ProcessGeometryError::AmbiguousRotation);
            }
            let mut position = base.position;
            let mut angle = base.angle_radians;
            let mut elevation = base.elevation_raw;
            for motion in motions {
                if !motion.progress.is_finite() || !(0.0..=1.0).contains(&motion.progress) {
                    return Err(ProcessGeometryError::InvalidValue);
                }
                match motion.kind {
                    MotionKind::Vertical(raw) => elevation += raw as f32 * motion.progress,
                    MotionKind::Rotate { radians, .. } => {
                        position = motion.proxy().transform(position, 0.0, motion.progress);
                        angle += radians * motion.progress;
                    }
                    _ => position = motion.proxy().transform(position, 0.0, motion.progress),
                }
            }
            if !near(position[0], object.position[0])
                || !near(position[1], object.position[1])
                || !near(angle, object.angle_radians)
                || !near(elevation, object.elevation_raw)
            {
                return Err(ProcessGeometryError::PoseMismatch);
            }
        }
        Ok(())
    }
    /// Trusted commands compiled from the fingerprint-verified level script.
    pub(crate) fn journal_actions(&self, id: u16) -> Option<&[ImmediateAction]> {
        self.processes
            .get(&id)
            .filter(|p| p.supported)
            .map(|p| p.immediate_actions.as_slice())
    }
    pub(crate) fn journal_wind(&self, id: u16) -> Option<&[WindZone]> {
        self.processes
            .get(&id)
            .filter(|p| p.supported && !p.wind_zones.is_empty())
            .map(|p| p.wind_zones.as_slice())
    }
    /// Validate host hazard/wind memberships and zones against compiled script
    /// state. This does not reconstruct the history-dependent wind grids.
    pub fn snapshot_effects_match(
        &self,
        hazards: &crate::hazards::HazardsSnapshot,
        wind: &crate::wind::WindSnapshot,
    ) -> bool {
        let active_hazards: Vec<_> = self
            .processes
            .iter()
            .filter(|(_, p)| p.hazard_active)
            .map(|(&id, p)| (id, &p.death_zones))
            .collect();
        let active_wind: Vec<_> = self
            .processes
            .iter()
            .filter(|(_, p)| p.wind_active)
            .map(|(&id, p)| (id, &p.wind_zones))
            .collect();
        active_hazards.len() == hazards.active.len()
            && active_hazards.iter().all(|(id, zones)| {
                hazards
                    .active
                    .iter()
                    .any(|h| h.process_id == *id && &h.zones == *zones)
            })
            && active_wind.len() == wind.active.len()
            && active_wind.iter().all(|(id, zones)| {
                wind.active
                    .iter()
                    .any(|(saved_id, saved_zones)| saved_id == id && saved_zones == *zones)
            })
    }
    /// Capture only after the host has consumed all process effects.
    pub fn snapshot(&self, world: &World) -> Result<ProcessRuntimeSnapshot, ProcessSnapshotError> {
        if !self.effects.is_empty() || self.staged_authorized {
            return Err(ProcessSnapshotError::PendingEffects);
        }
        if self
            .processes
            .values()
            .any(|p| !p.sources.is_empty() && !matches!(p.phase, Phase::Closed | Phase::Delay(_)))
        {
            return Err(ProcessSnapshotError::ActiveSourceLights);
        }
        if world.walls.len() != self.elevations.len() {
            return Err(ProcessSnapshotError::WallIdentity);
        }
        Ok(ProcessRuntimeSnapshot {
            processes: self
                .processes
                .iter()
                .map(|(&id, p)| ProcessEntrySnapshot {
                    id,
                    phase: match p.phase {
                        Phase::Closed => ProcessPhaseSnapshot::Closed,
                        Phase::Delay(t) => ProcessPhaseSnapshot::Delay(t),
                        Phase::Opening => ProcessPhaseSnapshot::Opening,
                        Phase::Open => ProcessPhaseSnapshot::Open,
                        Phase::Closing => ProcessPhaseSnapshot::Closing,
                    },
                    progress: p.progress,
                    open_time: p.open_time,
                    locked: p.locked,
                    hazard_active: p.hazard_active,
                    wind_active: p.wind_active,
                    wall_progress: p.motions.iter().map(|m| m.progress).collect(),
                    object_progress: p.object_motions.iter().map(|m| m.progress).collect(),
                    previous_cell_types: p.cell_changes.iter().map(|c| c.previous).collect(),
                })
                .collect(),
            previous_floor: self.previous_floor,
            keys: self.keys,
            wall_identity: world.walls.iter().map(|w| (w.cell, w.texture)).collect(),
            elevations: self.elevations.clone(),
            objects: self.objects.values().copied().collect(),
            cell_types: self.cell_types.clone(),
        })
    }

    /// Restore into a freshly constructed runtime for the same level and script.
    /// World geometry, hazard/wind fields and object/combat state are separate
    /// components and must be restored atomically by the owning simulation.
    pub fn restore(
        &mut self,
        snapshot: &ProcessRuntimeSnapshot,
        world: &World,
    ) -> Result<(), ProcessSnapshotError> {
        if !self.effects.is_empty() || self.staged_authorized {
            return Err(ProcessSnapshotError::PendingEffects);
        }
        if snapshot.wall_identity.len() != world.walls.len()
            || snapshot.elevations.len() != world.walls.len()
            || self.elevations.len() != world.walls.len()
            || snapshot
                .wall_identity
                .iter()
                .zip(&world.walls)
                .any(|(id, w)| *id != (w.cell, w.texture))
        {
            return Err(ProcessSnapshotError::WallIdentity);
        }
        if snapshot.processes.len() != self.processes.len() {
            return Err(ProcessSnapshotError::ProcessIdentity);
        }
        if snapshot.cell_types.as_ref().map(Vec::len) != self.cell_types.as_ref().map(Vec::len) {
            return Err(ProcessSnapshotError::CellTypes);
        }
        let valid_progress = |v: f32| v.is_finite() && (0.0..=1.0).contains(&v);
        if snapshot
            .previous_floor
            .is_some_and(|c| c[0] >= 64 || c[1] >= 64)
            || snapshot
                .elevations
                .iter()
                .any(|v| !v.is_finite() || v.abs() > 65536.0)
        {
            return Err(ProcessSnapshotError::InvalidValue);
        }
        let mut staged = self.clone();
        let mut seen = BTreeSet::new();
        for entry in &snapshot.processes {
            if !seen.insert(entry.id) {
                return Err(ProcessSnapshotError::ProcessIdentity);
            }
            let p = staged
                .processes
                .get_mut(&entry.id)
                .ok_or(ProcessSnapshotError::ProcessIdentity)?;
            if !p.sources.is_empty()
                && !matches!(
                    entry.phase,
                    ProcessPhaseSnapshot::Closed | ProcessPhaseSnapshot::Delay(_)
                )
            {
                return Err(ProcessSnapshotError::ActiveSourceLights);
            }
            if entry.wall_progress.len() != p.motions.len()
                || entry.object_progress.len() != p.object_motions.len()
                || entry.previous_cell_types.len() != p.cell_changes.len()
            {
                return Err(ProcessSnapshotError::ProcessIdentity);
            }
            if !valid_progress(entry.progress)
                || !entry.open_time.is_finite()
                || entry.open_time < 0.0
                || entry
                    .wall_progress
                    .iter()
                    .chain(entry.object_progress.iter())
                    .any(|&v| !valid_progress(v))
                || matches!(entry.phase, ProcessPhaseSnapshot::Delay(t) if !t.is_finite() || t < 0.0 || t > p.delay)
                || (entry.hazard_active && p.death_zones.is_empty())
                || (entry.wind_active && p.wind_zones.is_empty())
            {
                return Err(ProcessSnapshotError::InvalidValue);
            }
            p.phase = match entry.phase {
                ProcessPhaseSnapshot::Closed => Phase::Closed,
                ProcessPhaseSnapshot::Delay(t) => Phase::Delay(t),
                ProcessPhaseSnapshot::Opening => Phase::Opening,
                ProcessPhaseSnapshot::Open => Phase::Open,
                ProcessPhaseSnapshot::Closing => Phase::Closing,
            };
            p.progress = entry.progress;
            p.open_time = entry.open_time;
            p.locked = entry.locked;
            p.hazard_active = entry.hazard_active;
            p.wind_active = entry.wind_active;
            for (m, &v) in p.motions.iter_mut().zip(&entry.wall_progress) {
                m.progress = v;
            }
            for (m, &v) in p.object_motions.iter_mut().zip(&entry.object_progress) {
                m.progress = v;
            }
            for (c, &v) in p.cell_changes.iter_mut().zip(&entry.previous_cell_types) {
                c.previous = v;
            }
        }
        let mut objects = BTreeMap::new();
        for object in &snapshot.objects {
            if !self.objects.contains_key(&object.cell)
                || objects.insert(object.cell, *object).is_some()
            {
                return Err(ProcessSnapshotError::ObjectIdentity);
            }
            if object.cell.iter().any(|&v| v >= 64)
                || object
                    .position
                    .iter()
                    .any(|v| !v.is_finite() || v.abs() > 4096.0)
                || !object.angle_radians.is_finite()
                || !object.elevation_raw.is_finite()
                || !object.blocking_radius.is_finite()
                || !(0.0..=64.0).contains(&object.blocking_radius)
            {
                return Err(ProcessSnapshotError::InvalidValue);
            }
        }
        staged.previous_floor = snapshot.previous_floor;
        staged.keys = snapshot.keys;
        staged.elevations.clone_from(&snapshot.elevations);
        staged.objects = objects;
        staged.cell_types.clone_from(&snapshot.cell_types);
        *self = staged;
        Ok(())
    }
}

/// One validated replacement of a stable source map cell.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RuntimeObjectTransition {
    pub cell: [u8; 2],
    pub previous_type: u8,
    pub new_type: u8,
    pub blocking_radius: f32,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuntimeObjectSyncError {
    ActionMismatch,
    MissingCellTypes,
    MissingObject,
    InvalidTransition,
    StaleType,
    ActiveMotion,
}

/// A process activation staged on a private copy. Dropping it rolls back;
/// commit publishes the staged runtime and its effects. The exclusive borrow
/// prevents intervening runtime changes while the host validates the plan.
pub struct PreparedProcess<'a> {
    runtime: &'a mut ProcessRuntime,
    staged: ProcessRuntime,
    actions: Vec<ImmediateAction>,
    object_sync_complete: bool,
    ambient_sync_complete: bool,
    scheduled: Option<ProcessRuntime>,
    event: ProcessEvent,
}
impl PreparedProcess<'_> {
    pub fn is_delayed(&self) -> bool {
        self.scheduled.is_some()
    }

    /// Publish only the already-validated schedule, not staged target edits.
    pub(crate) fn commit_schedule(self) -> ProcessEvent {
        if !self.object_sync_complete
            && self
                .actions
                .iter()
                .any(|a| matches!(a, ImmediateAction::Object(_)))
            || !self.ambient_sync_complete
                && self
                    .actions
                    .iter()
                    .any(|a| matches!(a, ImmediateAction::Ambient(_)))
        {
            return ProcessEvent {
                process_id: self.event.process_id,
                outcome: ProcessOutcome::Unsupported,
            };
        }
        let Some(scheduled) = self.scheduled else {
            return ProcessEvent {
                process_id: self.event.process_id,
                outcome: ProcessOutcome::Unsupported,
            };
        };
        *self.runtime = scheduled;
        self.event
    }
    pub(crate) fn mark_ambient_staged(&mut self) {
        self.ambient_sync_complete = true;
    }
    pub fn actions(&self) -> &[ImmediateAction] {
        &self.actions
    }
    /// Pose at the time of preparation, used for collision preflight.
    pub fn object_pose(&self, cell: [u8; 2]) -> Option<ProcessObject> {
        self.staged.object_pose(cell)
    }
    /// Validate and stage the complete ordered object batch. Pose is preserved;
    /// the caller must separately validate collision overlap and presentation.
    pub fn stage_object_transitions(
        &mut self,
        transitions: &[RuntimeObjectTransition],
    ) -> Result<(), RuntimeObjectSyncError> {
        let actions: Vec<_> = self
            .actions
            .iter()
            .filter_map(|action| match action {
                ImmediateAction::Object(action) => Some(*action),
                _ => None,
            })
            .collect();
        if actions.len() != transitions.len() {
            return Err(RuntimeObjectSyncError::ActionMismatch);
        }
        let mut staged = self.staged.clone();
        for (action, transition) in actions.iter().zip(transitions) {
            let cell = transition.cell;
            let expected = match *action {
                ObjectAction::Explode { cell } => (cell, None),
                ObjectAction::Change { cell, raw_type } => (cell, Some(raw_type)),
            };
            if expected.0 != [usize::from(cell[0]), usize::from(cell[1])]
                || expected.1.is_some_and(|raw| raw != transition.new_type)
                || (matches!(action, ObjectAction::Explode { .. })
                    && transition.previous_type.checked_add(1) != Some(transition.new_type))
            {
                return Err(RuntimeObjectSyncError::ActionMismatch);
            }
            if cell[0] >= 64
                || cell[1] >= 64
                || !(163..=226).contains(&transition.previous_type)
                || !(163..=226).contains(&transition.new_type)
                || !transition.blocking_radius.is_finite()
                || !(0.0..=64.0).contains(&transition.blocking_radius)
            {
                return Err(RuntimeObjectSyncError::InvalidTransition);
            }
            if staged.processes.values().any(|process| {
                !matches!(process.phase, Phase::Closed)
                    && process
                        .object_motions
                        .iter()
                        .any(|motion| motion.cell == cell)
            }) {
                return Err(RuntimeObjectSyncError::ActiveMotion);
            }
            let types = staged
                .cell_types
                .as_mut()
                .ok_or(RuntimeObjectSyncError::MissingCellTypes)?;
            let index = usize::from(cell[0]) * 64 + usize::from(cell[1]);
            if types[index] != transition.previous_type {
                return Err(RuntimeObjectSyncError::StaleType);
            }
            let object = staged
                .objects
                .get_mut(&cell)
                .ok_or(RuntimeObjectSyncError::MissingObject)?;
            types[index] = transition.new_type;
            object.blocking_radius = transition.blocking_radius;
        }
        self.staged = staged;
        self.object_sync_complete = true;
        Ok(())
    }
    pub fn commit(self) -> ProcessEvent {
        if self.scheduled.is_some() {
            return ProcessEvent {
                process_id: self.event.process_id,
                outcome: ProcessOutcome::Unsupported,
            };
        }
        if !self.ambient_sync_complete
            && self
                .actions
                .iter()
                .any(|action| matches!(action, ImmediateAction::Ambient(_)))
        {
            return ProcessEvent {
                process_id: self.event.process_id,
                outcome: ProcessOutcome::Unsupported,
            };
        }
        if !self.object_sync_complete
            && self
                .actions
                .iter()
                .any(|action| matches!(action, ImmediateAction::Object(_)))
        {
            return ProcessEvent {
                process_id: self.event.process_id,
                outcome: ProcessOutcome::Unsupported,
            };
        }
        *self.runtime = self.staged;
        self.event
    }
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
            false,
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
            false,
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
            false,
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
        Self::with_object_capabilities(script, world, capabilities, raw_cells, object_poses, false)
    }
    /// Enable wind only when the authoritative host applies every emitted wind
    /// lifecycle event and routes displacement through world collision.
    pub fn with_wind(
        script: &Script,
        world: &World,
        capabilities: ProcessCapabilities<'_>,
        raw_cells: &[u8; 4096],
        object_poses: &[ProcessObject],
    ) -> Result<Self, ProcessObjectError> {
        Self::with_object_capabilities(script, world, capabilities, raw_cells, object_poses, true)
    }
    /// Enable immediate object/quake parsing for explicit prepared transactions.
    /// Ordinary trigger paths still reject these processes.
    pub fn with_staged_actions(
        script: &Script,
        world: &World,
        capabilities: ProcessCapabilities<'_>,
        raw_cells: &[u8; 4096],
        object_poses: &[ProcessObject],
    ) -> Result<Self, ProcessObjectError> {
        let mut runtime = Self::with_wind(script, world, capabilities, raw_cells, object_poses)?;
        let cells = capabilities.initial_cell_types.or(Some(raw_cells));
        runtime.cell_types = cells.map(|types| types.to_vec());
        runtime.processes = script
            .processes
            .iter()
            .map(|(&id, process)| {
                (
                    id,
                    RuntimeProcess::new(
                        process,
                        world,
                        capabilities,
                        cells,
                        true,
                        Some(&runtime.objects),
                        (true, true, false),
                    ),
                )
            })
            .collect();
        Ok(runtime)
    }
    /// Opt into authored source-light lifecycle; the host must consume events.
    pub fn with_source_lights(
        script: &Script,
        world: &World,
        capabilities: ProcessCapabilities<'_>,
        raw_cells: &[u8; 4096],
        object_poses: &[ProcessObject],
    ) -> Result<Self, ProcessObjectError> {
        let mut runtime =
            Self::with_staged_actions(script, world, capabilities, raw_cells, object_poses)?;
        let cells = capabilities.initial_cell_types.or(Some(raw_cells));
        runtime.processes = script
            .processes
            .iter()
            .map(|(&id, process)| {
                (
                    id,
                    RuntimeProcess::new(
                        process,
                        world,
                        capabilities,
                        cells,
                        true,
                        Some(&runtime.objects),
                        (true, true, true),
                    ),
                )
            })
            .collect();
        runtime.source_enabled = true;
        Ok(runtime)
    }
    pub fn source_lights_enabled(&self) -> bool {
        self.source_enabled
    }
    pub fn has_active_source_lights(&self) -> bool {
        self.processes.values().any(|p| p.source_active)
    }
    fn with_object_capabilities(
        script: &Script,
        world: &World,
        capabilities: ProcessCapabilities<'_>,
        raw_cells: &[u8; 4096],
        object_poses: &[ProcessObject],
        wind_enabled: bool,
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
            wind_enabled,
        ))
    }
    /// Mark a collected, destroyed or replaced object unavailable so later
    /// processes fail closed and any active movement stops emitting effects.
    pub fn remove_object(&mut self, cell: [u8; 2]) -> bool {
        self.objects.remove(&cell).is_some()
    }

    /// Iterate currently available source-object poses in stable cell order.
    pub fn objects(&self) -> impl Iterator<Item = &ProcessObject> {
        self.objects.values()
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
        wind_enabled: bool,
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
                            (wind_enabled, false, false),
                        ),
                    )
                })
                .collect(),
            links: script.triggers.clone(),
            previous_floor: None,
            keys: [false; 3],
            elevations: vec![0.0; world.walls.len()],
            static_walls: static_wall_mask(script, world),
            objects,
            cell_types: capabilities.initial_cell_types.map(|x| x.to_vec()),
            effects: Vec::new(),
            staged_authorized: false,
            source_enabled: false,
        }
    }
    /// Drain effects accumulated since the last call, including direct activations.
    /// The authoritative host must consume these before advancing another step.
    pub fn take_effects(&mut self) -> Vec<ProcessEffect> {
        std::mem::take(&mut self.effects)
    }
    /// Per-original-wall mask: `true` only when no known or unknown script
    /// action can address it. Walls without valid map cells remain false.
    /// Callers must invalidate this proof if external code mutates geometry or
    /// changes the wall vector, script, map or index order.
    pub fn static_wall_mask(&self) -> &[bool] {
        &self.static_walls
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
            Some(process) if !self.staged_authorized && !process.immediate_actions.is_empty() => {
                Err(ProcessOutcome::Unsupported)
            }
            Some(process) => process.trigger(self.keys),
            None => Err(ProcessOutcome::Missing),
        };
        let outcome = match result {
            Ok((changes, animations)) => {
                let deferred = self.processes.get(&id).is_some_and(|p| {
                    matches!(p.phase, Phase::Delay(_)) && !p.immediate_actions.is_empty()
                });
                if deferred {
                    return ProcessEvent {
                        process_id: id,
                        outcome: ProcessOutcome::Accepted,
                    };
                }
                if let Some(process) = self.processes.get_mut(&id) {
                    if process.phase == Phase::Opening {
                        process.start_hazard(id, &mut self.effects);
                    } else if process.phase == Phase::Closing {
                        process.stop_sources(id, &mut self.effects);
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
                            process.stop_sources(target, &mut self.effects);
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
    /// Prepare exactly one matching link, leaving this runtime unchanged on
    /// failure or drop. The caller must validate all actions and the effects
    /// they induce in every authoritative subsystem before committing.
    pub fn prepare_at(
        &mut self,
        kind: &str,
        cell: [u8; 2],
        id: u16,
    ) -> Result<PreparedProcess<'_>, ProcessOutcome> {
        if !matches!(kind, "floor" | "link" | "shoot" | "destroy")
            || !self
                .links
                .iter()
                .any(|link| link.kind == kind && [link.x, link.y] == cell && link.process == id)
        {
            return Err(ProcessOutcome::Missing);
        }
        let mut staged = self.clone();
        let actions = staged
            .processes
            .get(&id)
            .filter(|p| matches!(p.phase, Phase::Closed))
            .map(|p| p.immediate_actions.clone())
            .unwrap_or_default();
        if kind == "destroy" {
            if let Some(process) = staged.processes.get_mut(&id) {
                process.locked = false;
            }
        }
        staged.staged_authorized = true;
        let event = staged.trigger(id);
        staged.staged_authorized = false;
        if event.outcome != ProcessOutcome::Accepted {
            return Err(event.outcome);
        }
        let scheduled = staged
            .processes
            .get(&id)
            .is_some_and(|p| matches!(p.phase, Phase::Delay(_)) && !p.immediate_actions.is_empty())
            .then(|| staged.clone());
        Ok(PreparedProcess {
            runtime: self,
            staged,
            actions,
            object_sync_complete: false,
            ambient_sync_complete: false,
            scheduled,
            event,
        })
    }

    /// Stable process IDs whose delayed actions have reached their deadline.
    /// A failed host preflight leaves the process here for a future retry.
    pub fn ready_delayed(&self) -> Vec<u16> {
        self.processes
            .iter()
            .filter_map(|(&id, p)| {
                (matches!(p.phase, Phase::Delay(v) if v <= 0.0) && !p.immediate_actions.is_empty())
                    .then_some(id)
            })
            .collect()
    }

    /// Stage the complete start of an already armed delayed procedure.
    pub fn prepare_due(&mut self, id: u16) -> Result<PreparedProcess<'_>, ProcessOutcome> {
        let p = self.processes.get(&id).ok_or(ProcessOutcome::Missing)?;
        if !matches!(p.phase, Phase::Delay(v) if v <= 0.0) || p.immediate_actions.is_empty() {
            return Err(ProcessOutcome::Busy);
        }
        if p.locked {
            return Err(ProcessOutcome::Locked);
        }
        if p.object_motions.iter().any(|motion| {
            !self.objects.contains_key(&motion.cell)
                || self.cell_types.as_ref().is_some_and(|types| {
                    types[usize::from(motion.cell[0]) * 64 + usize::from(motion.cell[1])]
                        != motion.raw_type
                })
        }) {
            return Err(ProcessOutcome::Unsupported);
        }
        let mut staged = self.clone();
        let process = staged.processes.get_mut(&id).expect("checked process");
        let actions = process.immediate_actions.clone();
        let changes = process.lock_changes.clone();
        let animations = process.animations.clone();
        process.phase = Phase::Opening;
        process.start_hazard(id, &mut staged.effects);
        staged.effects.extend(
            animations
                .into_iter()
                .map(|object_id| ProcessEffect::PlayAnimation { object_id }),
        );
        for (target, locked) in changes {
            if let Some(p) = staged.processes.get_mut(&target) {
                p.locked = locked;
                if locked {
                    p.stop_hazard(target, &mut staged.effects);
                    p.stop_sources(target, &mut staged.effects);
                }
            }
        }
        Ok(PreparedProcess {
            runtime: self,
            staged,
            actions,
            object_sync_complete: false,
            ambient_sync_complete: false,
            scheduled: None,
            event: ProcessEvent {
                process_id: id,
                outcome: ProcessOutcome::Accepted,
            },
        })
    }

    /// Stage an unlinked, forced-unlock mission-only procedure. This narrow
    /// entry point is for the addon boss rule; it never accepts actions that
    /// require external object, presentation, or hazard synchronization.
    pub fn prepare_forced_mission(
        &mut self,
        id: u16,
    ) -> Result<PreparedProcess<'_>, ProcessOutcome> {
        let process = self.processes.get(&id).ok_or(ProcessOutcome::Missing)?;
        if !process.mission_effect
            || !process.motions.is_empty()
            || !process.object_motions.is_empty()
            || !process.empty_motion_cells.is_empty()
            || !process.lock_changes.is_empty()
            || !process.cell_changes.is_empty()
            || !process.animations.is_empty()
            || !process.death_zones.is_empty()
            || !process.wind_zones.is_empty()
            || !process.sources.is_empty()
            || !process.immediate_actions.is_empty()
        {
            return Err(ProcessOutcome::Unsupported);
        }
        let mut staged = self.clone();
        staged
            .processes
            .get_mut(&id)
            .expect("checked process")
            .locked = false;
        let event = staged.trigger(id);
        if event.outcome != ProcessOutcome::Accepted {
            return Err(event.outcome);
        }
        Ok(PreparedProcess {
            runtime: self,
            staged,
            actions: Vec::new(),
            object_sync_complete: false,
            ambient_sync_complete: false,
            scheduled: None,
            event,
        })
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
                let previous_lock = if kind == "destroy" {
                    self.processes.get_mut(&id).map(|process| {
                        let locked = process.locked;
                        process.locked = false;
                        locked
                    })
                } else {
                    None
                };
                let event = self.trigger(id);
                if event.outcome != ProcessOutcome::Accepted {
                    if let (Some(locked), Some(process)) =
                        (previous_lock, self.processes.get_mut(&id))
                    {
                        process.locked = locked;
                    }
                }
                event
            })
            .collect()
    }
    pub fn activate_links_at(&mut self, cell: [u8; 2]) -> Vec<ProcessEvent> {
        self.activate_at("link", cell)
    }
    /// Gather floor and use commands in authored order. Floor entry is sampled
    /// once; the caller must dispatch every returned command before advancing.
    pub fn collect_tick_commands(
        &mut self,
        position: [f32; 2],
        interact_pressed: bool,
    ) -> Vec<ProcessCommand> {
        let p = position;
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
        let mut commands = Vec::new();
        if floor != self.previous_floor {
            if let Some(cell) = floor {
                commands.extend(
                    self.links
                        .iter()
                        .filter(|t| {
                            matches!(t.kind.as_str(), "floor" | "returnf") && [t.x, t.y] == cell
                        })
                        .map(|t| ProcessCommand {
                            kind: t.kind.clone(),
                            cell,
                            process_id: t.process,
                        }),
                );
            }
            self.previous_floor = floor;
        }
        if interact_pressed && p[0].is_finite() && p[1].is_finite() {
            let nearest = self
                .links
                .iter()
                .filter(|link| matches!(link.kind.as_str(), "link" | "return"))
                .map(|link| {
                    let cell = [link.x, link.y];
                    // A switch's rendered pose can be far from its source map
                    // cell, and it may itself move during a process.
                    let target = self
                        .objects
                        .get(&cell)
                        .map(|object| object.position)
                        .unwrap_or([f32::from(link.x) + 0.5, f32::from(link.y) + 0.5]);
                    let dx = target[0] - p[0];
                    let dy = target[1] - p[1];
                    (cell, dx * dx + dy * dy)
                })
                .filter(|(_, distance)| *distance <= USE_RANGE * USE_RANGE)
                .min_by(|a, b| a.1.total_cmp(&b.1));
            if let Some((cell, _)) = nearest {
                commands.extend(
                    self.links
                        .iter()
                        .filter(|t| {
                            matches!(t.kind.as_str(), "link" | "return") && [t.x, t.y] == cell
                        })
                        .map(|t| ProcessCommand {
                            kind: t.kind.clone(),
                            cell,
                            process_id: t.process,
                        }),
                );
            }
        }
        commands
    }
    /// Dispatch one gathered command. Reverse commands have no activation event.
    pub fn dispatch_tick_command(&mut self, command: &ProcessCommand) -> Option<ProcessEvent> {
        if !self.links.iter().any(|t| {
            t.kind == command.kind && [t.x, t.y] == command.cell && t.process == command.process_id
        }) {
            return None;
        }
        if matches!(command.kind.as_str(), "return" | "returnf") {
            if let Some(process) = self.processes.get_mut(&command.process_id) {
                process.return_to_start(command.process_id, &mut self.effects);
            }
            None
        } else if matches!(command.kind.as_str(), "floor" | "link") {
            Some(self.trigger(command.process_id))
        } else {
            None
        }
    }
    /// Advance processes after all commands for this tick have been dispatched.
    pub fn advance_motions(&mut self, world: &mut World, dt: f32) {
        if !dt.is_finite() || dt <= 0.0 {
            return;
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
        let commands = self.collect_tick_commands(world.player.position, interact_pressed);
        let events = commands
            .iter()
            .filter_map(|command| self.dispatch_tick_command(command))
            .collect();
        self.advance_motions(world, dt);
        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::{Player, Wall};

    #[test]
    fn validates_reversed_object_motion_and_rejects_forgery() {
        let script = Script::parse(
            "#proc1\nBackWait 0.1\n#action\nmove 2 0 256 0\n#end\n#links\nfloor 0 0 1\n#end\n",
        );
        let base_world = world();
        let mut moving_world = base_world.clone();
        let mut cells = [0; 4096];
        cells[128] = 163;
        let baseline = ProcessRuntime::with_objects_and_hazards(
            &script,
            &base_world,
            ProcessCapabilities::default(),
            &cells,
            &[object_fixture([2, 0])],
        )
        .unwrap();
        let mut source = baseline.clone();
        for _ in 0..80 {
            source.tick(&mut moving_world, 1.0 / 60.0, false);
            source.take_effects();
        }
        let snapshot = source.snapshot(&moving_world).unwrap();
        assert!(matches!(
            snapshot.processes[0].phase,
            ProcessPhaseSnapshot::Closing
        ));
        let mut restored = baseline.clone();
        restored.restore(&snapshot, &moving_world).unwrap();
        assert_eq!(
            restored.validate_snapshot_geometry(&baseline, &base_world, &moving_world),
            Ok(())
        );
        let mut forged = snapshot;
        forged.objects[0].position[0] += 0.1;
        restored.restore(&forged, &moving_world).unwrap();
        assert_eq!(
            restored.validate_snapshot_geometry(&baseline, &base_world, &moving_world),
            Err(ProcessGeometryError::PoseMismatch)
        );
    }

    #[test]
    fn rejects_history_dependent_rotation_combinations() {
        let script = Script::parse(
            "#proc1\n#action\nrotate 1 0 256 0 90\n#end\n#proc2\n#action\nmove 1 0 256 0\n#end\n#links\nlink 0 0 2\n#end\n",
        );
        let world = world();
        let baseline = ProcessRuntime::new(&script, &world);
        assert_eq!(
            baseline.validate_snapshot_geometry(&baseline, &world, &world),
            Ok(())
        );
        let mut moved = world.clone();
        let mut runtime = baseline.clone();
        runtime.activate_at("link", [0, 0]);
        runtime.tick(&mut moved, 1.0 / 60.0, false);
        assert_eq!(
            runtime.validate_snapshot_geometry(&baseline, &world, &moved),
            Err(ProcessGeometryError::AmbiguousRotation)
        );
    }

    #[test]
    fn snapshot_effects_match_compiled_active_zones() {
        let script = Script::parse("#proc1\n#action\nnonstop\ndeath 0 0 2 2 0 64 20\nwind 0 0 2 2 4 0\n#end\n#links\nlink 0 0 1\n#end\n");
        let mut runtime = ProcessRuntime::with_wind(
            &script,
            &world(),
            ProcessCapabilities::default(),
            &[0; 4096],
            &[],
        )
        .unwrap();
        assert_eq!(
            runtime.activate_at("link", [0, 0])[0].outcome,
            ProcessOutcome::Accepted
        );
        let mut hazards = crate::hazards::Hazards::default();
        let mut wind = crate::wind::WindField::new();
        for effect in runtime.take_effects() {
            hazards.apply_effect(&effect);
            if let ProcessEffect::WindActivated { process_id, zones } = effect {
                wind.activate(process_id, &zones).unwrap();
            }
        }
        let hazards = hazards.snapshot();
        let wind = wind.snapshot();
        assert!(runtime.snapshot_effects_match(&hazards, &wind));
        let mut forged = hazards.clone();
        forged.active[0].zones[0].damage_raw += 1;
        assert!(!runtime.snapshot_effects_match(&forged, &wind));
        let mut forged = wind.clone();
        forged.active[0].1[0].direction[0] += 1;
        assert!(!runtime.snapshot_effects_match(&hazards, &forged));
    }

    #[test]
    fn forced_mission_stages_unlock_and_fails_closed() {
        let script =
            Script::parse("#proc30\nlock\nenddelay 13\n#action\n#end\n#links\nlink 1 0 30\n#end\n");
        let mut runtime = ProcessRuntime::new(&script, &world());
        drop(runtime.prepare_forced_mission(30).unwrap());
        assert_eq!(
            runtime.activate_at("link", [1, 0])[0].outcome,
            ProcessOutcome::Locked
        );
        assert_eq!(
            runtime.prepare_forced_mission(30).unwrap().commit().outcome,
            ProcessOutcome::Accepted
        );
        assert_eq!(
            runtime.activate_at("link", [1, 0])[0].outcome,
            ProcessOutcome::Accepted
        );
        assert!(matches!(
            runtime.prepare_forced_mission(31),
            Err(ProcessOutcome::Missing)
        ));
        let mixed = Script::parse("#proc30\nenddelay 13\n#action\nmove 1 0 256 0\n#end\n");
        let mut runtime = ProcessRuntime::new(&mixed, &world());
        assert!(matches!(
            runtime.prepare_forced_mission(30),
            Err(ProcessOutcome::Unsupported)
        ));
    }

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
    fn axis_moves_and_speed_overrides_finish_independently_and_reverse() {
        let script = Script::parse("#proc1\nSpeed 8\n#action\nxmove 1 0 256 256 s:16\nymove 2 0 256 256\n#end\n#links\nlink 0 0 1\nreturn 3 0 1\n#end\n");
        let mut world = world();
        world.player.position = [-10.0, -10.0];
        world.walls.push(Wall {
            a: [2.0, 0.0],
            b: [2.0, 1.0],
            texture: 1,
            blocks_movement: true,
            cell: Some([2, 0]),
        });
        let mut runtime = ProcessRuntime::new(&script, &world);
        assert_eq!(
            runtime.activate_at("link", [0, 0])[0].outcome,
            ProcessOutcome::Accepted
        );
        tick_for(&mut runtime, &mut world, 0.35);
        // Axis path bound is 2 seconds, and s:16 halves that duration.
        assert!(world.walls[0].a[0] > 1.6 && world.walls[0].a[1].abs() < 0.001);
        assert!(world.walls[1].a[0] == 2.0 && world.walls[1].a[1] > 0.3);
        tick_for(&mut runtime, &mut world, 0.85);
        assert!(
            (world.walls[0].a[0] - 2.0).abs() < 0.001 && (world.walls[0].a[1] - 1.0).abs() < 0.001
        );
        assert!(world.walls[1].a[0] < 3.0);
        runtime.activate_at("return", [3, 0]);
        tick_for(&mut runtime, &mut world, 3.0);
        assert!((world.walls[0].a[0] - 1.0).abs() < 0.001 && world.walls[0].a[1].abs() < 0.001);
        assert!((world.walls[1].a[0] - 2.0).abs() < 0.001 && world.walls[1].a[1].abs() < 0.001);
    }

    #[test]
    fn malformed_motion_speed_override_disables_the_whole_process() {
        for suffix in [
            "s:0",
            "s:-1",
            "s:nan",
            "s:",
            "s:8 extra",
            "32768",
            "-32769",
            "8.0",
            "8 2",
        ] {
            let script = Script::parse(&format!(
                "#proc1\n#action\nmove 1 0 256 0 {suffix}\n#end\n#links\nlink 0 0 1\n#end\n"
            ));
            let mut world = world();
            let original = world.walls[0].a;
            let mut runtime = ProcessRuntime::new(&script, &world);
            assert_eq!(
                runtime.activate_at("link", [0, 0])[0].outcome,
                ProcessOutcome::Unsupported,
                "{suffix}"
            );
            tick_for(&mut runtime, &mut world, 1.0);
            assert_eq!(world.walls[0].a, original);
        }
    }

    #[test]
    fn durable_snapshot_restores_delayed_stage_and_rejects_corruption() {
        let script = Script::parse("#proc1\nStartDelay 2\n#action\nquake 1 1 2 2 5\nmove 1 0 256 0\n#end\n#links\nlink 0 0 1\n#end\n");
        let mut world = world();
        world.player.position = [-10.0, -10.0];
        let mut runtime = ProcessRuntime::with_staged_actions(
            &script,
            &world,
            ProcessCapabilities::default(),
            &[0; 4096],
            &[],
        )
        .unwrap();
        let baseline = runtime.clone();
        assert_eq!(
            runtime
                .prepare_at("link", [0, 0], 1)
                .unwrap()
                .commit_schedule()
                .outcome,
            ProcessOutcome::Accepted
        );
        runtime.tick(&mut world, 1.0, false);
        let snap = runtime.snapshot(&world).unwrap();
        let mut restored = baseline;
        restored.restore(&snap, &world).unwrap();
        assert_eq!(restored.snapshot(&world).unwrap(), snap);
        let mut bad = snap.clone();
        bad.processes[0].progress = f32::NAN;
        assert_eq!(
            restored.restore(&bad, &world),
            Err(ProcessSnapshotError::InvalidValue)
        );
        assert_eq!(restored.snapshot(&world).unwrap(), snap);
        bad = snap.clone();
        bad.wall_identity[0].1 ^= 1;
        assert_eq!(
            restored.restore(&bad, &world),
            Err(ProcessSnapshotError::WallIdentity)
        );
        restored.tick(&mut world, 1.0, false);
        assert_eq!(restored.ready_delayed(), vec![1]);
        assert!(restored.prepare_due(1).is_ok());
    }

    #[test]
    fn source_lights_are_opt_in_and_follow_delay_and_reverse() {
        let script = Script::parse("#proc1\nStartDelay 0.5\nBackWait 0.5\n#action\nsource 29 29 5 28 160\n#end\n#links\nlink 0 0 1\n#end\n");
        let mut world = world();
        world.player.position = [-10.0, -10.0];
        let mut disabled = ProcessRuntime::with_staged_actions(
            &script,
            &world,
            ProcessCapabilities::default(),
            &[0; 4096],
            &[],
        )
        .unwrap();
        assert_eq!(
            disabled.activate_at("link", [0, 0])[0].outcome,
            ProcessOutcome::Unsupported
        );
        let mut runtime = ProcessRuntime::with_source_lights(
            &script,
            &world,
            ProcessCapabilities::default(),
            &[0; 4096],
            &[],
        )
        .unwrap();
        assert_eq!(
            runtime.activate_at("link", [0, 0])[0].outcome,
            ProcessOutcome::Accepted
        );
        assert!(runtime.take_effects().is_empty());
        runtime.tick(&mut world, 0.25, false);
        assert!(runtime.take_effects().is_empty());
        runtime.tick(&mut world, 0.3, false);
        assert!(
            matches!(&runtime.take_effects()[..], [ProcessEffect::SourceLightsActivated { process_id:1, sources }] if sources[0].position_raw == [7552,7552])
        );
        assert!(matches!(
            runtime.snapshot(&world),
            Err(ProcessSnapshotError::ActiveSourceLights)
        ));
        runtime.tick(&mut world, 0.5, false);
        assert_eq!(
            runtime.take_effects(),
            vec![ProcessEffect::SourceLightsDeactivated { process_id: 1 }]
        );
    }

    #[test]
    fn source_nonstop_survives_open_and_lock_stops_it() {
        let script = Script::parse("#proc1\n#action\nnonstop\nsource 1 1 5 28 160\n#end\n#proc2\n#action\nlock 1\n#end\n#links\nlink 0 0 1\nlink 1 0 2\n#end\n");
        let mut world = world();
        let mut runtime = ProcessRuntime::with_source_lights(
            &script,
            &world,
            ProcessCapabilities::default(),
            &[0; 4096],
            &[],
        )
        .unwrap();
        runtime.activate_at("link", [0, 0]);
        assert!(matches!(
            &runtime.take_effects()[..],
            [ProcessEffect::SourceLightsActivated { process_id: 1, .. }]
        ));
        runtime.tick(&mut world, 2.0, false);
        assert!(runtime.take_effects().is_empty());
        runtime.activate_at("link", [1, 0]);
        assert_eq!(
            runtime.take_effects(),
            vec![ProcessEffect::SourceLightsDeactivated { process_id: 1 }]
        );
    }

    #[test]
    fn durable_snapshot_preserves_inflight_motion_and_rejects_pending_effects() {
        let script = Script::parse(
            "#proc1\nSpeed 2\n#action\nmove 1 0 256 0\nup 2 0 80\n#end\n#links\nlink 0 0 1\n#end\n",
        );
        let mut world = world();
        world.player.position = [-10.0, -10.0];
        world.walls.push(Wall {
            a: [2.0, 0.0],
            b: [2.0, 1.0],
            texture: 1,
            blocks_movement: true,
            cell: Some([2, 0]),
        });
        let mut source = ProcessRuntime::with_capabilities(
            &script,
            &world,
            ProcessCapabilities {
                wall_elevation: true,
                initial_cell_types: None,
            },
        );
        let mut target = source.clone();
        source.activate_at("link", [0, 0]);
        source.tick(&mut world, 0.1, false);
        assert!(matches!(
            source.snapshot(&world),
            Err(ProcessSnapshotError::PendingEffects)
        ));
        source.take_effects();
        let snapshot = source.snapshot(&world).unwrap();
        let mut restored_world = world.clone();
        target.restore(&snapshot, &restored_world).unwrap();
        source.tick(&mut world, 0.1, false);
        target.tick(&mut restored_world, 0.1, false);
        assert_eq!(world.walls[0].a, restored_world.walls[0].a);
        assert_eq!(source.wall_elevation(1), target.wall_elevation(1));
    }

    #[test]
    fn numeric_move_suffix_is_preserved_but_does_not_change_motion() {
        let mut baseline: Option<f32> = None;
        for suffix in ["", " 64", " -32768", " 32767"] {
            let script = Script::parse(&format!("#proc1\nSpeed 4\n#action\nmove 1 0 256 0{suffix}\n#end\n#links\nlink 0 0 1\n#end\n"));
            if !suffix.is_empty() {
                assert_eq!(script.processes[&1].actions[0].arguments[4], suffix.trim());
            }
            let mut world = world();
            let start = world.walls[0].a;
            let mut runtime = ProcessRuntime::new(&script, &world);
            assert_eq!(
                runtime.activate_at("link", [0, 0])[0].outcome,
                ProcessOutcome::Accepted
            );
            tick_for(&mut runtime, &mut world, 0.1);
            let partial = world.walls[0].a[0];
            if let Some(expected) = baseline {
                assert!((partial - expected).abs() < 0.0001);
            } else {
                baseline = Some(partial);
            }
            tick_for(&mut runtime, &mut world, 10.0);
            assert!((world.walls[0].a[0] - start[0] - 1.0).abs() < 0.001);
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
    fn rejected_destroy_trigger_does_not_unlock_its_process() {
        let script = Script::parse(
            "#proc1\nLock\n#action\nunknown 1\n#end\n#links\ndestroy 1 1 1\nfloor 2 2 1\n#end\n",
        );
        let mut runtime = ProcessRuntime::new(&script, &world());
        assert_eq!(
            runtime.activate_at("destroy", [1, 1])[0].outcome,
            ProcessOutcome::Unsupported
        );
        assert_eq!(
            runtime.activate_at("floor", [2, 2])[0].outcome,
            ProcessOutcome::Locked
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
    fn use_tracks_live_object_pose_and_falls_back_to_cell_center() {
        let script = Script::parse("#proc1\n#action\nmove 1 0 256 0\n#end\n#proc2\n#action\nmove 2 0 256 0\n#end\n#links\nlink 10 10 1\nlink 0 1 2\n#end\n");
        let mut world = world();
        let mut cells = [0u8; 4096];
        cells[10 * 64 + 10] = 163;
        let pose = ProcessObject {
            cell: [10, 10],
            position: [2.0, 2.0],
            angle_radians: 0.0,
            elevation_raw: 0.0,
            blocking_radius: 0.0,
        };
        let mut runtime = ProcessRuntime::with_wind(
            &script,
            &world,
            ProcessCapabilities::default(),
            &cells,
            &[pose],
        )
        .unwrap();
        let chosen = runtime.collect_tick_commands([2.0, 2.0], true);
        assert!(chosen
            .iter()
            .any(|c| c.kind == "link" && c.cell == [10, 10]));
        assert!(!runtime
            .collect_tick_commands([10.5, 10.5], true)
            .iter()
            .any(|c| c.cell == [10, 10]));
        runtime.objects.get_mut(&[10, 10]).unwrap().position = [5.0, 5.0];
        assert!(runtime
            .collect_tick_commands([5.0, 5.0], true)
            .iter()
            .any(|c| c.cell == [10, 10]));
        assert!(!runtime
            .collect_tick_commands([2.0, 2.0], true)
            .iter()
            .any(|c| c.cell == [10, 10]));
        assert!(runtime
            .collect_tick_commands([0.5, 1.5], true)
            .iter()
            .any(|c| c.cell == [0, 1]));
        world.player.position = [f32::NAN, 0.0];
        assert!(runtime
            .collect_tick_commands(world.player.position, true)
            .is_empty());
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
    #[test]
    fn nonstop_wind_obeys_delay_unlock_and_lock() {
        let script = Script::parse("#proc1\nLock\nStartDelay 0.5\n#action\nnonstop\nwind 4 4 5 5 -21 0\n#end\n#proc2\n#action\nunlock 1\n#end\n#proc3\n#action\nlock 1\n#end\n#links\nfloor 0 0 1\nlink 1 0 2\nlink 2 0 3\n#end\n");
        let mut world = world();
        let cells = [0; 4096];
        let mut default =
            ProcessRuntime::with_hazards(&script, &world, ProcessCapabilities::default(), &cells);
        default.activate_at("link", [1, 0]);
        assert_eq!(
            default.activate_at("floor", [0, 0])[0].outcome,
            ProcessOutcome::Unsupported
        );
        let mut runtime =
            ProcessRuntime::with_wind(&script, &world, ProcessCapabilities::default(), &cells, &[])
                .unwrap();
        assert_eq!(
            runtime.activate_at("floor", [0, 0])[0].outcome,
            ProcessOutcome::Locked
        );
        runtime.activate_at("link", [1, 0]);
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
            vec![ProcessEffect::WindActivated {
                process_id: 1,
                zones: vec![WindZone {
                    min_cell: [4, 4],
                    max_cell: [5, 5],
                    direction: [-21, 0],
                    actor_only: false,
                }]
            }]
        );
        runtime.tick(&mut world, 2.0, false);
        assert!(runtime.take_effects().is_empty());
        runtime.activate_at("link", [2, 0]);
        assert_eq!(
            runtime.take_effects(),
            vec![ProcessEffect::WindDeactivated { process_id: 1 }]
        );
        assert_eq!(
            runtime.activate_at("floor", [0, 0])[0].outcome,
            ProcessOutcome::Locked
        );
    }

    #[test]
    fn finite_wind_and_hazard_activate_and_stop_together() {
        let script = Script::parse("#proc1\nSpeed 10\n#action\nwind 1 2 3 4 127 -128\ndeath 1 2 3 4 0 24 500\n#end\n#links\nlink 0 0 1\n#end\n");
        let mut world = world();
        world.player.position = [-10.0, -10.0];
        let mut runtime = ProcessRuntime::with_wind(
            &script,
            &world,
            ProcessCapabilities::default(),
            &[0; 4096],
            &[],
        )
        .unwrap();
        assert_eq!(
            runtime.activate_at("link", [0, 0])[0].outcome,
            ProcessOutcome::Accepted
        );
        let effects = runtime.take_effects();
        assert!(matches!(
            &effects[..],
            [
                ProcessEffect::HazardActivated { .. },
                ProcessEffect::WindActivated { .. }
            ]
        ));
        runtime.tick(&mut world, 0.5, false);
        assert!(runtime.take_effects().is_empty());
        runtime.tick(&mut world, 0.5, false);
        assert_eq!(
            runtime.take_effects(),
            vec![
                ProcessEffect::HazardDeactivated { process_id: 1 },
                ProcessEffect::WindDeactivated { process_id: 1 }
            ]
        );
    }

    #[test]
    fn invalid_wind_disables_whole_process() {
        for action in [
            "wind 0 0 1 1 1",
            "wind 0 0 1 1 1 2 2",
            "wind 0 0 1 1 1 2 bad",
            "wind 0 0 1 1 1 2 1 0",
            "wind -1 0 1 1 1 2",
            "wind 0 0 64 1 1 2",
            "wind 2 0 1 1 1 2",
            "wind 0 0 1 1 -129 2",
            "wind 0 0 1 1 1 128",
            "wind 0 0 1 1 bad 2",
            "nonstop",
            "wind 0 0 1 1 1 2\nquake 1 1 2 2 10",
        ] {
            let script = Script::parse(&format!(
                "#proc1\n#action\n{action}\n#end\n#links\nlink 0 0 1\n#end\n"
            ));
            let world = world();
            let mut runtime = ProcessRuntime::with_wind(
                &script,
                &world,
                ProcessCapabilities::default(),
                &[0; 4096],
                &[],
            )
            .unwrap();
            assert_eq!(
                runtime.activate_at("link", [0, 0])[0].outcome,
                ProcessOutcome::Unsupported,
                "{action}"
            );
            assert!(runtime.take_effects().is_empty(), "{action}");
        }
    }
    #[test]
    fn staged_actions_require_preparation_and_commit_atomically() {
        let script = Script::parse("#proc1\n#action\nnonstop\nexplode 27 46\nchange 29 48 200\nquake 17 40 30 50 10000\nwind 25 47 27 48 12 0\nunlock 2\n#end\n#proc2\nLock\nEndDelay 1\n#end\n#links\ndestroy 27 47 1\nlink 1 0 2\n#end\n");
        let world = world();
        let mut cells = [0; 4096];
        cells[27 * 64 + 46] = 163;
        cells[29 * 64 + 48] = 170;
        let poses = [[27, 46], [29, 48]].map(|cell| ProcessObject {
            cell,
            position: [cell[0] as f32, cell[1] as f32],
            angle_radians: 0.0,
            elevation_raw: 0.0,
            blocking_radius: 0.2,
        });
        let mut runtime = ProcessRuntime::with_staged_actions(
            &script,
            &world,
            ProcessCapabilities::default(),
            &cells,
            &poses,
        )
        .unwrap();
        assert_eq!(
            runtime.activate_at("destroy", [27, 47])[0].outcome,
            ProcessOutcome::Unsupported
        );
        assert_eq!(
            runtime.activate_at("link", [1, 0])[0].outcome,
            ProcessOutcome::Locked
        );
        let expected = vec![
            ImmediateAction::Object(ObjectAction::Explode { cell: [27, 46] }),
            ImmediateAction::Object(ObjectAction::Change {
                cell: [29, 48],
                raw_type: 200,
            }),
            ImmediateAction::Quake(QuakeZone {
                min_cell: [17, 40],
                max_cell: [30, 50],
                power: 10000,
            }),
        ];
        {
            let prepared = runtime.prepare_at("destroy", [27, 47], 1).unwrap();
            assert_eq!(prepared.actions(), expected);
        }
        assert!(runtime.take_effects().is_empty());
        assert_eq!(
            runtime.activate_at("link", [1, 0])[0].outcome,
            ProcessOutcome::Locked
        );
        let mut prepared = runtime.prepare_at("destroy", [27, 47], 1).unwrap();
        assert_eq!(prepared.actions(), expected);
        prepared
            .stage_object_transitions(&[
                RuntimeObjectTransition {
                    cell: [27, 46],
                    previous_type: 163,
                    new_type: 164,
                    blocking_radius: 0.0,
                },
                RuntimeObjectTransition {
                    cell: [29, 48],
                    previous_type: 170,
                    new_type: 200,
                    blocking_radius: 0.25,
                },
            ])
            .unwrap();
        assert_eq!(prepared.commit().outcome, ProcessOutcome::Accepted);
        assert_eq!(runtime.cell_type([27, 46]), Some(164));
        assert_eq!(runtime.object_pose([27, 46]).unwrap().blocking_radius, 0.0);
        assert!(matches!(
            &runtime.take_effects()[..],
            [ProcessEffect::WindActivated { process_id: 1, .. }]
        ));
        assert_eq!(
            runtime.activate_at("link", [1, 0])[0].outcome,
            ProcessOutcome::Accepted
        );
    }

    #[test]
    fn staged_actions_reject_malformed_and_unhandled_commands() {
        for text in [
            "#action\nexplode 1 1\nquake 1 1 64 2 0",
            "#action\nexplode 1 1\nchange 1 1 100",
            "#action\nquake 1 1 2 2 1\nambient 1 1 2 2 32",
        ] {
            let script =
                Script::parse(&format!("#proc1\n{text}\n#end\n#links\nlink 0 0 1\n#end\n"));
            let world = world();
            let mut runtime = ProcessRuntime::with_staged_actions(
                &script,
                &world,
                ProcessCapabilities::default(),
                &[0; 4096],
                &[],
            )
            .unwrap();
            assert_eq!(
                runtime.prepare_at("link", [0, 0], 1).err(),
                Some(ProcessOutcome::Unsupported),
                "{text}"
            );
            assert!(runtime.take_effects().is_empty());
        }
    }
    #[test]
    fn staged_replacements_reject_active_motion_and_stale_batches() {
        let script = Script::parse("#proc1\n#action\nexplode 2 0\n#end\n#proc2\n#action\nmove 2 0 256 0\n#end\n#links\nlink 0 0 1\nlink 3 0 2\n#end\n");
        let mut cells = [0; 4096];
        cells[2 * 64] = 163;
        let mut world = world();
        world.player.position = [-10.0, -10.0];
        let pose = ProcessObject {
            cell: [2, 0],
            position: [2.25, 0.5],
            angle_radians: 0.5,
            elevation_raw: 4.0,
            blocking_radius: 0.2,
        };
        let mut runtime = ProcessRuntime::with_staged_actions(
            &script,
            &world,
            ProcessCapabilities::default(),
            &cells,
            &[pose],
        )
        .unwrap();
        assert_eq!(
            runtime.activate_at("link", [3, 0])[0].outcome,
            ProcessOutcome::Accepted
        );
        let mut prepared = runtime.prepare_at("link", [0, 0], 1).unwrap();
        let update = RuntimeObjectTransition {
            cell: [2, 0],
            previous_type: 163,
            new_type: 164,
            blocking_radius: 0.0,
        };
        assert_eq!(
            prepared.stage_object_transitions(&[update]),
            Err(RuntimeObjectSyncError::ActiveMotion)
        );
        assert_eq!(prepared.commit().outcome, ProcessOutcome::Unsupported);
        assert_eq!(runtime.cell_type([2, 0]), Some(163));
        runtime.tick(&mut world, 5.0, false);
        let mut prepared = runtime.prepare_at("link", [0, 0], 1).unwrap();
        assert_eq!(
            prepared.stage_object_transitions(&[update]),
            Err(RuntimeObjectSyncError::ActiveMotion),
            "open motion can still reverse"
        );
        drop(prepared);

        let mut fresh = ProcessRuntime::with_staged_actions(
            &script,
            &world,
            ProcessCapabilities::default(),
            &cells,
            &[pose],
        )
        .unwrap();
        let mut prepared = fresh.prepare_at("link", [0, 0], 1).unwrap();
        let bad = RuntimeObjectTransition {
            previous_type: 165,
            new_type: 166,
            ..update
        };
        assert_eq!(
            prepared.stage_object_transitions(&[bad]),
            Err(RuntimeObjectSyncError::StaleType)
        );
        prepared.stage_object_transitions(&[update]).unwrap();
        assert_eq!(prepared.commit().outcome, ProcessOutcome::Accepted);
        assert_eq!(fresh.cell_type([2, 0]), Some(164));
        assert_eq!(
            fresh.object_pose([2, 0]),
            Some(ProcessObject {
                blocking_radius: 0.0,
                ..pose
            })
        );
        assert_eq!(
            fresh.activate_at("link", [3, 0])[0].outcome,
            ProcessOutcome::Unsupported
        );
    }
    #[test]
    fn static_mask_excludes_all_authored_geometry_targets_even_if_unsupported() {
        let script = Script::parse("#proc1\n#action\nmove 1 0 256 0\nquake 0 0 2 2 100\n#end\n#proc2\n#action\nxmove 2 0 malformed\nymove 3 0 1 1\nrotate 4 0 1 1 90\nup 5 0 80\nchange 6 0 5\nexplode 7 0\n#end\n");
        let mut world = world();
        world.walls = (1..=8)
            .map(|x| Wall {
                a: [x as f32, 0.0],
                b: [x as f32, 1.0],
                texture: 1,
                blocks_movement: true,
                cell: Some([x, 0]),
            })
            .collect();
        world.walls.push(Wall {
            a: [9.0, 0.0],
            b: [9.0, 1.0],
            texture: 1,
            blocks_movement: true,
            cell: None,
        });
        let runtime = ProcessRuntime::new(&script, &world);
        assert_eq!(
            runtime.static_wall_mask(),
            &[false, false, false, false, false, false, false, true, false]
        );
        assert_eq!(
            runtime.clone().static_wall_mask(),
            runtime.static_wall_mask()
        );
    }
    #[test]
    fn ambiguous_actions_or_script_diagnostics_make_all_walls_unverified() {
        let world = world();
        for body in [
            "move unknown 0 256 0",
            "move 64 0 256 0",
            "move 1",
            "unknown 1 0",
            "call 1",
        ] {
            let script = Script::parse(&format!("#proc1\n#action\n{body}\n#end\n"));
            assert_eq!(
                ProcessRuntime::new(&script, &world).static_wall_mask(),
                &[false],
                "{body}"
            );
        }
        let script = Script::parse("#proc 1\n#end\n#proc 1\n#end\n");
        assert!(!script.diagnostics.is_empty());
        assert_eq!(
            ProcessRuntime::new(&script, &world).static_wall_mask(),
            &[false]
        );
    }
    #[test]
    fn static_mask_rejects_invalid_geometry_and_map_cells() {
        let script = Script::default();
        let mut world = world();
        world.walls.push(Wall {
            a: [f32::NAN, 0.0],
            b: [2.0, 1.0],
            texture: 1,
            blocks_movement: true,
            cell: Some([2, 0]),
        });
        world.walls.push(Wall {
            a: [3.0, 0.0],
            b: [3.0, 1.0],
            texture: 1,
            blocks_movement: true,
            cell: Some([64, 0]),
        });
        assert_eq!(
            ProcessRuntime::new(&script, &world).static_wall_mask(),
            &[true, false, false]
        );
    }
}
