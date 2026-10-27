//! Original 3O object models and their separate ANI vertex animations.
//!
//! Parsing preserves raw coordinates, UVs and flags. Conversion to world-space
//! geometry, material interpretation and animation timing are responsibilities
//! of the simulation/rendering layers.
use std::io::{self, ErrorKind};

const POLYGON_START: usize = 0;
const VERTEX_START: usize = 0x3200;
const COUNTS_START: usize = 0x4800;
const SKIN_START: usize = 0x4806;
const POLYGON_SIZE: usize = 32;
const VERTEX_SIZE: usize = 6;
const MAX_POLYGONS: usize = 400;
const MAX_VERTICES: usize = (COUNTS_START - VERTEX_START) / VERTEX_SIZE;
pub const SKIN_WIDTH: usize = 64;
const TRIANGLE_SENTINEL: u16 = 255;
const MAX_ANIMATION_FRAMES: usize = 4096;

fn invalid(message: &str) -> io::Error {
    io::Error::new(ErrorKind::InvalidData, message)
}
fn word(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
}
fn signed_word(bytes: &[u8], offset: usize) -> i16 {
    i16::from_le_bytes([bytes[offset], bytes[offset + 1]])
}
fn vertex(bytes: &[u8], offset: usize) -> Vertex {
    Vertex {
        position: [
            signed_word(bytes, offset),
            signed_word(bytes, offset + 2),
            signed_word(bytes, offset + 4),
        ],
    }
}

/// Raw signed coordinates, in the model's own coordinate system.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Vertex {
    pub position: [i16; 3],
}

