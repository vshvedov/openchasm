//! Perspective floor and ceiling preview based on the level's tile grids.
//!
//! Geometry here is renderer-only: the camera height and lighting are chosen to
//! match the current wall preview, not yet validated against the original game.
use crate::color_pipeline::RenderPixel;
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
    static_light: crate::map::StaticLightMap,
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
            static_light: map.static_light().clone(),
            sky: None,
        }
    }

    pub fn static_light(&self) -> &crate::map::StaticLightMap {
        &self.static_light
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
        self.render_inner::<u32, false>(frame, None, width, height, camera, lights);
    }

    /// Paint planes and write perpendicular depths for actual geometry only.
    #[cfg(test)]
    pub fn render_with_depth(
        &self,
        frame: &mut [u32],
        depth: &mut [f32],
        width: usize,
        height: usize,
        camera: CameraView,
        lights: Option<&openchasm::dynamic_light::DynamicLightSampler>,
    ) {
        self.render_inner::<u32, false>(frame, Some(depth), width, height, camera, lights);
    }

    /// Render plane colors into either the legacy or linear-HDR target.
    pub fn render_to<C: RenderPixel>(
        &self,
        frame: &mut [C],
        depth: &mut [f32],
        width: usize,
        height: usize,
        camera: CameraView,
        lights: Option<&openchasm::dynamic_light::DynamicLightSampler>,
    ) {
        self.render_inner::<C, false>(frame, Some(depth), width, height, camera, lights);
    }

    fn render_inner<C: RenderPixel, const USE_REFERENCE_FLOOR: bool>(
        &self,
        frame: &mut [C],
        mut depth: Option<&mut [f32]>,
        width: usize,
        height: usize,
        camera: CameraView,
        lights: Option<&openchasm::dynamic_light::DynamicLightSampler>,
    ) {
        let position = camera.player.position;
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
        let [cos, sin] = camera.player.forward();
        let [right_x, right_y] = camera.player.right();
        let projection = CameraView::focal_length(height);
        let horizon = camera.horizon(height);
        let half_width = CameraView::half_width(width, height);
        let camera_plane = [right_x * half_width, right_y * half_width];
        for (y, row) in frame.chunks_mut(width).enumerate() {
            let ceiling = (y as f32 + 0.5) < horizon;
            let background = if ceiling {
                SKY_BACKGROUND
            } else {
                FLOOR_BACKGROUND
            };
            row.fill(C::from_srgb8(background));
            let paint_sky = |x: usize, pixel: &mut C| {
                if let Some(sky) = &self.sky {
                    let camera_x = 2.0 * (x as f32 + 0.5) / width as f32 - 1.0;
                    *pixel = C::from_srgb8(sky.sample([
                        cos + camera_plane[0] * camera_x,
                        sin + camera_plane[1] * camera_x,
                        (horizon - (y as f32 + 0.5)) / projection,
                    ]));
                }
            };
            // Half the current preview wall's projected height at unit distance.
            let delta = ((y as f32 + 0.5) - horizon).abs();
            if delta < f32::EPSILON {
                for (x, pixel) in row.iter_mut().enumerate() {
                    paint_sky(x, pixel);
                }
                continue;
            }
            let plane_height = if ceiling {
                2.0 - camera.eye_height
            } else {
                camera.eye_height
            };
            if plane_height <= 0.0 {
                for (x, pixel) in row.iter_mut().enumerate() {
                    paint_sky(x, pixel);
                }
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
            for (x, pixel) in row.iter_mut().enumerate() {
                let world_x = start[0] + increment[0] * (x as f32 + 0.5);
                let world_y = start[1] + increment[1] * (x as f32 + 0.5);
                if !(0.0..MAP_SIDE as f32).contains(&world_x)
                    || !(0.0..MAP_SIDE as f32).contains(&world_y)
                {
                    paint_sky(x, pixel);
                    continue;
                }
                // Intersections have already been checked inside [0, MAP_SIDE),
                // where truncating to usize is exactly equivalent to floor.
                let tile_x = if USE_REFERENCE_FLOOR {
                    world_x.floor() as usize
                } else {
                    world_x as usize
                };
                let tile_y = if USE_REFERENCE_FLOOR {
                    world_y.floor() as usize
                } else {
                    world_y as usize
                };
                let cell = tile_x * MAP_SIDE + tile_y;
                let id = if ceiling {
                    self.ceiling_ids[cell]
                } else {
                    self.floor_ids[cell]
                };
                if id == EMPTY_TILE || id == SKY_TILE || usize::from(id) >= FLOOR_TEXTURE_COUNT {
                    paint_sky(x, pixel);
                    continue;
                }
                let tx = ((world_x - tile_x as f32) * FLOOR_TEXTURE_SIDE as f32) as usize;
                let ty = ((world_y - tile_y as f32) * FLOOR_TEXTURE_SIDE as f32) as usize;
                let texel =
                    self.textures[usize::from(id) * FLOOR_TEXTURE_SIDE * FLOOR_TEXTURE_SIDE
                        + ty * FLOOR_TEXTURE_SIDE
                        + tx];
                let light = lights.map_or(0.0, |lights| lights.sample([world_x, world_y]));
                let static_light = self.static_light.sample([world_x, world_y]).unwrap_or(1.0);
                if let Some(buffer) = depth.as_deref_mut() {
                    buffer[y * width + x] = distance;
                }
                *pixel = C::shade_srgb8(texel, static_light * (1.0 + light));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixtures(floor_id: u8, ceiling_id: u8) -> (Map, Floors, Palette) {
        let mut map = vec![0; 0x27001 + 4];
        map[1..0x10001].fill(20);
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
    #[ignore = "manual release-mode plane rendering benchmark"]
    fn benchmark_plane_rendering() {
        use std::{hint::black_box, time::Instant};
        let (map, floors, palette) = fixtures(0, 0);
        let renderer = PlaneRenderer::new(&map, &floors, &palette);
        let camera = CameraView {
            player: crate::world::Player {
                position: [16.5, 16.5],
                angle: 0.63,
            },
            eye_height: 0.75,
            pitch: 0.12,
        };
        let mut frame = vec![crate::color_pipeline::LinearRgb::default(); 640 * 400];
        let mut depth = vec![f32::INFINITY; frame.len()];
        let started = Instant::now();
        for _ in 0..80 {
            renderer.render_to(
                black_box(&mut frame),
                black_box(&mut depth),
                640,
                400,
                camera,
                None,
            );
        }
        eprintln!(
            "plane benchmark: {:.3}ms per frame",
            started.elapsed().as_secs_f64() * 1000.0 / 80.0
        );
        black_box(frame);
    }

    #[test]
    fn installed_map_contains_spatially_varying_authored_light() {
        let explicit = std::env::var_os("OPENCHASM_TEST_CONTENT");
        let path = explicit
            .as_ref()
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| "assets/csm.bin".into());
        if !path.exists() && explicit.is_none() {
            return;
        }
        let mut resources = crate::content::ResourceSet::open(&path).unwrap();
        let map = Map::parse(&resources.read("map/map.01").unwrap()).unwrap();
        let light = map.static_light();
        let mut minimum = f32::INFINITY;
        let mut maximum = 0.0_f32;
        for y in 0..256 {
            for x in 0..256 {
                let value = light.sample([x as f32 / 4.0, y as f32 / 4.0]).unwrap();
                minimum = minimum.min(value);
                maximum = maximum.max(value);
            }
        }
        assert!(minimum >= 0.0 && maximum <= 1.3 && maximum - minimum > 0.5);
        assert!(light.sample([-1.0, 0.0]).is_none());
        assert!(light.wall_sample([64, 0], 0.5).is_none());
    }

    #[test]
    fn truncation_matches_floor_reference_for_scene_color_and_depth() {
        use crate::color_pipeline::LinearRgb;
        let (map, floors, palette) = fixtures(0, 1);
        let mut renderer = PlaneRenderer::new(&map, &floors, &palette);
        renderer.set_sky(Sky::solid(0x52749c));
        // Include plane holes, neighboring textures, world edges, pitched views,
        // and values very close to integer map and texture boundaries.
        renderer.floor_ids[16 * 64 + 16] = SKY_TILE;
        renderer.ceiling_ids[17 * 64 + 16] = EMPTY_TILE;
        for (position, angle, pitch) in [
            ([16.5, 16.5], 0.0, 0.0),
            ([16.999_998, 16.000_002], std::f32::consts::FRAC_PI_4, 0.31),
            ([0.000_001, 63.999_996], -1.57, -0.7),
            ([32.0, 32.0], std::f32::consts::PI, 0.6),
        ] {
            let camera = CameraView {
                player: crate::world::Player { position, angle },
                eye_height: 0.75,
                pitch,
            };
            let size = 127 * 83;
            let mut legacy = vec![0u32; size];
            let mut reference = legacy.clone();
            let mut depth = vec![f32::INFINITY; size];
            let mut reference_depth = depth.clone();
            renderer.render_inner::<u32, false>(
                &mut legacy,
                Some(&mut depth),
                127,
                83,
                camera,
                None,
            );
            renderer.render_inner::<u32, true>(
                &mut reference,
                Some(&mut reference_depth),
                127,
                83,
                camera,
                None,
            );
            assert_eq!(legacy, reference);
            assert_eq!(depth, reference_depth);
            let mut hdr = vec![LinearRgb::default(); size];
            let mut reference_hdr = hdr.clone();
            depth.fill(f32::INFINITY);
            reference_depth.fill(f32::INFINITY);
            renderer.render_inner::<LinearRgb, false>(
                &mut hdr,
                Some(&mut depth),
                127,
                83,
                camera,
                None,
            );
            renderer.render_inner::<LinearRgb, true>(
                &mut reference_hdr,
                Some(&mut reference_depth),
                127,
                83,
                camera,
                None,
            );
            assert_eq!(hdr, reference_hdr);
            assert_eq!(depth, reference_depth);
        }
    }

    #[test]
    fn generic_plane_target_preserves_hdr_and_matches_legacy_output() {
        use crate::color_pipeline::LinearRgb;
        let (map, floors, palette) = fixtures(0, SKY_TILE);
        let mut renderer = PlaneRenderer::new(&map, &floors, &palette);
        let raw = vec![0; 0x27001 + 4];
        renderer.static_light = Map::parse(&raw).unwrap().static_light().clone();
        let camera = CameraView {
            player: crate::world::Player {
                position: [16.5, 16.5],
                angle: 0.0,
            },
            eye_height: 1.0,
            pitch: 0.0,
        };
        let mut legacy = vec![0; 64];
        let mut generic = legacy.clone();
        let mut depth_a = vec![f32::INFINITY; 64];
        let mut depth_b = depth_a.clone();
        renderer.render_with_depth(&mut legacy, &mut depth_a, 8, 8, camera, None);
        renderer.render_to(&mut generic, &mut depth_b, 8, 8, camera, None);
        assert_eq!(legacy, generic);
        assert_eq!(depth_a, depth_b);
        let mut hdr = vec![LinearRgb::default(); 64];
        let mut depth_hdr = vec![f32::INFINITY; 64];
        renderer.render_to(&mut hdr, &mut depth_hdr, 8, 8, camera, None);
        assert!(hdr[7 * 8 + 4].0[0] > 1.0);
        assert_eq!(depth_a, depth_hdr);
    }

    #[test]
    fn floor_rays_use_same_world_right_as_original_camera() {
        let mut bytes = vec![0; 0x27001 + 4];
        bytes[1..0x10001].fill(20);
        bytes[0x23001..0x24001].fill(63);
        for x in 0..64 {
            for y in 0..64 {
                bytes[0x24001 + x * 64 + y] = u8::from(x >= 16);
            }
        }
        let map = Map::parse(&bytes).unwrap();
        let mut floor_bytes = vec![0; 64 + 64 * 64 * 86];
        floor_bytes[64..64 + 4096].fill(1);
        floor_bytes[64 + 5504..64 + 5504 + 4096].fill(2);
        let floors = Floors::parse(&floor_bytes).unwrap();
        let mut palette_bytes = vec![0; 768];
        palette_bytes[3] = 63;
        palette_bytes[7] = 63;
        let palette = Palette::parse(&palette_bytes).unwrap();
        let planes = PlaneRenderer::new(&map, &floors, &palette);
        let camera = CameraView {
            player: crate::world::Player {
                position: [16.5, 16.5],
                angle: 3.0 * std::f32::consts::FRAC_PI_2,
            },
            eye_height: 1.0,
            pitch: 0.0,
        };
        let mut frame = vec![0; 64 * 64];
        planes.render(&mut frame, 64, 64, camera);
        assert_ne!(
            frame[48 * 64 + 10] & 0x00ff00,
            0,
            "camera-left looks toward larger x"
        );
        assert_ne!(
            frame[48 * 64 + 54] & 0xff0000,
            0,
            "camera-right looks toward smaller x"
        );
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
        let expected = CameraView::focal_length(8) / (7.5 - camera.horizon(8));
        assert!((depth[7 * 8 + 4] - expected).abs() < 1e-6);
        // Opaque models and projectiles use this same depth comparison.
        assert!(3.0 >= depth[7 * 8 + 4]);
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
    fn sky_covers_horizon_off_map_and_invalid_tiles_without_depth() {
        let (map, floors, palette) = fixtures(255, 255);
        let mut renderer = PlaneRenderer::new(&map, &floors, &palette);
        renderer.set_sky(Sky::solid(0x123456));
        let mut frame = vec![0; 25];
        let mut depth = vec![42.0; 25];
        let camera = CameraView {
            player: crate::world::Player {
                position: [0.01, 0.01],
                angle: 0.7,
            },
            eye_height: 1.0,
            pitch: 0.0,
        };
        renderer.render_with_depth(&mut frame, &mut depth, 5, 5, camera, None);
        assert!(frame.iter().all(|pixel| *pixel == 0x123456));
        assert!(depth.iter().all(|value| *value == 42.0));
        let camera = CameraView {
            eye_height: 2.1,
            pitch: 0.2,
            ..camera
        };
        renderer.render_with_depth(&mut frame, &mut depth, 5, 5, camera, None);
        assert!(frame.iter().all(|pixel| *pixel == 0x123456));
        assert!(depth.iter().all(|value| *value == 42.0));
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
                pitch: (1.0 / CameraView::focal_length(8)).atan(),
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
        let mut low_depth = vec![f32::INFINITY; 64];
        let mut high_depth = low_depth.clone();
        renderer.render_with_depth(
            &mut low,
            &mut low_depth,
            8,
            8,
            CameraView {
                player,
                eye_height: 0.75,
                pitch: 0.0,
            },
            None,
        );
        renderer.render_with_depth(
            &mut high,
            &mut high_depth,
            8,
            8,
            CameraView {
                player,
                eye_height: 1.25,
                pitch: 0.0,
            },
            None,
        );
        assert!(low_depth[7 * 8 + 4] < high_depth[7 * 8 + 4]);
        assert!(low_depth[4] > high_depth[4]);
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
