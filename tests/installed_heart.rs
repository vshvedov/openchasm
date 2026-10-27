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
    process_runtime::{ProcessCapabilities, ProcessObject, ProcessOutcome, ProcessRuntime},
    process_transaction::PresentationManifest,
    session::{LevelSetup, ScriptedObjects, Session, SessionInput},
    world::{Player, Wall, World},
};
use std::path::PathBuf;

#[test]
fn destroying_level_16_heart_runs_its_atomic_process() {
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
    let Ok(map_bytes) = archive.read("map/map.16") else {
        eprintln!("Archive has no level 16; scenario skipped.");
        return;
    };
    let map = Map::parse(&map_bytes).unwrap();
    let inf = archive.read("chasm.inf").unwrap();
    let level = archive.read("map/resource.16").unwrap();
    let script = Script::parse(&String::from_utf8_lossy(
        &archive.read("map/process.16").unwrap(),
    ));
    let definitions = ObjectDefinitions::parse(&inf, &level).unwrap();
    let instances = objects::instances(&map);
    let heart = instances
        .iter()
        .find(|object| object.grid_cell == [27, 47])
        .expect("authored heart");
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
            position: [heart.position[0] - 0.2, heart.position[1]],
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
    let mut rockets = RocketDefinitions::parse(&inf).unwrap();
    let weapon_id = game.weapons.0[0].rocket_type as usize;
    if rockets.0.len() <= weapon_id {
        rockets.0.resize(
            weapon_id + 1,
            RocketDefinition {
                model_name: String::new(),
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
    let mut without_ambient = session.clone();
    session.set_ambient_grid(openchasm::ambient::AmbientGrid::from_map(&map));
    let mut blocked = session.clone();
    let before = blocked
        .scripted_objects
        .as_ref()
        .unwrap()
        .states
        .get([27, 47])
        .unwrap()
        .health;
    let rejected = blocked.step(SessionInput {
        fire: true,
        ..Default::default()
    });
    assert_eq!(
        rejected.object_hits,
        [([27, 47], false)],
        "{:?}",
        rejected.shot
    );
    assert!(rejected.object_transitions.is_empty());
    assert_eq!(
        blocked
            .scripted_objects
            .as_ref()
            .unwrap()
            .states
            .get([27, 47])
            .unwrap()
            .health,
        before
    );
    assert!(blocked
        .objects
        .as_ref()
        .unwrap()
        .is_active([27, 47])
        .unwrap());
    assert_eq!(
        blocked.processes.activate_at("floor", [27, 47])[0].outcome,
        ProcessOutcome::Locked
    );
    let replacements = [[27, 46], [28, 47], [28, 48], [27, 48], [29, 48]];
    let scripted = session.scripted_objects.as_mut().unwrap();
    for cell in replacements {
        let raw = scripted
            .states
            .get(cell)
            .expect("authored replacement target")
            .raw_type;
        scripted
            .manifest
            .allow(cell, if cell == [29, 48] { 200 } else { raw + 1 });
        without_ambient
            .scripted_objects
            .as_mut()
            .unwrap()
            .manifest
            .allow(cell, if cell == [29, 48] { 200 } else { raw + 1 });
    }
    let rejected = without_ambient.step(SessionInput {
        fire: true,
        ..Default::default()
    });
    assert_eq!(rejected.object_hits, [([27, 47], false)]);
    assert!(rejected.object_transitions.is_empty());
    assert_eq!(
        without_ambient.processes.activate_at("floor", [27, 47])[0].outcome,
        ProcessOutcome::Locked
    );
    let events = session.step(SessionInput {
        fire: true,
        ..Default::default()
    });
    assert_eq!(events.object_hits, [([27, 47], true)], "{:?}", events.shot);
    assert_eq!(
        events
            .object_transitions
            .iter()
            .map(|t| t.cell)
            .collect::<Vec<_>>(),
        replacements
    );
    assert_eq!(events.object_transitions[4].new_type, 200);
    assert_eq!(events.quake_changes.len(), 1);
    assert_eq!(
        session
            .scripted_objects
            .as_ref()
            .unwrap()
            .quake
            .cell([17, 40]),
        Some(10000)
    );
    assert_eq!(session.wind.cell([25, 47]), Some([12, 0]));
    assert_eq!(session.wind.cell([21, 47]), Some([10, 0]));
    assert_eq!(session.wind.cell([17, 47]), Some([8, 0]));
    let mut delayed_quake_reset = false;
    for _ in 0..125 {
        let delayed = session.step(SessionInput::default());
        delayed_quake_reset |= delayed.quake_changes.iter().any(|zone| zone.power == 0);
    }
    assert!(
        delayed_quake_reset,
        "procedure 8 should reset quake after its delay"
    );
    assert_eq!(
        session
            .scripted_objects
            .as_ref()
            .unwrap()
            .quake
            .cell([17, 40]),
        Some(0)
    );
    assert_eq!(session.ambient_grid().unwrap().cell([17, 40]), Some(1));
    assert_eq!(
        session.processes.activate_at("floor", [27, 47])[0].outcome,
        ProcessOutcome::Accepted
    );
}
