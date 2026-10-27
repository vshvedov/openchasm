//! Headless authoritative animation metadata for actor simulation.
use crate::monster::Monster;
use std::io::{self, ErrorKind};

pub const ACTOR_CLIP_SLOTS: usize = 20;
const MAX_FRAME: u32 = 1_000_000;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ActorClip {
    pub first_frame: u32,
    pub frame_count: u32,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClipSource {
    Car,
    Gltf,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActorClipSet {
    pub clips: [Option<ActorClip>; ACTOR_CLIP_SLOTS],
    pub source: ClipSource,
    /// glTF ranges require a model frame count to validate against geometry.
    pub bounds_verified: bool,
}
fn invalid(message: &str) -> io::Error {
    io::Error::new(ErrorKind::InvalidData, message)
}
impl ActorClipSet {
    pub fn from_car(monster: &Monster) -> Self {
        let clips = std::array::from_fn(|slot| {
            monster.animations[slot].as_ref().and_then(|animation| {
                let frame_count = u32::try_from(animation.frames.len()).ok()?;
                (frame_count > 0).then_some(ActorClip {
                    first_frame: 0,
                    frame_count,
                })
            })
        });
        Self {
            clips,
            source: ClipSource::Car,
            bounds_verified: true,
        }
    }
    /// Parse `id first_frame frame_count` rows, optionally validating against
    /// the model's frame count. Callers can use this without glTF dependencies.
    pub fn parse_gltf(bytes: &[u8], model_frame_count: Option<u32>) -> io::Result<Self> {
        let text = std::str::from_utf8(bytes).map_err(|_| invalid("clip table is not UTF-8"))?;
        let mut clips = [None; ACTOR_CLIP_SLOTS];
        for line in text.lines() {
            let line = line.split(';').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let fields: Vec<_> = line.split_whitespace().collect();
            if fields.len() != 3 {
                return Err(invalid("invalid clip row"));
            }
            let id = fields[0]
                .parse::<usize>()
                .map_err(|_| invalid("invalid clip id"))?;
            let first = fields[1]
                .parse::<u32>()
                .map_err(|_| invalid("invalid clip first frame"))?;
            let count = fields[2]
                .parse::<u32>()
                .map_err(|_| invalid("invalid clip frame count"))?;
            let end = first
                .checked_add(count)
                .ok_or_else(|| invalid("clip range overflows"))?;
            if id >= ACTOR_CLIP_SLOTS
                || count == 0
                || end > MAX_FRAME
                || clips[id].is_some()
                || model_frame_count.is_some_and(|limit| end > limit)
            {
                return Err(invalid("invalid clip range or duplicate"));
            }
            clips[id] = Some(ActorClip {
                first_frame: first,
                frame_count: count,
            });
        }
        Ok(Self {
            clips,
            source: ClipSource::Gltf,
            bounds_verified: model_frame_count.is_some(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_unverified_or_bounded_tables() {
        let data = b"0 0 2\n5 2 3 ; shoot\n6 5 1\n";
        let clips = ActorClipSet::parse_gltf(data, None).unwrap();
        assert!(!clips.bounds_verified);
        assert_eq!(
            clips.clips[5],
            Some(ActorClip {
                first_frame: 2,
                frame_count: 3
            })
        );
        assert!(
            ActorClipSet::parse_gltf(data, Some(6))
                .unwrap()
                .bounds_verified
        );
        assert!(ActorClipSet::parse_gltf(data, Some(5)).is_err());
    }
    #[test]
    fn rejects_malformed_and_overflowing_tables() {
        for data in [
            "5 0 0",
            "5 0 1\n5 1 1",
            "20 0 1",
            "5 4294967295 2",
            "5 0 1000001",
            "a b c",
            "0 1",
        ] {
            assert!(
                ActorClipSet::parse_gltf(data.as_bytes(), None).is_err(),
                "{data}"
            );
        }
    }
}
