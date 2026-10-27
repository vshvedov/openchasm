# Field and teleport snapshot components

`WindField`, `Hazards`, `AmbientGrid`, `QuakeField`, and `TeleportState` provide typed `snapshot` and validated `restore` methods. The caller should restore these into a temporary session and publish only after validating all components together.

Wind snapshots retain both actual player and actor grids as well as active process zones. Reconstructing the grids from active zones is incorrect: overlapping fields overwrite one another, and deactivation clears cells without restoring earlier values. Restore validates grid sizes, vector ranges, active rectangles, and duplicate process IDs. It does not prove that a grid could have resulted from the supplied zones or match them against process definitions.

Hazard snapshots retain each active process's zones and its damage tick counter. Restore validates rectangle and vertical bounds, the counter range, and process uniqueness; the aggregate loader must compare active zones against the process runtime and loaded content. Ambient snapshots retain all raw cell values, including unusual values inherited from authored map data. Quake snapshots preserve the whole raw power grid. Both reject incorrect grid sizes. Teleport snapshots preserve the selected rearm policy and waiting-for-exit flag; continuous policy cannot be waiting. The owning session must validate its enabled/capability settings and content identity separately.

These APIs do not define a save-file encoding, persist files, or validate cross-component consistency. The process runtime's active flags, hazard and wind ownership, player location, collision, and teleporter definitions must be validated together before a restored session is exposed.
