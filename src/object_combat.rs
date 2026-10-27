//! Provisional horizontal shot selection and damage for map objects.
//!
//! Destructible objects and objects explicitly referenced by `shoot` /
//! `destroy` links are shootable; other placements are retained for changes. Hit shapes are approximate cylinders, not
//! original 3O geometry. Callers compare the reported distance against walls
//! and actors before applying damage or dispatching process-link events.
use crate::map::UNITS_PER_TILE;
use crate::object_states::{ObjectTransition, TransitionKind};
use crate::objects::{ObjectDefinitions, ObjectInstance, ObjectKind};
use crate::pickups::Difficulty;
use crate::process::Script;
use std::collections::BTreeSet;

const DEFAULT_RADIUS: f32 = 0.125;
const MAX_RADIUS: f32 = 2.0;

fn target_radius(def: &crate::objects::ObjectDefinition) -> f32 {
    if def.collision_radius > 0 {
        (def.collision_radius as f32 / UNITS_PER_TILE).clamp(DEFAULT_RADIUS, MAX_RADIUS)
    } else {
        0.0
    }
}

fn is_shootable(def: &crate::objects::ObjectDefinition, linked: bool) -> bool {
    // The authored dummy model represents an empty replacement cell.
    !def.model_name.eq_ignore_ascii_case("dummy.3o") && (def.break_limit > 0 || linked)
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Target {
    cell: [usize; 2],
    raw_type: u8,
    linked: bool,
    shootable: bool,
    position: [f32; 2],
    radius: f32,
    health: Option<i32>,
    /// Raw BlowUp value is preserved for a future destruction/effect system.
    blow_up: i32,
    blastable: bool,
    difficulty_enabled: bool,
    enabled: bool,
    destroyed: bool,
}

/// Distance is in map tiles along the normalized query direction.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ObjectRayHit {
    pub cell: [usize; 2],
    pub distance: f32,
}

/// `destroyed` is true only for the hit that first reduced health to zero.
/// A non-destructible object's health is None; it can still trigger shoot links.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObjectHitEvent {
    pub cell: [usize; 2],
    pub damage: i32,
    pub remaining_health: Option<i32>,
    pub destroyed: bool,
    pub blow_up: i32,
}

#[derive(Debug, Clone, Default)]
pub struct ObjectCombat {
    targets: Vec<Target>,
}

impl ObjectCombat {
    /// Retain level objects; only positive break limit or shoot/destroy links
    /// make a non-dummy object shootable.
    /// Map item pickups and unrelated decorative objects are not yet shot
    /// blockers. Difficulty filtering follows the single-player pickup system.
    pub fn new(
        instances: &[ObjectInstance],
        definitions: &ObjectDefinitions,
        script: &Script,
        difficulty: Difficulty,
    ) -> Self {
        let mask = match difficulty {
            Difficulty::Easy => 1,
            Difficulty::Normal => 2,
            Difficulty::Hard => 4,
        };
        let linked: BTreeSet<[usize; 2]> = script
            .triggers
            .iter()
            .filter(|trigger| matches!(trigger.kind.as_str(), "shoot" | "destroy"))
            .map(|trigger| [usize::from(trigger.x), usize::from(trigger.y)])
            .collect();
        let mut targets = Vec::new();
        for instance in instances {
            if instance.kind != ObjectKind::LevelObject {
                continue;
            }
            let Some(def) = definitions.definition(instance) else {
                continue;
            };
            let Some(raw_type) = u8::try_from(instance.definition_index)
                .ok()
                .and_then(|index| index.checked_add(163))
                .filter(|raw| *raw <= 226)
            else {
                continue;
            };
            let linked = linked.contains(&instance.grid_cell);
            let shootable = is_shootable(def, linked);
            let radius = target_radius(def);
            targets.push(Target {
                cell: instance.grid_cell,
                raw_type,
                linked,
                shootable,
                position: instance.position,
                radius,
                health: (def.break_limit > 0).then_some(def.break_limit),
                blow_up: def.blow_up,
                blastable: def.collision_radius > 0 && def.blow_up != 0 && def.break_limit > 0,
                difficulty_enabled: instance.difficulty_flags & mask != 0,
                enabled: true,
                destroyed: false,
            });
        }
        Self { targets }
    }

