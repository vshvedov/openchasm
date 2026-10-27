//! Rendering-independent exploratory geometry and movement.
//! Distances are in map tiles. Movement has not been matched to the original game.

#[derive(Clone, Copy, Debug)]
pub struct Wall {
    pub a: [f32; 2],
    pub b: [f32; 2],
    pub texture: u8,
    /// Whether this wall blocks the player. Visual opacity is independent.
    pub blocks_movement: bool,
    /// Original map cell, used by map processes to address the wall.
    pub cell: Option<[u8; 2]>,
}

/// A horizontal collision circle provided by the authoritative session.
/// The caller filters dead, inactive and vertically separated entities.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CollisionCircle {
    pub center: [f32; 2],
    pub radius: f32,
}

#[derive(Clone, Copy, Debug)]
pub struct Player {
    pub position: [f32; 2],
    pub angle: f32,
}

#[derive(Default, Clone, Copy, Debug)]
pub struct Input {
    pub forward: f32,
    pub strafe: f32,
    pub turn: f32,
}

#[derive(Clone)]
pub struct World {
    pub walls: Vec<Wall>,
    pub player: Player,
}

fn cross(a: [f32; 2], b: [f32; 2]) -> f32 {
    a[0] * b[1] - a[1] * b[0]
}
fn sub(a: [f32; 2], b: [f32; 2]) -> [f32; 2] {
    [a[0] - b[0], a[1] - b[1]]
}

const PLAYER_RADIUS: f32 = 0.18;
const CONTACT_SLOP: f32 = 0.0001;

fn dot(a: [f32; 2], b: [f32; 2]) -> f32 {
    a[0] * b[0] + a[1] * b[1]
}
fn add(a: [f32; 2], b: [f32; 2]) -> [f32; 2] {
    [a[0] + b[0], a[1] + b[1]]
}
fn scale(a: [f32; 2], k: f32) -> [f32; 2] {
    [a[0] * k, a[1] * k]
}

/// Earliest entry time and outward contact normal of a moving disc against a segment.
fn sweep_wall(
    origin: [f32; 2],
    delta: [f32; 2],
    wall: &Wall,
    radius: f32,
) -> Option<(f32, [f32; 2])> {
    let edge = sub(wall.b, wall.a);
    let edge_length_sq = dot(edge, edge);
    if edge_length_sq < 1e-12 {
        return None;
    }
    let edge_length = edge_length_sq.sqrt();
    let tangent = scale(edge, 1.0 / edge_length);
    let normal = [-tangent[1], tangent[0]];
    let mut best: Option<(f32, [f32; 2])> = None;
    let mut consider = |time: f32, n: [f32; 2]| {
        if (0.0..=1.0).contains(&time)
            && dot(delta, n) < -1e-7
            && best.as_ref().is_none_or(|(t, _)| time < *t)
        {
            best = Some((time, n));
        }
    };

    // Only block motion further into an existing overlap; allow moving out.
    let relative = sub(origin, wall.a);
    let closest = add(
        wall.a,
        scale(tangent, dot(relative, tangent).clamp(0.0, edge_length)),
    );
    let separation = sub(origin, closest);
    let separation_sq = dot(separation, separation);
    if separation_sq < radius * radius {
        if separation_sq > 1e-12 {
            consider(0.0, scale(separation, 1.0 / separation_sq.sqrt()));
        } else {
            // At a segment center line, both escape directions should be possible.
            return None;
        }
    }

    // Intersections with the two infinite offset lines, constrained to the segment.
    let initial_distance = dot(relative, normal);
    let travel_normal = dot(delta, normal);
    if travel_normal.abs() > 1e-8 {
        for side in [-1.0, 1.0] {
            let time = (side * radius - initial_distance) / travel_normal;
            if (0.0..=1.0).contains(&time) {
                let contact = add(relative, scale(delta, time));
                let along = dot(contact, tangent);
                if (0.0..=edge_length).contains(&along) {
                    consider(time, scale(normal, side));
                }
            }
        }
    }

    // The two segment endpoints complete the capsule around the wall.
    let speed_sq = dot(delta, delta);
    if speed_sq > 1e-12 {
        for point in [wall.a, wall.b] {
            let offset = sub(origin, point);
            let b = dot(offset, delta);
            let c = dot(offset, offset) - radius * radius;
            let discriminant = b * b - speed_sq * c;
            if discriminant >= 0.0 {
                let time = (-b - discriminant.sqrt()) / speed_sq;
                if (0.0..=1.0).contains(&time) {
                    let contact = add(offset, scale(delta, time));
                    let length = dot(contact, contact).sqrt();
                    if length > 1e-8 {
                        consider(time, scale(contact, 1.0 / length));
                    }
                }
            }
        }
    }
    best
}

