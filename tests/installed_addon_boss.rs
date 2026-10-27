//! Optional integration against the owned remaster addon resources.
#[allow(dead_code)]
#[path = "../src/archive.rs"]
mod archive;
use openchasm::{
    actors::{ActorDefinitions, ActorMapping, ActorState},
    combat::RocketDefinitions,
    game::GameInventory,
    inventory::WeaponDefinitions,
    map::Map,
    mission::{MissionEvent, MissionPlan},
    objects::ObjectDefinitions,
    pickups::Difficulty,
    process::Script,
    process_runtime::ProcessRuntime,
    projectiles::{Launch, Owner},
    session::{LevelSetup, Session, SessionInput},
    world::{Player, World},
};
use std::path::PathBuf;

#[test]
fn addon_boss_reflection_completes_installed_level19() {
    let explicit = std::env::var_os("OPENCHASM_TEST_CONTENT");
    let path = explicit
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("assets/csm.bin"));
    if !path.exists() && explicit.is_none() {
        return;
    }
    let mut archive = archive::Archive::open(&path).unwrap();
    let inf = archive.read("chasm.inf").unwrap();
    let raw_map = archive.read("map/map.19").unwrap();
    let mapping = ActorMapping::recognize_shipped_addon(&raw_map, &inf);
    assert_eq!(mapping, ActorMapping::AddonToRemaster);
    let map = Map::parse(&raw_map).unwrap();
    let defs = ActorDefinitions::parse(&inf).unwrap();
    let mut actors = ActorState::from_placements_with_mapping(&map.monsters, &defs, mapping);
    actors.actors.retain(|actor| actor.placement.id == 118);
    let boss = &actors.actors[0];
    assert_eq!(boss.definition_index, 23);
    let boss_id = boss.source_index;
    let boss_pos = boss.position;
    let world = World {
        walls: vec![],
        player: Player {
            position: [boss_pos[0] - 2.0, boss_pos[1]],
            angle: 0.0,
        },
    };
    let text = archive.read("map/process.19").unwrap();
    let text = std::str::from_utf8(&text).unwrap();
    let script = Script::parse(text);
    let processes = ProcessRuntime::new(&script, &world);
    let weapons = WeaponDefinitions::parse(&inf).unwrap();
    let rockets = RocketDefinitions::parse(&inf).unwrap();
    let game = GameInventory::new(
        weapons,
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
        actors,
        actor_definitions: defs,
        game,
        rockets: rockets.clone(),
        shot_blocking: [true; 128],
        difficulty: Difficulty::Normal,
    });
    session.set_mission(MissionPlan::parse(&script, text));
    session.set_addon_boss_rule(mapping);
    session.game.protective_powerups.reflector_ticks_remaining = 600;
    session
        .projectiles
        .spawn(
            Launch {
                owner: Owner::Enemy {
                    source_index: boss_id,
                },
                rocket_id: 20,
                origin: [boss_pos[0] - 1.0, boss_pos[1]],
                direction: [-1.0, 0.0],
            },
            &rockets,
        )
        .unwrap();
    let mut reflected = false;
    let mut completed = false;
    for _ in 0..180 {
        let events = session.step(SessionInput::default());
        reflected |= events
            .projectile_reflections
            .iter()
            .any(|event| event.rocket_id == 20);
        completed |= events.mission.contains(&MissionEvent::LevelComplete);
        if completed {
            break;
        }
    }
    assert!(reflected);
    assert_eq!(session.actors.actors[0].health, 0);
    assert!(completed);
}
