//! Fixed-tick game session: simulation state independent of presentation and transport.
use crate::actors::{ActorDefinitions, ActorState};
use crate::ai::{EnemyAi, EnemyAttack, EnemyAttackKind, Mode};
use crate::combat::{Combat, FireResult, RocketDefinitions, ShotContext, ShotImpact};
use crate::game::GameInventory;
use crate::pickups::{Difficulty, PickupEvent};
use crate::process_runtime::ProcessRuntime;
use crate::world::{Input, World};

pub struct LevelSetup {
    pub world: World,
    pub processes: ProcessRuntime,
    pub actors: ActorState,
    pub actor_definitions: ActorDefinitions,
    pub game: GameInventory,
    pub rockets: RocketDefinitions,
    /// Indexed by wall texture ID; false means shots pass through.
    pub shot_blocking: [bool; 128],
    pub difficulty: Difficulty,
}

#[derive(Default)]
pub struct SessionInput {
    pub movement: Input,
    pub interact: bool,
    pub fire: bool,
    pub select_weapon: Option<usize>,
}
#[derive(Default)]
pub struct SessionEvents {
    pub pickups: Vec<PickupEvent>,
    pub shot: Option<FireResult>,
    pub damage: u16,
    pub enemy_attacks: Vec<EnemyAttack>,
}

#[derive(Clone)]
pub struct Session {
    pub world: World,
    pub processes: ProcessRuntime,
    pub actors: ActorState,
    pub actor_definitions: ActorDefinitions,
    pub game: GameInventory,
    pub ai: EnemyAi,
    pub selected_weapon: usize,
    combat: Combat,
    rockets: RocketDefinitions,
    shot_blocking: [bool; 128],
}
impl Session {
    pub fn new(mut setup: LevelSetup) -> Self {
        let ai = EnemyAi::new(&setup.actors, setup.difficulty);
        // Disabled difficulty placements must not be hittable by combat.
        for actor in &mut setup.actors.actors {
            if ai.mode(actor.source_index) == Some(Mode::Inactive) {
                actor.health = 0;
            }
        }
        Self {
            world: setup.world,
            processes: setup.processes,
            actors: setup.actors,
            actor_definitions: setup.actor_definitions,
            game: setup.game,
            ai,
            selected_weapon: 0,
            combat: Combat::new(),
            rockets: setup.rockets,
            shot_blocking: setup.shot_blocking,
        }
    }
    /// Advance exactly one 60 Hz simulation tick, returning presentation events.
    pub fn step(&mut self, input: SessionInput) -> SessionEvents {
        const DT: f32 = 1.0 / 60.0;
        let mut events = SessionEvents::default();
        if self.game.inventory.health == 0 {
            return events;
        }
        if let Some(index) = input.select_weapon.filter(|&index| {
            index < self.game.inventory.weapons.len() && self.game.inventory.weapons[index]
        }) {
            self.selected_weapon = index;
        }
        self.world.step(input.movement, DT);
        events.pickups = self.game.collect_near(self.world.player.position);
        self.processes.set_keys(self.game.inventory.keys);
        self.processes.tick(&mut self.world, DT, input.interact);
        self.combat.tick(DT);
        if input.fire {
            let shot = self.combat.fire(
                self.selected_weapon,
                ShotContext {
                    inventory: &mut self.game.inventory,
                    weapons: &self.game.weapons,
                    rockets: &self.rockets,
                    actors: &mut self.actors,
                    definitions: &self.actor_definitions,
                    world: &self.world,
                },
                |wall| {
                    self.shot_blocking
                        .get(wall.texture as usize)
                        .copied()
                        .unwrap_or(true)
                },
            );
            if let FireResult::Fired(ShotImpact::Actor {
                source_index,
                killed: false,
                ..
            }) = shot
            {
                self.ai.alert(source_index, self.world.player.position);
            }
            events.shot = Some(shot);
        }
        events.enemy_attacks = self.ai.tick_with_rockets(
            &mut self.actors,
            &self.actor_definitions,
            &self.rockets,
            &self.world,
        );
        for attack in &events.enemy_attacks {
            if matches!(attack.kind, EnemyAttackKind::Ranged { instant: false, .. }) {
                continue;
            }
            // Provisional armor: absorbs at most half of each incoming hit.
            let absorbed = (attack.damage / 2).min(self.game.inventory.armor);
            self.game.inventory.armor -= absorbed;
            let damage = attack.damage - absorbed;
            self.game.inventory.health = self.game.inventory.health.saturating_sub(damage);
            events.damage = events.damage.saturating_add(damage);
        }
        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actors::{Actor, ActorDefinition};
    use crate::combat::RocketDefinition;
    use crate::inventory::{WeaponDefinition, WeaponDefinitions};
    use crate::map::Monster;
    use crate::objects::ObjectDefinitions;
    use crate::process::Script;
    use crate::world::Player;

    #[test]
    fn session_fires_damages_enemies_and_consumes_ammunition() {
        let world = World {
            walls: vec![],
            player: Player {
                position: [0.0, 0.0],
                angle: 0.0,
            },
        };
        let process = ProcessRuntime::new(&Script::default(), &world);
        let placement = Monster {
            position: [512, 0],
            id: 101,
            angle: 0,
            flags: 2,
        };
        let actors = ActorState {
            player_spawns: vec![],
            actors: vec![Actor {
                source_index: 0,
                definition_index: 1,
                placement,
                position: [2.0, 0.0],
                angle_radians: 0.0,
                health: 30,
            }],
            unresolved: vec![],
        };
        let definition = ActorDefinition {
            model_name: "test.car".into(),
            wall_radius: 64,
            actor_radius: 64,
            speed: 0,
            rotation_speed: 0,
            health: 30,
            kick: 0,
            rocket: -1,
            separation_limit: 0,
            extra_fields: vec![],
            raw_line: "".into(),
        };
        let actor_definitions = ActorDefinitions {
            entries: vec![definition.clone(), definition],
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
            &ObjectDefinitions {
                items: vec![],
                level_objects: vec![],
            },
            Difficulty::Normal,
        );
        let rockets = RocketDefinitions(vec![RocketDefinition {
            model_name: "".into(),
            power: 10,
            raw_fields: vec![],
        }]);
        let mut session = Session::new(LevelSetup {
            world,
            processes: process,
            actors,
            actor_definitions,
            game,
            rockets,
            shot_blocking: [true; 128],
            difficulty: Difficulty::Normal,
        });
        let initial = session.clone();
        let events = session.step(SessionInput {
            fire: true,
            ..SessionInput::default()
        });
        assert!(matches!(
            events.shot,
            Some(FireResult::Fired(ShotImpact::Actor { damage: 10, .. }))
        ));
        assert_eq!(session.actors.actors[0].health, 20);
        assert_eq!(session.game.inventory.ammo[0], 9);
        assert_eq!(initial.game.inventory.ammo[0], 10);
        assert_eq!(initial.actors.actors[0].health, 30);
        session.actor_definitions.entries[1].rocket = 0;
        let events = session.step(SessionInput::default());
        assert!(matches!(
            events.enemy_attacks.first().map(|attack| attack.kind),
            Some(EnemyAttackKind::Ranged { instant: true, .. })
        ));
        assert_eq!(session.game.inventory.health, 90);
    }
}