/// Earliest contact of a swept player disc with one stationary actor circle.
fn sweep_circle(
    origin: [f32; 2],
    delta: [f32; 2],
    circle: &CollisionCircle,
    radius: f32,
) -> Option<(f32, [f32; 2])> {
    if !circle.radius.is_finite()
        || circle.radius < 0.0
        || !circle.center.iter().all(|value| value.is_finite())
    {
        return None;
    }
    let combined = radius + circle.radius;
    let combined_sq = combined * combined;
    let offset = sub(origin, circle.center);
    let distance_sq = dot(offset, offset);
    let travel_sq = dot(delta, delta);
    if !combined_sq.is_finite() || !distance_sq.is_finite() || travel_sq < 1e-12 {
        return None;
    }
    if distance_sq < combined_sq {
        // An existing overlap must not trap the player: only motion further
        // inward is blocked. Exactly coincident centers may move out freely.
        if distance_sq < 1e-12 {
            return None;
        }
        let normal = scale(offset, 1.0 / distance_sq.sqrt());
        return (dot(delta, normal) < -1e-7).then_some((0.0, normal));
    }
    let b = dot(offset, delta);
    let c = distance_sq - combined_sq;
    let discriminant = b * b - travel_sq * c;
    if discriminant < 0.0 {
        return None;
    }
    let time = (-b - discriminant.sqrt()) / travel_sq;
    if !(0.0..=1.0).contains(&time) {
        return None;
    }
    let at_contact = add(offset, scale(delta, time));
    let length_sq = dot(at_contact, at_contact);
    if length_sq < 1e-12 {
        return None;
    }
    let normal = scale(at_contact, 1.0 / length_sq.sqrt());
    (dot(delta, normal) < -1e-7).then_some((time, normal))
}

fn move_and_slide(
    position: [f32; 2],
    displacement: [f32; 2],
    walls: &[Wall],
    circles: &[CollisionCircle],
) -> [f32; 2] {
    move_circle_and_slide(position, displacement, PLAYER_RADIUS, walls, circles)
}

fn move_circle_and_slide(
    mut position: [f32; 2],
    mut displacement: [f32; 2],
    radius: f32,
    walls: &[Wall],
    circles: &[CollisionCircle],
) -> [f32; 2] {
    // Multiple contacts let a disc slide along walls and stop at corners.
    for _ in 0..4 {
        let travel_sq = dot(displacement, displacement);
        if travel_sq < 1e-12 {
            break;
        }
        let hit = walls
            .iter()
            .filter(|wall| wall.blocks_movement)
            .filter_map(|wall| sweep_wall(position, displacement, wall, radius))
            .chain(
                circles
                    .iter()
                    .filter_map(|circle| sweep_circle(position, displacement, circle, radius)),
            )
            .min_by(|a, b| a.0.total_cmp(&b.0));
        let Some((time, normal)) = hit else {
            position = add(position, displacement);
            break;
        };
        let safe_time = (time - CONTACT_SLOP / travel_sq.sqrt()).max(0.0);
        position = add(position, scale(displacement, safe_time));
        let remaining = scale(displacement, 1.0 - time);
        // Remove only the component directed into the contact surface.
        displacement = sub(remaining, scale(normal, dot(remaining, normal).min(0.0)));
    }
    position
}

impl World {
    /// Gather ray intersections from far to near for transparent compositing.
    /// Distance is perpendicular to the camera plane for an unnormalized camera ray.
    pub fn ray_hits(&self, direction: [f32; 2], hits: &mut Vec<(f32, usize, f32)>) {
        hits.clear();
        for (index, wall) in self.walls.iter().enumerate() {
            let segment = sub(wall.b, wall.a);
            let denominator = cross(direction, segment);
            if denominator.abs() < 0.00001 {
                continue;
            }
            let offset = sub(wall.a, self.player.position);
            let distance = cross(offset, segment) / denominator;
            let along = cross(offset, direction) / denominator;
            if distance > 0.001 && (0.0..=1.0).contains(&along) {
                hits.push((distance, index, along));
            }
        }
        hits.sort_unstable_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
    }

    /// Move a disc using the same swept wall and circle collision as the player.
    pub fn move_circle(
        &self,
        position: [f32; 2],
        displacement: [f32; 2],
        radius: f32,
        circles: &[CollisionCircle],
    ) -> [f32; 2] {
        if !position
            .iter()
            .chain(displacement.iter())
            .all(|v| v.is_finite())
            || !radius.is_finite()
            || radius <= 0.0
        {
            return position;
        }
        move_circle_and_slide(position, displacement, radius, &self.walls, circles)
    }

    /// Advance a bounded time step using a swept circular player and wall sliding.
    /// Gameplay values and material rules remain provisional pending comparison.
    pub fn step(&mut self, input: Input, dt: f32) {
        self.step_with_circles(input, dt, &[]);
    }

