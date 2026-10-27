//! Decoding the original Chasm map and floor-texture file formats.
//!
//! Values are kept in their original units; conversion and game rules belong
//! to the simulation, not to the asset reader. See docs/map-format.md.
use std::io::{self, ErrorKind};

pub const MAP_SIDE: usize = 64;
pub const UNITS_PER_TILE: f32 = 256.0;
const GRID_CELLS: usize = MAP_SIDE * MAP_SIDE;
const WALLS_OFFSET: usize = 0x18001;
const WALL_SIZE: usize = 11;
const CELLS_OFFSET: usize = 0x23001;
const LIGHT_COUNT_OFFSET: usize = 0x27001;
const FLOOR_TEXTURE_BYTES: usize = 64 * 86;
pub const FLOOR_TEXTURE_SIDE: usize = 64;
pub const FLOOR_TEXTURE_COUNT: usize = 64;

fn invalid(message: &str) -> io::Error {
    io::Error::new(ErrorKind::InvalidData, message)
}
fn u16le(data: &[u8], offset: usize) -> io::Result<u16> {
    let pair = data
        .get(
            offset
                ..offset
                    .checked_add(2)
                    .ok_or_else(|| invalid("offset overflow"))?,
        )
        .ok_or_else(|| invalid("truncated map"))?;
    Ok(u16::from_le_bytes([pair[0], pair[1]]))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WallCell {
    pub texture_id: u8,
    pub size: u8,
    pub flags: u8,
    pub start: [u16; 2],
    pub end: [u16; 2],
}
impl WallCell {
    /// This predicate identifies wall records; models and items share the grid.
    pub fn is_wall(&self) -> bool {
        self.texture_id > 0 && self.texture_id < 128 && matches!(self.size, 64 | 128)
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Light {
    pub position: [u16; 2],
    pub power: u16,
    pub inner_radius: u16,
    pub outer_radius: u16,
    pub brightness: u16,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Monster {
    pub position: [u16; 2],
    /// The raw ID 100 denotes a player spawn; other IDs begin at 101.
    pub id: u16,
    pub angle: u8,
    /// Difficulty flags for monsters, spawn number for player spawns.
    pub flags: u8,
}
impl Monster {
    pub fn is_player_spawn(&self) -> bool {
        self.id == 100
    }
    pub fn direction_radians(&self) -> f32 {
        f32::from(self.angle & 7) * (std::f32::consts::PI / 4.0) + std::f32::consts::PI
    }
}

#[derive(Debug)]
pub struct Map {
    // All four arrays retain the original x-major order.
    walls: Vec<WallCell>,
    ceilings: Vec<u8>,
    floors: Vec<u8>,
    ambient_light: Vec<u8>,
    ambient_sound: Vec<u8>,
    pub lights: Vec<Light>,
    pub monsters: Vec<Monster>,
    /// Not yet decoded; retained because some maps have data beyond monsters.
    pub trailing_data: Vec<u8>,
}
impl Map {
    pub fn parse(data: &[u8]) -> io::Result<Self> {
        if data.len() < LIGHT_COUNT_OFFSET + 2 {
            return Err(invalid("truncated map fixed sections"));
        }
        let mut walls = Vec::with_capacity(GRID_CELLS);
        for i in 0..GRID_CELLS {
            let off = WALLS_OFFSET + i * WALL_SIZE;
            walls.push(WallCell {
                texture_id: data[off],
                size: data[off + 1],
                flags: data[off + 2],
                start: [u16le(data, off + 3)?, u16le(data, off + 5)?],
                end: [u16le(data, off + 7)?, u16le(data, off + 9)?],
            });
        }
        let grid = |i: usize| {
            data[CELLS_OFFSET + i * GRID_CELLS..CELLS_OFFSET + (i + 1) * GRID_CELLS].to_vec()
        };
        let mut pos = LIGHT_COUNT_OFFSET;
        let light_count = usize::from(u16le(data, pos)?);
        pos += 2;
        let lights_end = pos
            .checked_add(
                light_count
                    .checked_mul(12)
                    .ok_or_else(|| invalid("light count overflow"))?,
            )
            .ok_or_else(|| invalid("light offset overflow"))?;
        if lights_end > data.len() {
            return Err(invalid("truncated light records"));
        }
        let mut lights = Vec::with_capacity(light_count);
        for _ in 0..light_count {
            lights.push(Light {
                position: [u16le(data, pos)?, u16le(data, pos + 2)?],
                power: u16le(data, pos + 4)?,
                inner_radius: u16le(data, pos + 6)?,
                outer_radius: u16le(data, pos + 8)?,
                brightness: u16le(data, pos + 10)?,
            });
            pos += 12;
        }
        let monster_count = usize::from(u16le(data, pos)?);
        pos += 2;
        let monsters_end = pos
            .checked_add(
                monster_count
                    .checked_mul(8)
                    .ok_or_else(|| invalid("monster count overflow"))?,
            )
            .ok_or_else(|| invalid("monster offset overflow"))?;
        if monsters_end > data.len() {
            return Err(invalid("truncated monster records"));
        }
        let mut monsters = Vec::with_capacity(monster_count);
        for _ in 0..monster_count {
            monsters.push(Monster {
                position: [u16le(data, pos)?, u16le(data, pos + 2)?],
                id: u16le(data, pos + 4)?,
                angle: data[pos + 6],
                flags: data[pos + 7],
            });
            pos += 8;
        }
        Ok(Self {
            walls,
            ceilings: grid(0),
            floors: grid(1),
            ambient_light: grid(2),
            ambient_sound: grid(3),
            lights,
            monsters,
            trailing_data: data[pos..].to_vec(),
        })
    }
    fn index(x: usize, y: usize) -> Option<usize> {
        if x < MAP_SIDE && y < MAP_SIDE {
            Some(x * MAP_SIDE + y)
        } else {
            None
        }
    }
    pub fn wall(&self, x: usize, y: usize) -> Option<&WallCell> {
        self.walls.get(Self::index(x, y)?)
    }
    pub fn ceiling(&self, x: usize, y: usize) -> Option<u8> {
        self.ceilings.get(Self::index(x, y)?).copied()
    }
    pub fn floor(&self, x: usize, y: usize) -> Option<u8> {
        self.floors.get(Self::index(x, y)?).copied()
    }
    pub fn ambient_light(&self, x: usize, y: usize) -> Option<u8> {
        self.ambient_light.get(Self::index(x, y)?).copied()
    }
    pub fn ambient_sound(&self, x: usize, y: usize) -> Option<u8> {
        self.ambient_sound.get(Self::index(x, y)?).copied()
    }
}

#[derive(Debug)]
pub struct Floors {
    data: Vec<u8>,
}
impl Floors {
    pub fn parse(data: &[u8]) -> io::Result<Self> {
        let expected = 64 + FLOOR_TEXTURE_COUNT * FLOOR_TEXTURE_BYTES;
        if data.len() != expected {
            return Err(invalid("unexpected floor texture file size"));
        }
        Ok(Self {
            data: data.to_vec(),
        })
    }
    /// Return the 64×64 palette indices for a texture; smaller mipmaps follow
    /// each base image in the original file and are not returned here.
    pub fn texture(&self, id: usize) -> Option<&[u8]> {
        if id >= FLOOR_TEXTURE_COUNT {
            return None;
        }
        let start = 64 + id * FLOOR_TEXTURE_BYTES;
        Some(&self.data[start..start + FLOOR_TEXTURE_SIDE * FLOOR_TEXTURE_SIDE])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_grid_records_and_dynamic_lists() {
        let mut data = vec![0; LIGHT_COUNT_OFFSET + 2 + 12 + 2 + 8];
        let off = WALLS_OFFSET + (2 * MAP_SIDE + 3) * WALL_SIZE;
        data[off..off + 11].copy_from_slice(&[9, 128, 4, 0, 1, 0, 2, 0, 3, 0, 4]);
        data[CELLS_OFFSET + GRID_CELLS + 2 * MAP_SIDE + 3] = 42;
        data[LIGHT_COUNT_OFFSET] = 1;
        let light = LIGHT_COUNT_OFFSET + 2;
        data[light..light + 12].copy_from_slice(&[0, 1, 0, 2, 3, 0, 4, 0, 5, 0, 6, 0]);
        data[light + 12] = 1;
        data[light + 14..light + 22].copy_from_slice(&[0, 1, 0, 2, 100, 0, 5, 0]);
        let map = Map::parse(&data).unwrap();
        let wall = map.wall(2, 3).unwrap();
        assert!(wall.is_wall());
        assert_eq!(wall.start, [256, 512]);
        assert_eq!(wall.end, [768, 1024]);
        assert_eq!(map.floor(2, 3), Some(42));
        assert_eq!(map.floor(3, 2), Some(0));
        assert_eq!(map.wall(64, 0), None);
        assert_eq!(map.lights[0].power, 3);
        assert!(map.monsters[0].is_player_spawn());
        assert_eq!(map.monsters[0].angle, 5);
        assert!(Map::parse(&data[..data.len() - 1]).is_err());
    }
    #[test]
    fn floor_texture_stride_and_bounds() {
        let mut bytes = vec![0; 64 + FLOOR_TEXTURE_COUNT * FLOOR_TEXTURE_BYTES];
        bytes[64 + FLOOR_TEXTURE_BYTES] = 123;
        let floors = Floors::parse(&bytes).unwrap();
        assert_eq!(floors.texture(1).unwrap()[0], 123);
        assert_eq!(floors.texture(0).unwrap()[0], 0);
        assert!(floors.texture(64).is_none());
        assert!(Floors::parse(&bytes[..bytes.len() - 1]).is_err());
    }
}
