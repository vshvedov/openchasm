//! Optional installed-data scenario: teleport to a key and the exit approach.
//! This validates linked game state, not traversability of the entire level.
#[allow(dead_code)]
#[path = "../src/archive.rs"]
mod archive;
use openchasm::{
    actors::{ActorDefinitions, ActorState},
    combat::RocketDefinitions,
    game::GameInventory,
    inventory::{Key, Pickup, WeaponDefinitions},
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
fn green_card_opens_linked_doors_and_enables_exit_approach() {
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
        .find(|item| item.grid_cell == [11, 26])
        .unwrap();
    assert_eq!(
        definitions.level_objects[key.definition_index].action_code,
        142
    );
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
    let gate_index = session
        .world
        .walls
        .iter()
        .position(|wall| wall.cell == Some([8, 25]))
        .unwrap();
    let initial_gate = session.world.walls[gate_index].a;
    let pickup = session.step(SessionInput::default());
    assert!(pickup
        .pickups
        .iter()
        .any(|p| p.cell == [11, 26] && p.pickup == Pickup::Key(Key::Green)));
    assert!(session.game.inventory.keys[1]);
    // Teleport to the next real trigger; combat and full route traversal are out of scope.
    session.world.player.position = [26.5, 13.5];
    for _ in 0..300 {
        session.step(SessionInput::default());
    }
    assert_ne!(
        session.world.walls[gate_index].a, initial_gate,
        "key pickup must start process 15"
    );
    let exit_index = session
        .world
        .walls
        .iter()
        .position(|wall| wall.cell == Some([26, 14]))
        .unwrap();
    assert!(
        (session.wall_elevations[exit_index] - 90.0).abs() < 0.01,
        "green key should open exit approach"
    );
}
