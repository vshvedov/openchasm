# Projectile and mine snapshots

`ProjectileSystem` and `MineSystem` expose portable value DTOs with fixed-width owner source IDs, stable active IDs and next-ID counters. Projectile snapshots include ownership, reflected-shot provenance, position, velocity, collision radius, damage and lifetime. Mine snapshots include owner, position, age and armed state. Entry order is preserved.

Restore checks capacity, unique IDs below the next counter, referenced actor owners, finite bounded positions, and lifecycle invariants before replacing any state. Projectile restore also verifies that the referenced loaded rocket is physical and has matching power, collision radius and provisional speed. The freshly loaded content remains authoritative. Mines validate the current arming and expiry rules; dead actor owners remain valid while their authored actor record exists.

These snapshots exclude Session-owned projectile heights, which must be restored and cross-checked separately by the eventual full Session loader. They do not serialize bytes or write files. Projectile motion and mine constants remain provisional gameplay approximations.
