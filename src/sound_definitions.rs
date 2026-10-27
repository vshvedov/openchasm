//! Global and level-local sound table metadata, independent of audio playback.
//!
//! IDs 0–79 address `[sounds]` in chasm.inf, and 80–99 address the current
//! level's `#newsounds`. ID 0 is preserved: whether it means silence in a
//! particular context (such as object BSFX) is not established here.
use std::{error::Error, fmt};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SoundDefinition {
    /// Blank table entries are preserved separately from missing IDs.
    pub file_name: Option<String>,
    /// Authored `v:` value, default 86, in the inclusive range 0–128.
    pub volume: u8,
}
impl SoundDefinition {
    pub fn resource_path(&self) -> Option<String> {
        self.file_name.as_ref().map(|name| format!("sound/{name}"))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SoundDefinitions {
    pub global: [Option<SoundDefinition>; 80],
    pub level: [Option<SoundDefinition>; 20],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SoundTable {
    Global,
    Level,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SoundParseError {
    pub table: SoundTable,
    pub line: usize,
    pub reason: &'static str,
}
impl fmt::Display for SoundParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:?} sound table line {}: {}",
            self.table, self.line, self.reason
        )
    }
}
impl Error for SoundParseError {}

impl SoundDefinitions {
    /// Parse both tables atomically. Missing sections are empty; a present
    /// level section must terminate with `#end`. Monster IDs 100+ are not
    /// represented by these tables and resolve to `None`.
    pub fn parse(global: &[u8], level: &[u8]) -> Result<Self, SoundParseError> {
        let mut result = Self {
            global: std::array::from_fn(|_| None),
            level: std::array::from_fn(|_| None),
        };
        parse_table(global, SoundTable::Global, &mut result.global)?;
        parse_table(level, SoundTable::Level, &mut result.level)?;
        Ok(result)
    }
    pub fn get(&self, id: u8) -> Option<&SoundDefinition> {
        match id {
            0..=79 => self.global[usize::from(id)].as_ref(),
            80..=99 => self.level[usize::from(id - 80)].as_ref(),
            _ => None,
        }
    }
}

fn parse_table(
    data: &[u8],
    table: SoundTable,
    entries: &mut [Option<SoundDefinition>],
) -> Result<(), SoundParseError> {
    let fail = |line, reason| SoundParseError {
        table,
        line,
        reason,
    };
    let text = std::str::from_utf8(data).map_err(|_| fail(0, "resource is not UTF-8"))?;
    let mut inside = false;
    let mut seen = false;
    for (index, raw) in text.lines().enumerate() {
        let line_number = index + 1;
        let line = raw.split(';').next().unwrap_or("").trim();
        let header = match table {
            SoundTable::Global => "[sounds]",
            SoundTable::Level => "#newsounds",
        };
        if line.eq_ignore_ascii_case(header) {
            if seen {
                return Err(fail(line_number, "duplicate sound section"));
            }
            seen = true;
            inside = true;
            continue;
        }
        if !inside {
            continue;
        }
        if table == SoundTable::Global && line.starts_with('[') && line.ends_with(']') {
            inside = false;
            continue;
        }
        if table == SoundTable::Level && line.eq_ignore_ascii_case("#end") {
            inside = false;
            continue;
        }
        if line.is_empty() {
            continue;
        }
        let (left, right) = line
            .split_once('=')
            .ok_or_else(|| fail(line_number, "missing equals sign"))?;
        let left = left.trim();
        let digits = left.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0
            || left
                .as_bytes()
                .get(digits)
                .is_some_and(|c| *c != b':' && !c.is_ascii_whitespace())
        {
            return Err(fail(line_number, "invalid sound ID"));
        }
        if table == SoundTable::Level && left[digits..].trim() != ":" {
            return Err(fail(line_number, "invalid level sound ID"));
        }
        let id = left[..digits]
            .parse::<usize>()
            .map_err(|_| fail(line_number, "invalid sound ID"))?;
        let offset = if table == SoundTable::Global { 0 } else { 80 };
        let slot = id
            .checked_sub(offset)
            .filter(|&n| n < entries.len())
            .ok_or_else(|| fail(line_number, "sound ID outside table range"))?;
        if entries[slot].is_some() {
            return Err(fail(line_number, "duplicate sound ID"));
        }
        let mut words = right.split_whitespace();
        let file_name = match words.next() {
            None => None,
            Some(name)
                if !name.contains("..")
                    && !name
                        .chars()
                        .any(|c| c.is_control() || matches!(c, '/' | '\\' | ':'))
                    && name.to_ascii_lowercase().ends_with(".wav") =>
            {
                Some(name.to_owned())
            }
            Some(_) => return Err(fail(line_number, "invalid sound resource name")),
        };
        let mut volume = None;
        for word in words {
            let (key, value) = word
                .split_once(':')
                .ok_or_else(|| fail(line_number, "invalid option"))?;
            if !key.eq_ignore_ascii_case("v") || volume.is_some() {
                return Err(fail(line_number, "unknown or repeated option"));
            }
            volume = Some(
                value
                    .parse::<u8>()
                    .ok()
                    .filter(|&n| n <= 128)
                    .ok_or_else(|| fail(line_number, "invalid volume"))?,
            );
        }
        entries[slot] = Some(SoundDefinition {
            file_name,
            volume: volume.unwrap_or(86),
        });
    }
    if table == SoundTable::Level && inside {
        return Err(fail(text.lines().count(), "unterminated sound section"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn resolves_global_and_local_with_authored_volume_and_blank_entries() {
        let defs = SoundDefinitions::parse(b"[sounds]\n00: thunder = thund0.wav\n39 laser =\n70: glass = glass3.wav V:96\n[other]\n", b"#newsounds\n80: =\n81: = metal_d.wav v:96\n#end\n").unwrap();
        assert_eq!(
            defs.get(0).unwrap().resource_path().as_deref(),
            Some("sound/thund0.wav")
        );
        assert_eq!(defs.get(70).unwrap().volume, 96);
        assert_eq!(defs.get(39).unwrap().file_name, None);
        assert_eq!(defs.get(80).unwrap().file_name, None);
        assert_eq!(
            defs.get(81).unwrap().resource_path().as_deref(),
            Some("sound/metal_d.wav")
        );
        assert_eq!(defs.get(81).unwrap().volume, 96);
        assert_eq!(defs.get(79), None);
        assert_eq!(defs.get(100), None);
    }
    #[test]
    fn malformed_tables_are_rejected() {
        for level in [
            "#newsounds\n80: = ../x.wav\n#end\n",
            "#newsounds\n80: = a.wav v:129\n#end\n",
            "#newsounds\n80: = a.wav v:1 V:2\n#end\n",
            "#newsounds\n100: = a.wav\n#end\n",
            "#newsounds\n80: = a.wav\n80: = b.wav\n#end\n",
            "#newsounds\n80: = a.wav\n",
            "#newsounds\n80: = a.wav\n#end\n#newsounds\n#end\n",
        ] {
            assert!(
                SoundDefinitions::parse(b"", level.as_bytes()).is_err(),
                "{level}"
            );
        }
        assert!(SoundDefinitions::parse(b"[sounds]\n80: = a.wav\n", b"").is_err());
        assert!(SoundDefinitions::parse(b"[sounds]\n0: = a.wav\n0: = b.wav\n", b"").is_err());
        assert!(SoundDefinitions::parse(&[0xff], b"").is_err());
    }
}