/// The fourth vertex is absent for triangles. Unused UV entries remain intact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Polygon {
    pub vertices: [u16; 4],
    pub uv: [[u16; 2]; 4],
    pub next: u16,
    pub distant: u16,
    pub group: u8,
    pub flags: u8,
    pub sprite_offset: i16,
}
impl Polygon {
    pub fn vertex_count(&self) -> usize {
        if self.vertices[3] == TRIANGLE_SENTINEL {
            3
        } else {
            4
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Model {
    pub vertices: Vec<Vertex>,
    pub polygons: Vec<Polygon>,
    /// Indexed image, SKIN_WIDTH pixels per row; zero height is permitted.
    /// The 3O format does not contain its own palette.
    pub skin_height: u16,
    pub skin_indices: Vec<u8>,
}
impl Model {
    pub fn parse(bytes: &[u8]) -> io::Result<Self> {
        if bytes.len() < SKIN_START {
            return Err(invalid("truncated 3O header and arrays"));
        }
        let vertex_count = usize::from(word(bytes, COUNTS_START));
        let polygon_count = usize::from(word(bytes, COUNTS_START + 2));
        let skin_height = word(bytes, COUNTS_START + 4);
        if vertex_count > MAX_VERTICES || polygon_count > MAX_POLYGONS {
            return Err(invalid("3O vertex or polygon count exceeds fixed array"));
        }
        let skin_size = usize::from(skin_height)
            .checked_mul(SKIN_WIDTH)
            .ok_or_else(|| invalid("3O skin size overflow"))?;
        let expected_size = SKIN_START
            .checked_add(skin_size)
            .ok_or_else(|| invalid("3O file size overflow"))?;
        if bytes.len() != expected_size {
            return Err(invalid("3O skin length does not match height"));
        }
        let vertices = (0..vertex_count)
            .map(|i| vertex(bytes, VERTEX_START + i * VERTEX_SIZE))
            .collect();
        let mut polygons = Vec::with_capacity(polygon_count);
        for i in 0..polygon_count {
            let pos = POLYGON_START + i * POLYGON_SIZE;
            let polygon = Polygon {
                vertices: std::array::from_fn(|j| word(bytes, pos + j * 2)),
                uv: std::array::from_fn(|j| {
                    [word(bytes, pos + 8 + j * 4), word(bytes, pos + 10 + j * 4)]
                }),
                next: word(bytes, pos + 24),
                distant: word(bytes, pos + 26),
                group: bytes[pos + 28],
                flags: bytes[pos + 29],
                sprite_offset: signed_word(bytes, pos + 30),
            };
            if polygon.vertices[..polygon.vertex_count()]
                .iter()
                .any(|&v| usize::from(v) >= vertex_count)
            {
                return Err(invalid("3O polygon references a nonexistent vertex"));
            }
            polygons.push(polygon);
        }
        Ok(Self {
            vertices,
            polygons,
            skin_height,
            skin_indices: bytes[SKIN_START..].to_vec(),
        })
    }
}

/// A sequence of raw per-vertex positions; frame timing is defined elsewhere.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Animation {
    pub vertex_count: u16,
    pub frames: Vec<Vec<Vertex>>,
}
impl Animation {
    pub fn parse(bytes: &[u8]) -> io::Result<Self> {
        if bytes.len() < 2 {
            return Err(invalid("truncated ANI header"));
        }
        let vertex_count = word(bytes, 0);
        if vertex_count == 0 || usize::from(vertex_count) > MAX_VERTICES {
            return Err(invalid("invalid ANI vertex count"));
        }
        let frame_bytes = usize::from(vertex_count) * VERTEX_SIZE;
        let payload = bytes.len() - 2;
        if payload == 0 || !payload.is_multiple_of(frame_bytes) {
            return Err(invalid("ANI contains a partial frame or no frames"));
        }
        let frame_count = payload / frame_bytes;
        if frame_count > MAX_ANIMATION_FRAMES {
            return Err(invalid("too many ANI frames"));
        }
        let mut frames = Vec::with_capacity(frame_count);
        for frame in 0..frame_count {
            let start = 2 + frame * frame_bytes;
            frames.push(
                (0..usize::from(vertex_count))
                    .map(|i| vertex(bytes, start + i * VERTEX_SIZE))
                    .collect(),
            );
        }
        Ok(Self {
            vertex_count,
            frames,
        })
    }

    pub fn matches_model(&self, model: &Model) -> bool {
        usize::from(self.vertex_count) == model.vertices.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Vec<u8> {
        let mut bytes = vec![0; SKIN_START + SKIN_WIDTH];
        for (i, number) in [0_u16, 1, 2, TRIANGLE_SENTINEL].iter().enumerate() {
            bytes[i * 2..i * 2 + 2].copy_from_slice(&number.to_le_bytes());
        }
        bytes[8..10].copy_from_slice(&12_u16.to_le_bytes());
        bytes[29] = 3;
        bytes[30..32].copy_from_slice(&(-2_i16).to_le_bytes());
        bytes[VERTEX_START..VERTEX_START + 2].copy_from_slice(&(-12_i16).to_le_bytes());
        bytes[COUNTS_START..COUNTS_START + 2].copy_from_slice(&3_u16.to_le_bytes());
        bytes[COUNTS_START + 2..COUNTS_START + 4].copy_from_slice(&1_u16.to_le_bytes());
        bytes[COUNTS_START + 4..COUNTS_START + 6].copy_from_slice(&1_u16.to_le_bytes());
        bytes[SKIN_START] = 42;
        bytes
    }
    #[test]
    fn parses_triangle_geometry_and_skin() {
        let model = Model::parse(&fixture()).unwrap();
        assert_eq!(model.vertices[0].position, [-12, 0, 0]);
        assert_eq!(model.polygons[0].vertex_count(), 3);
        assert_eq!(model.polygons[0].uv[0], [12, 0]);
        assert_eq!(model.polygons[0].flags, 3);
        assert_eq!(model.polygons[0].sprite_offset, -2);
        assert_eq!(model.skin_indices.len(), 64);
        assert_eq!(model.skin_indices[0], 42);
    }
    #[test]
    fn rejects_bad_counts_indices_and_truncation() {
        let mut data = fixture();
        assert!(Model::parse(&data[..data.len() - 1]).is_err());
        data[COUNTS_START + 2..COUNTS_START + 4].copy_from_slice(&401_u16.to_le_bytes());
        assert!(Model::parse(&data).is_err());
        data = fixture();
        data[2..4].copy_from_slice(&4_u16.to_le_bytes());
        assert!(Model::parse(&data).is_err());
    }
    #[test]
    fn parses_animation_and_checks_matching_model() {
        let model = Model::parse(&fixture()).unwrap();
        let mut data = vec![0; 2 + 3 * VERTEX_SIZE * 2];
        data[..2].copy_from_slice(&3_u16.to_le_bytes());
        data[2..4].copy_from_slice(&(-10_i16).to_le_bytes());
        data[2 + 3 * VERTEX_SIZE..4 + 3 * VERTEX_SIZE].copy_from_slice(&24_i16.to_le_bytes());
        let animation = Animation::parse(&data).unwrap();
        assert_eq!(animation.frames.len(), 2);
        assert_eq!(animation.frames[0][0].position[0], -10);
        assert_eq!(animation.frames[1][0].position[0], 24);
        assert!(animation.matches_model(&model));
        assert!(Animation::parse(&data[..data.len() - 1]).is_err());
        assert!(Animation::parse(&[0, 0]).is_err());
    }
}
