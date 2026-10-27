//! Public, headless movement scenarios through the authoritative session.
use openchasm::{
    actors::{ActorDefinitions, ActorState},
    combat::RocketDefinitions,
    game::GameInventory,
    inventory::{WeaponDefinition, WeaponDefinitions},
    movement::MovementMode,
    objects::ObjectDefinitions,
    pickups::Difficulty,
    process::Script,
    process_runtime::ProcessRuntime,
    save_format::ContentDigest,
    session::{LevelSetup, Session, SessionInput, SessionSnapshot},
    world::{Input, Player, Wall, World},
};

fn session(walls: Vec<Wall>) -> Session {
    let world = World {
        walls,
        player: Player {
            position: [10.0, 10.0],
            angle: 0.0,
        },
    };
    let processes = ProcessRuntime::new(&Script::parse(""), &world);
    let weapon = WeaponDefinition {
        rocket_type: 0,
        reload_time: 1,
        vertical_shift: 0,
        projectile_height: 0,
        ammo_per_portion: 1,
        ammo_limit: 10,
        ammo_on_weapon_pickup: 1,
        projectile_count: 1,
    };
    Session::new(LevelSetup {
        world,
        processes,
        actors: ActorState {
            player_spawns: vec![],
            actors: vec![],
            unresolved: vec![],
        },
        actor_definitions: ActorDefinitions { entries: vec![] },
        game: GameInventory::new(
            WeaponDefinitions([weapon; 8]),
            &[],
            &ObjectDefinitions {
                items: vec![],
                level_objects: vec![],
            },
            Difficulty::Normal,
        ),
        rockets: RocketDefinitions(vec![]),
        shot_blocking: [true; 128],
        difficulty: Difficulty::Normal,
    })
}

fn moving(forward: f32, strafe: f32, running: bool, jump_held: bool) -> SessionInput {
    SessionInput {
        movement: Input {
            forward,
            strafe,
            ..Default::default()
        },
        running,
        jump_held,
        ..Default::default()
    }
}

fn speed(session: &Session) -> f32 {
    let [x, y] = session.movement_velocity();
    x.hypot(y)
}

fn platform_session() -> Session {
    use openchasm::{
        object_combat::ObjectCombat,
        object_heights::{HeightBounds, ObjectHeightCatalog},
        object_states::ObjectStates,
        objects::{ObjectDefinition, ObjectInstance, ObjectKind},
        process_runtime::{ProcessCapabilities, ProcessObject},
        process_transaction::PresentationManifest,
        session::ScriptedObjects,
    };
    let mut session = session(vec![]);
    session.world.player.position = [0.45, 1.0];
    let definitions = ObjectDefinitions {
        items: vec![],
        level_objects: vec![ObjectDefinition {
            collision_radius: 77,
            shadow: 0,
            bmp_object: 0,
            bmp_z: 0,
            action_code: 0,
            blow_up: 0,
            break_limit: 0,
            ambient_sound: 0,
            break_sound: 0,
            model_name: "platform.3o".into(),
            animation_name: None,
        }],
    };
    let instances = [ObjectInstance {
        kind: ObjectKind::LevelObject,
        definition_index: 0,
        grid_cell: [1, 1],
        position: [1.0, 1.0],
        angle_radians: 0.0,
        flags: 0,
        difficulty_flags: 7,
        extra: 0,
        size: 0,
    }];
    let poses = [ProcessObject {
        cell: [1, 1],
        position: [1.0, 1.0],
        angle_radians: 0.0,
        elevation_raw: 0.0,
        blocking_radius: 77.0 / 256.0,
    }];
    let mut cells = [0u8; 4096];
    cells[65] = 163;
    let script = Script::default();
    session.processes = ProcessRuntime::with_staged_actions(
        &script,
        &session.world,
        ProcessCapabilities::default(),
        &cells,
        &poses,
    )
    .unwrap();
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
        false,
    ));
    let mut heights = ObjectHeightCatalog::new();
    heights.set(163, HeightBounds::new(0.0, 0.7).unwrap());
    assert!(session.set_object_heights(heights, cells));
    session
}

#[test]
fn quake_platform_exit_updates_grounding_before_save_and_next_air_command() {
    use openchasm::vertical::VerticalState;
    let baseline = platform_session();
    let mut source = baseline.clone();
    source.set_movement_mode(MovementMode::Quake1).unwrap();
    source.world.player.position = [1.4, 1.0];
    source.vertical = VerticalState::with_config(0.7, source.vertical.config()).unwrap();
    let platform_edge = 1.0 + 77.0 / 256.0 + openchasm::world::PLAYER_RADIUS + 0.0001;
    let mut exited = false;
    for _ in 0..20 {
        source.step(moving(1.0, 0.0, true, false));
        if source.world.player.position[0] > platform_edge {
            exited = true;
            break;
        }
    }
    assert!(exited);
    assert!(
        !source.vertical.grounded(),
        "destination has no platform support"
    );
    let digest = ContentDigest([71; 32]);
    let saved = source.snapshot(digest).unwrap();
    let mut restored = baseline.restore_from_baseline(&saved, digest).unwrap();
    assert_eq!(restored.snapshot(digest).unwrap(), saved);
    let velocity = source.movement_velocity();
    for state in [&mut source, &mut restored] {
        let events = state.step(moving(0.0, 0.0, false, true));
        assert!(
            !events.vertical.jumped,
            "an airborne jump must not be accepted"
        );
        assert!(!state.vertical.grounded());
        assert_eq!(
            state.movement_velocity(),
            velocity,
            "air momentum must not receive ground friction"
        );
        assert!(
            state.snapshot(digest).unwrap().movement.jump_released,
            "an airborne press must not consume Quake's release latch"
        );
    }
    assert_eq!(
        source.snapshot(digest).unwrap(),
        restored.snapshot(digest).unwrap()
    );
}

