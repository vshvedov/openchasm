//! Definition-backed actor placements, independent of the renderer or clock.
//!
//! No AI, difficulty filtering or damage rules are implemented. Raw map data
//! and unknown definition columns are preserved for later simulation work.
use crate::map::{Monster as MapActor, UNITS_PER_TILE};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, ErrorKind};

/// How authored map actor IDs resolve into a definition table.
/// The default preserves the normal ID 100+n -> definition n convention.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ActorMapping {
    #[default]
    Identity,
    /// Original addon IDs interpreted with the remaster's merged table.
    AddonToRemaster,
}

impl ActorMapping {
    /// Recognize the three installed remaster addon maps and their exact merged
    /// table. Modified or independently authored data remains identity; callers
    /// may explicitly select `AddonToRemaster` for compatible custom content.
    pub fn recognize_shipped_addon(map: &[u8], inf: &[u8]) -> Self {
        const INF: &str = "89c16e848597ae0432bc2875b2a377153fb56a5d7e403f959b71445558bd0198";
        const MAPS: [&str; 3] = [
            "d5f42f358b06b7a59da0970db1b056953ad9110d6bf77acfc238e01b71c09e71",
            "831ad13955ca4413aa03ff6da925852d270e2578fa44e62858b66f3731f0416c",
            "436ca88bf5b7577b46f427a3d6217b43aea1ea65460a4bf54e21852695663bff",
        ];
        let hash = |data: &[u8]| format!("{:x}", Sha256::digest(data));
        if hash(inf) == INF && MAPS.contains(&hash(map).as_str()) {
            Self::AddonToRemaster
        } else {
            Self::Identity
        }
    }

    pub fn definition_index(self, map_id: u16) -> Option<usize> {
        let index = usize::from(map_id.checked_sub(100)?);
        Some(match (self, index) {
            (Self::AddonToRemaster, 16..=18) => index + 5,
            _ => index,
        })
    }
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(ErrorKind::InvalidData, message.into())
}

