# Provisional projectile simulation

The projectile module maintains moving 2D projectiles separately from graphics, audio, inventory, AI, and networking. A launch identifies the rocket definition, owner, origin, and direction. Only rockets with a nonempty model name can be launched; the installed modding manual identifies empty-model rockets as instant, and those remain the responsibility of hitscan combat. Launch validates finite coordinates, direction, power and collision radius, and caps active projectiles at 4,096.

Each projectile advances over a caller-supplied simulation timestep until it hits or its 10-second lifetime expires. Continuous swept-circle tests use the rocket's `CRd` field as a collision radius in map units divided by 256 (capped at two tiles). They choose the first blocking wall, living actor, or eligible player; the caller supplies the wall shot-collision predicate. An enemy-owned projectile ignores its source actor and may hit the player or other actors. A player-owned projectile ignores the player and may hit actors. Friendly-fire rules have not been verified. Actors use the same provisional circular hit radius as hitscan combat; the player's current preview collision radius is 0.18 tiles.

The system reports `Impact` events in time-of-impact then projectile-ID order. Events include owner, rocket ID, target, position, and the rocket's `Pwr` value. It does not change health: the authoritative session decides how to apply damage, armor and effects. An expiry event has zero damage. Callers should apply events before using the next simulation state. A renderer can inspect the current `active()` slice.

This is an initial approximation, not the original game's projectile behavior. Speed follows the Fast field: 10 tiles per second normally or 20 when Fast is set, following [PanzerChasm’s reconstruction](https://github.com/Panzerschrek/Chasm-Reverse/blob/master/src/game_constants.hpp); comparison against the original game remains necessary. Lifetime, circle radii, friendly-fire behavior and hit ordering need comparison with the game. The legacy horizontal path ignores gravity and height. Neither path models ricochets, moving-wall sweeps, moving actor sweeps, inventory and reload rules, or replication. Current actor and wall positions are sampled once per tick. The system does not model physical grenades, mines or homing correctly simply because it can move their definitions; those behaviors require separate implementation.

Synthetic tests cover non-instant launch validation, delayed movement, actor and player impacts, owner exclusion, wall occlusion and pass-through, wall endpoint collision, long-tick tunneling, event order and lifetime expiry. A standalone check against the installed archive successfully launched and expired its 17 nonempty-model rocket definitions and rejected its four instant definitions. Those checks establish parser and simulation behavior, not game fidelity.

Session player shots fired with nonzero pitch use an additional height and vertical speed, traced continuously against provisional two-tile walls, actor cylinders, object model heights and flat floor/ceiling planes. Plane contact emits `ImpactTarget::Plane`; impact height is emitted alongside the impact and used for splash. New shots use a normalized 3D launch direction at 10/20 tiles per second, matching the independent reconstruction; legacy snapshots retain their original horizontal-speed behavior. Enemy shots use a provisional 0.5-tile muzzle and aim at the player’s current feet plus 0.5 tiles, including while jumping. The reconstruction has per-monster muzzle offsets that are not yet modeled. Gravity, bounce and moving-target sweeps are not modeled. Legacy horizontal projectiles without vertical state retain the earlier 2D path. Pitch and vertical speed survive validated in-memory snapshots.

When a host configures `Session::set_authored_shot_planes` with the level map,
pitched hitscan and physical projectile traces test the floor or ceiling tile
at the actual XY plane contact. Tiles 62 and 63 and invalid texture IDs are
open, consistent with the current plane renderer. Unconfigured sessions keep
the legacy solid flat room; positions outside the map remain conservative.
Planes still have provisional fixed heights z=0 and z=2, while player support
and platform collision are separate. This does not yet model sloped or dynamic
floor and ceiling geometry, projectile gravity, or bounce. Save restoration
checks that the baseline enables the same plane mode; the host must fingerprint
the source map and configure the mode before recording its baseline.
