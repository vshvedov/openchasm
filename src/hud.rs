//! Normal remaster status HUD, drawn after world/weapon tone mapping.
//!
//! Layout and atlas selection follow `docs/hud-research.md`. Nearest sampling is
//! an explicit approximation until the original HUD sampler is verified. The
//! original fragment shader discards alpha < 0.5 and outputs half-alpha, but its
//! pipeline descriptor selects blend mode zero (source ONE, destination ZERO
//! in the shared pipeline builder). We therefore replace RGB for surviving
//! texels; half-alpha alone is not evidence for translucent source-over.
//! Special-state faces, alternate bars and keys require
//! more state than this initial interface supplies and are not synthesized.
use crate::content::ResourceSet;
use crate::texture::CelImage;
use std::io::{self, Cursor};

const MAX_IMAGE_BYTES: usize = 4 * 1024 * 1024;
const MAX_FRAME_PIXELS: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HudState {
    pub health: u32,
    pub armor: u32,
    pub ammo: u32,
    /// Authored weapon index 0..7; index zero has no ammunition readout.
    pub weapon: usize,
}

struct Image {
    width: usize,
    height: usize,
    pixels: Vec<u32>, // ARGB; alpha rules depend on the atlas.
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

impl Image {
    fn cel(bytes: &[u8], weapon_icons: bool) -> io::Result<Self> {
        let cel = CelImage::parse(bytes).map_err(|e| invalid(&e.to_string()))?;
        let pixels = cel
            .indices
            .iter()
            .map(|&index| {
                let [r, g, b] = cel.palette.rgb8(index);
                let alpha = if weapon_icons {
                    if index == 0 {
                        180
                    } else {
                        255
                    }
                } else if index == 0 || index == 255 {
                    0
                } else {
                    255
                };
                (alpha << 24) | (u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b)
            })
            .collect();
        Ok(Self {
            width: cel.width.into(),
            height: cel.height.into(),
            pixels,
        })
    }

    fn png(bytes: &[u8]) -> io::Result<Self> {
        if bytes.len() > MAX_IMAGE_BYTES {
            return Err(invalid("HUD PNG exceeds encoded size limit"));
        }
        let mut decoder = png::Decoder::new(Cursor::new(bytes));
        decoder.set_limits(png::Limits {
            bytes: MAX_IMAGE_BYTES,
        });
        decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
        let mut reader = decoder.read_info().map_err(|e| invalid(&e.to_string()))?;
        let width = reader.info().width as usize;
        let height = reader.info().height as usize;
        let count = width
            .checked_mul(height)
            .filter(|&n| n > 0 && n <= MAX_IMAGE_BYTES / 4)
            .ok_or_else(|| invalid("invalid HUD PNG dimensions"))?;
        let size = reader
            .output_buffer_size()
            .filter(|&n| n <= MAX_IMAGE_BYTES)
            .ok_or_else(|| invalid("HUD PNG exceeds decoded size limit"))?;
        let mut bytes = vec![0; size];
        let info = reader
            .next_frame(&mut bytes)
            .map_err(|e| invalid(&e.to_string()))?;
        let channels = match info.color_type {
            png::ColorType::Rgb => 3,
            png::ColorType::Rgba => 4,
            png::ColorType::Grayscale => 1,
            png::ColorType::GrayscaleAlpha => 2,
            png::ColorType::Indexed => return Err(invalid("unexpanded HUD PNG palette")),
        };
        if info.buffer_size() != count * channels {
            return Err(invalid("HUD PNG pixel length mismatch"));
        }
        let pixels = bytes[..info.buffer_size()]
            .chunks_exact(channels)
            .map(|p| {
                let (r, g, b) = if channels < 3 {
                    (p[0], p[0], p[0])
                } else {
                    (p[0], p[1], p[2])
                };
                let a = match channels {
                    2 => p[1],
                    4 => p[3],
                    _ => 255,
                };
                (u32::from(a) << 24) | (u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b)
            })
            .collect();
        Ok(Self {
            width,
            height,
            pixels,
        })
    }

