//! Optional check of all teleport definitions in an owned Steam archive.
#[allow(dead_code)]
#[path = "../src/archive.rs"]
mod archive;
#[allow(dead_code)]
#[path = "../src/teleport.rs"]
mod teleport;
use std::path::PathBuf;

#[test]
fn campaign_teleports_parse_and_preserve_expansion_flags() {
    let explicit = std::env::var_os("OPENCHASM_TEST_CONTENT");
    let path = explicit
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(|| "assets/csm.bin".into());
    if !path.exists() && explicit.is_none() {
        eprintln!("No archive; installed teleport test skipped.");
        return;
    }
    let mut archive = archive::Archive::open(&path).unwrap();
    let mut total = 0;
    let mut extra = 0;
    for level in 1..=19 {
        let text = archive.read(&format!("map/process.{level:02}")).unwrap();
        let rows = teleport::parse(std::str::from_utf8(&text).unwrap()).unwrap();
        if level == 8 {
            assert_eq!(rows.len(), 3);
            assert_eq!(rows[0].source(), [16.5, 44.5]);
            assert_eq!(rows[0].destination(), [5.5, 32.5]);
            assert_eq!(rows[2].destination(), [2220.0 / 256.0, 28.0]);
        }
        if level == 17 {
            assert_eq!(rows.len(), 6);
            assert!(rows.iter().all(|row| row.extra == Some(2)));
        }
        total += rows.len();
        extra += rows.iter().filter(|row| row.extra.is_some()).count();
    }
    assert_eq!((total, extra), (76, 6));
}
