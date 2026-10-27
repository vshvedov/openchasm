# Teleports

`teleport::parse` reads ordered `#teleports` / `tcenter` rows atomically,
rejecting malformed coordinates, directions, unknown records and unterminated
sections. It retains the authored values and optional sixth numeric field. The
installed 2022 executable reads the first five numeric fields and ignores the
rest of the row; the six level-17 rows therefore execute normally while their
extra values remain available for diagnostics.

The installed executable converts each source cell to its center; source
coordinates outside 0–63 are rejected. Each destination component in 0–63
names a cell center, and 64–16383 names a position in 1/256-tile units. The
manual describes mixed coordinates for both ends, but the remaster parser
always treats source coordinates as cell indices. The remaster computes yaw as
`direction * pi/2 - pi/2`; direction 4 occurs in the installed campaign. Its
parser starts at virtual address 0x140096c20 in the installed Windows binary:
the four coordinate reads are at 0x140097072–0x1400970a1 and the direction
read/conversion at 0x14009710b–0x140097133. Independently reconstructed
PanzerChasm differs in destination centering and yaw sign.

`TeleportState` is per entity. Each tick selects the first source whose center
is strictly within 0.5 tiles and asks the host to validate destination
clearance. Blocked destinations do not consume the trigger. The reference checks
once per tick without a cooldown (`ReferenceContinuous`); optional
`AfterLeavingSources` rearm is a provisional loop-prevention approximation.
Trigger radius, clearance, direction and rearm still need original-game tests.

`Session::set_teleports` opts the player in; fresh sessions remain disabled.
After movement, a successful teleport updates position and yaw, resets vertical
velocity at a known destination platform top or the provisional flat floor, then runs destination pickups and floor
links in that tick. It emits `SessionEvents::teleport`; blocked destinations
are reported as `teleport_failure`. The host presents sound and networking.
AI and other entities do not teleport yet; wind currently applies before the
teleport. Session clones retain the teleport state.

The client loads object mesh heights for destination clearance, allowing level
17's first landing beneath the elevated `18_cl2.3o` roof. Missing or invalid
height data falls back to conservative blocking. The installed-data scenario
tests this landing without actors; it does not establish whole-map traversal
or clearance with the level's full enemy population.

A known solid model can support the destination when its loaded base-mesh bounds and authored radius leave full body clearance. Unknown heights remain conservatively blocking. An installed level 16 test now covers both authored teleports from the boss-side source through the raised 0.875-tile platform to the heart area, with a snapshot round trip on the platform; it places the player at the first source and excludes enemies. This follows the independent reconstruction’s floor placement rule and does not establish a full route.
