//! Client-side capability manifest for scripted level-object replacements.
//!
//! A manifest is a promise that a particular scene placement and destination
//! asset are available; authoritative transitions still validate game state.
use crate::content::ResourceSet;
use crate::object_render::ObjectScene;
use openchasm::objects::{ObjectDefinitions, ObjectInstance, ObjectKind};
use openchasm::pickups::Difficulty;
use openchasm::process::Script;
use openchasm::process_transaction::PresentationManifest;
use std::collections::{BTreeMap, BTreeSet};

/// Build capabilities only for represented, difficulty-enabled level objects.
/// A missing destination model or animation leaves that pair unavailable and
/// produces a diagnostic; it does not prevent the rest of the level loading.
#[allow(dead_code)] // Connected when the session's staged action path is enabled.
pub fn build_replacement_manifest(
    resources: &mut ResourceSet,
    scene: &mut ObjectScene,
    definitions: &ObjectDefinitions,
    instances: &[ObjectInstance],
    script: &Script,
    difficulty: Difficulty,
) -> PresentationManifest {
    let candidates = reachable_replacements(
        instances,
        definitions.level_objects.len(),
        script,
        difficulty,
    );
    let mut availability = BTreeMap::new();
    let mut manifest = PresentationManifest::default();
    for (cell, raw_type) in candidates {
        if !scene.has_cell(cell) {
            eprintln!("replacement unavailable at {cell:?}: source object has no render instance");
            continue;
        }
        let available = *availability
            .entry(raw_type)
            .or_insert_with(|| scene.preload_level_object(resources, definitions, raw_type));
        if available {
            manifest.allow(cell, raw_type);
        } else {
            eprintln!("replacement unavailable at {cell:?}: type {raw_type} has missing or invalid assets");
        }
    }
    manifest
}

