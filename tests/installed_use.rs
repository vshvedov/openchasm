//! Optional use interaction check against an owned full-game archive.
#[allow(dead_code)]
#[path = "../src/archive.rs"]
mod archive;
use openchasm::{
    map::{Map, MAP_SIDE, UNITS_PER_TILE},
    objects,
    process::Script,
    process_runtime::{ProcessCapabilities, ProcessObject, ProcessOutcome, ProcessRuntime},
    world::{Player, Wall, World},
};
use std::path::PathBuf;

#[test]
fn military_base_offset_switch_unlocks_gate_from_its_model_position() {
    let explicit = std::env::var_os("OPENCHASM_TEST_CONTENT");
    let path = explicit
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(|| "assets/csm.bin".into());
    if !path.exists() && explicit.is_none() {
        eprintln!("No archive; installed switch test skipped.");
        return;
    }
    let mut archive = archive::Archive::open(&path).unwrap();
    let map = Map::parse(&archive.read("map/map.02").unwrap()).unwrap();
    let script = Script::parse(&String::from_utf8_lossy(
        &archive.read("map/process.02").unwrap(),
    ));
    let instances = objects::instances(&map);
    let switch = instances.iter().find(|o| o.grid_cell == [16, 11]).unwrap();
    let center_distance = (switch.position[0] - 16.5).hypot(switch.position[1] - 11.5);
    assert!(center_distance > 1.25);
    let cells = std::array::from_fn(|i| map.wall(i / MAP_SIDE, i % MAP_SIDE).unwrap().texture_id);
    let walls = (0..MAP_SIDE)
        .flat_map(|x| {
            let map = &map;
            (0..MAP_SIDE).filter_map(move |y| {
                let wall = map.wall(x, y).unwrap();
                if !wall.is_wall() || wall.start == wall.end {
                    return None;
                }
                let convert =
                    |p: [u16; 2]| [p[0] as f32 / UNITS_PER_TILE, p[1] as f32 / UNITS_PER_TILE];
                Some(Wall {
                    a: convert(wall.start),
                    b: convert(wall.end),
                    texture: wall.texture_id,
                    blocks_movement: true,
                    cell: Some([x as u8, y as u8]),
                })
            })
        })
        .collect();
    let mut world = World {
        walls,
        player: Player {
            position: switch.position,
            angle: 0.0,
        },
    };
    let pose = ProcessObject {
        cell: [16, 11],
        position: switch.position,
        angle_radians: switch.angle_radians,
        elevation_raw: 0.0,
        blocking_radius: 0.0,
    };
    let mut runtime = ProcessRuntime::with_wind(
        &script,
        &world,
        ProcessCapabilities {
            wall_elevation: true,
            initial_cell_types: None,
        },
        &cells,
        &[pose],
    )
    .unwrap();
    let before = runtime.activate_at("floor", [20, 12]);
    assert!(before
        .iter()
        .any(|e| e.process_id == 8 && e.outcome == ProcessOutcome::Locked));
    let events = runtime.tick(&mut world, 1.0 / 60.0, true);
    assert!(
        events
            .iter()
            .any(|e| e.process_id == 10 && e.outcome == ProcessOutcome::Accepted),
        "{events:?}"
    );
    let after = runtime.activate_at("floor", [20, 12]);
    assert!(
        after
            .iter()
            .any(|e| e.process_id == 8 && e.outcome == ProcessOutcome::Accepted),
        "{after:?}"
    );
}
