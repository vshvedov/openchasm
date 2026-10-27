//! Fixed-tick damage for active script death zones.
use crate::actors::ActorState;
use crate::process_runtime::{DeathZone, ProcessEffect};
use std::collections::BTreeMap;

const PERIOD_TICKS: u8 = 20; // 3 Hz at 60 Hz; reference-derived, provisional.

#[derive(Clone, Debug)]
struct ActiveHazard {
    zones: Vec<DeathZone>,
    ticks: u8,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HazardTarget {
    Player,
    Actor { source_index: usize },
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HazardDamage {
    pub target: HazardTarget,
    pub damage: u32,
}
#[derive(Clone, Debug, Default)]
pub struct Hazards {
    active: BTreeMap<u16, ActiveHazard>,
}

fn contains(zone: &DeathZone, position: [f32; 2], feet: f32, head: f32) -> bool {
    if !position.iter().all(|x| x.is_finite()) || !feet.is_finite() || !head.is_finite() {
        return false;
    }
    let x = position[0].floor();
    let y = position[1].floor();
    x >= f32::from(zone.min_cell[0])
        && x <= f32::from(zone.max_cell[0])
        && y >= f32::from(zone.min_cell[1])
        && y <= f32::from(zone.max_cell[1])
        && head > zone.z_bottom_raw as f32 / 64.0
        && feet < zone.z_top_raw as f32 / 64.0
}
impl Hazards {
    /// Apply lifecycle events before advancing this tick.
    pub fn apply_effect(&mut self, effect: &ProcessEffect) {
        match effect {
            ProcessEffect::HazardActivated { process_id, zones } => {
                self.active.insert(
                    *process_id,
                    ActiveHazard {
                        zones: zones.clone(),
                        ticks: 0,
                    },
                );
            }
            ProcessEffect::HazardDeactivated { process_id } => {
                self.active.remove(process_id);
            }
            _ => {}
        }
    }
    /// Emit damage at 3 Hz; caller applies damage and gameplay consequences.
    /// Actor vertical extent is provisionally the full two-tile room.
    pub fn tick(
        &mut self,
        player: [f32; 2],
        feet: f32,
        head: f32,
        actors: &ActorState,
    ) -> Vec<HazardDamage> {
        let mut events = Vec::new();
        for hazard in self.active.values_mut() {
            hazard.ticks += 1;
            if hazard.ticks < PERIOD_TICKS {
                continue;
            }
            hazard.ticks = 0;
            for zone in &hazard.zones {
                if zone.z_bottom_raw >= 0 && contains(zone, player, feet, head) {
                    events.push(HazardDamage {
                        target: HazardTarget::Player,
                        damage: zone.damage_raw,
                    });
                }
                for actor in &actors.actors {
                    if actor.health > 0 && contains(zone, actor.position, 0.0, 2.0) {
                        events.push(HazardDamage {
                            target: HazardTarget::Actor {
                                source_index: actor.source_index,
                            },
                            damage: zone.damage_raw,
                        });
                    }
                }
            }
        }
        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn actors() -> ActorState {
        ActorState {
            player_spawns: vec![],
            actors: vec![],
            unresolved: vec![],
        }
    }
    #[test]
    fn damages_at_three_hertz_and_stops_after_deactivation() {
        let mut hazards = Hazards::default();
        let zone = DeathZone {
            min_cell: [1, 1],
            max_cell: [2, 2],
            z_bottom_raw: 0,
            z_top_raw: 128,
            damage_raw: 25,
        };
        hazards.apply_effect(&ProcessEffect::HazardActivated {
            process_id: 7,
            zones: vec![zone],
        });
        for _ in 0..19 {
            assert!(hazards.tick([1.5, 1.5], 0.0, 0.9, &actors()).is_empty());
        }
        assert_eq!(
            hazards.tick([1.5, 1.5], 0.0, 0.9, &actors()),
            vec![HazardDamage {
                target: HazardTarget::Player,
                damage: 25
            }]
        );
        hazards.apply_effect(&ProcessEffect::HazardDeactivated { process_id: 7 });
        for _ in 0..20 {
            assert!(hazards.tick([1.5, 1.5], 0.0, 0.9, &actors()).is_empty());
        }
    }
    #[test]
    fn negative_floor_still_damages_monsters() {
        let mut hazards = Hazards::default();
        let zone = DeathZone {
            min_cell: [0, 0],
            max_cell: [0, 0],
            z_bottom_raw: -1,
            z_top_raw: 128,
            damage_raw: 100,
        };
        hazards.apply_effect(&ProcessEffect::HazardActivated {
            process_id: 1,
            zones: vec![zone],
        });
        let actor = crate::actors::Actor {
            source_index: 5,
            definition_index: 1,
            placement: crate::map::Monster {
                position: [128, 128],
                id: 101,
                angle: 0,
                flags: 2,
            },
            position: [0.5, 0.5],
            angle_radians: 0.0,
            health: 50,
        };
        let actors = ActorState {
            player_spawns: vec![],
            actors: vec![actor],
            unresolved: vec![],
        };
        for _ in 0..19 {
            assert!(hazards.tick([0.5, 0.5], 0.0, 0.9, &actors).is_empty());
        }
        assert_eq!(
            hazards.tick([0.5, 0.5], 0.0, 0.9, &actors),
            vec![HazardDamage {
                target: HazardTarget::Actor { source_index: 5 },
                damage: 100
            }]
        );
    }
    #[test]
    fn negative_floor_excludes_player_and_vertical_clearance_is_respected() {
        let mut hazards = Hazards::default();
        let zone = DeathZone {
            min_cell: [0, 0],
            max_cell: [0, 0],
            z_bottom_raw: -1,
            z_top_raw: 64,
            damage_raw: 50,
        };
        hazards.apply_effect(&ProcessEffect::HazardActivated {
            process_id: 1,
            zones: vec![zone],
        });
        for _ in 0..20 {
            assert!(hazards.tick([0.5, 0.5], 0.0, 0.9, &actors()).is_empty());
        }
        assert!(!contains(&zone, [0.5, 0.5], 1.1, 2.0));
    }
}
