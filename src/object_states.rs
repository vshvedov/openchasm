//! Atomic, renderer-independent scripted transitions of level objects.
//!
//! An `explode` advances the authored level-object definition to its next row;
//! a `change` selects an explicit map object ID. Walls, pickup items, object
//! creation in empty cells, and invalid replacement definitions are rejected.
use crate::objects::{ObjectDefinition, ObjectDefinitions, ObjectInstance, ObjectKind};
use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;

const FIRST_OBJECT: u8 = 163;
const LAST_OBJECT: u8 = 226;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectAction {
    Explode { cell: [usize; 2] },
    Change { cell: [usize; 2], raw_type: u8 },
}

#[derive(Debug, Clone, PartialEq)]
pub struct ObjectState {
    /// Stable source placement; position, rotation and difficulty are retained
    /// as the model changes. Runtime motion may update the placement later.
    pub placement: ObjectInstance,
    pub raw_type: u8,
    pub health: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransitionKind {
    Explode,
    Change,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectTransition {
    pub cell: [usize; 2],
    pub kind: TransitionKind,
    pub previous_type: u8,
    pub new_type: u8,
    pub previous_health: i32,
    pub new_health: i32,
    /// Emitted as a request to reset animation after changing the mesh.
    pub reset_animation: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObjectStateError {
    DuplicateCell([usize; 2]),
    UnknownCell([usize; 2]),
    MissingDefinition { cell: [usize; 2], index: usize },
    UnsupportedType { cell: [usize; 2], raw_type: u8 },
    NoNextDefinition { cell: [usize; 2], raw_type: u8 },
}
impl fmt::Display for ObjectStateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateCell(cell) => write!(f, "duplicate level object at {cell:?}"),
            Self::UnknownCell(cell) => write!(f, "no level object at {cell:?}"),
            Self::MissingDefinition { cell, index } => write!(
                f,
                "object at {cell:?} references missing definition {index}"
            ),
            Self::UnsupportedType { cell, raw_type } => write!(
                f,
                "change at {cell:?} targets unsupported map type {raw_type}"
            ),
            Self::NoNextDefinition { cell, raw_type } => write!(
                f,
                "object type {raw_type} at {cell:?} has no next definition"
            ),
        }
    }
}
impl Error for ObjectStateError {}

/// Authoritative level-object type and health state, separate from loaded meshes.
/// The owning session must also synchronize its combat and renderer on events.
#[derive(Debug, Clone)]
pub struct ObjectStates {
    definitions: Vec<ObjectDefinition>,
    states: BTreeMap<[usize; 2], ObjectState>,
}
impl ObjectStates {
    pub fn new(
        instances: &[ObjectInstance],
        definitions: &ObjectDefinitions,
    ) -> Result<Self, ObjectStateError> {
        let mut states = BTreeMap::new();
        for instance in instances
            .iter()
            .filter(|instance| instance.kind == ObjectKind::LevelObject)
        {
            let index = instance.definition_index;
            let raw_type = usize::from(FIRST_OBJECT)
                .checked_add(index)
                .filter(|value| *value <= usize::from(LAST_OBJECT))
                .ok_or(ObjectStateError::MissingDefinition {
                    cell: instance.grid_cell,
                    index,
                })? as u8;
            let def = definitions.level_objects.get(index).ok_or(
                ObjectStateError::MissingDefinition {
                    cell: instance.grid_cell,
                    index,
                },
            )?;
            let cell = instance.grid_cell;
            let state = ObjectState {
                placement: instance.clone(),
                raw_type,
                health: def.break_limit.max(0),
            };
            if states.insert(cell, state).is_some() {
                return Err(ObjectStateError::DuplicateCell(cell));
            }
        }
        Ok(Self {
            definitions: definitions.level_objects.clone(),
            states,
        })
    }

