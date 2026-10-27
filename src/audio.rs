//! Client-side sound effects; simulation emits events without an audio dependency.
//!
//! Jump uses ID 12 from `chasm.inf`. Actual rocket collisions use ID 30 plus
//! the rocket type, following the independent PanzerChasm reconstruction.
//! Unmapped or unavailable samples are silent, as are missing output devices.
//! Mine arming uses ID 72 and detonation ID 40; placement has no mapped sound.
//! This initial playback is non-spatial and ignores authored volume metadata.
use crate::content::ResourceSet;
use rodio::{DeviceSinkBuilder, MixerDeviceSink, Player, Source};
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
        let left = left.trim_start();
        let digit_count = left.bytes().take_while(u8::is_ascii_digit).count();
        if digit_count == 0
            || left
                .as_bytes()
                .get(digit_count)
                .is_some_and(|c| *c != b':' && !c.is_ascii_whitespace())
        {
            continue;
        }
        let Ok(number) = left[..digit_count].parse::<u8>() else {
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

fn music_track(resource: &[u8]) -> Option<u8> {
    String::from_utf8_lossy(resource).lines().find_map(|line| {
        let line = line.trim();
        let (key, value) = line.split_once('=')?;
        if key.trim().eq_ignore_ascii_case("#cdtrack") {
            value.trim().parse::<u8>().ok().filter(|id| *id > 0)
        } else {
            None
        }
    })
}

/// IDs follow the original sound table: jump 12 and impact slots 30–50.
/// Only actual collisions emit impacts; launches, misses and expiration do not.
pub fn gameplay_sound_ids(events: &openchasm::session::SessionEvents, rocket_type: i32) -> Vec<u8> {
    use openchasm::combat::{FireResult, ShotImpact};
    use openchasm::projectiles::ImpactTarget;
    let mut ids = Vec::new();
    if events.vertical.jumped {
        ids.push(12);
    }
    let sound_for = |rocket: usize| {
        u8::try_from(rocket)
            .ok()
            .filter(|&id| id <= 20)
            .map(|id| 30 + id)
    };
    if matches!(
        events.shot,
        Some(FireResult::Fired(
            ShotImpact::Actor { .. } | ShotImpact::Wall { .. } | ShotImpact::Object { .. }
        ))
    ) {
        if let Some(id) = usize::try_from(rocket_type).ok().and_then(sound_for) {
            ids.push(id);
        }
    }
    for impact in &events.projectile_impacts {
        if impact.target != ImpactTarget::Expired {
            if let Some(id) = sound_for(impact.rocket_id) {
                ids.push(id);
            }
        }
    }
    for mine_event in &events.mine_events {
        use openchasm::explosives::MineEvent;
        match mine_event {
            MineEvent::Armed { .. } => ids.push(72),
            MineEvent::Detonated { .. } => ids.push(40),
            MineEvent::Placed(_) | MineEvent::Expired { .. } => {}
        }
    }
    ids
}

pub struct Audio {
    sink: Option<MixerDeviceSink>,
    sounds: BTreeMap<u8, Arc<[u8]>>,
    music: Option<Player>,
}
impl Audio {
    pub fn new(resources: &mut ResourceSet, inf: &[u8]) -> Self {
        let mut sounds = BTreeMap::new();
        for (id, path) in sound_names(inf) {
            if matches!(id, 12 | 21..=28 | 30..=50 | 51..=58 | 60 | 61 | 62 | 63 | 72) {
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
        Self {
            sink,
            sounds,
            music: None,
        }
    }
    /// Start the level's configured music track if available.
    pub fn play_level_music(&mut self, resources: &mut ResourceSet, resource: &[u8]) {
        let Some(sink) = &self.sink else {
            return;
        };
        let Some(id) = music_track(resource) else {
            return;
        };
        let Ok(bytes) = resources.read(&format!("music/track{id:02}.flac")) else {
            return;
        };
        let Ok(source) = rodio::Decoder::try_from(Cursor::new(bytes)) else {
            return;
        };
        let player = Player::connect_new(sink.mixer());
        player.set_volume(0.25);
        player.append(source.repeat_infinite());
        self.music = Some(player);
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
    fn installed_music_flac_decodes_when_available() {
        use std::path::Path;
        let path = Path::new("assets/csm.bin");
        if !path.exists() {
            return;
        }
        let mut archive = crate::archive::Archive::open(path).unwrap();
        let bytes = archive.read("music/track03.flac").unwrap();
        let source = rodio::Decoder::try_from(Cursor::new(bytes)).unwrap();
        assert!(source.take(1024).count() > 0);
    }
    #[test]
    fn reads_music_track_case_insensitively() {
        assert_eq!(music_track(b"#name=Level\n#CDTRACK=3\n"), Some(3));
        assert_eq!(music_track(b"#cdtrack=0"), None);
        assert_eq!(music_track(b"#cdtrack=wrong"), None);
    }
    #[test]
    fn parses_sound_table_and_rejects_unsafe_paths() {
        let names = sound_names(b"[SOUNDS]\n21: GunFire = shoot1.wav\n60: Item = itemup.wav V:128\n61: = ../escape.wav\n62: =\n[SOUNDS_END]\n63: = skip.wav\n");
        assert_eq!(names.get(&21).map(String::as_str), Some("sound/shoot1.wav"));
        assert_eq!(names.get(&60).map(String::as_str), Some("sound/itemup.wav"));
        assert!(!names.contains_key(&61));
        assert!(!names.contains_key(&63));
        let compact = sound_names(b"[SOUNDS]\n30:-hero-disk---= exp_met.wav\n[SOUNDS_END]\n");
        assert_eq!(
            compact.get(&30).map(String::as_str),
            Some("sound/exp_met.wav")
        );
    }
    #[test]
    fn impact_and_jump_sound_selection_excludes_non_collisions() {
        use openchasm::combat::{FireResult, ShotImpact};
        use openchasm::projectiles::{Impact, ImpactTarget, Owner};
        use openchasm::session::SessionEvents;
        let mut events = SessionEvents::default();
        events.vertical.jumped = true;
        events.shot = Some(FireResult::Fired(ShotImpact::Wall {
            wall_index: 0,
            distance: 1.0,
        }));
        let impact = |target, rocket_id| Impact {
            projectile_id: 1,
            owner: Owner::Player,
            rocket_id,
            target,
            position: [0.0, 0.0],
            damage: 1,
            time: 0.0,
        };
        events.projectile_impacts = vec![
            impact(ImpactTarget::Player, 2),
            impact(ImpactTarget::Expired, 3),
            impact(ImpactTarget::Wall { wall_index: 0 }, 21),
        ];
        assert_eq!(gameplay_sound_ids(&events, 1), vec![12, 31, 32]);
        events.vertical.jumped = false;
        events.projectile_impacts.clear();
        events.shot = Some(FireResult::Fired(ShotImpact::Launch { rocket_id: 0 }));
        assert!(gameplay_sound_ids(&events, 1).is_empty());
        events.shot = Some(FireResult::Fired(ShotImpact::Miss));
        assert!(gameplay_sound_ids(&events, 1).is_empty());
        events.shot = Some(FireResult::Fired(ShotImpact::Wall {
            wall_index: 0,
            distance: 1.0,
        }));
        assert!(gameplay_sound_ids(&events, -1).is_empty());
        assert!(gameplay_sound_ids(&events, 21).is_empty());
    }
    #[test]
    fn mine_sounds_follow_committed_lifecycle_only() {
        use openchasm::explosives::{Explosion, Mine, MineEvent};
        use openchasm::projectiles::Owner;
        let mine = Mine {
            id: 7,
            owner: Owner::Player,
            position: [1.0, 1.0, 0.0],
            age_ticks: 0,
            armed: false,
        };
        let explosion = Explosion {
            owner: Owner::Player,
            center: mine.position,
            radius: 2.3,
            damage: 140,
        };
        let mut events = openchasm::session::SessionEvents::default();
        events.mine_events.extend([
            MineEvent::Placed(mine),
            MineEvent::Armed { id: 7 },
            MineEvent::Detonated { id: 7, explosion },
            MineEvent::Expired { id: 8 },
        ]);
        assert_eq!(gameplay_sound_ids(&events, 0), vec![72, 40]);
        events.mine_events.clear();
        assert!(gameplay_sound_ids(&events, 0).is_empty());
    }

    #[test]
    fn installed_jump_and_impact_samples_decode_when_available() {
        let path = std::env::var_os("OPENCHASM_TEST_CONTENT")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from("assets/csm.bin"));
        if !path.exists() && std::env::var_os("OPENCHASM_TEST_CONTENT").is_none() {
            return;
        }
        let mut archive = crate::archive::Archive::open(&path).unwrap();
        let names = sound_names(&archive.read("chasm.inf").unwrap());
        for id in [12, 30, 38, 40, 72] {
            let bytes = archive
                .read(names.get(&id).expect("mapped sample"))
                .unwrap();
            let decoder = rodio::Decoder::try_from(Cursor::new(bytes)).unwrap();
            assert!(decoder.take(1024).count() > 0);
        }
        assert!(!names.contains_key(&33)); // explicitly blank impact slot
    }
}
