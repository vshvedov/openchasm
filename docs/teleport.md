# Teleport definitions and state

`teleport::parse` reads ordered `#teleports` / `tcenter` rows atomically,
rejecting malformed coordinates, directions, unknown records and unterminated
sections. It retains the five authored values and the expansion's sixth field.
Six-field rows parse but `TeleportState::tick` returns `UnsupportedExtra` if
one triggers: that field's meaning is unverified, and it is not silently lost.

Each coordinate component in 0–63 denotes its cell center; 64–16383 denotes
1/256-tile position units, following the bundled 2022 modding manual. Source
and destination are independently converted. The manual documents directions
0–3, but installed levels also use 4; conversion follows PanzerChasm's
`-direction * pi/2 - pi/2`, normalized to a turn. PanzerChasm uses a different
interpretation for small destinations (integer tile corners), so the correct
original behavior remains an explicit open question.

`TeleportState` is per entity. Each tick selects the first source whose center
is strictly within 0.5 tiles and requests a destination-clearance decision from
the host. Blocked destinations do not consume a trigger. A successful event
includes source, destination and yaw; the host must atomically update position,
vertical floor/velocity, view, collision, sound, rendering, and network state.
It must not continue using stale pre-teleport floor or proximity triggers.
This subsystem does not currently alter session movement or any entity.

`ReferenceContinuous` reproduces PanzerChasm's per-tick triggering and has no
cooldown; paired/nearby sources may cause repeated teleports. The optional
`AfterLeavingSources` policy prevents loops until an entity leaves every source
circle; it is a provisional safety approximation, not evidence of the original
cooldown. Original trigger radius, collision, direction and rearm behavior need
comparison against the game. Each entity requires its own state, and a restart
must restore its corresponding snapshot.

The optional installed-corpus test checks 76 rows in maps 01–19, including all
six level-17 rows with the extra field. It does not establish map traversal or
prove that flagged expansion teleports work.

`Session::set_teleports` opts the player into this behavior and selects a rearm
policy; fresh sessions remain disabled. After movement, a successful teleport
updates position and yaw, resets vertical velocity at the provisional flat
floor, then runs destination pickups and floor links in that tick. It emits
`SessionEvents::teleport`; blocked destinations and unsupported extra fields
are reported as `teleport_failure`. Clearance checks represented live actor
and object circles plus blocking walls, map edges and the provisional ceiling.
Source floor triggers are not fired on a successful teleport. Destination
floor entry is reset even when source and destination share a cell. AI and
other entities do not teleport yet; wind currently applies before teleport,
unlike the reference's provisional order.
