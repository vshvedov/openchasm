//! Optional checks against the owned Steam archive; set OPENCHASM_TEST_CONTENT
//! to its csm.bin path when it is not available at assets/csm.bin.
#[allow(dead_code)]
#[path = "../src/archive.rs"]
mod archive;

use openchasm::{
    process::Script,
    process_runtime::{ProcessCapabilities, ProcessEffect, ProcessOutcome, ProcessRuntime},
    wind::{WindField, WindZone},
    world::{Player, World},
};
use std::path::PathBuf;

#[test]
fn level_12_floor_trigger_starts_authored_nonstop_wind() {
    let explicit = std::env::var_os("OPENCHASM_TEST_CONTENT");
    let path = explicit
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("assets/csm.bin"));
    if !path.exists() && explicit.is_none() {
        eprintln!("No local archive: installed wind check skipped. Set OPENCHASM_TEST_CONTENT to enable it.");
        return;
    }
    let mut archive = archive::Archive::open(&path).expect("open game archive");
    let data = archive
        .read("map/process.12")
        .expect("read level 12 script");
    let script = Script::parse(std::str::from_utf8(&data).expect("UTF-8 script"));
    assert!(script.diagnostics.is_empty(), "{:?}", script.diagnostics);
    let mut world = World {
        walls: vec![],
        player: Player {
            position: [-100.0, -100.0],
            angle: 0.0,
        },
    };
    let mut runtime = ProcessRuntime::with_wind(
        &script,
        &world,
        ProcessCapabilities::default(),
        &[0; 4096],
        &[],
    )
    .unwrap();
    let attempts = runtime.activate_at("floor", [23, 48]);
    assert!(
        attempts
            .iter()
            .any(|event| event.process_id == 11 && event.outcome == ProcessOutcome::Accepted),
        "{attempts:?}"
    );
    let effects = runtime.take_effects();
    let zones = effects
        .iter()
        .find_map(|event| match event {
            ProcessEffect::WindActivated {
                process_id: 11,
                zones,
            } => Some(zones),
            _ => None,
        })
        .expect("level 12 process 11 must activate wind");
    assert_eq!(
        zones,
        &[WindZone {
            min_cell: [54, 54],
            max_cell: [58, 54],
            direction: [0, 16],
            actor_only: false,
        }]
    );
    let mut field = WindField::new();
    field.activate(11, zones).unwrap();
    assert_eq!(field.cell([56, 54]), Some([0, 16]));
    let movement = field.displacement([56.5, 54.5], 1.0).unwrap();
    assert_eq!(movement[0], 0.0);
    assert!(movement[1] > 0.0, "authored positive Y wind: {movement:?}");
    for _ in 0..180 {
        runtime.tick(&mut world, 1.0 / 60.0, false);
        assert!(!runtime
            .take_effects()
            .iter()
            .any(|event| matches!(event, ProcessEffect::WindDeactivated { process_id: 11 })));
    }
    assert!(field.is_active(11), "nonstop wind persists after opening");
}

#[test]
fn level_12_process_26_authored_actor_only_wind() {
    let explicit = std::env::var_os("OPENCHASM_TEST_CONTENT");
    let path = explicit
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("assets/csm.bin"));
    if !path.exists() && explicit.is_none() {
        return;
    }
    let mut archive = archive::Archive::open(&path).unwrap();
    let data = archive.read("map/process.12").unwrap();
    let script = Script::parse(std::str::from_utf8(&data).unwrap());
    let wind = script.processes[&26]
        .actions
        .iter()
        .find(|action| action.name == "wind")
        .unwrap();
    assert_eq!(wind.arguments, ["34", "31", "34", "32", "10", "0", "1"]);
    // Isolate the real authored command so other process 26 motions do not
    // require a complete map and object scene in this wind-specific test.
    let isolated = Script::parse(&format!(
        "#proc26\n#action\nwind {}\nnonstop\n#end\n#links\nfloor 30 31 26\n#end\n",
        wind.arguments.join(" ")
    ));
    let world = World {
        walls: vec![],
        player: Player {
            position: [-100.0, -100.0],
            angle: 0.0,
        },
    };
    let mut runtime = ProcessRuntime::with_wind(
        &isolated,
        &world,
        ProcessCapabilities::default(),
        &[0; 4096],
        &[],
    )
    .unwrap();
    assert_eq!(
        runtime.activate_at("floor", [30, 31])[0].outcome,
        ProcessOutcome::Accepted
    );
    let effects = runtime.take_effects();
    let zones = effects
        .iter()
        .find_map(|effect| match effect {
            ProcessEffect::WindActivated {
                process_id: 26,
                zones,
            } => Some(zones),
            _ => None,
        })
        .unwrap();
    assert!(zones[0].actor_only);
    let mut field = WindField::new();
    field.activate(26, zones).unwrap();
    assert_eq!(field.displacement([34.5, 31.5], 1.0), Some([0.0, 0.0]));
    assert_eq!(
        field.actor_displacement([34.5, 31.5], 1.0),
        Some([5.0, 0.0])
    );
}
