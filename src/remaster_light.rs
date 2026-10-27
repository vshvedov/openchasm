//! Client-owned N1 static lighting reconstructed from the installed remaster.
//!
//! See `docs/remaster-lighting.md` for binary/shader evidence. The directional
//! field uses f32 rather than per-addition RGBA16F rounding. Shadow triangles
//! reproduce the observed extrusion and UV correction, but nearest alpha
//! filtering and inclusive triangle edges remain approximations. This module
//! intentionally does not consume the provisional legacy fine light map.
use crate::map::{Light, Map, MAP_SIDE};
use crate::world::World;

pub const FIELD_SIDE: usize = 1024;
const TEXELS_PER_TILE: f32 = FIELD_SIDE as f32 / MAP_SIDE as f32;
const TEXEL_SIZE: f32 = 1.0 / TEXELS_PER_TILE;
const MAX_SOURCE_TEXELS: usize = 64 * 1024 * 1024;
const MAX_SHADOW_TEXELS: usize = 256 * 1024 * 1024;

/// Borrowed packed ARGB wall pixels; the alpha byte alone is used.
#[derive(Clone, Copy)]
pub struct ShadowTexture<'a> {
    pub width: usize,
    pub height: usize,
    pub pixels: &'a [u32],
}
impl ShadowTexture<'_> {
    fn valid(self) -> bool {
        self.width > 0
            && self.height > 0
            && self.width.checked_mul(self.height) == Some(self.pixels.len())
    }

    fn alpha(self, u: f32, v: f32) -> f32 {
        let x = (u.rem_euclid(1.0) * self.width as f32) as usize % self.width;
        let y = (v * self.height as f32) as usize % self.height;
        (self.pixels[y * self.width + x] >> 24) as f32 / 255.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LightingError {
    Allocation,
    SourceWorkLimit,
    ShadowWorkLimit,
}
impl std::fmt::Display for LightingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Allocation => "unable to allocate remaster lighting field",
            Self::SourceWorkLimit => "remaster lighting source build budget exceeded",
            Self::ShadowWorkLimit => "remaster lighting shadow build budget exceeded",
        })
    }
}
impl std::error::Error for LightingError {}

#[derive(Clone, Copy, Debug)]
struct Source {
    position: [f32; 2],
    power: f32,
    maximum: f32,
    inner: f32,
    outer: f32,
    two_sided: bool,
    cells: [[i32; 2]; 2],
}
impl Source {
    fn decode(raw: Light) -> Option<Self> {
        let xy = raw.position.map(|v| i32::from(v as i16));
        let raw_outer = i32::from(raw.outer_radius as i16);
        let raw_inner = i32::from(raw.inner_radius as i16);
        let inner = if raw_inner < raw_outer { raw_inner } else { 0 };
        let result = Self {
            position: xy.map(|v| v as f32 / 256.0),
            power: i32::from(raw.power as i16).abs() as f32 / 128.0,
            maximum: (128 - i32::from(raw.brightness & 0x3fff)) as f32 / 128.0,
            inner: inner.max(0) as f32 / 256.0,
            outer: raw_outer as f32 / 256.0 + TEXEL_SIZE,
            two_sided: inner < 0,
            cells: xy.map(|v| {
                [
                    ((v - raw_outer) / 256 - 2).clamp(0, 63),
                    ((v + raw_outer) / 256 + 2).clamp(0, 63),
                ]
            }),
        };
        // Explicit malformed-source policy: skip nonpositive source levels
        // and invalid signed radii. Preserve original source iteration order.
        (raw_outer >= 0
            && result.power > 0.0
            && result.maximum > 0.0
            && result.outer > result.inner)
            .then_some(result)
    }

