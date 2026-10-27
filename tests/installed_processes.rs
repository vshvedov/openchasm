//! Optional Power Station process checks against the user's installed archive.
//! CI and source distributions do not need proprietary data; set
//! OPENCHASM_TEST_CONTENT to an owned 2022 Steam `csm.bin` to run explicitly.
#[allow(dead_code)]
#[path = "../src/archive.rs"]
mod archive;

use openchasm::{
    map::{Map, MAP_SIDE, UNITS_PER_TILE},
    process::Script,
    process_runtime::{ProcessCapabilities, ProcessEffect, ProcessOutcome, ProcessRuntime},
    world::{Player, Wall, World},
};
use std::path::PathBuf;

fn fixture() -> Option<(World, Script)> {
    let explicit = std::env::var_os("OPENCHASM_TEST_CONTENT");
    let path = explicit
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("assets/csm.bin"));
    if !path.exists() && explicit.is_none() {
        eprintln!("No local archive: Power Station process checks skipped. Set OPENCHASM_TEST_CONTENT to enable them.");
        return None;
    }
    let mut archive = archive::Archive::open(&path).expect("open explicitly selected game archive");
    let map = Map::parse(&archive.read("map/map.01").expect("read Power Station map"))
        .expect("parse Power Station map");
    let process = archive
        .read("map/process.01")
        .expect("read Power Station process file");
    let script = Script::parse(std::str::from_utf8(&process).expect("process text is UTF-8"));
    assert!(script.diagnostics.is_empty(), "{:?}", script.diagnostics);
    let mut walls = Vec::new();
    for x in 0..MAP_SIDE {
        for y in 0..MAP_SIDE {
            let wall = map.wall(x, y).expect("valid map cell");
            if wall.is_wall() && wall.start != wall.end {
                let convert = |point: [u16; 2]| {
                    [
                        f32::from(point[0]) / UNITS_PER_TILE,
                        f32::from(point[1]) / UNITS_PER_TILE,
                    ]
                };
                walls.push(Wall {
                    a: convert(wall.start),
                    b: convert(wall.end),
                    texture: wall.texture_id,
                    blocks_movement: true,
                    cell: Some([x as u8, y as u8]),
                });
            }
        }
    }
    // Test process operation without an unrelated player blocking a moving wall.
    let world = World {
        walls,
        player: Player {
            position: [-100.0, -100.0],
            angle: 0.0,
        },
    };
    Some((world, script))
}

fn accepted(events: &[openchasm::process_runtime::ProcessEvent], id: u16) {
    let event = events
        .iter()
        .find(|event| event.process_id == id)
        .expect("expected real script link");
    assert_eq!(event.outcome, ProcessOutcome::Accepted, "process {id}");
}
fn wall_index(world: &World, cell: [u8; 2]) -> usize {
    world
        .walls
        .iter()
        .position(|wall| wall.cell == Some(cell))
        .expect("expected real wall at target cell")
}

#[test]
fn power_switch_starts_animation_and_unlocks_power_router_door() {
    let Some((mut world, script)) = fixture() else {
        return;
    };
    let mut runtime = ProcessRuntime::with_capabilities(
        &script,
        &world,
        ProcessCapabilities {
            wall_elevation: true,
            initial_cell_types: None,
        },
    );
    let door_cell = [28, 34];
    let index = wall_index(&world, door_cell);
    let initially_locked = runtime.activate_at("floor", [26, 33]);
    assert!(initially_locked
        .iter()
        .any(|event| event.process_id == 5 && event.outcome == ProcessOutcome::Locked));
    accepted(&runtime.activate_at("link", [37, 40]), 7);
    assert!(runtime
        .take_effects()
        .iter()
        .any(|effect| matches!(effect, ProcessEffect::PlayAnimation { object_id: 58 })));
    accepted(&runtime.activate_at("floor", [26, 33]), 5);
    let mut max_raw = 0.0_f32;
    for _ in 0..180 {
        runtime.tick(&mut world, 1.0 / 60.0, false);
        for effect in runtime.take_effects() {
            if let ProcessEffect::WallElevation {
                wall_index,
                offset_raw,
            } = effect
            {
                if wall_index == index {
                    max_raw = max_raw.max(offset_raw);
                }
            }
        }
    }
    assert!(
        (max_raw - 80.0).abs() < 0.01,
        "door frame {door_cell:?} reached {max_raw}, expected 80 raw units"
    );
}

