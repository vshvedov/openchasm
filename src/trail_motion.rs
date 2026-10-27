//! Adapt committed projectile motion to the client-only trail renderer.
//!
//! Reflection events contain an XY contact and tick-relative time, but no
//! incoming direction for a projectile spawned during that same tick. In that
//! case only the known portion after the first reflection is emitted. Terminal
//! projectiles absent from both snapshots similarly have no recoverable origin.
use std::collections::{BTreeMap, BTreeSet};

use crate::projectile_trails::ProjectileTrails;
use openchasm::session::{Session, SessionEvents};

const TICK_SECONDS: f32 = 1.0 / 60.0;

#[derive(Clone, Copy, Debug)]
struct Pose {
    rocket: usize,
    position: [f32; 3],
    velocity: [f32; 3],
    age: f32,
}

/// Projectile positions sampled immediately before an authoritative session tick.
#[derive(Default)]
pub struct TrailSnapshot(BTreeMap<u64, Pose>);

impl TrailSnapshot {
    pub fn capture(session: &Session) -> Self {
        Self(
            session
                .projectiles
                .active()
                .iter()
                .filter_map(|projectile| {
                    let z = session.projectile_height(projectile.id)?;
                    let pitch = session.projectile_pitch(projectile.id)?;
                    let horizontal = projectile.velocity[0].hypot(projectile.velocity[1]);
                    let pose = Pose {
                        rocket: projectile.rocket_id,
                        position: [projectile.position[0], projectile.position[1], z],
                        velocity: [
                            projectile.velocity[0],
                            projectile.velocity[1],
                            horizontal * pitch.tan(),
                        ],
                        age: projectile.elapsed_seconds(),
                    };
                    Some((projectile.id, pose))
                })
                .collect(),
        )
    }
}

/// Emit actual motion segments after a tick, then forget removed projectiles.
pub fn emit(
    trails: &mut ProjectileTrails,
    before: &TrailSnapshot,
    session: &Session,
    events: &SessionEvents,
) {
    let after = TrailSnapshot::capture(session);
    walk(before, &after, events, |event| match event {
        Motion::Segment(id, rocket, from, to) => trails.segment(id, rocket, from, to),
        Motion::Finish(id) => trails.finish(id),
    });
}

#[derive(Debug, PartialEq)]
enum Motion {
    Segment(u64, usize, [f32; 3], [f32; 3]),
    Finish(u64),
}

