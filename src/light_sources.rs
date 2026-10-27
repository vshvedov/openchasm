//! Headless process-owned dynamic light sources.
//!
//! The caller owns process timing and presentation; this state preserves the
//! remaster's authored coordinates and startup delay without inventing a blink
//! pattern or binding simulation to a renderer.
use std::collections::BTreeMap;

const RAW_MAP_LIMIT: u16 = 64 * 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceLight {
    /// X/Y in raw 8.8 map units after remaster cell-center conversion.
    pub position_raw: [u16; 2],
    /// Radius in raw 8.8 units, equal to authored radius * 64.
    pub radius_raw: u16,
    pub brightness: u8,
    pub delay_ms: u16,
}
impl SourceLight {
    /// Parse five authored `source` arguments. Coordinates below 64 are
    /// cell indices; larger coordinates are already in 8.8 map units.
    pub fn parse(args: &[String]) -> Result<Self, LightSourceError> {
        if args.len() != 5 {
            return Err(LightSourceError::InvalidDefinition);
        }
        let parsed = args
            .iter()
            .map(|v| v.parse::<i16>().ok())
            .collect::<Option<Vec<_>>>()
            .ok_or(LightSourceError::InvalidDefinition)?;
        let coordinate = |raw: i16| -> Option<u16> {
            let v = u16::try_from(raw).ok()?;
            let v = if v < 64 { v * 256 + 128 } else { v };
            (v < RAW_MAP_LIMIT).then_some(v)
        };
        let radius = u16::try_from(parsed[2])
            .ok()
            .filter(|n| *n <= 511)
            .ok_or(LightSourceError::InvalidDefinition)?;
        Ok(Self {
            position_raw: [
                coordinate(parsed[0]).ok_or(LightSourceError::InvalidDefinition)?,
                coordinate(parsed[1]).ok_or(LightSourceError::InvalidDefinition)?,
            ],
            radius_raw: radius * 64,
            brightness: u8::try_from(parsed[3]).map_err(|_| LightSourceError::InvalidDefinition)?,
            delay_ms: u16::try_from(parsed[4]).map_err(|_| LightSourceError::InvalidDefinition)?,
        })
    }
    fn valid(&self) -> bool {
        self.position_raw.iter().all(|v| *v < RAW_MAP_LIMIT)
            && self.radius_raw % 64 == 0
            && self.radius_raw <= 511 * 64
            && self.delay_ms <= i16::MAX as u16
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LightSourceError {
    InvalidDefinition,
    EmptySources,
    AlreadyActive,
    DuplicateProcess,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LightSourceProcessSnapshot {
    pub process_id: u16,
    /// Sources in authored action order.
    pub sources: Vec<SourceLight>,
    /// Number of authoritative 60 Hz ticks since activation.
    pub elapsed_ticks: u64,
}
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LightSourcesSnapshot {
    pub active: Vec<LightSourceProcessSnapshot>,
}
#[derive(Debug, Clone, Default)]
pub struct LightSources {
    active: BTreeMap<u16, LightSourceProcessSnapshot>,
}
impl LightSources {
    pub fn activate(
        &mut self,
        process_id: u16,
        sources: &[SourceLight],
    ) -> Result<(), LightSourceError> {
        if sources.is_empty() {
            return Err(LightSourceError::EmptySources);
        }
        if sources.iter().any(|s| !s.valid()) {
            return Err(LightSourceError::InvalidDefinition);
        }
        if self.active.contains_key(&process_id) {
            return Err(LightSourceError::AlreadyActive);
        }
        self.active.insert(
            process_id,
            LightSourceProcessSnapshot {
                process_id,
                sources: sources.to_vec(),
                elapsed_ticks: 0,
            },
        );
        Ok(())
    }
    pub fn deactivate(&mut self, process_id: u16) -> bool {
        self.active.remove(&process_id).is_some()
    }
    pub fn tick(&mut self) {
        for process in self.active.values_mut() {
            process.elapsed_ticks = process.elapsed_ticks.saturating_add(1);
        }
    }
    pub fn active(&self) -> impl Iterator<Item = &LightSourceProcessSnapshot> {
        self.active.values()
    }
    pub fn snapshot(&self) -> LightSourcesSnapshot {
        LightSourcesSnapshot {
            active: self.active.values().cloned().collect(),
        }
    }
    /// Validate all processes and sources before publishing the restored state.
    pub fn restore(&mut self, snapshot: &LightSourcesSnapshot) -> Result<(), LightSourceError> {
        let mut active = BTreeMap::new();
        for process in &snapshot.active {
            if process.sources.is_empty() {
                return Err(LightSourceError::EmptySources);
            }
            if process.sources.iter().any(|s| !s.valid()) {
                return Err(LightSourceError::InvalidDefinition);
            }
            if active.insert(process.process_id, process.clone()).is_some() {
                return Err(LightSourceError::DuplicateProcess);
            }
        }
        self.active = active;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn parse(s: &str) -> SourceLight {
        SourceLight::parse(&s.split_whitespace().map(str::to_owned).collect::<Vec<_>>()).unwrap()
    }
    #[test]
    fn remaster_converts_cell_centers_and_fixed_coordinates() {
        let cell = parse("29 29 5 28 160");
        assert_eq!(cell.position_raw, [29 * 256 + 128, 29 * 256 + 128]);
        assert_eq!(
            (cell.radius_raw, cell.brightness, cell.delay_ms),
            (320, 28, 160)
        );
        let fixed = parse("10240 8192 9 32 10");
        assert_eq!(fixed.position_raw, [10240, 8192]);
        assert_eq!(fixed.radius_raw, 576);
        assert_eq!(parse("63 64 0 0 0").position_raw, [63 * 256 + 128, 64]);
    }
    #[test]
    fn validates_and_restores_multiple_sources_atomically() {
        let mut sources = LightSources::default();
        let a = parse("29 29 5 28 160");
        let b = parse("26 29 5 28 190");
        sources.activate(2, &[a, b]).unwrap();
        sources.tick();
        let snapshot = sources.snapshot();
        let mut restored = LightSources::default();
        restored.restore(&snapshot).unwrap();
        assert_eq!(restored.snapshot(), snapshot);
        let mut bad = snapshot.clone();
        bad.active[0].sources[0].position_raw[0] = RAW_MAP_LIMIT;
        assert_eq!(
            restored.restore(&bad),
            Err(LightSourceError::InvalidDefinition)
        );
        assert_eq!(restored.snapshot(), snapshot);
        assert!(restored.deactivate(2));
        assert!(restored.active().next().is_none());
    }
    #[test]
    fn rejects_invalid_authored_inputs() {
        for source in [
            "-1 1 1 1 1",
            "16384 1 1 1 1",
            "1 1 -1 1 1",
            "1 1 512 1 1",
            "1 1 1 256 1",
            "1 1 1 1 -1",
            "1 1 1 1 1 1",
        ] {
            assert!(
                SourceLight::parse(
                    &source
                        .split_whitespace()
                        .map(str::to_owned)
                        .collect::<Vec<_>>()
                )
                .is_err(),
                "{source}"
            );
        }
    }
}
