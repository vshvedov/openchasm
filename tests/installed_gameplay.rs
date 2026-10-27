//! Optional integration test of the installed game's first destructible lock.
#[allow(dead_code)]
#[path = "../src/archive.rs"]
mod archive;
use openchasm::{
    actors::{ActorDefinitions, ActorState},
    combat::{FireResult, RocketDefinitions, ShotImpact},
    game::GameInventory,
    inventory::WeaponDefinitions,
    map::{Map, MAP_SIDE, UNITS_PER_TILE},
    object_combat::ObjectCombat,
    objects::{self, ObjectDefinitions},
    pickups::Difficulty,
    process::Script,
    process_runtime::ProcessRuntime,
    session::{LevelSetup, Session, SessionInput},
    world::{Player, Wall, World},
};
use std::path::Path;

#[test]
fn power_station_lock_dispatches_destroy_script() {
    let path = Path::new("assets/csm.bin");
    if !path.exists() {
        eprintln!("No local archive; real-game scenario skipped.");
        return;
    }
    let mut archive = archive::Archive::open(path).unwrap();
    let map = Map::parse(&archive.read("map/map.01").unwrap()).unwrap();
    let inf = archive.read("chasm.inf").unwrap();
    let level = archive.read("map/resource.01").unwrap();
    let script = Script::parse(&String::from_utf8_lossy(
        &archive.read("map/process.01").unwrap(),
    ));
    let definitions = ObjectDefinitions::parse(&inf, &level).unwrap();
    let instances = objects::instances(&map);
    let lock = instances
        .iter()
        .find(|item| item.grid_cell == [22, 55])
        .expect("Power Station lock");
    let mut walls = Vec::new();
    for x in 0..MAP_SIDE {
        for y in 0..MAP_SIDE {
            let wall = map.wall(x, y).unwrap();
            if wall.is_wall() && wall.start != wall.end {
                let convert = |p: [u16; 2]| {
                    [
                        f32::from(p[0]) / UNITS_PER_TILE,
                        f32::from(p[1]) / UNITS_PER_TILE,
                    ]
                };
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
            position: [lock.position[0] - 0.4, lock.position[1]],
            angle: 0.0,
        },
    };
    let processes = ProcessRuntime::new(&script, &world);
    let game = GameInventory::new(
        WeaponDefinitions::parse(&inf).unwrap(),
        &instances,
        &definitions,
        Difficulty::Normal,
    );
    let actor_definitions = ActorDefinitions::parse(&inf).unwrap();
    let actors = ActorState {
        player_spawns: vec![],
        actors: vec![],
        unresolved: vec![],
    };
    let mut session = Session::new(LevelSetup {
        world,
        processes,
        actors,
        actor_definitions,
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
    let initial_gate = session
        .world
        .walls
        .iter()
        .find(|wall| wall.cell == Some([24, 52]))
        .unwrap()
        .a;
    let events = session.step(SessionInput {
        fire: true,
        ..SessionInput::default()
    });
    assert!(
        matches!(
            events.shot,
            Some(FireResult::Fired(ShotImpact::Object {
                cell: [22, 55],
                destroyed: true,
                ..
            }))
        ),
        "actual hit: {:?}",
        events.shot
    );
    assert_eq!(events.object_hits, [([22, 55], true)]);
    for _ in 0..60 {
        session.step(SessionInput::default());
    }
    let opened_gate = session
        .world
        .walls
        .iter()
        .find(|wall| wall.cell == Some([24, 52]))
        .unwrap()
        .a;
    assert_ne!(
        initial_gate, opened_gate,
        "destroyed lock should start the real gate process"
    );
}
