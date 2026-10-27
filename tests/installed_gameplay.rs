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

#[test]
fn power_station_switches_raise_gates_and_exit_completes() {
    use openchasm::mission::{MissionEvent, MissionPlan};
    use openchasm::process_runtime::ProcessCapabilities;
    let path = Path::new("assets/csm.bin");
    if !path.exists() {
        eprintln!("No local archive; real-game scenario skipped.");
        return;
    }
    let mut archive = archive::Archive::open(path).unwrap();
    let map = Map::parse(&archive.read("map/map.01").unwrap()).unwrap();
    let inf = archive.read("chasm.inf").unwrap();
    let level = archive.read("map/resource.01").unwrap();
    let process_text = String::from_utf8(archive.read("map/process.01").unwrap()).unwrap();
    let script = Script::parse(&process_text);
    let defs = ObjectDefinitions::parse(&inf, &level).unwrap();
    let instances = objects::instances(&map);
    let mut walls = Vec::new();
    for x in 0..MAP_SIDE {
        for y in 0..MAP_SIDE {
            let wall = map.wall(x, y).unwrap();
            if wall.is_wall() && wall.start != wall.end {
                let pos = |p: [u16; 2]| {
                    [
                        f32::from(p[0]) / UNITS_PER_TILE,
                        f32::from(p[1]) / UNITS_PER_TILE,
                    ]
                };
                walls.push(Wall {
                    a: pos(wall.start),
                    b: pos(wall.end),
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
            position: [37.5, 40.5],
            angle: 0.0,
        },
    };
    let processes = ProcessRuntime::with_capabilities(
        &script,
        &world,
        ProcessCapabilities {
            wall_elevation: true,
            initial_cell_types: None,
        },
    );
    let game = GameInventory::new(
        WeaponDefinitions::parse(&inf).unwrap(),
        &instances,
        &defs,
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
    session.set_mission(MissionPlan::parse(&script, &process_text));
    let switched = session.step(SessionInput {
        interact: true,
        ..SessionInput::default()
    });
    assert!(switched.process_effects.iter().any(|event| matches!(
        event,
        openchasm::process_runtime::ProcessEffect::PlayAnimation { object_id: 58 }
    )));
    session.world.player.position = [26.5, 33.5];
    session.step(SessionInput::default());
    session.world.player.position = [-100.0, -100.0];
    let router_index = session
        .world
        .walls
        .iter()
        .position(|wall| wall.cell == Some([28, 34]))
        .unwrap();
    let mut max_router = 0.0_f32;
    for _ in 0..180 {
        session.step(SessionInput::default());
        max_router = max_router.max(session.wall_elevations[router_index]);
    }
    assert!(
        (max_router - 80.0).abs() < 0.01,
        "router wall rose {max_router}"
    );
    for cell in [[44u8, 48], [43, 23]] {
        session.world.player.position = [f32::from(cell[0]) + 0.5, f32::from(cell[1]) + 0.5];
        session.step(SessionInput {
            interact: true,
            ..SessionInput::default()
        });
    }
    session.world.player.position = [-100.0, -100.0];
    let fence_indices = [[38, 41], [27, 28], [39, 24]].map(|cell| {
        session
            .world
            .walls
            .iter()
            .position(|wall| wall.cell == Some(cell))
            .unwrap()
    });
    let mut max_fences = [0.0_f32; 3];
    for _ in 0..180 {
        session.step(SessionInput::default());
        for (i, &index) in fence_indices.iter().enumerate() {
            max_fences[i] = max_fences[i].max(session.wall_elevations[index]);
        }
    }
    assert!(
        max_fences
            .iter()
            .all(|&height| (height - 96.0).abs() < 0.01),
        "fence heights: {max_fences:?}"
    );
    session.world.player.position = [21.5, 25.5];
    session.step(SessionInput::default());
    session.world.player.position = [-100.0, -100.0];
    assert!((0..150).any(|_| session
        .step(SessionInput::default())
        .mission
        .contains(&MissionEvent::LevelComplete)));
}
