//! Optional installed-data scenario for a level 8 teleport.
//! The fixture starts at the source and does not establish level traversal.
#[allow(dead_code)]
#[path = "../src/archive.rs"]
mod archive;
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
    assert_eq!(session.world.player.angle, 0.0);
}
