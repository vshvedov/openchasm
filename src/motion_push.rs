//! Bounded, deterministic, headless translation and entity-push proposal.
//!
//! This is a conservative first pass, not the original CheckGo/CheckBack or
//! Mortal implementation. The host chooses the motion policy and applies any
//! damage. No mutation occurs when a push is blocked or validation fails.
use crate::world::{CollisionCircle, World};
use std::collections::BTreeSet;

const SUBSTEP: f32 = 1.0 / 64.0;
const MAX_STEPS: usize = 256;
const MAX_MOVERS: usize = 128;
const MAX_ENTITIES: usize = 256;
const MAX_OBSTACLES: usize = 8192;
// Upper bound on potential entity/obstacle checks in one proposal.
const MAX_WORK: usize = 16_000_000;
const EPSILON: f32 = 0.0001;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum EntityId {
    Player,
    Actor(u32),
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PushEntity {
    pub id: EntityId,
    pub position: [f32; 2],
    pub radius: f32,
    /// Absolute bottom and top in tiles.
    pub vertical: [f32; 2],
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum MoverId {
    Wall(usize),
    Circle(usize),
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Translation {
    pub mover: MoverId,
    pub delta: [f32; 2],
    /// Absolute bottom and top in tiles.
    pub vertical: [f32; 2],
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MotionError {
    TooManyInputs,
    InvalidGeometry,
    InvalidEntity,
    InvalidMotion,
    DuplicateEntity,
    DuplicateMover,
    MissingMover,
    ExceedsSweepLimit,
    WorkLimit,
}
#[derive(Clone)]
pub struct CommittedMotion {
    pub world: World,
    pub circles: Vec<CollisionCircle>,
    /// Ordered by stable entity ID, independent of caller input order.
    pub entities: Vec<PushEntity>,
}
#[derive(Clone)]
pub enum MotionProposal {
    Committed(CommittedMotion),
    /// Only identifies the pin; damage, event cadence and feedback are host policy.
    Blocked {
        mover: MoverId,
        entity: EntityId,
    },
}
fn finite_position(p: [f32; 2]) -> bool {
    p.iter().all(|v| v.is_finite() && v.abs() <= 4096.0)
}
fn valid_vertical(z: [f32; 2]) -> bool {
    z.iter().all(|v| v.is_finite() && v.abs() <= 4096.0) && z[0] < z[1]
}
fn overlap_z(a: [f32; 2], b: [f32; 2]) -> bool {
    a[0] < b[1] && b[0] < a[1]
}
fn radius_valid(r: f32) -> bool {
    r.is_finite() && (1.0 / 32.0..=2.0).contains(&r)
}
fn nearest_on_segment(p: [f32; 2], a: [f32; 2], b: [f32; 2]) -> [f32; 2] {
    let d = [b[0] - a[0], b[1] - a[1]];
    let t = (((p[0] - a[0]) * d[0] + (p[1] - a[1]) * d[1]) / (d[0] * d[0] + d[1] * d[1]))
        .clamp(0.0, 1.0);
    [a[0] + d[0] * t, a[1] + d[1] * t]
}
fn segment_distance(p: [f32; 2], a: [f32; 2], b: [f32; 2]) -> f32 {
    let nearest = if a == b {
        a
    } else {
        nearest_on_segment(p, a, b)
    };
    (p[0] - nearest[0]).hypot(p[1] - nearest[1])
}

// Distance to the parallelogram swept by a translated segment. Degenerate
// parallel translations are covered by its boundary segments.
fn swept_wall_distance(p: [f32; 2], a: [f32; 2], b: [f32; 2], d: [f32; 2]) -> f32 {
    let points = [a, b, [b[0] + d[0], b[1] + d[1]], [a[0] + d[0], a[1] + d[1]]];
    let area = (b[0] - a[0]) * d[1] - (b[1] - a[1]) * d[0];
    let mut positive = false;
    let mut negative = false;
    let mut distance = f32::INFINITY;
    for i in 0..4 {
        let x = points[i];
        let y = points[(i + 1) % 4];
        let cross = (y[0] - x[0]) * (p[1] - x[1]) - (y[1] - x[1]) * (p[0] - x[0]);
        positive |= cross > 0.0;
        negative |= cross < 0.0;
        distance = distance.min(segment_distance(p, x, y));
    }
    if area != 0.0 && !(positive && negative) {
        0.0
    } else {
        distance
    }
}

/// Resolve simultaneous translations in stable mover/entity order. All movers
/// advance together in increments at most 1/64 tile. Circle obstacles describe
/// static and moving map objects, not entities; entities are supplied separately.
/// Rotations, elevation changes, chain pushing and sub-tile vertical geometry
/// are unsupported; callers must treat a blocked or invalid result as no-op.
pub fn propose(
    world: &World,
    circles: &[CollisionCircle],
    entities: &[PushEntity],
    translations: &[Translation],
) -> Result<MotionProposal, MotionError> {
    if translations.len() > MAX_MOVERS
        || entities.len() > MAX_ENTITIES
        || circles.len() > MAX_OBSTACLES
        || world.walls.len() > MAX_OBSTACLES
    {
        return Err(MotionError::TooManyInputs);
    }
    if world
        .walls
        .iter()
        .any(|w| !finite_position(w.a) || !finite_position(w.b) || w.a == w.b)
        || circles.iter().any(|c| {
            !finite_position(c.center)
                || !c.radius.is_finite()
                || c.radius <= 0.0
                || c.radius > 64.0
        })
    {
        return Err(MotionError::InvalidGeometry);
    }
    let mut ids = BTreeSet::new();
    for e in entities {
        if !ids.insert(e.id) {
            return Err(MotionError::DuplicateEntity);
        }
        if !finite_position(e.position) || !radius_valid(e.radius) || !valid_vertical(e.vertical) {
            return Err(MotionError::InvalidEntity);
        }
        if e.id == EntityId::Player && world.player.position != e.position {
            return Err(MotionError::InvalidEntity);
        }
    }
    let mut seen = BTreeSet::new();
    let mut steps = 1;
    for m in translations {
        if !seen.insert(m.mover) {
            return Err(MotionError::DuplicateMover);
        }
        if !finite_position(m.delta) || !valid_vertical(m.vertical) {
            return Err(MotionError::InvalidMotion);
        }
        match m.mover {
            MoverId::Wall(index) if index >= world.walls.len() => {
                return Err(MotionError::MissingMover)
            }
            MoverId::Circle(index) if index >= circles.len() => {
                return Err(MotionError::MissingMover)
            }
            _ => {}
        }
        let point_after = |p: [f32; 2]| finite_position([p[0] + m.delta[0], p[1] + m.delta[1]]);
        let valid_end = match m.mover {
            MoverId::Wall(i) => point_after(world.walls[i].a) && point_after(world.walls[i].b),
            MoverId::Circle(i) => point_after(circles[i].center),
        };
        if !valid_end {
            return Err(MotionError::InvalidMotion);
        }
        let distance = m.delta[0].hypot(m.delta[1]);
        if distance > SUBSTEP * MAX_STEPS as f32 {
            return Err(MotionError::ExceedsSweepLimit);
        }
        steps = steps.max((distance / SUBSTEP).ceil() as usize);
    }
    let work = steps
        .checked_mul(translations.len())
        .and_then(|n| n.checked_mul(entities.len()))
        .and_then(|n| n.checked_mul(world.walls.len() + circles.len() + entities.len()))
        .ok_or(MotionError::WorkLimit)?;
    if work > MAX_WORK {
        return Err(MotionError::WorkLimit);
    }
    let mut movers = translations.to_vec();
    movers.sort_by_key(|m| m.mover);
    let mut staged_world = world.clone();
    let mut staged_circles = circles.to_vec();
    let mut staged_entities = entities.to_vec();
    staged_entities.sort_by_key(|e| e.id);
    for _ in 0..steps {
        for m in &movers {
            let d = [m.delta[0] / steps as f32, m.delta[1] / steps as f32];
            match m.mover {
                MoverId::Wall(i) => {
                    let wall = &mut staged_world.walls[i];
                    for p in [&mut wall.a, &mut wall.b] {
                        p[0] += d[0];
                        p[1] += d[1];
                    }
                }
                MoverId::Circle(i) => {
                    staged_circles[i].center[0] += d[0];
                    staged_circles[i].center[1] += d[1];
                }
            }
        }
        for mover in &movers {
            for index in 0..staged_entities.len() {
                let entity = staged_entities[index];
                if !overlap_z(entity.vertical, mover.vertical) {
                    continue;
                }
                let (center, combined_radius) = match mover.mover {
                    MoverId::Wall(i) => {
                        let wall = staged_world.walls[i];
                        if !wall.blocks_movement {
                            continue;
                        }
                        (
                            nearest_on_segment(entity.position, wall.a, wall.b),
                            entity.radius,
                        )
                    }
                    MoverId::Circle(i) => (
                        staged_circles[i].center,
                        entity.radius + staged_circles[i].radius,
                    ),
                };
                let diff = [
                    entity.position[0] - center[0],
                    entity.position[1] - center[1],
                ];
                let distance = diff[0].hypot(diff[1]);
                if distance >= combined_radius {
                    // A grazing contact can begin and end between samples.
                    // Refuse that motion instead of silently tunnelling; its
                    // exact push response requires a continuous-time solver.
                    let d = [mover.delta[0] / steps as f32, mover.delta[1] / steps as f32];
                    let (start_distance, swept_distance) = match mover.mover {
                        MoverId::Wall(i) => {
                            let wall = staged_world.walls[i];
                            let a = [wall.a[0] - d[0], wall.a[1] - d[1]];
                            let b = [wall.b[0] - d[0], wall.b[1] - d[1]];
                            (
                                segment_distance(entity.position, a, b),
                                swept_wall_distance(entity.position, a, b, d),
                            )
                        }
                        MoverId::Circle(i) => {
                            let end = staged_circles[i].center;
                            let start = [end[0] - d[0], end[1] - d[1]];
                            (
                                (entity.position[0] - start[0])
                                    .hypot(entity.position[1] - start[1]),
                                segment_distance(entity.position, start, end),
                            )
                        }
                    };
                    if start_distance >= combined_radius && swept_distance < combined_radius {
                        return Ok(MotionProposal::Blocked {
                            mover: mover.mover,
                            entity: entity.id,
                        });
                    }
                    continue;
                }
                if mover.delta == [0.0, 0.0] {
                    continue;
                }
                let direction = if distance > 1e-7 {
                    [diff[0] / distance, diff[1] / distance]
                } else {
                    let length = mover.delta[0].hypot(mover.delta[1]);
                    if length <= 1e-7 {
                        return Ok(MotionProposal::Blocked {
                            mover: mover.mover,
                            entity: entity.id,
                        });
                    }
                    [mover.delta[0] / length, mover.delta[1] / length]
                };
                // Initial overlap can be a legitimate transient. A stationary
                // or receding mover must not eject the entity or report a pin.
                if mover.delta[0] * direction[0] + mover.delta[1] * direction[1] <= 0.0 {
                    continue;
                }
                let push = combined_radius - distance + EPSILON;
                let wanted = [direction[0] * push, direction[1] * push];
                let mut collision_world = staged_world.clone();
                if let MoverId::Wall(i) = mover.mover {
                    collision_world.walls[i].blocks_movement = false;
                }
                let mut obstacles: Vec<_> = staged_circles
                    .iter()
                    .enumerate()
                    .filter_map(|(i, c)| (mover.mover != MoverId::Circle(i)).then_some(*c))
                    .collect();
                obstacles.extend(
                    staged_entities
                        .iter()
                        .enumerate()
                        .filter(|(i, e)| *i != index && overlap_z(e.vertical, entity.vertical))
                        .map(|(_, e)| CollisionCircle {
                            center: e.position,
                            radius: e.radius,
                        }),
                );
                let arrived =
                    collision_world.move_circle(entity.position, wanted, entity.radius, &obstacles);
                let desired = [
                    entity.position[0] + wanted[0],
                    entity.position[1] + wanted[1],
                ];
                if (arrived[0] - desired[0]).hypot(arrived[1] - desired[1]) > EPSILON {
                    return Ok(MotionProposal::Blocked {
                        mover: mover.mover,
                        entity: entity.id,
                    });
                }
                if !finite_position(arrived) {
                    return Err(MotionError::InvalidEntity);
                }
                staged_entities[index].position = arrived;
                if entity.id == EntityId::Player {
                    staged_world.player.position = arrived;
                }
            }
        }
    }
    Ok(MotionProposal::Committed(CommittedMotion {
        world: staged_world,
        circles: staged_circles,
        entities: staged_entities,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::{Player, Wall};
    fn wall(x: f32) -> Wall {
        Wall {
            a: [x, 0.0],
            b: [x, 2.0],
            texture: 1,
            blocks_movement: true,
            cell: None,
        }
    }
    fn entity(id: EntityId, x: f32, y: f32) -> PushEntity {
        PushEntity {
            id,
            position: [x, y],
            radius: 0.2,
            vertical: [0.0, 1.0],
        }
    }
    fn movement(id: MoverId, dx: f32) -> Translation {
        Translation {
            mover: id,
            delta: [dx, 0.0],
            vertical: [0.0, 2.0],
        }
    }
    fn world() -> World {
        World {
            walls: vec![wall(1.0)],
            player: Player {
                position: [1.3, 1.0],
                angle: 0.0,
            },
        }
    }
    #[test]
    fn pushes_player_and_leaves_input_unchanged() {
        let world = world();
        let result = propose(
            &world,
            &[],
            &[entity(EntityId::Player, 1.3, 1.0)],
            &[movement(MoverId::Wall(0), 0.5)],
        )
        .unwrap();
        let MotionProposal::Committed(result) = result else {
            panic!("expected push")
        };
        assert!((result.world.walls[0].a[0] - 1.5).abs() < 0.001);
        assert!(result.entities[0].position[0] > 1.69);
        assert_eq!(world.walls[0].a[0], 1.0);
        assert_eq!(world.player.position, [1.3, 1.0]);
        assert_eq!(result.world.player.position, result.entities[0].position);
    }
    #[test]
    fn stationary_and_receding_movers_do_not_push_existing_overlap() {
        let mut world = world();
        world.player.position = [1.1, 1.0];
        let player = entity(EntityId::Player, 1.1, 1.0);
        for delta in [0.0, -0.1] {
            let MotionProposal::Committed(result) =
                propose(&world, &[], &[player], &[movement(MoverId::Wall(0), delta)]).unwrap()
            else {
                panic!("receding motion cannot pin")
            };
            assert_eq!(result.entities[0], player);
        }
        let circle = CollisionCircle {
            center: [2.0, 1.0],
            radius: 0.3,
        };
        let actor = entity(EntityId::Actor(1), 2.3, 1.0);
        let MotionProposal::Committed(result) = propose(
            &world,
            &[circle],
            &[actor],
            &[movement(MoverId::Circle(0), -0.1)],
        )
        .unwrap() else {
            panic!("receding circle cannot pin")
        };
        assert_eq!(result.entities[0], actor);
    }

    #[test]
    fn grazing_swept_endpoints_and_circles_fail_closed() {
        let mut world = world();
        world.walls[0].a = [1.0, 1.0];
        world.walls[0].b = [1.0, 2.0];
        let actor = entity(EntityId::Actor(1), 1.0078125, 0.80001);
        assert!(matches!(
            propose(
                &world,
                &[],
                &[actor],
                &[movement(MoverId::Wall(0), 1.0 / 64.0)]
            )
            .unwrap(),
            MotionProposal::Blocked { .. }
        ));
        let circle = CollisionCircle {
            center: [1.0, 1.0],
            radius: 0.1,
        };
        let actor = entity(EntityId::Actor(1), 1.0078125, 0.70001);
        assert!(matches!(
            propose(
                &world,
                &[circle],
                &[actor],
                &[movement(MoverId::Circle(0), 1.0 / 64.0)]
            )
            .unwrap(),
            MotionProposal::Blocked { .. }
        ));
    }

    #[test]
    fn blocked_push_returns_only_pin_and_never_publishes_prefix() {
        let mut world = world();
        world.walls.push(wall(1.8));
        let entities = [entity(EntityId::Player, 1.3, 1.0)];
        let result = propose(&world, &[], &entities, &[movement(MoverId::Wall(0), 0.5)]).unwrap();
        assert!(matches!(
            result,
            MotionProposal::Blocked {
                mover: MoverId::Wall(0),
                entity: EntityId::Player
            }
        ));
        assert_eq!(world.walls[0].a[0], 1.0);
        assert_eq!(entities[0].position, [1.3, 1.0]);
    }
    #[test]
    fn multiple_movers_and_entities_are_order_independent() {
        let world = world();
        let circles = [CollisionCircle {
            center: [3.0, 1.0],
            radius: 0.2,
        }];
        let entities = [
            entity(EntityId::Actor(8), 3.45, 1.0),
            entity(EntityId::Player, 1.3, 1.0),
        ];
        let motions = [
            movement(MoverId::Circle(0), 0.5),
            movement(MoverId::Wall(0), 0.5),
        ];
        let MotionProposal::Committed(a) = propose(&world, &circles, &entities, &motions).unwrap()
        else {
            panic!("expected push")
        };
        let MotionProposal::Committed(b) = propose(
            &world,
            &circles,
            &[entities[1], entities[0]],
            &[motions[1], motions[0]],
        )
        .unwrap() else {
            panic!("expected push")
        };
        assert_eq!(a.entities, b.entities);
        assert!(a.entities[1].position[0] > 3.89);
        assert_eq!(a.circles, b.circles);
    }
    #[test]
    fn vertically_separated_entities_are_unchanged() {
        let world = world();
        let mut player = entity(EntityId::Player, 1.3, 1.0);
        player.vertical = [3.0, 4.0];
        let MotionProposal::Committed(result) =
            propose(&world, &[], &[player], &[movement(MoverId::Wall(0), 0.5)]).unwrap()
        else {
            panic!("must not collide")
        };
        assert_eq!(result.entities[0], player);
    }
    #[test]
    fn rejects_bad_inputs_and_oversized_sweeps() {
        let world = world();
        let mut e = entity(EntityId::Player, 1.3, 1.0);
        e.radius = f32::NAN;
        assert!(matches!(
            propose(&world, &[], &[e], &[]),
            Err(MotionError::InvalidEntity)
        ));
        assert!(matches!(
            propose(&world, &[], &[], &[movement(MoverId::Wall(0), 4.01)]),
            Err(MotionError::ExceedsSweepLimit)
        ));
        assert!(matches!(
            propose(
                &world,
                &[],
                &[],
                &[
                    movement(MoverId::Wall(0), 0.1),
                    movement(MoverId::Wall(0), 0.1)
                ]
            ),
            Err(MotionError::DuplicateMover)
        ));
    }
}
