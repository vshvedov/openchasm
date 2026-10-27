//! Optional installed-data scenario for a level 8 teleport.
//! The fixture starts at the source and does not establish level traversal.
#[allow(dead_code)]
#[path = "../src/archive.rs"]
mod archive;
#[cfg(feature = "client")]
#[allow(dead_code)]
#[path = "../src/content.rs"]
mod content;
#[cfg(feature = "client")]
#[allow(dead_code)]
#[path = "../src/gltf_model.rs"]
mod gltf_model;
#[cfg(feature = "client")]
#[allow(dead_code)]
#[path = "../src/object_height_loader.rs"]
mod object_height_loader;
use openchasm::{
    actors::{ActorDefinitions, ActorState},
    combat::RocketDefinitions,
    game::GameInventory,
    inventory::WeaponDefinitions,
    map::{Map, MAP_SIDE, UNITS_PER_TILE},
    objects::{self, ObjectDefinitions, ObjectKind},
    pickups::Difficulty,
    process::Script,
    process_runtime::{ProcessCapabilities, ProcessObject, ProcessRuntime},
    session::{LevelSetup, Session, SessionInput},
    world::{Player, Wall, World},
};
use std::path::PathBuf;

#[test]
fn level08_authored_teleport_moves_player() {
    let explicit = std::env::var_os("OPENCHASM_TEST_CONTENT");
    let path = explicit
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(|| "assets/csm.bin".into());
    if !path.exists() && explicit.is_none() {
        eprintln!(
            "No installed archive; set OPENCHASM_TEST_CONTENT to run the level 8 teleport scenario."
        );
        return;
    }
    let mut archive = archive::Archive::open(&path).unwrap();
    let map = Map::parse(&archive.read("map/map.08").unwrap()).unwrap();
    let inf = archive.read("chasm.inf").unwrap();
    let definitions =
        ObjectDefinitions::parse(&inf, &archive.read("map/resource.08").unwrap()).unwrap();
    let script_data = archive.read("map/process.08").unwrap();
    let script = Script::parse(std::str::from_utf8(&script_data).unwrap());
    let instances = objects::instances(&map);
    let mut walls = Vec::new();
    let mut cells = [0u8; MAP_SIDE * MAP_SIDE];
    for x in 0..MAP_SIDE {
        for y in 0..MAP_SIDE {
            let wall = map.wall(x, y).unwrap();
            cells[x * MAP_SIDE + y] = wall.texture_id;
            if wall.is_wall() && wall.start != wall.end {
                let convert =
                    |p: [u16; 2]| [p[0] as f32 / UNITS_PER_TILE, p[1] as f32 / UNITS_PER_TILE];
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
    let world = World {
        walls,
        player: Player {
            position: [16.5, 44.5],
            angle: 0.0,
        },
    };
    let poses: Vec<_> = instances
        .iter()
        .filter(|o| o.difficulty_flags & 2 != 0)
        .filter_map(|o| {
            let definition = definitions.definition(o)?;
            Some(ProcessObject {
                cell: [o.grid_cell[0] as u8, o.grid_cell[1] as u8],
                position: o.position,
                angle_radians: o.angle_radians,
                elevation_raw: 0.0,
                blocking_radius: if o.kind == ObjectKind::LevelObject
                    && !(141..=143).contains(&definition.action_code)
                {
                    (definition.collision_radius.max(0) as f32 / 256.0).min(2.0)
                } else {
                    0.0
                },
            })
        })
        .collect();
    let processes = ProcessRuntime::with_wind(
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
    let rows = openchasm::teleport::parse(std::str::from_utf8(&script_data).unwrap()).unwrap();
    session.set_teleports(rows, openchasm::teleport::RearmPolicy::ReferenceContinuous);
    let event = session.step(SessionInput::default());
    assert_eq!(event.teleport_failure, None);
    assert_eq!(event.teleport.unwrap().index, 0);
    assert_eq!(session.world.player.position, [5.5, 32.5]);
    assert!((session.world.player.angle - std::f32::consts::PI).abs() < 1e-5);
}

#[test]
fn level17_extra_teleport_field_is_ignored() {
    let explicit = std::env::var_os("OPENCHASM_TEST_CONTENT");
    let path = explicit
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(|| "assets/csm.bin".into());
    if !path.exists() && explicit.is_none() {
        eprintln!(
            "No installed archive; set OPENCHASM_TEST_CONTENT to run the level 8 teleport scenario."
        );
        return;
    }
    let mut archive = archive::Archive::open(&path).unwrap();
    let map = Map::parse(&archive.read("map/map.17").unwrap()).unwrap();
    let inf = archive.read("chasm.inf").unwrap();
    let definitions =
        ObjectDefinitions::parse(&inf, &archive.read("map/resource.17").unwrap()).unwrap();
    let script_data = archive.read("map/process.17").unwrap();
    let script = Script::parse(std::str::from_utf8(&script_data).unwrap());
    let instances = objects::instances(&map);
    let mut walls = Vec::new();
    let mut cells = [0u8; MAP_SIDE * MAP_SIDE];
    for x in 0..MAP_SIDE {
        for y in 0..MAP_SIDE {
            let wall = map.wall(x, y).unwrap();
            cells[x * MAP_SIDE + y] = wall.texture_id;
            if wall.is_wall() && wall.start != wall.end {
                let convert =
                    |p: [u16; 2]| [p[0] as f32 / UNITS_PER_TILE, p[1] as f32 / UNITS_PER_TILE];
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
    let world = World {
        walls,
        player: Player {
            position: [41.5, 53.5],
            angle: 0.0,
        },
    };
    let poses: Vec<_> = instances
        .iter()
        .filter(|o| o.difficulty_flags & 2 != 0)
        .filter_map(|o| {
            let definition = definitions.definition(o)?;
            Some(ProcessObject {
                cell: [o.grid_cell[0] as u8, o.grid_cell[1] as u8],
                position: o.position,
                angle_radians: o.angle_radians,
                elevation_raw: 0.0,
                blocking_radius: if o.kind == ObjectKind::LevelObject
                    && !(141..=143).contains(&definition.action_code)
                {
                    (definition.collision_radius.max(0) as f32 / 256.0).min(2.0)
                } else {
                    0.0
                },
            })
        })
        .collect();
    let processes = ProcessRuntime::with_wind(
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
    let rows = openchasm::teleport::parse(std::str::from_utf8(&script_data).unwrap()).unwrap();
    session.set_teleports(rows, openchasm::teleport::RearmPolicy::ReferenceContinuous);
    #[cfg(feature = "client")]
    {
        let mut resource_set = content::ResourceSet::open(&path).unwrap();
        let heights = object_height_loader::load_object_heights(&mut resource_set, &definitions);
        let mut with_heights = session.clone();
        assert!(with_heights.set_object_heights(heights, cells));
        with_heights.world.player.position = [41.5, 53.5];
        let event = with_heights.step(SessionInput::default());
        assert_eq!(event.teleport_failure, None);
        assert_eq!(event.teleport.unwrap().index, 0);
        assert_eq!(with_heights.world.player.position, [37.5, 16.5]);
    }
    let mut roof_blocked = session.clone();
    roof_blocked.world.player.position = [41.5, 53.5];
    let blocked = roof_blocked.step(SessionInput::default());
    assert!(matches!(
        blocked.teleport_failure,
        Some(openchasm::teleport::TeleportFailure::DestinationBlocked { index: 0 })
    ));
    session.world.player.position = [38.5, 55.5];
    let event = session.step(SessionInput::default());
    assert_eq!(event.teleport_failure, None);
    assert_eq!(event.teleport.unwrap().index, 1);
    assert_eq!(session.world.player.position, [43.5, 16.5]);
    assert!((session.world.player.angle - std::f32::consts::FRAC_PI_2).abs() < 1e-5);
}
