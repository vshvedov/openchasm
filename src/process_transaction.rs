//! Validation and staging for process-owned object and quake commands.
//!
//! Object transitions are deliberately not committable yet: `PreparedProcess`
//! cannot update its staged runtime's object cell types and collision poses.
//! Accepting one would leave movement/collision state inconsistent. The host
//! must also synchronize presentation resources before enabling such commits.
use crate::object_combat::ObjectCombat;
use crate::object_states::{ObjectStateError, ObjectStates, ObjectTransition};
use crate::objects::ObjectDefinitions;
use crate::process_runtime::{ImmediateAction, PreparedProcess, ProcessEvent};
use crate::quake::{QuakeError, QuakeField};
use std::collections::BTreeSet;

/// Cells and replacement types for which the client has already loaded an
/// instance and its replacement model. Server callers may build the same
/// capability manifest from their content package instead of a renderer.
#[derive(Clone, Debug, Default)]
pub struct PresentationManifest {
    replacements: BTreeSet<([usize; 2], u8)>,
}
impl PresentationManifest {
    pub fn allow(&mut self, cell: [usize; 2], raw_type: u8) {
        self.replacements.insert((cell, raw_type));
    }
    pub fn contains(&self, cell: [usize; 2], raw_type: u8) -> bool {
        self.replacements.contains(&(cell, raw_type))
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum TransactionError {
    Object(ObjectStateError),
    CombatRejected {
        cell: [usize; 2],
    },
    PresentationUnavailable {
        cell: [usize; 2],
        raw_type: u8,
    },
    Quake(QuakeError),
    /// ProcessRuntime needs a transactional update for type, pose and radius.
    RuntimeObjectSynchronizationUnavailable,
}

/// Holds exclusive borrows until validation and commit are complete. Dropping
/// this value leaves all original states and process effects untouched.
pub struct ProcessTransaction<'a> {
    process: PreparedProcess<'a>,
    object_states: &'a mut ObjectStates,
    object_combat: &'a mut ObjectCombat,
    quake_field: &'a mut QuakeField,
    staged_states: ObjectStates,
    staged_combat: ObjectCombat,
    staged_quake: QuakeField,
    transitions: Vec<ObjectTransition>,
}
impl<'a> ProcessTransaction<'a> {
    pub fn prepare(
        process: PreparedProcess<'a>,
        states: &'a mut ObjectStates,
        combat: &'a mut ObjectCombat,
        quake: &'a mut QuakeField,
        definitions: &ObjectDefinitions,
        manifest: &PresentationManifest,
    ) -> Result<Self, TransactionError> {
        let mut staged_states = states.clone();
        let mut staged_combat = combat.clone();
        let mut staged_quake = quake.clone();
        let mut transitions = Vec::new();
        for action in process.actions() {
            match *action {
                ImmediateAction::Object(action) => {
                    let changes = staged_states
                        .apply_batch(&[action])
                        .map_err(TransactionError::Object)?;
                    let change = &changes[0];
                    if !manifest.contains(change.cell, change.new_type) {
                        return Err(TransactionError::PresentationUnavailable {
                            cell: change.cell,
                            raw_type: change.new_type,
                        });
                    }
                    if !staged_combat.apply_transitions(&changes, definitions) {
                        return Err(TransactionError::CombatRejected { cell: change.cell });
                    }
                    transitions.extend(changes);
                }
                ImmediateAction::Quake(zone) => staged_quake
                    .apply_batch(&[zone])
                    .map_err(TransactionError::Quake)?,
            }
        }
        Ok(Self {
            process,
            object_states: states,
            object_combat: combat,
            quake_field: quake,
            staged_states,
            staged_combat,
            staged_quake,
            transitions,
        })
    }
    pub fn transitions(&self) -> &[ObjectTransition] {
        &self.transitions
    }

