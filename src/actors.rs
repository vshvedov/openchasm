//! Definition-backed actor placements, independent of the renderer or clock.
//!
//! No AI, difficulty filtering or damage rules are implemented. Raw map data
//! and unknown definition columns are preserved for later simulation work.
use crate::map::{Monster as MapActor, UNITS_PER_TILE};
use std::io::{self, ErrorKind};

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
impl ActorState {
    pub fn from_placements(placements: &[MapActor], definitions: &ActorDefinitions) -> Self {
        let mut result = Self {
            player_spawns: Vec::new(),
            actors: Vec::new(),
            unresolved: Vec::new(),
        };
        for (source_index, &placement) in placements.iter().enumerate() {
            if placement.is_player_spawn() {
                result.player_spawns.push((source_index, placement));
            } else if let Some(definition) = definitions.for_map_id(placement.id) {
                let definition_index = usize::from(placement.id - 100);
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
