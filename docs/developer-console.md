# Developer console

Press the console key (tilde/backtick) to open or close the overlay. The
console accepts SDL text input; use Left/Right, Home/End, Backspace/Delete to
edit, Up/Down for command history, Page Up/Down for scrollback, and Tab for
command completion. Escape closes the console. History and output are bounded
and apply only to the current client session.

- `help` or `cmdlist` lists commands.
- `clear` clears output; `echo text` prints text; `version` prints the build version.
- `fps [on|off|1|0]` shows or sets the FPS display.
- `maxfps [0..1000]` shows or sets the frame rate cap; 0 is uncapped.
- `status` reports the current game status.
- `restart` restarts the current level. `quit` leaves the current game (returning
  to the launcher when started there).
- `sensitivity [value]` shows or sets mouse sensitivity in radians per pixel;
  accepted values are 0.0001 through 1.0.

Only the commands above are implemented. Closing, pausing and input capture
are managed by the client; the command parser itself never mutates gameplay.

The FPS counter is visible by default. `status` reports the recent wall-clock
FPS, average frame time, simulation/render/presentation costs, and longest frame
in the half-second sample. The default cap is 60 FPS; `maxfps 0` measures rendering
headroom without that cap. The console pauses simulation and releases the mouse;
closing it resumes input capture when gameplay has focus.

For repeatable profiling, `OPENCHASM_PROFILE=1` prints periodic timings to stderr.
`OPENCHASM_UNCAPPED=1` starts without the frame cap; normal launches remain capped.