    fn dimensions(self, width: usize, height: usize) -> io::Result<Self> {
        if (self.width, self.height) != (width, height) {
            Err(invalid(
                "HUD atlas dimensions do not match the authored layout",
            ))
        } else {
            Ok(self)
        }
    }
}

fn atlas(
    resources: &mut ResourceSet,
    name: &str,
    width: usize,
    height: usize,
    weapon_icons: bool,
) -> io::Result<Image> {
    let image = match resources.read(&format!("{name}.png")) {
        Ok(bytes) => Image::png(&bytes)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            Image::cel(&resources.read(name)?, weapon_icons)?
        }
        Err(error) => return Err(error),
    };
    image.dimensions(width, height)
}

pub struct Hud {
    status: Image,
    digits: Image,
    weapons: Image,
    faces: Option<Image>,
}

impl Hud {
    /// Load only through ResourceSet so resource traces include these assets.
    /// Missing optional extra faces retain the face in the status artwork;
    /// malformed present assets are errors rather than silently ignored.
    pub fn load(resources: &mut ResourceSet) -> io::Result<Self> {
        let status = atlas(resources, "texture/status2.cel", 320, 76, false)?;
        let digits = atlas(resources, "texture/bfont2.cel", 230, 39, false)?;
        let weapons = atlas(resources, "texture/wicons.cel", 224, 25, true)?;
        let faces = match resources.read("texture/hud-player-extra.png") {
            Ok(bytes) => Some(Image::png(&bytes)?.dimensions(182, 52)?),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        Ok(Self {
            status,
            digits,
            weapons,
            faces,
        })
    }

    /// Render into display RGB (0x00RRGGBB), without allocating. Invalid frame
    /// dimensions or length leave the frame untouched. Small frames clip the
    /// original minimum scale of two rather than inventing a fractional scale.
    pub fn render(&self, frame: &mut [u32], width: usize, height: usize, state: HudState) {
        let Some(count) = width.checked_mul(height) else {
            return;
        };
        if width == 0
            || height == 0
            || width > 32768
            || height > 32768
            || count > MAX_FRAME_PIXELS
            || count != frame.len()
        {
            return;
        }
        let scale = ((height + 160) / 320).min(width / 320).max(2) as i64;
        let left = (width / 2) as i64 - 160 * scale;
        let mut canvas = Canvas {
            frame,
            width,
            height,
            scale,
        };
        canvas.blit(
            &self.status,
            [0, 12, 320, 32],
            left,
            height as i64 - 32 * scale,
        );
        if let (Some(faces), Some(column)) = (&self.faces, face_column(state.health)) {
            canvas.blit(
                faces,
                [column * 26, 0, 26, 26],
                left + 4 * scale,
                height as i64 - 29 * scale,
            );
        }
        self.number(&mut canvas, left + 104 * scale, state.health, 25);
        if state.weapon < 8 {
            canvas.blit(
                &self.weapons,
                [state.weapon * 28 + 1, 1, 26, 23],
                left + 106 * scale,
                height as i64 - 28 * scale,
            );
            if state.weapon != 0 {
                self.number(&mut canvas, left + 205 * scale, state.ammo, 10);
            }
        }
        self.number(&mut canvas, left + 315 * scale, state.armor, 25);
    }

    fn number(&self, canvas: &mut Canvas<'_>, anchor: i64, value: u32, threshold: u32) {
        let mut remaining = value;
        let mut right = anchor;
        let row = if value < threshold { 20 } else { 0 };
        loop {
            let digit = (remaining % 10) as usize;
            right -= 23 * canvas.scale;
            canvas.blit(
                &self.digits,
                [digit * 23, row, 22, 19],
                right,
                canvas.height as i64 - 25 * canvas.scale,
            );
            remaining /= 10;
            if remaining == 0 {
                break;
            }
        }
    }
}

fn face_column(health: u32) -> Option<usize> {
    match health {
        81.. => None,
        61..=80 => Some(0),
        41..=60 => Some(1),
        21..=40 => Some(2),
        1..=20 => Some(3),
        0 => Some(4),
    }
}

struct Canvas<'a> {
    frame: &'a mut [u32],
    width: usize,
    height: usize,
    scale: i64,
}
impl Canvas<'_> {
    fn blit(&mut self, image: &Image, source: [usize; 4], x: i64, y: i64) {
        let [sx, sy, w, h] = source;
        let x0 = x.max(0).min(self.width as i64) as usize;
        let y0 = y.max(0).min(self.height as i64) as usize;
        let x1 = (x + w as i64 * self.scale).max(0).min(self.width as i64) as usize;
        let y1 = (y + h as i64 * self.scale).max(0).min(self.height as i64) as usize;
        for dy in y0..y1 {
            let src_y = sy + ((dy as i64 - y) / self.scale) as usize;
            for dx in x0..x1 {
                let src_x = sx + ((dx as i64 - x) / self.scale) as usize;
                let pixel = image.pixels[src_y * image.width + src_x];
                if pixel >> 24 >= 128 {
                    self.frame[dy * self.width + dx] = pixel & 0x00ff_ffff;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn image(width: usize, height: usize, color: u32) -> Image {
        Image {
            width,
            height,
            pixels: vec![color; width * height],
        }
    }
    fn synthetic() -> Hud {
        let mut digits = image(230, 39, 0);
        for d in 0..10 {
            for y in 0..19 {
                for x in 0..22 {
                    digits.pixels[y * 230 + d * 23 + x] = 0xff00_0000 | (d as u32 + 1);
                    digits.pixels[(y + 20) * 230 + d * 23 + x] =
                        0xff00_0000 | ((d as u32 + 1) << 8);
                }
            }
        }
        Hud {
            status: image(320, 76, 0xff12_3456),
            digits,
            weapons: image(224, 25, 0),
            faces: None,
        }
    }
    #[test]
    fn original_scale_centers_bar_without_stretching() {
        let hud = synthetic();
        let mut frame = vec![0xabcdef; 1920 * 1080];
        hud.render(&mut frame, 1920, 1080, HudState::default());
        assert_eq!(frame[984 * 1920 + 480], 0x123456);
        assert_eq!(frame[983 * 1920 + 480], 0xabcdef);
        assert_eq!(frame[1000 * 1920 + 479], 0xabcdef);
        assert_eq!(frame[1000 * 1920 + 1440], 0xabcdef);
    }
    #[test]
    fn digits_use_authored_spacing_low_row_and_right_anchors() {
        let hud = synthetic();
        let mut frame = vec![0; 640 * 400];
        hud.render(
            &mut frame,
            640,
            400,
            HudState {
                health: 100,
                ammo: 9,
                armor: 25,
                weapon: 1,
            },
        );
        assert_eq!(frame[350 * 640 + 70], 2); // health '1'
        assert_eq!(frame[350 * 640 + 114], 0x123456); // two-pixel gap
        assert_eq!(frame[350 * 640 + 116], 1); // health '0'
        assert_eq!(frame[350 * 640 + 364], 10 << 8); // ammo low row '9'
        assert_eq!(frame[350 * 640 + 538], 3); // armor normal row '2'
        assert_eq!(frame[388 * 640 + 70], 0x123456);
    }
    #[test]
    fn source_rows_faces_and_alpha_cutoff_are_distinct() {
        let mut hud = synthetic();
        hud.status = image(320, 76, 0);
        hud.status.pixels[12 * 320 + 40] = 0x7fff_ffff;
        hud.status.pixels[12 * 320 + 41] = 0x8011_2233;
        let mut faces = image(182, 52, 0);
        faces.pixels[4 * 26] = 0xffff_0000;
        hud.faces = Some(faces);
        let mut frame = vec![0xabcdef; 640 * 400];
        hud.render(&mut frame, 640, 400, HudState::default());
        assert_eq!(frame[336 * 640 + 80], 0xabcdef);
        assert_eq!(frame[336 * 640 + 82], 0x112233);
        assert_eq!(frame[342 * 640 + 8], 0xff0000);
        for (health, column) in [
            (0, Some(4)),
            (20, Some(3)),
            (21, Some(2)),
            (40, Some(2)),
            (41, Some(1)),
            (60, Some(1)),
            (61, Some(0)),
            (80, Some(0)),
            (81, None),
        ] {
            assert_eq!(face_column(health), column);
        }
    }
    #[test]
    fn tiny_odd_and_invalid_frames_are_safe() {
        let hud = synthetic();
        for (w, h) in [(1, 1), (319, 11), (641, 401)] {
            let mut frame = vec![0; w * h];
            hud.render(
                &mut frame,
                w,
                h,
                HudState {
                    health: u32::MAX,
                    ammo: 0,
                    armor: 0,
                    weapon: usize::MAX,
                },
            );
            assert!(frame.iter().any(|&p| p != 0));
        }
        let mut frame = [0xabc; 7];
        for (w, h) in [(2, 4), (0, 7), (usize::MAX, 2), (32769, 1)] {
            hud.render(&mut frame, w, h, HudState::default());
            assert_eq!(frame, [0xabc; 7]);
        }
    }
    #[test]
    fn palette_zero_and_255_remain_transparent() {
        let mut bytes = vec![0; 800 + 3];
        bytes[..2].copy_from_slice(&[0x19, 0x91]);
        bytes[2..4].copy_from_slice(&3u16.to_le_bytes());
        bytes[4..6].copy_from_slice(&1u16.to_le_bytes());
        bytes[10..12].copy_from_slice(&8u16.to_le_bytes());
        bytes[12..16].copy_from_slice(&3u32.to_le_bytes());
        bytes[35] = 63;
        bytes[800..].copy_from_slice(&[0, 1, 255]);
        let image = Image::cel(&bytes, false).unwrap();
        assert_eq!(image.pixels, [0, 0xfffc0000, 0]);
        let icons = Image::cel(&bytes, true).unwrap();
        assert_eq!(icons.pixels, [0xb4000000, 0xfffc0000, 0xff000000]);
    }
    #[test]
    fn weapon_cells_omit_borders_and_melee_ammo() {
        let mut hud = synthetic();
        hud.weapons = image(224, 25, 0xffaabbcc);
        for y in 1..24 {
            for x in 29..55 {
                hud.weapons.pixels[y * 224 + x] = 0xff112233;
            }
        }
        let mut frame = vec![0; 640 * 400];
        hud.render(
            &mut frame,
            640,
            400,
            HudState {
                weapon: 1,
                ..HudState::default()
            },
        );
        assert_eq!(frame[344 * 640 + 212], 0x112233);
        assert_eq!(frame[389 * 640 + 263], 0x112233);
        assert_eq!(frame[344 * 640 + 264], 0x123456);
        assert_eq!(frame[390 * 640 + 212], 0x123456);
        assert_eq!(frame[350 * 640 + 364], 0x100); // zero ammo, low row
        hud.render(&mut frame, 640, 400, HudState::default());
        assert_eq!(frame[350 * 640 + 364], 0x123456); // melee suppresses ammo
    }
    #[test]
    fn installed_assets_load_when_available() {
        let Some(home) = std::env::var_os("HOME") else {
            return;
        };
        let path = std::path::PathBuf::from(home)
            .join(".local/share/Steam/steamapps/common/Chasm The Rift/csm.bin");
        if !path.is_file() {
            return;
        }
        let mut resources = ResourceSet::open(&path).unwrap();
        let hud = Hud::load(&mut resources).unwrap();
        let mut frame = vec![0x102030; 640 * 400];
        hud.render(
            &mut frame,
            640,
            400,
            HudState {
                health: 100,
                armor: 0,
                ammo: 20,
                weapon: 1,
            },
        );
        assert!(frame[336 * 640..].iter().any(|&p| p != 0x102030));
        assert!(frame[..336 * 640].iter().all(|&p| p == 0x102030));
    }
}
