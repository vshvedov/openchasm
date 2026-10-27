//! Optional installed-data scenario for the Military Base invisibility item.
//! The test starts at the item and does not validate level traversal.
#[allow(dead_code)]
#[path = "../src/archive.rs"]
mod archive;
use openchasm::{
    actors::{ActorDefinitions, ActorState},
    combat::RocketDefinitions,
    game::GameInventory,
    inventory::{Pickup, WeaponDefinitions},
    map::{Map, MAP_SIDE, UNITS_PER_TILE},
    objects::{self, ObjectDefinitions},
    pickups::Difficulty,
    process::Script,
    process_runtime::{ProcessCapabilities, ProcessRuntime},
    session::{LevelSetup, Session, SessionInput},
    world::{Player, Wall, World},
};
use std::path::PathBuf;

#[test]
fn military_base_invisibility_item_is_collected_and_expires() {
    let explicit = std::env::var_os("OPENCHASM_TEST_CONTENT");
    let path = explicit
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(|| "assets/csm.bin".into());
    if !path.exists() && explicit.is_none() {
        eprintln!(
            "No installed archive; set OPENCHASM_TEST_CONTENT to run the Military Base scenario."
        );
        return;
    }
    let mut archive = archive::Archive::open(&path).unwrap();
    let map = Map::parse(&archive.read("map/map.02").unwrap()).unwrap();
    let inf = archive.read("chasm.inf").unwrap();
    let definitions =
        ObjectDefinitions::parse(&inf, &archive.read("map/resource.02").unwrap()).unwrap();
    let script_data = archive.read("map/process.02").unwrap();
    let script = Script::parse(std::str::from_utf8(&script_data).unwrap());
    let instances = objects::instances(&map);
    let key = instances
        .iter()
        .find(|item| item.grid_cell == [19, 38])
        .unwrap();
    assert_eq!(definitions.items[key.definition_index].action_code, 131);
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
            position: key.position,
            angle: 0.0,
        },
    };
    let processes = ProcessRuntime::with_cell_data(
        &script,
        &world,
        ProcessCapabilities {
            wall_elevation: true,
            initial_cell_types: None,
        },
        &cells,
    );
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
    let first = session.step(SessionInput::default());
    assert!(first
        .pickups
        .iter()
        .any(|p| p.cell == [19, 38] && p.pickup == Pickup::Invisibility));
    assert_eq!(session.game.inventory.invisibility_ticks_remaining, 2400);
    for _ in 0..2400 {
        session.step(SessionInput::default());
    }
    assert_eq!(session.game.inventory.invisibility_ticks_remaining, 0);
}
