//! Optional checks against owned Steam data; set OPENCHASM_TEST_CONTENT to csm.bin.
#[allow(dead_code)]
#[path = "../src/ambient_definitions.rs"]
mod ambient_definitions;
#[allow(dead_code)]
#[path = "../src/archive.rs"]
mod archive;

use ambient_definitions::AmbientDefinitions;
use std::path::PathBuf;

#[test]
fn installed_level_ambient_definitions() {
    let explicit = std::env::var_os("OPENCHASM_TEST_CONTENT");
    let path = explicit
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("assets/csm.bin"));
    if !path.exists() && explicit.is_none() {
        return;
    }
    let mut archive = archive::Archive::open(&path).unwrap();
    for level in 1..=19 {
        let bytes = archive.read(&format!("map/resource.{level:02}")).unwrap();
        let defs =
            AmbientDefinitions::parse(&bytes).unwrap_or_else(|e| panic!("level {level}: {e}"));
        if level == 1 {
            let rain = defs.entries[4].as_ref().unwrap();
            assert_eq!(rain.resource_path().as_deref(), Some("sound/mix_rain.wav"));
            assert_eq!((rain.volume, rain.background_sound), (40, Some(0)));
            assert_eq!(defs.entries[1].as_ref().unwrap().file_name, None);
        }
        if level == 19 {
            assert_eq!(
                defs.entries[0].as_ref().unwrap().resource_path().as_deref(),
                Some("sound/8_amb1.wav")
            );
            let wind = defs.entries[8].as_ref().unwrap();
            assert_eq!(
                (wind.resource_path().as_deref(), wind.volume),
                (Some("sound/wind_amb.wav"), 86)
            );
        }
    }
}
