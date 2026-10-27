# Quake region state

`quake.rs` provides `QuakeZone` and a headless `QuakeField`. The field stores
unscaled authored power per cell and supports atomic, ordered `apply_batch`
and cell/position lookup. A later rectangle provisionally overwrites prior
powers; zero clears the covered cells. The host must parse signed script
arguments without truncation, reject invalid coordinates or negative powers,
and decide exactly when to apply actions. The module does not provide a camera
shake, audio, random numbers, collision changes, or automatic process cleanup.

The bundled [CSM_EDIT.TXT](https://github.com/Panzerschrek/Chasm-Reverse/blob/master/docs/CSM_EDIT.TXT)
describes `quake X1 Y1 X2 Y2 PWR` as vibration within a rectangle and says it
needs `nonstop`, but does not specify power units, overlap, coordinate boundary,
or lifetime rules. The reconstructed PanzerChasm engine recognizes the command
but does not implement quake zones; its task list identifies them as unfinished.
This state is therefore a working hypothesis, not a faithful reconstruction.

The installed full-game level 16 links both processes 7 and 8 to destruction
of the heart at (27,47). Process 7 has zero start delay and sets `(17,40)` to
`(30,50)` to 10000; process 8 has a 2-second start delay and sets the same
rectangle to zero. Both contain `nonstop`. The overwrite interpretation permits
this authored sequence to stop the effect; inclusive cells and exact activation
timing still require comparison to the original. Do not claim that merely
recording these values implements the visible quake effect.

The client viewer samples the active field at the player's location and applies
a provisional camera shake to rendering only. Power 10000 or greater is capped
at 0.012 radians of yaw and 0.012 tiles of vertical eye displacement; smaller
powers scale linearly. Fixed-tick sinusoidal motion makes the result bounded and
repeatable. Walls, planes, objects, and projectiles use the same displaced camera;
the weapon and HUD stay fixed. The conversion and waveform are invented for a
visible preview and require comparison with the original game. A zero power or
out-of-map sample produces no shake, and restarting resets the session field.
