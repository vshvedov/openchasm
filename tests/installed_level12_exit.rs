//! Optional installed-data process test. Player and collision clearance are
//! controlled here; this is not a full traversal or a rendering test.
#[allow(dead_code)]
#[path = "../src/archive.rs"]
mod archive;
use openchasm::{
    map::{Map, MAP_SIDE, UNITS_PER_TILE},
    object_combat::ObjectCombat,
    object_states::ObjectStates,
    objects::{self, ObjectDefinitions, ObjectKind},
    pickups::Difficulty,
    process::Script,
    process_runtime::{ProcessCapabilities, ProcessObject, ProcessOutcome, ProcessRuntime},
    process_transaction::{PresentationManifest, ProcessTransaction},
    quake::QuakeField,
    world::{Player, Wall, World},
};
use std::path::PathBuf;

#[test]
fn exit_unlocker_accepts_fifth_move_argument_and_commits_replacement() {
    let explicit = std::env::var_os("OPENCHASM_TEST_CONTENT");
    let path = explicit
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("assets/csm.bin"));
    if !path.exists() && explicit.is_none() {
        return;
    }
    let mut archive = archive::Archive::open(&path).unwrap();
    let map = Map::parse(&archive.read("map/map.12").unwrap()).unwrap();
    let script =
        Script::parse(std::str::from_utf8(&archive.read("map/process.12").unwrap()).unwrap());
    let definitions = ObjectDefinitions::parse(
        &archive.read("chasm.inf").unwrap(),
        &archive.read("map/resource.12").unwrap(),
    )
    .unwrap();
    let instances = objects::instances(&map);
    assert!(script.processes[&27]
        .actions
        .iter()
        .any(|a| a.name == "move" && a.arguments == ["29", "33", "0", "-510", "64"]));
    let mut walls = Vec::new();
    for x in 0..MAP_SIDE {
        for y in 0..MAP_SIDE {
            let wall = map.wall(x, y).unwrap();
            if wall.is_wall() && wall.start != wall.end {
                let cv = |p: [u16; 2]| p.map(|v| v as f32 / UNITS_PER_TILE);
                walls.push(Wall {
                    a: cv(wall.start),
                    b: cv(wall.end),
                    texture: wall.texture_id,
                    blocks_movement: true,
                    cell: Some([x as u8, y as u8]),
                });
            }
        }
    }
    let mut world = World {
        walls,
        player: Player {
            position: [-100.0, -100.0],
            angle: 0.0,
        },
    };
    let cells = std::array::from_fn(|i| map.wall(i / MAP_SIDE, i % MAP_SIDE).unwrap().texture_id);
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
    let mut rt = ProcessRuntime::with_staged_actions(
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
    rt.set_keys([true; 3]);
    assert_eq!(
        rt.prepare_at("floor", [30, 31], 29).err(),
        Some(ProcessOutcome::Locked)
    );
    let mut states = ObjectStates::new(&instances, &definitions).unwrap();
    let mut combat = ObjectCombat::new(&instances, &definitions, &script, Difficulty::Normal);
    let mut quake = QuakeField::new();
    let mut manifest = PresentationManifest::default();
    manifest.allow([31, 32], 165);
    let prepared = rt
        .prepare_at("floor", [35, 31], 27)
        .expect("real unlocker must be supported");
    let transaction = ProcessTransaction::prepare(
        prepared,
        &mut states,
        &mut combat,
        &mut quake,
        &definitions,
        &manifest,
        |_, _, _| true,
    )
    .expect("preflight real replacement");
    assert_eq!(transaction.transitions().len(), 1);
    assert_eq!(
        transaction.commit().unwrap().outcome,
        ProcessOutcome::Accepted
    );
    assert_eq!(states.get([31, 32]).unwrap().raw_type, 165);
    assert!(rt.prepare_at("floor", [30, 31], 29).is_ok());
    let index = world
        .walls
        .iter()
        .position(|w| w.cell == Some([29, 33]))
        .unwrap();
    let before = world.walls[index].a;
    for _ in 0..600 {
        rt.tick(&mut world, 1.0 / 60.0, false);
    }
    assert!((world.walls[index].a[1] - before[1] + 510.0 / 256.0).abs() < 0.01);
    // Exercise the actual authored source and object change separately from
    // the exit sequence, with test-provided asset and collision preflight.
    use openchasm::{
        actors::{ActorDefinitions, ActorState},
        combat::RocketDefinitions,
        game::GameInventory,
        inventory::WeaponDefinitions,
        save_format::ContentDigest,
        session::{LevelSetup, ScriptedObjects, Session, SessionInput},
    };
    let mut source_runtime = ProcessRuntime::with_source_lights(
        &script,
        &world,
        ProcessCapabilities::default(),
        &cells,
        &poses,
    )
    .unwrap();
    for (cell, id) in [([39, 37], 30), ([39, 26], 31)] {
        assert_eq!(
            source_runtime
                .prepare_at("link", cell, id)
                .unwrap()
                .commit()
                .outcome,
            ProcessOutcome::Accepted
        );
    }
    let mut source_states = ObjectStates::new(&instances, &definitions).unwrap();
    let mut source_combat =
        ObjectCombat::new(&instances, &definitions, &script, Difficulty::Normal);
    let mut source_quake = QuakeField::new();
    let prepared = source_runtime.prepare_at("link", [44, 31], 32).unwrap();
    assert!(ProcessTransaction::prepare(
        prepared,
        &mut source_states,
        &mut source_combat,
        &mut source_quake,
        &definitions,
        &PresentationManifest::default(),
        |_, _, _| true
    )
    .is_err());
    assert!(!source_runtime.has_active_source_lights());
    assert!(source_runtime.take_effects().is_empty());
    let mut source_manifest = PresentationManifest::default();
    source_manifest.allow([40, 31], 216);
    let prepared = source_runtime.prepare_at("link", [44, 31], 32).unwrap();
    let transaction = ProcessTransaction::prepare(
        prepared,
        &mut source_states,
        &mut source_combat,
        &mut source_quake,
        &definitions,
        &source_manifest,
        |_, _, _| true,
    )
    .unwrap();
    assert_eq!(
        transaction.commit().unwrap().outcome,
        ProcessOutcome::Accepted
    );
    assert_eq!(source_states.get([40, 31]).unwrap().raw_type, 216);
    let inf = archive.read("chasm.inf").unwrap();
    let game = GameInventory::new(
        WeaponDefinitions::parse(&inf).unwrap(),
        &instances,
        &definitions,
        Difficulty::Normal,
    );
    let mut session = Session::new(LevelSetup {
        world,
        processes: source_runtime,
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
    let events = session.step(SessionInput::default());
    assert!(events.light_errors.is_empty());
    let light = session
        .light_sources
        .as_ref()
        .unwrap()
        .active()
        .find(|p| p.process_id == 32)
        .unwrap();
    assert_eq!(light.sources[0].position_raw, [10240, 8192]);
    assert_eq!(light.sources[0].radius_raw, 576);
    assert_eq!(light.sources[0].delay_ms, 10);
    let snapshot = session.snapshot(ContentDigest([0; 32])).unwrap();
    assert_eq!(
        snapshot.light_sources.as_ref().unwrap().active[0].process_id,
        32
    );

    // Construct a genuinely fresh baseline and exercise the same installed
    // source procedure through Session so its committed lifecycle is journalled.
    let baseline_world = session.world.clone();
    let runtime = ProcessRuntime::with_source_lights(
        &script,
        &baseline_world,
        ProcessCapabilities::default(),
        &cells,
        &poses,
    )
    .unwrap();
    let mut baseline = Session::new(LevelSetup {
        world: baseline_world,
        processes: runtime,
        actors: ActorState {
            player_spawns: vec![],
            actors: vec![],
            unresolved: vec![],
        },
        actor_definitions: ActorDefinitions::parse(&inf).unwrap(),
        game: GameInventory::new(
            WeaponDefinitions::parse(&inf).unwrap(),
            &instances,
            &definitions,
            Difficulty::Normal,
        ),
        rockets: RocketDefinitions::parse(&inf).unwrap(),
        shot_blocking: [true; 128],
        difficulty: Difficulty::Normal,
    });
    baseline.set_objects(ObjectCombat::new(
        &instances,
        &definitions,
        &script,
        Difficulty::Normal,
    ));
    let mut manifest = PresentationManifest::default();
    manifest.allow([40, 31], 216);
    baseline.set_scripted_objects(ScriptedObjects::new(
        ObjectStates::new(&instances, &definitions).unwrap(),
        definitions.clone(),
        manifest,
        &script,
        false,
    ));
    let mut source = baseline.clone();
    for (cell, id) in [([39, 37], 30), ([39, 26], 31)] {
        assert_eq!(
            source
                .processes
                .prepare_at("link", cell, id)
                .unwrap()
                .commit()
                .outcome,
            ProcessOutcome::Accepted
        );
    }
    source.step(SessionInput::default());
    let position = poses
        .iter()
        .find(|pose| pose.cell == [44, 31])
        .map_or([44.5, 31.5], |pose| pose.position);
    source.world.player.position = position;
    source.step(SessionInput {
        interact: true,
        ..Default::default()
    });
    assert!(source
        .light_sources
        .as_ref()
        .unwrap()
        .active()
        .any(|light| light.process_id == 32));
    for _ in 0..3 {
        source.step(SessionInput::default());
    }
    let digest = ContentDigest([1; 32]);
    let snapshot = source.snapshot(digest).unwrap();
    let mut restored = baseline.restore_from_baseline(&snapshot, digest).unwrap();
    assert_eq!(restored.snapshot(digest).unwrap(), snapshot);
    source.step(SessionInput::default());
    restored.step(SessionInput::default());
    assert_eq!(
        source.light_sources.as_ref().unwrap().snapshot(),
        restored.light_sources.as_ref().unwrap().snapshot()
    );
}
