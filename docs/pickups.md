# Pickup overlap and visibility

The pickup subsystem is independent of rendering and networking. It consumes map object placements and definition action codes, and emits collection events identified by the object's grid cell. The authoritative caller updates the inventory and pickup state, hides collected cells in presentation, and may later use the event to trigger map links and sounds. Full or unsupported pickups remain visible and can be retried.

Single-player difficulty uses the placement's low bits: 1 for easy, 2 for normal, 4 for hard. Disabled candidates start hidden. Script enable/disable is tracked separately from difficulty and cannot resurrect collected items. Positions can be updated by map procedures, but the current caller must do so explicitly.

Ordinary items use a horizontal pickup distance of 100/256 map tiles; level-object keys use 60/256 tiles plus the object's GoRad radius. These values and the horizontal-only overlap check follow the independent [PanzerChasm map simulation](https://github.com/Panzerschrek/Chasm-Reverse/blob/master/src/server/map.cpp) and [game constants](https://github.com/Panzerschrek/Chasm-Reverse/blob/master/src/game_constants.hpp). They remain assumptions pending comparison to the original game. Level objects with keys are eligible; other level objects, including switches, are not automatically picked up. Deathmatch filtering and respawn timers, vertical overlap, moving platform heights, and linked-procedure events are not yet implemented.

Tests cover pickup boundaries, full inventory, unsupported items, difficulty, scripts, moving objects and invalid coordinates. A local check traversed placement positions in all 20 installed maps and produced 351 supported collection events; it is a smoke check, not proof of gameplay fidelity.

The preview consumes pickup events to hide collected objects, play pickup sounds, display inventory values, and provide keys to the process runtime. Linked processes at a collected pickup cell are activated in script order; other pickup-related script behaviors remain incomplete.
