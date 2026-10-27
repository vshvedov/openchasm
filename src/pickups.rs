//! Authoritative pickup overlap and collection state, independent of graphics.
//!
//! Initial behavior follows the published PanzerChasm reconstruction: ordinary
//! items use a 100/256 tile horizontal radius and level-object keys use the
//! player's 60/256 radius plus the key's GoRad. Difficulty flags are checked at
//! level load. Vertical overlap, movement of pickups by map procedures, and
//! multiplayer respawns remain to be validated against the original game.
use crate::inventory::{Inventory, Pickup, PickupOutcome, WeaponDefinitions};
use crate::objects::{ObjectDefinitions, ObjectInstance, ObjectKind};
use std::collections::{BTreeMap, BTreeSet};

const ITEM_RADIUS: f32 = 100.0 / 256.0;
const PLAYER_RADIUS: f32 = 60.0 / 256.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Difficulty {
    Easy,
    Normal,
    Hard,
}
impl Difficulty {
    fn mask(self) -> u16 {
        match self {
            Self::Easy => 1,
            Self::Normal => 2,
            Self::Hard => 4,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Candidate {
    cell: [usize; 2],
    position: [f32; 2],
    action_code: i32,
    radius: f32,
    difficulty_enabled: bool,
    script_enabled: bool,
    collected: bool,
}

/// The cell is the stable placement identifier shared with the map renderer.
/// Emit these events to presentation, sounds, and map procedure links; inventory
/// state and collection are already updated by the time the event is returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PickupEvent {
    pub cell: [usize; 2],
    pub action_code: i32,
    pub pickup: Pickup,
}

/// Pickup candidates and whether each one has been consumed. State has no
/// clocks or transport-specific dependencies, so it can live on a game server.
#[derive(Debug, Clone, Default)]
pub struct Pickups {
    candidates: Vec<Candidate>,
}

/// Only mutable pickup state; action codes, radii and difficulty come from
/// freshly loaded level content, keyed by the original map cell.
#[derive(Debug, Clone, PartialEq)]
pub struct PickupSnapshot {
    pub cell: [u8; 2],
    pub position: [f32; 2],
    pub script_enabled: bool,
    pub collected: bool,
}
#[derive(Debug, Clone, PartialEq)]
pub struct PickupsSnapshot {
    pub entries: Vec<PickupSnapshot>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickupsSnapshotError {
    InvalidCell,
    DuplicateCell,
    CountMismatch,
    MissingCell,
    InvalidPosition,
}

impl Pickups {
    pub fn snapshot(&self) -> Result<PickupsSnapshot, PickupsSnapshotError> {
        let mut seen = BTreeSet::new();
        let mut entries = Vec::with_capacity(self.candidates.len());
        for candidate in &self.candidates {
            let [x, y] = candidate.cell;
            if x >= 64 || y >= 64 {
                return Err(PickupsSnapshotError::InvalidCell);
            }
            let cell = [x as u8, y as u8];
            if !seen.insert(cell) {
                return Err(PickupsSnapshotError::DuplicateCell);
            }
            if !candidate.position.iter().all(|v| v.is_finite()) {
                return Err(PickupsSnapshotError::InvalidPosition);
            }
            entries.push(PickupSnapshot {
                cell,
                position: candidate.position,
                script_enabled: candidate.script_enabled,
                collected: candidate.collected,
            });
        }
        Ok(PickupsSnapshot { entries })
    }
    /// Validate the entire saved cell set before applying any changes.
    pub fn restore(&mut self, snapshot: &PickupsSnapshot) -> Result<(), PickupsSnapshotError> {
        if snapshot.entries.len() != self.candidates.len() {
            return Err(PickupsSnapshotError::CountMismatch);
        }
        let mut saved = BTreeMap::new();
        for entry in &snapshot.entries {
            if entry.cell.iter().any(|x| *x >= 64) {
                return Err(PickupsSnapshotError::InvalidCell);
            }
            if !entry.position.iter().all(|v| v.is_finite()) {
                return Err(PickupsSnapshotError::InvalidPosition);
            }
            if saved.insert(entry.cell, entry).is_some() {
                return Err(PickupsSnapshotError::DuplicateCell);
            }
        }
        let mut known = BTreeSet::new();
        for candidate in &self.candidates {
            let [x, y] = candidate.cell;
            if x >= 64 || y >= 64 {
                return Err(PickupsSnapshotError::InvalidCell);
            }
            let cell = [x as u8, y as u8];
            if !known.insert(cell) {
                return Err(PickupsSnapshotError::DuplicateCell);
            }
            if !saved.contains_key(&cell) {
                return Err(PickupsSnapshotError::MissingCell);
            }
        }
        for candidate in &mut self.candidates {
            let value = saved[&[candidate.cell[0] as u8, candidate.cell[1] as u8]];
            candidate.position = value.position;
            candidate.script_enabled = value.script_enabled;
            candidate.collected = value.collected;
        }
        Ok(())
    }
    /// Construct single-player candidates from level instances. Missing object
    /// definitions, decorations, switches, and level objects other than keys
    /// are ignored. Unsupported item actions remain visible if approached.
    pub fn new(
        instances: &[ObjectInstance],
        definitions: &ObjectDefinitions,
        difficulty: Difficulty,
    ) -> Self {
        let mut candidates = Vec::new();
        for instance in instances {
            let Some(definition) = definitions.definition(instance) else {
                continue;
            };
            let radius = match instance.kind {
                ObjectKind::Item => ITEM_RADIUS,
                ObjectKind::LevelObject if (141..=143).contains(&definition.action_code) => {
                    PLAYER_RADIUS + definition.collision_radius.max(0) as f32 / 256.0
                }
                _ => continue,
            };
            candidates.push(Candidate {
                cell: instance.grid_cell,
                position: instance.position,
                action_code: definition.action_code,
                radius,
                difficulty_enabled: instance.difficulty_flags & difficulty.mask() != 0,
                script_enabled: true,
                collected: false,
            });
        }
        Self { candidates }
    }

    /// Advance pickup overlap for one player in a stable order. A collected
    /// candidate generates exactly one event; full and unsupported items remain
    /// available for later ticks. The player and pickup Z coordinates are not
    /// represented yet, matching the reference's horizontal-distance test.
    pub fn collect_near(
        &mut self,
        position: [f32; 2],
        inventory: &mut Inventory,
        weapons: &WeaponDefinitions,
    ) -> Vec<PickupEvent> {
        self.collect_near_with_powerups(position, inventory, weapons, None)
    }
    /// Optional protection effects are owned by the game session.
    pub fn collect_near_with_powerups(
        &mut self,
        position: [f32; 2],
        inventory: &mut Inventory,
        weapons: &WeaponDefinitions,
        mut powerups: Option<&mut crate::inventory::ProtectivePowerups>,
    ) -> Vec<PickupEvent> {
        let mut events = Vec::new();
        if !position.iter().all(|value| value.is_finite()) {
            return events;
        }
        for candidate in &mut self.candidates {
            if !candidate.difficulty_enabled || !candidate.script_enabled || candidate.collected {
                continue;
            }
            let dx = position[0] - candidate.position[0];
            let dy = position[1] - candidate.position[1];
            if dx * dx + dy * dy > candidate.radius * candidate.radius {
                continue;
            }
            let outcome = if matches!(candidate.action_code, 132 | 133) {
                powerups
                    .as_deref_mut()
                    .map_or(PickupOutcome::Unsupported, |effects| {
                        effects.pickup(candidate.action_code)
                    })
            } else {
                inventory.pickup(candidate.action_code, weapons)
            };
            if let PickupOutcome::Collected(pickup) = outcome {
                candidate.collected = true;
                events.push(PickupEvent {
                    cell: candidate.cell,
                    action_code: candidate.action_code,
                    pickup,
                });
            }
        }
        events
    }

    /// Report visibility for tracked pickup cells. `None` means this subsystem
    /// has no candidate for the cell; it must not hide unrelated scene objects.
    pub fn is_visible(&self, cell: [usize; 2]) -> Option<bool> {
        self.candidates
            .iter()
            .find(|candidate| candidate.cell == cell)
            .map(|candidate| {
                candidate.difficulty_enabled && candidate.script_enabled && !candidate.collected
            })
    }

    /// Current visibility for every tracked pickup placement.
    pub fn visibility(&self) -> impl Iterator<Item = ([usize; 2], bool)> + '_ {
        self.candidates.iter().map(|candidate| {
            (
                candidate.cell,
                candidate.difficulty_enabled && candidate.script_enabled && !candidate.collected,
            )
        })
    }

    /// Iterate tracked cells that are hidden because of difficulty or state.
    /// Call at level initialization and after script enable/disable changes.
    pub fn hidden_cells(&self) -> impl Iterator<Item = [usize; 2]> + '_ {
        self.candidates
            .iter()
            .filter(|candidate| {
                !candidate.difficulty_enabled || !candidate.script_enabled || candidate.collected
            })
            .map(|candidate| candidate.cell)
    }

    /// Toggle a candidate for a map procedure without bypassing difficulty or
    /// resurrecting an already collected pickup. Returns false for unknown cells.
    pub fn set_enabled(&mut self, cell: [usize; 2], enabled: bool) -> bool {
        if let Some(candidate) = self
            .candidates
            .iter_mut()
            .find(|candidate| candidate.cell == cell)
        {
            candidate.script_enabled = enabled;
            true
        } else {
            false
        }
    }

    /// Update a moving candidate from authoritative procedure state.
    pub fn set_position(&mut self, cell: [usize; 2], position: [f32; 2]) -> bool {
        if !position.iter().all(|value| value.is_finite()) {
            return false;
        }
        if let Some(candidate) = self
            .candidates
            .iter_mut()
            .find(|candidate| candidate.cell == cell)
        {
            candidate.position = position;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inventory::WeaponDefinition;
    use crate::objects::ObjectDefinition;

    #[test]
    fn snapshot_restores_by_cell_and_rejects_invalid_set_atomically() {
        let defs = ObjectDefinitions {
            items: vec![definition(102, 0)],
            level_objects: vec![],
        };
        let items = [
            instance(ObjectKind::Item, 0, [1, 1], [1.5, 1.5], 7),
            instance(ObjectKind::Item, 0, [2, 1], [2.5, 1.5], 7),
        ];
        let mut source = Pickups::new(&items, &defs, Difficulty::Normal);
        source.set_position([1, 1], [4.0, 4.0]);
        source.set_enabled([2, 1], false);
        let mut snapshot = source.snapshot().unwrap();
        snapshot.entries.reverse();
        let mut target = Pickups::new(&items, &defs, Difficulty::Normal);
        target.restore(&snapshot).unwrap();
        assert_eq!(target.snapshot().unwrap(), source.snapshot().unwrap());
        let before = target.snapshot().unwrap();
        snapshot.entries[1].cell = snapshot.entries[0].cell;
        assert_eq!(
            target.restore(&snapshot),
            Err(PickupsSnapshotError::DuplicateCell)
        );
        assert_eq!(target.snapshot().unwrap(), before);
        snapshot = before.clone();
        snapshot.entries[1].position[0] = f32::NAN;
        assert_eq!(
            target.restore(&snapshot),
            Err(PickupsSnapshotError::InvalidPosition)
        );
        assert_eq!(target.snapshot().unwrap(), before);
    }

    fn definition(action_code: i32, collision_radius: i32) -> ObjectDefinition {
        ObjectDefinition {
            collision_radius,
            shadow: 0,
            bmp_object: -1,
            bmp_z: 0,
            action_code,
            blow_up: 0,
            break_limit: 0,
            ambient_sound: 0,
            break_sound: 0,
            model_name: "test.3o".to_string(),
            animation_name: None,
        }
    }
    fn instance(
        kind: ObjectKind,
        index: usize,
        cell: [usize; 2],
        position: [f32; 2],
        difficulty_flags: u16,
    ) -> ObjectInstance {
        ObjectInstance {
            kind,
            definition_index: index,
            grid_cell: cell,
            position,
            angle_radians: 0.0,
            flags: 0,
            difficulty_flags,
            extra: 0,
            size: 0,
        }
    }
    fn weapons() -> WeaponDefinitions {
        WeaponDefinitions(
            [WeaponDefinition {
                rocket_type: 0,
                reload_time: 0,
                vertical_shift: 0,
                projectile_height: 0,
                ammo_per_portion: 10,
                ammo_limit: 50,
                ammo_on_weapon_pickup: 20,
                projectile_count: 1,
            }; 8],
        )
    }
    #[test]
    fn full_health_stays_visible_then_is_collected_once() {
        let definitions = ObjectDefinitions {
            items: vec![definition(102, 0)],
            level_objects: vec![],
        };
        let objects = vec![instance(ObjectKind::Item, 0, [3, 4], [1.0, 1.0], 7)];
        let mut state = Pickups::new(&objects, &definitions, Difficulty::Normal);
        let spec = weapons();
        let mut player = Inventory::new(&spec);
        assert!(state
            .collect_near([1.0, 1.0], &mut player, &spec)
            .is_empty());
        assert_eq!(state.is_visible([3, 4]), Some(true));
        player.health = 80;
        assert!(state
            .collect_near([1.0 + ITEM_RADIUS + 0.01, 1.0], &mut player, &spec)
            .is_empty());
        let events = state.collect_near([1.0, 1.0], &mut player, &spec);
        assert_eq!(
            events,
            vec![PickupEvent {
                cell: [3, 4],
                action_code: 102,
                pickup: Pickup::Health
            }]
        );
        assert_eq!(player.health, 100);
        assert_eq!(state.is_visible([3, 4]), Some(false));
        assert!(state
            .collect_near([1.0, 1.0], &mut player, &spec)
            .is_empty());
    }
    #[test]
    fn key_uses_its_radius_and_reports_exact_cell() {
        let definitions = ObjectDefinitions {
            items: vec![],
            level_objects: vec![definition(141, 64)],
        };
        let objects = vec![instance(ObjectKind::LevelObject, 0, [1, 2], [2.0, 3.0], 7)];
        let mut state = Pickups::new(&objects, &definitions, Difficulty::Normal);
        let spec = weapons();
        let mut player = Inventory::new(&spec);
        assert!(state
            .collect_near([2.0 + PLAYER_RADIUS + 0.26, 3.0], &mut player, &spec)
            .is_empty());
        assert_eq!(
            state.collect_near([2.0 + PLAYER_RADIUS + 0.24, 3.0], &mut player, &spec)[0].cell,
            [1, 2]
        );
        assert!(player.keys[0]);
    }
    #[test]
    fn difficulty_script_and_unknown_cells_control_visibility() {
        let definitions = ObjectDefinitions {
            items: vec![definition(22, 0), definition(133, 0)],
            level_objects: vec![],
        };
        let objects = vec![
            instance(ObjectKind::Item, 0, [1, 1], [0.0, 0.0], 1),
            instance(ObjectKind::Item, 0, [2, 2], [0.0, 0.0], 7),
            instance(ObjectKind::Item, 1, [3, 3], [0.0, 0.0], 7),
        ];
        let mut state = Pickups::new(&objects, &definitions, Difficulty::Normal);
        assert_eq!(state.hidden_cells().collect::<Vec<_>>(), vec![[1, 1]]);
        assert_eq!(state.is_visible([8, 8]), None);
        assert!(state.set_enabled([2, 2], false));
        let spec = weapons();
        let mut player = Inventory::new(&spec);
        assert!(state
            .collect_near([0.0, 0.0], &mut player, &spec)
            .is_empty());
        assert_eq!(state.is_visible([3, 3]), Some(true)); // unimplemented power-up
        assert!(state.set_enabled([2, 2], true));
        assert_eq!(state.collect_near([0.0, 0.0], &mut player, &spec).len(), 1);
        assert!(state.set_enabled([2, 2], false));
        assert!(state.set_enabled([2, 2], true));
        assert_eq!(state.is_visible([2, 2]), Some(false)); // script must not respawn
    }
    #[test]
    fn moving_and_invalid_positions_are_handled() {
        let definitions = ObjectDefinitions {
            items: vec![definition(22, 0)],
            level_objects: vec![],
        };
        let objects = vec![instance(ObjectKind::Item, 0, [1, 1], [0.0, 0.0], 7)];
        let mut state = Pickups::new(&objects, &definitions, Difficulty::Normal);
        assert!(!state.set_position([1, 1], [f32::NAN, 0.0]));
        assert!(state.set_position([1, 1], [5.0, 0.0]));
        let spec = weapons();
        let mut player = Inventory::new(&spec);
        assert!(state
            .collect_near([f32::NAN, 0.0], &mut player, &spec)
            .is_empty());
        assert!(state
            .collect_near([0.0, 0.0], &mut player, &spec)
            .is_empty());
        assert_eq!(state.collect_near([5.0, 0.0], &mut player, &spec).len(), 1);
    }
}
