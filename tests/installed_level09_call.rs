//! Optional installed-data check: real level 09 floor link and two processes.
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

#[test]
fn level09_shared_floor_link_starts_both_authored_doors() {
    let explicit = std::env::var_os("OPENCHASM_TEST_CONTENT");
    let path = explicit
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("assets/csm.bin"));
    if !path.exists() && explicit.is_none() {
        return;
    }
    let mut archive = archive::Archive::open(&path).unwrap();
    let map = Map::parse(&archive.read("map/map.09").unwrap()).unwrap();
    let script =
        Script::parse(std::str::from_utf8(&archive.read("map/process.09").unwrap()).unwrap());
    let call = script.processes[&20]
        .actions
        .iter()
        .find(|a| a.name == "call")
        .unwrap();
    assert_eq!(call.arguments, ["21"]);
    for id in [20, 21] {
        assert!(script
            .triggers
            .iter()
            .any(|t| t.kind == "floor" && [t.x, t.y] == [55, 27] && t.process == id));
    }
    let mut walls = Vec::new();
    for x in 0..MAP_SIDE {
        for y in 0..MAP_SIDE {
            let w = map.wall(x, y).unwrap();
            if w.is_wall() && w.start != w.end {
                let cv = |p: [u16; 2]| p.map(|v| v as f32 / UNITS_PER_TILE);
                walls.push(Wall {
                    a: cv(w.start),
                    b: cv(w.end),
                    texture: w.texture_id,
                    blocks_movement: true,
                    cell: Some([x as u8, y as u8]),
                });
            }
        }
    }
    let mut world = World {
        walls,
        player: Player {
            position: [-100.0, -100.0],
            angle: 0.0,
        },
    };
    let cells = std::array::from_fn(|i| map.wall(i / MAP_SIDE, i % MAP_SIDE).unwrap().texture_id);
    let rot = world
        .walls
        .iter()
        .position(|w| w.cell == Some([56, 26]))
        .unwrap();
    let mov = world
        .walls
        .iter()
        .position(|w| w.cell == Some([54, 27]))
        .unwrap();
    let start_rot = world.walls[rot].a;
    let start_move = world.walls[mov].a;
    let mut runtime = ProcessRuntime::with_cell_data(
        &script,
        &world,
        ProcessCapabilities {
            wall_elevation: true,
            initial_cell_types: None,
        },
        &cells,
    );
    let events = runtime.activate_at("floor", [55, 27]);
    for id in [20, 21] {
        assert!(
            events
                .iter()
                .any(|e| e.process_id == id && e.outcome == ProcessOutcome::Accepted),
            "{events:?}"
        );
    }
    // Each door has its own authored delay; the `call` does not trigger 21.
    for _ in 0..180 {
        runtime.tick(&mut world, 1.0 / 60.0, false);
    }
    assert_ne!(world.walls[rot].a, start_rot);
    assert_eq!(world.walls[mov].a, start_move);
    for _ in 0..540 {
        runtime.tick(&mut world, 1.0 / 60.0, false);
    }
    assert!((world.walls[mov].a[0] - start_move[0] - 400.0 / 256.0).abs() < 0.01);
}
