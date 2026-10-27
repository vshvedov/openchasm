# Fixed-tick session

`Session` composes a world, process runtime, pickups and inventory, actors, AI, and hitscan combat without window or network dependencies. It receives one input per 60 Hz tick and returns pickup, shot, and damage events for presentation. This is a basis for an authoritative server, not a network implementation or a guarantee of cross-platform deterministic floating-point results.

Current gameplay is intentionally provisional: instant weapons and selected physical weapons fire, and physical enemy launches are collision-tested; attack timing, AI and hit geometry need comparison with the original; armor absorbs up to half of each melee hit as a placeholder; dead players stop updating; the preview can restore a fresh level snapshot with R. Difficulty-disabled actors are removed from combat by setting their health to zero for this session. Scripts, projectile weapons, game completion, saves, and networking remain incomplete.

The library builds without the SDL and audio dependencies using `cargo test --locked --no-default-features --lib`. The default `client` feature enables the desktop executable. A network server, its protocol, and replication are not implemented yet.

The preview opts into scripted death-zone effects and applies damage at a provisional three times per second. The original raw damage values are retained and negative lower Z bounds exclude the player. Monster height is still approximated as the full room; exact damage cadence and overflow behavior need comparison with the game.
