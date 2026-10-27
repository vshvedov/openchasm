//! Shared inventory and pickup state for a running level.
use crate::inventory::{Inventory, WeaponDefinitions};
use crate::objects::{ObjectDefinitions, ObjectInstance};
use crate::pickups::{Difficulty, PickupEvent, Pickups};

pub struct GameInventory {
    pub inventory: Inventory,
    pub pickups: Pickups,
    pub weapons: WeaponDefinitions,
}
impl GameInventory {
    pub fn new(
        weapons: WeaponDefinitions,
        instances: &[ObjectInstance],
        definitions: &ObjectDefinitions,
        difficulty: Difficulty,
    ) -> Self {
        Self {
            inventory: Inventory::new(&weapons),
            pickups: Pickups::new(instances, definitions, difficulty),
            weapons,
        }
    }
    pub fn weapons(&self) -> &WeaponDefinitions {
        &self.weapons
    }

    /// Update inventory on overlap and return presentation/script events.
    pub fn collect_near(&mut self, position: [f32; 2]) -> Vec<PickupEvent> {
        self.pickups
            .collect_near(position, &mut self.inventory, &self.weapons)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inventory::WeaponDefinition;
    use crate::objects::{ObjectDefinition, ObjectKind};
    use crate::process::Script;
    use crate::process_runtime::ProcessRuntime;
    use crate::world::{Player, Wall, World};

    #[test]
    fn collected_key_unlocks_a_linked_door() {
        let definition = ObjectDefinition {
            collision_radius: 0,
            shadow: 0,
            bmp_object: 0,
            bmp_z: 0,
            action_code: 142,
            blow_up: 0,
            break_limit: 0,
            ambient_sound: 0,
            break_sound: 0,
            model_name: "key.3o".into(),
            animation_name: None,
        };
        let definitions = ObjectDefinitions {
            items: vec![definition],
            level_objects: vec![],
        };
        let instance = ObjectInstance {
            kind: ObjectKind::Item,
            definition_index: 0,
            grid_cell: [0, 0],
            position: [0.5, 0.5],
            angle_radians: 0.0,
            flags: 0,
            difficulty_flags: 2,
            extra: 0,
            size: 0,
        };
        let weapons = WeaponDefinitions(
            [WeaponDefinition {
                rocket_type: 0,
                reload_time: 0,
                vertical_shift: 0,
                projectile_height: 0,
                ammo_per_portion: 1,
                ammo_limit: 10,
                ammo_on_weapon_pickup: 1,
                projectile_count: 1,
            }; 8],
        );
        let mut game = GameInventory::new(weapons, &[instance], &definitions, Difficulty::Normal);
        let mut world = World {
            player: Player {
                position: [0.5, 0.5],
                angle: 0.0,
            },
            walls: vec![Wall {
                a: [1.0, 0.0],
                b: [1.0, 1.0],
                texture: 1,
                blocks_movement: true,
                cell: Some([1, 0]),
            }],
        };
        let script = Script::parse(
            "#proc1\nGreenKey\n#action\nmove 1 0 256 0\n#end\n#links\nlink 1 0 1\n#end\n",
        );
        let mut processes = ProcessRuntime::new(&script, &world);
        assert_eq!(game.collect_near(world.player.position).len(), 1);
        assert!(game.inventory.keys[1]);
        processes.set_keys(game.inventory.keys);
        processes.tick(&mut world, 1.0 / 60.0, true);
        for _ in 0..60 {
            processes.tick(&mut world, 1.0 / 60.0, false);
        }
        assert!((world.walls[0].a[0] - 2.0).abs() < 0.001);
        assert_eq!(game.pickups.is_visible([0, 0]), Some(false));
    }
}
