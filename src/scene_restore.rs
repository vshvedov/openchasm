//! Rebuild scene presentation from a validated restored simulation session.
//!
//! This does not serialize a scene. Assets are resolved through the same
//! loader as a fresh level. Actor idle/run/death phase and script-controlled
//! object animation clocks are not authoritative and restart at their initial
//! pose; callers must disclose that limitation before offering durable saves.
use crate::content::ResourceSet;
use crate::object_render::{ActorAnimationMode, ObjectScene};
use openchasm::{
    ai::Mode,
    objects::{ObjectInstance, ObjectKind},
    pickups::Difficulty,
    session::Session,
};
use std::collections::BTreeSet;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SceneRestoreError {
    MissingObject([usize; 2]),
    MissingObjectState([usize; 2]),
    MissingObjectAsset([usize; 2], u8),
    InvalidObjectPose([usize; 2]),
    MissingActor(usize),
    InvalidActorPose(usize),
    MissingActorAnimation(usize),
}

/// Restore into a scene built from the same verified map and definitions. All
/// instance edits roll back on failure; preloaded immutable cache entries may
/// remain. Items, actor placements and object cells use original stable IDs.
#[allow(dead_code)] // No persistent save UI is enabled yet.
pub fn rebuild_scene(
    scene: &mut ObjectScene,
    resources: &mut ResourceSet,
    session: &Session,
    instances: &[ObjectInstance],
    difficulty: Difficulty,
) -> Result<(), SceneRestoreError> {
    let before = scene.snapshot();
    let result = rebuild_inner(scene, resources, session, instances, difficulty);
    if result.is_err() {
        // The snapshot was made from this same scene; newly loaded meshes only
        // append, so instance identities and old mesh indices remain valid.
        assert!(scene.restore(&before), "same-scene rollback must succeed");
    }
    result
}

