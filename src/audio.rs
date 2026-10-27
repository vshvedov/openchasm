//! Client-side sound effects; simulation emits events without an audio dependency.
//!
//! Jump uses ID 12 from `chasm.inf`. Actual rocket collisions use ID 30 plus
//! the rocket type, following the independent PanzerChasm reconstruction.
//! Unmapped or unavailable samples are silent, as are missing output devices.
//! Mine arming uses ID 72 and detonation ID 40; placement has no mapped sound.
//! Enemy attack samples use the documented model-name WAV convention and a
//! provisional distance-based volume; other sounds are non-spatial. Authored
//! volume metadata and original spatialization are not yet implemented.
//! Player damage uses the first monster definition's documented pain/death WAVs.
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
    // Stable by authored teleport index, including across checkpoint reloads.
    if let Some(teleport) = events.teleport {
        ids.push(17 + (teleport.index % 3) as u8);
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EnemySound {
    Melee,
    Ranged,
}
impl EnemySound {
    fn slot(self) -> usize {
        match self {
            Self::Melee => 0,
            Self::Ranged => 1,
        }
    }
    fn suffix(self) -> &'static str {
        match self {
            Self::Melee => "attack",
            Self::Ranged => "shoot",
        }
    }
}

fn enemy_sound_path(model_name: &str, kind: EnemySound) -> Option<String> {
    if model_name.is_empty()
        || model_name == "."
        || model_name == ".."
        || model_name.contains(['/', '\\'])
    {
        return None;
    }
    Some(format!("sound/{}_{}.wav", model_name, kind.suffix()))
}

// Returns only events with a resolvable actor, finite listener/source, and an
// audible provisional distance. Source indices are stable placement IDs.
fn enemy_attack_samples(
    attacks: &[openchasm::ai::EnemyAttack],
    actors: &openchasm::actors::ActorState,
    listener: [f32; 2],
) -> Vec<(usize, EnemySound, f32)> {
    use openchasm::ai::EnemyAttackKind;
    if !listener.iter().all(|value| value.is_finite()) {
        return Vec::new();
    }
    let mut result = Vec::new();
    for attack in attacks {
        let Some(actor) = actors
            .actors
            .iter()
            .find(|actor| actor.source_index == attack.source_index)
        else {
            continue;
        };
        if !actor.position.iter().all(|value| value.is_finite()) {
            continue;
        }
        let distance = (actor.position[0] - listener[0]).hypot(actor.position[1] - listener[1]);
        if !distance.is_finite() || distance >= 24.0 {
            continue;
        }
        let volume = (1.0 - distance / 24.0).clamp(0.0, 1.0);
        let sound = match attack.kind {
            EnemyAttackKind::Melee => EnemySound::Melee,
            EnemyAttackKind::Ranged { .. } => EnemySound::Ranged,
        };
        result.push((actor.definition_index, sound, volume));
    }
    result
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlayerSound {
    Pain0,
    Pain1,
    Death,
}
impl PlayerSound {
    fn slot(self) -> usize {
        match self {
            Self::Pain0 => 0,
            Self::Pain1 => 1,
            Self::Death => 2,
        }
    }
    fn suffix(self) -> &'static str {
        match self {
            Self::Pain0 => "pain0",
            Self::Pain1 => "pain1",
            Self::Death => "death",
        }
    }
}

