//! Provisional headless state for authored `quake` rectangles.
//!
//! This records raw power only; it does not invent vibration, damage, timing,
//! or networking behavior. See `docs/quake.md` for the evidence and limits.
use std::error::Error;
use std::fmt;

const SIDE: usize = 64;

/// An inclusive rectangle of map cells and its unscaled authored power.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QuakeZone {
    pub min_cell: [u8; 2],
    pub max_cell: [u8; 2],
    pub power: u32,
}
impl QuakeZone {
    fn valid(&self) -> bool {
        self.min_cell[0] <= self.max_cell[0]
            && self.min_cell[1] <= self.max_cell[1]
            && usize::from(self.max_cell[0]) < SIDE
            && usize::from(self.max_cell[1]) < SIDE
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QuakeError {
    pub action_index: usize,
}
impl fmt::Display for QuakeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid quake rectangle at action {}", self.action_index)
    }
}
impl Error for QuakeError {}

/// Map-cell powers. Later actions overwrite earlier ones; zero clears a cell.
/// This is a provisional interpretation, not verified original-game behavior.
#[derive(Clone, Debug)]
pub struct QuakeField {
    cells: Vec<u32>,
}
impl Default for QuakeField {
    fn default() -> Self {
        Self {
            cells: vec![0; SIDE * SIDE],
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuakeSnapshot {
    pub cells: Vec<u32>,
}

impl QuakeField {
    pub fn snapshot(&self) -> QuakeSnapshot {
        QuakeSnapshot {
            cells: self.cells.clone(),
        }
    }
    pub fn restore(&mut self, snapshot: &QuakeSnapshot) -> Result<(), &'static str> {
        if snapshot.cells.len() != SIDE * SIDE {
            return Err("invalid quake grid length");
        }
        self.cells.clone_from(&snapshot.cells);
        Ok(())
    }

    pub fn new() -> Self {
        Self::default()
    }

    /// Validate the complete ordered batch before applying any assignments.
    pub fn apply_batch(&mut self, zones: &[QuakeZone]) -> Result<(), QuakeError> {
        if let Some((action_index, _)) = zones.iter().enumerate().find(|(_, z)| !z.valid()) {
            return Err(QuakeError { action_index });
        }
        for zone in zones {
            for x in zone.min_cell[0]..=zone.max_cell[0] {
                for y in zone.min_cell[1]..=zone.max_cell[1] {
                    self.cells[usize::from(x) * SIDE + usize::from(y)] = zone.power;
                }
            }
        }
        Ok(())
    }

    /// Look up the unscaled authored power at a grid cell.
    pub fn cell(&self, cell: [u8; 2]) -> Option<u32> {
        if usize::from(cell[0]) >= SIDE || usize::from(cell[1]) >= SIDE {
            return None;
        }
        Some(self.cells[usize::from(cell[0]) * SIDE + usize::from(cell[1])])
    }

    /// Select the containing cell; out-of-map or non-finite positions have no value.
    pub fn sample(&self, position: [f32; 2]) -> Option<u32> {
        if !position
            .iter()
            .all(|v| v.is_finite() && *v >= 0.0 && *v < SIDE as f32)
        {
            return None;
        }
        self.cell([position[0].floor() as u8, position[1].floor() as u8])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn zone(min_cell: [u8; 2], max_cell: [u8; 2], power: u32) -> QuakeZone {
        QuakeZone {
            min_cell,
            max_cell,
            power,
        }
    }
    #[test]
    fn snapshot_preserves_grid_and_rejects_short_input() {
        let mut field = QuakeField::new();
        field.apply_batch(&[zone([1, 1], [2, 2], 100)]).unwrap();
        let snapshot = field.snapshot();
        let mut target = QuakeField::new();
        target.restore(&snapshot).unwrap();
        assert_eq!(target.snapshot(), snapshot);
        let mut bad = snapshot.clone();
        bad.cells.pop();
        assert!(target.restore(&bad).is_err());
        assert_eq!(target.snapshot(), snapshot);
    }

    #[test]
    fn level_16_heart_then_later_stop_clears_the_same_region() {
        let mut field = QuakeField::new();
        field
            .apply_batch(&[zone([17, 40], [30, 50], 10000)])
            .unwrap();
        assert_eq!(field.sample([17.0, 40.0]), Some(10000));
        assert_eq!(field.sample([30.9, 50.9]), Some(10000));
        assert_eq!(field.cell([31, 50]), Some(0));
        field.apply_batch(&[zone([17, 40], [30, 50], 0)]).unwrap();
        assert_eq!(field.cell([17, 40]), Some(0));
        assert_eq!(field.cell([30, 50]), Some(0));
    }
    #[test]
    fn batches_are_atomic_and_overlaps_are_ordered() {
        let mut field = QuakeField::new();
        field
            .apply_batch(&[zone([1, 1], [2, 2], 10), zone([2, 2], [3, 3], 20)])
            .unwrap();
        assert_eq!(field.cell([2, 2]), Some(20));
        let error = field
            .apply_batch(&[zone([1, 1], [1, 1], 99), zone([4, 4], [3, 4], 4)])
            .unwrap_err();
        assert_eq!(error.action_index, 1);
        assert_eq!(field.cell([1, 1]), Some(10));
        assert!(field.apply_batch(&[zone([0, 0], [64, 1], 2)]).is_err());
        assert_eq!(field.sample([f32::NAN, 1.0]), None);
        assert_eq!(field.sample([-0.01, 0.0]), None);
    }
}
