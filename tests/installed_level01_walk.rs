//! Walk from the real Power Station spawn to a vantage for its first lock.
//! Optional owned data, no enemies, and conservative solid wall approximations;
//! this checks one interaction route, not full level completion.
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
    world::{Input, Player, Wall, World},
};
use std::path::PathBuf;

#[test]
fn spawn_can_reach_and_destroy_first_lock() {
    let explicit = std::env::var_os("OPENCHASM_TEST_CONTENT");
    let path = explicit
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(|| "assets/csm.bin".into());
    if !path.exists() && explicit.is_none() {
        eprintln!("No owned archive; walk test skipped.");
        return;
    }
    let mut archive = archive::Archive::open(&path).unwrap();
    let map = Map::parse(&archive.read("map/map.01").unwrap()).unwrap();
    let inf = archive.read("chasm.inf").unwrap();
    let definitions =
        ObjectDefinitions::parse(&inf, &archive.read("map/resource.01").unwrap()).unwrap();
    let script = Script::parse(&String::from_utf8_lossy(
        &archive.read("map/process.01").unwrap(),
    ));
    let instances = objects::instances(&map);
    let spawn = map
        .monsters
        .iter()
        .find(|m| m.is_player_spawn() && m.flags == 0)
        .unwrap();
    let point = |p: [u16; 2]| [p[0] as f32 / UNITS_PER_TILE, p[1] as f32 / UNITS_PER_TILE];
    let mut walls = Vec::new();
    for x in 0..MAP_SIDE {
        for y in 0..MAP_SIDE {
            let w = map.wall(x, y).unwrap();
            if w.is_wall() && w.start != w.end {
                walls.push(Wall {
                    a: point(w.start),
                    b: point(w.end),
                    texture: w.texture_id,
                    blocks_movement: true,
                    cell: Some([x as u8, y as u8]),
                });
            }
        }
    }
    let world = World {
        walls,
        player: Player {
            position: point(spawn.position),
            angle: spawn.direction_radians(),
        },
    };
    assert_eq!(world.player.position, [31.5, 58.5]);
    let cells = std::array::from_fn(|i| map.wall(i / MAP_SIDE, i % MAP_SIDE).unwrap().texture_id);
    let poses: Vec<_> = instances
        .iter()
        .filter(|o| o.difficulty_flags & 2 != 0)
        .filter_map(|o| {
            let d = definitions.definition(o)?;
            Some(ProcessObject {
                cell: [o.grid_cell[0] as u8, o.grid_cell[1] as u8],
                position: o.position,
                angle_radians: o.angle_radians,
                elevation_raw: 0.0,
                blocking_radius: if o.kind == ObjectKind::Item
                    || (141..=143).contains(&d.action_code)
                {
                    0.0
                } else {
                    (d.collision_radius.max(0) as f32 / 256.0).min(2.0)
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
            actors: vec![],
            player_spawns: vec![],
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
    session.set_scripted_objects(ScriptedObjects::new(
        ObjectStates::new(&instances, &definitions).unwrap(),
        definitions,
        PresentationManifest::default(),
        &script,
        true,
    ));
    let gate = session
        .world
        .walls
        .iter()
        .position(|w| w.cell == Some([24, 52]))
        .unwrap();
    let gate_start = session.world.walls[gate].a;
    let mut checkpoints = Vec::new();
    for x in 0..=4 {
        checkpoints.push([31.375 - 0.25 * x as f32, 58.625]);
    }
    for y in 1..=12 {
        checkpoints.push([30.375, 58.625 - 0.25 * y as f32]);
    }
    for x in 1..=8 {
        checkpoints.push([30.375 - 0.25 * x as f32, 55.625]);
    }
    for y in 1..=8 {
        checkpoints.push([28.375, 55.625 - 0.25 * y as f32]);
    }
    for x in 1..=10 {
        checkpoints.push([28.375 - 0.25 * x as f32, 53.625]);
    }
    for y in 1..=4 {
        checkpoints.push([25.875, 53.625 - 0.25 * y as f32]);
    }
    for target in checkpoints {
        for _ in 0..12 {
            let p = session.world.player.position;
            let d = [target[0] - p[0], target[1] - p[1]];
            let distance = d[0].hypot(d[1]);
            if distance < 0.035 {
                break;
            }
            let desired = d[1].atan2(d[0]);
            let yaw = (desired - session.world.player.angle + std::f32::consts::PI)
                .rem_euclid(std::f32::consts::TAU)
                - std::f32::consts::PI;
            session.step(SessionInput {
                yaw_delta: yaw,
                movement: Input {
                    forward: (distance / (2.5 / 60.0)).min(1.0),
                    ..Input::default()
                },
                ..SessionInput::default()
            });
        }
        let p = session.world.player.position;
        assert!(
            (p[0] - target[0]).hypot(p[1] - target[1]) < 0.12,
            "blocked at {p:?} before checkpoint {target:?}"
        );
    }
    let p = session.world.player.position;
    let target = instances
        .iter()
        .find(|o| o.grid_cell == [22, 55])
        .unwrap()
        .position;
    let desired = (target[1] - p[1]).atan2(target[0] - p[0]);
    let yaw = (desired - session.world.player.angle + std::f32::consts::PI)
        .rem_euclid(std::f32::consts::TAU)
        - std::f32::consts::PI;
    session.step(SessionInput {
        yaw_delta: yaw,
        ..SessionInput::default()
    });
    let mut destroyed = false;
    for _ in 0..300 {
        let events = session.step(SessionInput {
            fire: true,
            ..SessionInput::default()
        });
        if events
            .object_hits
            .iter()
            .any(|hit| *hit == ([22, 55], true))
        {
            destroyed = true;
            break;
        }
    }
    assert!(
        destroyed,
        "lock should be shootable from reachable position"
    );
    for _ in 0..90 {
        session.step(SessionInput::default());
    }
    assert_ne!(
        session.world.walls[gate].a, gate_start,
        "destroying lock opens gate"
    );
}
