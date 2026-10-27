//! Map object placements and their data-driven definitions.
//!
//! The original game has two independent definition tables. Grid IDs 131–162
//! select an item from `chasm.inf`, and IDs 163–226 select a level-specific
//! object from the map resource's `#newobjects` section. Values that control
//! game behavior are preserved here; simulating them belongs to game rules.
use crate::map::{Map, MAP_SIDE, UNITS_PER_TILE};
use std::io::{self, ErrorKind};
use std::path::Path;

const FIRST_ITEM: u8 = 131;
const FIRST_OBJECT: u8 = 163;
const LAST_OBJECT: u8 = 226;

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(ErrorKind::InvalidData, message.into())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectKind {
    Item,
    LevelObject,
}

/// One row from `[3D_OBJECTS]` or `#newobjects`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectDefinition {
    /// Raw map units; the exact shape is controlled by the low bit.
    pub collision_radius: i32,
    pub shadow: i32,
    pub bmp_object: i32,
    pub bmp_z: i32,
    pub action_code: i32,
    pub blow_up: i32,
    pub break_limit: i32,
    pub ambient_sound: i32,
    pub break_sound: i32,
    /// Name as stored in the definition, with no inferred file extension.
    pub model_name: String,
    /// Animation marker `%`, when present, is retained for the caller.
    pub animation_name: Option<String>,
}

impl ObjectDefinition {
    /// Return the original 3O path when the definition explicitly names one.
    /// Some modern definitions name only a glTF model and have no 3O.
    pub fn model_3o_path(&self) -> Option<String> {
        if self.model_name.to_ascii_lowercase().ends_with(".3o") {
            Some(format!("model/{}", self.model_name.to_ascii_lowercase()))
        } else {
            None
        }
    }

    /// Path convention used for modern replacement assets in the Steam data.
    pub fn model_gltf_path(&self) -> String {
        let name = self.model_name.to_ascii_lowercase();
        let stem = name.strip_suffix(".3o").unwrap_or(&name);
        format!("model/{stem}.gltf")
    }

    /// Suggested animation resource path; `%` is a behavior marker, not part
    /// of the filename. No playback behavior is inferred by this parser.
    pub fn animation_path(&self) -> Option<String> {
        self.animation_name.as_ref().map(|name| {
            format!(
                "model/{}",
                name.trim_start_matches('%').to_ascii_lowercase()
            )
        })
    }
}

/// Original placement data, plus map-space position in tiles.
#[derive(Debug, Clone, PartialEq)]
pub struct ObjectInstance {
    pub kind: ObjectKind,
    pub definition_index: usize,
    pub grid_cell: [usize; 2],
    pub position: [f32; 2],
    /// The three low flag bits select an eighth-turn direction, with the map's
    /// half-turn correction. The high flag bits are retained in `flags`.
    pub angle_radians: f32,
    pub flags: u8,
    /// The first field of the second coordinate pair, whose low bits encode
    /// difficulty/game mode in the existing research; preserve it verbatim.
    pub difficulty_flags: u16,
    pub extra: u16,
    pub size: u8,
}

#[derive(Debug, Clone)]
pub struct ObjectDefinitions {
    pub items: Vec<ObjectDefinition>,
    pub level_objects: Vec<ObjectDefinition>,
}

fn parse_definition(line: &str, line_number: usize) -> io::Result<ObjectDefinition> {
    let words: Vec<_> = line
        .split(';')
        .next()
        .unwrap_or("")
        .split_whitespace()
        .collect();
    if words.len() < 10 {
        return Err(invalid(format!(
            "object definition line {line_number} has fewer than 10 fields"
        )));
    }
    let mut values = [0i32; 9];
    for (i, value) in values.iter_mut().enumerate() {
        *value = words[i].parse().map_err(|_| {
            invalid(format!(
                "invalid numeric field on object definition line {line_number}"
            ))
        })?;
    }
    // Referenced names are simple filenames. Reject separators and traversal so
    // callers cannot accidentally resolve names outside the resource namespace.
    let validate_name = |name: &str| {
        let path = Path::new(name);
        !name.is_empty()
            && !name.contains(['/', '\\'])
            && name != "."
            && name != ".."
            && path.components().count() == 1
    };
    if !validate_name(words[9])
        || words
            .get(10)
            .is_some_and(|name| !validate_name(name.trim_start_matches('%')))
    {
        return Err(invalid(format!(
            "invalid resource filename on object definition line {line_number}"
        )));
    }
    Ok(ObjectDefinition {
        collision_radius: values[0],
        shadow: values[1],
        bmp_object: values[2],
        bmp_z: values[3],
        action_code: values[4],
        blow_up: values[5],
        break_limit: values[6],
        ambient_sound: values[7],
        break_sound: values[8],
        model_name: words[9].to_owned(),
        animation_name: words.get(10).map(|name| (*name).to_owned()),
    })
}

