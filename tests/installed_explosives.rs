//! Optional integration tests using the owner's installed weapon definitions.
//! Set OPENCHASM_TEST_CONTENT to an owned `csm.bin`; no game data is bundled.
#[allow(dead_code)]
#[path = "../src/archive.rs"]
mod archive;
use openchasm::{
    actors::{ActorDefinitions, ActorState},
    combat::{FireResult, RocketDefinitions, ShotImpact},
    explosives::{MineEvent, TargetId, MINE_ARM_TICKS},
    game::GameInventory,
    inventory::{Pickup, PickupOutcome, WeaponDefinitions},
    objects::ObjectDefinitions,
    pickups::Difficulty,
    process::Script,
    process_runtime::ProcessRuntime,
    projectiles::Owner,
    session::{ExplosionOrigin, LevelSetup, Session, SessionInput},
    world::{Player, Wall, World},
};
use std::path::PathBuf;

fn fixture() -> Option<Session> {
    let explicit = std::env::var_os("OPENCHASM_TEST_CONTENT");
    let path = explicit
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("assets/csm.bin"));
    if !path.exists() && explicit.is_none() {
        eprintln!("No owned archive; explosives checks skipped.");
        return None;
    }
    let mut archive = archive::Archive::open(&path).expect("open selected archive");
    let inf = archive
        .read("chasm.inf")
        .expect("read installed weapon definitions");
    let world = World {
        walls: vec![Wall {
            a: [1.5, 0.0],
            b: [1.5, 1.0],
            texture: 1,
            blocks_movement: true,
            cell: None,
        }],
        player: Player {
            position: [0.5, 0.5],
            angle: 0.0,
        },
    };
    let processes = ProcessRuntime::new(&Script::default(), &world);
    let definitions = ObjectDefinitions {
        items: vec![],
        level_objects: vec![],
    };
    Some(Session::new(LevelSetup {
        world,
        processes,
        actors: ActorState {
            player_spawns: vec![],
            actors: vec![],
            unresolved: vec![],
        },
        actor_definitions: ActorDefinitions::parse(&inf).unwrap(),
        game: GameInventory::new(
            WeaponDefinitions::parse(&inf).unwrap(),
            &[],
            &definitions,
            Difficulty::Normal,
        ),
        rockets: RocketDefinitions::parse(&inf).unwrap(),
        shot_blocking: [true; 128],
        difficulty: Difficulty::Normal,
    }))
}

#[test]
fn installed_grenade_weapon_launches_authored_splash() {
    let Some(mut session) = fixture() else {
        return;
    };
    let weapons = session.game.weapons.clone();
    assert_eq!(weapons.0[5].rocket_type, 10);
    assert_eq!(
        session.game.inventory.pickup(6, &weapons),
        PickupOutcome::Collected(Pickup::Weapon(5))
    );
    let before = session.game.inventory.ammo[5];
    assert!(before > 0);
    session.set_explosions_enabled(true);
    let shot = session.step(SessionInput {
        fire: true,
        select_weapon: Some(5),
        ..Default::default()
    });
    assert_eq!(session.selected_weapon, 5);
    assert!(matches!(
        shot.shot,
        Some(FireResult::Fired(ShotImpact::Launch { rocket_id: 10 }))
    ));
    assert_eq!(session.game.inventory.ammo[5], before - 1);
    let mut blast = None;
    for _ in 0..30 {
        let events = session.step(SessionInput::default());
        if let Some(explosion) = events.explosions.first() {
            assert_eq!(
                events.explosion_origins,
                [ExplosionOrigin::Projectile {
                    projectile_id: 0,
                    rocket_id: 10
                }]
            );
            assert!(events
                .explosion_damage
                .iter()
                .any(|d| d.target == TargetId::Player && d.owner == Owner::Player && d.damage > 0));
            blast = Some(*explosion);
            break;
        }
    }
    let blast = blast.expect("installed grenade must collide and explode");
    assert_eq!(blast.damage, 100);
    assert!((blast.radius - 424.0 / 256.0).abs() < 1e-6);
    assert!(session.game.inventory.health < 100);
    assert_eq!(session.game.inventory.keys, [false; 3]);
}

#[test]
fn installed_mine_weapon_consumes_ammo_and_arms() {
    let Some(mut session) = fixture() else {
        return;
    };
    let weapons = session.game.weapons.clone();
    assert_eq!(
        session.game.inventory.pickup(7, &weapons),
        PickupOutcome::Collected(Pickup::Weapon(6))
    );
    assert_eq!(
        session.game.inventory.ammo[6], 0,
        "installed mine weapon starts empty"
    );
    assert_eq!(
        session.game.inventory.pickup(71, &weapons),
        PickupOutcome::Collected(Pickup::Ammo(6))
    );
    assert_eq!(session.game.inventory.ammo[6], 1);
    session.set_mines_enabled(true);
    let placed = session.step(SessionInput {
        fire: true,
        select_weapon: Some(6),
        ..Default::default()
    });
    assert_eq!(session.selected_weapon, 6);
    assert_eq!(placed.shot, Some(FireResult::Fired(ShotImpact::MinePlaced)));
    assert_eq!(session.game.inventory.ammo[6], 0);
    assert!(
        matches!(&placed.mine_events[..], [MineEvent::Placed(mine)] if mine.id == 0 && mine.age_ticks == 0)
    );
    for _ in 1..MINE_ARM_TICKS {
        assert!(session.step(SessionInput::default()).mine_events.is_empty());
    }
    let detonation = session.step(SessionInput::default());
    assert!(matches!(
        &detonation.mine_events[..],
        [
            MineEvent::Armed { id: 0 },
            MineEvent::Detonated { id: 0, .. }
        ]
    ));
    assert_eq!(
        detonation.explosion_origins,
        [ExplosionOrigin::Mine { mine_id: 0 }]
    );
    assert!(detonation
        .explosion_damage
        .iter()
        .any(|d| d.target == TargetId::Player && d.owner == Owner::Player));
    assert!(session.game.inventory.health < 100);
    assert!(session.mines().is_empty());
    assert_eq!(session.game.inventory.keys, [false; 3]);
}
