//! Optional headless check of clip metadata in an owned game archive.
#[allow(dead_code)]
#[path = "../src/archive.rs"]
mod archive;
use openchasm::{actor_clips::ActorClipSet, monster::Monster};
use std::path::PathBuf;

#[test]
fn installed_early_enemies_expose_authored_attack_clips() {
    let explicit = std::env::var_os("OPENCHASM_TEST_CONTENT");
    let path = explicit
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("assets/csm.bin"));
    if !path.exists() && explicit.is_none() {
        eprintln!("No owned archive; actor clip check skipped.");
        return;
    }
    let mut archive = archive::Archive::open(&path).expect("open selected archive");
    for (name, ranged) in [("mongf", true), ("spider", false), ("wman", true)] {
        let car = Monster::parse(
            &archive
                .read(&format!("monster/{name}.car"))
                .expect("read CAR"),
        )
        .expect("parse CAR");
        let clips = ActorClipSet::from_car(&car);
        assert!(clips.bounds_verified);
        assert!(clips.clips[0].is_some(), "{name} run");
        assert_eq!(clips.clips[5].is_some(), ranged, "{name} ranged slot");
        assert!(
            (6..=8).any(|slot| clips.clips[slot].is_some()),
            "{name} melee slots"
        );
        for (slot, animation) in car.animations.iter().enumerate() {
            assert_eq!(
                clips.clips[slot].map(|c| c.frame_count as usize),
                animation.as_ref().map(|a| a.frames.len())
            );
        }
    }
    let table = archive
        .read("monster/faust.gltf.ani")
        .expect("read Faust clip table");
    let clips = ActorClipSet::parse_gltf(&table, None).expect("parse Faust metadata");
    assert!(!clips.bounds_verified);
    assert_eq!(clips.clips[5].unwrap().frame_count, 16);
    assert_eq!(clips.clips[6].unwrap().frame_count, 18);
    assert_eq!(clips.clips[7].unwrap().frame_count, 22);
    assert_eq!(clips.clips[8].unwrap().frame_count, 22);
}