/// One row in the `[MONSTERS]` table. Numeric fields are preserved in the
/// original units and semantics; negative attack IDs may have special meaning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActorDefinition {
    pub model_name: String,
    pub wall_radius: i32,
    pub actor_radius: i32,
    pub speed: i32,
    pub rotation_speed: i32,
    pub health: i32,
    pub kick: i32,
    pub rocket: i32,
    pub separation_limit: i32,
    /// Unrecognized columns, retained verbatim.
    pub extra_fields: Vec<String>,
    /// Original row, including any semicolon comment.
    pub raw_line: String,
}
impl ActorDefinition {
    /// A CAR path only when the table explicitly gives a CAR filename.
    /// Extensionless entries can identify modern replacement assets instead.
    pub fn car_path(&self) -> Option<String> {
        self.model_name
            .to_ascii_lowercase()
            .ends_with(".car")
            .then(|| format!("monster/{}", self.model_name.to_ascii_lowercase()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActorDefinitions {
    pub entries: Vec<ActorDefinition>,
}
impl ActorDefinitions {
    pub fn parse(inf: &[u8]) -> io::Result<Self> {
        let text = std::str::from_utf8(inf).map_err(|_| invalid("chasm.inf is not UTF-8"))?;
        let mut lines = text.lines().enumerate();
        if !lines
            .by_ref()
            .any(|(_, line)| line.trim().eq_ignore_ascii_case("[MONSTERS]"))
        {
            return Err(invalid("missing [MONSTERS] section"));
        }
        let (_, count_line) = lines
            .by_ref()
            .find(|(_, line)| {
                let value = line.split(';').next().unwrap_or("").trim();
                !value.is_empty()
            })
            .ok_or_else(|| invalid("missing monster count"))?;
        let count: usize = count_line
            .split_whitespace()
            .next()
            .ok_or_else(|| invalid("missing monster count"))?
            .parse()
            .map_err(|_| invalid("invalid monster count"))?;
        if count > u16::MAX as usize - 100 {
            return Err(invalid("monster count exceeds map ID range"));
        }
        let mut entries = Vec::with_capacity(count);
        for (index, raw) in lines {
            if entries.len() == count {
                break;
            }
            let line = raw.split(';').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            if line.starts_with('[') || line.starts_with('*') {
                break;
            }
            let fields: Vec<_> = line.split_whitespace().collect();
            if fields.len() < 9 {
                return Err(invalid(format!(
                    "monster definition line {} has fewer than 9 fields",
                    index + 1
                )));
            }
            let name = fields[0];
            if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\\']) {
                return Err(invalid(format!(
                    "invalid monster resource name on line {}",
                    index + 1
                )));
            }
            let mut numbers = [0i32; 8];
            for (i, value) in numbers.iter_mut().enumerate() {
                *value = fields[i + 1].parse().map_err(|_| {
                    invalid(format!(
                        "invalid monster numeric field on line {}",
                        index + 1
                    ))
                })?;
            }
            entries.push(ActorDefinition {
                model_name: name.to_owned(),
                wall_radius: numbers[0],
                actor_radius: numbers[1],
                speed: numbers[2],
                rotation_speed: numbers[3],
                health: numbers[4],
                kick: numbers[5],
                rocket: numbers[6],
                separation_limit: numbers[7],
                extra_fields: fields[9..].iter().map(|x| (*x).to_owned()).collect(),
                raw_line: raw.to_owned(),
            });
        }
        if entries.len() != count {
            return Err(invalid("[MONSTERS] ended before declared count"));
        }
        Ok(Self { entries })
    }
    /// Map IDs 100+n correspond to definition n. ID 100 is used by player
    /// spawn records and must not itself be instantiated as a monster.
    pub fn for_map_id(&self, map_id: u16) -> Option<&ActorDefinition> {
        self.entries.get(usize::from(map_id.checked_sub(100)?))
    }
}

/// The source index is a stable per-map identifier while the map is loaded.
/// It identifies a placement, not a future network protocol entity ID.
#[derive(Debug, Clone, PartialEq)]
pub struct Actor {
    pub source_index: usize,
    pub definition_index: usize,
    pub placement: MapActor,
    pub position: [f32; 2],
    pub angle_radians: f32,
    pub health: i32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct UnresolvedActor {
    pub source_index: usize,
    pub placement: MapActor,
}

/// Each source placement is represented as a spawn, a known actor, or an
/// unresolved record; no entry is silently discarded or difficulty-filtered.
#[derive(Debug, Clone, PartialEq)]
pub struct ActorState {
    pub player_spawns: Vec<(usize, MapActor)>,
    pub actors: Vec<Actor>,
    pub unresolved: Vec<UnresolvedActor>,
}
/// Mutable actor values keyed by authored placement identity. Definition and
/// map placement data remain sourced from the content on restore.
#[derive(Debug, Clone, PartialEq)]
pub struct ActorSnapshot {
    pub source_index: u32,
    pub authored_id: u16,
    pub definition_index: u32,
    pub position: [f32; 2],
    pub angle_radians: f32,
    pub health: i32,
}
#[derive(Debug, Clone, PartialEq)]
pub struct ActorStateSnapshot {
    pub actors: Vec<ActorSnapshot>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActorSnapshotError {
    IndexOutOfRange,
    CountMismatch,
    DuplicateSource,
    MissingSource,
    IdentityMismatch,
    InvalidPosition,
    InvalidHealth,
    MissingDefinition,
}
impl ActorState {
    pub fn snapshot(&self) -> Result<ActorStateSnapshot, ActorSnapshotError> {
        let mut seen = BTreeSet::new();
        let mut actors = Vec::with_capacity(self.actors.len());
        for actor in &self.actors {
            let source_index = u32::try_from(actor.source_index)
                .map_err(|_| ActorSnapshotError::IndexOutOfRange)?;
            if !seen.insert(source_index) {
                return Err(ActorSnapshotError::DuplicateSource);
            }
            let definition_index = u32::try_from(actor.definition_index)
                .map_err(|_| ActorSnapshotError::IndexOutOfRange)?;
            actors.push(ActorSnapshot {
                source_index,
                authored_id: actor.placement.id,
                definition_index,
                position: actor.position,
                angle_radians: actor.angle_radians,
                health: actor.health,
            });
        }
        Ok(ActorStateSnapshot { actors })
    }
    /// Validate the entire actor set against its reconstructed definitions
    /// before mutating any actor. Disabled actors may legitimately have zero HP.
    pub fn restore(
        &mut self,
        snapshot: &ActorStateSnapshot,
        definitions: &ActorDefinitions,
    ) -> Result<(), ActorSnapshotError> {
        if snapshot.actors.len() != self.actors.len() {
            return Err(ActorSnapshotError::CountMismatch);
        }
        let mut saved = BTreeMap::new();
        for actor in &snapshot.actors {
            if saved.insert(actor.source_index, actor).is_some() {
                return Err(ActorSnapshotError::DuplicateSource);
            }
            if !actor.angle_radians.is_finite()
                || !actor
                    .position
                    .iter()
                    .all(|v| v.is_finite() && v.abs() <= 4096.0)
            {
                return Err(ActorSnapshotError::InvalidPosition);
            }
        }
        let mut seen = BTreeSet::new();
        for actor in &self.actors {
            let id = u32::try_from(actor.source_index)
                .map_err(|_| ActorSnapshotError::IndexOutOfRange)?;
            if !seen.insert(id) {
                return Err(ActorSnapshotError::DuplicateSource);
            }
            let saved = saved.get(&id).ok_or(ActorSnapshotError::MissingSource)?;
            if saved.authored_id != actor.placement.id
                || usize::try_from(saved.definition_index).ok() != Some(actor.definition_index)
            {
                return Err(ActorSnapshotError::IdentityMismatch);
            }
            let def = definitions
                .entries
                .get(actor.definition_index)
                .ok_or(ActorSnapshotError::MissingDefinition)?;
            if saved.health < 0 || saved.health > def.health.max(0) {
                return Err(ActorSnapshotError::InvalidHealth);
            }
        }
        for actor in &mut self.actors {
            let value = saved[&(actor.source_index as u32)];
            actor.position = value.position;
            actor.angle_radians = value.angle_radians;
            actor.health = value.health;
        }
        Ok(())
    }
    pub fn from_placements(placements: &[MapActor], definitions: &ActorDefinitions) -> Self {
        Self::from_placements_with_mapping(placements, definitions, ActorMapping::Identity)
    }

    /// Resolve definitions in the given context while retaining each original
    /// placement and its authored ID, flags, coordinates, and source index.
    pub fn from_placements_with_mapping(
        placements: &[MapActor],
        definitions: &ActorDefinitions,
        mapping: ActorMapping,
    ) -> Self {
        let mut result = Self {
            player_spawns: Vec::new(),
            actors: Vec::new(),
            unresolved: Vec::new(),
        };
        for (source_index, &placement) in placements.iter().enumerate() {
            if placement.is_player_spawn() {
                result.player_spawns.push((source_index, placement));
            } else if let Some((definition_index, definition)) = mapping
                .definition_index(placement.id)
                .and_then(|index| definitions.entries.get(index).map(|def| (index, def)))
            {
                result.actors.push(Actor {
                    source_index,
                    definition_index,
                    placement,
                    position: [
                        f32::from(placement.position[0]) / UNITS_PER_TILE,
                        f32::from(placement.position[1]) / UNITS_PER_TILE,
                    ],
                    angle_radians: placement.direction_radians(),
                    health: definition.health,
                });
            } else {
                result.unresolved.push(UnresolvedActor {
                    source_index,
                    placement,
                });
            }
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const TABLE: &str = "[MONSTERS]\n2 labels\nhero.car 100 100 10 5 100 0 0 0\njoker.car 52 190 10 5 100 20 -1 16 extra ; comment\n[OTHER]\n";
    #[test]
    fn actor_snapshot_uses_source_identity_and_restores_atomically() {
        let defs = ActorDefinitions::parse(TABLE.as_bytes()).unwrap();
        let placement = |id, x| MapActor {
            position: [x, 128],
            id,
            angle: 0,
            flags: 7,
        };
        let placements = [placement(101, 128), placement(101, 256)];
        let mut source = ActorState::from_placements(&placements, &defs);
        source.actors[0].position = [3.0, 4.0];
        source.actors[1].health = 0;
        let mut snapshot = source.snapshot().unwrap();
        snapshot.actors.reverse();
        let mut restored = ActorState::from_placements(&placements, &defs);
        restored.restore(&snapshot, &defs).unwrap();
        assert_eq!(restored, source);
        let before = restored.clone();
        snapshot.actors[0].authored_id = 118;
        assert_eq!(
            restored.restore(&snapshot, &defs),
            Err(ActorSnapshotError::IdentityMismatch)
        );
        assert_eq!(restored, before);
        snapshot = source.snapshot().unwrap();
        snapshot.actors[0].health = 101;
        assert_eq!(
            restored.restore(&snapshot, &defs),
            Err(ActorSnapshotError::InvalidHealth)
        );
        assert_eq!(restored, before);
        snapshot = source.snapshot().unwrap();
        snapshot.actors[1].position[0] = f32::NAN;
        assert_eq!(
            restored.restore(&snapshot, &defs),
            Err(ActorSnapshotError::InvalidPosition)
        );
        assert_eq!(restored, before);
    }
    #[test]
    fn addon_mapping_is_explicit_and_retains_raw_placements() {
        let mut defs = ActorDefinitions::parse(TABLE.as_bytes()).unwrap();
        let template = defs.entries[1].clone();
        defs.entries.resize(24, template);
        defs.entries[23].health = 73;
        let placement = MapActor {
            position: [128, 256],
            id: 118,
            angle: 3,
            flags: 4,
        };
        let identity = ActorState::from_placements(&[placement], &defs);
        let addon = ActorState::from_placements_with_mapping(
            &[placement],
            &defs,
            ActorMapping::AddonToRemaster,
        );
        assert_eq!(identity.actors[0].definition_index, 18);
        assert_eq!(addon.actors[0].definition_index, 23);
        assert_eq!(addon.actors[0].health, 73);
        assert_eq!(addon.actors[0].placement, placement);
        assert_eq!(
            ActorMapping::AddonToRemaster.definition_index(119),
            Some(19)
        );
        assert_eq!(ActorMapping::AddonToRemaster.definition_index(99), None);
        defs.entries.truncate(23);
        let unresolved = ActorState::from_placements_with_mapping(
            &[placement],
            &defs,
            ActorMapping::AddonToRemaster,
        );
        assert!(unresolved.actors.is_empty());
        assert_eq!(unresolved.unresolved[0].placement, placement);
    }
    #[test]
    fn preserves_definitions_and_source_fields() {
        let defs = ActorDefinitions::parse(TABLE.as_bytes()).unwrap();
        let joker = &defs.entries[1];
        assert_eq!(joker.rocket, -1);
        assert_eq!(joker.extra_fields, ["extra"]);
        assert!(joker.raw_line.ends_with("; comment"));
        assert_eq!(joker.car_path().as_deref(), Some("monster/joker.car"));
        assert_eq!(defs.for_map_id(101), Some(joker));
        assert!(defs.for_map_id(99).is_none());
    }
    #[test]
    fn represents_spawns_actors_and_unknowns_without_filtering() {
        let defs = ActorDefinitions::parse(TABLE.as_bytes()).unwrap();
        let placements = [
            MapActor {
                position: [256, 512],
                id: 100,
                angle: 0,
                flags: 0,
            },
            MapActor {
                position: [128, 640],
                id: 101,
                angle: 2,
                flags: 7,
            },
            MapActor {
                position: [0, 0],
                id: 120,
                angle: 0,
                flags: 3,
            },
        ];
        let state = ActorState::from_placements(&placements, &defs);
        assert_eq!(state.player_spawns.len(), 1);
        assert_eq!(state.actors.len(), 1);
        assert_eq!(state.actors[0].source_index, 1);
        assert_eq!(state.actors[0].position, [0.5, 2.5]);
        assert_eq!(state.actors[0].placement.flags, 7);
        assert_eq!(state.actors[0].health, 100);
        assert_eq!(state.unresolved[0].source_index, 2);
    }
    #[test]
    fn rejects_malformed_or_incomplete_tables() {
        assert!(ActorDefinitions::parse(b"[MONSTERS]\n1\nbad.car 1 2\n").is_err());
        assert!(ActorDefinitions::parse(b"[MONSTERS]\n1\n../bad.car 1 2 3 4 5 6 7 8\n").is_err());
        assert!(
            ActorDefinitions::parse(b"[MONSTERS]\n2\nhero 1 2 3 4 5 6 7 8\n[OTHER]\n").is_err()
        );
        assert!(ActorDefinitions::parse(b"missing").is_err());
    }
    #[test]
    fn does_not_guess_extensionless_car_names() {
        let defs = ActorDefinitions::parse(b"[MONSTERS]\n1\nfaust 1 2 3 4 5 6 7 8\n").unwrap();
        assert_eq!(defs.entries[0].car_path(), None);
    }
}
