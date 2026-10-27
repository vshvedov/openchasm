//! Optional check against the user's installed game data; no assets are copied.
#[allow(dead_code)]
#[path = "../src/archive.rs"]
mod archive;
use openchasm::{
    actors::ActorState,
    combat::RocketDefinitions,
    projectiles::{Launch, Owner, ProjectileSystem},
};
use std::path::PathBuf;

#[test]
fn installed_physical_rockets_preserve_authored_speed_at_pitch() {
    let explicit = std::env::var_os("OPENCHASM_TEST_CONTENT");
    let path = explicit
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("assets/csm.bin"));
    if !path.exists() && explicit.is_none() {
        return;
    }
    let mut archive = archive::Archive::open(&path).unwrap();
    let definitions = RocketDefinitions::parse(&archive.read("chasm.inf").unwrap()).unwrap();
    let actors = ActorState {
        player_spawns: vec![],
        actors: vec![],
        unresolved: vec![],
    };
    let mut checked = 0;
    for (rocket_id, rocket) in definitions.0.iter().enumerate() {
        if rocket.model_name.is_empty() {
            continue;
        }
        let mut system = ProjectileSystem::new();
        let Ok(id) = system.spawn(
            Launch {
                owner: Owner::Player,
                rocket_id,
                origin: [1.0, 1.0],
                direction: [1.0, 0.0],
            },
            &definitions,
        ) else {
            continue;
        };
        let z = system.apply_launch_pitch(id, 0.7).unwrap();
        let projectile = &system.active()[0];
        let speed = if rocket.raw_fields.get(10).copied().unwrap_or(0) != 0 {
            20.0
        } else {
            10.0
        };
        assert!(
            (projectile.velocity[0]
                .hypot(projectile.velocity[1])
                .hypot(z)
                - speed)
                .abs()
                < 1e-4,
            "rocket {rocket_id}"
        );
        let snapshot = system.snapshot().unwrap();
        let mut restored = ProjectileSystem::new();
        restored
            .restore_with_vertical(&snapshot, &definitions, &actors, &[(id, z)])
            .unwrap();
        assert_eq!(restored.snapshot().unwrap(), snapshot);
        checked += 1;
    }
    assert!(checked > 0);
}
