//! Level-selected remaster cube skies, with classic CEL spherical fallback.
use crate::content::ResourceSet;
use crate::texture::{CelImage, Palette};
use std::io::Cursor;

const MAX_PIXELS: usize = 4096 * 4096;
const MAX_BYTES: usize = 64 * 1024 * 1024;

struct Image {
    width: usize,
    height: usize,
    pixels: Vec<u32>,
}
impl Image {
    fn png(bytes: &[u8]) -> Option<Self> {
        let mut decoder = png::Decoder::new(Cursor::new(bytes));
        decoder.set_limits(png::Limits { bytes: MAX_BYTES });
        decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
        let mut reader = decoder.read_info().ok()?;
        let width = usize::try_from(reader.info().width).ok()?;
        let height = usize::try_from(reader.info().height).ok()?;
        let count = width
            .checked_mul(height)
            .filter(|n| *n > 0 && *n <= MAX_PIXELS)?;
        let size = reader.output_buffer_size().filter(|n| *n <= MAX_BYTES)?;
        let mut data = vec![0; size];
        let info = reader.next_frame(&mut data).ok()?;
        let channels = match info.color_type {
            png::ColorType::Rgb => 3,
            png::ColorType::Rgba => 4,
            png::ColorType::Grayscale => 1,
            png::ColorType::GrayscaleAlpha => 2,
            png::ColorType::Indexed => return None,
        };
        let raw = data.get(..info.buffer_size())?;
        if raw.len() != count.checked_mul(channels)? {
            return None;
        }
        let pixels = raw
            .chunks_exact(channels)
            .map(|p| {
                let (r, g, b) = if channels < 3 {
                    (p[0], p[0], p[0])
                } else {
                    (p[0], p[1], p[2])
                };
                (u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b)
            })
            .collect();
        Some(Self {
            width,
            height,
            pixels,
        })
    }
    fn cel(cel: &CelImage, palette: &Palette) -> Self {
        let pixels = cel
            .indices
            .iter()
            .map(|&index| {
                let [r, g, b] = palette.rgb8(index);
                (u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b)
            })
            .collect();
        Self {
            width: usize::from(cel.width),
            height: usize::from(cel.height),
            pixels,
        }
    }
    fn sample_clamped(&self, u: f32, v: f32) -> u32 {
        let x = ((u.clamp(0.0, 1.0) * self.width as f32) as usize).min(self.width - 1);
        let y = ((v.clamp(0.0, 1.0) * self.height as f32) as usize).min(self.height - 1);
        self.pixels[y * self.width + x]
    }
    fn sample_repeat(&self, u: f32, v: f32) -> u32 {
        self.sample_clamped(u.rem_euclid(1.0), v.rem_euclid(1.0))
    }
}

enum SkyImage {
    Cube([Image; 6]),
    Classic(Image),
}
pub struct Sky(SkyImage);

