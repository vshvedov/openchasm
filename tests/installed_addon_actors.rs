//! Optional checks against an owned remaster archive; no game assets are embedded.
#[allow(dead_code)]
#[path = "../src/archive.rs"]
mod archive;

use openchasm::{
    actors::{ActorDefinitions, ActorMapping, ActorState},
    map::Map,
};
use std::path::PathBuf;

#[test]
fn shipped_addon_actor_resolution_is_content_scoped() {
    let explicit = std::env::var_os("OPENCHASM_TEST_CONTENT");
    let path = explicit
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("assets/csm.bin"));
    if !path.exists() && explicit.is_none() {
        return;
    }
    let mut archive = archive::Archive::open(&path).expect("open content");
    let inf = archive.read("chasm.inf").unwrap();
    let defs = ActorDefinitions::parse(&inf).unwrap();
    for level in ["17", "18", "19"] {
        let bytes = archive.read(&format!("map/map.{level}")).unwrap();
        assert_eq!(
            ActorMapping::recognize_shipped_addon(&bytes, &inf),
            ActorMapping::AddonToRemaster
        );
        let map = Map::parse(&bytes).unwrap();
        let actors = ActorState::from_placements_with_mapping(
            &map.monsters,
            &defs,
            ActorMapping::AddonToRemaster,
        );
        assert!(actors.unresolved.is_empty());
        for actor in &actors.actors {
            assert_eq!(
                actor.definition_index,
                match actor.placement.id {
                    116..=118 => usize::from(actor.placement.id - 95),
                    id => usize::from(id - 100),
                }
            );
        }
        if level == "17" {
            for (id, expected_name, expected_flags) in [
                (116, "shot.car", vec![4, 4, 4, 6, 6, 7, 7, 7]),
                (117, "wolf2.car", vec![4, 4, 6, 6, 7, 7]),
            ] {
                let selected: Vec<_> = actors
                    .actors
                    .iter()
                    .filter(|a| a.placement.id == id)
                    .collect();
                let mut flags: Vec<_> = selected.iter().map(|a| a.placement.flags).collect();
                flags.sort_unstable();
                assert_eq!(flags, expected_flags);
                assert!(selected.iter().all(|a| defs.entries[a.definition_index]
                    .model_name
                    .eq_ignore_ascii_case(expected_name)));
            }
        }
        if level == "19" {
            let wizard = actors
                .actors
                .iter()
                .find(|a| a.placement.id == 118)
                .unwrap();
            assert_eq!(wizard.definition_index, 23);
            assert_eq!(wizard.placement.flags, 7);
            assert!(defs.entries[23]
                .model_name
                .eq_ignore_ascii_case("wizard2.car"));
            assert_eq!(defs.entries[23].rocket, 20);
        }
        let mut altered_map = bytes.clone();
        altered_map[0] ^= 1;
        assert_eq!(
            ActorMapping::recognize_shipped_addon(&altered_map, &inf),
            ActorMapping::Identity
        );
        let mut altered_inf = inf.clone();
        altered_inf[0] ^= 1;
        assert_eq!(
            ActorMapping::recognize_shipped_addon(&bytes, &altered_inf),
            ActorMapping::Identity
        );
    }
    let base = archive.read("map/map.08").unwrap();
    assert_eq!(
        ActorMapping::recognize_shipped_addon(&base, &inf),
        ActorMapping::Identity
    );
    let base = Map::parse(&base).unwrap();
    let actors = ActorState::from_placements(&base.monsters, &defs);
    let sphinx = actors
        .actors
        .iter()
        .find(|a| a.placement.id == 118)
        .unwrap();
    assert_eq!(sphinx.definition_index, 18);
    assert!(defs.entries[18]
        .model_name
        .eq_ignore_ascii_case("sphinx1.car"));
}
