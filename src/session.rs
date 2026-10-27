//! Fixed-tick game session: simulation state independent of presentation and transport.
use crate::actors::{ActorDefinitions, ActorState};
use crate::ai::{EnemyAi, EnemyAttack, EnemyAttackKind, Mode};
use crate::combat::{Combat, FireResult, RocketDefinitions, ShotContext, ShotImpact};
use crate::game::GameInventory;
use crate::mission::{MissionEvent, MissionPlan, MissionState};
use crate::pickups::{Difficulty, PickupEvent};
use crate::process_runtime::{ProcessOutcome, ProcessRuntime};
use crate::projectiles::{Impact, ImpactTarget, Launch, Owner, ProjectileSystem};
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
    pub projectile_impacts: Vec<Impact>,
    /// Mission presentation/completion events produced during this step.
    pub mission: Vec<MissionEvent>,
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
    pub projectiles: ProjectileSystem,
    pub mission: Option<MissionState>,
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
            projectiles: ProjectileSystem::new(),
            mission: None,
            combat: Combat::new(),
            rockets: setup.rockets,
            shot_blocking: setup.shot_blocking,
        }
    }
    /// Attach a freshly parsed mission before taking the session's restart snapshot.
    pub fn set_mission(&mut self, plan: MissionPlan) {
        self.mission = Some(MissionState::new(plan));
    }
    fn apply_player_damage(&mut self, amount: u16) -> u16 {
        // Provisional armor: absorbs at most half of each incoming hit.
        let absorbed = (amount / 2).min(self.game.inventory.armor);
        self.game.inventory.armor -= absorbed;
        let damage = amount - absorbed;
        self.game.inventory.health = self.game.inventory.health.saturating_sub(damage);
        damage
    }
    /// Advance exactly one 60 Hz simulation tick, returning presentation events.
    pub fn step(&mut self, input: SessionInput) -> SessionEvents {
        const DT: f32 = 1.0 / 60.0;
        let mut events = SessionEvents::default();
        if self.game.inventory.health == 0 {
            return events;
        }
        if let Some(mission) = &mut self.mission {
            events.mission.extend(mission.advance(DT));
            if mission.is_complete() {
                return events;
            }
        }
        if let Some(index) = input.select_weapon.filter(|&index| {
            index < self.game.inventory.weapons.len() && self.game.inventory.weapons[index]
        }) {
            self.selected_weapon = index;
        }
        self.world.step(input.movement, DT);
        events.pickups = self.game.collect_near(self.world.player.position);
        self.processes.set_keys(self.game.inventory.keys);
        let attempts = self.processes.tick(&mut self.world, DT, input.interact);
        if let Some(mission) = &mut self.mission {
            for attempt in attempts {
                match attempt.outcome {
                    ProcessOutcome::Accepted => events
                        .mission
                        .extend(mission.attempt(attempt.process_id, true)),
                    ProcessOutcome::Locked => events
                        .mission
                        .extend(mission.attempt(attempt.process_id, false)),
                    ProcessOutcome::Unsupported
                    | ProcessOutcome::Busy
                    | ProcessOutcome::Missing => {}
                }
            }
        }
        self.combat.tick(DT);
        if input.fire {
            let mut shot = self.combat.fire(
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
            if let FireResult::Fired(ShotImpact::Launch { rocket_id }) = shot {
                let (sin, cos) = self.world.player.angle.sin_cos();
                let launch = Launch {
                    owner: Owner::Player,
                    rocket_id,
                    origin: self.world.player.position,
                    direction: [cos, sin],
                };
                if self.projectiles.spawn(launch, &self.rockets).is_err() {
                    self.game.inventory.ammo[self.selected_weapon] =
                        self.game.inventory.ammo[self.selected_weapon].saturating_add(1);
                    self.combat.cancel_shot();
                    shot = FireResult::Unsupported;
                }
            }
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
        events.enemy_attacks = self.ai.tick_with_collision(
            &mut self.actors,
            &self.actor_definitions,
            &self.rockets,
            &self.world,
            &|wall| {
                self.shot_blocking
                    .get(wall.texture as usize)
                    .copied()
                    .unwrap_or(true)
            },
        );
        for attack in &events.enemy_attacks {
            if let EnemyAttackKind::Ranged {
                rocket_id,
                instant: false,
                origin,
                target,
            } = attack.kind
            {
                let _ = self.projectiles.spawn(
                    Launch {
                        owner: Owner::Enemy {
                            source_index: attack.source_index,
                        },
                        rocket_id,
                        origin,
                        direction: [target[0] - origin[0], target[1] - origin[1]],
                    },
                    &self.rockets,
                );
                continue;
            }
            events.damage = events
                .damage
                .saturating_add(self.apply_player_damage(attack.damage));
        }
        events.projectile_impacts = self.projectiles.tick(
            DT,
            &self.world,
            &self.actors,
            &self.actor_definitions,
            |wall| {
                self.shot_blocking
                    .get(wall.texture as usize)
                    .copied()
                    .unwrap_or(true)
            },
        );
        for impact in &events.projectile_impacts {
            match impact.target {
                ImpactTarget::Player => {
                    events.damage = events.damage.saturating_add(
                        self.apply_player_damage(impact.damage.clamp(0, u16::MAX as i32) as u16),
                    )
                }
                ImpactTarget::Actor { source_index } => {
                    if let Some(actor) = self
                        .actors
                        .actors
                        .iter_mut()
                        .find(|actor| actor.source_index == source_index)
                    {
                        actor.health = actor.health.saturating_sub(impact.damage).max(0);
                        if actor.health > 0 {
                            self.ai.alert(source_index, self.world.player.position);
                        }
                    }
                }
                _ => {}
            }
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

    fn mission_session(text: &str) -> Session {
        let world = World {
            walls: vec![],
            player: Player {
                position: [0.5, 0.5],
                angle: 0.0,
            },
        };
        let script = Script::parse(text);
        let processes = ProcessRuntime::new(&script, &world);
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
        let mut session = Session::new(LevelSetup {
            world,
            processes,
            actors: ActorState {
                player_spawns: vec![],
                actors: vec![],
                unresolved: vec![],
            },
            actor_definitions: ActorDefinitions { entries: vec![] },
            game,
            rockets: RocketDefinitions(vec![]),
            shot_blocking: [true; 128],
            difficulty: Difficulty::Normal,
        });
        session.set_mission(MissionPlan::parse(&script, text));
        session
    }
    #[test]
    fn end_process_emits_message_and_completion_once_and_restart_restores_state() {
        let text = "#mess1\nDelay 3\nTextA: 0 0 \"Done!\"\n#end\n#proc1\nEndDelay 0.05\nOnMessage 1\n#action\nplayani 8\n#end\n#links\nfloor 0 0 1\n#end\n";
        let mut session = mission_session(text);
        let restart = session.clone();
        assert_eq!(
            session.step(SessionInput::default()).mission,
            [MissionEvent::ShowMessage(1)]
        );
        let mut completion_count = 0;
        for _ in 0..10 {
            completion_count += session
                .step(SessionInput::default())
                .mission
                .iter()
                .filter(|event| **event == MissionEvent::LevelComplete)
                .count();
        }
        assert_eq!(completion_count, 1);
        assert!(session.mission.as_ref().unwrap().is_complete());
        let mut restarted = restart;
        assert!(!restarted.mission.as_ref().unwrap().is_complete());
        assert_eq!(
            restarted.step(SessionInput::default()).mission,
            [MissionEvent::ShowMessage(1)]
        );
    }
    #[test]
    fn locked_exit_messages_but_never_completes_and_unsupported_is_not_accepted() {
        let text = "#mess2\nDelay 1\nTextA: 0 0 \"Locked\"\n#end\n#proc1\nLock\nLockMessage 2\nEndDelay 0.01\n#end\n#links\nlink 0 0 1\n#end\n";
        let mut session = mission_session(text);
        assert_eq!(
            session
                .step(SessionInput {
                    interact: true,
                    ..Default::default()
                })
                .mission,
            [MissionEvent::ShowMessage(2)]
        );
        for _ in 0..60 {
            assert!(!session
                .step(SessionInput::default())
                .mission
                .contains(&MissionEvent::LevelComplete));
        }
        assert_eq!(session.mission.as_ref().unwrap().end_remaining(), None);
        let text =
            "#proc1\nEndDelay 0.01\n#action\nchange 0 0 3\n#end\n#links\nfloor 0 0 1\n#end\n";
        let mut session = mission_session(text);
        for _ in 0..60 {
            assert!(!session
                .step(SessionInput::default())
                .mission
                .contains(&MissionEvent::LevelComplete));
        }
        assert_eq!(session.mission.as_ref().unwrap().end_remaining(), None);
    }
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
        let mut projectile_session = initial;
        projectile_session.game.inventory.weapons[3] = true;
        projectile_session.game.inventory.ammo[3] = 2;
        projectile_session.game.weapons.0[3].rocket_type = 1;
        projectile_session.rockets.0.push(RocketDefinition {
            model_name: "disk.3o".into(),
            power: 12,
            raw_fields: vec![0, 0, 0, 20, 12],
        });
        let event = projectile_session.step(SessionInput {
            fire: true,
            select_weapon: Some(3),
            ..SessionInput::default()
        });
        assert!(matches!(
            event.shot,
            Some(FireResult::Fired(ShotImpact::Launch { rocket_id: 1 }))
        ));
        assert_eq!(projectile_session.game.inventory.ammo[3], 1);
        for _ in 0..40 {
            projectile_session.step(SessionInput::default());
        }
        assert_eq!(projectile_session.actors.actors[0].health, 18);
    }
}
