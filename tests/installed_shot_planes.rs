//! Optional real map check: authored outdoor sky must not stop upward shots.
#[allow(dead_code)]
#[path = "../src/archive.rs"]
mod archive;
use openchasm::{
    actors::{ActorDefinitions, ActorState},
    combat::{FireResult, RocketDefinitions, ShotImpact, ShotPlane},
    game::GameInventory,
    inventory::WeaponDefinitions,
    map::Map,
    objects::ObjectDefinitions,
    pickups::Difficulty,
    process::Script,
    process_runtime::ProcessRuntime,
    session::{LevelSetup, Session, SessionInput},
    world::{Player, World},
};

#[test]
fn level01_sky_and_roof_follow_authored_tiles() {
    let path = std::path::Path::new("assets/csm.bin");
    if !path.exists() {
        return;
    }
    let mut archive = archive::Archive::open(path).unwrap();
    let map = Map::parse(&archive.read("map/map.01").unwrap()).unwrap();
    let inf = archive.read("chasm.inf").unwrap();
    let ceiling_cell = |open: bool| {
        (1..63)
            .flat_map(|x| (1..63).map(move |y| (x, y)))
            .find(|&(x, y)| (map.ceiling(x, y).unwrap() >= 62) == open)
            .unwrap()
    };
    for open in [true, false] {
        let (x, y) = ceiling_cell(open);
        let world = World {
            walls: vec![],
            player: Player {
                position: [x as f32 + 0.2, y as f32 + 0.5],
                angle: 0.0,
            },
        };
        let processes = ProcessRuntime::new(&Script::default(), &world);
        let game = GameInventory::new(
            WeaponDefinitions::parse(&inf).unwrap(),
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
            actor_definitions: ActorDefinitions::parse(&inf).unwrap(),
            game,
            rockets: RocketDefinitions::parse(&inf).unwrap(),
            shot_blocking: [true; 128],
            difficulty: Difficulty::Normal,
        });
        session.game.inventory.weapons[0] = true;
        session.game.inventory.ammo[0] = 1;
        session.set_authored_shot_planes(&map);
        let events = session.step(SessionInput {
            fire: true,
            pitch_delta: 1.3,
            ..Default::default()
        });
        let plane = matches!(
            events.shot,
            Some(FireResult::Fired(ShotImpact::Plane {
                plane: ShotPlane::Ceiling,
                ..
            }))
        );
        assert_eq!(plane, !open, "level01 cell ({x},{y})");
    }
}
