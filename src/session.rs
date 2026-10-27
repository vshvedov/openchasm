//! Fixed-tick game session: simulation state independent of presentation and transport.
use crate::actors::{ActorDefinitions, ActorState};
use crate::ai::{EnemyAi, EnemyAttack, EnemyAttackKind, Mode};
use crate::combat::{Combat, FireResult, RocketDefinitions, ShotContext, ShotImpact};
use crate::game::GameInventory;
use crate::hazards::{HazardDamage, HazardTarget, Hazards};
use crate::mission::{MissionEvent, MissionPlan, MissionState};
use crate::object_combat::ObjectCombat;
use crate::pickups::{Difficulty, PickupEvent};
use crate::process_runtime::{ProcessEffect, ProcessOutcome, ProcessRuntime};
use crate::projectiles::{Impact, ImpactTarget, Launch, Owner, ProjectileSystem};
use crate::vertical::{VerticalBounds, VerticalEvents, VerticalState};
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
    /// One-shot jump press edge; a held button is not automatically repeated.
    pub jump: bool,
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
    pub process_effects: Vec<ProcessEffect>,
    pub hazard_damage: Vec<HazardDamage>,
    pub vertical: VerticalEvents,
}

#[derive(Clone)]
pub struct Session {
    pub world: World,
    pub vertical: VerticalState,
    pub processes: ProcessRuntime,
    pub actors: ActorState,
    pub actor_definitions: ActorDefinitions,
    pub game: GameInventory,
    pub ai: EnemyAi,
    pub selected_weapon: usize,
    pub projectiles: ProjectileSystem,
    pub objects: Option<ObjectCombat>,
    pub mission: Option<MissionState>,
    pub hazards: Hazards,
    /// Absolute offsets in original script units, indexed by wall index.
    pub wall_elevations: Vec<f32>,
    base_wall_blocking: Vec<bool>,
    elevation_by_cell: Vec<f32>,
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
        let wall_elevations = vec![0.0; setup.world.walls.len()];
        let base_wall_blocking = setup
            .world
            .walls
            .iter()
            .map(|wall| wall.blocks_movement)
            .collect();
        Self {
            wall_elevations,
            base_wall_blocking,
            elevation_by_cell: vec![0.0; 64 * 64],
            world: setup.world,
            vertical: VerticalState::new(0.0).expect("default vertical state is valid"),
            processes: setup.processes,
            actors: setup.actors,
            actor_definitions: setup.actor_definitions,
            game: setup.game,
            ai,
            selected_weapon: 0,
            projectiles: ProjectileSystem::new(),
            objects: None,
            mission: None,
            hazards: Hazards::default(),
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
    fn blocks_shot(
        wall: &crate::world::Wall,
        shot_blocking: &[bool; 128],
        elevation_by_cell: &[f32],
        shot_height: f32,
    ) -> bool {
        let elevated = wall.cell.is_some_and(|[x, y]| {
            x < 64
                && y < 64
                && elevation_by_cell
                    .get(usize::from(x) * 64 + usize::from(y))
                    .is_some_and(|offset| *offset >= shot_height * 64.0)
        });
        !elevated
            && shot_blocking
                .get(wall.texture as usize)
                .copied()
                .unwrap_or(true)
    }
    /// Conservative clearance under nearby elevated solid walls. Flat map
    /// floor/ceiling assumptions remain provisional until plane geometry exists.
    fn player_bounds(&self) -> VerticalBounds {
        let p = self.world.player.position;
        let mut bounds = VerticalBounds::flat_room();
        for (i, wall) in self.world.walls.iter().enumerate() {
            if !self.base_wall_blocking.get(i).copied().unwrap_or(false) {
                continue;
            }
            let offset = self.wall_elevations.get(i).copied().unwrap_or(0.0) / 64.0;
            if offset <= self.vertical.feet() || offset >= bounds.ceiling.unwrap_or(f32::INFINITY) {
                continue;
            }
            let d = [wall.b[0] - wall.a[0], wall.b[1] - wall.a[1]];
            let len = d[0] * d[0] + d[1] * d[1];
            let t = if len <= 1e-12 {
                0.0
            } else {
                (((p[0] - wall.a[0]) * d[0] + (p[1] - wall.a[1]) * d[1]) / len).clamp(0.0, 1.0)
            };
            let dx = p[0] - (wall.a[0] + d[0] * t);
            let dy = p[1] - (wall.a[1] + d[1] * t);
            if dx * dx + dy * dy < 0.18 * 0.18 {
                bounds.ceiling = Some(offset);
            }
        }
        bounds
    }
    fn sync_wall_blocking(&mut self) {
        let needed = (self.vertical.head() * 64.0).max(80.0);
        for (i, wall) in self.world.walls.iter_mut().enumerate() {
            wall.blocks_movement = self.base_wall_blocking.get(i).copied().unwrap_or(false)
                && self.wall_elevations.get(i).copied().unwrap_or(0.0) < needed;
        }
    }
    fn apply_process_effects(&mut self) -> Vec<ProcessEffect> {
        let effects = self.processes.take_effects();
        for effect in &effects {
            self.hazards.apply_effect(effect);
            if let ProcessEffect::ObjectPose { cell, position, .. } = *effect {
                let cell = [usize::from(cell[0]), usize::from(cell[1])];
                self.game.pickups.set_position(cell, position);
                if let Some(objects) = &mut self.objects {
                    objects.set_position(cell, position);
                }
            }
            if let ProcessEffect::WallElevation {
                wall_index,
                offset_raw,
            } = *effect
            {
                if let Some(wall) = self.world.walls.get_mut(wall_index) {
                    self.wall_elevations[wall_index] = offset_raw;
                    if let Some([x, y]) = wall.cell.filter(|[x, y]| *x < 64 && *y < 64) {
                        self.elevation_by_cell[usize::from(x) * 64 + usize::from(y)] = offset_raw;
                    }
                }
            }
        }
        self.sync_wall_blocking();
        effects
    }
    fn invalidate_process_object(&mut self, cell: [usize; 2]) {
        if let (Ok(x), Ok(y)) = (u8::try_from(cell[0]), u8::try_from(cell[1])) {
            self.processes.remove_object([x, y]);
        }
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
        let bounds = self.player_bounds();
        // A closing wall can temporarily leave insufficient clearance; retain
        // the last valid vertical state until the player can move clear.
        events.vertical = self
            .vertical
            .step(DT, input.jump, bounds)
            .unwrap_or_default();
        self.sync_wall_blocking();
        // Actors currently use a provisional full-room vertical extent.
        let mut circles: Vec<_> = self
            .actors
            .actors
            .iter()
            .filter_map(|actor| {
                if actor.health <= 0 || self.ai.mode(actor.source_index) == Some(Mode::Inactive) {
                    return None;
                }
                let definition = self.actor_definitions.entries.get(actor.definition_index)?;
                Some(crate::world::CollisionCircle {
                    center: actor.position,
                    radius: (definition.wall_radius as f32 / 256.0).clamp(0.05, 2.0),
                })
            })
            .collect();
        // Only opt-in, live, represented objects have a process-owned pose.
        // Their authored 2D radii are provisional circular approximations;
        // object height is not yet known, so do not allow jumping over them.
        circles.extend(
            self.processes
                .objects()
                .filter(|object| {
                    object.blocking_radius > 0.0
                        && object.elevation_raw / 64.0 < self.vertical.head()
                })
                .map(|object| crate::world::CollisionCircle {
                    center: object.position,
                    radius: object.blocking_radius,
                }),
        );
        self.world.step_with_circles(input.movement, DT, &circles);
        events.pickups = self.game.collect_near(self.world.player.position);
        self.processes.set_keys(self.game.inventory.keys);
        let mut attempts = Vec::new();
        for pickup in &events.pickups {
            self.invalidate_process_object(pickup.cell);
            if let Some(objects) = &mut self.objects {
                objects.set_enabled(pickup.cell, false);
            }
            if let (Ok(x), Ok(y)) = (u8::try_from(pickup.cell[0]), u8::try_from(pickup.cell[1])) {
                attempts.extend(self.processes.activate_links_at([x, y]));
            }
        }
        attempts.extend(self.processes.tick(&mut self.world, DT, input.interact));
        // Process-owned object positions must be applied before this tick's
        // hitscan or projectile queries and before enemy simulation.
        events.process_effects.extend(self.apply_process_effects());
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
                    Self::blocks_shot(
                        wall,
                        &self.shot_blocking,
                        &self.elevation_by_cell,
                        self.vertical.eye(),
                    )
                },
            );
            match shot {
                FireResult::Fired(ShotImpact::Object {
                    cell, destroyed, ..
                }) => {
                    if destroyed {
                        self.invalidate_process_object(cell);
                    }
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
        events.process_effects.extend(self.apply_process_effects());
        events.enemy_attacks = self.ai.tick_with_collision(
            &mut self.actors,
            &self.actor_definitions,
            &self.rockets,
            &self.world,
            &|wall| Self::blocks_shot(wall, &self.shot_blocking, &self.elevation_by_cell, 1.0),
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
            |wall| Self::blocks_shot(wall, &self.shot_blocking, &self.elevation_by_cell, 1.0),
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
                        if hit.destroyed {
                            self.invalidate_process_object(cell);
                        }
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
        events.process_effects.extend(self.apply_process_effects());
        events.hazard_damage = self.hazards.tick(
            self.world.player.position,
            self.vertical.feet(),
            self.vertical.head(),
            &self.actors,
        );
        for hazard in &events.hazard_damage {
            match hazard.target {
                HazardTarget::Player => {
                    events.damage = events.damage.saturating_add(
                        self.apply_player_damage(hazard.damage.min(u16::MAX as u32) as u16),
                    )
                }
                HazardTarget::Actor { source_index } => {
                    if let Some(actor) = self
                        .actors
                        .actors
                        .iter_mut()
                        .find(|actor| actor.source_index == source_index)
                    {
                        actor.health = actor
                            .health
                            .saturating_sub(hazard.damage.min(i32::MAX as u32) as i32)
                            .max(0);
                    }
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
        let mut raw = [0u8; 4096];
        raw[64] = 163;
        session.processes = ProcessRuntime::with_objects_and_hazards(
            &Script::parse(text),
            &session.world,
            crate::process_runtime::ProcessCapabilities::default(),
            &raw,
            &[crate::process_runtime::ProcessObject {
                cell: [1, 0],
                position: [1.5, 0.5],
                angle_radians: 0.0,
                elevation_raw: 0.0,
                blocking_radius: 0.125,
            }],
        )
        .unwrap();
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
        assert!(projectile_session.processes.object_pose([1, 0]).is_none());
        let events = session.step(SessionInput {
            fire: true,
            ..SessionInput::default()
        });
        assert_eq!(events.object_hits, vec![([1, 0], true)]);
        assert!(session.processes.object_pose([1, 0]).is_none());
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
    fn process_object_updates_shootable_and_pickup_positions_then_invalidates_on_collection() {
        use crate::objects::{ObjectDefinition, ObjectInstance, ObjectKind};
        use crate::process_runtime::{ProcessCapabilities, ProcessObject};
        let text =
            "#proc1\n#action\nmove 2 0 256 0\n#end\n#links\nfloor 0 0 1\nshoot 2 0 1\n#end\n";
        let script = Script::parse(text);
        let mut session = mission_session(text);
        let mut raw = [0u8; 4096];
        raw[128] = 163;
        let pose = ProcessObject {
            cell: [2, 0],
            position: [2.5, 0.5],
            angle_radians: 0.0,
            elevation_raw: 0.0,
            blocking_radius: 0.0,
        };
        session.processes = ProcessRuntime::with_objects_and_hazards(
            &script,
            &session.world,
            ProcessCapabilities::default(),
            &raw,
            &[pose],
        )
        .unwrap();
        let definition = ObjectDefinition {
            collision_radius: 0,
            shadow: 0,
            bmp_object: -1,
            bmp_z: 0,
            action_code: 141,
            blow_up: 0,
            break_limit: 0,
            ambient_sound: 0,
            break_sound: 0,
            model_name: "key.3o".into(),
            animation_name: None,
        };
        let instance = ObjectInstance {
            kind: ObjectKind::LevelObject,
            definition_index: 0,
            grid_cell: [2, 0],
            position: [2.5, 0.5],
            angle_radians: 0.0,
            flags: 0,
            difficulty_flags: 2,
            extra: 0,
            size: 0,
        };
        let definitions = ObjectDefinitions {
            items: vec![],
            level_objects: vec![definition],
        };
        session.game = GameInventory::new(
            session.game.weapons.clone(),
            std::slice::from_ref(&instance),
            &definitions,
            Difficulty::Normal,
        );
        session.set_objects(ObjectCombat::new(
            &[instance],
            &definitions,
            &script,
            Difficulty::Normal,
        ));
        let mut saw_effect = false;
        for _ in 0..80 {
            saw_effect |= session
                .step(SessionInput::default())
                .process_effects
                .iter()
                .any(|event| matches!(event, ProcessEffect::ObjectPose { cell: [2, 0], .. }));
        }
        assert!(saw_effect);
        assert!((session.processes.object_pose([2, 0]).unwrap().position[0] - 3.5).abs() < 0.001);
        assert!(session
            .objects
            .as_ref()
            .unwrap()
            .raycast([3.0, 0.5], [1.0, 0.0], 1.0, 0.0)
            .is_some());
        session.world.player.position = [3.5, 0.5];
        let event = session.step(SessionInput::default());
        assert_eq!(event.pickups.len(), 1);
        assert!(session.game.inventory.keys[0]);
        assert!(session.processes.object_pose([2, 0]).is_none());
        assert_eq!(
            session.objects.as_ref().unwrap().is_active([2, 0]),
            Some(false)
        );
    }
    #[test]
    fn process_object_radius_blocks_player_until_removed() {
        use crate::process_runtime::{ProcessCapabilities, ProcessObject};
        let mut session = mission_session("");
        let mut raw = [0u8; 4096];
        raw[64] = 163;
        let pose = ProcessObject {
            cell: [1, 0],
            position: [1.5, 0.5],
            angle_radians: 0.0,
            elevation_raw: 0.0,
            blocking_radius: 0.3,
        };
        session.processes = ProcessRuntime::with_objects_and_hazards(
            &Script::default(),
            &session.world,
            ProcessCapabilities::default(),
            &raw,
            &[pose],
        )
        .unwrap();
        for _ in 0..120 {
            session.step(SessionInput {
                movement: Input {
                    forward: 1.0,
                    ..Default::default()
                },
                ..Default::default()
            });
        }
        assert!((session.world.player.position[0] - (1.5 - 0.3 - 0.18)).abs() < 0.002);
        assert!(session.processes.remove_object([1, 0]));
        for _ in 0..40 {
            session.step(SessionInput {
                movement: Input {
                    forward: 1.0,
                    ..Default::default()
                },
                ..Default::default()
            });
        }
        assert!(session.world.player.position[0] > 1.5);
    }
    #[test]
    fn raised_objects_allow_clearance_but_low_objects_block() {
        use crate::process_runtime::{ProcessCapabilities, ProcessObject};
        let mut session = mission_session("");
        let mut raw = [0u8; 4096];
        raw[64] = 163;
        let mut pose = ProcessObject {
            cell: [1, 0],
            position: [1.5, 0.5],
            angle_radians: 0.0,
            elevation_raw: 96.0,
            blocking_radius: 0.3,
        };
        session.processes = ProcessRuntime::with_objects_and_hazards(
            &Script::default(),
            &session.world,
            ProcessCapabilities::default(),
            &raw,
            &[pose],
        )
        .unwrap();
        for _ in 0..60 {
            session.step(SessionInput {
                movement: Input {
                    forward: 1.0,
                    ..Default::default()
                },
                ..Default::default()
            });
        }
        assert!(session.world.player.position[0] > 1.5);
        session.world.player.position = [0.5, 0.5];
        pose.elevation_raw = 32.0;
        session.processes = ProcessRuntime::with_objects_and_hazards(
            &Script::default(),
            &session.world,
            ProcessCapabilities::default(),
            &raw,
            &[pose],
        )
        .unwrap();
        for _ in 0..60 {
            session.step(SessionInput {
                movement: Input {
                    forward: 1.0,
                    ..Default::default()
                },
                ..Default::default()
            });
        }
        assert!((session.world.player.position[0] - 1.02).abs() < 0.002);
    }
    #[test]
    fn player_can_escape_initial_wall_and_object_overlap() {
        use crate::process_runtime::{ProcessCapabilities, ProcessObject};
        use crate::world::Wall;
        let mut session = mission_session("");
        session.world.player.position = [0.9, 0.5];
        session.world.walls.push(Wall {
            a: [1.0, 0.0],
            b: [1.0, 1.0],
            texture: 1,
            blocks_movement: true,
            cell: Some([1, 0]),
        });
        session.base_wall_blocking.push(true);
        session.wall_elevations.push(0.0);
        let mut raw = [0u8; 4096];
        raw[128] = 163;
        let pose = ProcessObject {
            cell: [2, 0],
            position: [0.9, 0.5],
            angle_radians: 0.0,
            elevation_raw: 0.0,
            blocking_radius: 0.3,
        };
        session.processes = ProcessRuntime::with_objects_and_hazards(
            &Script::default(),
            &session.world,
            ProcessCapabilities::default(),
            &raw,
            &[pose],
        )
        .unwrap();
        for _ in 0..20 {
            session.step(SessionInput {
                movement: Input {
                    forward: -1.0,
                    ..Default::default()
                },
                ..Default::default()
            });
        }
        assert!(session.world.player.position[0] < 0.3);
    }
    #[test]
    fn session_jump_and_restart_clone_preserve_vertical_state() {
        let mut session = mission_session("");
        let initial = session.clone();
        let first = session.step(SessionInput {
            jump: true,
            ..Default::default()
        });
        assert!(first.vertical.jumped);
        assert!(session.vertical.feet() > 0.0);
        let airborne = session.step(SessionInput {
            jump: true,
            ..Default::default()
        });
        assert!(!airborne.vertical.jumped);
        for _ in 0..120 {
            session.step(SessionInput::default());
        }
        assert!(session.vertical.grounded());
        assert!((session.vertical.feet() - 0.0).abs() < 1e-6);
        assert!(initial.vertical.grounded());
        assert_eq!(initial.vertical.feet(), 0.0);
    }
    #[test]
    fn jumping_under_raised_solid_gate_hits_its_underside() {
        use crate::world::Wall;
        let mut session = mission_session("");
        session.world.player.position = [1.0, 0.5];
        session.world.walls.push(Wall {
            a: [1.0, 0.0],
            b: [1.0, 1.0],
            texture: 1,
            blocks_movement: false,
            cell: Some([1, 0]),
        });
        session.base_wall_blocking.push(true);
        session.wall_elevations.push(80.0);
        session.elevation_by_cell[64] = 80.0;
        let first = session.step(SessionInput {
            jump: true,
            ..Default::default()
        });
        assert!(first.vertical.jumped);
        let mut hit = false;
        for _ in 0..100 {
            let event = session.step(SessionInput::default());
            hit |= event.vertical.hit_ceiling;
            assert!(session.vertical.head() <= 1.25 + 1e-5);
        }
        assert!(hit);
        assert!(session.vertical.grounded());
    }
    #[test]
    fn raised_wall_becomes_passable_and_remains_visible_as_elevated_geometry() {
        use crate::process_runtime::ProcessCapabilities;
        use crate::world::Wall;
        let text = "#proc1\n#action\nup 1 0 80\n#end\n#links\nfloor 0 0 1\n#end\n";
        let old = mission_session(text);
        let world = World {
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
        let script = Script::parse(text);
        let processes = ProcessRuntime::with_capabilities(
            &script,
            &world,
            ProcessCapabilities {
                wall_elevation: true,
                initial_cell_types: None,
            },
        );
        let mut session = Session::new(LevelSetup {
            world,
            processes,
            actors: old.actors,
            actor_definitions: old.actor_definitions,
            game: old.game,
            rockets: RocketDefinitions(vec![]),
            shot_blocking: [true; 128],
            difficulty: Difficulty::Normal,
        });
        for _ in 0..120 {
            session.step(SessionInput::default());
        }
        assert!((session.wall_elevations[0] - 80.0).abs() < 0.001);
        assert!(!session.world.walls[0].blocks_movement);
        assert!(!Session::blocks_shot(
            &session.world.walls[0],
            &session.shot_blocking,
            &session.elevation_by_cell,
            1.0
        ));
        assert!(Session::blocks_shot(
            &session.world.walls[0],
            &session.shot_blocking,
            &session.elevation_by_cell,
            1.3
        ));
    }
    #[test]
    fn script_hazard_applies_periodic_damage_to_player() {
        use crate::process_runtime::ProcessCapabilities;
        let text =
            "#proc1\n#action\nnonstop\ndeath 0 0 0 0 0 128 30\n#end\n#links\nfloor 0 0 1\n#end\n";
        let mut session = mission_session(text);
        let cells = [0u8; 4096];
        session.processes = ProcessRuntime::with_hazards(
            &Script::parse(text),
            &session.world,
            ProcessCapabilities::default(),
            &cells,
        );
        for _ in 0..20 {
            session.step(SessionInput::default());
        }
        assert_eq!(session.game.inventory.health, 70);
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
        let mut collision_session = initial.clone();
        for _ in 0..120 {
            collision_session.step(SessionInput {
                movement: Input {
                    forward: 1.0,
                    ..Input::default()
                },
                ..SessionInput::default()
            });
        }
        assert!(
            collision_session.world.player.position[0] < 1.6,
            "player should not pass through the living enemy"
        );
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
