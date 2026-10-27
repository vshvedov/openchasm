//! Small, rendering-independent geometry and movement prototype.
//! All distances are in map tiles. This is exploratory, not original game physics.

#[derive(Clone, Copy, Debug)]
pub struct Wall {
    pub a: [f32; 2],
    pub b: [f32; 2],
    pub texture: u8,
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

impl World {
    /// Return perpendicular distance and wall index of the nearest forward hit.
    pub fn cast_ray(&self, direction: [f32; 2]) -> Option<(f32, usize, f32)> {
        let mut nearest = f32::INFINITY;
        let mut hit = None;
        for (index, wall) in self.walls.iter().enumerate() {
            let segment = sub(wall.b, wall.a);
            let denominator = cross(direction, segment);
            if denominator.abs() < 0.00001 {
                continue;
            }
            let offset = sub(wall.a, self.player.position);
            let distance = cross(offset, segment) / denominator;
            let along = cross(offset, direction) / denominator;
            if distance > 0.001 && distance < nearest && (0.0..=1.0).contains(&along) {
                nearest = distance;
                hit = Some((distance, index, along));
            }
        }
        hit
    }

    fn clear(&self, point: [f32; 2]) -> bool {
        const RADIUS: f32 = 0.18;
        self.walls.iter().all(|wall| {
            let line = sub(wall.b, wall.a);
            let len_sq = line[0] * line[0] + line[1] * line[1];
            if len_sq < 0.000001 {
                return true;
            }
            let relative = sub(point, wall.a);
            let t = ((relative[0] * line[0] + relative[1] * line[1]) / len_sq).clamp(0.0, 1.0);
            let dx = point[0] - (wall.a[0] + t * line[0]);
            let dy = point[1] - (wall.a[1] + t * line[1]);
            dx * dx + dy * dy >= RADIUS * RADIUS
        })
    }

    /// Advance a bounded time step. Collision slides along walls, axis by axis.
    pub fn step(&mut self, input: Input, dt: f32) {
        let dt = dt.clamp(0.0, 1.0 / 30.0);
        self.player.angle += input.turn.clamp(-1.0, 1.0) * 2.0 * dt;
        let (sin, cos) = self.player.angle.sin_cos();
        let mut forward = input.forward.clamp(-1.0, 1.0);
        let mut strafe = input.strafe.clamp(-1.0, 1.0);
        let magnitude = (forward * forward + strafe * strafe).sqrt();
        if magnitude > 1.0 {
            forward /= magnitude;
            strafe /= magnitude;
        }
        let dx = (cos * forward - sin * strafe) * 2.5 * dt;
        let dy = (sin * forward + cos * strafe) * 2.5 * dt;
        let x = [self.player.position[0] + dx, self.player.position[1]];
        if self.clear(x) {
            self.player.position = x;
        }
        let y = [self.player.position[0], self.player.position[1] + dy];
        if self.clear(y) {
            self.player.position = y;
        }
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
        assert_eq!(w.cast_ray([1.0, 0.0]), Some((1.0, 0, 0.5)));
        assert_eq!(w.cast_ray([-1.0, 0.0]), None);
        assert_eq!(w.cast_ray([0.0, 1.0]), None);
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
}
