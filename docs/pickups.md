# Pickup overlap

The headless pickup state resolves item action codes from level object definitions and updates the player's inventory on horizontal overlap. Item collection emits a map-cell event for presentation and future script hooks. Full or unsupported items remain in the level. The initial behavior filters the three difficulty bits and uses overlap radii documented in the independent PanzerChasm reconstruction: 100/256 tiles for items, and 60/256 tiles plus object radius for keys. These distances and difficulty behavior still require comparison against the original.

This subsystem does not yet handle vertical reach, pickup respawns, multiplayer ownership, movement of items by map processes, sound, or map scripts that react to collection. The viewer consumes pickup events to hide collected objects, display inventory values, and provide keys to the process runtime. See [inventory.md](inventory.md) for item effect assumptions.