    /// Commit only after all external presentation and collision consumers are
    /// known to accept the change. Object commits are disabled until the
    /// process runtime can stage corresponding object pose/type updates.
    pub fn commit(self) -> Result<ProcessEvent, TransactionError> {
        if !self.transitions.is_empty() {
            return Err(TransactionError::RuntimeObjectSynchronizationUnavailable);
        }
        *self.object_states = self.staged_states;
        *self.object_combat = self.staged_combat;
        *self.quake_field = self.staged_quake;
        Ok(self.process.commit())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::objects::{ObjectDefinition, ObjectInstance, ObjectKind};
    use crate::pickups::Difficulty;
    use crate::process::Script;
    use crate::process_runtime::{ProcessCapabilities, ProcessOutcome, ProcessRuntime};
    use crate::world::{Player, World};

    fn fixtures(script: &Script) -> (ObjectDefinitions, ObjectStates, ObjectCombat, World) {
        let def = |name: &str| ObjectDefinition {
            collision_radius: 32,
            shadow: 0,
            bmp_object: 0,
            bmp_z: 0,
            action_code: 0,
            blow_up: 0,
            break_limit: 10,
            ambient_sound: 0,
            break_sound: 0,
            model_name: name.into(),
            animation_name: None,
        };
        let defs = ObjectDefinitions {
            items: vec![],
            level_objects: vec![def("a"), def("b"), def("c")],
        };
        let instance = ObjectInstance {
            kind: ObjectKind::LevelObject,
            definition_index: 0,
            grid_cell: [1, 1],
            position: [1.5, 1.5],
            angle_radians: 0.0,
            flags: 0,
            difficulty_flags: 7,
            extra: 0,
            size: 0,
        };
        let objects = vec![instance];
        let states = ObjectStates::new(&objects, &defs).unwrap();
        let combat = ObjectCombat::new(&objects, &defs, script, Difficulty::Normal);
        let world = World {
            walls: vec![],
            player: Player {
                position: [-10.0, -10.0],
                angle: 0.0,
            },
        };
        (defs, states, combat, world)
    }
    fn script(actions: &str) -> Script {
        Script::parse(&format!(
            "#proc1\n#action\n{actions}\n#end\n#links\nlink 0 0 1\n#end\n"
        ))
    }
    #[test]
    fn missing_replacement_and_incomplete_runtime_sync_leave_everything_unchanged() {
        let script = script("explode 1 1\nquake 1 1 2 2 99\nchange 1 1 165");
        let (defs, mut states, mut combat, world) = fixtures(&script);
        let mut runtime = ProcessRuntime::with_staged_actions(
            &script,
            &world,
            ProcessCapabilities::default(),
            &[0; 4096],
            &[],
        )
        .unwrap();
        let mut quake = QuakeField::new();
        let mut manifest = PresentationManifest::default();
        manifest.allow([1, 1], 164);
        let prepared = runtime.prepare_at("link", [0, 0], 1).unwrap();
        let result = ProcessTransaction::prepare(
            prepared,
            &mut states,
            &mut combat,
            &mut quake,
            &defs,
            &manifest,
        );
        assert!(matches!(
            result,
            Err(TransactionError::PresentationUnavailable {
                cell: [1, 1],
                raw_type: 165
            })
        ));
        assert_eq!(states.get([1, 1]).unwrap().raw_type, 163);
        assert_eq!(quake.cell([1, 1]), Some(0));
        assert!(runtime.take_effects().is_empty());
        manifest.allow([1, 1], 165);
        let prepared = runtime.prepare_at("link", [0, 0], 1).unwrap();
        let transaction = ProcessTransaction::prepare(
            prepared,
            &mut states,
            &mut combat,
            &mut quake,
            &defs,
            &manifest,
        )
        .unwrap();
        assert_eq!(
            transaction
                .transitions()
                .iter()
                .map(|x| x.new_type)
                .collect::<Vec<_>>(),
            vec![164, 165]
        );
        assert_eq!(
            transaction.commit(),
            Err(TransactionError::RuntimeObjectSynchronizationUnavailable)
        );
        assert_eq!(states.get([1, 1]).unwrap().raw_type, 163);
        assert_eq!(quake.cell([1, 1]), Some(0));
        assert!(runtime.take_effects().is_empty());
    }
    #[test]
    fn quake_only_transaction_commits_and_drop_rolls_back() {
        let script = script("quake 1 1 2 2 99");
        let (defs, mut states, mut combat, world) = fixtures(&script);
        let mut runtime = ProcessRuntime::with_staged_actions(
            &script,
            &world,
            ProcessCapabilities::default(),
            &[0; 4096],
            &[],
        )
        .unwrap();
        let mut quake = QuakeField::new();
        let manifest = PresentationManifest::default();
        let prepared = runtime.prepare_at("link", [0, 0], 1).unwrap();
        let transaction = ProcessTransaction::prepare(
            prepared,
            &mut states,
            &mut combat,
            &mut quake,
            &defs,
            &manifest,
        )
        .unwrap();
        drop(transaction);
        assert_eq!(quake.cell([1, 1]), Some(0));
        let prepared = runtime.prepare_at("link", [0, 0], 1).unwrap();
        let transaction = ProcessTransaction::prepare(
            prepared,
            &mut states,
            &mut combat,
            &mut quake,
            &defs,
            &manifest,
        )
        .unwrap();
        assert_eq!(
            transaction.commit().unwrap().outcome,
            ProcessOutcome::Accepted
        );
        assert_eq!(quake.cell([1, 1]), Some(99));
    }
    #[test]
    fn mismatched_combat_state_rejects_without_mutation() {
        let script = script("explode 1 1");
        let (defs, mut states, _, world) = fixtures(&script);
        let mut combat = ObjectCombat::default();
        let mut runtime = ProcessRuntime::with_staged_actions(
            &script,
            &world,
            ProcessCapabilities::default(),
            &[0; 4096],
            &[],
        )
        .unwrap();
        let mut quake = QuakeField::new();
        let mut manifest = PresentationManifest::default();
        manifest.allow([1, 1], 164);
        let prepared = runtime.prepare_at("link", [0, 0], 1).unwrap();
        let result = ProcessTransaction::prepare(
            prepared,
            &mut states,
            &mut combat,
            &mut quake,
            &defs,
            &manifest,
        );
        assert!(matches!(
            result,
            Err(TransactionError::CombatRejected { cell: [1, 1] })
        ));
        assert_eq!(states.get([1, 1]).unwrap().raw_type, 163);
        assert!(runtime.take_effects().is_empty());
    }
}
