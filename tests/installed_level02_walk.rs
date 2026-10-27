//! Walk from the real Military Base spawn to its offset gate switch.
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
    objects::{self, ObjectDefinitions, ObjectKind},
    pickups::Difficulty,
    process::Script,
    process_runtime::{ProcessCapabilities, ProcessObject, ProcessOutcome, ProcessRuntime},
    session::{LevelSetup, Session, SessionInput},
    world::{Input, Player, Wall, WallCollisionMode, World},
};
use std::path::PathBuf;

#[test]
fn spawn_can_walk_to_offset_switch_and_unlock_gate() {
    run_walk(WallCollisionMode::TwoSided);
}

#[test]
fn spawn_can_walk_to_offset_switch_and_unlock_gate_with_authored_sides() {
    run_walk(WallCollisionMode::AuthoredSides);
}

fn run_walk(mode: WallCollisionMode) {
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
    let map = Map::parse(&archive.read("map/map.02").unwrap()).unwrap();
    let inf = archive.read("chasm.inf").unwrap();
    let definitions =
        ObjectDefinitions::parse(&inf, &archive.read("map/resource.02").unwrap()).unwrap();
    let script = Script::parse(&String::from_utf8_lossy(
        &archive.read("map/process.02").unwrap(),
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
    assert_eq!(world.player.position, [14.5, 13.5]);
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
    assert!(
        session
            .processes
            .static_wall_mask()
            .iter()
            .any(|is_static| *is_static),
        "installed map should exercise the static mask"
    );
    session.set_wall_collision_mode(mode);
    let initially = session.processes.activate_at("floor", [20, 12]);
    assert!(initially
        .iter()
        .any(|e| e.process_id == 8 && e.outcome == ProcessOutcome::Locked));
    // Quarter-tile checkpoints follow a route around the wall near the spawn.
    let mut checkpoints = vec![[14.875, 13.625]];
    for x in 0..=6 {
        checkpoints.push([15.125 + 0.25 * x as f32, 13.625]);
    }
    for y in 1..=13 {
        checkpoints.push([16.375, 13.625 - 0.25 * y as f32]);
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
    session.step(SessionInput {
        interact: true,
        ..SessionInput::default()
    });
    let after = session.processes.activate_at("floor", [20, 12]);
    assert!(
        after
            .iter()
            .any(|e| e.process_id == 8 && e.outcome == ProcessOutcome::Accepted),
        "{after:?}"
    );
}
