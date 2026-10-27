//! Perspective floor and ceiling preview based on the level's tile grids.
//!
//! Geometry here is renderer-only: the camera height and lighting are chosen to
//! match the current wall preview, not yet validated against the original game.
use crate::map::{Floors, Map, FLOOR_TEXTURE_COUNT, FLOOR_TEXTURE_SIDE, MAP_SIDE};
use crate::texture::Palette;

const FLOOR_BACKGROUND: u32 = 0x38312b;
const SKY_BACKGROUND: u32 = 0x303847;
// Tile 63 dominates map space outside rooms and has a uniform texture in the
// installed data. Tile 62 occurs mainly on ceilings, commonly in sky areas.
// Their complete original semantics are not yet understood.
const EMPTY_TILE: u8 = 63;
const SKY_CEILING_TILE: u8 = 62;

/// Predecoded data for fast, allocation-free per-frame plane drawing.
pub struct PlaneRenderer {
    floor_ids: Vec<u8>,
    ceiling_ids: Vec<u8>,
    textures: Vec<u32>,
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
        }
    }

    /// Paint the floor and ceiling before drawing walls, using the same camera
    /// plane and projected wall height as the geometry preview. Invalid or
    /// off-map intersections retain a plain background.
    pub fn render(
        &self,
        frame: &mut [u32],
        width: usize,
        height: usize,
        position: [f32; 2],
        angle: f32,
    ) {
        if width == 0 || height == 0 || width.checked_mul(height) != Some(frame.len()) {
            return;
        }
        let (sin, cos) = angle.sin_cos();
        let horizon = height as f32 / 2.0;
        let camera_plane = [-sin * 0.66, cos * 0.66];
        for (y, row) in frame.chunks_mut(width).enumerate() {
            let ceiling = y < height / 2;
            let background = if ceiling {
                SKY_BACKGROUND
            } else {
                FLOOR_BACKGROUND
            };
            row.fill(background);
            // Half the current preview wall's projected height at unit distance.
            let delta = ((y as f32 + 0.5) - horizon).abs();
            if delta < f32::EPSILON {
                continue;
            }
            let distance = horizon / delta;
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
                if id == EMPTY_TILE
                    || (ceiling && id == SKY_CEILING_TILE)
                    || usize::from(id) >= FLOOR_TEXTURE_COUNT
                {
                    continue;
                }
                let tx = ((world_x - tile_x as f32) * FLOOR_TEXTURE_SIDE as f32) as usize;
                let ty = ((world_y - tile_y as f32) * FLOOR_TEXTURE_SIDE as f32) as usize;
                let texel =
                    self.textures[usize::from(id) * FLOOR_TEXTURE_SIDE * FLOOR_TEXTURE_SIDE
                        + ty * FLOOR_TEXTURE_SIDE
                        + tx];
                let shade = |channel| ((channel as f32 * attenuation) as u32).min(255);
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
    fn draws_distinct_floor_and_ceiling_with_palette() {
        let (map, floors, palette) = fixtures(0, 1);
        let renderer = PlaneRenderer::new(&map, &floors, &palette);
        let mut image = vec![0; 8 * 8];
        renderer.render(&mut image, 8, 8, [16.5, 16.5], 0.0);
        assert!(image[4] & 0x00ff00 > 0);
        assert_eq!(image[4] & 0xff00ff, 0);
        assert!(image[7 * 8 + 4] & 0xff0000 > 0);
        assert_eq!(image[7 * 8 + 4] & 0x00ffff, 0);
    }

    #[test]
    fn empty_and_sky_cells_leave_the_fallback_background() {
        let (map, floors, palette) = fixtures(63, 62);
        let renderer = PlaneRenderer::new(&map, &floors, &palette);
        let mut image = vec![0; 8 * 8];
        renderer.render(&mut image, 8, 8, [16.5, 16.5], 0.0);
        assert_eq!(image[4], SKY_BACKGROUND);
        assert_eq!(image[7 * 8 + 4], FLOOR_BACKGROUND);
    }

    #[test]
    fn handles_off_map_and_mismatched_frame_safely() {
        let (map, floors, palette) = fixtures(0, 0);
        let renderer = PlaneRenderer::new(&map, &floors, &palette);
        let mut image = vec![0; 8 * 8];
        renderer.render(&mut image, 8, 8, [-100.0, -100.0], 0.0);
        assert_eq!(image[4], SKY_BACKGROUND);
        assert_eq!(image[7 * 8 + 4], FLOOR_BACKGROUND);
        let mut short = [42; 3];
        renderer.render(&mut short, 4, 4, [0.0, 0.0], 0.0);
        assert_eq!(short, [42; 3]);
    }
}
