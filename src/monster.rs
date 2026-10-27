//! CAR monster geometry and raw animation/sound segments.
//!
//! The 2022 resource files embed a 3O-like model, vertex animations, optional
//! detached-part models and sounds. All numeric coordinates remain raw; see
//! docs/monster-format.md for observed format and unverified behavior.
use crate::model::{Animation, Model};
use std::io::{self, ErrorKind};

const HEADER_SIZE: usize = 0x66;
const MODEL_HEADER_SIZE: usize = 0x4806;
const MODEL_SKIN_LENGTH_OFFSET: usize = 0x4804;
const MAX_FILE_SIZE: usize = 64 * 1024 * 1024;
const SKIN_WIDTH: usize = 64;

fn invalid(message: &str) -> io::Error {
    io::Error::new(ErrorKind::InvalidData, message)
}
fn word(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
}
fn take<'a>(bytes: &'a [u8], position: &mut usize, length: usize) -> io::Result<&'a [u8]> {
    let end = position
        .checked_add(length)
        .ok_or_else(|| invalid("CAR offset overflow"))?;
    let result = bytes
        .get(*position..end)
        .ok_or_else(|| invalid("truncated CAR segment"))?;
    *position = end;
    Ok(result)
}

/// Optional detached body part. Its skin is shared with the body in the
/// observed files, so `model.skin_indices` is empty. `skin_height_hint` is
/// kept as stored rather than assumed to describe part-specific image data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BodyPart {
    pub model: Model,
    pub skin_height_hint: u16,
    /// Individual segments in the same order as their CAR header lengths.
    pub animations: [Option<Animation>; 2],
}

/// Decoded geometry and original header fields. Sound bytes are left encoded
/// because their sample format and playback behavior are not established here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Monster {
    pub raw_header: [u8; HEADER_SIZE],
    pub model: Model,
    pub animations: [Option<Animation>; 20],
    pub parts: [Option<BodyPart>; 3],
    pub fall_sound_ids: [u16; 3],
    pub sound_volumes: [u16; 8],
    pub sounds: [Vec<u8>; 8],
    /// Bytes after all declared segments, if any.
    pub trailing_data: Vec<u8>,
}

fn animation(bytes: &[u8], vertices: usize) -> io::Result<Option<Animation>> {
    if bytes.is_empty() {
        return Ok(None);
    }
    let count =
        u16::try_from(vertices).map_err(|_| invalid("CAR animation vertex count too large"))?;
    let mut ani = Vec::with_capacity(bytes.len() + 2);
    ani.extend_from_slice(&count.to_le_bytes());
    ani.extend_from_slice(bytes);
    Animation::parse(&ani)
        .map(Some)
        .map_err(|e| invalid(&format!("invalid CAR animation: {e}")))
}

