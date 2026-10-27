# Fixed-tick session

`Session` composes a world, process runtime, pickups and inventory, actors, AI, and hitscan combat without window or network dependencies. It receives one input per 60 Hz tick and returns pickup, shot, and damage events for presentation. This is a basis for an authoritative server, not a network implementation or a guarantee of cross-platform deterministic floating-point results.

Current gameplay is intentionally provisional: only instant weapons fire; attack timing, AI and hit geometry need comparison with the original; armor absorbs up to half of each melee hit as a placeholder; dead players stop updating and must restart the level. Difficulty-disabled actors are removed from combat by setting their health to zero for this session. Scripts, projectile weapons, game completion, saves, and networking remain incomplete.
