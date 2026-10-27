# Development priorities

The goal remains a fully playable cross-platform Rust reimplementation using owned demo/full resources, with future original content and authoritative internet multiplayer. The current build is a preview, not a completed campaign or a verified 1:1 clone.

The latest user priorities are the original game's look, enemy animation, blocked first-map routes, aspect ratio and smooth performance. A visible FPS counter and a Quake-style tilde console are implemented and have passed live Linux input checks. Release profiling found a render bottleneck, with measured improvements from about 36 to 39 FPS after removing sky overdraw and about 50 FPS with near-first wall shading at the verified 75-degree FOV. Recent commits correct projection scale, verified FOV and a mirrored camera basis, add authored lighting, repair monster timing/firing meshes, fix open-sky shots and a level-16 teleport, and extend the installed-data early-level route checks. See [fidelity audit](fidelity-audit.md) and [performance measurements](performance.md) for evidence and limits.

## Current gameplay priority

The latest manual report finds slow movement/mouselook and incorrect enemy
attack/facing behavior. Verify the original movement equations and mouse
coefficient, add running with autorun enabled by default, and expose an
independent Quake 1 movement option. Gameplay mode and velocity must belong to
authoritative simulation and snapshots. Diagnose installed enemy encounters,
including mesh orientation, acquisition and committed attacks. These gameplay
fixes take priority over further GPU optimization.

## Faithful baseline

- Calibrate lighting and palette/colour response against controlled original-game views. Executable analysis has now verified the 75-degree vertical world FOV, separate weapon FOV and sRGB/HDR/ACES chain. The projection defaults and linear HDR colour chain are implemented; generated lighting, static shadows and distance response are implemented from executable evidence, while precision, sampling/coverage details, model coordinates and transient sources still need calibration.
- Verify enemy run, idle, ranged, melee, pain and death behavior against owned animations; finish missing reactions and detached parts.
- Extend first-map progression tests using the same height, wall-material and collision setup as the client. Controlled tests without enemies do not prove a full route.
- Measure render, simulation and presentation time in release builds, then improve actual hot paths and frame pacing. Retain fixed authoritative ticks while improving presentation smoothness.
- Preserve local and native-platform validation, atomic commits and asset-free public history.

## Optional visual enhancements

The first selectable enhanced presentation adds bloom to the linear HDR path, alongside independently adjustable tone mapping and colour controls. Continue the remaining fidelity and performance corrections while extending optional enhancements. Options should be independently adjustable and carry explicit performance budgets:

1. Render resolution/scale, filtering, FOV, frame limit and vsync.
2. Richer particles, projectile/explosion lighting and configurable effect density.
3. Bloom and colour controls with a documented linear/sRGB pipeline, separate from the HUD.
4. Optional dynamic shadows and other lighting improvements once measured and supported across platforms.

A GPU backend is a candidate for true rotated-camera perspective, shared translucent composition and more expensive effects. Keep simulation and resource identity independent of graphics APIs; extract render snapshots incrementally and validate Linux, macOS and Windows surface/device behavior. The existing software renderer remains useful for fallback and comparison. The initial enhanced profile is a small bloom option, not a completed modern renderer. An optional GPU post/bloom/UI backend is implemented; genuine 3D world rendering remains planned.

## Remaining game work

Continue campaign progression, independent multi-pellet weapons, projectile gravity/bounce/homing, collision calibration, persistent saves, original presentation/audio and end-to-end playthroughs. The simulation/input separation supports future multiplayer architecture, but networking and cross-platform deterministic behavior are not implemented or proven.
