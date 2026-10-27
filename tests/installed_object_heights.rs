//! Optional collision-height check against an owned full-game archive.
#[allow(dead_code)]
#[path = "../src/archive.rs"]
mod archive;
#[allow(dead_code)]
#[path = "../src/model.rs"]
mod model;
#[allow(dead_code)]
#[path = "../src/object_heights.rs"]
mod object_heights;
use std::path::PathBuf;

#[test]
fn level17_teleport_roof_is_above_player_height() {
    let explicit = std::env::var_os("OPENCHASM_TEST_CONTENT");
    let path = explicit
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(|| "assets/csm.bin".into());
    if !path.exists() && explicit.is_none() {
        eprintln!("No archive; roof-height test skipped.");
        return;
    }
    let mut archive = archive::Archive::open(&path).unwrap();
    let model = model::Model::parse(&archive.read("model/18_cl2.3o").unwrap()).unwrap();
    let bounds = object_heights::HeightBounds::from_3o(&model).unwrap();
    assert_eq!(bounds.bottom, 2.0);
    assert_eq!(bounds.top, 9626.0 / 2048.0);
    let mut catalog = object_heights::ObjectHeightCatalog::new();
    assert!(catalog.set(165, bounds));
    assert!(!catalog.overlaps(165, 0.0, 0.0, 0.9));
}
