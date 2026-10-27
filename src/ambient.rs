//! Authoritative ambient-sound IDs for the map's cells.
//!
//! Sound selection and playback are the responsibility of the presentation
//! layer. Process actions update this grid once and do not restore it on close.
use crate::map::{Map, MAP_SIDE};
use std::error::Error;
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AmbientRegion {
    /// Inclusive, signed authored coordinates, clipped to the map when applied.
    pub min_cell: [i16; 2],
    pub max_cell: [i16; 2],
    /// Index into the level's ambient sound definitions, from 0 through 31.
    pub sound_id: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidAmbientRegion {
    /// Index of the first invalid region in the requested batch.
    pub index: usize,
}
impl fmt::Display for InvalidAmbientRegion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid ambient sound ID in region {}", self.index)
    }
}
impl Error for InvalidAmbientRegion {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AmbientGrid {
    // X-major, matching Map's on-disk and public indexing.
    cells: [u8; MAP_SIDE * MAP_SIDE],
}
impl AmbientGrid {
    /// Copy authored ambient sound IDs, preserving even unusual raw IDs.
    pub fn from_map(map: &Map) -> Self {
        let mut cells = [0; MAP_SIDE * MAP_SIDE];
        for x in 0..MAP_SIDE {
            for y in 0..MAP_SIDE {
                cells[x * MAP_SIDE + y] = map.ambient_sound(x, y).expect("valid map cell");
            }
        }
        Self { cells }
    }

    pub fn cell(&self, cell: [u8; 2]) -> Option<u8> {
        let [x, y] = cell.map(usize::from);
        (x < MAP_SIDE && y < MAP_SIDE).then(|| self.cells[x * MAP_SIDE + y])
    }

    /// Validate the entire batch, then apply regions in their authored order.
    /// Out-of-map rectangles are clipped; empty or inverted rectangles are
    /// valid no-ops, as in the remaster's bounded inclusive loops.
    pub fn apply(&mut self, regions: &[AmbientRegion]) -> Result<(), InvalidAmbientRegion> {
        if let Some(index) = regions.iter().position(|region| region.sound_id >= 32) {
            return Err(InvalidAmbientRegion { index });
        }
        for region in regions {
            let [x1, y1] = region.min_cell.map(|v| v.max(0));
            let [x2, y2] = region.max_cell.map(|v| v.min(MAP_SIDE as i16 - 1));
            if x1 > x2 || y1 > y2 {
                continue;
            }
            for x in x1..=x2 {
                for y in y1..=y2 {
                    self.cells[x as usize * MAP_SIDE + y as usize] = region.sound_id;
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map() -> Map {
        let mut bytes = vec![0; 0x27005];
        bytes[0x26001 + 2 * MAP_SIDE + 3] = 7;
        bytes[0x26001 + 3 * MAP_SIDE + 2] = 9;
        Map::parse(&bytes).unwrap()
    }
    fn region(min_cell: [i16; 2], max_cell: [i16; 2], sound_id: u8) -> AmbientRegion {
        AmbientRegion {
            min_cell,
            max_cell,
            sound_id,
        }
    }
    #[test]
    fn map_ids_and_coordinates_are_preserved() {
        let grid = AmbientGrid::from_map(&map());
        assert_eq!(grid.cell([2, 3]), Some(7));
        assert_eq!(grid.cell([3, 2]), Some(9));
        assert_eq!(grid.cell([64, 2]), None);
    }
    #[test]
    fn regions_clip_and_later_writes_overwrite() {
        let mut grid = AmbientGrid::from_map(&map());
        grid.apply(&[
            region([-2, -1], [2, 3], 4),
            region([2, 3], [90, 90], 5),
            region([70, 1], [80, 5], 8),
            region([5, 5], [4, 6], 8),
        ])
        .unwrap();
        assert_eq!(grid.cell([0, 0]), Some(4));
        assert_eq!(grid.cell([2, 3]), Some(5));
        assert_eq!(grid.cell([63, 63]), Some(5));
        assert_eq!(grid.cell([3, 2]), Some(9));
    }
    #[test]
    fn invalid_batch_does_not_partially_update() {
        let mut grid = AmbientGrid::from_map(&map());
        let before = grid.clone();
        assert_eq!(
            grid.apply(&[region([0, 0], [5, 5], 1), region([0, 0], [0, 0], 32)]),
            Err(InvalidAmbientRegion { index: 1 })
        );
        assert_eq!(grid, before);
    }
}
