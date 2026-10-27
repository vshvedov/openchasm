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
    mission::MissionPlan,
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
fn level16_teleport_lands_on_authored_platform() {
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
    assert_eq!(definitions.definition(heart).unwrap().break_limit, 300);
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
            position: [36.5, 38.5],
            angle: std::f32::consts::FRAC_PI_2,
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
    let rockets = RocketDefinitions::parse(&inf).unwrap();
    assert_eq!(rockets.0[game.weapons.0[0].rocket_type as usize].power, 25);
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
    let mut heights = openchasm::object_heights::ObjectHeightCatalog::new();
    for (index, definition) in session
        .scripted_objects
        .as_ref()
        .unwrap()
        .definitions
        .level_objects
        .iter()
        .enumerate()
    {
        if let Ok(bytes) = archive.read(&format!("model/{}", definition.model_name)) {
            if let Ok(model) = openchasm::model::Model::parse(&bytes) {
                if let Some(bounds) = openchasm::object_heights::HeightBounds::from_3o(&model) {
                    heights.set(163 + index as u8, bounds);
                }
            }
        }
    }
    assert!(session.set_object_heights(heights, cells));
    session.set_ambient_grid(openchasm::ambient::AmbientGrid::from_map(&map));
    session.set_mission(MissionPlan::parse(
        &script,
        &String::from_utf8_lossy(&archive.read("map/process.16").unwrap()),
    ));
    let replacements = [[27, 39], [28, 39], [27, 37], [28, 37], [27, 38]];
    for cell in replacements {
        let scripted = session.scripted_objects.as_mut().unwrap();
        let raw = scripted.states.get(cell).unwrap().raw_type;
        scripted.manifest.allow(cell, raw + 1);
    }
    let process_bytes = archive.read("map/process.16").unwrap();
    let teleports =
        openchasm::teleport::parse(std::str::from_utf8(&process_bytes).unwrap()).unwrap();
    session.set_teleports(
        teleports,
        openchasm::teleport::RearmPolicy::AfterLeavingSources,
    );
    let baseline = session.clone();
    let events = session.step(SessionInput::default());
    assert!(events.teleport.is_some(), "{:?}", events.teleport_failure);
    assert_eq!(session.world.player.position, [28.0, 38.5]);
    assert!((session.vertical.feet() - 0.875).abs() < 1e-6);
    session.step(SessionInput::default());
    assert!((session.vertical.feet() - 0.875).abs() < 1e-6);
    let digest = openchasm::save_format::ContentDigest([16; 32]);
    let snapshot = session.snapshot(digest).unwrap();
    let restored = baseline.restore_from_baseline(&snapshot, digest).unwrap();
    assert_eq!(restored.snapshot(digest).unwrap(), snapshot);
    let mut second = None;
    for _ in 0..90 {
        let events = session.step(SessionInput {
            movement: Input {
                forward: 1.0,
                ..Default::default()
            },
            ..Default::default()
        });
        if events.teleport.is_some() {
            second = events.teleport;
            break;
        }
        if events.teleport_failure.is_some() {
            panic!("second teleport failed: {:?}", events.teleport_failure);
        }
    }
    assert!(
        second.is_some(),
        "platform exit was not triggered: {:?}",
        session.world.player.position
    );
    assert_eq!(session.world.player.position, [21.0, 48.0]);
}
