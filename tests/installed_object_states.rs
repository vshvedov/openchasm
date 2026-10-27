//! Optional level 16 object transition check against an owned Steam archive.
//! Set OPENCHASM_TEST_CONTENT to a 2022-release csm.bin; proprietary data is
//! never included in the repository or required by CI.
#[allow(dead_code)]
#[path = "../src/archive.rs"]
mod archive;

use openchasm::{
    map::Map,
    object_states::{ObjectAction, ObjectStates, TransitionKind},
    objects::{self, ObjectDefinitions},
    process::Script,
};
use std::path::PathBuf;

#[test]
fn level_16_heart_aftermath_uses_authored_replacements() {
    let explicit = std::env::var_os("OPENCHASM_TEST_CONTENT");
    let path = explicit
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("assets/csm.bin"));
    if !path.exists() && explicit.is_none() {
        eprintln!("No local archive: level 16 object transition check skipped. Set OPENCHASM_TEST_CONTENT to enable it.");
        return;
    }
    let mut archive = archive::Archive::open(&path).expect("open selected game archive");
    let map = Map::parse(&archive.read("map/map.16").expect("read level 16 map"))
        .expect("parse level 16 map");
    let inf = archive.read("chasm.inf").expect("read game definitions");
    let level = archive
        .read("map/resource.16")
        .expect("read level 16 definitions");
    let definitions = ObjectDefinitions::parse(&inf, &level).expect("parse object definitions");
    let script_data = archive
        .read("map/process.16")
        .expect("read level 16 script");
    let script = Script::parse(std::str::from_utf8(&script_data).expect("script text is UTF-8"));
    assert!(script.diagnostics.is_empty(), "{:?}", script.diagnostics);
    let process = script.processes.get(&7).expect("level 16 heart procedure");
    let actions: Vec<ObjectAction> = process
        .actions
        .iter()
        .filter_map(|command| match command.name.as_str() {
            "explode" => Some(ObjectAction::Explode {
                cell: [
                    command.arguments[0].parse().expect("valid explode x"),
                    command.arguments[1].parse().expect("valid explode y"),
                ],
            }),
            "change" => Some(ObjectAction::Change {
                cell: [
                    command.arguments[0].parse().expect("valid change x"),
                    command.arguments[1].parse().expect("valid change y"),
                ],
                raw_type: command.arguments[2]
                    .parse()
                    .expect("valid replacement type"),
            }),
            _ => None,
        })
        .collect();
    let expected = [
        ObjectAction::Explode { cell: [27, 46] },
        ObjectAction::Explode { cell: [28, 47] },
        ObjectAction::Explode { cell: [28, 48] },
        ObjectAction::Explode { cell: [27, 48] },
        ObjectAction::Change {
            cell: [29, 48],
            raw_type: 200,
        },
    ];
    assert_eq!(
        actions, expected,
        "installed script differs from the checked level"
    );
    let instances = objects::instances(&map);
    let mut state = ObjectStates::new(&instances, &definitions).expect("index level objects");
    for cell in [[27, 46], [28, 47], [28, 48], [27, 48]] {
        assert_eq!(state.get(cell).unwrap().raw_type, 201);
        assert_eq!(
            state.get(cell).unwrap().health,
            0,
            "scripted explode must work even with zero BLim"
        );
    }
    let portal_position = state.get([29, 48]).unwrap().placement.position;
    let events = state
        .apply_batch(&actions)
        .expect("apply heart aftermath transitions");
    assert_eq!(events.len(), 5);
    for (index, cell) in [[27, 46], [28, 47], [28, 48], [27, 48]]
        .into_iter()
        .enumerate()
    {
        let event = &events[index];
        assert_eq!(
            (event.cell, event.kind, event.previous_type, event.new_type),
            (cell, TransitionKind::Explode, 201, 202)
        );
        assert!(event.reset_animation);
        assert_eq!(state.definition(cell).unwrap().model_name, "dummy.3o");
    }
    let portal = &events[4];
    assert_eq!(
        (
            portal.cell,
            portal.kind,
            portal.previous_type,
            portal.new_type
        ),
        ([29, 48], TransitionKind::Change, 193, 200)
    );
    assert_eq!(state.definition([29, 48]).unwrap().model_name, "fch2.3o");
    assert_eq!(
        state.get([29, 48]).unwrap().placement.position,
        portal_position
    );
}
