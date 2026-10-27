//! Optional checks against owned Steam data.
#[allow(dead_code)]
#[path = "../src/archive.rs"]
mod archive;
use openchasm::sound_definitions::SoundDefinitions;
use std::path::PathBuf;

#[test]
fn installed_global_and_all_level_sound_tables_parse() {
    let explicit = std::env::var_os("OPENCHASM_TEST_CONTENT");
    let path = explicit
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("assets/csm.bin"));
    if !path.exists() && explicit.is_none() {
        return;
    }
    let mut archive = archive::Archive::open(&path).unwrap();
    let global = archive.read("chasm.inf").unwrap();
    for level in 1..=19 {
        let data = archive.read(&format!("map/resource.{level:02}")).unwrap();
        let defs = SoundDefinitions::parse(&global, &data)
            .unwrap_or_else(|error| panic!("level {level}: {error}"));
        assert_eq!(
            defs.get(70).unwrap().resource_path().as_deref(),
            Some("sound/glass3.wav")
        );
        if level == 2 {
            assert_eq!(
                defs.get(81).unwrap().resource_path().as_deref(),
                Some("sound/metal_d.wav")
            );
            assert_eq!(defs.get(81).unwrap().volume, 96);
        }
        if level == 5 {
            assert_eq!(
                defs.get(91).unwrap().resource_path().as_deref(),
                Some("sound/lighturn.wav")
            );
        }
    }
}
