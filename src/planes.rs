//! Perspective floor and ceiling preview based on the level's tile grids.
//!
//! Geometry here is renderer-only: the camera height and lighting are chosen to
//! match the current wall preview, not yet validated against the original game.
use crate::map::{Floors, Map, FLOOR_TEXTURE_COUNT, FLOOR_TEXTURE_SIDE, MAP_SIDE};
use crate::object_render::CameraView;
use crate::sky::Sky;
use crate::texture::Palette;

const FLOOR_BACKGROUND: u32 = 0x38312b;
const SKY_BACKGROUND: u32 = 0x303847;
// The reference renderer treats both 62 and 63 as absent plane geometry.
const EMPTY_TILE: u8 = 63;
const SKY_TILE: u8 = 62;

/// Predecoded data for fast, allocation-free per-frame plane drawing.
pub struct PlaneRenderer {
    floor_ids: Vec<u8>,
    ceiling_ids: Vec<u8>,
    textures: Vec<u32>,
    sky: Option<Sky>,
}

impl PlaneRenderer {
    pub fn new(map: &Map, floors: &Floors, palette: &Palette) -> Self {
        let mut floor_ids = Vec::with_capacity(MAP_SIDE * MAP_SIDE);
        let mut ceiling_ids = Vec::with_capacity(MAP_SIDE * MAP_SIDE);
        for x in 0..MAP_SIDE {
            for y in 0..MAP_SIDE {
                floor_ids.push(map.floor(x, y).unwrap_or(EMPTY_TILE));
                ceiling_ids.push(map.ceiling(x, y).unwrap_or(EMPTY_TILE));
            }
        }
        let mut textures =
            Vec::with_capacity(FLOOR_TEXTURE_COUNT * FLOOR_TEXTURE_SIDE * FLOOR_TEXTURE_SIDE);
        for i in 0..FLOOR_TEXTURE_COUNT {
            // Floors::parse verifies the data contains all 64 textures.
            for &index in floors.texture(i).expect("valid floor texture index") {
                let [r, g, b] = palette.rgb8(index);
                textures.push((u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b));
            }
        }
        Self {
            floor_ids,
            ceiling_ids,
            textures,
            sky: None,
        }
    }

    pub fn set_sky(&mut self, sky: Sky) {
        self.sky = Some(sky);
    }

    /// Paint the floor and ceiling before drawing walls, using the same camera
    /// plane and projected wall height as the geometry preview. Invalid or
    /// off-map intersections retain a plain background.
    #[cfg(test)]
    pub fn render(&self, frame: &mut [u32], width: usize, height: usize, camera: CameraView) {
        self.render_lit(frame, width, height, camera, None);
    }

    /// Dynamic light adds a provisional boost at each world intersection.
    #[cfg(test)]
    pub fn render_lit(
        &self,
        frame: &mut [u32],
        width: usize,
        height: usize,
        camera: CameraView,
        lights: Option<&openchasm::dynamic_light::DynamicLightSampler>,
    ) {
        self.render_inner(frame, None, width, height, camera, lights);
    }

    /// Paint planes and write perpendicular depths for actual geometry only.
    pub fn render_with_depth(
        &self,
        frame: &mut [u32],
        depth: &mut [f32],
        width: usize,
        height: usize,
        camera: CameraView,
        lights: Option<&openchasm::dynamic_light::DynamicLightSampler>,
    ) {
        self.render_inner(frame, Some(depth), width, height, camera, lights);
    }

