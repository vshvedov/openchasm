# Campaign progression audit

Installed-data tests exercise individual campaign mechanisms with owned Steam content. An accepted process or passing scenario does not establish a complete playable route: several tests place the player at a trigger, suppress enemies, or provide presentation manifests explicitly.

- Level 01: a no-teleport session walk reaches the green key, passes its door and destroys a window. Other tests activate the power-switch and door logic with controlled positions. A complete route to the power switch and exit remains unverified.
- Level 02: a no-teleport walk reaches a nearby switch. Separate controlled-position tests collect the green key and activate its exit approach; complete traversal remains unverified.
- Level 03: real paired door rotations restore from snapshots when they share a pivot. This is a persistence test, not a progression route.
- Level 08: an installed-data scenario exercises the staged destruction and replacement of linked targets. It does not establish full traversal.
- Level 09: the real process 20/21 scenario covers the reconstructed `call` handling and door action; `call` is treated as a documented no-op pending stronger evidence.
- Level 12: real process 27 executes its object replacement and wall motion and unlocks process 29. The test supplies clearance and keys; full route traversal remains unverified. Source-light lifecycle and snapshot restoration are separately tested.
- Level 16: one controlled-start session follows the two authored boss-area teleports to the heart area, including landing on the real 0.875-tile platform and a snapshot round trip. Another destroys the heart using its authored 300 health and the first weapon's 25 damage, commits replacements, walks into the unlocked exit, displays its message and completes the timer. Both suppress enemies and explicitly preload replacement presentation; they do not establish full-level traversal. The destination platform height comes from installed 3O vertices. Standing, collision clearance and platform pull approximate the independent reconstruction and still need direct comparison with the original executable.
- Level 18: a real process 2 session scenario changes a wall material and moves it, checking collision and failure rollback. This does not establish a route to the exit.
- Level 19: an isolated installed-data encounter reflects the boss projectile and reaches mission completion; it does not establish a full playable route.

An older initial-state scan reported unsupported actions and exit acceptance, but subsequent runtime changes made its counts stale. These targeted tests supersede those counts. The remaining mandatory paths, full enemy encounters, and dynamic geometry still need end-to-end validation; unsupported process actions and approximated movement may prevent progression elsewhere.
