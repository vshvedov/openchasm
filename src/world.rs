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
fn sweep_wall(origin: [f32; 2], delta: [f32; 2], wall: &Wall) -> Option<(f32, [f32; 2])> {
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
    if separation_sq < PLAYER_RADIUS * PLAYER_RADIUS {
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
            let time = (side * PLAYER_RADIUS - initial_distance) / travel_normal;
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
            let c = dot(offset, offset) - PLAYER_RADIUS * PLAYER_RADIUS;
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

fn move_and_slide(mut position: [f32; 2], mut displacement: [f32; 2], walls: &[Wall]) -> [f32; 2] {
    // Multiple contacts let the player slide along walls and stop at corners.
    for _ in 0..4 {
        let travel_sq = dot(displacement, displacement);
        if travel_sq < 1e-12 {
            break;
        }
        let hit = walls
            .iter()
            .filter(|wall| wall.blocks_movement)
            .filter_map(|wall| sweep_wall(position, displacement, wall))
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

    /// Advance a bounded time step using a swept circular player and wall sliding.
    /// Gameplay values and material rules remain provisional pending comparison.
    pub fn step(&mut self, input: Input, dt: f32) {
        if !dt.is_finite()
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
            (cos * forward - sin * strafe) * 2.5 * dt,
            (sin * forward + cos * strafe) * 2.5 * dt,
        ];
        self.player.position = move_and_slide(self.player.position, displacement, &self.walls);
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
        assert_eq!(move_and_slide([0.0, 0.0], [2.0, 0.0], &w.walls), [2.0, 0.0]);
        w.walls[0].texture = 120;
        w.walls[0].blocks_movement = true;
        let end = move_and_slide([0.0, 0.0], [2.0, 0.0], &w.walls);
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
        let end = move_and_slide([0.0, 0.0], [4.0, 0.0], &w.walls);
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
        let end = move_and_slide([1.0, 0.5], [0.0, 1.0], &[wall]);
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
        let end = move_and_slide([0.0, -0.1], [2.0, 0.0], &[wall]);
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
        let end = move_and_slide([0.9, 0.0], [-0.5, 0.0], &[wall]);
        assert!((end[0] - 0.4).abs() < 1e-6, "{end:?}");
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
