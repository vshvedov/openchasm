//! Optional real-data runtime preflight; this does not apply wall materials.
#[allow(dead_code)]
#[path = "../src/archive.rs"]
mod archive;
use openchasm::{
    map::{Map, MAP_SIDE, UNITS_PER_TILE},
    objects::{self, ObjectDefinitions, ObjectKind},
    process::Script,
    process_runtime::{
        ImmediateAction, ProcessCapabilities, ProcessObject, ProcessOutcome, ProcessRuntime,
        RuntimeWallTransition,
    },
    world::{Player, Wall, World},
};
use std::path::PathBuf;
#[test]
fn real_level18_wall_and_object_motion_requires_wall_preflight() {
    let explicit = std::env::var_os("OPENCHASM_TEST_CONTENT");
    let path = explicit
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("assets/csm.bin"));
    if !path.exists() && explicit.is_none() {
        return;
    }
    let mut archive = archive::Archive::open(&path).unwrap();
    let map = Map::parse(&archive.read("map/map.18").unwrap()).unwrap();
    let script =
        Script::parse(std::str::from_utf8(&archive.read("map/process.18").unwrap()).unwrap());
    let defs = ObjectDefinitions::parse(
        &archive.read("chasm.inf").unwrap(),
        &archive.read("map/resource.18").unwrap(),
    )
    .unwrap();
    let instances = objects::instances(&map);
    let mut walls = Vec::new();
    for x in 0..MAP_SIDE {
        for y in 0..MAP_SIDE {
            let w = map.wall(x, y).unwrap();
            if w.is_wall() && w.start != w.end {
                let cv = |p: [u16; 2]| p.map(|v| v as f32 / UNITS_PER_TILE);
                walls.push(Wall {
                    cell: Some([x as u8, y as u8]),
                    a: cv(w.start),
                    b: cv(w.end),
                    texture: w.texture_id,
                    blocks_movement: true,
                });
            }
        }
    }
    let world = World {
        walls,
        player: Player {
            position: [-100.0, -100.0],
            angle: 0.0,
        },
    };
    let cells = std::array::from_fn(|i| map.wall(i / MAP_SIDE, i % MAP_SIDE).unwrap().texture_id);
    let poses: Vec<_> = instances
        .iter()
        .filter(|o| o.kind == ObjectKind::LevelObject && o.difficulty_flags & 2 != 0)
        .filter_map(|o| {
            let d = defs.definition(o)?;
            Some(ProcessObject {
                cell: [o.grid_cell[0] as u8, o.grid_cell[1] as u8],
                position: o.position,
                angle_radians: o.angle_radians,
                elevation_raw: 0.0,
                blocking_radius: (d.collision_radius.max(0) as f32 / 256.0).min(2.0),
            })
        })
        .collect();
    let mut runtime = ProcessRuntime::with_wall_changes(
        &script,
        &world,
        ProcessCapabilities {
            wall_elevation: true,
            initial_cell_types: None,
        },
        &cells,
        &poses,
    )
    .unwrap();
    let index = world
        .walls
        .iter()
        .position(|w| w.cell == Some([20, 17]))
        .unwrap();
    assert_eq!(world.walls[index].texture, 1);
    let mut prepared = runtime
        .prepare_at("link", [17, 22], 2)
        .expect("authored mixed motion must compile");
    assert!(prepared.actions().contains(&ImmediateAction::WallChange {
        cell: [20, 17],
        raw_type: 115
    }));
    prepared
        .stage_wall_transitions(&[RuntimeWallTransition {
            cell: [20, 17],
            wall_index: index,
            previous_type: 1,
            new_type: 115,
        }])
        .unwrap();
    assert_eq!(prepared.commit().outcome, ProcessOutcome::Accepted);
    assert_eq!(runtime.cell_type([20, 17]), Some(115));
}