fn rebuild_inner(
    scene: &mut ObjectScene,
    resources: &mut ResourceSet,
    session: &Session,
    instances: &[ObjectInstance],
    difficulty: Difficulty,
) -> Result<(), SceneRestoreError> {
    let mask = match difficulty {
        Difficulty::Easy => 1,
        Difficulty::Normal => 2,
        Difficulty::Hard => 4,
    };
    let hidden: BTreeSet<_> = session.game.pickups.hidden_cells().collect();
    scene.reset_level_objects();
    scene.reset_object_animations();
    for placement in instances {
        let cell = placement.grid_cell;
        let active = session
            .objects
            .as_ref()
            .and_then(|objects| objects.is_active(cell))
            .unwrap_or(true);
        let visible = placement.difficulty_flags & mask != 0 && active && !hidden.contains(&cell);
        if !scene.has_cell(cell) {
            if visible {
                return Err(SceneRestoreError::MissingObject(cell));
            }
            continue;
        }
        if !scene.set_visible(cell, visible) {
            return Err(SceneRestoreError::MissingObject(cell));
        }
        if !visible {
            continue;
        }
        if placement.kind == ObjectKind::LevelObject {
            if let Some(scripted) = &session.scripted_objects {
                let state = scripted
                    .states
                    .get(cell)
                    .ok_or(SceneRestoreError::MissingObjectState(cell))?;
                let authored = u8::try_from(placement.definition_index)
                    .ok()
                    .and_then(|n| n.checked_add(163));
                if !scene.preload_level_object(resources, &scripted.definitions, state.raw_type)
                    || (authored != Some(state.raw_type)
                        && !scene.replace_level_object(cell, state.raw_type))
                {
                    return Err(SceneRestoreError::MissingObjectAsset(cell, state.raw_type));
                }
            }
        }
        let pose = session
            .processes
            .objects()
            .find(|object| [usize::from(object.cell[0]), usize::from(object.cell[1])] == cell);
        let (position, angle, elevation) = if let Some(pose) = pose {
            (pose.position, pose.angle_radians, pose.elevation_raw)
        } else if let Some(state) = session
            .scripted_objects
            .as_ref()
            .and_then(|scripted| scripted.states.get(cell))
        {
            (state.placement.position, state.placement.angle_radians, 0.0)
        } else {
            (placement.position, placement.angle_radians, 0.0)
        };
        if !scene.set_pose(cell, position, angle, elevation) {
            return Err(SceneRestoreError::InvalidObjectPose(cell));
        }
    }
    for actor in &session.actors.actors {
        let mode = session
            .ai
            .mode(actor.source_index)
            .unwrap_or(Mode::Inactive);
        if mode == Mode::Inactive {
            let _ = scene.set_actor_pose(
                actor.source_index,
                actor.position,
                actor.angle_radians,
                false,
            );
            continue;
        }
        if !actor.position.iter().all(|v| v.is_finite()) || !actor.angle_radians.is_finite() {
            return Err(SceneRestoreError::InvalidActorPose(actor.source_index));
        }
        if !scene.set_actor_pose(
            actor.source_index,
            actor.position,
            actor.angle_radians,
            true,
        ) {
            return Err(SceneRestoreError::MissingActor(actor.source_index));
        }
        let animation = match mode {
            Mode::Inactive | Mode::Idle => ActorAnimationMode::Idle,
            Mode::Chasing => ActorAnimationMode::Run,
            Mode::Attacking => ActorAnimationMode::Attack,
            Mode::Dead => ActorAnimationMode::Death,
        };
        if !scene.set_actor_animation(actor.source_index, animation, 0.0) {
            return Err(SceneRestoreError::MissingActor(actor.source_index));
        }
        let phase = session
            .ai
            .attack_animation(actor.source_index)
            .map(|phase| (phase.slot, phase.frame));
        if !scene.set_actor_attack_frame(actor.source_index, phase) {
            return Err(SceneRestoreError::MissingActorAnimation(actor.source_index));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use openchasm::{
        actors::{ActorDefinitions, ActorState},
        combat::RocketDefinitions,
        game::GameInventory,
        inventory::{WeaponDefinition, WeaponDefinitions},
        objects::ObjectDefinitions,
        process::Script,
        process_runtime::ProcessRuntime,
        session::LevelSetup,
        world::{Player, World},
    };

    fn fixture(with_actor: bool) -> (Session, Vec<ObjectInstance>) {
        let world = World {
            walls: vec![],
            player: Player {
                position: [0.0, 0.0],
                angle: 0.0,
            },
        };
        let script = Script::parse("");
        let processes = ProcessRuntime::new(&script, &world);
        let placement = ObjectInstance {
            kind: ObjectKind::LevelObject,
            definition_index: 0,
            grid_cell: [1, 1],
            position: [4.0, 0.0],
            angle_radians: 0.0,
            flags: 0,
            difficulty_flags: 7,
            extra: 0,
            size: 64,
        };
        let definitions = ObjectDefinitions {
            items: vec![],
            level_objects: vec![],
        };
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
        let game = GameInventory::new(
            WeaponDefinitions([weapon; 8]),
            &[],
            &definitions,
            Difficulty::Normal,
        );
        let actor_definitions = ActorDefinitions::parse(
            b"[MONSTERS]\n2\nhero.car 1 1 1 1 1 1 1 1\nan4.car 1 1 1 1 100 1 1 1\n",
        )
        .unwrap();
        let placements = if with_actor {
            vec![openchasm::map::Monster {
                position: [256, 0],
                id: 101,
                angle: 0,
                flags: 7,
            }]
        } else {
            vec![]
        };
        let actors = ActorState::from_placements(&placements, &actor_definitions);
        let session = Session::new(LevelSetup {
            world,
            processes,
            actors,
            actor_definitions,
            game,
            rockets: RocketDefinitions(vec![]),
            shot_blocking: [true; 128],
            difficulty: Difficulty::Normal,
        });
        (session, vec![placement])
    }
    fn frame(scene: &ObjectScene) -> Vec<u32> {
        let mut image = vec![0; 64 * 64];
        let mut depth = vec![f32::INFINITY; image.len()];
        scene.render(
            &mut image,
            &mut depth,
            64,
            64,
            crate::object_render::CameraView {
                player: Player {
                    position: [0.0, 0.0],
                    angle: 0.0,
                },
                eye_height: 1.0,
            },
        );
        image
    }
    #[test]
    fn object_pose_rebuild_and_failure_roll_back_atomically() {
        let path =
            std::env::temp_dir().join(format!("openchasm-scene-restore-{}", std::process::id()));
        std::fs::create_dir_all(&path).unwrap();
        let mut resources = ResourceSet::open(&path).unwrap();
        let mut scene = crate::object_render::tests::gltf_scene(255);
        let before = frame(&scene);
        let (bad, instances) = fixture(true);
        assert_eq!(
            rebuild_scene(
                &mut scene,
                &mut resources,
                &bad,
                &instances,
                Difficulty::Normal
            ),
            Err(SceneRestoreError::MissingActor(0))
        );
        assert_eq!(
            frame(&scene),
            before,
            "failed restore must roll back earlier object pose changes"
        );
        let (good, instances) = fixture(false);
        rebuild_scene(
            &mut scene,
            &mut resources,
            &good,
            &instances,
            Difficulty::Normal,
        )
        .unwrap();
        assert_ne!(
            frame(&scene),
            before,
            "successful restore applies authoritative pose"
        );
        std::fs::remove_dir_all(path).unwrap();
    }
    #[test]
    fn missing_visible_object_is_error_but_hidden_one_is_not_required() {
        let path =
            std::env::temp_dir().join(format!("openchasm-scene-missing-{}", std::process::id()));
        std::fs::create_dir_all(&path).unwrap();
        let mut resources = ResourceSet::open(&path).unwrap();
        let mut scene = crate::object_render::tests::gltf_scene(255);
        let (session, mut instances) = fixture(false);
        instances[0].grid_cell = [8, 8];
        assert_eq!(
            rebuild_scene(
                &mut scene,
                &mut resources,
                &session,
                &instances,
                Difficulty::Normal
            ),
            Err(SceneRestoreError::MissingObject([8, 8]))
        );
        instances[0].difficulty_flags = 1;
        assert!(rebuild_scene(
            &mut scene,
            &mut resources,
            &session,
            &instances,
            Difficulty::Normal
        )
        .is_ok());
        std::fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn missing_replacement_asset_leaves_original_scene_intact() {
        use openchasm::{
            object_states::{ObjectAction, ObjectStates},
            objects::ObjectDefinition,
            process_transaction::PresentationManifest,
            session::ScriptedObjects,
        };
        let path = std::env::temp_dir().join(format!(
            "openchasm-scene-replacement-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).unwrap();
        let mut resources = ResourceSet::open(&path).unwrap();
        let mut scene = crate::object_render::tests::gltf_scene(255);
        let before = frame(&scene);
        let (mut session, instances) = fixture(false);
        let definition = |name: &str| ObjectDefinition {
            collision_radius: 1,
            shadow: 0,
            bmp_object: 0,
            bmp_z: 0,
            action_code: 0,
            blow_up: 0,
            break_limit: 1,
            ambient_sound: 0,
            break_sound: 0,
            model_name: name.to_owned(),
            animation_name: None,
        };
        let definitions = ObjectDefinitions {
            items: vec![],
            level_objects: vec![definition("original.3o"), definition("missing.3o")],
        };
        let mut states = ObjectStates::new(&instances, &definitions).unwrap();
        states
            .apply_batch(&[ObjectAction::Change {
                cell: [1, 1],
                raw_type: 164,
            }])
            .unwrap();
        session.set_scripted_objects(ScriptedObjects::new(
            states,
            definitions,
            PresentationManifest::default(),
            &Script::parse(""),
            false,
        ));
        assert_eq!(
            rebuild_scene(
                &mut scene,
                &mut resources,
                &session,
                &instances,
                Difficulty::Normal
            ),
            Err(SceneRestoreError::MissingObjectAsset([1, 1], 164))
        );
        assert_eq!(frame(&scene), before);
        std::fs::remove_dir_all(path).unwrap();
    }
}