pub struct Audio {
    sink: Option<MixerDeviceSink>,
    sounds: BTreeMap<u8, Arc<[u8]>>,
    enemy_sounds: Vec<[Option<Arc<[u8]>>; 2]>,
    player_sounds: [Option<Arc<[u8]>>; 3],
    music: Option<Player>,
}
impl Audio {
    pub fn new(resources: &mut ResourceSet, inf: &[u8]) -> Self {
        let mut sounds = BTreeMap::new();
        for (id, path) in sound_names(inf) {
            if matches!(id, 12 | 17..=19 | 21..=28 | 30..=50 | 51..=58 | 60 | 61 | 62 | 63 | 72) {
                if let Ok(bytes) = resources.read(&path) {
                    sounds.insert(id, Arc::from(bytes));
                }
            }
        }
        // Resolve case-insensitively while retaining archive spelling, which
        // matters on Linux and in user-authored loose resource trees.
        let paths: BTreeMap<String, String> = resources
            .names()
            .filter(|name| name.starts_with("sound/"))
            .map(|name| (name.to_ascii_lowercase(), name.to_owned()))
            .collect();
        let mut enemy_sounds = Vec::new();
        let mut player_sounds: [Option<Arc<[u8]>>; 3] = [None, None, None];
        let mut loaded_bytes = 0usize;
        if let Ok(definitions) = openchasm::actors::ActorDefinitions::parse(inf) {
            if let Some(hero) = definitions.entries.first() {
                for kind in [PlayerSound::Pain0, PlayerSound::Pain1, PlayerSound::Death] {
                    let name = format!("sound/{}_{}.wav", hero.model_name, kind.suffix());
                    if let Some(path) = paths.get(&name.to_ascii_lowercase()) {
                        if let Ok(bytes) = resources.read(path) {
                            if bytes.len() <= 2 * 1024 * 1024 {
                                player_sounds[kind.slot()] = Some(Arc::from(bytes));
                            }
                        }
                    }
                }
            }
            for definition in definitions.entries {
                let mut slots: [Option<Arc<[u8]>>; 2] = [None, None];
                for kind in [EnemySound::Melee, EnemySound::Ranged] {
                    let Some(path) = enemy_sound_path(&definition.model_name, kind)
                        .and_then(|name| paths.get(&name.to_ascii_lowercase()))
                    else {
                        continue;
                    };
                    if loaded_bytes >= 32 * 1024 * 1024 {
                        continue;
                    }
                    if let Ok(bytes) = resources.read(path) {
                        if bytes.len() <= 2 * 1024 * 1024
                            && loaded_bytes + bytes.len() <= 32 * 1024 * 1024
                        {
                            loaded_bytes += bytes.len();
                            slots[kind.slot()] = Some(Arc::from(bytes));
                        }
                    }
                }
                enemy_sounds.push(slots);
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
            enemy_sounds,
            player_sounds,
            music: None,
        }
    }
    /// Play one committed player pain/death cue. A missing pain variant falls
    /// back to the other; unavailable audio or missing files are silent.
    pub fn play_player_sound(&self, kind: PlayerSound) {
        let Some(sink) = &self.sink else {
            return;
        };
        let sample = self.player_sounds[kind.slot()]
            .as_ref()
            .or_else(|| match kind {
                PlayerSound::Pain0 => self.player_sounds[PlayerSound::Pain1.slot()].as_ref(),
                PlayerSound::Pain1 => self.player_sounds[PlayerSound::Pain0.slot()].as_ref(),
                PlayerSound::Death => None,
            });
        if let Some(bytes) = sample {
            if let Ok(player) = rodio::play(sink.mixer(), Cursor::new(Arc::clone(bytes))) {
                player.detach();
            }
        }
    }

    /// Play committed attacks using authored model-name samples. Missing
    /// actors, files, decoders and audio output are silently ignored.
    pub fn play_enemy_attacks(
        &self,
        attacks: &[openchasm::ai::EnemyAttack],
        actors: &openchasm::actors::ActorState,
        listener: [f32; 2],
    ) {
        let Some(sink) = &self.sink else {
            return;
        };
        for (index, kind, volume) in enemy_attack_samples(attacks, actors, listener) {
            let Some(bytes) = self
                .enemy_sounds
                .get(index)
                .and_then(|slots| slots[kind.slot()].as_ref())
            else {
                continue;
            };
            if let Ok(player) = rodio::play(sink.mixer(), Cursor::new(Arc::clone(bytes))) {
                player.set_volume(volume);
                player.detach();
            }
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
        for id in [12, 17, 18, 19, 30, 38, 40, 72] {
            let bytes = archive
                .read(names.get(&id).expect("mapped sample"))
                .unwrap();
            let decoder = rodio::Decoder::try_from(Cursor::new(bytes)).unwrap();
            assert!(decoder.take(1024).count() > 0);
        }
        assert!(!names.contains_key(&33)); // explicitly blank impact slot
    }
    #[test]
    fn enemy_attack_selection_uses_stable_actor_id_kind_and_distance() {
        use openchasm::actors::{ActorDefinitions, ActorState};
        use openchasm::ai::{EnemyAttack, EnemyAttackKind};
        let defs = ActorDefinitions::parse(
            b"[MONSTERS]\n2\nhero.car 1 1 1 1 1 1 1 1\nan4.car 1 1 1 1 1 1 1 1\n",
        )
        .unwrap();
        let actor = openchasm::map::Monster {
            position: [256, 0],
            id: 101,
            angle: 0,
            flags: 7,
        };
        let actors = ActorState::from_placements(&[actor], &defs);
        let attacks = [
            EnemyAttack {
                source_index: 0,
                damage: 1,
                kind: EnemyAttackKind::Melee,
            },
            EnemyAttack {
                source_index: 0,
                damage: 1,
                kind: EnemyAttackKind::Ranged {
                    rocket_id: 1,
                    instant: false,
                    origin: [1.0, 0.0],
                    target: [0.0, 0.0],
                },
            },
            EnemyAttack {
                source_index: 7,
                damage: 1,
                kind: EnemyAttackKind::Melee,
            },
        ];
        let samples = enemy_attack_samples(&attacks, &actors, [0.0, 0.0]);
        assert_eq!(samples.len(), 2);
        assert_eq!((samples[0].0, samples[0].1), (1, EnemySound::Melee));
        assert_eq!((samples[1].0, samples[1].1), (1, EnemySound::Ranged));
        assert!((samples[0].2 - 23.0 / 24.0).abs() < 0.00001);
        assert!(enemy_attack_samples(&attacks, &actors, [30.0, 0.0]).is_empty());
        assert!(enemy_attack_samples(&attacks, &actors, [f32::NAN, 0.0]).is_empty());
        assert_eq!(
            enemy_sound_path("SFAG.car", EnemySound::Melee).as_deref(),
            Some("sound/SFAG.car_attack.wav")
        );
        assert_eq!(
            enemy_sound_path("faust", EnemySound::Ranged).as_deref(),
            Some("sound/faust_shoot.wav")
        );
        assert!(enemy_sound_path("../bad", EnemySound::Melee).is_none());
    }

    #[test]
    fn installed_enemy_attack_samples_decode_when_available() {
        let explicit = std::env::var_os("OPENCHASM_TEST_CONTENT");
        let path = explicit
            .as_ref()
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from("assets/csm.bin"));
        if !path.exists() && explicit.is_none() {
            return;
        }
        let mut resources = ResourceSet::open(&path).unwrap();
        for (model, kind) in [
            ("an4.car", EnemySound::Melee),
            ("faust", EnemySound::Ranged),
            ("SFAG.car", EnemySound::Melee),
        ] {
            let bytes = resources
                .read(&enemy_sound_path(model, kind).unwrap())
                .unwrap();
            let decoder = rodio::Decoder::try_from(Cursor::new(bytes)).unwrap();
            assert!(decoder.take(1024).count() > 0);
        }
    }
    #[test]
    fn teleport_sound_selection_is_deterministic_and_only_on_success() {
        use openchasm::teleport::{TeleportEvent, TeleportFailure};
        let mut events = openchasm::session::SessionEvents {
            teleport_failure: Some(TeleportFailure::DestinationBlocked { index: 1 }),
            ..Default::default()
        };
        assert!(gameplay_sound_ids(&events, 0).is_empty());
        events.teleport_failure = None;
        for index in 0..6 {
            events.teleport = Some(TeleportEvent {
                index,
                source: [0.0, 0.0],
                destination: [1.0, 1.0],
                angle_radians: 0.0,
            });
            assert_eq!(gameplay_sound_ids(&events, 0), vec![17 + (index % 3) as u8]);
        }
    }
}