fn walk(
    before: &TrailSnapshot,
    after: &TrailSnapshot,
    events: &SessionEvents,
    mut emit: impl FnMut(Motion),
) {
    let mut ids: BTreeSet<u64> = before.0.keys().chain(after.0.keys()).copied().collect();
    ids.extend(
        events
            .projectile_impacts
            .iter()
            .map(|event| event.projectile_id),
    );
    ids.extend(
        events
            .projectile_reflections
            .iter()
            .map(|event| event.projectile_id),
    );
    for id in ids {
        let old = before.0.get(&id);
        let new = after.0.get(&id);
        let terminal = events
            .projectile_impacts
            .iter()
            .zip(&events.projectile_impact_heights)
            .find(|(impact, _)| impact.projectile_id == id);
        let reflections: Vec<_> = events
            .projectile_reflections
            .iter()
            .filter(|event| event.projectile_id == id)
            .collect();
        let Some(rocket) = old
            .map(|p| p.rocket)
            .or_else(|| new.map(|p| p.rocket))
            .or_else(|| terminal.map(|(event, _)| event.rocket_id))
            .or_else(|| reflections.first().map(|event| event.rocket_id))
        else {
            continue;
        };
        let velocity_z = old.or(new).map(|pose| pose.velocity[2]);
        let endpoint = terminal
            .map(|(event, z)| ([event.position[0], event.position[1], *z], event.time))
            .or_else(|| new.map(|pose| (pose.position, TICK_SECONDS)));
        let mut previous = old.map(|pose| pose.position).or_else(|| {
            // The current velocity is only the original velocity when no reflection occurred.
            if reflections.is_empty() {
                new.map(|pose| {
                    std::array::from_fn(|i| {
                        pose.position[i] - pose.velocity[i] * pose.age.min(TICK_SECONDS)
                    })
                })
            } else {
                None
            }
        });
        for reflection in reflections {
            let z = old
                .map(|pose| pose.position[2] + velocity_z.unwrap_or(0.0) * reflection.time)
                .or_else(|| {
                    endpoint
                        .zip(velocity_z)
                        .map(|((point, time), speed)| point[2] - speed * (time - reflection.time))
                });
            let Some(z) = z else {
                previous = None;
                continue;
            };
            let point = [reflection.position[0], reflection.position[1], z];
            if let Some(from) = previous {
                emit(Motion::Segment(id, rocket, from, point));
            }
            previous = Some(point);
        }
        if let (Some(from), Some((to, _))) = (previous, endpoint) {
            emit(Motion::Segment(id, rocket, from, to));
        }
        if new.is_none() {
            emit(Motion::Finish(id));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openchasm::projectiles::{Impact, ImpactTarget, Owner};
    use openchasm::session::ProjectileReflection;

    fn pose(position: [f32; 3], velocity: [f32; 3], age: f32) -> Pose {
        Pose {
            rocket: 3,
            position,
            velocity,
            age,
        }
    }
    fn snapshot(entries: &[(u64, Pose)]) -> TrailSnapshot {
        TrailSnapshot(entries.iter().copied().collect())
    }
    fn collect(
        before: &TrailSnapshot,
        after: &TrailSnapshot,
        events: &SessionEvents,
    ) -> Vec<Motion> {
        let mut result = Vec::new();
        walk(before, after, events, |event| result.push(event));
        result
    }
    fn impact(id: u64, position: [f32; 2], time: f32, target: ImpactTarget) -> Impact {
        Impact {
            projectile_id: id,
            owner: Owner::Player,
            rocket_id: 3,
            target,
            position,
            damage: 1,
            time,
        }
    }
    #[test]
    fn terminal_segment_and_expiry_finish_track() {
        let before = snapshot(&[(1, pose([0., 0., 1.], [1., 0., 0.], 1.))]);
        let after = TrailSnapshot::default();
        let mut events = SessionEvents::default();
        events
            .projectile_impacts
            .push(impact(1, [0.7, 0.], 0.01, ImpactTarget::Expired));
        events.projectile_impact_heights.push(1.2);
        assert_eq!(
            collect(&before, &after, &events),
            vec![
                Motion::Segment(1, 3, [0., 0., 1.], [0.7, 0., 1.2]),
                Motion::Finish(1)
            ]
        );
        events.projectile_impacts.clear();
        assert_eq!(collect(&before, &after, &events), vec![Motion::Finish(1)]);
    }
    #[test]
    fn reflection_splits_incoming_and_outgoing_motion() {
        let before = snapshot(&[(1, pose([0., 0., 1.], [1., 0., 2.], 1.))]);
        let after = snapshot(&[(1, pose([0., 1., 1.02], [0., 1., 2.], 1.02))]);
        let mut events = SessionEvents::default();
        events.projectile_reflections.push(ProjectileReflection {
            projectile_id: 1,
            rocket_id: 3,
            previous_owner: Owner::Player,
            position: [1., 0.],
            time: 0.005,
        });
        let result = collect(&before, &after, &events);
        assert_eq!(
            result,
            vec![
                Motion::Segment(1, 3, [0., 0., 1.], [1., 0., 1.01]),
                Motion::Segment(1, 3, [1., 0., 1.01], [0., 1., 1.02])
            ]
        );
    }
    #[test]
    fn newly_spawned_motion_is_reconstructed_but_not_across_reflection() {
        let before = TrailSnapshot::default();
        let after = snapshot(&[(2, pose([1., 2., 1.], [6., 0., 0.], 0.01))]);
        let mut events = SessionEvents::default();
        let result = collect(&before, &after, &events);
        assert_eq!(
            result,
            vec![Motion::Segment(2, 3, [0.94, 2., 1.], [1., 2., 1.])]
        );
        events.projectile_reflections.push(ProjectileReflection {
            projectile_id: 2,
            rocket_id: 3,
            previous_owner: Owner::Player,
            position: [1., 1.],
            time: 0.005,
        });
        assert_eq!(
            collect(&before, &after, &events),
            vec![Motion::Segment(2, 3, [1., 1., 1.], [1., 2., 1.])]
        );
    }
    #[test]
    fn new_terminal_without_incoming_pose_does_not_invent_a_path() {
        let mut events = SessionEvents::default();
        events
            .projectile_impacts
            .push(impact(5, [1., 2.], 0.005, ImpactTarget::Plane));
        events.projectile_impact_heights.push(2.);
        assert_eq!(
            collect(
                &TrailSnapshot::default(),
                &TrailSnapshot::default(),
                &events
            ),
            vec![Motion::Finish(5)]
        );
    }
}
