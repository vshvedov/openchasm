//! Optional installed-data snapshot check; this activates switches directly and
//! does not claim a complete walkable route.
#[allow(dead_code)]
#[path = "../src/archive.rs"]
mod archive;
use openchasm::{
    map::{Map, MAP_SIDE, UNITS_PER_TILE},
    process::Script,
    process_runtime::{ProcessGeometryError, ProcessOutcome, ProcessRuntime},
    world::{Player, Wall, World},
};
use std::path::PathBuf;

#[test]
fn installed_castle_shared_pivot_doors_restore_after_either_switch() {
    let explicit = std::env::var_os("OPENCHASM_TEST_CONTENT");
    let path = explicit
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("assets/csm.bin"));
    if !path.exists() && explicit.is_none() {
        return;
    }
    let mut archive = archive::Archive::open(&path).unwrap();
    let map = Map::parse(&archive.read("map/map.03").unwrap()).unwrap();
    let data = archive.read("map/process.03").unwrap();
    let script = Script::parse(std::str::from_utf8(&data).unwrap());
    let mut walls = Vec::new();
    for x in 0..MAP_SIDE {
        for y in 0..MAP_SIDE {
            let wall = map.wall(x, y).unwrap();
            if wall.is_wall() && wall.start != wall.end {
                let cv = |p: [u16; 2]| {
                    [
                        f32::from(p[0]) / UNITS_PER_TILE,
                        f32::from(p[1]) / UNITS_PER_TILE,
                    ]
                };
                walls.push(Wall {
                    a: cv(wall.start),
                    b: cv(wall.end),
                    texture: wall.texture_id,
                    blocks_movement: true,
                    cell: Some([x as u8, y as u8]),
                });
            }
        }
    }
    let world = World {
        walls,
        player: Player {
            position: [-100.0, -100.0],
            angle: 0.0,
        },
    };
    let baseline = ProcessRuntime::new(&script, &world);
    for (cell, id) in [
        ([26, 28], 17),
        ([26, 25], 18),
        ([26, 42], 24),
        ([26, 43], 25),
    ] {
        let mut runtime = baseline.clone();
        let mut moved = world.clone();
        assert!(runtime
            .activate_at("link", cell)
            .iter()
            .any(|e| e.process_id == id && e.outcome == ProcessOutcome::Accepted));
        runtime.advance_motions(&mut moved, 0.1);
        let snapshot = runtime.snapshot(&moved).unwrap();
        let mut restored = baseline.clone();
        restored.restore(&snapshot, &moved).unwrap();
        assert_eq!(
            restored.validate_snapshot_geometry(&baseline, &world, &moved),
            Ok(())
        );
        let wall = moved
            .walls
            .iter_mut()
            .find(|w| match id {
                17 | 18 => w.cell == Some([26, 26]),
                _ => w.cell == Some([27, 43]),
            })
            .unwrap();
        wall.a[0] += 0.1;
        assert_eq!(
            restored.validate_snapshot_geometry(&baseline, &world, &moved),
            Err(ProcessGeometryError::PoseMismatch)
        );
    }
}
