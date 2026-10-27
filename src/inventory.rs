//! Window- and network-independent player inventory and pickup rules.
//!
//! Weapon pickup quantities come from `chasm.inf`. Values for health and armor
//! are based on existing reconstruction work and still require comparison
//! against the original game. Timed power-ups and combat are not modeled yet.
use std::io::{self, ErrorKind};

pub const WEAPON_COUNT: usize = 8;
pub const STARTING_HEALTH: u16 = 100;
pub const NORMAL_HEALTH_LIMIT: u16 = 100;
pub const MAX_HEALTH: u16 = 200;
pub const MAX_ARMOR: u16 = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WeaponDefinition {
    pub rocket_type: i32,
    /// Original reload value; the conversion to simulation time is unverified.
    pub reload_time: u32,
    pub vertical_shift: i32,
    pub projectile_height: i32,
    pub ammo_per_portion: u16,
    pub ammo_limit: u16,
    pub ammo_on_weapon_pickup: u16,
    pub projectile_count: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WeaponDefinitions(pub [WeaponDefinition; WEAPON_COUNT]);

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(ErrorKind::InvalidData, message.into())
}

impl WeaponDefinitions {
    /// Parse exactly eight weapon definitions from the textual game resource.
    /// Geometry and animation paths are intentionally left to the asset layer.
    pub fn parse(inf: &[u8]) -> io::Result<Self> {
        let text = std::str::from_utf8(inf).map_err(|_| invalid("chasm.inf is not UTF-8"))?;
        let mut lines = text.lines().enumerate();
        if !lines
            .by_ref()
            .any(|(_, line)| line.trim().eq_ignore_ascii_case("[WEAPONS]"))
        {
            return Err(invalid("missing [WEAPONS] section"));
        }
        let (_, count_line) = lines
            .by_ref()
            .find(|(_, line)| {
                let line = line.trim();
                !line.is_empty() && !line.starts_with(';')
            })
            .ok_or_else(|| invalid("missing weapon count"))?;
        let count: usize = count_line
            .split_whitespace()
            .next()
            .ok_or_else(|| invalid("missing weapon count"))?
            .parse()
            .map_err(|_| invalid("invalid weapon count"))?;
        if count != WEAPON_COUNT {
            return Err(invalid(format!(
                "expected {WEAPON_COUNT} weapons, got {count}"
            )));
        }
        let mut definitions = Vec::with_capacity(WEAPON_COUNT);
        for (line_number, line) in lines {
            let line = line.split(';').next().unwrap_or("").trim();
            if line.is_empty() || line.starts_with('*') || line.contains('=') {
                continue;
            }
            if line.starts_with('[') {
                break;
            }
            let values: Vec<_> = line.split_whitespace().collect();
            if values.len() != 8 {
                return Err(invalid(format!(
                    "invalid weapon row on line {}",
                    line_number + 1
                )));
            }
            let int = |index: usize| -> io::Result<i32> {
                values[index].parse().map_err(|_| {
                    invalid(format!("invalid weapon field on line {}", line_number + 1))
                })
            };
            let small = |index: usize| -> io::Result<u16> {
                u16::try_from(int(index)?).map_err(|_| {
                    invalid(format!(
                        "weapon field out of range on line {}",
                        line_number + 1
                    ))
                })
            };
            let reload_time = u32::try_from(int(1)?)
                .map_err(|_| invalid(format!("negative reload on line {}", line_number + 1)))?;
            let ammo_limit = small(5)?;
            let ammo_on_weapon_pickup = small(6)?;
            definitions.push(WeaponDefinition {
                rocket_type: int(0)?,
                reload_time,
                vertical_shift: int(2)?,
                projectile_height: int(3)?,
                ammo_per_portion: small(4)?,
                ammo_limit,
                ammo_on_weapon_pickup,
                projectile_count: small(7)?,
            });
            if definitions.len() == WEAPON_COUNT {
                break;
            }
        }
        Ok(Self(definitions.try_into().map_err(|_| {
            invalid("weapon section ended before all rows were read")
        })?))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Red,
    Green,
    Blue,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pickup {
    Weapon(usize),
    Ammo(usize),
    Health,
    Armor,
    Key(Key),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickupOutcome {
    /// The caller may remove the item or mark it as collected.
    Collected(Pickup),
    /// A known item had no effect because the corresponding stat was full.
    Full,
    /// The action is not yet modeled and must not be consumed by the caller.
    Unsupported,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inventory {
    pub health: u16,
    pub armor: u16,
    pub weapons: [bool; WEAPON_COUNT],
    pub ammo: [u16; WEAPON_COUNT],
    pub keys: [bool; 3],
}

impl Inventory {
    /// Start with the first weapon and its configured ammunition limit.
    /// Respawns and between-level inventory transfer need separate rules.
    pub fn new(weapons: &WeaponDefinitions) -> Self {
        let mut inventory = Self {
            health: STARTING_HEALTH,
            armor: 0,
            weapons: [false; WEAPON_COUNT],
            ammo: [0; WEAPON_COUNT],
            keys: [false; 3],
        };
        inventory.weapons[0] = true;
        inventory.ammo[0] = weapons.0[0].ammo_limit;
        inventory
    }

    /// Apply one action code from an object definition. Unknown and unmodeled
    /// action codes return Unsupported without changing the player state.
    pub fn pickup(&mut self, action_code: i32, weapons: &WeaponDefinitions) -> PickupOutcome {
        match action_code {
            1..=8 => {
                let index = (action_code - 1) as usize;
                let spec = weapons.0[index];
                if self.weapons[index] && self.ammo[index] >= spec.ammo_limit {
                    return PickupOutcome::Full;
                }
                self.weapons[index] = true;
                self.ammo[index] = self.ammo[index]
                    .saturating_add(spec.ammo_on_weapon_pickup)
                    .min(spec.ammo_limit);
                PickupOutcome::Collected(Pickup::Weapon(index))
            }
            10..=89 => {
                let index = (action_code / 10 - 1) as usize;
                let portions = (action_code % 10) as u16;
                let spec = weapons.0[index];
                if portions == 0 || spec.ammo_per_portion == 0 {
                    return PickupOutcome::Unsupported;
                }
                if self.ammo[index] >= spec.ammo_limit {
                    return PickupOutcome::Full;
                }
                let amount = spec.ammo_per_portion.saturating_mul(portions);
                self.ammo[index] = self.ammo[index].saturating_add(amount).min(spec.ammo_limit);
                PickupOutcome::Collected(Pickup::Ammo(index))
            }
            102 => {
                if self.health >= NORMAL_HEALTH_LIMIT {
                    return PickupOutcome::Full;
                }
                self.health = self.health.saturating_add(20).min(NORMAL_HEALTH_LIMIT);
                PickupOutcome::Collected(Pickup::Health)
            }
            120 => {
                if self.health >= MAX_HEALTH {
                    return PickupOutcome::Full;
                }
                self.health = self.health.saturating_add(100).min(MAX_HEALTH);
                PickupOutcome::Collected(Pickup::Health)
            }
            124 | 128 => {
                if self.armor >= MAX_ARMOR {
                    return PickupOutcome::Full;
                }
                let amount = if action_code == 124 { 100 } else { 200 };
                self.armor = self.armor.saturating_add(amount).min(MAX_ARMOR);
                PickupOutcome::Collected(Pickup::Armor)
            }
            141..=143 => {
                let (index, key) = match action_code {
                    141 => (0, Key::Red),
                    142 => (1, Key::Green),
                    _ => (2, Key::Blue),
                };
                // The published reconstruction consumes world keys even when
                // the player already has that color, allowing map links to fire.
                self.keys[index] = true;
                PickupOutcome::Collected(Pickup::Key(key))
            }
            _ => PickupOutcome::Unsupported,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn weapon_inf() -> String {
        let mut s = String::from("[WEAPONS]\n8 ; count\n");
        for i in 0..8 {
            s.push_str(&format!(
                "MODEL=gun{i}.3o\nSTAT=idle.ani\nSHOOT=fire.ani\n3 86 32 180 10 50 20 1\n"
            ));
        }
        s.push_str("[ROCKETS]\n");
        s
    }
    #[test]
    fn reads_weapon_amounts_and_rejects_bad_data() {
        let text = weapon_inf();
        let defs = WeaponDefinitions::parse(text.as_bytes()).unwrap();
        assert_eq!(defs.0[0].ammo_per_portion, 10);
        assert_eq!(defs.0[7].ammo_limit, 50);
        assert_eq!(defs.0[3].ammo_on_weapon_pickup, 20);
        assert!(
            WeaponDefinitions::parse(text.replace("[WEAPONS]", "[NOWEAPONS]").as_bytes()).is_err()
        );
        assert!(
            WeaponDefinitions::parse(text.replace("8 ; count", "9 ; count").as_bytes()).is_err()
        );
        assert!(
            WeaponDefinitions::parse(text.replace("10 50 20 1", "-1 50 20 1").as_bytes()).is_err()
        );
    }
    #[test]
    fn weapon_and_ammo_pickups_obey_limits_and_do_not_require_ownership() {
        let defs = WeaponDefinitions::parse(weapon_inf().as_bytes()).unwrap();
        let mut player = Inventory::new(&defs);
        assert!(player.weapons[0]);
        assert_eq!(player.ammo[0], 50);
        assert_eq!(
            player.pickup(22, &defs),
            PickupOutcome::Collected(Pickup::Ammo(1))
        );
        assert_eq!(player.ammo[1], 20);
        assert!(!player.weapons[1]);
        assert_eq!(
            player.pickup(2, &defs),
            PickupOutcome::Collected(Pickup::Weapon(1))
        );
        assert!(player.weapons[1]);
        assert_eq!(player.ammo[1], 40);
        assert_eq!(
            player.pickup(2, &defs),
            PickupOutcome::Collected(Pickup::Weapon(1))
        );
        assert_eq!(player.ammo[1], 50);
        assert_eq!(player.pickup(2, &defs), PickupOutcome::Full);
        assert_eq!(player.pickup(21, &defs), PickupOutcome::Full);
        assert_eq!(player.pickup(20, &defs), PickupOutcome::Unsupported);
    }
    #[test]
    fn health_armor_keys_and_unimplemented_codes() {
        let defs = WeaponDefinitions::parse(weapon_inf().as_bytes()).unwrap();
        let mut player = Inventory::new(&defs);
        assert_eq!(player.pickup(102, &defs), PickupOutcome::Full);
        player.health = 95;
        assert_eq!(
            player.pickup(102, &defs),
            PickupOutcome::Collected(Pickup::Health)
        );
        assert_eq!(player.health, 100);
        assert_eq!(
            player.pickup(120, &defs),
            PickupOutcome::Collected(Pickup::Health)
        );
        assert_eq!(player.health, 200);
        assert_eq!(player.pickup(102, &defs), PickupOutcome::Full);
        assert_eq!(player.pickup(120, &defs), PickupOutcome::Full);
        assert_eq!(
            player.pickup(124, &defs),
            PickupOutcome::Collected(Pickup::Armor)
        );
        assert_eq!(player.armor, 100);
        assert_eq!(
            player.pickup(128, &defs),
            PickupOutcome::Collected(Pickup::Armor)
        );
        assert_eq!(player.armor, 200);
        assert_eq!(player.pickup(124, &defs), PickupOutcome::Full);
        assert_eq!(
            player.pickup(141, &defs),
            PickupOutcome::Collected(Pickup::Key(Key::Red))
        );
        assert!(player.keys[0]);
        let previous = player.clone();
        assert_eq!(player.pickup(133, &defs), PickupOutcome::Unsupported);
        assert_eq!(player, previous);
    }
}
