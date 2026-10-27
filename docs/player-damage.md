# Player damage and armor

All session damage sources use one calculation based on the independently reconstructed `Player::Hit` in PanzerChasm. For a living player, armor absorbs `min(incoming_damage, armor)` points, consuming that amount of armor. Health damage is the unabsorbed portion plus one quarter of the absorbed portion, rounded down. Health saturates at zero; subsequent hits in the same tick do not consume armor or deal further damage. `SessionEvents::damage` reports health damage before saturation, so an overkill hit can report more than the remaining health.

This rule is based on a local reconstruction, not yet verified against the original executable. The bundled modding manual identifies health and armor item types but does not specify their damage formula. Timed shields, chojin protection, hit impulse, pain timing and other damage reactions are separate work.