impl ObjectDefinitions {
    pub fn parse(inf: &[u8], resource: &[u8]) -> io::Result<Self> {
        let inf = std::str::from_utf8(inf).map_err(|_| invalid("chasm.inf is not UTF-8"))?;
        let resource =
            std::str::from_utf8(resource).map_err(|_| invalid("map resource is not UTF-8"))?;
        let mut lines = inf.lines().enumerate();
        if !lines
            .by_ref()
            .any(|(_, line)| line.trim().eq_ignore_ascii_case("[3D_OBJECTS]"))
        {
            return Err(invalid("missing [3D_OBJECTS] section"));
        }
        let (_, count_line) = lines
            .by_ref()
            .find(|(_, line)| {
                let line = line.trim();
                !line.is_empty() && !line.starts_with(';')
            })
            .ok_or_else(|| invalid("missing [3D_OBJECTS] count"))?;
        let count: usize = count_line
            .split_whitespace()
            .next()
            .ok_or_else(|| invalid("missing object count"))?
            .parse()
            .map_err(|_| invalid("invalid object count"))?;
        if count > usize::from(FIRST_OBJECT - FIRST_ITEM) {
            return Err(invalid("item count exceeds the map's item ID range"));
        }
        let mut items = Vec::with_capacity(count);
        for (line_number, line) in lines {
            if items.len() == count {
                break;
            }
            let line = line.trim();
            if line.is_empty() || line.starts_with(';') {
                continue;
            }
            if line.starts_with('[') || line.starts_with('*') {
                break;
            }
            items.push(parse_definition(line, line_number + 1)?);
            if items.len() == count {
                break;
            }
        }
        if items.len() != count {
            return Err(invalid("[3D_OBJECTS] ended before declared item count"));
        }

        let mut level_objects = Vec::new();
        let mut in_section = false;
        let mut closed = false;
        for (line_number, line) in resource.lines().enumerate() {
            let line = line.trim();
            if !in_section {
                if line.eq_ignore_ascii_case("#newobjects") {
                    in_section = true;
                }
                continue;
            }
            if line.eq_ignore_ascii_case("#end") || line.eq_ignore_ascii_case("#end.") {
                closed = true;
                break;
            }
            if line.is_empty() || line.starts_with(';') {
                continue;
            }
            if level_objects.len() == usize::from(LAST_OBJECT - FIRST_OBJECT) + 1 {
                return Err(invalid(
                    "level object count exceeds the map's object ID range",
                ));
            }
            level_objects.push(parse_definition(line, line_number + 1)?);
        }
        if !in_section || !closed {
            return Err(invalid("missing or unterminated #newobjects section"));
        }
        Ok(Self {
            items,
            level_objects,
        })
    }

    pub fn definition(&self, instance: &ObjectInstance) -> Option<&ObjectDefinition> {
        match instance.kind {
            ObjectKind::Item => self.items.get(instance.definition_index),
            ObjectKind::LevelObject => self.level_objects.get(instance.definition_index),
        }
    }
}

