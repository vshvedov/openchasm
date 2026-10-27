//! Decoding of the uncompressed 8-bit CEL resources used by the 2022 game data.
//!
//! These routines do not depend on the renderer or the resource container, so
//! the same decoded data can be used by a client, editor, or offline tool.
use std::error::Error;
use std::fmt;

const CEL_HEADER_SIZE: usize = 32;
const PALETTE_SIZE: usize = 256 * 3;
const PIXEL_OFFSET: usize = CEL_HEADER_SIZE + PALETTE_SIZE;
const MAX_PIXELS: usize = 4096 * 4096;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TextureError {
    Truncated,
    InvalidMagic,
    UnsupportedDepth(u16),
    InvalidDimensions,
    InvalidPalette,
    InvalidPixelLength,
}

impl fmt::Display for TextureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated => write!(f, "truncated CEL or palette"),
            Self::InvalidMagic => write!(f, "invalid CEL magic"),
            Self::UnsupportedDepth(depth) => write!(f, "unsupported CEL depth {depth}"),
            Self::InvalidDimensions => write!(f, "invalid or excessive CEL dimensions"),
            Self::InvalidPalette => {
                write!(f, "palette is not 256 RGB entries with 6-bit components")
            }
            Self::InvalidPixelLength => write!(f, "CEL pixel length does not match dimensions"),
        }
    }
}
impl Error for TextureError {}

/// The game's palette uses VGA DAC components in the 0..=63 range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Palette {
    rgb6: [[u8; 3]; 256],
}
impl Palette {
    /// Read exactly 256 RGB triples, validating every component.
    pub fn parse(bytes: &[u8]) -> Result<Self, TextureError> {
        if bytes.len() != PALETTE_SIZE || bytes.iter().any(|&component| component > 63) {
            return Err(TextureError::InvalidPalette);
        }
        let mut rgb6 = [[0; 3]; 256];
        for (entry, source) in rgb6.iter_mut().zip(bytes.chunks(3)) {
            entry.copy_from_slice(source);
        }
        Ok(Self { rgb6 })
    }
    /// Convert using the same 4x scaling observed in the game's palette.png.
    pub fn rgb8(&self, index: u8) -> [u8; 3] {
        self.rgb6[index as usize].map(|component| component * 4)
    }
}

/// An indexed image and its embedded palette, with pixels in row-major order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CelImage {
    pub width: u16,
    pub height: u16,
    pub palette: Palette,
    pub indices: Vec<u8>,
}
impl CelImage {
    pub fn parse(bytes: &[u8]) -> Result<Self, TextureError> {
        if bytes.len() < PIXEL_OFFSET {
            return Err(TextureError::Truncated);
        }
        if bytes[..2] != [0x19, 0x91] {
            return Err(TextureError::InvalidMagic);
        }
        let width = u16::from_le_bytes([bytes[2], bytes[3]]);
        let height = u16::from_le_bytes([bytes[4], bytes[5]]);
        let depth = u16::from_le_bytes([bytes[10], bytes[11]]);
        if depth != 8 {
            return Err(TextureError::UnsupportedDepth(depth));
        }
        let pixel_count = usize::from(width)
            .checked_mul(usize::from(height))
            .filter(|&count| count > 0 && count <= MAX_PIXELS)
            .ok_or(TextureError::InvalidDimensions)?;
        let declared_size = u32::from_le_bytes(bytes[12..16].try_into().unwrap()) as usize;
        if declared_size != pixel_count || bytes.len() != PIXEL_OFFSET + pixel_count {
            return Err(TextureError::InvalidPixelLength);
        }
        let palette = Palette::parse(&bytes[CEL_HEADER_SIZE..PIXEL_OFFSET])?;
        Ok(Self {
            width,
            height,
            palette,
            indices: bytes[PIXEL_OFFSET..].to_vec(),
        })
    }
    /// Convert to RGBA, optionally making one palette index transparent.
    /// Transparency is a material property, not inferred from the CEL header.
    pub fn rgba8(&self, transparent_index: Option<u8>) -> Vec<u8> {
        let mut rgba = Vec::with_capacity(self.indices.len() * 4);
        for &index in &self.indices {
            rgba.extend_from_slice(&self.palette.rgb8(index));
            rgba.push(if transparent_index == Some(index) {
                0
            } else {
                255
            });
        }
        rgba
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Vec<u8> {
        let mut bytes = vec![0; PIXEL_OFFSET + 4];
        bytes[..2].copy_from_slice(&[0x19, 0x91]);
        bytes[2..4].copy_from_slice(&2u16.to_le_bytes());
        bytes[4..6].copy_from_slice(&2u16.to_le_bytes());
        bytes[10..12].copy_from_slice(&8u16.to_le_bytes());
        bytes[12..16].copy_from_slice(&4u32.to_le_bytes());
        bytes[CEL_HEADER_SIZE + 3..CEL_HEADER_SIZE + 6].copy_from_slice(&[63, 16, 1]);
        bytes[PIXEL_OFFSET..].copy_from_slice(&[0, 1, 1, 0]);
        bytes
    }

    #[test]
    fn decodes_indexed_pixels_and_optional_transparency() {
        let image = CelImage::parse(&fixture()).unwrap();
        assert_eq!((image.width, image.height), (2, 2));
        assert_eq!(
            image.rgba8(Some(0)),
            vec![0, 0, 0, 0, 252, 64, 4, 255, 252, 64, 4, 255, 0, 0, 0, 0]
        );
        assert_eq!(image.rgba8(None)[3], 255);
    }

    #[test]
    fn rejects_malformed_images_and_palette() {
        assert_eq!(CelImage::parse(&[]), Err(TextureError::Truncated));
        let mut data = fixture();
        data[0] = 0;
        assert_eq!(CelImage::parse(&data), Err(TextureError::InvalidMagic));
        let mut data = fixture();
        data[10] = 16;
        assert_eq!(
            CelImage::parse(&data),
            Err(TextureError::UnsupportedDepth(16))
        );
        let mut data = fixture();
        data[2..4].copy_from_slice(&0u16.to_le_bytes());
        assert_eq!(CelImage::parse(&data), Err(TextureError::InvalidDimensions));
        let mut data = fixture();
        data[CEL_HEADER_SIZE] = 64;
        assert_eq!(CelImage::parse(&data), Err(TextureError::InvalidPalette));
        let mut data = fixture();
        data.pop();
        assert_eq!(
            CelImage::parse(&data),
            Err(TextureError::InvalidPixelLength)
        );
    }
}