    pub fn get(&self, cell: [usize; 2]) -> Option<&ObjectState> {
        self.states.get(&cell)
    }
    pub fn definition(&self, cell: [usize; 2]) -> Option<&ObjectDefinition> {
        let state = self.get(cell)?;
        self.definitions.get(state.placement.definition_index)
    }
    pub fn iter(&self) -> impl Iterator<Item = &ObjectState> {
        self.states.values()
    }

    /// Keep moving-object position aligned with authoritative process state.
    pub fn set_position(&mut self, cell: [usize; 2], position: [f32; 2]) -> bool {
        if !position.iter().all(|v| v.is_finite()) {
            return false;
        }
        if let Some(state) = self.states.get_mut(&cell) {
            state.placement.position = position;
            true
        } else {
            false
        }
    }

    /// Synchronize damage from the authoritative combat layer. A scripted
    /// explosion does not require positive health and does not call this method.
    pub fn set_health(&mut self, cell: [usize; 2], health: i32) -> bool {
        if health < 0 {
            return false;
        }
        if let Some(state) = self.states.get_mut(&cell) {
            state.health = health;
            true
        } else {
            false
        }
    }

    /// Resolve all actions against a temporary copy before publishing them.
    /// On an unsupported target, invalid replacement or missing cell, no state
    /// changes and no transitions are emitted. No destruction links are fired
    /// implicitly; the owner decides whether to dispatch associated effects.
    pub fn apply_batch(
        &mut self,
        actions: &[ObjectAction],
    ) -> Result<Vec<ObjectTransition>, ObjectStateError> {
        let mut staged = self.states.clone();
        let mut transitions = Vec::with_capacity(actions.len());
        for action in actions {
            let (cell, requested, kind) = match *action {
                ObjectAction::Explode { cell } => {
                    let state = staged
                        .get(&cell)
                        .ok_or(ObjectStateError::UnknownCell(cell))?;
                    let next = state
                        .raw_type
                        .checked_add(1)
                        .filter(|next| *next <= LAST_OBJECT)
                        .ok_or(ObjectStateError::NoNextDefinition {
                            cell,
                            raw_type: state.raw_type,
                        })?;
                    (cell, next, TransitionKind::Explode)
                }
                ObjectAction::Change { cell, raw_type } => (cell, raw_type, TransitionKind::Change),
            };
            if !(FIRST_OBJECT..=LAST_OBJECT).contains(&requested) {
                return Err(ObjectStateError::UnsupportedType {
                    cell,
                    raw_type: requested,
                });
            }
            let index = usize::from(requested - FIRST_OBJECT);
            let def = self.definitions.get(index).ok_or_else(|| match kind {
                TransitionKind::Explode => ObjectStateError::NoNextDefinition {
                    cell,
                    raw_type: requested - 1,
                },
                TransitionKind::Change => ObjectStateError::MissingDefinition { cell, index },
            })?;
            let state = staged
                .get_mut(&cell)
                .ok_or(ObjectStateError::UnknownCell(cell))?;
            let previous_type = state.raw_type;
            let previous_health = state.health;
            state.raw_type = requested;
            state.placement.definition_index = index;
            state.health = def.break_limit.max(0);
            transitions.push(ObjectTransition {
                cell,
                kind,
                previous_type,
                new_type: requested,
                previous_health,
                new_health: state.health,
                reset_animation: true,
            });
        }
        self.states = staged;
        Ok(transitions)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn def(name: &str, health: i32) -> ObjectDefinition {
        ObjectDefinition {
            collision_radius: 0,
            shadow: 0,
            bmp_object: -1,
            bmp_z: 0,
            action_code: 0,
            blow_up: 0,
            break_limit: health,
            ambient_sound: 0,
            break_sound: 0,
            model_name: name.into(),
            animation_name: None,
        }
    }
    fn obj(index: usize, cell: [usize; 2]) -> ObjectInstance {
        ObjectInstance {
            kind: ObjectKind::LevelObject,
            definition_index: index,
            grid_cell: cell,
            position: [2.25, 3.75],
            angle_radians: 1.5,
            flags: 3,
            difficulty_flags: 7,
            extra: 0,
            size: 0,
        }
    }
    #[test]
    fn scripted_explode_advances_even_indestructible_object() {
        let defs = ObjectDefinitions {
            items: vec![],
            level_objects: vec![def("barrier.3o", 0), def("dummy.3o", 17)],
        };
        let mut state = ObjectStates::new(&[obj(0, [1, 2])], &defs).unwrap();
        let result = state
            .apply_batch(&[ObjectAction::Explode { cell: [1, 2] }])
            .unwrap();
        assert_eq!(
            (
                result[0].previous_type,
                result[0].new_type,
                result[0].new_health
            ),
            (163, 164, 17)
        );
        assert!(result[0].reset_animation);
        assert_eq!(state.definition([1, 2]).unwrap().model_name, "dummy.3o");
        assert_eq!(state.get([1, 2]).unwrap().placement.position, [2.25, 3.75]);
        assert_eq!(state.get([1, 2]).unwrap().placement.angle_radians, 1.5);
    }
    #[test]
    fn change_resets_damage_to_authored_health_and_syncs_motion() {
        let defs = ObjectDefinitions {
            items: vec![],
            level_objects: vec![def("switch.3o", 20), def("portal.3o", 5)],
        };
        let mut state = ObjectStates::new(&[obj(0, [1, 2])], &defs).unwrap();
        assert!(state.set_health([1, 2], 2));
        assert!(state.set_position([1, 2], [9.0, 8.0]));
        let result = state
            .apply_batch(&[ObjectAction::Change {
                cell: [1, 2],
                raw_type: 164,
            }])
            .unwrap();
        assert_eq!((result[0].previous_health, result[0].new_health), (2, 5));
        assert_eq!(state.get([1, 2]).unwrap().placement.position, [9.0, 8.0]);
        assert!(!state.set_position([1, 2], [f32::NAN, 0.0]));
        assert!(!state.set_health([1, 2], -1));
    }
    #[test]
    fn invalid_actions_are_atomic_and_unknown_map_types_are_rejected() {
        let defs = ObjectDefinitions {
            items: vec![],
            level_objects: vec![def("a.3o", 1), def("b.3o", 0)],
        };
        let mut state = ObjectStates::new(&[obj(0, [1, 2]), obj(1, [3, 4])], &defs).unwrap();
        assert!(state
            .apply_batch(&[
                ObjectAction::Explode { cell: [1, 2] },
                ObjectAction::Explode { cell: [3, 4] }
            ])
            .is_err());
        assert_eq!(state.get([1, 2]).unwrap().raw_type, 163);
        assert!(matches!(
            state.apply_batch(&[ObjectAction::Change {
                cell: [1, 2],
                raw_type: 120
            }]),
            Err(ObjectStateError::UnsupportedType { .. })
        ));
        assert!(matches!(
            state.apply_batch(&[ObjectAction::Change {
                cell: [9, 9],
                raw_type: 164
            }]),
            Err(ObjectStateError::UnknownCell(_))
        ));
        assert!(matches!(
            state.apply_batch(&[ObjectAction::Change {
                cell: [1, 2],
                raw_type: 200
            }]),
            Err(ObjectStateError::MissingDefinition { .. })
        ));
    }
    #[test]
    fn missing_and_duplicate_placements_are_rejected() {
        let defs = ObjectDefinitions {
            items: vec![],
            level_objects: vec![def("a.3o", 1)],
        };
        assert!(matches!(
            ObjectStates::new(&[obj(1, [0, 0])], &defs),
            Err(ObjectStateError::MissingDefinition { .. })
        ));
        assert!(matches!(
            ObjectStates::new(&[obj(0, [0, 0]), obj(0, [0, 0])], &defs),
            Err(ObjectStateError::DuplicateCell(_))
        ));
    }
}