/// Extract placements without silently discarding entries with an out-of-range
/// definition index. Such entries can be diagnosed via `definition()`.
pub fn instances(map: &Map) -> Vec<ObjectInstance> {
    let mut result = Vec::new();
    for x in 0..MAP_SIDE {
        for y in 0..MAP_SIDE {
            let cell = map.wall(x, y).expect("coordinates in map range");
            let (kind, first) = match cell.texture_id {
                FIRST_ITEM..FIRST_OBJECT => (ObjectKind::Item, FIRST_ITEM),
                FIRST_OBJECT..=LAST_OBJECT => (ObjectKind::LevelObject, FIRST_OBJECT),
                _ => continue,
            };
            result.push(ObjectInstance {
                kind,
                definition_index: usize::from(cell.texture_id - first),
                grid_cell: [x, y],
                position: [
                    f32::from(cell.start[0]) / UNITS_PER_TILE,
                    f32::from(cell.start[1]) / UNITS_PER_TILE,
                ],
                angle_radians: f32::from(cell.flags & 7) * (std::f32::consts::PI / 4.0)
                    + std::f32::consts::PI,
                flags: cell.flags,
                difficulty_flags: cell.end[0],
                extra: cell.end[1],
                size: cell.size,
            });
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    const INF: &str = "[3D_OBJECTS]\n2 [header]\n; comment\n0 0 -1 0 102 0 0 0 0 life.3o\n32 1 -2 0 0 3 10 9 70 Lamp.3O %flash.ani\n[WEAPONS]\n";
    const RESOURCE: &str =
        "#newobjects\n12 0 -1 0 0 0 0 0 0 box.3o box.ani\n0 0 -1 0 0 0 0 0 0 modern\n#end\n";
    fn map_with(id: u8, flags: u8) -> Map {
        let mut bytes = vec![0; 0x27001 + 4];
        let off = 0x18001 + (2 * MAP_SIDE + 3) * 11;
        bytes[off..off + 11].copy_from_slice(&[id, 17, flags, 0, 1, 128, 2, 7, 0, 9, 0]);
        Map::parse(&bytes).unwrap()
    }
    #[test]
    fn parses_definitions_and_resource_paths() {
        let definitions = ObjectDefinitions::parse(INF.as_bytes(), RESOURCE.as_bytes()).unwrap();
        assert_eq!(definitions.items.len(), 2);
        assert_eq!(definitions.level_objects.len(), 2);
        assert_eq!(
            definitions.items[1].model_3o_path().as_deref(),
            Some("model/lamp.3o")
        );
        assert_eq!(
            definitions.items[1].animation_path().as_deref(),
            Some("model/flash.ani")
        );
        assert_eq!(definitions.level_objects[1].model_3o_path(), None);
        assert_eq!(
            definitions.level_objects[1].model_gltf_path(),
            "model/modern.gltf"
        );
    }
    #[test]
    fn extracts_item_and_object_with_raw_flags() {
        let definitions = ObjectDefinitions::parse(INF.as_bytes(), RESOURCE.as_bytes()).unwrap();
        let items = instances(&map_with(132, 0x82));
        assert_eq!(items[0].grid_cell, [2, 3]);
        assert_eq!(items[0].position, [1.0, 2.5]);
        assert_eq!(items[0].flags, 0x82);
        assert_eq!(items[0].difficulty_flags, 7);
        assert_eq!(items[0].extra, 9);
        assert!((items[0].angle_radians - 1.5 * std::f32::consts::PI).abs() < 1e-6);
        assert_eq!(
            definitions.definition(&items[0]).unwrap().model_name,
            "Lamp.3O"
        );
        let objects = instances(&map_with(163, 0));
        assert_eq!(objects[0].kind, ObjectKind::LevelObject);
        assert_eq!(
            definitions.definition(&objects[0]).unwrap().model_name,
            "box.3o"
        );
        assert!(definitions
            .definition(&instances(&map_with(226, 0))[0])
            .is_none());
        assert!(instances(&map_with(250, 0)).is_empty());
    }
    #[test]
    fn rejects_invalid_definitions_and_traversal() {
        assert!(ObjectDefinitions::parse(INF.as_bytes(), b"#newobjects\ninvalid\n#end\n").is_err());
        assert!(ObjectDefinitions::parse(
            INF.as_bytes(),
            b"#newobjects\n0 0 -1 0 0 0 0 0 0 ../model.3o\n#end\n"
        )
        .is_err());
        assert!(ObjectDefinitions::parse(INF.as_bytes(), b"#newobjects\n").is_err());
        assert!(ObjectDefinitions::parse(b"[3D_OBJECTS]\n33\n", RESOURCE.as_bytes()).is_err());
    }
}