/// Parse a safe level-local sky basename, avoiding arbitrary resource paths.
fn sky_name(resource: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(resource).ok()?;
    for line in text.lines() {
        let line = line.split(';').next()?.trim();
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if !key.trim().eq_ignore_ascii_case("#sky") {
            continue;
        }
        let name = value.trim();
        if name.len() > 128
            || name.is_empty()
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'.')
            || !name.to_ascii_lowercase().ends_with(".cel")
        {
            return None;
        }
        return Some(name.to_ascii_lowercase());
    }
    None
}
impl Sky {
    #[cfg(test)]
    pub(crate) fn solid(color: u32) -> Self {
        Self(SkyImage::Cube(std::array::from_fn(|_| Image {
            width: 1,
            height: 1,
            pixels: vec![color],
        })))
    }
    /// Prefer all six remaster PNG cube faces; incomplete sets fall back to CEL.
    pub fn load(resources: &mut ResourceSet, resource: &[u8], palette: &Palette) -> Option<Self> {
        let name = sky_name(resource)?;
        let base = format!("texture/{name}");
        let faces: Option<Vec<_>> = (0..6)
            .map(|i| {
                resources
                    .read(&format!("{base}.{i}.png"))
                    .ok()
                    .and_then(|bytes| Image::png(&bytes))
            })
            .collect();
        if let Some(faces) = faces {
            if faces
                .iter()
                .all(|face| face.width == face.height && face.width == faces[0].width)
            {
                if let Ok(faces) = faces.try_into() {
                    return Some(Self(SkyImage::Cube(faces)));
                }
            }
        }
        let bytes = resources.read(&base).ok()?;
        let cel = CelImage::parse(&bytes).ok()?;
        Some(Self(SkyImage::Classic(Image::cel(&cel, palette))))
    }
    /// A world-space direction with z up. Cubemap face order follows OpenGL
    /// +X, -X, +Y, -Y, +Z, -Z with Y as the cube's vertical axis.
    pub fn sample(&self, direction: [f32; 3]) -> u32 {
        let [x, y, z] = direction;
        match &self.0 {
            SkyImage::Cube(faces) => {
                let (cx, cy, cz) = (x, z, y);
                let (ax, ay, az) = (cx.abs(), cy.abs(), cz.abs());
                let (i, sc, tc, major) = if ax >= ay && ax >= az {
                    if cx >= 0.0 {
                        (0, -cz, -cy, ax)
                    } else {
                        (1, cz, -cy, ax)
                    }
                } else if ay >= az {
                    if cy >= 0.0 {
                        (2, cx, cz, ay)
                    } else {
                        (3, cx, -cz, ay)
                    }
                } else if cz >= 0.0 {
                    (4, cx, -cy, az)
                } else {
                    (5, -cx, -cy, az)
                };
                if major <= 0.0 || !major.is_finite() {
                    return 0;
                }
                faces[i].sample_clamped(0.5 * (sc / major + 1.0), 0.5 * (tc / major + 1.0))
            }
            SkyImage::Classic(image) => {
                let len = x.hypot(y).hypot(z);
                if len <= 0.0 || !len.is_finite() {
                    return 0;
                }
                let u = 5.0 * y.atan2(x) / std::f32::consts::TAU;
                let v = -3.0 * (z / len).clamp(-1.0, 1.0).asin() / std::f32::consts::FRAC_PI_2;
                image.sample_repeat(u, v)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_resource_name_and_rejects_paths() {
        assert_eq!(
            sky_name(b"#name=x\n#Sky = SNOWSKY.CEL ; comment\n"),
            Some("snowsky.cel".into())
        );
        assert_eq!(sky_name(b"#sky=../secret.cel"), None);
        assert_eq!(sky_name(b"#sky=bad.png"), None);
    }
    #[test]
    fn cube_selects_six_axes_and_clamps_edges() {
        let sky = Sky(SkyImage::Cube(std::array::from_fn(|i| Image {
            width: 1,
            height: 1,
            pixels: vec![i as u32],
        })));
        for (dir, index) in [
            ([1., 0., 0.], 0),
            ([-1., 0., 0.], 1),
            ([0., 0., 1.], 2),
            ([0., 0., -1.], 3),
            ([0., 1., 0.], 4),
            ([0., -1., 0.], 5),
        ] {
            assert_eq!(sky.sample(dir), index);
        }
    }

    #[test]
    fn installed_level_skies_load_matching_faces() {
        let explicit = std::env::var_os("OPENCHASM_TEST_CONTENT");
        let path = explicit
            .as_ref()
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| "assets/csm.bin".into());
        if !path.exists() && explicit.is_none() {
            return;
        }
        let mut resources = ResourceSet::open(&path).unwrap();
        let palette = Palette::parse(&resources.read("chasm2.pal").unwrap()).unwrap();
        for (level, name) in [("01", "sky.cel"), ("12", "sky1.cel"), ("17", "snowsky.cel")] {
            let resource = resources.read(&format!("map/resource.{level}")).unwrap();
            assert_eq!(sky_name(&resource).as_deref(), Some(name));
            let sky = Sky::load(&mut resources, &resource, &palette).expect("installed sky");
            assert!(matches!(sky.0, SkyImage::Cube(_)), "remaster sky faces");
            assert_ne!(sky.sample([1.0, 0.2, 0.7]), 0, "nonblack upper sky");
        }
    }
}
