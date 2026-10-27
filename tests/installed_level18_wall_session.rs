//! Optional installed-data Session scenario; player is placed at the authored
//! switch and shot probes are placed near the wall. This is not a route test.
#[allow(dead_code)]
#[path = "../src/archive.rs"]
mod archive;
#[allow(dead_code)]
#[path = "../src/texture.rs"]
mod texture;
use openchasm::{
    actors::{ActorDefinitions, ActorState},
    combat::{FireResult, RocketDefinitions, ShotImpact},
    game::GameInventory,
    inventory::WeaponDefinitions,
    map::{Map, MAP_SIDE, UNITS_PER_TILE},
    object_combat::ObjectCombat,
    object_states::ObjectStates,
    objects::{self, ObjectDefinitions, ObjectKind},
    pickups::Difficulty,
    process::Script,
    process_runtime::{ProcessCapabilities, ProcessObject, ProcessRuntime},
    process_transaction::PresentationManifest,
    session::{LevelSetup, ScriptedObjects, Session, SessionInput},
    wall_transitions::{WallMaterial, WallMaterialCatalog},
    world::{Player, Wall, World},
};
use std::path::PathBuf;

#[test]
fn real_wall_change_updates_session_material_and_rolls_back_failure() {
    let explicit = std::env::var_os("OPENCHASM_TEST_CONTENT");
    let path = explicit
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("assets/csm.bin"));
    if !path.exists() && explicit.is_none() {
        return;
    }
    let mut archive = archive::Archive::open(&path).unwrap();
    let map = Map::parse(&archive.read("map/map.18").unwrap()).unwrap();
    let inf = archive.read("chasm.inf").unwrap();
    let level = archive.read("map/resource.18").unwrap();
    let defs = ObjectDefinitions::parse(&inf, &level).unwrap();
    let script =
        Script::parse(std::str::from_utf8(&archive.read("map/process.18").unwrap()).unwrap());
    let instances = objects::instances(&map);
    // Decode the actual #GFX rows using the same observed g/s conventions as
    // the client, and verify the replacement CEL is actually readable.
    let text = std::str::from_utf8(&level).unwrap();
    let mut walk = [true; 128];
    let mut shots = [true; 128];
    let mut names: [Option<String>; 128] = std::array::from_fn(|_| None);
    let mut section = false;
    for line in text.lines().map(str::trim) {
        if line.eq_ignore_ascii_case("#gfx") {
            section = true;
            continue;
        }
        if section && line.to_ascii_lowercase().starts_with("#end") {
            break;
        }
        if !section {
            continue;
        }
        let Some((id, rest)) = line.split_once(':') else {
            continue;
        };
        let Some(id) = id.trim().parse::<usize>().ok().filter(|id| *id < 128) else {
            continue;
        };
        let mut fields = rest.split_whitespace();
        let Some(name) = fields.next() else { continue };
        if !name.to_ascii_lowercase().ends_with(".cel") {
            continue;
        }
        names[id] = Some(name.to_ascii_lowercase());
        if (87..=119).contains(&id) {
            if let Some(flags) = fields.next() {
                let flags = flags.to_ascii_lowercase();
                walk[id] = !flags.starts_with('g');
                shots[id] = flags.as_bytes().get(1) != Some(&b's');
            }
        }
    }
    assert!(walk[1] && shots[1]);
    assert!(!walk[115] && !shots[115]);
    for id in [1, 115] {
        let bytes = archive
            .read(&format!("texture/{}", names[id].as_ref().unwrap()))
            .unwrap();
        texture::CelImage::parse(&bytes).expect("authored wall material must decode");
    }
    let mut walls = Vec::new();
    for x in 0..MAP_SIDE {
        for y in 0..MAP_SIDE {
            let w = map.wall(x, y).unwrap();
            if w.is_wall() && w.start != w.end {
                let cv = |p: [u16; 2]| p.map(|v| v as f32 / UNITS_PER_TILE);
                walls.push(Wall {
                    cell: Some([x as u8, y as u8]),
                    a: cv(w.start),
                    b: cv(w.end),
                    texture: w.texture_id,
                    blocks_movement: walk[usize::from(w.texture_id)],
                });
            }
        }
    }
    let world = World {
        walls,
        player: Player {
            position: [17.5, 20.2],
            angle: 0.0,
        },
    };
    let cells = std::array::from_fn(|i| map.wall(i / MAP_SIDE, i % MAP_SIDE).unwrap().texture_id);
    let poses: Vec<_> = instances
        .iter()
        .filter(|o| o.difficulty_flags & 2 != 0)
        .filter_map(|o| {
            let d = defs.definition(o)?;
            Some(ProcessObject {
                cell: [o.grid_cell[0] as u8, o.grid_cell[1] as u8],
                position: o.position,
                angle_radians: o.angle_radians,
                elevation_raw: 0.0,
                blocking_radius: if o.kind == ObjectKind::Item
                    || (141..=143).contains(&d.action_code)
                {
                    0.0
                } else {
                    (d.collision_radius.max(0) as f32 / 256.0).min(2.0)
                },
            })
        })
        .collect();
    let runtime = ProcessRuntime::with_wall_changes(
        &script,
        &world,
        ProcessCapabilities {
            wall_elevation: true,
            initial_cell_types: None,
        },
        &cells,
        &poses,
    )
    .unwrap();
    let game = GameInventory::new(
        WeaponDefinitions::parse(&inf).unwrap(),
        &instances,
        &defs,
        Difficulty::Normal,
    );
    let mut session = Session::new(LevelSetup {
        world,
        processes: runtime,
        actors: ActorState {
            player_spawns: vec![],
            actors: vec![],
            unresolved: vec![],
        },
        actor_definitions: ActorDefinitions::parse(&inf).unwrap(),
        game,
        rockets: RocketDefinitions::parse(&inf).unwrap(),
        shot_blocking: shots,
        difficulty: Difficulty::Normal,
    });
    session.set_objects(ObjectCombat::new(
        &instances,
        &defs,
        &script,
        Difficulty::Normal,
    ));
    session.set_scripted_objects(ScriptedObjects::new(
        ObjectStates::new(&instances, &defs).unwrap(),
        defs,
        PresentationManifest::default(),
        &script,
        true,
    ));
    let index = session
        .world
        .walls
        .iter()
        .position(|w| w.cell == Some([20, 17]))
        .unwrap();
    let make_catalog = |available| {
        let mut catalog = WallMaterialCatalog::default();
        for id in [1u8, 115] {
            catalog.set(
                id,
                WallMaterial {
                    blocks_movement: walk[id as usize],
                    blocks_shots: shots[id as usize],
                    presentation_available: id != 115 || available,
                },
            );
        }
        catalog
    };
    let mut blocked = session.clone();
    assert!(blocked.set_wall_material_catalog(make_catalog(false)));
    let old_pose = blocked.processes.object_pose([20, 16]).unwrap();
    blocked.step(SessionInput {
        interact: true,
        ..Default::default()
    });
    for _ in 0..60 {
        blocked.step(SessionInput::default());
    }
    assert_eq!(blocked.world.walls[index].texture, 1);
    assert!(blocked.world.walls[index].blocks_movement);
    assert_eq!(blocked.processes.cell_type([20, 17]), Some(1));
    assert_eq!(blocked.processes.object_pose([20, 16]).unwrap(), old_pose);
    assert!(session.set_wall_material_catalog(make_catalog(true)));
    let mut original_shot = session.clone();
    original_shot.world.player.position = [18.5, 17.5];
    original_shot.world.player.angle = 0.0;
    let shot = original_shot
        .step(SessionInput {
            fire: true,
            ..Default::default()
        })
        .shot;
    assert!(
        matches!(shot, Some(FireResult::Fired(ShotImpact::Wall { wall_index, .. })) if wall_index == index),
        "{shot:?}"
    );
    let before = session.processes.object_pose([20, 16]).unwrap();
    let commands = session
        .processes
        .collect_tick_commands(session.world.player.position, true);
    assert!(commands.iter().any(|c| c.process_id == 2), "{commands:?}");
    session.step(SessionInput {
        interact: true,
        ..Default::default()
    });
    assert_eq!(session.world.walls[index].texture, 115);
    assert!(!session.world.walls[index].blocks_movement);
    assert_eq!(session.processes.cell_type([20, 17]), Some(115));
    // Fire through the wall's original geometry from a controlled position.
    // The target is now authored shoot-through; verify that the hit is not it.
    session.world.player.position = [18.5, 17.5];
    session.world.player.angle = 0.0;
    let fired = session
        .step(SessionInput {
            fire: true,
            ..Default::default()
        })
        .shot;
    assert!(matches!(fired, Some(FireResult::Fired(_))), "{fired:?}");
    assert!(
        !matches!(fired,Some(FireResult::Fired(ShotImpact::Wall {wall_index,..})) if wall_index==index),
        "{fired:?}"
    );
    for _ in 0..90 {
        session.step(SessionInput::default());
    }
    assert!(
        session.processes.object_pose([20, 16]).unwrap().position[0] < before.position[0] - 0.45
    );
}
