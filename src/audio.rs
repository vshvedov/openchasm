//! Client-side sound effects; simulation emits events without an audio dependency.
use crate::content::ResourceSet;
use rodio::{DeviceSinkBuilder, MixerDeviceSink};
use std::collections::BTreeMap;
use std::io::Cursor;
use std::sync::Arc;

/// Extract sound-ID names from the game's data table. Ignore malformed or
/// unsafe paths; an absent sound must not prevent the level from running.
fn sound_names(inf: &[u8]) -> BTreeMap<u8, String> {
    let text = String::from_utf8_lossy(inf);
    let mut in_section = false;
    let mut names = BTreeMap::new();
    for line in text.lines().map(str::trim) {
        if line.eq_ignore_ascii_case("[sounds]") {
            in_section = true;
            continue;
        }
        if in_section && line.starts_with('[') {
            break;
        }
        if !in_section {
            continue;
        }
        let Some((left, right)) = line.split_once('=') else {
            continue;
        };
        let Some(number) = left
            .split_whitespace()
            .next()
            .and_then(|n| n.trim_end_matches(':').parse::<u8>().ok())
        else {
            continue;
        };
        let Some(name) = right.split_whitespace().next() else {
            continue;
        };
        if name.contains(['/', '\\']) || !name.to_ascii_lowercase().ends_with(".wav") {
            continue;
        }
        names.insert(number, format!("sound/{name}"));
    }
    names
}

pub struct Audio {
    sink: Option<MixerDeviceSink>,
    sounds: BTreeMap<u8, Arc<[u8]>>,
}
impl Audio {
    pub fn new(resources: &mut ResourceSet, inf: &[u8]) -> Self {
        let mut sounds = BTreeMap::new();
        for (id, path) in sound_names(inf) {
            if matches!(id, 21..=28 | 51..=58 | 60 | 62 | 63) {
                if let Ok(bytes) = resources.read(&path) {
                    sounds.insert(id, Arc::from(bytes));
                }
            }
        }
        let sink = match DeviceSinkBuilder::open_default_sink() {
            Ok(sink) => Some(sink),
            Err(error) => {
                eprintln!("audio unavailable: {error}");
                None
            }
        };
        Self { sink, sounds }
    }
    /// Start a detached one-shot sample if output and the requested asset exist.
    pub fn play(&self, id: u8) {
        let (Some(sink), Some(bytes)) = (&self.sink, self.sounds.get(&id)) else {
            return;
        };
        if let Ok(player) = rodio::play(sink.mixer(), Cursor::new(Arc::clone(bytes))) {
            player.detach();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn installed_pickup_wav_decodes_when_game_data_is_available() {
        use rodio::Source;
        use std::path::Path;
        let path = Path::new("assets/csm.bin");
        if !path.exists() {
            return;
        }
        let mut archive = crate::archive::Archive::open(path).unwrap();
        let bytes = archive.read("sound/itemup.wav").unwrap();
        let source = rodio::Decoder::try_from(Cursor::new(bytes)).unwrap();
        assert!(source.sample_rate().get() > 0 && source.channels().get() > 0);
        assert!(source.take(1024).count() > 0);
    }
    #[test]
    fn parses_sound_table_and_rejects_unsafe_paths() {
        let names = sound_names(b"[SOUNDS]\n21: GunFire = shoot1.wav\n60: Item = itemup.wav V:128\n61: = ../escape.wav\n62: =\n[SOUNDS_END]\n63: = skip.wav\n");
        assert_eq!(names.get(&21).map(String::as_str), Some("sound/shoot1.wav"));
        assert_eq!(names.get(&60).map(String::as_str), Some("sound/itemup.wav"));
        assert!(!names.contains_key(&61));
        assert!(!names.contains_key(&63));
    }
}
