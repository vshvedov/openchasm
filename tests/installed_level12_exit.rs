//! Optional installed-data process test. Player and collision clearance are
//! controlled here; this is not a full traversal or a rendering test.
#[allow(dead_code)]
#[path = "../src/archive.rs"]
mod archive;
use openchasm::{
    map::{Map, MAP_SIDE, UNITS_PER_TILE},
    object_combat::ObjectCombat,
    object_states::ObjectStates,
    objects::{self, ObjectDefinitions, ObjectKind},
    pickups::Difficulty,
    process::Script,
    process_runtime::{ProcessCapabilities, ProcessObject, ProcessOutcome, ProcessRuntime},
    process_transaction::{PresentationManifest, ProcessTransaction},
    quake::QuakeField,
    world::{Player, Wall, World},
};
use std::path::PathBuf;

#[test]
fn exit_unlocker_accepts_fifth_move_argument_and_commits_replacement() {
    let explicit = std::env::var_os("OPENCHASM_TEST_CONTENT");
    let path = explicit
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("assets/csm.bin"));
    if !path.exists() && explicit.is_none() {
        return;
    }
    let mut archive = archive::Archive::open(&path).unwrap();
    let map = Map::parse(&archive.read("map/map.12").unwrap()).unwrap();
    let script =
        Script::parse(std::str::from_utf8(&archive.read("map/process.12").unwrap()).unwrap());
    let definitions = ObjectDefinitions::parse(
        &archive.read("chasm.inf").unwrap(),
        &archive.read("map/resource.12").unwrap(),
    )
    .unwrap();
    let instances = objects::instances(&map);
    assert!(script.processes[&27]
        .actions
        .iter()
        .any(|a| a.name == "move" && a.arguments == ["29", "33", "0", "-510", "64"]));
    let mut walls = Vec::new();
    for x in 0..MAP_SIDE {
        for y in 0..MAP_SIDE {
            let wall = map.wall(x, y).unwrap();
            if wall.is_wall() && wall.start != wall.end {
                let cv = |p: [u16; 2]| p.map(|v| v as f32 / UNITS_PER_TILE);
                walls.push(Wall {
                    a: cv(wall.start),
                    b: cv(wall.end),
                    texture: wall.texture_id,
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
    let poses: Vec<_> = instances
        .iter()
        .filter(|o| o.kind == ObjectKind::LevelObject && o.difficulty_flags & 2 != 0)
        .filter_map(|o| {
            let def = definitions.definition(o)?;
            Some(ProcessObject {
                cell: [o.grid_cell[0] as u8, o.grid_cell[1] as u8],
                position: o.position,
                angle_radians: o.angle_radians,
                elevation_raw: 0.0,
                blocking_radius: if (141..=143).contains(&def.action_code) {
                    0.0
                } else {
                    (def.collision_radius.max(0) as f32 / 256.0).min(2.0)
                },
            })
        })
        .collect();
    let mut rt = ProcessRuntime::with_staged_actions(
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
    rt.set_keys([true; 3]);
    assert_eq!(
        rt.prepare_at("floor", [30, 31], 29).err(),
        Some(ProcessOutcome::Locked)
    );
    let mut states = ObjectStates::new(&instances, &definitions).unwrap();
    let mut combat = ObjectCombat::new(&instances, &definitions, &script, Difficulty::Normal);
    let mut quake = QuakeField::new();
    let mut manifest = PresentationManifest::default();
    manifest.allow([31, 32], 165);
    let prepared = rt
        .prepare_at("floor", [35, 31], 27)
        .expect("real unlocker must be supported");
    let transaction = ProcessTransaction::prepare(
        prepared,
        &mut states,
        &mut combat,
        &mut quake,
        &definitions,
        &manifest,
        |_, _, _| true,
    )
    .expect("preflight real replacement");
    assert_eq!(transaction.transitions().len(), 1);
    assert_eq!(
        transaction.commit().unwrap().outcome,
        ProcessOutcome::Accepted
    );
    assert_eq!(states.get([31, 32]).unwrap().raw_type, 165);
    assert!(rt.prepare_at("floor", [30, 31], 29).is_ok());
    let index = world
        .walls
        .iter()
        .position(|w| w.cell == Some([29, 33]))
        .unwrap();
    let before = world.walls[index].a;
    for _ in 0..600 {
        rt.tick(&mut world, 1.0 / 60.0, false);
    }
    assert!((world.walls[index].a[1] - before[1] + 510.0 / 256.0).abs() < 0.01);
}
