//! Optional real-data verification; no proprietary resources are embedded.
#[allow(dead_code)]
#[path = "../src/archive.rs"]
mod archive;
use openchasm::{
    ambient::AmbientRegion,
    process::Script,
    process_runtime::{ImmediateAction, ProcessCapabilities, ProcessOutcome, ProcessRuntime},
    world::{Player, World},
};
use std::path::PathBuf;

#[test]
fn level19_process8_prepares_authored_ambient_and_fails_closed_without_host() {
    let explicit = std::env::var_os("OPENCHASM_TEST_CONTENT");
    let path = explicit
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("assets/csm.bin"));
    if !path.exists() && explicit.is_none() {
        return;
    }
    let mut archive = archive::Archive::open(&path).unwrap();
    let bytes = archive.read("map/process.19").unwrap();
    let script = Script::parse(std::str::from_utf8(&bytes).unwrap());
    let world = World {
        walls: vec![],
        player: Player {
            position: [0.5, 0.5],
            angle: 0.0,
        },
    };
    let mut runtime = ProcessRuntime::with_staged_actions(
        &script,
        &world,
        ProcessCapabilities::default(),
        &[0; 4096],
        &[],
    )
    .unwrap();
    let prepared = runtime.prepare_at("link", [8, 32], 8).unwrap();
    assert!(prepared
        .actions()
        .contains(&ImmediateAction::Ambient(AmbientRegion {
            min_cell: [36, 18],
            max_cell: [43, 22],
            sound_id: 0
        })));
    assert_eq!(prepared.commit().outcome, ProcessOutcome::Unsupported);
    assert!(runtime.take_effects().is_empty());
}
