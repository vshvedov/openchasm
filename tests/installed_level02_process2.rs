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
fn military_base_floor_replaces_grates_and_moves_doors_atomically() {
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
    let Ok(map_bytes) = archive.read("map/map.02") else {
        eprintln!("Archive has no level 16; scenario skipped.");
        return;
    };
    let map = Map::parse(&map_bytes).unwrap();
    let inf = archive.read("chasm.inf").unwrap();
    let level = archive.read("map/resource.02").unwrap();
    let script = Script::parse(&String::from_utf8_lossy(
        &archive.read("map/process.02").unwrap(),
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
            position: [22.5, 27.5],
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
    session.set_objects(ObjectCombat::new(
        &instances,
        &definitions,
        &script,
        Difficulty::Normal,
    ));
    let states = ObjectStates::new(&instances, &definitions).unwrap();
    let mut manifest = PresentationManifest::default();
    for (cell, raw) in [([29, 27], 184), ([27, 28], 185), ([29, 28], 186)] {
        manifest.allow(cell, raw);
    }
    session.set_scripted_objects(ScriptedObjects::new(
        states,
        definitions,
        manifest,
        &script,
        true,
    ));
    let wall = session
        .world
        .walls
        .iter()
        .position(|wall| wall.cell == Some([23, 27]))
        .unwrap();
    let initial = session.world.walls[wall].a;
    let mut rejected = session.clone();
    rejected.scripted_objects.as_mut().unwrap().manifest = PresentationManifest::default();
    assert!(rejected
        .step(SessionInput::default())
        .object_transitions
        .is_empty());
    rejected.world.player.position = [40.5, 40.5];
    for _ in 0..180 {
        rejected.step(SessionInput::default());
    }
    assert_eq!(
        rejected.world.walls[wall].a, initial,
        "failed preflight must not open any door"
    );
    assert_eq!(
        rejected
            .scripted_objects
            .as_ref()
            .unwrap()
            .states
            .get([29, 27])
            .unwrap()
            .raw_type,
        185
    );
    let events = session.step(SessionInput::default());
    assert_eq!(
        events
            .object_transitions
            .iter()
            .map(|t| (t.cell, t.previous_type, t.new_type))
            .collect::<Vec<_>>(),
        vec![
            ([29, 27], 185, 184),
            ([27, 28], 187, 185),
            ([29, 28], 187, 186)
        ]
    );
    let scripted = session.scripted_objects.as_ref().unwrap();
    for (cell, name) in [
        ([29, 27], "setka6~.3o"),
        ([27, 28], "setka~.3o"),
        ([29, 28], "setka2~.3o"),
    ] {
        assert_eq!(scripted.states.definition(cell).unwrap().model_name, name);
    }
    // Move clear so the ordinary obstruction check cannot hold the door.
    session.world.player.position = [40.5, 40.5];
    for _ in 0..180 {
        session.step(SessionInput::default());
    }
    assert!((session.world.walls[wall].a[1] - (initial[1] - 240.0 / 256.0)).abs() < 0.01);
    for cell in [[35, 30], [21, 30]] {
        let index = session
            .world
            .walls
            .iter()
            .position(|wall| wall.cell == Some(cell))
            .unwrap();
        assert!((session.wall_elevations[index] - 100.0).abs() < 0.01);
    }
}