#[test]
fn fence_switches_raise_all_three_real_walls() {
    let Some((mut world, script)) = fixture() else {
        return;
    };
    let mut runtime = ProcessRuntime::with_capabilities(
        &script,
        &world,
        ProcessCapabilities {
            wall_elevation: true,
            initial_cell_types: None,
        },
    );
    let cells = [[38, 41], [27, 28], [39, 24]];
    let indices = cells.map(|cell| wall_index(&world, cell));
    accepted(&runtime.activate_at("link", [44, 48]), 16);
    accepted(&runtime.activate_at("link", [43, 23]), 17);
    let mut max_raw = [0.0_f32; 3];
    for _ in 0..180 {
        runtime.tick(&mut world, 1.0 / 60.0, false);
        for effect in runtime.take_effects() {
            if let ProcessEffect::WallElevation {
                wall_index,
                offset_raw,
            } = effect
            {
                for (i, &index) in indices.iter().enumerate() {
                    if index == wall_index {
                        max_raw[i] = max_raw[i].max(offset_raw);
                    }
                }
            }
        }
    }
    for (cell, height) in cells.iter().zip(max_raw) {
        assert!(
            (height - 96.0).abs() < 0.01,
            "fence {cell:?} reached {height}, expected 96 raw units"
        );
    }
}

#[test]
fn real_empty_cell_exit_actions_complete_levels_05_and_17() {
    use openchasm::mission::{MissionEvent, MissionPlan, MissionState};
    let explicit = std::env::var_os("OPENCHASM_TEST_CONTENT");
    let path = explicit
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("assets/csm.bin"));
    if !path.exists() && explicit.is_none() {
        return;
    }
    let mut archive = archive::Archive::open(&path).unwrap();
    for (id, exit, cell) in [("05", 14, [53, 31]), ("17", 7, [16, 30])] {
        let map = Map::parse(&archive.read(&format!("map/map.{id}")).unwrap()).unwrap();
        let text = String::from_utf8(archive.read(&format!("map/process.{id}")).unwrap()).unwrap();
        let script = Script::parse(&text);
        let cells: [u8; 4096] = std::array::from_fn(|index| {
            map.wall(index / MAP_SIDE, index % MAP_SIDE)
                .unwrap()
                .texture_id
        });
        assert_eq!(cells[0], 0);
        let world = World {
            walls: vec![],
            player: Player {
                position: [-100.0, -100.0],
                angle: 0.0,
            },
        };
        let mut runtime =
            ProcessRuntime::with_cell_data(&script, &world, ProcessCapabilities::default(), &cells);
        let attempts = runtime.activate_at("floor", cell);
        assert!(
            attempts
                .iter()
                .any(|event| event.process_id == exit && event.outcome == ProcessOutcome::Accepted),
            "{id}: {attempts:?}"
        );
        let mut mission = MissionState::new(MissionPlan::parse(&script, &text));
        mission.attempt(exit, true);
        assert_eq!(mission.advance(2.0), vec![MissionEvent::LevelComplete]);
    }
}

#[test]
fn level_11_switch_moves_its_authored_object() {
    use openchasm::objects;
    use openchasm::process_runtime::ProcessObject;
    let explicit = std::env::var_os("OPENCHASM_TEST_CONTENT");
    let path = explicit
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("assets/csm.bin"));
    if !path.exists() && explicit.is_none() {
        return;
    }
    let mut archive = archive::Archive::open(&path).expect("open game archive");
    let map = Map::parse(&archive.read("map/map.11").unwrap()).unwrap();
    let text = String::from_utf8(archive.read("map/process.11").unwrap()).unwrap();
    let script = Script::parse(&text);
    let cells = std::array::from_fn(|index| {
        map.wall(index / MAP_SIDE, index % MAP_SIDE)
            .unwrap()
            .texture_id
    });
    let instance = objects::instances(&map)
        .into_iter()
        .find(|instance| instance.grid_cell == [27, 10])
        .expect("authored moving object");
    let initial = instance.position;
    let mut world = World {
        walls: vec![],
        player: Player {
            position: [-100.0, -100.0],
            angle: 0.0,
        },
    };
    let mut runtime = ProcessRuntime::with_objects_and_hazards(
        &script,
        &world,
        ProcessCapabilities::default(),
        &cells,
        &[ProcessObject {
            cell: [27, 10],
            position: initial,
            angle_radians: instance.angle_radians,
            elevation_raw: 0.0,
            blocking_radius: 0.0,
        }],
    )
    .unwrap();
    accepted(&runtime.activate_at("link", [44, 15]), 7);
    let mut final_position = initial;
    let mut updates = 0;
    for _ in 0..180 {
        runtime.tick(&mut world, 1.0 / 60.0, false);
        for effect in runtime.take_effects() {
            if let ProcessEffect::ObjectPose {
                cell: [27, 10],
                position,
                ..
            } = effect
            {
                final_position = position;
                updates += 1;
            }
        }
    }
    assert!(updates > 1, "expected incremental pose updates");
    assert!((final_position[0] - initial[0]).abs() < 0.001);
    assert!((final_position[1] - initial[1] - 2.0).abs() < 0.001);
}
