//! Client-only, provisional explored-wall map. The reference reveals opaque
//! walls in view over time; exact remaster visibility and zoom are unknown.
use crate::world::World;
const RAYS: usize = 160;
const LEFT: i32 = 12;
const TOP: i32 = 55;
const WIDTH: i32 = 240;
const HEIGHT: i32 = 180;
const SCALE: f32 = 8.0;

#[derive(Clone)]
pub struct Minimap {
    visible: Vec<bool>,
    pub open: bool,
    observation: Option<ObservationKey>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct WallKey {
    endpoints: [u32; 4],
    texture: u8,
}
impl WallKey {
    fn of(wall: &crate::world::Wall) -> Self {
        Self {
            endpoints: [
                wall.a[0].to_bits(),
                wall.a[1].to_bits(),
                wall.b[0].to_bits(),
                wall.b[1].to_bits(),
            ],
            texture: wall.texture,
        }
    }
}
#[derive(Clone)]
struct ObservationKey {
    pose: [u32; 3],
    walls: Vec<WallKey>,
}
impl ObservationKey {
    fn pose(world: &World) -> [u32; 3] {
        [
            world.player.position[0].to_bits(),
            world.player.position[1].to_bits(),
            world.player.angle.to_bits(),
        ]
    }
    fn matches(&self, world: &World) -> bool {
        self.pose == Self::pose(world)
            && self.walls.len() == world.walls.len()
            && self
                .walls
                .iter()
                .zip(&world.walls)
                .all(|(saved, wall)| *saved == WallKey::of(wall))
    }
    fn new(world: &World) -> Self {
        Self {
            pose: Self::pose(world),
            walls: world.walls.iter().map(WallKey::of).collect(),
        }
    }
}
impl Minimap {
    pub fn new(world: &World) -> Self {
        Self {
            visible: vec![false; world.walls.len()],
            open: false,
            observation: None,
        }
    }
    /// Discover only the nearest opaque wall along each approximate view ray.
    /// Transparent and invisible walls do not occlude discovery, matching the
    /// independent reference. Actor and raised-wall occlusion are approximate.
    pub fn observe(&mut self, world: &World) {
        let player = world.player;
        if self.visible.len() != world.walls.len()
            || !player.angle.is_finite()
            || !player.position.iter().all(|x| x.is_finite())
        {
            return;
        }
        if self
            .observation
            .as_ref()
            .is_some_and(|key| key.matches(world))
        {
            return;
        }
        let (sin, cos) = player.angle.sin_cos();
        let mut hits = Vec::new();
        for x in 0..RAYS {
            let camera_x = (2.0 * (x as f32 + 0.5) / RAYS as f32 - 1.0) * 0.66;
            world.ray_hits([cos - sin * camera_x, sin + cos * camera_x], &mut hits);
            if let Some((_, index, _)) = hits
                .iter()
                .rev()
                .find(|(_, index, _)| (1..=86).contains(&world.walls[*index].texture))
            {
                self.visible[*index] = true;
            }
        }
        self.observation = Some(ObservationKey::new(world));
    }
    /// Draw current positions of discovered walls and a heading marker.
    pub fn draw(&self, frame: &mut [u32], width: usize, height: usize, world: &World) {
        if !self.open
            || width.checked_mul(height) != Some(frame.len())
            || self.visible.len() != world.walls.len()
            || !world.player.position.iter().all(|p| p.is_finite())
            || !world.player.angle.is_finite()
        {
            return;
        }
        for y in TOP..TOP + HEIGHT {
            for x in LEFT..LEFT + WIDTH {
                put(frame, width, height, x, y, 0x14202a);
            }
        }
        let center = [(LEFT + WIDTH / 2) as f32, (TOP + HEIGHT / 2) as f32];
        let project = |point: [f32; 2]| {
            [
                center[0] + (point[0] - world.player.position[0]) * SCALE,
                center[1] - (point[1] - world.player.position[1]) * SCALE,
            ]
        };
        for (wall, visible) in world.walls.iter().zip(&self.visible) {
            if *visible {
                line(
                    frame,
                    width,
                    height,
                    project(wall.a),
                    project(wall.b),
                    0xe3d7b1,
                );
            }
        }
        let (sin, cos) = world.player.angle.sin_cos();
        let tip = [center[0] + cos * 7.0, center[1] - sin * 7.0];
        let back = [center[0] - cos * 4.0, center[1] + sin * 4.0];
        let left = [back[0] - sin * 4.0, back[1] - cos * 4.0];
        let right = [back[0] + sin * 4.0, back[1] + cos * 4.0];
        line(frame, width, height, tip, left, 0x6dff70);
        line(frame, width, height, left, right, 0x6dff70);
        line(frame, width, height, right, tip, 0x6dff70);
    }
}
fn put(frame: &mut [u32], width: usize, height: usize, x: i32, y: i32, color: u32) {
    if (LEFT..LEFT + WIDTH).contains(&x)
        && (TOP..TOP + HEIGHT).contains(&y)
        && x >= 0
        && y >= 0
        && (x as usize) < width
        && (y as usize) < height
    {
        frame[y as usize * width + x as usize] = color;
    }
}
/// Clip first so huge or invalid endpoints cannot produce unbounded iteration.
fn line(frame: &mut [u32], width: usize, height: usize, a: [f32; 2], b: [f32; 2], color: u32) {
    if !a
        .iter()
        .chain(b.iter())
        .all(|v| v.is_finite() && v.abs() < 1_000_000.0)
    {
        return;
    }
    let delta = [b[0] - a[0], b[1] - a[1]];
    let mut lo = 0.0f32;
    let mut hi = 1.0f32;
    for (p, q) in [
        (-delta[0], a[0] - LEFT as f32),
        (delta[0], (LEFT + WIDTH - 1) as f32 - a[0]),
        (-delta[1], a[1] - TOP as f32),
        (delta[1], (TOP + HEIGHT - 1) as f32 - a[1]),
    ] {
        if p == 0.0 {
            if q < 0.0 {
                return;
            }
        } else {
            let ratio = q / p;
            if p < 0.0 {
                lo = lo.max(ratio);
            } else {
                hi = hi.min(ratio);
            }
            if lo > hi {
                return;
            }
        }
    }
    let start = [a[0] + lo * delta[0], a[1] + lo * delta[1]];
    let end = [a[0] + hi * delta[0], a[1] + hi * delta[1]];
    let steps = (end[0] - start[0])
        .abs()
        .max((end[1] - start[1]).abs())
        .ceil() as usize;
    for i in 0..=steps {
        let t = i as f32 / steps.max(1) as f32;
        put(
            frame,
            width,
            height,
            (start[0] + (end[0] - start[0]) * t).round() as i32,
            (start[1] + (end[1] - start[1]) * t).round() as i32,
            color,
        );
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::{Player, Wall};
    fn wall(x: f32, texture: u8) -> Wall {
        Wall {
            a: [x, -2.0],
            b: [x, 2.0],
            texture,
            blocks_movement: true,
            cell: None,
        }
    }
    #[test]
    fn reveals_only_nearest_opaque_wall_and_keeps_exploration() {
        let mut world = World {
            player: Player {
                position: [0.0, 0.0],
                angle: 0.0,
            },
            walls: vec![wall(1.0, 87), wall(2.0, 1), wall(3.0, 2)],
        };
        let mut map = Minimap::new(&world);
        map.observe(&world);
        assert_eq!(map.visible, [false, true, false]);
        let saved = map.clone();
        world.player.position = [2.5, 0.0];
        map.observe(&world);
        assert_eq!(map.visible, [false, true, true]);
        map = saved;
        assert_eq!(map.visible, [false, true, false]);
        assert_eq!(Minimap::new(&world).visible, [false; 3]);
    }
    #[test]
    fn current_geometry_is_drawn_and_clipping_is_bounded() {
        let mut world = World {
            player: Player {
                position: [0.0, 0.0],
                angle: 0.0,
            },
            walls: vec![wall(2.0, 1)],
        };
        let mut map = Minimap::new(&world);
        map.observe(&world);
        map.open = true;
        let mut first = vec![0; 640 * 400];
        map.draw(&mut first, 640, 400, &world);
        world.walls[0].a[0] = 4.0;
        world.walls[0].b[0] = 4.0;
        let mut moved = vec![0; 640 * 400];
        map.draw(&mut moved, 640, 400, &world);
        assert_ne!(first, moved);
        world.walls[0].a[0] = f32::NAN;
        map.draw(&mut moved, 640, 400, &world);
        assert_eq!(moved[0], 0);
    }
    #[test]
    fn observation_cache_rechecks_pose_geometry_and_material() {
        let mut world = World {
            player: Player {
                position: [0.0, 0.0],
                angle: 0.0,
            },
            walls: vec![wall(1.0, 1), wall(2.0, 2)],
        };
        let mut map = Minimap::new(&world);
        map.observe(&world);
        assert_eq!(map.visible, [true, false]);
        let saved = map.clone();
        // A cache hit leaves visibility untouched; simulate an externally
        // cleared bit to distinguish a hit from another raycast in this test.
        map.visible[0] = false;
        map.observe(&world);
        assert_eq!(map.visible, [false, false]);
        world.player.angle = 0.01;
        map.observe(&world);
        assert_eq!(map.visible, [true, false]);
        world.walls[0].texture = 87;
        map.observe(&world);
        assert_eq!(map.visible, [true, true]);
        map = saved;
        map.observe(&world);
        assert_eq!(map.visible, [true, true]);
        world.walls[0].texture = 1;
        let mut moved = Minimap::new(&world);
        moved.observe(&world);
        assert_eq!(moved.visible, [true, false]);
        world.walls[0].a[0] = 3.0;
        world.walls[0].b[0] = 3.0;
        moved.observe(&world);
        assert_eq!(moved.visible, [true, true]);
    }
}
