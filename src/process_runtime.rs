//! Experimental subset of level process behavior, independent of rendering.
//!
//! Supported: floor and nearby use links, `move`, `StartDelay`, `BackWait`,
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
    displacement: [f32; 2],
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Phase {
    Closed,
    Delay(f32),
    Opening,
    Open,
    Closing,
}

#[derive(Debug)]
struct RuntimeProcess {
    motions: Vec<Motion>,
    phase: Phase,
    progress: f32,
    duration: f32,
    delay: f32,
    back_wait: f32,
    open_time: f32,
    locked: bool,
    waitout: Vec<([f32; 2], f32)>,
    lock_changes: Vec<(u16, bool)>,
    supported: bool,
    required_keys: [bool; 3],
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
    fn new(process: &Process, world: &World) -> Self {
        let mut motions = Vec::new();
        let mut waitout = Vec::new();
        let mut lock_changes = Vec::new();
        let mut supported = true;
        for action in &process.actions {
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
            if action.name != "move" || action.arguments.len() != 4 {
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
            let Some(dy) = action.arguments.get(3).and_then(|s| s.parse::<i32>().ok()) else {
                supported = false;
                break;
            };
            let before = motions.len();
            for (index, _) in world
                .walls
                .iter()
                .enumerate()
                .filter(|(_, wall)| wall.cell == Some([cell.0, cell.1]))
            {
                motions.push(Motion {
                    wall: index,
                    displacement: [
                        dx as f32 / MAP_UNITS_PER_TILE,
                        dy as f32 / MAP_UNITS_PER_TILE,
                    ],
                });
            }
            // Missing walls could be models or malformed references. Never
            // perform only the subset of a process that happened to resolve.
            if motions.len() == before {
                supported = false;
                break;
            }
        }
        if !supported {
            motions.clear();
            lock_changes.clear();
        }
        // The manual labels Speed but does not specify units. At speed 8 we use
        // one tile per second; each process completes when its longest move does.
        let speed = property_float(process, "speed", 8.0).max(0.01) / 8.0;
        let max_distance = motions
            .iter()
            .map(|m| m.displacement[0].hypot(m.displacement[1]))
            .fold(0.0f32, f32::max);
        let locked = process.properties.iter().any(|cmd| cmd.name == "lock");
        let required_keys = ["redkey", "greenkey", "bluekey"]
            .map(|name| process.properties.iter().any(|cmd| cmd.name == name));
        Self {
            motions,
            phase: Phase::Closed,
            progress: 0.0,
            duration: (max_distance / speed).max(0.001),
            delay: property_float(process, "startdelay", 0.0),
            back_wait: property_float(process, "backwait", 0.0),
            open_time: 0.0,
            locked,
            waitout,
            lock_changes,
            supported,
            required_keys,
        }
    }
    fn trigger(&mut self, keys: [bool; 3]) -> Option<Vec<(u16, bool)>> {
        if self.locked
            || self
                .required_keys
                .iter()
                .zip(keys)
                .any(|(required, owned)| *required && !owned)
            || !self.supported
            || (self.motions.is_empty() && self.lock_changes.is_empty())
        {
            return None;
        }
        match self.phase {
            Phase::Closed => {
                if !self.motions.is_empty() {
                    self.phase = if self.delay > 0.0 {
                        Phase::Delay(self.delay)
                    } else {
                        Phase::Opening
                    };
                }
                Some(self.lock_changes.clone())
            }
            Phase::Open if !self.motions.is_empty() => {
                self.phase = Phase::Closing;
                self.open_time = 0.0;
                Some(Vec::new())
            }
            _ => None,
        }
    }
    fn tick(&mut self, world: &mut World, dt: f32) {
        if self.phase == Phase::Closing
            && self.waitout.iter().any(|(center, radius)| {
                let dx = world.player.position[0] - center[0];
                let dy = world.player.position[1] - center[1];
                dx * dx + dy * dy <= radius * radius
            })
        {
            self.phase = Phase::Opening;
        }
        let previous = self.progress;
        match self.phase {
            Phase::Delay(remaining) => {
                // Leftover tick time is carried into motion when delay expires.
                if remaining > dt {
                    self.phase = Phase::Delay(remaining - dt);
                } else {
                    self.phase = Phase::Opening;
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
            for motion in &self.motions {
                if let Some(wall) = world.walls.get_mut(motion.wall) {
                    let shift = [
                        motion.displacement[0] * change,
                        motion.displacement[1] * change,
                    ];
                    wall.a[0] += shift[0];
                    wall.a[1] += shift[1];
                    wall.b[0] += shift[0];
                    wall.b[1] += shift[1];
                }
            }
        }
    }
}

/// Process state can be ticked by a client or a headless authoritative server.
/// The owning world's wall vector must retain its original order and length.
pub struct ProcessRuntime {
    processes: BTreeMap<u16, RuntimeProcess>,
    links: Vec<crate::process::Trigger>,
    previous_floor: Option<[u8; 2]>,
    keys: [bool; 3],
}

impl ProcessRuntime {
    pub fn new(script: &Script, world: &World) -> Self {
        Self {
            processes: script
                .processes
                .iter()
                .map(|(&id, process)| (id, RuntimeProcess::new(process, world)))
                .collect(),
            links: script.triggers.clone(),
            previous_floor: None,
            keys: [false; 3],
        }
    }
    /// Number of initially unlocked processes with fully supported wall motion.
    pub fn usable_count(&self) -> usize {
        self.processes
            .values()
            .filter(|p| {
                !p.locked && !p.required_keys.iter().any(|&key| key) && !p.motions.is_empty()
            })
            .count()
    }
    /// Synchronize the owning player's red, green and blue key state.
    pub fn set_keys(&mut self, keys: [bool; 3]) {
        self.keys = keys;
    }

    fn trigger(&mut self, id: u16) {
        let changes = self
            .processes
            .get_mut(&id)
            .and_then(|process| process.trigger(self.keys))
            .unwrap_or_default();
        for (target, locked) in changes {
            if let Some(process) = self.processes.get_mut(&target) {
                process.locked = locked;
            }
        }
    }
    /// Call after movement once per simulation step. `interact_pressed` must be
    /// true on the press edge only, not every frame while the key is held.
    pub fn tick(&mut self, world: &mut World, dt: f32, interact_pressed: bool) {
        if !dt.is_finite() || dt <= 0.0 {
            return;
        }
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
                let processes: Vec<u16> = self
                    .links
                    .iter()
                    .filter(|trigger| trigger.kind == "floor" && [trigger.x, trigger.y] == cell)
                    .map(|trigger| trigger.process)
                    .collect();
                for id in processes {
                    self.trigger(id);
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
                .filter(|link| link.kind == "link")
                .map(|link| {
                    let dx = f32::from(link.x) + 0.5 - p[0];
                    let dy = f32::from(link.y) + 0.5 - p[1];
                    (link.process, dx * dx + dy * dy)
                })
                .filter(|(_, distance)| *distance <= USE_RANGE * USE_RANGE)
                .min_by(|a, b| a.1.total_cmp(&b.1));
            if let Some((id, _)) = nearest {
                self.trigger(id);
            }
        }
        for process in self.processes.values_mut() {
            process.tick(world, dt);
        }
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
    fn use_chooses_nearest_link() {
        let script = Script::parse("#proc1\n#action\nmove 1 0 256 0\n#end\n#proc2\n#action\nmove 1 0 512 0\n#end\n#links\nlink 1 0 1\nlink 0 1 2\n#end\n");
        let mut world = world();
        world.player.position = [1.2, 0.5];
        let mut runtime = ProcessRuntime::new(&script, &world);
        runtime.tick(&mut world, 1.0 / 60.0, true);
        tick_for(&mut runtime, &mut world, 2.1);
        assert!((world.walls[0].a[0] - 2.0).abs() < 0.001);
    }
    #[test]
    fn unsupported_or_missing_targets_disable_the_entire_process() {
        for action in [
            "rotate 1 0 1 1",
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
}