    fn contribution(self, position: [f32; 2], shadow: f32) -> [f32; 4] {
        let delta = [
            self.position[0] - position[0],
            self.position[1] - position[1],
        ];
        let distance = (delta[0] * delta[0] + delta[1] * delta[1]).sqrt();
        let falloff = 1.0 - ((distance - self.inner).max(0.0) / (self.outer - self.inner)).min(1.0);
        let light = (1.0 - shadow) * (self.power * falloff).min(self.maximum);
        if self.two_sided {
            [light, light, -light, -light]
        } else if distance > 0.0 {
            let x = delta[0] / distance * light;
            let y = delta[1] / distance * light;
            [x.max(0.0), y.max(0.0), x.min(0.0), y.min(0.0)]
        } else {
            // normalize(0) is undefined in the shader. Zero is finite and
            // does not invent a directional preference for this one texel.
            [0.0; 4]
        }
    }
}

/// A fixed-size field: rendering samples four texels, never individual sources.
pub struct RemasterLightMap {
    field: Vec<[f32; 4]>,
    ambient: Vec<f32>,
    skipped_sources: usize,
}
impl RemasterLightMap {
    /// Build from current wall endpoints and vertical offsets in map tiles.
    /// Missing elevation entries are initial zero. Missing/invalid textures
    /// do not cast shadows, matching the available client presentation.
    /// Work limits reject the complete build rather than dropping valid lights.
    pub fn build(
        map: &Map,
        world: &World,
        elevations: &[f32],
        textures: &[Option<ShadowTexture<'_>>],
    ) -> Result<Self, LightingError> {
        let mut field = allocate(FIELD_SIDE * FIELD_SIDE, [0.0; 4])?;
        let mut shadow = allocate(FIELD_SIDE * FIELD_SIDE, 0.0f32)?;
        let mut ambient = allocate(MAP_SIDE * MAP_SIDE, 0.0)?;
        for x in 0..MAP_SIDE {
            for y in 0..MAP_SIDE {
                ambient[x * MAP_SIDE + y] = ambient_value(map.ambient_light(x, y).unwrap_or(15));
            }
        }
        let mut source_work = MAX_SOURCE_TEXELS;
        let mut shadow_work = MAX_SHADOW_TEXELS;
        let mut skipped_sources = 0;
        for &raw in &map.lights {
            let Some(source) = Source::decode(raw) else {
                skipped_sources += 1;
                continue;
            };
            let rect = Rect::bounds(
                source.position.map(|p| p - source.outer),
                source.position.map(|p| p + source.outer),
            );
            charge(
                &mut source_work,
                rect.area(),
                LightingError::SourceWorkLimit,
            )?;
            if rect.area() == 0 {
                continue;
            }
            charge(
                &mut shadow_work,
                world.walls.len(),
                LightingError::ShadowWorkLimit,
            )?;
            for y in rect.y0..rect.y1 {
                shadow[y * FIELD_SIDE + rect.x0..y * FIELD_SIDE + rect.x1].fill(0.0);
            }
            for (index, wall) in world.walls.iter().enumerate() {
                let Some(cell) = wall.cell else { continue };
                if !(1..=119).contains(&wall.texture)
                    || elevations
                        .get(index)
                        .is_some_and(|h| !h.is_finite() || *h > 80.0 / 256.0)
                    || (0..2).any(|axis| {
                        i32::from(cell[axis]) < source.cells[axis][0]
                            || i32::from(cell[axis]) > source.cells[axis][1]
                    })
                {
                    continue;
                }
                let Some(texture) = textures
                    .get(usize::from(wall.texture))
                    .copied()
                    .flatten()
                    .filter(|t| t.valid())
                else {
                    continue;
                };
                let Some(authored) = map.wall(usize::from(cell[0]), usize::from(cell[1])) else {
                    continue;
                };
                let Some(vertices) = shadow_vertices(
                    wall.a,
                    wall.b,
                    source.position,
                    f32::from(authored.size) / 128.0,
                ) else {
                    continue;
                };
                for triangle in [
                    [vertices[0], vertices[1], vertices[2]],
                    [vertices[1], vertices[3], vertices[2]],
                ] {
                    raster_shadow(&mut shadow, rect, triangle, texture, &mut shadow_work)?;
                }
            }
            for y in rect.y0..rect.y1 {
                for x in rect.x0..rect.x1 {
                    let index = y * FIELD_SIDE + x;
                    let position = [(x as f32 + 0.5) * TEXEL_SIZE, (y as f32 + 0.5) * TEXEL_SIZE];
                    let contribution = source.contribution(position, shadow[index]);
                    for (sum, term) in field[index].iter_mut().zip(contribution) {
                        *sum += term;
                    }
                }
            }
        }
        Ok(Self {
            field,
            ambient,
            skipped_sources,
        })
    }

    /// Nonpositive or malformed authored source records skipped at build time.
    pub fn skipped_sources(&self) -> usize {
        self.skipped_sources
    }

    /// Ambient plus directional response, before camera fade or tone mapping.
    /// Normalization matches the N1 shader. Nonfinite inputs return zero;
    /// generated light repeats at map borders, while off-map ambient is zero.
    pub fn sample(&self, position: [f32; 2], normal: [f32; 3]) -> f32 {
        if !position.iter().chain(normal.iter()).all(|v| v.is_finite()) {
            return 0.0;
        }
        let directional = self.directional(position);
        let ambient = ambient_index(position).map_or(0.0, |i| self.ambient[i]);
        ambient + surface_response(directional, normal)
    }

    /// Uses the rendered authored face normal, including the reverse face for
    /// two-sided wall materials. Do not automatically face normals to camera.
    pub fn sample_wall(&self, position: [f32; 2], normal: [f32; 2]) -> f32 {
        let length = (normal[0] * normal[0] + normal[1] * normal[1]).sqrt();
        if !length.is_finite() || length <= 0.0 {
            return 0.0;
        }
        let normal = normal.map(|v| v / length);
        self.sample(
            [
                position[0] + normal[0] * 1.55 * TEXEL_SIZE,
                position[1] + normal[1] * 1.55 * TEXEL_SIZE,
            ],
            [normal[0], normal[1], 0.0],
        )
    }

    fn directional(&self, position: [f32; 2]) -> [f32; 4] {
        let (x0, x1, blend_x) = sample_axis(position[0]);
        let (y0, y1, blend_y) = sample_axis(position[1]);
        let row0 = y0 * FIELD_SIDE;
        let row1 = y1 * FIELD_SIDE;
        let a = self.field[row0 + x0];
        let b = self.field[row0 + x1];
        let c = self.field[row1 + x0];
        let d = self.field[row1 + x1];
        std::array::from_fn(|i| {
            let top = a[i] + (b[i] - a[i]) * blend_x;
            let bottom = c[i] + (d[i] - c[i]) * blend_x;
            top + (bottom - top) * blend_y
        })
    }
}

#[inline]
fn sample_axis(position: f32) -> (usize, usize, f32) {
    // Ordinary map fragments need no floating-point remainder. Keep the
    // original reduction for negative/border/huge coordinates: reducing a
    // negative coordinate can round to 64, so wrapping after multiplication
    // would not preserve the same interpolation weights in all cases.
    let wrapped = if (0.0..MAP_SIDE as f32).contains(&position) {
        position
    } else {
        position.rem_euclid(MAP_SIDE as f32)
    };
    let texel = wrapped * TEXELS_PER_TILE - 0.5;
    let floor = texel.floor();
    let base = floor as i32;
    const MASK: i32 = FIELD_SIDE as i32 - 1;
    const { assert!(FIELD_SIDE.is_power_of_two()) };
    (
        (base & MASK) as usize,
        ((base + 1) & MASK) as usize,
        texel - floor,
    )
}

/// F is 10*setting with tone mapping, or 5*setting without it. Distance must
/// be full 3D world-to-camera length, not projected depth. Valid setting range
/// in the remaster is .05..2, making F positive and at most 20.
pub fn distance_multiplier(distance: f32, fade: f32) -> f32 {
    if !distance.is_finite() || !fade.is_finite() || distance < 0.0 || fade <= 0.0 {
        return 0.0;
    }
    let d = (fade / (distance + fade))
        .max((fade / 50.0).min(1.0))
        .min(1.0);
    d * d
}

fn ambient_value(raw: u8) -> f32 {
    // Outside the observed 0..15 range preserve the helper's integer/byte
    // operations, rather than inventing an intensity interpolation.
    let b = (35 - i32::from(raw)) * 255 / 35;
    let value = 25 + (b - 145) * 230 / 110;
    f32::from(value as u8) / 255.0
}

fn ambient_index(position: [f32; 2]) -> Option<usize> {
    // Shader float-to-signed conversion truncates toward zero, including the
    // narrow negative interval reached by a wall displacement at a map edge.
    let xy = position.map(f32::trunc);
    xy.iter()
        .all(|v| *v >= 0.0 && *v < MAP_SIDE as f32)
        .then(|| xy[0] as usize * MAP_SIDE + xy[1] as usize)
}

fn surface_response(field: [f32; 4], normal: [f32; 3]) -> f32 {
    let length = normal.iter().map(|v| v * v).sum::<f32>().sqrt();
    if !length.is_finite() || length <= 0.0 {
        return 0.0;
    }
    let normal = normal.map(|v| v / length);
    let horizontal = (normal[0] * field[0]).max(0.0)
        + (normal[1] * field[1]).max(0.0)
        + (normal[0] * field[2]).max(0.0)
        + (normal[1] * field[3]).max(0.0);
    horizontal + normal[2].abs() * field.iter().map(|v| v * v).sum::<f32>().sqrt()
}

fn allocate<T: Clone>(count: usize, value: T) -> Result<Vec<T>, LightingError> {
    let mut result = Vec::new();
    result
        .try_reserve_exact(count)
        .map_err(|_| LightingError::Allocation)?;
    result.resize(count, value);
    Ok(result)
}

fn charge(remaining: &mut usize, work: usize, error: LightingError) -> Result<(), LightingError> {
    *remaining = remaining.checked_sub(work).ok_or(error)?;
    Ok(())
}

#[derive(Clone, Copy)]
struct Rect {
    x0: usize,
    y0: usize,
    x1: usize,
    y1: usize,
}
impl Rect {
    fn bounds(min: [f32; 2], max: [f32; 2]) -> Self {
        let lower =
            min.map(|v| (v * TEXELS_PER_TILE).floor().clamp(0.0, FIELD_SIDE as f32) as usize);
        let upper =
            max.map(|v| (v * TEXELS_PER_TILE).ceil().clamp(0.0, FIELD_SIDE as f32) as usize);
        Self {
            x0: lower[0],
            y0: lower[1],
            x1: upper[0],
            y1: upper[1],
        }
    }
    fn area(self) -> usize {
        self.x1.saturating_sub(self.x0) * self.y1.saturating_sub(self.y0)
    }
    fn intersect(self, other: Self) -> Self {
        let x0 = self.x0.max(other.x0);
        let y0 = self.y0.max(other.y0);
        Self {
            x0,
            y0,
            x1: self.x1.min(other.x1).max(x0),
            y1: self.y1.min(other.y1).max(y0),
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct ShadowVertex {
    position: [f32; 2],
    uw: f32,
    width: f32,
}

fn shadow_vertices(
    a: [f32; 2],
    b: [f32; 2],
    source: [f32; 2],
    u: f32,
) -> Option<[ShadowVertex; 4]> {
    let extrude = |p: [f32; 2]| {
        let delta = [p[0] - source[0], p[1] - source[1]];
        let length = (delta[0] * delta[0] + delta[1] * delta[1]).sqrt();
        if !length.is_finite() || length <= 1e-8 {
            return None;
        }
        let direction = delta.map(|v| v / length);
        Some([
            [
                p[0] - direction[0] * TEXEL_SIZE * 0.5,
                p[1] - direction[1] * TEXEL_SIZE * 0.5,
            ],
            [p[0] + direction[0] * 32.0, p[1] + direction[1] * 32.0],
        ])
    };
    let a = extrude(a)?;
    let b = extrude(b)?;
    let width = |i: usize| ((a[i][0] - b[i][0]).powi(2) + (a[i][1] - b[i][1]).powi(2)).sqrt();
    let near = width(0);
    let far = width(1);
    if near <= 1e-8 || far <= 1e-8 {
        return None;
    }
    Some([
        ShadowVertex {
            position: a[0],
            uw: u * near,
            width: near,
        },
        ShadowVertex {
            position: b[0],
            uw: 0.0,
            width: near,
        },
        ShadowVertex {
            position: a[1],
            uw: u * far,
            width: far,
        },
        ShadowVertex {
            position: b[1],
            uw: 0.0,
            width: far,
        },
    ])
}

fn cross(a: [f32; 2], b: [f32; 2], p: [f32; 2]) -> f32 {
    (b[0] - a[0]) * (p[1] - a[1]) - (b[1] - a[1]) * (p[0] - a[0])
}

fn raster_shadow(
    mask: &mut [f32],
    source_rect: Rect,
    vertices: [ShadowVertex; 3],
    texture: ShadowTexture<'_>,
    remaining: &mut usize,
) -> Result<(), LightingError> {
    let [a, b, c] = vertices;
    let area = cross(a.position, b.position, c.position);
    if !area.is_finite() || area.abs() <= 1e-10 {
        return Ok(());
    }
    let min = std::array::from_fn(|i| a.position[i].min(b.position[i]).min(c.position[i]));
    let max = std::array::from_fn(|i| a.position[i].max(b.position[i]).max(c.position[i]));
    let rect = source_rect.intersect(Rect::bounds(min, max));
    charge(remaining, rect.area(), LightingError::ShadowWorkLimit)?;
    for y in rect.y0..rect.y1 {
        for x in rect.x0..rect.x1 {
            let p = [(x as f32 + 0.5) * TEXEL_SIZE, (y as f32 + 0.5) * TEXEL_SIZE];
            let weights = [
                cross(b.position, c.position, p) / area,
                cross(c.position, a.position, p) / area,
                cross(a.position, b.position, p) / area,
            ];
            if weights.iter().any(|w| *w < 0.0) {
                continue;
            }
            let width = weights[0] * a.width + weights[1] * b.width + weights[2] * c.width;
            let uw = weights[0] * a.uw + weights[1] * b.uw + weights[2] * c.uw;
            let u = uw / width;
            let alpha = texture.alpha(u, 0.7).min(texture.alpha(u, 0.3));
            let index = y * FIELD_SIDE + x;
            mask[index] = mask[index].max(alpha);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::{Player, Wall};

    fn source() -> Light {
        Light {
            position: [7552, 13952],
            power: 128,
            inner_radius: 193,
            outer_radius: 385,
            brightness: 0,
        }
    }

    fn map(lights: &[Light]) -> Map {
        // Entirely synthetic container, including its wall and light sections.
        let mut bytes = vec![0u8; 0x27001 + 2 + lights.len() * 12 + 2];
        bytes[0x25001..0x26001].fill(15);
        bytes[0x27001..0x27003].copy_from_slice(&(lights.len() as u16).to_le_bytes());
        for (i, light) in lights.iter().enumerate() {
            let words = [
                light.position[0],
                light.position[1],
                light.power,
                light.inner_radius,
                light.outer_radius,
                light.brightness,
            ];
            for (j, word) in words.iter().enumerate() {
                let offset = 0x27003 + i * 12 + j * 2;
                bytes[offset..offset + 2].copy_from_slice(&word.to_le_bytes());
            }
        }
        // One authored full-width wall at logical cell (2,2).
        let offset = 0x18001 + (2 * MAP_SIDE + 2) * 11;
        bytes[offset] = 1;
        bytes[offset + 1] = 128;
        Map::parse(&bytes).unwrap()
    }

    fn world(walls: Vec<Wall>) -> World {
        World {
            walls,
            player: Player {
                position: [0.0; 2],
                angle: 0.0,
            },
        }
    }

    fn close(a: f32, b: f32) {
        assert!((a - b).abs() < 1e-5, "{a} != {b}");
    }

    #[test]
    fn signed_source_decode_and_observed_record() {
        let decoded = Source::decode(source()).unwrap();
        assert_eq!(decoded.position, [29.5, 54.5]);
        assert_eq!(decoded.inner, 0.75390625);
        assert_eq!(decoded.outer, 401.0 / 256.0);
        assert_eq!(decoded.maximum, 1.0);
        assert_eq!(decoded.power, 1.0);
        let signed = Source::decode(Light {
            position: [(-256i16) as u16, 512],
            power: (-128i16) as u16,
            inner_radius: (-1i16) as u16,
            outer_radius: 256,
            brightness: 0xc020,
        })
        .unwrap();
        assert_eq!(signed.position, [-1.0, 2.0]);
        assert_eq!(signed.power, 1.0);
        assert_eq!(signed.maximum, 0.75);
        assert!(signed.two_sided);
        assert_eq!(signed.inner, 0.0);
        assert_eq!(
            signed.contribution(signed.position, 0.0),
            [0.75, 0.75, -0.75, -0.75]
        );
    }

    #[test]
    fn source_falloff_direction_and_shadow_precede_accumulation() {
        let s = Source::decode(source()).unwrap();
        assert_eq!(
            s.contribution([s.position[0] - 0.5, s.position[1]], 0.0),
            [1.0, 0.0, 0.0, 0.0]
        );
        let distance = (s.inner + s.outer) * 0.5;
        let sample = [s.position[0] + distance, s.position[1]];
        close(s.contribution(sample, 0.0)[2], -0.5);
        close(s.contribution(sample, 0.25)[2], -0.375);
        assert_eq!(s.contribution(sample, 1.0), [0.0; 4]);
        assert_eq!(
            s.contribution([s.position[0] + s.outer, s.position[1]], 0.0),
            [0.0; 4]
        );
        assert_eq!(s.contribution(s.position, 0.0), [0.0; 4]);
        close(
            surface_response([2.0, 0.0, -2.0, 0.0], [0.0, 0.0, 2.0]),
            8.0f32.sqrt(),
        );
        assert_eq!(
            surface_response([2.0, 0.0, -2.0, 0.0], [-2.0, 0.0, 0.0]),
            2.0
        );
    }

    #[test]
    fn ambient_integer_mapping_and_x_major_nearest_lookup() {
        for (raw, expected) in [(0, 255), (5, 177), (10, 102), (15, 25)] {
            close(ambient_value(raw), expected as f32 / 255.0);
        }
        assert_eq!(ambient_index([2.99, 3.01]), Some(2 * MAP_SIDE + 3));
        assert_eq!(ambient_index([-0.09, 3.0]), Some(3));
        assert_eq!(ambient_index([64.0, 3.0]), None);
        assert_eq!(ambient_index([-1.0, 3.0]), None);
    }

    #[test]
    fn bilinear_repeat_wall_displacement_and_uncapped_overlap() {
        let map = map(&[source(), source()]);
        let mut built = RemasterLightMap::build(&map, &world(vec![]), &[], &[]).unwrap();
        let p = [29.0 + TEXEL_SIZE * 0.5, 54.5 + TEXEL_SIZE * 0.5];
        assert!(built.sample(p, [1.0, 0.0, 0.0]) > 2.0);
        built.field.fill([0.0; 4]);
        built.ambient.fill(0.0);
        built.field[FIELD_SIDE - 1] = [2.0, 0.0, 0.0, 0.0];
        built.field[0] = [6.0, 0.0, 0.0, 0.0];
        close(built.sample([0.0, TEXEL_SIZE * 0.5], [1.0, 0.0, 0.0]), 4.0);
        close(built.sample([64.0, TEXEL_SIZE * 0.5], [1.0, 0.0, 0.0]), 4.0);
        built.ambient[MAP_SIDE] = 0.75;
        close(built.sample_wall([0.95, 0.5], [1.0, 0.0]), 0.75);
        close(built.sample([0.95, 0.5], [1.0, 0.0, 0.0]), 0.0);
        assert_eq!(built.sample([f32::NAN, 0.0], [1.0, 0.0, 0.0]), 0.0);
    }

    #[test]
    fn fast_sample_coordinates_match_original_repeat_bits() {
        let reference = |position: f32| {
            let texel = position.rem_euclid(MAP_SIDE as f32) * TEXELS_PER_TILE - 0.5;
            let base = texel.floor() as i32;
            (
                base.rem_euclid(FIELD_SIDE as i32) as usize,
                (base + 1).rem_euclid(FIELD_SIDE as i32) as usize,
                (texel - texel.floor()).to_bits(),
            )
        };
        let check = |position| {
            let (a, b, blend) = sample_axis(position);
            assert_eq!((a, b, blend.to_bits()), reference(position), "{position:?}");
        };
        for value in [
            -f32::MAX,
            -128.0,
            -64.0,
            -63.999996,
            -0.096875,
            -f32::MIN_POSITIVE,
            -f32::from_bits(1),
            -0.0,
            0.0,
            f32::from_bits(1),
            f32::MIN_POSITIVE,
            0.03125,
            0.096875,
            1.0,
            63.999996,
            64.0,
            64.09687,
            128.0,
            f32::MAX,
        ] {
            check(value);
        }
        // Exercise both ordinary fragments and arbitrary finite f32 bit
        // patterns, without a dependency or nondeterministic random seed.
        let mut bits = 0x8123_4567u32;
        for i in 0..16_384 {
            check(i as f32 / 256.0);
            bits = bits.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let value = f32::from_bits(bits);
            if value.is_finite() {
                check(value);
            }
        }
    }

    #[test]
    fn extruded_shadow_respects_alpha_strips_height_and_material() {
        let raw = Light {
            position: [384, 640],
            power: 128,
            inner_radius: 0,
            outer_radius: 1024,
            brightness: 0,
        };
        let map = map(&[raw]);
        let wall = Wall {
            a: [2.0, 2.0],
            b: [2.0, 3.0],
            texture: 1,
            blocks_movement: false,
            cell: Some([2, 2]),
        };
        let world = world(vec![wall]);
        let opaque = [0xff00_0000; 10];
        let mut strips = opaque;
        strips[3] = 0;
        let texture = |pixels| ShadowTexture {
            width: 1,
            height: 10,
            pixels,
        };
        let sample = |textures: &[Option<ShadowTexture<'_>>], elevations: &[f32]| {
            RemasterLightMap::build(&map, &world, elevations, textures)
                .unwrap()
                .sample([3.0, 2.5], [-1.0, 0.0, 0.0])
        };
        let unshadowed = sample(&[], &[]);
        assert!(unshadowed > 0.5);
        close(sample(&[None, Some(texture(&opaque))], &[]), 25.0 / 255.0);
        close(sample(&[None, Some(texture(&strips))], &[]), unshadowed);
        close(
            sample(&[None, Some(texture(&opaque))], &[81.0 / 256.0]),
            unshadowed,
        );
        close(
            sample(&[None, Some(texture(&opaque))], &[80.0 / 256.0]),
            25.0 / 255.0,
        );
        let vertices = shadow_vertices([2.0, 2.0], [2.0, 3.0], [1.5, 2.5], 1.0).unwrap();
        close(vertices[0].uw / vertices[0].width, 1.0);
        close(vertices[1].uw / vertices[1].width, 0.0);
        let displacement = [vertices[0].position[0] - 2.0, vertices[0].position[1] - 2.0];
        close(
            (displacement[0].powi(2) + displacement[1].powi(2)).sqrt(),
            0.5 * TEXEL_SIZE,
        );
    }

    #[test]
    fn malformed_sources_work_limits_and_camera_response() {
        let invalid = Light {
            brightness: 129,
            ..source()
        };
        assert!(Source::decode(invalid).is_none());
        assert!(Source::decode(Light {
            outer_radius: (-256i16) as u16,
            ..source()
        })
        .is_none());
        let map = map(&[invalid]);
        let built = RemasterLightMap::build(&map, &world(vec![]), &[], &[]).unwrap();
        assert_eq!(built.skipped_sources(), 1);
        let mut budget = 10;
        assert_eq!(
            charge(&mut budget, 10, LightingError::SourceWorkLimit),
            Ok(())
        );
        assert_eq!(
            charge(&mut budget, 1, LightingError::SourceWorkLimit),
            Err(LightingError::SourceWorkLimit)
        );
        close(distance_multiplier(0.0, 10.0), 1.0);
        close(distance_multiplier(10.0, 10.0), 0.25);
        close(distance_multiplier(10000.0, 10.0), 0.04);
        close(distance_multiplier(5.0, 5.0), 0.25);
        assert_eq!(distance_multiplier(f32::NAN, 10.0), 0.0);
    }

    #[test]
    fn installed_map01_builds_bounded_finite_field() {
        let explicit = std::env::var_os("OPENCHASM_TEST_CONTENT");
        let path = explicit
            .as_ref()
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from("assets/csm.bin"));
        if !path.exists() && explicit.is_none() {
            return;
        }
        let mut resources = crate::content::ResourceSet::open(&path).unwrap();
        let last_level = if std::env::var_os("OPENCHASM_TEST_ALL_LIGHTMAPS").is_some() {
            19
        } else {
            1
        };
        for level in 1..=last_level {
            let map = Map::parse(&resources.read(&format!("map/map.{level:02}")).unwrap()).unwrap();
            let graphics = crate::parse_wall_graphics(
                &resources.read(&format!("map/resource.{level:02}")).unwrap(),
            );
            let images = crate::load_wall_textures(&mut resources, &graphics);
            let packed: Vec<Option<Vec<u32>>> = images
                .iter()
                .map(|image| {
                    image.as_ref().map(|image| {
                        image
                            .indices
                            .iter()
                            .map(|&index| if index == 255 { 0 } else { 0xff00_0000 })
                            .collect()
                    })
                })
                .collect();
            let textures: Vec<_> = images
                .iter()
                .zip(&packed)
                .map(|(image, pixels)| {
                    image
                        .as_ref()
                        .zip(pixels.as_ref())
                        .map(|(image, pixels)| ShadowTexture {
                            width: image.width as usize,
                            height: image.height as usize,
                            pixels,
                        })
                })
                .collect();
            let world = crate::make_world(&map, &graphics).unwrap();
            let started = std::time::Instant::now();
            let field = RemasterLightMap::build(&map, &world, &[], &textures).unwrap();
            eprintln!(
                "MAP{level:02} remaster field: {} sources, {} skipped, {:.3}s",
                map.lights.len(),
                field.skipped_sources(),
                started.elapsed().as_secs_f64()
            );
            assert!(field.field.iter().flatten().all(|value| value.is_finite()));
            assert!(field
                .field
                .iter()
                .any(|value| value.iter().any(|channel| *channel != 0.0)));
            for position in [
                world.player.position,
                [29.5, 54.5],
                [29.0, 54.5],
                [30.5, 54.5],
                [0.0, 0.0],
                [63.99, 63.99],
            ] {
                for normal in [[0.0, 0.0, 1.0], [1.0, 0.0, 0.0], [-1.0, 0.0, 0.0]] {
                    let value = field.sample(position, normal);
                    assert!(
                        value.is_finite() && value >= 0.0,
                        "{position:?} {normal:?}: {value}"
                    );
                }
                let value = field.sample_wall(position, [0.0, 1.0]);
                assert!(value.is_finite() && value >= 0.0);
            }
            // This is an installed-asset construction smoke test, not a pixel
            // comparison against the original renderer.
        }
    }
}
