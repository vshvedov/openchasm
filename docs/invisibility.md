# Invisibility

Action code 131 collects an invisibility item and adds 2,400 ticks (40 seconds
at the authoritative 60 Hz rate) to `Inventory::invisibility_ticks_remaining`.
Pickup occurs before enemy simulation and immediately prevents idle enemies
from visually acquiring the player. Enemies already alerted keep tracking and
attacking; explicit damage alerts still work. Time advances once per live
session tick and saturates at zero. The inventory and AI state are cloned in
session snapshots, so restart and future server replication can preserve them.
A new pickup extends any remaining duration. Call `clear_level_effects()` when
carrying inventory to another map.

The duration, stacking, map reset, and idle-enemy rule follow the independently
reconstructed [PanzerChasm player](https://github.com/Panzerschrek/Chasm-Reverse/blob/master/src/server/player.cpp)
and [monster](https://github.com/Panzerschrek/Chasm-Reverse/blob/master/src/server/monster.cpp)
implementations; direct original-game validation remains outstanding. Visual
flashing, enemy hearing and a multiplayer target model are not modeled here.
