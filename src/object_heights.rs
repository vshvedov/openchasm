//! Conservative, headless object height metadata from base mesh geometry.
//! Unknown models remain blocking; this module does not load assets or mutate
//! entity state. See `docs/object-heights.md` for assumptions and integration.
use crate::model::Model;
use std::collections::BTreeMap;

const MODEL_SCALE: f32 = 1.0 / 2048.0;

/// Local, unanimated height range in map tiles, relative to the object pose.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HeightBounds {
    pub bottom: f32,
    pub top: f32,
}
impl HeightBounds {
    pub fn new(bottom: f32, top: f32) -> Option<Self> {
        (bottom.is_finite() && top.is_finite() && bottom <= top).then_some(Self { bottom, top })
    }
    /// Match the reference's base 3O mesh rather than its animation frames.
    pub fn from_3o(model: &Model) -> Option<Self> {
        let mut vertices = model.vertices.iter();
        let first = vertices.next()?.position[2];
        let (mut low, mut high) = (first, first);
        for vertex in vertices {
            low = low.min(vertex.position[2]);
            high = high.max(vertex.position[2]);
        }
        Self::new(f32::from(low) * MODEL_SCALE, f32::from(high) * MODEL_SCALE)
    }
    /// Base glTF geometry with column-major accumulated node transforms, as
    /// consumed by the client renderer. Any invalid primitive rejects all
    /// bounds, preserving the conservative fallback for that model.
    pub fn from_gltf_base(primitives: &[GltfPrimitive<'_>]) -> Option<Self> {
        let (mut low, mut high) = (f32::INFINITY, f32::NEG_INFINITY);
        let mut seen = false;
        for primitive in primitives {
            if !primitive.transform.iter().flatten().all(|v| v.is_finite()) {
                return None;
            }
            for position in primitive.positions {
                if !position.iter().all(|v| v.is_finite()) {
                    return None;
                }
                let z = primitive.transform[0][2] * position[0]
                    + primitive.transform[1][2] * position[1]
                    + primitive.transform[2][2] * position[2]
                    + primitive.transform[3][2];
                if !z.is_finite() {
                    return None;
                }
                low = low.min(z);
                high = high.max(z);
                seen = true;
            }
        }
        if seen {
            Self::new(low, high)
        } else {
            None
        }
    }
}

/// A glTF primitive's basis-frame positions and accumulated node transform.
pub struct GltfPrimitive<'a> {
    pub positions: &'a [[f32; 3]],
    pub transform: [[f32; 4]; 4],
}

/// Height metadata keyed by original raw map type (items 131–162, level
/// objects 163–226). A replacement selects its new raw type at query time.
#[derive(Clone, Debug, Default)]
pub struct ObjectHeightCatalog {
    bounds: BTreeMap<u8, HeightBounds>,
}
impl ObjectHeightCatalog {
    pub fn new() -> Self {
        Self::default()
    }
    /// Reject invalid IDs and bounds; on failure do not change the catalog.
    pub fn set(&mut self, raw_type: u8, bounds: HeightBounds) -> bool {
        if !(131..=226).contains(&raw_type)
            || HeightBounds::new(bounds.bottom, bounds.top).is_none()
        {
            return false;
        }
        self.bounds.insert(raw_type, bounds);
        true
    }
    pub fn get(&self, raw_type: u8) -> Option<HeightBounds> {
        self.bounds.get(&raw_type).copied()
    }
    /// True when the intervals overlap or inputs/metadata are unknown. Exact
    /// touching is considered overlap, matching the reference's strict
    /// separated-interval comparisons. Elevation is in map tiles.
    pub fn overlaps(
        &self,
        raw_type: u8,
        elevation: f32,
        entity_bottom: f32,
        entity_top: f32,
    ) -> bool {
        if !elevation.is_finite()
            || !entity_bottom.is_finite()
            || !entity_top.is_finite()
            || entity_bottom > entity_top
        {
            return true;
        }
        let Some(bounds) = self.get(raw_type) else {
            return true;
        };
        let bottom = f64::from(bounds.bottom) + f64::from(elevation);
        let top = f64::from(bounds.top) + f64::from(elevation);
        !(f64::from(entity_top) < bottom || f64::from(entity_bottom) > top)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Vertex;
    #[test]
    fn geometry_height_is_scaled_and_empty_data_fails_closed() {
        let model = Model {
            vertices: vec![
                Vertex {
                    position: [0, 0, 4096],
                },
                Vertex {
                    position: [0, 0, 9626],
                },
            ],
            polygons: vec![],
            skin_height: 0,
            skin_indices: vec![],
        };
        let bounds = HeightBounds::from_3o(&model).unwrap();
        assert_eq!(bounds.bottom, 2.0);
        assert_eq!(bounds.top, 9626.0 / 2048.0);
        assert!(HeightBounds::from_3o(&Model {
            vertices: vec![],
            ..model
        })
        .is_none());
    }
    #[test]
    fn gltf_bounds_apply_full_node_transform_and_reject_invalid_values() {
        let positions = [[1.0, 2.0, 3.0], [-1.0, -2.0, -3.0]];
        let transform = [
            [1.0, 0.0, 2.0, 0.0],
            [0.0, 1.0, -1.0, 0.0],
            [0.0, 0.0, 0.5, 0.0],
            [0.0, 0.0, 4.0, 1.0],
        ];
        let bounds = HeightBounds::from_gltf_base(&[GltfPrimitive {
            positions: &positions,
            transform,
        }])
        .unwrap();
        assert_eq!(
            bounds,
            HeightBounds {
                bottom: 2.5,
                top: 5.5
            }
        );
        let invalid = [[0.0, 0.0, f32::NAN]];
        assert!(HeightBounds::from_gltf_base(&[GltfPrimitive {
            positions: &invalid,
            transform
        }])
        .is_none());
        assert!(HeightBounds::from_gltf_base(&[]).is_none());
    }
    #[test]
    fn unknown_invalid_and_touching_bounds_remain_blocking() {
        let mut c = ObjectHeightCatalog::new();
        assert!(c.overlaps(165, 0.0, 0.0, 0.9));
        assert!(!c.set(
            165,
            HeightBounds {
                bottom: f32::NAN,
                top: 2.0
            }
        ));
        assert!(!c.set(
            120,
            HeightBounds {
                bottom: 1.0,
                top: 2.0
            }
        ));
        assert!(c.set(
            165,
            HeightBounds {
                bottom: 2.0,
                top: 4.7
            }
        ));
        assert!(!c.overlaps(165, 0.0, 0.0, 0.9));
        assert!(c.overlaps(165, -1.1, 0.0, 0.9));
        assert!(c.overlaps(165, 0.0, 2.0, 3.0));
        assert!(c.overlaps(165, f32::NAN, 0.0, 1.0));
        assert!(c.overlaps(166, 0.0, 0.0, 0.9));
    }
}
