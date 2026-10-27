# Process wind fields

`WindField` is a renderer- and transport-independent field. A procedure activates one or more `WindZone` rectangles under its process ID, and the host deactivates them when that process's wind effect ends or is locked. Rectangles are inclusive 64×64 map-cell coordinates; components retain their authored signed integer values. Activation validates every zone before changing any cell, applies zones in input order, and rejects duplicate activation of an already active process. Deactivation clears all cells in that process's authored rectangles.

Overlapping regions follow the published PanzerChasm implementation: activation overwrites the grid, and deactivation clears cells even when another active process had subsequently written there. This is not a layered or restoring model; the correctness of overlap behavior still needs confirmation against the 2022 game. The subsystem does not decide when `nonstop` ends, parse process commands, move actors, or emit process events.

`sample_actor` and `actor_displacement` query the actor grid; `sample` and `displacement` query the player grid. `sample(position)` returns the interpolated raw vector at a tile-space position. It uses the four nearest field-cell centers and the reference's truncation and boundary behavior. `displacement(position, dt)` multiplies that vector by 0.5 and the supplied seconds. The reference applies this adjustment to monsters and players before subsequent map collision. The host must apply the resulting movement through collision handling rather than teleporting an entity, and should advance simulation on its authoritative fixed tick.

The algorithm and 0.5 factor are based on [PanzerChasm's ProcessWind and monster update](https://github.com/Panzerschrek/Chasm-Reverse/blob/master/src/server/map.cpp). Their comments describe interpolation as approximate; neither the original engine's units nor edge handling have been verified. Signed field values are limited to the reference's signed-byte range and invalid inputs fail without altering state.

The bundled 2022 modding manual documents the optional seventh field: value 1 ignores players. The remaster executable independently checks this flag before applying player wind. Level 12 process 26 uses `wind 34 31 34 32 10 0 1` to affect the Phantom while leaving players unaffected. Normal and actor-only zones use separate player and actor grids; actor-only activation and clearing do not touch the player grid. Overlapping zones overwrite, and clearing a rectangle does not restore prior vectors. This is the existing grid approximation: remaster code evaluates actions per entity and changes player velocity only when the wind is stronger, so overlapping and motion semantics are not yet faithful.

The session consumes process wind activation and deactivation effects. Wind
active at the start of a 60 Hz tick is combined with player input in one swept
collision move, then applied to living actors in stable actor order using their
provisional wall radii. Actors collide with walls, the player, other living
actors, and represented blocking process objects. A wind activated during the
process update starts moving entities on the next tick. Invalid activation
errors are exposed in `SessionEvents::wind_errors`. The existing 2D wall and
object clearance rules are approximations; actor wind uses the same current
wall blocking state as player movement, pending actor-specific vertical physics.