    /// Synchronize a committed script transition. This validates the prior and
    /// destination types and the authored destination health before mutation;
    /// invalid or stale transitions return false and leave combat unchanged.
    /// Position, difficulty, enabled state and coordinate-based links survive.
    pub fn apply_transition(
        &mut self,
        transition: &ObjectTransition,
        definitions: &ObjectDefinitions,
    ) -> bool {
        let Some(index) = transition
            .new_type
            .checked_sub(163)
            .map(usize::from)
            .filter(|&index| index < 64)
        else {
            return false;
        };
        let Some(def) = definitions.level_objects.get(index) else {
            return false;
        };
        if transition.new_health != def.break_limit.max(0)
            || transition.previous_health < 0
            || !transition.reset_animation
            || (transition.kind == TransitionKind::Explode
                && transition.previous_type.checked_add(1) != Some(transition.new_type))
        {
            return false;
        }
        let Some(target) = self.targets.iter_mut().find(|target| {
            target.cell == transition.cell && target.raw_type == transition.previous_type
        }) else {
            return false;
        };
        target.raw_type = transition.new_type;
        target.shootable = is_shootable(def, target.linked);
        target.radius = target_radius(def);
        target.health = (def.break_limit > 0).then_some(transition.new_health);
        target.blow_up = def.blow_up;
        target.blastable = def.collision_radius > 0 && def.blow_up != 0 && def.break_limit > 0;
        target.destroyed = false;
        true
    }

    /// Apply a complete ordered batch atomically. Clone this combat state and
    /// preflight before committing the corresponding authoritative state.
    pub fn apply_transitions(
        &mut self,
        transitions: &[ObjectTransition],
        definitions: &ObjectDefinitions,
    ) -> bool {
        let mut staged = self.clone();
        if !transitions
            .iter()
            .all(|transition| staged.apply_transition(transition, definitions))
        {
            return false;
        }
        *self = staged;
        true
    }

    /// Find the nearest eligible object hit without applying damage. Direction
    /// need not be unit length. `radius` is the projectile radius in tiles;
    /// use zero for an instant ray. No wall, actor, or vertical test is implied.
    pub fn raycast(
        &self,
        origin: [f32; 2],
        direction: [f32; 2],
        max_distance: f32,
        radius: f32,
    ) -> Option<ObjectRayHit> {
        self.raycast_filtered(origin, direction, max_distance, radius, |_, _| true)
    }

    /// Filter each candidate before selecting the nearest. The predicate sees
    /// the object's cell and current raw type, including script replacements.
    pub fn raycast_filtered(
        &self,
        origin: [f32; 2],
        direction: [f32; 2],
        max_distance: f32,
        radius: f32,
        accepts: impl Fn([usize; 2], u8) -> bool,
    ) -> Option<ObjectRayHit> {
        if !origin.iter().chain(direction.iter()).all(|x| x.is_finite())
            || !max_distance.is_finite()
            || max_distance < 0.0
            || !radius.is_finite()
            || radius < 0.0
        {
            return None;
        }
        let magnitude = direction[0].hypot(direction[1]);
        if !magnitude.is_finite() || magnitude < 1e-6 {
            return None;
        }
        let ray = [direction[0] / magnitude, direction[1] / magnitude];
        let mut nearest = None;
        for target in &self.targets {
            if !target.shootable
                || target.radius <= 0.0
                || !target.difficulty_enabled
                || !target.enabled
                || target.destroyed
                || !accepts(target.cell, target.raw_type)
                || !target.position.iter().all(|v| v.is_finite())
            {
                continue;
            }
            let delta = [
                target.position[0] - origin[0],
                target.position[1] - origin[1],
            ];
            let along = delta[0] * ray[0] + delta[1] * ray[1];
            let perpendicular_sq =
                (delta[0] * delta[0] + delta[1] * delta[1] - along * along).max(0.0);
            let sum_radius = target.radius + radius.min(MAX_RADIUS);
            if perpendicular_sq > sum_radius * sum_radius {
                continue;
            }
            let half = (sum_radius * sum_radius - perpendicular_sq).sqrt();
            if along + half < 0.0 {
                continue;
            }
            let distance = (along - half).max(0.0);
            if distance > max_distance {
                continue;
            }
            if nearest
                .as_ref()
                .is_none_or(|hit: &ObjectRayHit| distance < hit.distance)
            {
                nearest = Some(ObjectRayHit {
                    cell: target.cell,
                    distance,
                });
            }
        }
        nearest
    }

