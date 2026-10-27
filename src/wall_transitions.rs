//! Headless preflight for replacing an existing wall's material.
//!
//! The host supplies a catalog derived from the loaded level's GFX table,
//! presentation availability, elevation-aware blocking, and entity clearance.
//! The returned copy is unpublished until the enclosing process commits.
use crate::process_runtime::RuntimeWallTransition;
use crate::world::{Wall, World};

const MATERIAL_COUNT: usize = 128;
const MAX_TRANSITIONS: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WallMaterial {
    pub blocks_movement: bool,
    pub blocks_shots: bool,
    /// The host has preloaded the visible texture or supports an invisible wall.
    pub presentation_available: bool,
}

/// An absent entry means unknown, and therefore cannot be substituted.
#[derive(Clone, Debug)]
pub struct WallMaterialCatalog {
    entries: [Option<WallMaterial>; MATERIAL_COUNT],
}
impl Default for WallMaterialCatalog {
    fn default() -> Self {
        Self {
            entries: [None; MATERIAL_COUNT],
        }
    }
}
impl WallMaterialCatalog {
    pub fn set(&mut self, raw_type: u8, material: WallMaterial) -> bool {
        if !(1..=127).contains(&raw_type) {
            return false;
        }
        self.entries[usize::from(raw_type)] = Some(material);
        true
    }
    pub fn get(&self, raw_type: u8) -> Option<WallMaterial> {
        self.entries.get(usize::from(raw_type)).copied().flatten()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WallTransitionError {
    TooManyTransitions,
    WallCount,
    WallIdentity,
    StaleType,
    InvalidMaterial,
    MaterialUnavailable,
    PresentationUnavailable,
    MovementMismatch,
    InvalidGeometry,
    CollisionNotClear,
}

/// Prepared private world and its elevation-independent movement flags.
/// Shot collision is looked up in the catalog by each wall's current type.
#[derive(Clone)]
pub struct PreparedWallTransitions {
    pub world: World,
    pub base_blocking: Vec<bool>,
    pub transitions: Vec<RuntimeWallTransition>,
}

/// Validate a complete ordered batch without modifying the caller's state.
/// `effective_blocking` applies the host's current elevation/height rules.
/// `is_clear` must test the actual wall segment against the player and all
/// relevant living actors; it is called whenever a material becomes solid.
pub fn prepare_wall_transitions(
    world: &World,
    base_blocking: &[bool],
    catalog: &WallMaterialCatalog,
    transitions: &[RuntimeWallTransition],
    mut effective_blocking: impl FnMut(usize, bool) -> bool,
    mut is_clear: impl FnMut(usize, &Wall) -> bool,
) -> Result<PreparedWallTransitions, WallTransitionError> {
    use WallTransitionError as E;
    if transitions.len() > MAX_TRANSITIONS {
        return Err(E::TooManyTransitions);
    }
    if base_blocking.len() != world.walls.len() {
        return Err(E::WallCount);
    }
    let mut staged = world.clone();
    let mut blocking = base_blocking.to_vec();
    for transition in transitions {
        let wall = staged
            .walls
            .get(transition.wall_index)
            .ok_or(E::WallIdentity)?;
        if wall.cell != Some(transition.cell)
            || staged
                .walls
                .iter()
                .filter(|w| w.cell == Some(transition.cell))
                .count()
                != 1
        {
            return Err(E::WallIdentity);
        }
        if wall.texture != transition.previous_type {
            return Err(E::StaleType);
        }
        if !(1..=127).contains(&transition.previous_type)
            || !(1..=127).contains(&transition.new_type)
        {
            return Err(E::InvalidMaterial);
        }
        let old = catalog
            .get(transition.previous_type)
            .ok_or(E::MaterialUnavailable)?;
        let new = catalog
            .get(transition.new_type)
            .ok_or(E::MaterialUnavailable)?;
        if !new.presentation_available {
            return Err(E::PresentationUnavailable);
        }
        if blocking[transition.wall_index] != old.blocks_movement {
            return Err(E::MovementMismatch);
        }
        if wall
            .a
            .iter()
            .chain(wall.b.iter())
            .any(|n| !n.is_finite() || n.abs() > 4096.0)
            || wall.a == wall.b
        {
            return Err(E::InvalidGeometry);
        }
        if !old.blocks_movement && new.blocks_movement && !is_clear(transition.wall_index, wall) {
            return Err(E::CollisionNotClear);
        }
        let wall = &mut staged.walls[transition.wall_index];
        wall.texture = transition.new_type;
        wall.blocks_movement =
            new.blocks_movement && effective_blocking(transition.wall_index, new.blocks_movement);
        blocking[transition.wall_index] = new.blocks_movement;
    }
    Ok(PreparedWallTransitions {
        world: staged,
        base_blocking: blocking,
        transitions: transitions.to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::Player;
    fn fixture() -> (World, WallMaterialCatalog) {
        let world = World {
            player: Player {
                position: [0.5, 0.5],
                angle: 0.0,
            },
            walls: vec![Wall {
                a: [1.0, 0.0],
                b: [1.0, 1.0],
                texture: 87,
                blocks_movement: false,
                cell: Some([1, 0]),
            }],
        };
        let mut catalog = WallMaterialCatalog::default();
        catalog.set(
            87,
            WallMaterial {
                blocks_movement: false,
                blocks_shots: true,
                presentation_available: true,
            },
        );
        catalog.set(
            2,
            WallMaterial {
                blocks_movement: true,
                blocks_shots: false,
                presentation_available: true,
            },
        );
        catalog.set(
            120,
            WallMaterial {
                blocks_movement: false,
                blocks_shots: false,
                presentation_available: true,
            },
        );
        (world, catalog)
    }
    fn change(from: u8, to: u8) -> RuntimeWallTransition {
        RuntimeWallTransition {
            cell: [1, 0],
            wall_index: 0,
            previous_type: from,
            new_type: to,
        }
    }
    #[test]
    fn ordered_transitions_apply_independent_flags_and_preserve_original() {
        let (world, catalog) = fixture();
        let mut checks = 0;
        let staged = prepare_wall_transitions(
            &world,
            &[false],
            &catalog,
            &[change(87, 2), change(2, 120)],
            |_, base| base,
            |_, wall| {
                checks += 1;
                assert_eq!(wall.texture, 87);
                true
            },
        )
        .unwrap();
        assert_eq!(checks, 1);
        assert_eq!(world.walls[0].texture, 87);
        assert_eq!(staged.world.walls[0].texture, 120);
        assert!(!staged.base_blocking[0]);
        assert!(!staged.world.walls[0].blocks_movement);
        assert!(!catalog.get(2).unwrap().blocks_shots);
        assert!(catalog.get(87).unwrap().blocks_shots);
    }
    #[test]
    fn rejects_collision_unavailable_stale_ambiguous_and_invalid_without_mutation() {
        let (world, mut catalog) = fixture();
        let run = |world: &World,
                   catalog: &WallMaterialCatalog,
                   batch: &[RuntimeWallTransition]| {
            prepare_wall_transitions(world, &[false], catalog, batch, |_, b| b, |_, _| false).err()
        };
        assert_eq!(
            run(&world, &catalog, &[change(87, 2)]),
            Some(WallTransitionError::CollisionNotClear)
        );
        assert_eq!(
            run(&world, &catalog, &[change(2, 120)]),
            Some(WallTransitionError::StaleType)
        );
        assert_eq!(
            run(&world, &catalog, &[change(87, 0)]),
            Some(WallTransitionError::InvalidMaterial)
        );
        catalog.set(
            2,
            WallMaterial {
                blocks_movement: true,
                blocks_shots: false,
                presentation_available: false,
            },
        );
        assert_eq!(
            run(&world, &catalog, &[change(87, 2)]),
            Some(WallTransitionError::PresentationUnavailable)
        );
        let mut ambiguous = world.clone();
        ambiguous.walls.push(ambiguous.walls[0]);
        assert_eq!(
            prepare_wall_transitions(
                &ambiguous,
                &[false, false],
                &catalog,
                &[change(87, 2)],
                |_, b| b,
                |_, _| true
            )
            .err(),
            Some(WallTransitionError::WallIdentity)
        );
        assert_eq!(world.walls[0].texture, 87);
    }
    #[test]
    fn caller_controls_elevation_and_clearance_for_new_solid_wall() {
        let (world, catalog) = fixture();
        let result = prepare_wall_transitions(
            &world,
            &[false],
            &catalog,
            &[change(87, 2)],
            |_, _| false,
            |_, _| true,
        )
        .unwrap();
        assert!(result.base_blocking[0]);
        assert!(!result.world.walls[0].blocks_movement);
        assert_eq!(result.world.walls[0].texture, 2);
    }
}
