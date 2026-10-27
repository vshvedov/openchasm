//! Optional, read-only compatibility checks against a locally owned archive.
//! CI does not need proprietary assets; set OPENCHASM_TEST_CONTENT to run them.
#[allow(dead_code)]
#[path = "../src/archive.rs"]
mod archive;
#[allow(dead_code)]
#[path = "../src/map.rs"]
mod map;
#[allow(dead_code)]
#[path = "../src/model.rs"]
mod model;
#[allow(dead_code)]
#[path = "../src/texture.rs"]
mod texture;

use std::path::PathBuf;

#[test]
fn installed_archive_parses_game_data() {
    let explicit = std::env::var_os("OPENCHASM_TEST_CONTENT");
    let path = explicit
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("assets/csm.bin"));
    if !path.exists() && explicit.is_none() {
        eprintln!("No local archive: corpus verification skipped. Set OPENCHASM_TEST_CONTENT to enable it.");
        return;
    }
    let mut archive = archive::Archive::open(&path).expect("open installed resource archive");
    let info = archive.read("chasm.inf").expect("read game definitions");
    let weapons =
        openchasm::inventory::WeaponDefinitions::parse(&info).expect("parse weapon definitions");
    let mut inventory = openchasm::inventory::Inventory::new(&weapons);
    assert_eq!(inventory.ammo[0], weapons.0[0].ammo_limit);
    let pickup = inventory.pickup(22, &weapons);
    assert_eq!(
        pickup,
        openchasm::inventory::PickupOutcome::Collected(openchasm::inventory::Pickup::Ammo(1))
    );
    assert_eq!(
        inventory.ammo[1],
        weapons.0[1]
            .ammo_per_portion
            .saturating_mul(2)
            .min(weapons.0[1].ammo_limit)
    );
    let definitions =
        openchasm::actors::ActorDefinitions::parse(&info).expect("parse actor definitions");
    let names: Vec<String> = archive.names().map(str::to_owned).collect();
    let mut maps = 0;
    let mut textures = 0;
    let mut models = 0;
    let mut animations = 0;
    let mut monsters = 0;
    for name in names {
        if let Some(id) = name.strip_prefix("map/map.") {
            let bytes = archive.read(&name).unwrap();
            let map = map::Map::parse(&bytes).unwrap_or_else(|e| panic!("{name}: {e}"));
            let shared_map = openchasm::map::Map::parse(&bytes).unwrap();
            let actors =
                openchasm::actors::ActorState::from_placements(&shared_map.monsters, &definitions);
            assert!(
                actors.unresolved.is_empty(),
                "{name} contains unresolved actor IDs"
            );
            assert!(
                map.monsters
                    .iter()
                    .any(|m| m.is_player_spawn() && m.flags == 0),
                "{name} has no default spawn"
            );
            let floors_name = format!("map/floors.{id}");
            let floors = archive.read(&floors_name).unwrap();
            map::Floors::parse(&floors).unwrap_or_else(|e| panic!("{floors_name}: {e}"));
            maps += 1;
        } else if name.starts_with("texture/") && name.ends_with(".cel") {
            let bytes = archive.read(&name).unwrap();
            texture::CelImage::parse(&bytes).unwrap_or_else(|e| panic!("{name}: {e}"));
            textures += 1;
        } else if name.starts_with("model/") && name.ends_with(".3o") {
            let bytes = archive.read(&name).unwrap();
            model::Model::parse(&bytes).unwrap_or_else(|e| panic!("{name}: {e}"));
            models += 1;
        } else if name.starts_with("monster/") && name.ends_with(".car") {
            let bytes = archive.read(&name).unwrap();
            openchasm::monster::Monster::parse(&bytes).unwrap_or_else(|e| panic!("{name}: {e}"));
            monsters += 1;
        } else if name.starts_with("model/") && name.ends_with(".ani") {
            let bytes = archive.read(&name).unwrap();
            model::Animation::parse(&bytes).unwrap_or_else(|e| panic!("{name}: {e}"));
            animations += 1;
        }
    }
    assert!(
        maps > 0 && textures > 0 && models > 0 && animations > 0 && monsters > 0,
        "archive missing expected content categories"
    );
    eprintln!("Validated {maps} maps, {textures} textures, {models} models and {animations} animations and {monsters} monster models in {}", path.display());
}