    fn render_inner(
        &self,
        frame: &mut [u32],
        mut depth: Option<&mut [f32]>,
        width: usize,
        height: usize,
        camera: CameraView,
        lights: Option<&openchasm::dynamic_light::DynamicLightSampler>,
    ) {
        let position = camera.player.position;
        let angle = camera.player.angle;
        if !camera.eye_height.is_finite()
            || width == 0
            || height == 0
            || width.checked_mul(height) != Some(frame.len())
            || depth
                .as_ref()
                .is_some_and(|buffer| buffer.len() != frame.len())
        {
            return;
        }
        let (sin, cos) = angle.sin_cos();
        let projection = height as f32 / 2.0;
        let horizon = camera.horizon(height);
        let camera_plane = [-sin * 0.66, cos * 0.66];
        for (y, row) in frame.chunks_mut(width).enumerate() {
            let ceiling = (y as f32 + 0.5) < horizon;
            let background = if ceiling {
                SKY_BACKGROUND
            } else {
                FLOOR_BACKGROUND
            };
            row.fill(background);
            if let Some(sky) = &self.sky {
                for (x, pixel) in row.iter_mut().enumerate() {
                    let camera_x = 2.0 * (x as f32 + 0.5) / width as f32 - 1.0;
                    *pixel = sky.sample([
                        cos + camera_plane[0] * camera_x,
                        sin + camera_plane[1] * camera_x,
                        (horizon - (y as f32 + 0.5)) / projection,
                    ]);
                }
            }
            // Half the current preview wall's projected height at unit distance.
            let delta = ((y as f32 + 0.5) - horizon).abs();
            if delta < f32::EPSILON {
                continue;
            }
            let plane_height = if ceiling {
                2.0 - camera.eye_height
            } else {
                camera.eye_height
            };
            if plane_height <= 0.0 {
                continue;
            }
            let distance = projection * plane_height / delta;
            let start = [
                position[0] + distance * (cos - camera_plane[0]),
                position[1] + distance * (sin - camera_plane[1]),
            ];
            let increment = [
                2.0 * distance * camera_plane[0] / width as f32,
                2.0 * distance * camera_plane[1] / width as f32,
            ];
            let attenuation = (1.0 / (1.0 + distance * 0.08)).clamp(0.25, 1.0);
            for (x, pixel) in row.iter_mut().enumerate() {
                let world_x = start[0] + increment[0] * x as f32;
                let world_y = start[1] + increment[1] * x as f32;
                if !(0.0..MAP_SIDE as f32).contains(&world_x)
                    || !(0.0..MAP_SIDE as f32).contains(&world_y)
                {
                    continue;
                }
                let tile_x = world_x.floor() as usize;
                let tile_y = world_y.floor() as usize;
                let cell = tile_x * MAP_SIDE + tile_y;
                let id = if ceiling {
                    self.ceiling_ids[cell]
                } else {
                    self.floor_ids[cell]
                };
                if id == EMPTY_TILE || id == SKY_TILE || usize::from(id) >= FLOOR_TEXTURE_COUNT {
                    continue;
                }
                let tx = ((world_x - tile_x as f32) * FLOOR_TEXTURE_SIDE as f32) as usize;
                let ty = ((world_y - tile_y as f32) * FLOOR_TEXTURE_SIDE as f32) as usize;
                let texel =
                    self.textures[usize::from(id) * FLOOR_TEXTURE_SIDE * FLOOR_TEXTURE_SIDE
                        + ty * FLOOR_TEXTURE_SIDE
                        + tx];
                let light = lights.map_or(0.0, |lights| lights.sample([world_x, world_y]));
                let shade =
                    |channel| ((channel as f32 * attenuation * (1.0 + light)) as u32).min(255);
                if let Some(buffer) = depth.as_deref_mut() {
                    buffer[y * width + x] = distance;
                }
                *pixel = (shade((texel >> 16) & 255) << 16)
                    | (shade((texel >> 8) & 255) << 8)
                    | shade(texel & 255);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixtures(floor_id: u8, ceiling_id: u8) -> (Map, Floors, Palette) {
        let mut map = vec![0; 0x27001 + 4];
        map[0x23001..0x24001].fill(ceiling_id);
        map[0x24001..0x25001].fill(floor_id);
        let mut floors = vec![0; 64 + 64 * 64 * 86];
        // Tile 0 uses palette index 1; tile 1 uses palette index 2.
        floors[64..64 + 4096].fill(1);
        floors[64 + 5504..64 + 5504 + 4096].fill(2);
        let mut palette = vec![0; 768];
        palette[3] = 63;
        palette[7] = 63;
        (
            Map::parse(&map).unwrap(),
            Floors::parse(&floors).unwrap(),
            Palette::parse(&palette).unwrap(),
        )
    }

    #[test]
    fn solid_planes_write_perpendicular_depth_but_sky_does_not() {
        let (map, floors, palette) = fixtures(0, SKY_TILE);
        let renderer = PlaneRenderer::new(&map, &floors, &palette);
        let camera = CameraView {
            player: crate::world::Player {
                position: [16.5, 16.5],
                angle: 0.0,
            },
            eye_height: 1.0,
            pitch: 0.25_f32.atan(),
        };
        let mut image = vec![0; 64];
        let mut depth = vec![f32::INFINITY; 64];
        renderer.render_with_depth(&mut image, &mut depth, 8, 8, camera, None);
        assert!(depth[4].is_infinite());
        // Horizon is 5: at row 7.5, distance = 4 / 2.5 = 1.6 tiles.
        assert!((depth[7 * 8 + 4] - 1.6).abs() < 1e-6);
        // Opaque models and projectiles use this same depth comparison.
        assert!(2.0 >= depth[7 * 8 + 4]);
        assert!(1.0 < depth[7 * 8 + 4]);
    }

    #[test]
    fn sky_fills_open_ceiling_but_not_floor_or_solid_ceiling() {
        let (map, floors, palette) = fixtures(0, SKY_TILE);
        let mut renderer = PlaneRenderer::new(&map, &floors, &palette);
        renderer.set_sky(Sky::solid(0x123456));
        let camera = CameraView {
            player: crate::world::Player {
                position: [32.5, 32.5],
                angle: 0.0,
            },
            eye_height: 1.0,
            pitch: 0.0,
        };
        let mut frame = vec![0; 64];
        renderer.render(&mut frame, 8, 8, camera);
        assert_eq!(frame[4], 0x123456);
        assert_ne!(frame[7 * 8 + 4], 0x123456);
        let (map, floors, palette) = fixtures(0, 1);
        let mut renderer = PlaneRenderer::new(&map, &floors, &palette);
        renderer.set_sky(Sky::solid(0x123456));
        renderer.render(&mut frame, 8, 8, camera);
        assert_ne!(frame[4], 0x123456);
    }

    #[test]
    fn draws_distinct_floor_and_ceiling_with_palette() {
        let (map, floors, palette) = fixtures(0, 1);
        let renderer = PlaneRenderer::new(&map, &floors, &palette);
        let mut image = vec![0; 8 * 8];
        renderer.render(
            &mut image,
            8,
            8,
            CameraView {
                player: crate::world::Player {
                    position: [16.5, 16.5],
                    angle: 0.0,
                },
                eye_height: 1.0,
                pitch: 0.0,
            },
        );
        assert!(image[4] & 0x00ff00 > 0);
        assert_eq!(image[4] & 0xff00ff, 0);
        assert!(image[7 * 8 + 4] & 0xff0000 > 0);
        assert_eq!(image[7 * 8 + 4] & 0x00ffff, 0);
    }
    #[test]
    fn looking_up_moves_horizon_down_with_floor_depth_preserved() {
        let (map, floors, palette) = fixtures(0, 1);
        let renderer = PlaneRenderer::new(&map, &floors, &palette);
        let player = crate::world::Player {
            position: [16.5, 16.5],
            angle: 0.0,
        };
        let mut level = vec![0; 64];
        let mut up = level.clone();
        renderer.render(
            &mut level,
            8,
            8,
            CameraView {
                player,
                eye_height: 1.0,
                pitch: 0.0,
            },
        );
        renderer.render(
            &mut up,
            8,
            8,
            CameraView {
                player,
                eye_height: 1.0,
                pitch: 0.5_f32.atan(),
            },
        );
        // The center row changes from floor (red) to ceiling (green).
        assert_ne!(level[4 * 8 + 4] & 0xff0000, 0);
        assert_ne!(up[4 * 8 + 4] & 0x00ff00, 0);
        // The same distance from the shifted horizon has the same shade.
        assert_eq!(level[5 * 8 + 4], (up[4 * 8 + 4] << 8) & 0xff0000);
    }

    #[test]
    fn dynamic_light_uses_plane_intersections_and_leaves_sky_unchanged() {
        use openchasm::light_sources::{
            LightSourceProcessSnapshot, LightSourcesSnapshot, SourceLight,
        };
        let (map, floors, palette) = fixtures(0, 62);
        let renderer = PlaneRenderer::new(&map, &floors, &palette);
        let camera = CameraView {
            player: crate::world::Player {
                position: [16.5, 16.5],
                angle: 0.0,
            },
            eye_height: 1.0,
            pitch: 0.0,
        };
        let light =
            openchasm::dynamic_light::DynamicLightSampler::from_snapshot(&LightSourcesSnapshot {
                active: vec![LightSourceProcessSnapshot {
                    process_id: 1,
                    elapsed_ticks: 0,
                    sources: vec![SourceLight {
                        position_raw: [18 * 256, 16 * 256 + 128],
                        radius_raw: 256,
                        brightness: 255,
                        delay_ms: 0,
                    }],
                }],
            });
        let mut original = vec![0; 8 * 8];
        let mut lit = original.clone();
        renderer.render(&mut original, 8, 8, camera);
        renderer.render_lit(&mut lit, 8, 8, camera, Some(&light));
        assert!(lit[7 * 8 + 4] > original[7 * 8 + 4]);
        assert_eq!(lit[4], original[4]);
        let mut no_lights = vec![0; 64];
        renderer.render_lit(&mut no_lights, 8, 8, camera, None);
        assert_eq!(original, no_lights);
    }

    #[test]
    fn plane_distance_changes_with_camera_height() {
        let (map, floors, palette) = fixtures(0, 1);
        let renderer = PlaneRenderer::new(&map, &floors, &palette);
        let mut low = vec![0; 8 * 8];
        let mut high = vec![0; 8 * 8];
        let player = crate::world::Player {
            position: [16.5, 16.5],
            angle: 0.0,
        };
        renderer.render(
            &mut low,
            8,
            8,
            CameraView {
                player,
                eye_height: 0.75,
                pitch: 0.0,
            },
        );
        renderer.render(
            &mut high,
            8,
            8,
            CameraView {
                player,
                eye_height: 1.25,
                pitch: 0.0,
            },
        );
        // Higher camera sees a more distant floor and a closer ceiling.
        assert!((low[7 * 8 + 4] & 0xff0000) > (high[7 * 8 + 4] & 0xff0000));
        assert!((low[4] & 0x00ff00) < (high[4] & 0x00ff00));
    }
    #[test]
    fn empty_and_sky_cells_leave_the_fallback_background() {
        let (map, floors, palette) = fixtures(63, 62);
        let renderer = PlaneRenderer::new(&map, &floors, &palette);
        let mut image = vec![0; 8 * 8];
        renderer.render(
            &mut image,
            8,
            8,
            CameraView {
                player: crate::world::Player {
                    position: [16.5, 16.5],
                    angle: 0.0,
                },
                eye_height: 1.0,
                pitch: 0.0,
            },
        );
        assert_eq!(image[4], SKY_BACKGROUND);
        assert_eq!(image[7 * 8 + 4], FLOOR_BACKGROUND);
    }

    #[test]
    fn handles_off_map_and_mismatched_frame_safely() {
        let (map, floors, palette) = fixtures(0, 0);
        let renderer = PlaneRenderer::new(&map, &floors, &palette);
        let mut image = vec![0; 8 * 8];
        renderer.render(
            &mut image,
            8,
            8,
            CameraView {
                player: crate::world::Player {
                    position: [-100.0, -100.0],
                    angle: 0.0,
                },
                eye_height: 1.0,
                pitch: 0.0,
            },
        );
        assert_eq!(image[4], SKY_BACKGROUND);
        assert_eq!(image[7 * 8 + 4], FLOOR_BACKGROUND);
        let mut short = [42; 3];
        renderer.render(
            &mut short,
            4,
            4,
            CameraView {
                player: crate::world::Player {
                    position: [0.0, 0.0],
                    angle: 0.0,
                },
                eye_height: 1.0,
                pitch: 0.0,
            },
        );
        assert_eq!(short, [42; 3]);
    }
}
