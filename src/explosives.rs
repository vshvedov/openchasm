//! Server-independent explosion damage and provisional mine lifecycle.
//!
//! The host selects eligible targets, supplies their current cylinders, and
//! applies returned damage once. No wall occlusion, scoring or chained object
//! destruction is inferred here. Mine constants follow an uncalibrated local
//! reconstruction and should be validated against the original game.
use crate::actors::ActorState;
use crate::projectiles::{Owner, OwnerSnapshot};
use std::collections::BTreeSet;

const MAX_COORD: f32 = 4096.0;
const MAX_RADIUS: f32 = 64.0;
const MAX_TARGETS: usize = 16_384;
pub const MINE_ARM_TICKS: u32 = 90;
pub const MINE_EXPIRE_TICKS: u32 = 1800;
pub const MINE_TRIGGER_RADIUS: f32 = 0.5;
pub const MINE_BLAST_RADIUS: f32 = 2.3;
pub const MINE_DAMAGE: i32 = 140;
pub const MAX_MINES: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum TargetId {
    Player,
    Actor { source_index: usize },
    Object { cell: [usize; 2] },
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Cylinder {
    pub center: [f32; 2],
    pub radius: f32,
    /// Absolute floor and top heights, in map tiles.
    pub bottom: f32,
    pub top: f32,
}
impl Cylinder {
    fn valid(self) -> bool {
        valid_xy(self.center)
            && self.radius.is_finite()
            && (0.0..=MAX_RADIUS).contains(&self.radius)
            && valid_scalar(self.bottom)
            && valid_scalar(self.top)
            && self.bottom <= self.top
    }
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Explosion {
    pub owner: Owner,
    pub center: [f32; 3],
    pub radius: f32,
    pub damage: i32,
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ExplosionTarget {
    pub id: TargetId,
    pub cylinder: Cylinder,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DamageEvent {
    pub target: TargetId,
    pub owner: Owner,
    pub damage: i32,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExplosiveError {
    InvalidExplosion,
    InvalidTarget,
    DuplicateTarget,
    TooManyTargets,
    InvalidMinePosition,
    Capacity,
    IdExhausted,
}
fn valid_scalar(value: f32) -> bool {
    value.is_finite() && value.abs() <= MAX_COORD
}
fn valid_xy(value: [f32; 2]) -> bool {
    value.into_iter().all(valid_scalar)
}
fn validate_targets(targets: &[ExplosionTarget]) -> Result<(), ExplosiveError> {
    if targets.len() > MAX_TARGETS {
        return Err(ExplosiveError::TooManyTargets);
    }
    let mut ids = BTreeSet::new();
    for target in targets {
        if matches!(target.id, TargetId::Object { cell } if cell[0] >= 64 || cell[1] >= 64)
            || !target.cylinder.valid()
        {
            return Err(ExplosiveError::InvalidTarget);
        }
        if !ids.insert(target.id) {
            return Err(ExplosiveError::DuplicateTarget);
        }
    }
    Ok(())
}
/// Resolve one explosion into one event per damaged target, in supplied order.
/// Cylinder-surface distance gives linear falloff; owner is not immune. Caller
/// must avoid additionally applying direct projectile damage for splash impacts.
pub fn resolve_explosion(
    explosion: Explosion,
    targets: &[ExplosionTarget],
) -> Result<Vec<DamageEvent>, ExplosiveError> {
    if !explosion.center.into_iter().all(valid_scalar)
        || !explosion.radius.is_finite()
        || explosion.radius <= 0.0
        || explosion.radius > MAX_RADIUS
        || explosion.damage <= 0
    {
        return Err(ExplosiveError::InvalidExplosion);
    }
    validate_targets(targets)?;
    let mut events = Vec::new();
    for target in targets {
        let c = target.cylinder;
        let horizontal =
            (explosion.center[0] - c.center[0]).hypot(explosion.center[1] - c.center[1]);
        let horizontal = (horizontal - c.radius).max(0.0);
        let vertical = (c.bottom - explosion.center[2])
            .max(0.0)
            .max(explosion.center[2] - c.top);
        let distance = horizontal.hypot(vertical);
        if distance >= explosion.radius {
            continue;
        }
        let damage =
            ((f64::from(explosion.damage) * (1.0 - f64::from(distance / explosion.radius))).round()
                as i32)
                .max(0);
        if damage > 0 {
            events.push(DamageEvent {
                target: target.id,
                owner: explosion.owner,
                damage,
            });
        }
    }
    Ok(events)
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Mine {
    pub id: u64,
    pub owner: Owner,
    pub position: [f32; 3],
    pub age_ticks: u32,
    pub armed: bool,
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MineTrigger {
    pub id: TargetId,
    pub position: [f32; 2],
    pub radius: f32,
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum MineEvent {
    Placed(Mine),
    Armed { id: u64 },
    Detonated { id: u64, explosion: Explosion },
    Expired { id: u64 },
}
#[derive(Clone, Debug, Default)]
pub struct MineSystem {
    mines: Vec<Mine>,
    next_id: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MineSnapshot {
    pub id: u64,
    pub owner: OwnerSnapshot,
    pub position: [f32; 3],
    pub age_ticks: u32,
    pub armed: bool,
}
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MineSystemSnapshot {
    pub next_id: u64,
    pub mines: Vec<MineSnapshot>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MineSnapshotError {
    Capacity,
    IdOutOfRange,
    DuplicateId,
    InvalidOwner,
    InvalidPosition,
    InvalidLifecycle,
}
impl MineSystem {
    pub fn snapshot(&self) -> Result<MineSystemSnapshot, MineSnapshotError> {
        let mut mines = Vec::with_capacity(self.mines.len());
        for mine in &self.mines {
            mines.push(MineSnapshot {
                id: mine.id,
                owner: OwnerSnapshot::from_owner(mine.owner)
                    .ok_or(MineSnapshotError::InvalidOwner)?,
                position: mine.position,
                age_ticks: mine.age_ticks,
                armed: mine.armed,
            });
        }
        Ok(MineSystemSnapshot {
            next_id: self.next_id,
            mines,
        })
    }
    /// Validate IDs, actors, positions and lifecycle before publishing state.
    pub fn restore(
        &mut self,
        value: &MineSystemSnapshot,
        actors: &ActorState,
    ) -> Result<(), MineSnapshotError> {
        if value.mines.len() > MAX_MINES {
            return Err(MineSnapshotError::Capacity);
        }
        let mut ids = BTreeSet::new();
        let mut mines = Vec::with_capacity(value.mines.len());
        for mine in &value.mines {
            if mine.id >= value.next_id {
                return Err(MineSnapshotError::IdOutOfRange);
            }
            if !ids.insert(mine.id) {
                return Err(MineSnapshotError::DuplicateId);
            }
            let owner = mine
                .owner
                .resolve(actors)
                .ok_or(MineSnapshotError::InvalidOwner)?;
            if !mine.position.into_iter().all(valid_scalar)
                || !(0.0..64.0).contains(&mine.position[0])
                || !(0.0..64.0).contains(&mine.position[1])
            {
                return Err(MineSnapshotError::InvalidPosition);
            }
            if mine.age_ticks > MINE_EXPIRE_TICKS
                || mine.armed != (mine.age_ticks >= MINE_ARM_TICKS)
            {
                return Err(MineSnapshotError::InvalidLifecycle);
            }
            mines.push(Mine {
                id: mine.id,
                owner,
                position: mine.position,
                age_ticks: mine.age_ticks,
                armed: mine.armed,
            });
        }
        self.mines = mines;
        self.next_id = value.next_id;
        Ok(())
    }
    pub fn new() -> Self {
        Self::default()
    }
    pub fn active(&self) -> &[Mine] {
        &self.mines
    }
    /// Plant on an already validated floor; map bounds are 64 by 64 tiles.
    pub fn plant(&mut self, owner: Owner, position: [f32; 3]) -> Result<MineEvent, ExplosiveError> {
        if !position.into_iter().all(valid_scalar)
            || !(0.0..64.0).contains(&position[0])
            || !(0.0..64.0).contains(&position[1])
        {
            return Err(ExplosiveError::InvalidMinePosition);
        }
        if self.mines.len() >= MAX_MINES {
            return Err(ExplosiveError::Capacity);
        }
        let next = self
            .next_id
            .checked_add(1)
            .ok_or(ExplosiveError::IdExhausted)?;
        let mine = Mine {
            id: self.next_id,
            owner,
            position,
            age_ticks: 0,
            armed: false,
        };
        self.next_id = next;
        self.mines.push(mine);
        Ok(MineEvent::Placed(mine))
    }
    /// Advance exactly one 1/60-second tick. Input is validated before any
    /// mine advances. Eligible living player/actor circles are supplied by host;
    /// objects do not trigger mines. Expiry occurs after 30 seconds, as in the
    /// reconstruction, and takes precedence over triggering on that tick.
    pub fn tick(&mut self, triggers: &[MineTrigger]) -> Result<Vec<MineEvent>, ExplosiveError> {
        if triggers.len() > MAX_TARGETS {
            return Err(ExplosiveError::TooManyTargets);
        }
        let mut ids = BTreeSet::new();
        for trigger in triggers {
            if matches!(trigger.id, TargetId::Object { .. })
                || !valid_xy(trigger.position)
                || !trigger.radius.is_finite()
                || !(0.0..=MAX_RADIUS).contains(&trigger.radius)
            {
                return Err(ExplosiveError::InvalidTarget);
            }
            if !ids.insert(trigger.id) {
                return Err(ExplosiveError::DuplicateTarget);
            }
        }
        let mut events = Vec::new();
        self.mines.retain_mut(|mine| {
            mine.age_ticks += 1;
            if mine.age_ticks > MINE_EXPIRE_TICKS {
                events.push(MineEvent::Expired { id: mine.id });
                return false;
            }
            if mine.age_ticks >= MINE_ARM_TICKS {
                if !mine.armed {
                    mine.armed = true;
                    events.push(MineEvent::Armed { id: mine.id });
                }
                if triggers.iter().any(|trigger| {
                    let distance = (trigger.position[0] - mine.position[0])
                        .hypot(trigger.position[1] - mine.position[1]);
                    distance < MINE_TRIGGER_RADIUS + trigger.radius
                }) {
                    events.push(MineEvent::Detonated {
                        id: mine.id,
                        explosion: Explosion {
                            owner: mine.owner,
                            center: mine.position,
                            radius: MINE_BLAST_RADIUS,
                            damage: MINE_DAMAGE,
                        },
                    });
                    return false;
                }
            }
            true
        });
        Ok(events)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mine_snapshot_preserves_lifecycle_ids_and_validates_atomically() {
        let actors = ActorState {
            player_spawns: vec![],
            actors: vec![],
            unresolved: vec![],
        };
        let mut source = MineSystem::new();
        source.plant(Owner::Player, [3.0, 4.0, 0.0]).unwrap();
        for _ in 0..MINE_ARM_TICKS {
            source.tick(&[]).unwrap();
        }
        source.plant(Owner::Player, [4.0, 4.0, 0.0]).unwrap();
        let snapshot = source.snapshot().unwrap();
        let mut restored = MineSystem::new();
        restored.restore(&snapshot, &actors).unwrap();
        assert_eq!(restored.snapshot().unwrap(), snapshot);
        let mut invalid = snapshot.clone();
        invalid.mines[1].id = invalid.mines[0].id;
        assert_eq!(
            restored.restore(&invalid, &actors),
            Err(MineSnapshotError::DuplicateId)
        );
        assert_eq!(restored.snapshot().unwrap(), snapshot);
        invalid = snapshot.clone();
        invalid.mines[0].armed = false;
        assert_eq!(
            restored.restore(&invalid, &actors),
            Err(MineSnapshotError::InvalidLifecycle)
        );
        invalid = snapshot.clone();
        invalid.mines[0].position[0] = f32::NAN;
        assert_eq!(
            restored.restore(&invalid, &actors),
            Err(MineSnapshotError::InvalidPosition)
        );
        invalid = snapshot.clone();
        invalid.mines[0].owner = OwnerSnapshot::Enemy { source_index: 99 };
        assert_eq!(
            restored.restore(&invalid, &actors),
            Err(MineSnapshotError::InvalidOwner)
        );
        invalid = snapshot.clone();
        invalid.next_id = 1;
        assert_eq!(
            restored.restore(&invalid, &actors),
            Err(MineSnapshotError::IdOutOfRange)
        );
        assert_eq!(restored.snapshot().unwrap(), snapshot);
        invalid = snapshot.clone();
        invalid.mines[0].age_ticks = MINE_EXPIRE_TICKS + 1;
        assert_eq!(
            restored.restore(&invalid, &actors),
            Err(MineSnapshotError::InvalidLifecycle)
        );
        invalid = snapshot.clone();
        invalid.mines.resize(MAX_MINES + 1, invalid.mines[0]);
        assert_eq!(
            restored.restore(&invalid, &actors),
            Err(MineSnapshotError::Capacity)
        );
        assert_eq!(restored.snapshot().unwrap(), snapshot);
        assert!(matches!(
            restored.plant(Owner::Player, [5.0, 4.0, 0.0]).unwrap(),
            MineEvent::Placed(Mine { id: 2, .. })
        ));
        assert_eq!(source.tick(&[]).unwrap(), {
            let mut same = MineSystem::new();
            same.restore(&snapshot, &actors).unwrap();
            same.tick(&[]).unwrap()
        });
    }
    fn cylinder(x: f32, radius: f32, bottom: f32, top: f32) -> Cylinder {
        Cylinder {
            center: [x, 0.0],
            radius,
            bottom,
            top,
        }
    }
    fn blast() -> Explosion {
        Explosion {
            owner: Owner::Player,
            center: [0.0, 0.0, 0.5],
            radius: 2.0,
            damage: 100,
        }
    }
    #[test]
    fn damage_uses_cylinder_surface_and_vertical_distance_in_stable_order() {
        let targets = [
            ExplosionTarget {
                id: TargetId::Player,
                cylinder: cylinder(0.0, 0.2, 0.0, 1.0),
            },
            ExplosionTarget {
                id: TargetId::Actor { source_index: 2 },
                cylinder: cylinder(1.5, 0.5, 0.0, 1.0),
            },
            ExplosionTarget {
                id: TargetId::Object { cell: [1, 1] },
                cylinder: cylinder(1.5, 0.5, 1.5, 2.0),
            },
            ExplosionTarget {
                id: TargetId::Actor { source_index: 3 },
                cylinder: cylinder(2.5, 0.5, 0.0, 1.0),
            },
        ];
        let events = resolve_explosion(blast(), &targets).unwrap();
        assert_eq!(
            events.iter().map(|e| e.damage).collect::<Vec<_>>(),
            vec![100, 50, 29]
        );
        assert_eq!(events[0].target, TargetId::Player);
        assert!(events.iter().all(|e| e.owner == Owner::Player));
    }
    #[test]
    fn invalid_or_duplicate_targets_reject_complete_explosion() {
        let target = ExplosionTarget {
            id: TargetId::Player,
            cylinder: cylinder(0.0, 0.2, 0.0, 1.0),
        };
        assert_eq!(
            resolve_explosion(blast(), &[target, target]),
            Err(ExplosiveError::DuplicateTarget)
        );
        let invalid = ExplosionTarget {
            cylinder: cylinder(0.0, -1.0, 0.0, 1.0),
            ..target
        };
        assert_eq!(
            resolve_explosion(blast(), &[target, invalid]),
            Err(ExplosiveError::InvalidTarget)
        );
        assert_eq!(
            resolve_explosion(
                Explosion {
                    radius: f32::NAN,
                    ..blast()
                },
                &[target]
            ),
            Err(ExplosiveError::InvalidExplosion)
        );
    }
    #[test]
    fn mine_arms_then_detonates_once_even_on_owner() {
        let mut mines = MineSystem::new();
        let placed = mines.plant(Owner::Player, [3.0, 4.0, 0.0]).unwrap();
        assert!(matches!(
            placed,
            MineEvent::Placed(Mine {
                id: 0,
                armed: false,
                ..
            })
        ));
        let owner = MineTrigger {
            id: TargetId::Player,
            position: [3.0, 4.0],
            radius: 0.18,
        };
        for _ in 1..MINE_ARM_TICKS {
            assert!(mines.tick(&[owner]).unwrap().is_empty());
        }
        let events = mines.tick(&[owner]).unwrap();
        assert!(
            matches!(&events[..], [MineEvent::Armed { id: 0 }, MineEvent::Detonated { id: 0, explosion }] if explosion.owner == Owner::Player && explosion.damage == MINE_DAMAGE)
        );
        assert!(mines.active().is_empty());
        assert!(mines.tick(&[owner]).unwrap().is_empty());
    }
    #[test]
    fn strict_proximity_expiry_and_invalid_tick_rollback() {
        let mut mines = MineSystem::new();
        mines.plant(Owner::Player, [3.0, 4.0, 0.0]).unwrap();
        let exact = MineTrigger {
            id: TargetId::Player,
            position: [3.0 + MINE_TRIGGER_RADIUS, 4.0],
            radius: 0.0,
        };
        for _ in 0..MINE_ARM_TICKS {
            mines.tick(&[exact]).unwrap();
        }
        assert!(mines.active()[0].armed);
        assert_eq!(mines.active()[0].age_ticks, MINE_ARM_TICKS);
        assert_eq!(
            mines.tick(&[exact, exact]),
            Err(ExplosiveError::DuplicateTarget)
        );
        assert_eq!(mines.active()[0].age_ticks, MINE_ARM_TICKS);
        for _ in MINE_ARM_TICKS..MINE_EXPIRE_TICKS {
            assert!(mines.tick(&[exact]).unwrap().is_empty());
        }
        assert_eq!(
            mines.tick(&[exact]).unwrap(),
            vec![MineEvent::Expired { id: 0 }]
        );
    }
    #[test]
    fn placement_bounds_and_capacity_are_checked_without_consuming_ids() {
        let mut mines = MineSystem::new();
        assert_eq!(
            mines.plant(Owner::Player, [64.0, 0.0, 0.0]),
            Err(ExplosiveError::InvalidMinePosition)
        );
        for id in 0..MAX_MINES {
            assert!(
                matches!(mines.plant(Owner::Player, [0.0, 0.0, 0.0]), Ok(MineEvent::Placed(Mine { id: actual, .. })) if actual == id as u64)
            );
        }
        assert_eq!(
            mines.plant(Owner::Player, [0.0, 0.0, 0.0]),
            Err(ExplosiveError::Capacity)
        );
        assert_eq!(mines.active().len(), MAX_MINES);
    }
}
