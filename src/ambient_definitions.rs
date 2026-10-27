//! Level-local ambient sound definitions, independent of audio playback.
use std::error::Error;
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AmbientDefinition {
    /// Resource name beneath `sound/`; an authored blank slot has no name.
    pub file_name: Option<String>,
    /// Authored volume, defaulting to 86 when omitted, in the range 0..=128.
    pub volume: u8,
    /// Optional global or level sound ID for a randomized background cue.
    pub background_sound: Option<u8>,
}
impl AmbientDefinition {
    pub fn resource_path(&self) -> Option<String> {
        self.file_name.as_ref().map(|name| format!("sound/{name}"))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AmbientDefinitions {
    pub entries: [Option<AmbientDefinition>; 32],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AmbientParseError {
    pub line: usize,
    pub reason: &'static str,
}
impl fmt::Display for AmbientParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ambient definitions line {}: {}", self.line, self.reason)
    }
}
impl Error for AmbientParseError {}

impl AmbientDefinitions {
    /// Parse the level resource's `#ambients` block. Unknown or malformed
    /// fields reject the resource rather than silently changing its meaning.
    pub fn parse(data: &[u8]) -> Result<Self, AmbientParseError> {
        let text = std::str::from_utf8(data).map_err(|_| AmbientParseError {
            line: 0,
            reason: "resource is not UTF-8",
        })?;
        let mut result = Self {
            entries: std::array::from_fn(|_| None),
        };
        let mut in_section = false;
        let mut seen_section = false;
        for (index, raw) in text.lines().enumerate() {
            let line = raw.split(';').next().unwrap_or("").trim();
            if line.eq_ignore_ascii_case("#ambients") {
                if seen_section {
                    return Err(AmbientParseError {
                        line: index + 1,
                        reason: "duplicate ambient section",
                    });
                }
                seen_section = true;
                in_section = true;
                continue;
            }
            if !in_section {
                continue;
            }
            if line.eq_ignore_ascii_case("#end") {
                in_section = false;
                continue;
            }
            if line.is_empty() {
                continue;
            }
            let fail = |reason| AmbientParseError {
                line: index + 1,
                reason,
            };
            let (id, value) = line
                .split_once('=')
                .ok_or_else(|| fail("missing equals sign"))?;
            let id = id
                .trim()
                .strip_suffix(':')
                .ok_or_else(|| fail("missing ID colon"))?
                .trim()
                .parse::<usize>()
                .ok()
                .filter(|&n| n < 32)
                .ok_or_else(|| fail("invalid ambient ID"))?;
            if result.entries[id].is_some() {
                return Err(fail("duplicate ambient ID"));
            }
            let mut parts = value.split_whitespace();
            let file_name = match parts.next() {
                Some(name)
                    if name != "."
                        && name != ".."
                        && !name.contains("..")
                        && !name
                            .chars()
                            .any(|c| c.is_control() || matches!(c, '/' | '\\' | ':')) =>
                {
                    Some(name.to_owned())
                }
                Some(_) => return Err(fail("invalid resource name")),
                None => None,
            };
            let mut volume = None;
            let mut background_sound = None;
            for part in parts {
                let (key, value) = part.split_once(':').ok_or_else(|| fail("invalid option"))?;
                match key.to_ascii_lowercase().as_str() {
                    "v" if volume.is_none() => {
                        volume = Some(
                            value
                                .parse::<u8>()
                                .ok()
                                .filter(|n| *n <= 128)
                                .ok_or_else(|| fail("invalid volume"))?,
                        )
                    }
                    "b" if background_sound.is_none() => {
                        background_sound = Some(
                            value
                                .parse::<u8>()
                                .ok()
                                .filter(|n| *n <= 154)
                                .ok_or_else(|| fail("invalid background sound ID"))?,
                        )
                    }
                    _ => return Err(fail("unknown or repeated option")),
                }
            }
            result.entries[id] = Some(AmbientDefinition {
                file_name,
                volume: volume.unwrap_or(86),
                background_sound,
            });
        }
        if in_section {
            return Err(AmbientParseError {
                line: text.lines().count(),
                reason: "unterminated ambient section",
            });
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_blank_default_and_options() {
        let defs = AmbientDefinitions::parse(
            b"#AMBIENTS\n00: = a.wav\n1: =\n2: = mix.wav v:40 b:0 ; comment\n#end\n",
        )
        .unwrap();
        assert_eq!(defs.entries[0].as_ref().unwrap().volume, 86);
        assert_eq!(defs.entries[1].as_ref().unwrap().file_name, None);
        let def = defs.entries[2].as_ref().unwrap();
        assert_eq!(def.resource_path().as_deref(), Some("sound/mix.wav"));
        assert_eq!((def.volume, def.background_sound), (40, Some(0)));
        assert!(defs.entries[3].is_none());
    }
    #[test]
    fn rejects_invalid_ids_paths_and_options() {
        for row in [
            "32: = x.wav",
            "1: = ../x.wav",
            "1: = x.wav v:129",
            "1: = x.wav b:155",
            "1: = x.wav v:1 v:2",
            "1: = x.wav q:2",
            "1 = x.wav",
            "1: = x.wav\n1: = y.wav",
        ] {
            assert!(
                AmbientDefinitions::parse(format!("#ambients\n{row}\n#end\n").as_bytes()).is_err(),
                "{row}"
            );
        }
        assert!(AmbientDefinitions::parse(b"#ambients\n1: = x.wav").is_err());
    }
}