#[test]
fn restoring_changed_mode_uses_saved_vertical_state_not_baseline_support() {
    use openchasm::vertical::VerticalState;
    let mut baseline = platform_session();
    baseline.world.player.position = [1.4, 1.0];
    baseline.vertical = VerticalState::with_config(0.7, baseline.vertical.config()).unwrap();
    assert_eq!(baseline.movement_mode(), MovementMode::Original);
    let mut source = baseline.clone();
    source.set_movement_mode(MovementMode::Quake1).unwrap();
    for _ in 0..60 {
        source.step(moving(1.0, 0.0, true, false));
    }
    assert!(source.world.player.position[0] > 2.0);
    assert!(source.vertical.grounded());
    assert_eq!(source.vertical.feet(), 0.0);
    let digest = ContentDigest([72; 32]);
    let saved = source.snapshot(digest).unwrap();
    // The baseline is grounded at .7; that old state is invalid at the saved
    // ground-floor position. Only the saved feet/mode belong in restoration.
    let restored = baseline.restore_from_baseline(&saved, digest).unwrap();
    assert_eq!(restored.snapshot(digest).unwrap(), saved);
    assert_eq!(baseline.vertical.feet(), 0.7);
    assert!(baseline.vertical.grounded());
    assert_eq!(baseline.movement_mode(), MovementMode::Original);
}

#[test]
fn original_run_walk_acceleration_braking_and_diagonal_distance() {
    let mut run = session(vec![]);
    let mut walk = session(vec![]);
    let mut diagonal = session(vec![]);
    assert_eq!(run.movement_mode(), MovementMode::Original);
    run.step(moving(1.0, 0.0, true, false));
    assert!(speed(&run) > 0.8 && speed(&run) < 0.9);
    assert!(run.world.player.position[0] > 10.0);
    for tick in 0..60 {
        if tick != 0 {
            run.step(moving(1.0, 0.0, true, false));
        }
        walk.step(moving(1.0, 0.0, false, false));
        diagonal.step(moving(1.0, 1.0, true, false));
    }
    assert!((speed(&run) - 5.1975).abs() < 1e-5);
    assert!((speed(&walk) - 3.1185).abs() < 1e-5);
    assert!((speed(&diagonal) - 5.1975).abs() < 1e-5);
    // Independent continuous constant-acceleration distance: accelerate to v,
    // then cruise for the remainder of one second. Allow substep integration
    // and accumulated binary32 position rounding, not an instant-speed start.
    let ideal_distance = 5.1975 * (1.0 - 5.1975 / (2.0 * 52.5));
    let actual_distance = run.world.player.position[0] - 10.0;
    assert!(
        (actual_distance - ideal_distance).abs() < 0.004,
        "one-second run: {actual_distance}, continuous ideal: {ideal_distance}"
    );
    let diagonal_delta = diagonal.world.player.position.map(|v| v - 10.0);
    assert!(diagonal_delta[0] > 0.0 && diagonal_delta[1] < 0.0);
    assert!((diagonal_delta[0].hypot(diagonal_delta[1]) - actual_distance).abs() < 0.004);
    assert!((diagonal_delta[0] + diagonal_delta[1]).abs() < 0.001);
    let cruise_start = run.world.player.position[0];
    for _ in 0..60 {
        run.step(moving(1.0, 0.0, true, false));
    }
    assert!((run.world.player.position[0] - cruise_start - 5.1975).abs() < 0.004);
    run.step(SessionInput::default());
    assert!(speed(&run) > 4.2 && speed(&run) < 4.4);
    let braking_position = run.world.player.position[0];
    for _ in 0..10 {
        run.step(SessionInput::default());
    }
    assert_eq!(run.movement_velocity(), [0.0; 2]);
    assert!(run.world.player.position[0] > braking_position);
    let stopped = run.world.player.position;
    run.step(SessionInput::default());
    assert_eq!(run.world.player.position, stopped);
}

