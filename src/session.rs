//! Fixed-tick game session: simulation state independent of presentation and transport.
use crate::actors::{ActorDefinitions, ActorState};
use crate::ai::{EnemyAi, EnemyAttack, EnemyAttackKind, Mode};
use crate::combat::{Combat, FireResult, RocketDefinitions, ShotContext, ShotImpact};
use crate::game::GameInventory;
use crate::mission::{MissionEvent, MissionPlan, MissionState};
use crate::object_combat::ObjectCombat;
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
    /// Horizontal look displacement in radians for this tick, independent of frame rate.
    pub yaw_delta: f32,
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
    /// Object cell and whether it was destroyed by this hit.
    pub object_hits: Vec<([usize; 2], bool)>,
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
    pub objects: Option<ObjectCombat>,
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
            objects: None,
            mission: None,
            combat: Combat::new(),
            rockets: setup.rockets,
            shot_blocking: setup.shot_blocking,
        }
    }
    /// Install shootable object state before taking the restart snapshot.
    pub fn set_objects(&mut self, objects: ObjectCombat) {
        self.objects = Some(objects);
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
        // Process look as simulation input so a future server can own the view
        // direction. Discard non-finite values and bound a single tick's turn.
        if input.yaw_delta.is_finite() && self.world.player.angle.is_finite() {
            let delta = input
                .yaw_delta
                .clamp(-std::f32::consts::PI, std::f32::consts::PI);
            self.world.player.angle =
                (self.world.player.angle + delta).rem_euclid(std::f32::consts::TAU);
        }
        self.world.step(input.movement, DT);
        events.pickups = self.game.collect_near(self.world.player.position);
        self.processes.set_keys(self.game.inventory.keys);
        let mut attempts = Vec::new();
        for pickup in &events.pickups {
            if let (Ok(x), Ok(y)) = (u8::try_from(pickup.cell[0]), u8::try_from(pickup.cell[1])) {
                attempts.extend(self.processes.activate_links_at([x, y]));
            }
        }
        attempts.extend(self.processes.tick(&mut self.world, DT, input.interact));
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
                    objects: self.objects.as_mut(),
                },
                |wall| {
                    self.shot_blocking
                        .get(wall.texture as usize)
                        .copied()
                        .unwrap_or(true)
                },
            );
            match shot {
                FireResult::Fired(ShotImpact::Object {
                    cell, destroyed, ..
                }) => {
                    if let (Ok(x), Ok(y)) = (u8::try_from(cell[0]), u8::try_from(cell[1])) {
                        attempts.extend(self.processes.activate_at("shoot", [x, y]));
                        if destroyed {
                            attempts.extend(self.processes.activate_at("destroy", [x, y]));
                        }
                    }
                    events.object_hits.push((cell, destroyed));
                }
                FireResult::Fired(ShotImpact::Wall { wall_index, .. }) => {
                    if let Some(cell) = self.world.walls.get(wall_index).and_then(|wall| wall.cell)
                    {
                        attempts.extend(self.processes.activate_at("shoot", cell));
                    }
                }
                _ => {}
            }
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
        events.projectile_impacts = self.projectiles.tick_with_objects(
            DT,
            &self.world,
            &self.actors,
            &self.actor_definitions,
            self.objects.as_ref(),
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
                ImpactTarget::Object { cell } => {
                    if let Some(hit) = self
                        .objects
                        .as_mut()
                        .and_then(|objects| objects.apply_hit(cell, impact.damage))
                    {
                        events.object_hits.push((cell, hit.destroyed));
                        if impact.owner == Owner::Player {
                            if let (Ok(x), Ok(y)) = (u8::try_from(cell[0]), u8::try_from(cell[1])) {
                                attempts.extend(self.processes.activate_at("shoot", [x, y]));
                                if hit.destroyed {
                                    attempts.extend(self.processes.activate_at("destroy", [x, y]));
                                }
                            }
                        }
                    }
                }
                ImpactTarget::Wall { wall_index } if impact.owner == Owner::Player => {
                    if let Some(cell) = self.world.walls.get(wall_index).and_then(|wall| wall.cell)
                    {
                        attempts.extend(self.processes.activate_at("shoot", cell));
                    }
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
    fn look_input_is_finite_bounded_and_applied_before_movement() {
        let mut session = mission_session("");
        session.step(SessionInput {
            yaw_delta: std::f32::consts::FRAC_PI_2,
            movement: Input {
                forward: 1.0,
                ..Input::default()
            },
            ..Default::default()
        });
        assert!((session.world.player.angle - std::f32::consts::FRAC_PI_2).abs() < 1e-6);
        assert!(session.world.player.position[1] > 0.5);
        let angle = session.world.player.angle;
        session.step(SessionInput {
            yaw_delta: f32::NAN,
            ..Default::default()
        });
        assert_eq!(session.world.player.angle, angle);
        session.step(SessionInput {
            yaw_delta: 100_000.0,
            ..Default::default()
        });
        assert!((session.world.player.angle - (angle + std::f32::consts::PI)).abs() < 1e-6);
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
    fn collecting_a_linked_key_activates_its_process() {
        use crate::objects::{ObjectDefinition, ObjectInstance, ObjectKind};
        let text = "#mess1\nDelay 1\nTextA: 0 0 \"Key acquired\"\n#end\n#proc1\nOnMessage 1\nEndDelay 1\n#end\n#links\nlink 0 0 1\n#end\n";
        let mut session = mission_session(text);
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
        let placement = ObjectInstance {
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
        session.game = GameInventory::new(
            session.game.weapons.clone(),
            &[placement],
            &ObjectDefinitions {
                items: vec![definition],
                level_objects: vec![],
            },
            Difficulty::Normal,
        );
        let events = session.step(SessionInput::default());
        assert_eq!(events.pickups.len(), 1);
        assert!(session.game.inventory.keys[1]);
        assert_eq!(events.mission, vec![MissionEvent::ShowMessage(1)]);
    }
    #[test]
    fn destroying_a_linked_lock_unlocks_its_process_and_ends_level() {
        use crate::objects::{ObjectDefinition, ObjectInstance, ObjectKind};
        let text = "#proc1\nLock\nEndDelay 0.05\n#end\n#links\ndestroy 1 0 1\n#end\n";
        let mut session = mission_session(text);
        session.rockets = RocketDefinitions(vec![RocketDefinition {
            model_name: "".into(),
            power: 10,
            raw_fields: vec![],
        }]);
        let definition = ObjectDefinition {
            collision_radius: 32,
            shadow: 0,
            bmp_object: 0,
            bmp_z: 0,
            action_code: 0,
            blow_up: 0,
            break_limit: 10,
            ambient_sound: 0,
            break_sound: 0,
            model_name: "lock.3o".into(),
            animation_name: None,
        };
        let placement = ObjectInstance {
            kind: ObjectKind::LevelObject,
            definition_index: 0,
            grid_cell: [1, 0],
            position: [1.5, 0.5],
            angle_radians: 0.0,
            flags: 0,
            difficulty_flags: 2,
            extra: 0,
            size: 0,
        };
        session.set_objects(ObjectCombat::new(
            &[placement],
            &ObjectDefinitions {
                items: vec![],
                level_objects: vec![definition],
            },
            &Script::parse(text),
            Difficulty::Normal,
        ));
        let mut projectile_session = session.clone();
        projectile_session.game.inventory.weapons[3] = true;
        projectile_session.game.inventory.ammo[3] = 1;
        projectile_session.game.weapons.0[3].rocket_type = 1;
        projectile_session.rockets.0.push(RocketDefinition {
            model_name: "disk.3o".into(),
            power: 10,
            raw_fields: vec![0, 0, 0, 20, 10],
        });
        let first = projectile_session.step(SessionInput {
            fire: true,
            select_weapon: Some(3),
            ..SessionInput::default()
        });
        assert!(matches!(
            first.shot,
            Some(FireResult::Fired(ShotImpact::Launch { .. }))
        ));
        let mut projectile_destroyed = false;
        let mut projectile_completed = false;
        for _ in 0..30 {
            let event = projectile_session.step(SessionInput::default());
            projectile_destroyed |= event.object_hits.contains(&([1, 0], true));
            projectile_completed |= event.mission.contains(&MissionEvent::LevelComplete);
        }
        assert!(projectile_destroyed && projectile_completed);
        let events = session.step(SessionInput {
            fire: true,
            ..SessionInput::default()
        });
        assert_eq!(events.object_hits, vec![([1, 0], true)]);
        assert!(matches!(
            events.shot,
            Some(FireResult::Fired(ShotImpact::Object {
                destroyed: true,
                ..
            }))
        ));
        let mut completed = false;
        for _ in 0..10 {
            completed |= session
                .step(SessionInput::default())
                .mission
                .contains(&MissionEvent::LevelComplete);
        }
        assert!(completed);
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
