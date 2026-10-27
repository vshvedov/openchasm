//! Optional end-to-end check against an owned full-game csm.bin.
#[allow(dead_code)]
#[path = "../src/archive.rs"]
mod archive;
use openchasm::{
    actors::{ActorDefinitions, ActorState},
    combat::{RocketDefinition, RocketDefinitions},
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
fn level08_destroy_arms_delayed_hazards_and_switch_changes() {
    let explicit = std::env::var_os("OPENCHASM_TEST_CONTENT");
    let path = explicit
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("assets/csm.bin"));
    if !path.exists() && explicit.is_none() {
        eprintln!("No owned archive; level 16 scenario skipped.");
        return;
    }
    let mut archive = archive::Archive::open(&path).expect("open selected archive");
    let Ok(map_bytes) = archive.read("map/map.08") else {
        eprintln!("Archive has no level 08; scenario skipped.");
        return;
    };
    let map = Map::parse(&map_bytes).unwrap();
    let inf = archive.read("chasm.inf").unwrap();
    let level = archive.read("map/resource.08").unwrap();
    let script = Script::parse(&String::from_utf8_lossy(
        &archive.read("map/process.08").unwrap(),
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
            position: [6.0, 25.5],
            angle: std::f32::consts::FRAC_PI_2,
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
    let mut rockets = RocketDefinitions::parse(&inf).unwrap();
    let weapon_id = game.weapons.0[0].rocket_type as usize;
    if rockets.0.len() <= weapon_id {
        rockets.0.resize(
            weapon_id + 1,
            RocketDefinition {
                model_name: String::new(),
                animation_name: String::new(),
                power: 0,
                raw_fields: vec![],
            },
        );
    }
    rockets.0[weapon_id].power = 100000;
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
        rockets,
        shot_blocking: [true; 128],
        difficulty: Difficulty::Normal,
    });
    session.set_objects(ObjectCombat::new(
        &instances,
        &definitions,
        &script,
        Difficulty::Normal,
    ));
    let states = ObjectStates::new(&instances, &definitions).unwrap();
    session.set_scripted_objects(ScriptedObjects::new(
        states,
        definitions,
        PresentationManifest::default(),
        &script,
        true,
    ));
    for cell in [[10, 27], [11, 27], [11, 28], [13, 27]] {
        session
            .scripted_objects
            .as_mut()
            .unwrap()
            .manifest
            .allow(cell, 212);
    }
    for (cell, raw) in [
        ([6, 28], 202),
        ([7, 27], 199),
        ([7, 28], 206),
        ([8, 28], 208),
        ([8, 27], 210),
    ] {
        session
            .scripted_objects
            .as_mut()
            .unwrap()
            .manifest
            .allow(cell, raw);
    }
    session.game.inventory.ammo[0] = 100;
    let mut rejected = session.clone();
    rejected.scripted_objects.as_mut().unwrap().manifest = PresentationManifest::default();
    let events = rejected.step(SessionInput {
        fire: true,
        ..Default::default()
    });
    assert!(
        events.object_hits.contains(&([6, 27], false)),
        "{:?}",
        events.shot
    );
    assert_eq!(rejected.processes.ready_delayed(), Vec::<u16>::new());
    let first = session.step(SessionInput {
        fire: true,
        ..Default::default()
    });
    assert!(
        first.object_hits.contains(&([6, 27], true)),
        "{:?}",
        first.shot
    );
    assert!(first.quake_changes.is_empty());
    assert_eq!(session.processes.cell_type([6, 28]), Some(202));
    for _ in 0..60 {
        session.step(SessionInput::default());
    }
    let mut rejected_second = session.clone();
    rejected_second.scripted_objects.as_mut().unwrap().manifest = PresentationManifest::default();
    let rejected_hit = rejected_second.step(SessionInput {
        fire: true,
        ..Default::default()
    });
    assert!(
        rejected_hit.object_hits.contains(&([6, 28], false)),
        "{:?}",
        rejected_hit.shot
    );
    assert!(rejected_second.processes.ready_delayed().is_empty());
    assert_eq!(rejected_second.processes.cell_type([6, 28]), Some(202));
    let second = session.step(SessionInput {
        fire: true,
        ..Default::default()
    });
    assert!(
        second.object_hits.contains(&([6, 28], true)),
        "{:?}",
        second.shot
    );
    assert!(second.quake_changes.is_empty());
    assert!(second.object_transitions.is_empty());
    let mut retry = session.clone();
    retry.scripted_objects.as_mut().unwrap().manifest = PresentationManifest::default();
    for _ in 0..130 {
        let e = retry.step(SessionInput::default());
        assert!(e.object_transitions.is_empty());
        assert!(e.quake_changes.is_empty());
    }
    assert!(retry.processes.ready_delayed().contains(&11));
    retry
        .scripted_objects
        .as_mut()
        .unwrap()
        .manifest
        .allow([10, 27], 212);
    let resumed = retry.step(SessionInput::default());
    assert!(resumed
        .object_transitions
        .iter()
        .any(|t| t.cell == [10, 27]));
    assert_eq!(resumed.quake_changes.len(), 3);
    assert!(!retry.processes.ready_delayed().contains(&11));
    for _ in 0..110 {
        let e = session.step(SessionInput::default());
        assert!(e.quake_changes.is_empty());
        assert!(e.object_transitions.is_empty());
    }
    let mut q11 = false;
    let mut p11 = false;
    for _ in 0..20 {
        let e = session.step(SessionInput::default());
        q11 |= !e.quake_changes.is_empty();
        p11 |= e
            .object_transitions
            .iter()
            .any(|t| t.cell == [10, 27] && t.new_type == 212);
    }
    assert!(q11 && p11, "proc11 should commit around 2s");
    let mut p12 = 0;
    for _ in 0..70 {
        let e = session.step(SessionInput::default());
        p12 += e
            .object_transitions
            .iter()
            .filter(|t| [[11, 27], [11, 28]].contains(&t.cell) && t.new_type == 212)
            .count();
    }
    assert_eq!(p12, 2, "proc12 should commit around 3s");
    let mut p13 = false;
    for _ in 0..70 {
        let e = session.step(SessionInput::default());
        p13 |= e
            .object_transitions
            .iter()
            .any(|t| t.cell == [13, 27] && t.new_type == 212);
    }
    assert!(p13, "proc13 should commit around 4s");
}