/// Conservative closure: each authored change can set a target to its explicit
/// type, and any authored explode can be invoked repeatedly. We don't infer
/// process lock/trigger reachability, so this can overestimate actual assets.
fn reachable_replacements(
    instances: &[ObjectInstance],
    definition_count: usize,
    script: &Script,
    difficulty: Difficulty,
) -> BTreeSet<([usize; 2], u8)> {
    let mask = match difficulty {
        Difficulty::Easy => 1,
        Difficulty::Normal => 2,
        Difficulty::Hard => 4,
    };
    let valid = |raw: u8| (163..=226).contains(&raw) && usize::from(raw - 163) < definition_count;
    let mut cells = BTreeMap::new();
    for instance in instances {
        if instance.kind != ObjectKind::LevelObject || instance.difficulty_flags & mask == 0 {
            continue;
        }
        let Some(raw) = u8::try_from(instance.definition_index)
            .ok()
            .and_then(|i| i.checked_add(163))
            .filter(|&raw| valid(raw))
        else {
            continue;
        };
        cells.insert(instance.grid_cell, raw);
    }
    let mut changes: BTreeMap<[usize; 2], BTreeSet<u8>> = BTreeMap::new();
    let mut explodes = BTreeSet::new();
    for action in script
        .processes
        .values()
        .flat_map(|process| &process.actions)
    {
        let args = &action.arguments;
        let expected = match action.name.as_str() {
            "change" => 3,
            "explode" => 2,
            _ => continue,
        };
        if args.len() != expected {
            continue;
        }
        let Some(cell) = args[0]
            .parse::<usize>()
            .ok()
            .zip(args[1].parse::<usize>().ok())
            .map(|(x, y)| [x, y])
            .filter(|cell| cell[0] < 64 && cell[1] < 64 && cells.contains_key(cell))
        else {
            continue;
        };
        if action.name == "explode" {
            explodes.insert(cell);
        } else if let Ok(raw) = args[2].parse::<u8>() {
            if valid(raw) {
                changes.entry(cell).or_default().insert(raw);
            }
        }
    }
    let mut result = BTreeSet::new();
    for (cell, initial) in cells {
        let mut types = changes.remove(&cell).unwrap_or_default();
        if explodes.contains(&cell) {
            // All values at or above the least possible seed are reachable by
            // repeated explode. Do not include the initial type unless a change
            // or a cycle explicitly selects it.
            let lowest = types
                .iter()
                .copied()
                .min()
                .map_or(initial, |other| initial.min(other));
            for raw in lowest.saturating_add(1)..=226 {
                if valid(raw) {
                    types.insert(raw);
                }
            }
        }
        result.extend(types.into_iter().map(|raw| (cell, raw)));
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    fn instance(index: usize, cell: [usize; 2], flags: u16, kind: ObjectKind) -> ObjectInstance {
        ObjectInstance {
            kind,
            definition_index: index,
            grid_cell: cell,
            position: [1.0, 1.0],
            angle_radians: 0.0,
            flags: 0,
            difficulty_flags: flags,
            extra: 0,
            size: 0,
        }
    }
    #[test]
    fn closure_includes_repeated_explosions_and_explicit_changes_only_for_eligible_cells() {
        let script = Script::parse("#proc1\n#action\nexplode 1 1\nchange 1 1 164\nchange 2 2 166\nchange 3 3 165\nchange 9 9 165\nchange 1 1 250\nexplode 1 1 extra\n#end\n");
        let instances = [
            instance(3, [1, 1], 7, ObjectKind::LevelObject),
            instance(0, [2, 2], 1, ObjectKind::LevelObject),
            instance(0, [3, 3], 7, ObjectKind::Item),
        ];
        let got = reachable_replacements(&instances, 6, &script, Difficulty::Normal);
        assert_eq!(
            got,
            [164, 165, 166, 167, 168]
                .into_iter()
                .map(|raw| ([1, 1], raw))
                .collect()
        );
    }
    #[test]
    fn no_explosion_only_includes_authored_changes_and_valid_destinations() {
        let script = Script::parse(
            "#proc1\n#action\nchange 1 1 164\nchange 1 1 165\nchange 1 1 163\n#end\n",
        );
        let instances = [instance(0, [1, 1], 7, ObjectKind::LevelObject)];
        let got = reachable_replacements(&instances, 2, &script, Difficulty::Normal);
        assert_eq!(got, [([1, 1], 163), ([1, 1], 164)].into_iter().collect());
    }
    #[test]
    fn installed_preflight_only_promises_loadable_replacements() {
        use crate::texture::Palette;
        let explicit = std::env::var_os("OPENCHASM_TEST_CONTENT");
        let path = explicit
            .as_ref()
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from("assets/csm.bin"));
        if !path.exists() && explicit.is_none() {
            return;
        }
        let mut resources = ResourceSet::open(&path).unwrap();
        let map = openchasm::map::Map::parse(&resources.read("map/map.16").unwrap()).unwrap();
        let mut definitions = ObjectDefinitions::parse(
            &resources.read("chasm.inf").unwrap(),
            &resources.read("map/resource.16").unwrap(),
        )
        .unwrap();
        let palette = Palette::parse(&resources.read("chasm2.pal").unwrap()).unwrap();
        let mut scene = ObjectScene::load(&mut resources, &map, &definitions, &palette);
        let instances = openchasm::objects::instances(&map);
        // Simulate a missing replacement without changing the archive.
        definitions.level_objects[usize::from(200_u8 - 163)].model_name = "not-present.3o".into();
        let script = Script::parse("#proc1\n#action\nchange 27 46 202\nchange 29 48 200\n#end\n");
        let manifest = build_replacement_manifest(
            &mut resources,
            &mut scene,
            &definitions,
            &instances,
            &script,
            Difficulty::Normal,
        );
        assert!(manifest.contains([27, 46], 202));
        assert!(!manifest.contains([29, 48], 200));
        assert!(!manifest.contains([28, 47], 202));
        assert!(scene.replace_level_object([27, 46], 202));
    }
    #[test]
    #[ignore]
    fn timing_probe() {
        use crate::object_render::CameraView;
        use crate::texture::Palette;
        use crate::world::Player;
        use std::time::Instant;
        let path = std::env::var_os("OPENCHASM_TEST_CONTENT")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from("assets/csm.bin"));
        let mut resources =
            ResourceSet::open(&path).expect("timing probe requires an owned game archive");
        let palette = Palette::parse(&resources.read("chasm2.pal").unwrap()).unwrap();
        let inf = resources.read("chasm.inf").unwrap();
        for id in ["01", "02", "16"] {
            let map =
                openchasm::map::Map::parse(&resources.read(&format!("map/map.{id}")).unwrap())
                    .unwrap();
            let definitions = ObjectDefinitions::parse(
                &inf,
                &resources.read(&format!("map/resource.{id}")).unwrap(),
            )
            .unwrap();
            let script_bytes = resources.read(&format!("map/process.{id}")).unwrap();
            let script = Script::parse(std::str::from_utf8(&script_bytes).unwrap());
            let instances = openchasm::objects::instances(&map);
            let start = Instant::now();
            let mut scene = ObjectScene::load(&mut resources, &map, &definitions, &palette);
            let actor_defs = openchasm::actors::ActorDefinitions::parse(&inf).unwrap();
            let actors = openchasm::actors::ActorState::from_placements(&map.monsters, &actor_defs);
            scene.add_actors(&mut resources, &actors, &actor_defs);
            let load = start.elapsed();
            let start = Instant::now();
            let _ = build_replacement_manifest(
                &mut resources,
                &mut scene,
                &definitions,
                &instances,
                &script,
                Difficulty::Normal,
            );
            let preload = start.elapsed();
            let player = map.monsters.iter().find(|m| m.is_player_spawn()).unwrap();
            let camera = CameraView {
                player: Player {
                    position: [
                        f32::from(player.position[0]) / 256.0,
                        f32::from(player.position[1]) / 256.0,
                    ],
                    angle: player.direction_radians(),
                },
                eye_height: 0.75,
            };
            let mut frame = vec![0; 640 * 400];
            let mut depth = vec![f32::INFINITY; 640 * 400];
            let start = Instant::now();
            for _ in 0..30 {
                depth.fill(f32::INFINITY);
                scene.render(&mut frame, &mut depth, 640, 400, camera);
            }
            let render = start.elapsed() / 30;
            eprintln!(
                "PROBE map{id} scene={load:?} preload={preload:?} render_no_wall_depth={render:?}"
            );
        }
    }
}