impl Monster {
    pub fn parse(bytes: &[u8]) -> io::Result<Self> {
        if bytes.len() < HEADER_SIZE + MODEL_HEADER_SIZE {
            return Err(invalid("truncated CAR header or model"));
        }
        if bytes.len() > MAX_FILE_SIZE {
            return Err(invalid("CAR exceeds 64 MiB limit"));
        }
        let mut raw_header = [0u8; HEADER_SIZE];
        raw_header.copy_from_slice(&bytes[..HEADER_SIZE]);
        let animation_lengths: [u16; 20] = std::array::from_fn(|i| word(&raw_header, i * 2));
        let part_lengths: [[u16; 2]; 3] = std::array::from_fn(|i| {
            [
                word(&raw_header, 0x28 + i * 4),
                word(&raw_header, 0x2a + i * 4),
            ]
        });
        let fall_sound_ids = std::array::from_fn(|i| word(&raw_header, 0x40 + i * 2));
        let sound_lengths: [u16; 8] = std::array::from_fn(|i| word(&raw_header, 0x46 + i * 2));
        let sound_volumes = std::array::from_fn(|i| word(&raw_header, 0x56 + i * 2));

        let mut pos = HEADER_SIZE;
        let raw_model_header = take(bytes, &mut pos, MODEL_HEADER_SIZE)?;
        // CAR stores the body skin length in bytes; 3O stores rows of 64.
        let skin_bytes = usize::from(word(raw_model_header, MODEL_SKIN_LENGTH_OFFSET));
        if !skin_bytes.is_multiple_of(SKIN_WIDTH) {
            return Err(invalid("CAR skin size is not a multiple of 64"));
        }
        let skin = take(bytes, &mut pos, skin_bytes)?;
        let mut data = raw_model_header.to_vec();
        data[MODEL_SKIN_LENGTH_OFFSET..MODEL_SKIN_LENGTH_OFFSET + 2]
            .copy_from_slice(&((skin_bytes / SKIN_WIDTH) as u16).to_le_bytes());
        data.extend_from_slice(skin);
        let model = Model::parse(&data).map_err(|e| invalid(&format!("invalid CAR body: {e}")))?;
        let mut animations: [Option<Animation>; 20] = std::array::from_fn(|_| None);
        for (out, length) in animations.iter_mut().zip(animation_lengths) {
            *out = animation(
                take(bytes, &mut pos, usize::from(length))?,
                model.vertices.len(),
            )?;
        }
        let mut parts: [Option<BodyPart>; 3] = std::array::from_fn(|_| None);
        for (part, lengths) in parts.iter_mut().zip(part_lengths) {
            if lengths == [0, 0] {
                continue;
            }
            let raw_part = take(bytes, &mut pos, MODEL_HEADER_SIZE)?;
            let skin_height_hint = word(raw_part, MODEL_SKIN_LENGTH_OFFSET);
            let mut part_model = raw_part.to_vec();
            // The part contains no inline skin; keep its hint separately.
            part_model[MODEL_SKIN_LENGTH_OFFSET..MODEL_SKIN_LENGTH_OFFSET + 2]
                .copy_from_slice(&0u16.to_le_bytes());
            let part_model = Model::parse(&part_model)
                .map_err(|e| invalid(&format!("invalid CAR part: {e}")))?;
            let mut part_animations = [None, None];
            for (out, length) in part_animations.iter_mut().zip(lengths) {
                *out = animation(
                    take(bytes, &mut pos, usize::from(length))?,
                    part_model.vertices.len(),
                )?;
            }
            *part = Some(BodyPart {
                model: part_model,
                skin_height_hint,
                animations: part_animations,
            });
        }
        let mut sounds: [Vec<u8>; 8] = std::array::from_fn(|_| Vec::new());
        for (out, length) in sounds.iter_mut().zip(sound_lengths) {
            *out = take(bytes, &mut pos, usize::from(length))?.to_vec();
        }
        Ok(Self {
            raw_header,
            model,
            animations,
            parts,
            fall_sound_ids,
            sound_volumes,
            sounds,
            trailing_data: bytes[pos..].to_vec(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model_header(vertices: u16, skin_bytes: u16) -> Vec<u8> {
        let mut model = vec![0; MODEL_HEADER_SIZE];
        model[0x4800..0x4802].copy_from_slice(&vertices.to_le_bytes());
        model[0x4804..0x4806].copy_from_slice(&skin_bytes.to_le_bytes());
        model
    }
    fn fixture() -> Vec<u8> {
        let mut out = vec![0; HEADER_SIZE];
        out[..2].copy_from_slice(&6u16.to_le_bytes());
        out[0x28..0x2a].copy_from_slice(&6u16.to_le_bytes());
        out[0x46..0x48].copy_from_slice(&3u16.to_le_bytes());
        out[0x56..0x58].copy_from_slice(&64u16.to_le_bytes());
        out.extend(model_header(1, 64));
        out.extend([12u8; 64]);
        out.extend([1u8, 0, 2, 0, 3, 0]);
        out.extend(model_header(1, 123));
        out.extend([4u8, 0, 5, 0, 6, 0]);
        out.extend([7u8, 8, 9]);
        out
    }
    #[test]
    fn parses_main_and_detached_animations_and_sound() {
        let data = fixture();
        let model = Monster::parse(&data).unwrap();
        assert_eq!(model.model.skin_height, 1);
        assert_eq!(model.model.skin_indices, vec![12; 64]);
        assert_eq!(
            model.animations[0].as_ref().unwrap().frames[0][0].position,
            [1, 2, 3]
        );
        let part = model.parts[0].as_ref().unwrap();
        assert_eq!(part.skin_height_hint, 123);
        assert!(part.model.skin_indices.is_empty());
        assert_eq!(
            part.animations[0].as_ref().unwrap().frames[0][0].position,
            [4, 5, 6]
        );
        assert_eq!(model.sounds[0], [7, 8, 9]);
        assert_eq!(model.sound_volumes[0], 64);
        assert!(model.trailing_data.is_empty());
    }
    #[test]
    fn preserves_unknown_header_and_trailing_data() {
        let mut data = fixture();
        data[0x34] = 123;
        data.extend([4, 5, 6]);
        let parsed = Monster::parse(&data).unwrap();
        assert_eq!(parsed.raw_header[0x34], 123);
        assert_eq!(parsed.trailing_data, [4, 5, 6]);
    }
    #[test]
    fn rejects_truncation_and_incomplete_vertex_frames() {
        let data = fixture();
        assert!(Monster::parse(&data[..data.len() - 1]).is_err());
        let mut malformed = data.clone();
        malformed[..2].copy_from_slice(&5u16.to_le_bytes());
        assert!(Monster::parse(&malformed).is_err());
        let mut malformed = data;
        malformed[HEADER_SIZE + 0x4804..HEADER_SIZE + 0x4806].copy_from_slice(&63u16.to_le_bytes());
        assert!(Monster::parse(&malformed).is_err());
    }
}
