//! Provisional horizontal shot selection and damage for map objects.
//!
//! Only destructible objects or objects explicitly referenced by `shoot` /
//! `destroy` links are indexed. Hit shapes are approximate cylinders, not
//! original 3O geometry. Callers compare the reported distance against walls
//! and actors before applying damage or dispatching process-link events.
use crate::map::UNITS_PER_TILE;
use crate::objects::{ObjectDefinitions, ObjectInstance, ObjectKind};
use crate::pickups::Difficulty;
use crate::process::Script;
use std::collections::BTreeSet;

const DEFAULT_RADIUS: f32 = 0.125;
const MAX_RADIUS: f32 = 2.0;

#[derive(Debug, Clone, Copy, PartialEq)]
struct Target {
    cell: [usize; 2],
    position: [f32; 2],
    radius: f32,
    health: Option<i32>,
    /// Raw BlowUp value is preserved for a future destruction/effect system.
    blow_up: i32,
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
    /// Index level objects with positive break limit or a shoot/destroy link.
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
            if def.break_limit <= 0 && !linked.contains(&instance.grid_cell) {
                continue;
            }
            let radius = if def.collision_radius > 0 {
                (def.collision_radius as f32 / UNITS_PER_TILE).clamp(DEFAULT_RADIUS, MAX_RADIUS)
            } else {
                DEFAULT_RADIUS
            };
            targets.push(Target {
                cell: instance.grid_cell,
                position: instance.position,
                radius,
                health: (def.break_limit > 0).then_some(def.break_limit),
                blow_up: def.blow_up,
                difficulty_enabled: instance.difficulty_flags & mask != 0,
                enabled: true,
                destroyed: false,
            });
        }
        Self { targets }
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
            if !target.difficulty_enabled
                || !target.enabled
                || target.destroyed
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
            target.cell == cell && target.difficulty_enabled && target.enabled && !target.destroyed
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

    /// Return whether a tracked object can currently be hit. Untracked cells
    /// return None; they may still contain walls or unrelated decorations.
    pub fn is_active(&self, cell: [usize; 2]) -> Option<bool> {
        self.targets
            .iter()
            .find(|target| target.cell == cell)
            .map(|target| target.difficulty_enabled && target.enabled && !target.destroyed)
    }

    /// Toggle visibility/collision for script-controlled objects. This does not
    /// resurrect a destroyed object; reconstruction of replacement models is a
    /// future process-system responsibility.
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
}
