//! Headless process-owned wind fields and reference-derived interpolation.
//!
//! The owning simulation decides when procedures activate/deactivate, applies
//! displacement through its collision system, and synchronizes state over the
//! network. This module neither moves entities nor uses a wall clock.
use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;

const SIDE: usize = 64;
/// PanzerChasm applies wind displacement at half the authored vector per second.
/// Both this factor and interpolation need validation against the original game.
const DISPLACEMENT_SCALE: f32 = 0.5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindZone {
    /// Inclusive map grid coordinates.
    pub min_cell: [u8; 2],
    pub max_cell: [u8; 2],
    /// Authored signed field components; not yet confirmed physical units.
    pub direction: [i16; 2],
}
impl WindZone {
    fn valid(&self) -> bool {
        self.min_cell[0] <= self.max_cell[0]
            && self.min_cell[1] <= self.max_cell[1]
            && usize::from(self.max_cell[0]) < SIDE
            && usize::from(self.max_cell[1]) < SIDE
            && self.direction.iter().all(|n| (-128..=127).contains(n))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindError {
    EmptyZones,
    InvalidZone(usize),
    AlreadyActive(u16),
}
impl fmt::Display for WindError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyZones => write!(f, "wind activation requires at least one zone"),
            Self::InvalidZone(index) => write!(f, "invalid wind zone at index {index}"),
            Self::AlreadyActive(id) => write!(f, "wind process {id} is already active"),
        }
    }
}
impl Error for WindError {}

#[derive(Debug, Clone)]
pub struct WindField {
    // x-major, as in the map structures; each cell is an authored vector.
    cells: Vec<[i16; 2]>,
    active: BTreeMap<u16, Vec<WindZone>>,
}
impl Default for WindField {
    fn default() -> Self {
        Self::new()
    }
}
impl WindField {
    pub fn new() -> Self {
        Self {
            cells: vec![[0, 0]; SIDE * SIDE],
            active: BTreeMap::new(),
        }
    }

    /// Activate all zones atomically. Later zones overwrite earlier ones; a
    /// process cannot be activated twice without a deactivation in between.
    pub fn activate(&mut self, process_id: u16, zones: &[WindZone]) -> Result<(), WindError> {
        if zones.is_empty() {
            return Err(WindError::EmptyZones);
        }
        if let Some((index, _)) = zones.iter().enumerate().find(|(_, zone)| !zone.valid()) {
            return Err(WindError::InvalidZone(index));
        }
        if self.active.contains_key(&process_id) {
            return Err(WindError::AlreadyActive(process_id));
        }
        for zone in zones {
            self.write_zone(zone, zone.direction);
        }
        self.active.insert(process_id, zones.to_vec());
        Ok(())
    }

    /// Deactivate and clear this process's rectangles, returning false if it
    /// was not active. Per the reference, clearing does not restore earlier or
    /// later overlapping winds; clearing one process can erase another's cells.
    pub fn deactivate(&mut self, process_id: u16) -> bool {
        let Some(zones) = self.active.remove(&process_id) else {
            return false;
        };
        for zone in &zones {
            self.write_zone(zone, [0, 0]);
        }
        true
    }
    pub fn is_active(&self, process_id: u16) -> bool {
        self.active.contains_key(&process_id)
    }

    fn write_zone(&mut self, zone: &WindZone, value: [i16; 2]) {
        for x in zone.min_cell[0]..=zone.max_cell[0] {
            for y in zone.min_cell[1]..=zone.max_cell[1] {
                self.cells[usize::from(x) * SIDE + usize::from(y)] = value;
            }
        }
    }
    pub fn cell(&self, cell: [u8; 2]) -> Option<[i16; 2]> {
        if usize::from(cell[0]) >= SIDE || usize::from(cell[1]) >= SIDE {
            return None;
        }
        Some(self.cells[usize::from(cell[0]) * SIDE + usize::from(cell[1])])
    }

    /// Interpolate surrounding field vectors about their tile centers. Returns
    /// None for non-finite coordinates, or zero outside the sampled map region.
    /// Reference behavior near boundaries is provisional: it truncates (x-.5)
    /// toward zero and ignores samples whose base cell is the last grid row.
    pub fn sample(&self, position: [f32; 2]) -> Option<[f32; 2]> {
        if !position.iter().all(|x| x.is_finite()) {
            return None;
        }
        if !(0.0..SIDE as f32).contains(&position[0]) || !(0.0..SIDE as f32).contains(&position[1])
        {
            return Some([0.0, 0.0]);
        }
        let offset = [position[0] - 0.5, position[1] - 0.5];
        let base = [offset[0].trunc() as i32, offset[1].trunc() as i32];
        if base
            .iter()
            .any(|&coord| coord < 0 || coord >= SIDE as i32 - 1)
        {
            return Some([0.0, 0.0]);
        }
        let fraction = [offset[0] - base[0] as f32, offset[1] - base[1] as f32];
        let (x, y) = (base[0] as usize, base[1] as usize);
        let weights = [
            (1.0 - fraction[0]) * (1.0 - fraction[1]),
            (1.0 - fraction[0]) * fraction[1],
            fraction[0] * (1.0 - fraction[1]),
            fraction[0] * fraction[1],
        ];
        let cells = [
            self.cells[x * SIDE + y],
            self.cells[x * SIDE + y + 1],
            self.cells[(x + 1) * SIDE + y],
            self.cells[(x + 1) * SIDE + y + 1],
        ];
        let mut wind = [0.0; 2];
        for (cell, weight) in cells.iter().zip(weights) {
            wind[0] += f32::from(cell[0]) * weight;
            wind[1] += f32::from(cell[1]) * weight;
        }
        Some(wind)
    }