#[test]
fn quake_air_momentum_survives_serialization_and_fresh_baseline_restore() {
    let baseline = session(vec![]);
    let mut source = session(vec![]);
    source.set_movement_mode(MovementMode::Quake1).unwrap();
    for _ in 0..30 {
        source.step(moving(1.0, 0.0, true, false));
    }
    assert!((speed(&source) - 5.0).abs() < 1e-5);
    assert!(source.step(moving(1.0, 0.0, true, true)).vertical.jumped);
    let velocity = source.movement_velocity();
    let start = source.world.player.position;
    for _ in 0..5 {
        source.step(SessionInput::default());
        assert!(!source.vertical.grounded());
        assert_eq!(source.movement_velocity(), velocity);
    }
    assert!((source.world.player.position[0] - start[0] - 5.0 / 12.0).abs() < 1e-4);
    let digest = ContentDigest([17; 32]);
    let saved = source.snapshot(digest).unwrap();
    let saved: SessionSnapshot =
        serde_json::from_slice(&serde_json::to_vec(&saved).unwrap()).unwrap();
    assert_eq!(saved.movement.mode, MovementMode::Quake1);
    assert_eq!(saved.movement.velocity, velocity);
    assert!(saved.vertical.feet > 0.0);
    let mut restored = baseline.restore_from_baseline(&saved, digest).unwrap();
    assert_eq!(restored.movement_mode(), MovementMode::Quake1);
    assert_eq!(restored.snapshot(digest).unwrap(), saved);
    for tick in 0..120 {
        // Include air steering, landing, ground braking and another jump.
        let input = || moving(0.0, if tick < 20 { 1.0 } else { 0.0 }, true, tick == 80);
        let source_events = source.step(input());
        let restored_events = restored.step(input());
        assert_eq!(source_events.vertical, restored_events.vertical);
        assert_eq!(
            source.snapshot(digest).unwrap(),
            restored.snapshot(digest).unwrap()
        );
    }
}

#[test]
fn changing_mode_clears_horizontal_momentum_without_teleporting_feet() {
    let mut session = session(vec![]);
    for _ in 0..15 {
        session.step(moving(1.0, 0.0, true, false));
    }
    assert!(session.step(moving(1.0, 0.0, true, true)).vertical.jumped);
    assert!(speed(&session) > 5.0);
    for mode in [MovementMode::Quake1, MovementMode::Original] {
        let feet = session.vertical.feet();
        let position = session.world.player.position;
        session.set_movement_mode(mode).unwrap();
        assert_eq!(session.movement_mode(), mode);
        assert_eq!(session.movement_velocity(), [0.0; 2]);
        assert_eq!(session.world.player.position, position);
        assert_eq!(session.vertical.feet(), feet);
        assert!(feet.is_finite() && feet > 0.0);
        assert!(session.vertical.velocity().is_finite());
    }
    for _ in 0..120 {
        session.step(SessionInput::default());
    }
    assert!(session.vertical.grounded());
    assert_eq!(session.vertical.feet(), 0.0);
}

#[test]
fn held_jump_repeats_in_original_but_quake_requires_release() {
    for mode in [MovementMode::Original, MovementMode::Quake1] {
        let mut session = session(vec![]);
        session.set_movement_mode(mode).unwrap();
        let mut jumps = 0;
        let mut landings = 0;
        for _ in 0..150 {
            let events = session.step(moving(0.0, 0.0, false, true));
            jumps += usize::from(events.vertical.jumped);
            landings += usize::from(events.vertical.landed);
        }
        assert!(landings > 0);
        match mode {
            MovementMode::Original => assert!(jumps >= 3, "held Original jumps: {jumps}"),
            MovementMode::Quake1 => {
                assert_eq!(jumps, 1);
                assert!(session.vertical.grounded());
                session.step(SessionInput::default());
                assert!(session.step(moving(0.0, 0.0, false, true)).vertical.jumped);
            }
        }
    }
}

#[test]
fn one_tile_raised_door_clears_verified_body_but_lower_gap_blocks() {
    for mode in [MovementMode::Original, MovementMode::Quake1] {
        for (elevation, passable) in [(64.0, true), (56.0, false)] {
            let door = Wall {
                a: [11.0, 8.0],
                b: [11.0, 12.0],
                texture: 1,
                blocks_movement: true,
                cell: Some([11, 10]),
            };
            let mut session = session(vec![door]);
            session.set_movement_mode(mode).unwrap();
            assert_eq!(session.vertical.config().body_height, 0.9375);
            // Model the already-raised geometry directly; these fixtures are
            // deliberately not saved because no authored process raised it.
            session.wall_elevations[0] = elevation;
            for _ in 0..60 {
                session.step(moving(1.0, 0.0, true, false));
            }
            let x = session.world.player.position[0];
            if passable {
                assert!(x > 12.0, "{mode:?} stopped at {x} under one-tile gap");
            } else {
                assert!(
                    (10.70..=10.751).contains(&x),
                    "{mode:?} crossed low gap: {x}"
                );
                assert!(
                    speed(&session) < 0.01,
                    "contact must remove inward momentum"
                );
            }
            assert_eq!(session.vertical.feet(), 0.0);
        }
    }
}