    /// Advance with walls and caller-selected stationary actor circles. This is
    /// a 2D approximation; circles should include only live, vertically
    /// overlapping actors. The input time cap and movement constants match step.
    pub fn step_with_circles(&mut self, input: Input, dt: f32, circles: &[CollisionCircle]) {
        self.step_with_external(input, dt, circles, [0.0, 0.0]);
    }

    /// Combine external displacement with input before sweeping for collisions.
    pub fn step_with_external(
        &mut self,
        input: Input,
        dt: f32,
        circles: &[CollisionCircle],
        external: [f32; 2],
    ) {
        if !external.iter().all(|v| v.is_finite())
            || !self.player.angle.is_finite()
            || !self.player.position.iter().all(|value| value.is_finite())
            || !dt.is_finite()
            || dt <= 0.0
            || !input.forward.is_finite()
            || !input.strafe.is_finite()
            || !input.turn.is_finite()
        {
            return;
        }
        let dt = dt.min(1.0 / 30.0);
        self.player.angle += input.turn.clamp(-1.0, 1.0) * 2.0 * dt;
        let (sin, cos) = self.player.angle.sin_cos();
        let mut forward = input.forward.clamp(-1.0, 1.0);
        let mut strafe = input.strafe.clamp(-1.0, 1.0);
        let magnitude = (forward * forward + strafe * strafe).sqrt();
        if magnitude > 1.0 {
            forward /= magnitude;
            strafe /= magnitude;
        }
        let displacement = [
            (cos * forward - sin * strafe) * 2.5 * dt + external[0],
            (sin * forward + cos * strafe) * 2.5 * dt + external[1],
        ];
        self.player.position =
            move_and_slide(self.player.position, displacement, &self.walls, circles);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn test_world() -> World {
        World {
            walls: vec![Wall {
                a: [1.0, -1.0],
                b: [1.0, 1.0],
                texture: 1,
                blocks_movement: true,
                cell: None,
            }],
            player: Player {
                position: [0.0, 0.0],
                angle: 0.0,
            },
        }
    }
    #[test]
    fn ray_hits_segment_and_ignores_opposite_direction() {
        let w = test_world();
        let mut hits = Vec::new();
        w.ray_hits([1.0, 0.0], &mut hits);
        assert_eq!(hits, [(1.0, 0, 0.5)]);
        w.ray_hits([-1.0, 0.0], &mut hits);
        assert!(hits.is_empty());
        w.ray_hits([0.0, 1.0], &mut hits);
        assert!(hits.is_empty());
    }
    #[test]
    fn nonblocking_wall_allows_passage_but_invisible_wall_blocks() {
        let mut w = test_world();
        w.walls[0].texture = 87;
        w.walls[0].blocks_movement = false;
        assert_eq!(
            move_and_slide([0.0, 0.0], [2.0, 0.0], &w.walls, &[]),
            [2.0, 0.0]
        );
        w.walls[0].texture = 120;
        w.walls[0].blocks_movement = true;
        let end = move_and_slide([0.0, 0.0], [2.0, 0.0], &w.walls, &[]);
        assert!(end[0] < 0.83);
    }
    #[test]
    fn movement_stops_before_wall() {
        let mut w = test_world();
        for _ in 0..120 {
            w.step(
                Input {
                    forward: 1.0,
                    ..Input::default()
                },
                1.0 / 60.0,
            );
        }
        assert!(w.player.position[0] < 0.83 && w.player.position[0] > 0.7);
    }
    #[test]
    fn swept_motion_cannot_tunnel_through_a_wall() {
        let w = test_world();
        let end = move_and_slide([0.0, 0.0], [4.0, 0.0], &w.walls, &[]);
        assert!(end[0] > 0.81 && end[0] < 0.821, "{end:?}");
    }
    #[test]
    fn slides_along_diagonal_wall() {
        let wall = Wall {
            a: [0.0, 0.0],
            b: [4.0, 4.0],
            texture: 1,
            blocks_movement: true,
            cell: None,
        };
        let end = move_and_slide([1.0, 0.5], [0.0, 1.0], &[wall], &[]);
        assert!(end[0] > 1.2 && end[1] > 0.7, "{end:?}");
        assert!(
            end[0] - end[1] >= PLAYER_RADIUS * 2.0_f32.sqrt() - 0.001,
            "{end:?}"
        );
    }
    #[test]
    fn prevents_cutting_across_a_segment_endpoint() {
        let wall = Wall {
            a: [1.0, 0.0],
            b: [1.0, 1.0],
            texture: 1,
            blocks_movement: true,
            cell: None,
        };
        let end = move_and_slide([0.0, -0.1], [2.0, 0.0], &[wall], &[]);
        assert!(end[1] < -0.18, "{end:?}");
        assert!(end[0] < 1.9, "{end:?}");
    }
    #[test]
    fn player_can_move_out_of_existing_overlap() {
        let wall = Wall {
            a: [1.0, -2.0],
            b: [1.0, 2.0],
            texture: 1,
            blocks_movement: true,
            cell: None,
        };
        let end = move_and_slide([0.9, 0.0], [-0.5, 0.0], &[wall], &[]);
        assert!((end[0] - 0.4).abs() < 1e-6, "{end:?}");
    }
    #[test]
    fn swept_player_cannot_tunnel_through_an_actor_circle() {
        let circle = CollisionCircle {
            center: [1.0, 0.0],
            radius: 0.2,
        };
        let end = move_and_slide([0.0, 0.0], [4.0, 0.0], &[], &[circle]);
        assert!((end[0] - 0.62).abs() < 0.001, "{end:?}");
    }
    #[test]
    fn sliding_around_actor_preserves_clearance() {
        let circle = CollisionCircle {
            center: [1.0, 0.15],
            radius: 0.2,
        };
        let end = move_and_slide([0.0, 0.0], [2.0, 0.0], &[], &[circle]);
        assert!(end[0] > 0.8 && end[1] < -0.15, "{end:?}");
        let distance = (end[0] - circle.center[0]).hypot(end[1] - circle.center[1]);
        assert!(distance >= PLAYER_RADIUS + circle.radius - 1e-4, "{end:?}");
    }
    #[test]
    fn nearest_wall_or_actor_blocks_regardless_of_list_order() {
        let wall = Wall {
            a: [1.0, -1.0],
            b: [1.0, 1.0],
            texture: 1,
            blocks_movement: true,
            cell: None,
        };
        let behind = CollisionCircle {
            center: [2.0, 0.0],
            radius: 0.2,
        };
        let end = move_and_slide([0.0, 0.0], [4.0, 0.0], &[wall], &[behind]);
        assert!((end[0] - 0.82).abs() < 0.001, "{end:?}");
        let in_front = CollisionCircle {
            center: [0.8, 0.0],
            radius: 0.2,
        };
        let end = move_and_slide([0.0, 0.0], [4.0, 0.0], &[wall], &[behind, in_front]);
        assert!((end[0] - 0.42).abs() < 0.001, "{end:?}");
    }
    #[test]
    fn actor_overlap_allows_escape_but_blocks_deeper_motion() {
        let circle = CollisionCircle {
            center: [1.0, 0.0],
            radius: 0.2,
        };
        let escape = move_and_slide([0.8, 0.0], [-0.5, 0.0], &[], &[circle]);
        assert!((escape[0] - 0.3).abs() < 1e-5, "{escape:?}");
        let inward = move_and_slide([0.8, 0.0], [0.5, 0.0], &[], &[circle]);
        assert!((inward[0] - 0.8).abs() < 1e-5, "{inward:?}");
        let coincident = move_and_slide([1.0, 0.0], [0.5, 0.0], &[], &[circle]);
        assert!((coincident[0] - 1.5).abs() < 1e-5, "{coincident:?}");
    }
    #[test]
    fn invalid_actor_circles_cannot_corrupt_player_state() {
        let circles = [
            CollisionCircle {
                center: [f32::NAN, 0.0],
                radius: 0.2,
            },
            CollisionCircle {
                center: [1.0, 0.0],
                radius: -1.0,
            },
            CollisionCircle {
                center: [1.0, 0.0],
                radius: f32::INFINITY,
            },
        ];
        let end = move_and_slide([0.0, 0.0], [2.0, 0.0], &[], &circles);
        assert_eq!(end, [2.0, 0.0]);
    }
    #[test]
    fn public_actor_step_filters_only_the_supplied_circles() {
        let mut w = test_world();
        w.walls.clear();
        let circle = CollisionCircle {
            center: [1.0, 0.0],
            radius: 0.2,
        };
        let input = Input {
            forward: 1.0,
            ..Input::default()
        };
        for _ in 0..60 {
            w.step_with_circles(input, 1.0 / 60.0, &[circle]);
        }
        assert!((w.player.position[0] - 0.62).abs() < 0.001);
        w.step(input, 1.0 / 60.0);
        assert!(w.player.position[0] > 0.62);
    }
    #[test]
    fn invalid_input_does_not_corrupt_player_state() {
        let mut w = test_world();
        w.step(
            Input {
                forward: f32::NAN,
                ..Input::default()
            },
            1.0 / 60.0,
        );
        assert_eq!(w.player.position, [0.0, 0.0]);
        w.step(Input::default(), f32::NAN);
        assert_eq!(w.player.angle, 0.0);
    }
}