    /// Apply positive damage to a previously resolved cell. The caller should
    /// always dispatch its `shoot` links, and dispatch `destroy` links only when
    /// `destroyed` is true. Non-destructibles still report hits.
    pub fn apply_hit(&mut self, cell: [usize; 2], damage: i32) -> Option<ObjectHitEvent> {
        if damage <= 0 {
            return None;
        }
        let target = self.targets.iter_mut().find(|target| {
            target.cell == cell
                && target.shootable
                && target.difficulty_enabled
                && target.enabled
                && !target.destroyed
        })?;
        if let Some(health) = &mut target.health {
            *health = health.saturating_sub(damage).max(0);
            if *health == 0 {
                target.destroyed = true;
            }
        }
        Some(ObjectHitEvent {
            cell,
            damage,
            remaining_health: target.health,
            destroyed: target.destroyed,
            blow_up: target.blow_up,
        })
    }

    /// Active destructible blast targets; height is supplied by the host.
    pub fn explosion_targets(&self) -> impl Iterator<Item = ([usize; 2], [f32; 2], f32)> + '_ {
        self.explosion_targets_with_types()
            .map(|(cell, position, radius, _)| (cell, position, radius))
    }

    /// Active blast targets including their live replacement raw type.
    pub fn explosion_targets_with_types(
        &self,
    ) -> impl Iterator<Item = ([usize; 2], [f32; 2], f32, u8)> + '_ {
        self.targets
            .iter()
            .filter(|target| {
                target.blastable
                    && target.shootable
                    && target.difficulty_enabled
                    && target.enabled
                    && !target.destroyed
            })
            .map(|target| (target.cell, target.position, target.radius, target.raw_type))
    }

    /// Return whether a tracked object can currently be hit. Untracked cells
    /// return None; they may still contain walls or unrelated decorations.
    pub fn is_active(&self, cell: [usize; 2]) -> Option<bool> {
        self.targets
            .iter()
            .find(|target| target.cell == cell && target.shootable)
            .map(|target| target.difficulty_enabled && target.enabled && !target.destroyed)
    }

    /// Visibility of each tracked object, for presentation synchronization.
    pub fn visibility(&self) -> impl Iterator<Item = ([usize; 2], bool)> + '_ {
        self.targets
            .iter()
            .filter(|target| target.shootable)
            .map(|target| {
                (
                    target.cell,
                    target.difficulty_enabled && target.enabled && !target.destroyed,
                )
            })
    }

    /// Toggle visibility/collision for script-controlled objects. This does not
    /// resurrect a destroyed object; use `apply_transition` for replacements.
    pub fn set_enabled(&mut self, cell: [usize; 2], enabled: bool) -> bool {
        if let Some(target) = self.targets.iter_mut().find(|target| target.cell == cell) {
            target.enabled = enabled;
            true
        } else {
            false
        }
    }

    pub fn set_position(&mut self, cell: [usize; 2], position: [f32; 2]) -> bool {
        if !position.iter().all(|v| v.is_finite()) {
            return false;
        }
        if let Some(target) = self.targets.iter_mut().find(|target| target.cell == cell) {
            target.position = position;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::objects::ObjectDefinition;

    fn definition(radius: i32, health: i32) -> ObjectDefinition {
        ObjectDefinition {
            collision_radius: radius,
            shadow: 0,
            bmp_object: -1,
            bmp_z: 0,
            action_code: 0,
            blow_up: 3,
            break_limit: health,
            ambient_sound: 0,
            break_sound: 0,
            model_name: "box.3o".into(),
            animation_name: None,
        }
    }
    fn instance(index: usize, cell: [usize; 2], position: [f32; 2], flags: u16) -> ObjectInstance {
        ObjectInstance {
            kind: ObjectKind::LevelObject,
            definition_index: index,
            grid_cell: cell,
            position,
            angle_radians: 0.0,
            flags: 0,
            difficulty_flags: flags,
            extra: 0,
            size: 0,
        }
    }
    #[test]
    fn shoot_link_tracks_immortal_object_and_damage_events_are_repeatable() {
        let definitions = ObjectDefinitions {
            items: vec![],
            level_objects: vec![definition(32, 0)],
        };
        let objects = [instance(0, [2, 3], [2.0, 0.0], 7)];
        let script = Script::parse("#links\nshoot 2 3 9\n#end\n");
        let mut combat = ObjectCombat::new(&objects, &definitions, &script, Difficulty::Normal);
        assert_eq!(
            combat
                .raycast([0.0, 0.0], [2.0, 0.0], 3.0, 0.0)
                .unwrap()
                .cell,
            [2, 3]
        );
        let hit = combat.apply_hit([2, 3], 5).unwrap();
        assert_eq!(hit.remaining_health, None);
        assert!(!hit.destroyed);
        assert!(!combat.apply_hit([2, 3], 100).unwrap().destroyed);
    }
    #[test]
    fn destructible_hit_reports_one_destroy_transition_and_unblocks_ray() {
        let definitions = ObjectDefinitions {
            items: vec![],
            level_objects: vec![definition(32, 10)],
        };
        let objects = [
            instance(0, [1, 1], [1.0, 0.0], 7),
            instance(0, [2, 2], [2.0, 0.0], 7),
        ];
        let mut combat = ObjectCombat::new(
            &objects,
            &definitions,
            &Script::default(),
            Difficulty::Normal,
        );
        let first = combat.raycast([0.0, 0.0], [1.0, 0.0], 3.0, 0.0).unwrap();
        assert_eq!(first.cell, [1, 1]);
        assert!((first.distance - 0.875).abs() < 1e-6);
        assert_eq!(
            combat.apply_hit(first.cell, 4).unwrap().remaining_health,
            Some(6)
        );
        assert!(combat.apply_hit(first.cell, 6).unwrap().destroyed);
        assert!(combat.apply_hit(first.cell, 1).is_none());
        assert_eq!(
            combat
                .raycast([0.0, 0.0], [1.0, 0.0], 3.0, 0.0)
                .unwrap()
                .cell,
            [2, 2]
        );
    }
    #[test]
    fn respects_difficulty_moved_targets_and_finite_input() {
        let definitions = ObjectDefinitions {
            items: vec![],
            level_objects: vec![definition(32, 10)],
        };
        let objects = [
            instance(0, [1, 1], [1.0, 0.0], 1),
            instance(0, [2, 2], [2.0, 0.0], 7),
        ];
        let mut combat = ObjectCombat::new(
            &objects,
            &definitions,
            &Script::default(),
            Difficulty::Normal,
        );
        assert_eq!(combat.raycast([0.0, 0.0], [1.0, 0.0], 1.5, 0.0), None);
        assert!(combat.set_enabled([1, 1], true));
        assert_eq!(combat.raycast([0.0, 0.0], [1.0, 0.0], 1.5, 0.0), None);
        assert!(combat.set_position([2, 2], [1.0, 0.2]));
        assert_eq!(combat.raycast([0.0, 0.0], [1.0, 0.0], 2.0, 0.0), None);
        assert_eq!(
            combat
                .raycast([0.0, 0.0], [1.0, 0.0], 2.0, 0.1)
                .unwrap()
                .cell,
            [2, 2]
        );
        assert!(combat.set_enabled([2, 2], false));
        assert!(combat.raycast([0.0, 0.0], [1.0, 0.0], 2.0, 0.1).is_none());
        assert!(combat
            .raycast([0.0, 0.0], [f32::NAN, 0.0], 2.0, 0.0)
            .is_none());
        assert!(!combat.set_position([2, 2], [f32::INFINITY, 0.0]));
    }
    #[test]
    fn zero_radius_linked_target_is_not_a_ray_hit_until_replaced() {
        use crate::object_states::{ObjectAction, ObjectStates};
        let defs = ObjectDefinitions {
            items: vec![],
            level_objects: vec![definition(0, 400), definition(64, 600)],
        };
        let objects = [instance(0, [1, 1], [1.0, 0.0], 7)];
        let script = Script::parse("#links\ndestroy 1 1 8\n#end\n");
        let mut states = ObjectStates::new(&objects, &defs).unwrap();
        let mut combat = ObjectCombat::new(&objects, &defs, &script, Difficulty::Normal);
        assert_eq!(combat.raycast([0.0, 0.0], [1.0, 0.0], 2.0, 0.2), None);
        let transitions = states
            .apply_batch(&[ObjectAction::Change {
                cell: [1, 1],
                raw_type: 164,
            }])
            .unwrap();
        assert!(combat.apply_transitions(&transitions, &defs));
        assert_eq!(
            combat
                .raycast([0.0, 0.0], [1.0, 0.0], 2.0, 0.0)
                .unwrap()
                .cell,
            [1, 1]
        );
        assert!(combat.apply_hit([1, 1], 600).unwrap().destroyed);
    }
    #[test]
    fn skips_unlinked_decorations() {
        let definitions = ObjectDefinitions {
            items: vec![],
            level_objects: vec![definition(0, 0)],
        };
        let objects = [instance(0, [0, 0], [1.0, 0.0], 7)];
        let combat = ObjectCombat::new(
            &objects,
            &definitions,
            &Script::default(),
            Difficulty::Normal,
        );
        assert!(combat.raycast([0.0, 0.0], [1.0, 0.0], 2.0, 0.0).is_none());
    }
    #[test]
    fn transitions_update_targets_and_preserve_motion_difficulty_and_links() {
        use crate::object_states::{ObjectAction, ObjectStates};
        let mut dummy = definition(64, 0);
        dummy.model_name = "dummy.3o".into();
        let mut tough = definition(64, 12);
        tough.blow_up = 7;
        let defs = ObjectDefinitions {
            items: vec![],
            level_objects: vec![definition(0, 0), tough, dummy],
        };
        let objects = [
            instance(0, [1, 1], [1.0, 0.0], 7),
            instance(0, [2, 2], [2.0, 0.0], 1),
        ];
        let script = Script::parse("#links\nshoot 1 1 9\n#end\n");
        let mut states = ObjectStates::new(&objects, &defs).unwrap();
        let mut combat = ObjectCombat::new(&objects, &defs, &script, Difficulty::Normal);
        assert!(combat.set_position([1, 1], [3.0, 0.0]));
        let transitions = states
            .apply_batch(&[
                ObjectAction::Change {
                    cell: [1, 1],
                    raw_type: 164,
                },
                ObjectAction::Change {
                    cell: [2, 2],
                    raw_type: 164,
                },
            ])
            .unwrap();
        assert!(combat.apply_transitions(&transitions, &defs));
        assert_eq!(
            combat
                .raycast([0.0, 0.0], [1.0, 0.0], 5.0, 0.0)
                .unwrap()
                .cell,
            [1, 1]
        );
        assert_eq!(combat.is_active([2, 2]), Some(false));
        let hit = combat.apply_hit([1, 1], 12).unwrap();
        assert!(hit.destroyed);
        assert_eq!(hit.blow_up, 7);
        let transitions = states
            .apply_batch(&[ObjectAction::Explode { cell: [1, 1] }])
            .unwrap();
        assert!(combat.apply_transitions(&transitions, &defs));
        assert!(combat.raycast([0.0, 0.0], [1.0, 0.0], 5.0, 0.0).is_none());
        // Returning to an immortal linked object restores its shoot link.
        let transitions = states
            .apply_batch(&[ObjectAction::Change {
                cell: [1, 1],
                raw_type: 163,
            }])
            .unwrap();
        assert!(combat.apply_transitions(&transitions, &defs));
        assert_eq!(combat.apply_hit([1, 1], 1).unwrap().remaining_health, None);
    }

    #[test]
    fn transitions_can_make_a_decoration_shootable_and_reject_invalid_batches_atomically() {
        use crate::object_states::{ObjectAction, ObjectStates};
        let defs = ObjectDefinitions {
            items: vec![],
            level_objects: vec![definition(0, 0), definition(32, 10)],
        };
        let objects = [instance(0, [1, 1], [1.0, 0.0], 7)];
        let mut states = ObjectStates::new(&objects, &defs).unwrap();
        let mut combat = ObjectCombat::new(&objects, &defs, &Script::default(), Difficulty::Normal);
        assert_eq!(combat.is_active([1, 1]), None);
        let transitions = states
            .apply_batch(&[ObjectAction::Change {
                cell: [1, 1],
                raw_type: 164,
            }])
            .unwrap();
        let mut bad = transitions[0].clone();
        bad.new_type = 255;
        assert!(!combat.apply_transitions(&[transitions[0].clone(), bad], &defs));
        assert_eq!(combat.is_active([1, 1]), None);
        assert!(combat.apply_transitions(&transitions, &defs));
        assert_eq!(
            combat.apply_hit([1, 1], 1).unwrap().remaining_health,
            Some(9)
        );
        assert!(!combat.apply_transitions(&transitions, &defs)); // stale
        assert_eq!(
            combat.apply_hit([1, 1], 1).unwrap().remaining_health,
            Some(8)
        );
    }
}
