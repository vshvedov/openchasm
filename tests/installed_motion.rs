//! Optional Military Base process checks against the user's installed archive.
//! CI and source distributions do not need proprietary data; set
//! OPENCHASM_TEST_CONTENT to an owned 2022 Steam `csm.bin` to run explicitly.
#[allow(dead_code)]
#[path = "../src/archive.rs"]
mod archive;

use openchasm::{
    map::{Map, MAP_SIDE, UNITS_PER_TILE},
    process::Script,
    process_runtime::{ProcessCapabilities, ProcessOutcome, ProcessRuntime},
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
        eprintln!("No local archive: Military Base process checks skipped. Set OPENCHASM_TEST_CONTENT to enable them.");
        return None;
    }
    let mut archive = archive::Archive::open(&path).expect("open explicitly selected game archive");
    let map = Map::parse(&archive.read("map/map.02").expect("read Military Base map"))
        .expect("parse Military Base map");
    let process = archive
        .read("map/process.02")
        .expect("read Military Base process file");
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

#[test]
fn military_base_axis_door_and_green_key_exit_approach_activate() {
    let Some((mut world, script)) = fixture() else {
        return;
    };
    let cells = [0u8; 4096];
    let mut runtime = ProcessRuntime::with_cell_data(
        &script,
        &world,
        ProcessCapabilities {
            wall_elevation: true,
            initial_cell_types: None,
        },
        &cells,
    );
    let door = runtime.activate_at("floor", [17, 5]);
    assert!(
        door.iter()
            .any(|e| e.process_id == 5 && e.outcome == ProcessOutcome::Accepted),
        "{door:?}"
    );
    runtime.set_keys([false, true, false]);
    let exit = runtime.activate_at("floor", [26, 13]);
    assert!(
        exit.iter()
            .any(|e| e.process_id == 31 && e.outcome == ProcessOutcome::Accepted),
        "{exit:?}"
    );
    for _ in 0..360 {
        runtime.tick(&mut world, 1.0 / 60.0, false);
    }
    let i = world
        .walls
        .iter()
        .position(|w| w.cell == Some([26, 14]))
        .unwrap();
    assert!((runtime.wall_elevation(i).unwrap() - 90.0).abs() < 0.01);
}
