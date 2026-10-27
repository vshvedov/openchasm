//! Optional end-to-end check against an owned full-game csm.bin.
#[allow(dead_code)]
#[path = "../src/archive.rs"]
mod archive;
use openchasm::{
    actors::{ActorDefinitions, ActorState},
    combat::RocketDefinitions,
    game::GameInventory,
    inventory::WeaponDefinitions,
    map::{Map, MAP_SIDE, UNITS_PER_TILE},
    object_combat::ObjectCombat,
    object_states::ObjectStates,
    objects::{self, ObjectDefinitions, ObjectKind},
    pickups::Difficulty,
    process::Script,
    process_runtime::{ProcessCapabilities, ProcessObject, ProcessRuntime},
    process_transaction::PresentationManifest,
    session::{LevelSetup, ScriptedObjects, Session, SessionInput},
    world::{Player, Wall, World},
};
use std::path::PathBuf;

#[test]
fn level19_switch_updates_ambient_and_unlocks_gate() {
    let explicit = std::env::var_os("OPENCHASM_TEST_CONTENT");
    let path = explicit
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("assets/csm.bin"));
    if !path.exists() && explicit.is_none() {
        eprintln!("No owned archive; level 02 scenario skipped.");
        return;
    }
    let mut archive = archive::Archive::open(&path).expect("open selected archive");
    let Ok(map_bytes) = archive.read("map/map.19") else {
        eprintln!("Archive has no level 16; scenario skipped.");
        return;
    };
    let map = Map::parse(&map_bytes).unwrap();
    let inf = archive.read("chasm.inf").unwrap();
    let level = archive.read("map/resource.19").unwrap();
    let script = Script::parse(&String::from_utf8_lossy(
        &archive.read("map/process.19").unwrap(),
    ));
    let definitions = ObjectDefinitions::parse(&inf, &level).unwrap();
    let instances = objects::instances(&map);
    let mut walls = Vec::new();
    for x in 0..MAP_SIDE {
        for y in 0..MAP_SIDE {
            let wall = map.wall(x, y).unwrap();
            if wall.is_wall() && wall.start != wall.end {
                let point = |p: [u16; 2]| {
                    [
                        f32::from(p[0]) / UNITS_PER_TILE,
                        f32::from(p[1]) / UNITS_PER_TILE,
                    ]
                };
                walls.push(Wall {
                    a: point(wall.start),
                    b: point(wall.end),
                    texture: wall.texture_id,
                    blocks_movement: true,
                    cell: Some([x as u8, y as u8]),
                });
            }
        }
    }
    let world = World {
        walls,
        player: Player {
            position: [10.5, 32.5],
            angle: 0.0,
        },
    };
    let cells: [u8; 4096] =
        std::array::from_fn(|i| map.wall(i / MAP_SIDE, i % MAP_SIDE).unwrap().texture_id);
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
    let processes = ProcessRuntime::with_staged_actions(
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
    let game = GameInventory::new(
        WeaponDefinitions::parse(&inf).unwrap(),
        &instances,
        &definitions,
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
        actor_definitions: ActorDefinitions::parse(&inf).unwrap(),
        game,
        rockets: RocketDefinitions::parse(&inf).unwrap(),
        shot_blocking: [true; 128],
        difficulty: Difficulty::Normal,
    });
    assert!(session
        .processes
        .static_wall_mask()
        .iter()
        .any(|is_static| *is_static));
    session.set_ambient_grid(openchasm::ambient::AmbientGrid::from_map(&map));
    session.set_objects(ObjectCombat::new(
        &instances,
        &definitions,
        &script,
        Difficulty::Normal,
    ));
    let states = ObjectStates::new(&instances, &definitions).unwrap();
    let mut manifest = PresentationManifest::default();
    let targets = [
        [37, 18],
        [39, 17],
        [40, 17],
        [42, 17],
        [36, 23],
        [38, 23],
        [40, 23],
        [42, 22],
    ];
    for cell in targets {
        manifest.allow(cell, 183);
    }
    session.set_scripted_objects(ScriptedObjects::new(
        states,
        definitions,
        manifest,
        &script,
        true,
    ));
    let before = session.ambient_grid().unwrap().clone();
    assert_eq!(before.cell([38, 18]), Some(5));
    let mut rejected = session.clone();
    rejected.scripted_objects.as_mut().unwrap().manifest = PresentationManifest::default();
    assert_eq!(
        rejected.processes.activate_at("floor", [38, 18])[0].outcome,
        openchasm::process_runtime::ProcessOutcome::Locked
    );
    let fail = rejected.step(SessionInput {
        interact: true,
        ..Default::default()
    });
    assert!(fail.object_transitions.is_empty());
    assert_eq!(rejected.ambient_grid().unwrap(), &before);
    assert_eq!(
        rejected.processes.activate_at("floor", [38, 18])[0].outcome,
        openchasm::process_runtime::ProcessOutcome::Locked
    );
    let accepted = session.step(SessionInput {
        interact: true,
        ..Default::default()
    });
    assert_eq!(
        accepted.object_transitions.len(),
        8,
        "{:?}",
        accepted.object_transitions
    );
    assert_eq!(
        accepted
            .object_transitions
            .iter()
            .map(|t| t.cell)
            .collect::<Vec<_>>(),
        targets
    );
    for cell in targets {
        assert_eq!(
            session.processes.cell_type([cell[0] as u8, cell[1] as u8]),
            Some(183)
        );
    }
    assert_eq!(session.ambient_grid().unwrap().cell([36, 18]), Some(0));
    assert_eq!(session.ambient_grid().unwrap().cell([43, 22]), Some(0));
    assert_eq!(session.ambient_grid().unwrap().cell([38, 18]), Some(0));
    assert!(session.wind.is_active(8));
    assert_ne!(
        session.processes.activate_at("floor", [38, 18])[0].outcome,
        openchasm::process_runtime::ProcessOutcome::Locked
    );
}