    /// Proposed external movement in tiles for a tick. The host must apply it
    /// through entity/wall collision rather than directly teleporting an actor.
    /// Invalid time, coordinates or floating-point overflow return None.
    pub fn displacement(&self, position: [f32; 2], dt: f32) -> Option<[f32; 2]> {
        if !dt.is_finite() || dt < 0.0 {
            return None;
        }
        let wind = self.sample(position)?;
        let result = [
            wind[0] * DISPLACEMENT_SCALE * dt,
            wind[1] * DISPLACEMENT_SCALE * dt,
        ];
        result.iter().all(|v| v.is_finite()).then_some(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn zone(min: [u8; 2], max: [u8; 2], direction: [i16; 2]) -> WindZone {
        WindZone {
            min_cell: min,
            max_cell: max,
            direction,
        }
    }
    #[test]
    fn bilinear_sampling_and_displacement_use_cell_centers() {
        let mut field = WindField::new();
        field
            .activate(7, &[zone([1, 1], [1, 1], [12, -4])])
            .unwrap();
        assert_eq!(field.sample([1.5, 1.5]), Some([12.0, -4.0]));
        assert_eq!(field.sample([2.0, 1.5]), Some([6.0, -2.0]));
        assert_eq!(field.sample([2.0, 2.0]), Some([3.0, -1.0]));
        assert_eq!(field.displacement([1.5, 1.5], 0.5), Some([3.0, -1.0]));
        assert_eq!(field.sample([2.5, 2.5]), Some([0.0, 0.0]));
    }
    #[test]
    fn overlapping_processes_overwrite_and_clear_without_restoring() {
        let mut field = WindField::new();
        field.activate(1, &[zone([1, 1], [2, 1], [4, 0])]).unwrap();
        field.activate(2, &[zone([2, 1], [3, 1], [8, 0])]).unwrap();
        assert_eq!(field.cell([1, 1]), Some([4, 0]));
        assert_eq!(field.cell([2, 1]), Some([8, 0]));
        assert!(field.deactivate(1));
        assert!(field.is_active(2));
        assert_eq!(field.cell([1, 1]), Some([0, 0]));
        assert_eq!(field.cell([2, 1]), Some([0, 0]));
        assert_eq!(field.cell([3, 1]), Some([8, 0]));
        assert!(!field.deactivate(1));
        assert!(field.deactivate(2));
    }
    #[test]
    fn validation_is_atomic_and_rejects_duplicate_activation() {
        let mut field = WindField::new();
        let valid = zone([1, 1], [2, 2], [2, 0]);
        let invalid = zone([1, 1], [64, 2], [2, 0]);
        assert_eq!(
            field.activate(1, &[valid, invalid]),
            Err(WindError::InvalidZone(1))
        );
        assert!(!field.is_active(1));
        assert_eq!(field.cell([1, 1]), Some([0, 0]));
        field.activate(1, &[valid]).unwrap();
        assert_eq!(
            field.activate(1, &[zone([1, 1], [1, 1], [8, 0])]),
            Err(WindError::AlreadyActive(1))
        );
        assert_eq!(field.cell([1, 1]), Some([2, 0]));
        assert_eq!(field.activate(2, &[]), Err(WindError::EmptyZones));
        assert_eq!(
            field.activate(2, &[zone([0, 0], [0, 0], [128, 0])]),
            Err(WindError::InvalidZone(0))
        );
    }
    #[test]
    fn boundary_and_invalid_values_do_not_corrupt_simulation() {
        let mut field = WindField::new();
        field
            .activate(1, &[zone([63, 63], [63, 63], [8, 8])])
            .unwrap();
        assert_eq!(field.sample([63.5, 63.5]), Some([0.0, 0.0]));
        assert_eq!(field.sample([-1.0, 2.0]), Some([0.0, 0.0]));
        assert_eq!(field.sample([f32::NAN, 2.0]), None);
        assert_eq!(field.displacement([1.0, 1.0], -1.0), None);
        assert_eq!(field.displacement([1.0, 1.0], f32::INFINITY), None);
        assert_eq!(field.cell([64, 0]), None);
    }
}
