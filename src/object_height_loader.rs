//! Client-side loading of conservative base-geometry collision heights.
//!
//! The resulting shared catalog uses raw level-object IDs and remains usable
//! by a headless simulation; this module alone depends on client asset readers.
use crate::content::ResourceSet;
use crate::gltf_model::GltfModel;
use openchasm::model::Model;
use openchasm::object_heights::{GltfPrimitive, HeightBounds, ObjectHeightCatalog};
use openchasm::objects::{ObjectDefinition, ObjectDefinitions};
use std::collections::{BTreeSet, HashMap};

fn model_path(definition: &ObjectDefinition) -> Option<(String, bool)> {
    let name = &definition.model_name;
    if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\\']) {
        return None;
    }
    if let Some(path) = definition.model_3o_path() {
        Some((path, false))
    } else {
        Some((definition.model_gltf_path(), true))
    }
}

/// Load base bounds once per unique model, including definitions that may be
/// selected by a later scripted replacement. Missing/invalid assets remain
/// absent, so collision continues to conservatively treat them as overlapping.
#[allow(dead_code)] // The client opts in after simulation support is connected.
pub fn load_object_heights(
    resources: &mut ResourceSet,
    definitions: &ObjectDefinitions,
) -> ObjectHeightCatalog {
    let mut catalog = ObjectHeightCatalog::new();
    let mut cache: HashMap<String, Option<HeightBounds>> = HashMap::new();
    let mut warnings = BTreeSet::new();
    for (index, definition) in definitions.level_objects.iter().take(64).enumerate() {
        let Some((path, gltf)) = model_path(definition) else {
            warnings.insert(format!(
                "invalid object model name {:?}",
                definition.model_name
            ));
            continue;
        };
        let bounds = *cache.entry(path.clone()).or_insert_with(|| {
            let result = if gltf {
                GltfModel::load(resources, &path).ok().and_then(|model| {
                    if model.primitives.is_empty()
                        || model
                            .primitives
                            .iter()
                            .any(|primitive| primitive.positions.is_empty())
                    {
                        return None;
                    }
                    let primitives: Vec<_> = model
                        .primitives
                        .iter()
                        .map(|primitive| GltfPrimitive {
                            positions: &primitive.positions,
                            transform: primitive.node_transform,
                        })
                        .collect();
                    HeightBounds::from_gltf_base(&primitives)
                })
            } else {
                resources
                    .read(&path)
                    .ok()
                    .and_then(|bytes| Model::parse(&bytes).ok())
                    .and_then(|model| HeightBounds::from_3o(&model))
            };
            if result.is_none() {
                warnings.insert(format!(
                    "{path}: no valid base geometry; using conservative collision"
                ));
            }
            result
        });
        if let Some(bounds) = bounds {
            catalog.set(163 + index as u8, bounds);
        }
    }
    for warning in warnings {
        eprintln!("object height: {warning}");
    }
    catalog
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};
    fn definition(name: &str) -> ObjectDefinition {
        ObjectDefinition {
            collision_radius: 0,
            shadow: 0,
            bmp_object: 0,
            bmp_z: 0,
            action_code: 0,
            blow_up: 0,
            break_limit: 0,
            ambient_sound: 0,
            break_sound: 0,
            model_name: name.into(),
            animation_name: None,
        }
    }
    #[test]
    fn duplicates_are_supported_and_missing_or_invalid_geometry_stays_conservative() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("openchasm-heights-{}-{stamp}", std::process::id()));
        std::fs::create_dir_all(dir.join("model")).unwrap();
        let mut bytes = vec![0; 0x4806];
        bytes[0x4800..0x4802].copy_from_slice(&2_u16.to_le_bytes());
        bytes[0x3204..0x3206].copy_from_slice(&4096_i16.to_le_bytes());
        bytes[0x320a..0x320c].copy_from_slice(&8192_i16.to_le_bytes());
        std::fs::write(dir.join("model/roof.3o"), bytes).unwrap();
        std::fs::write(dir.join("model/bad.3o"), b"bad").unwrap();
        let definitions = ObjectDefinitions {
            items: vec![],
            level_objects: vec![
                definition("roof.3o"),
                definition("roof.3o"),
                definition("missing.3o"),
                definition("bad.3o"),
                definition("../bad.3o"),
            ],
        };
        let mut resources = ResourceSet::open(&dir).unwrap();
        let catalog = load_object_heights(&mut resources, &definitions);
        assert_eq!(catalog.get(163), HeightBounds::new(2.0, 4.0));
        assert_eq!(catalog.get(164), catalog.get(163));
        assert!(!catalog.overlaps(163, 0.0, 0.0, 0.9));
        for id in 165..=167 {
            assert_eq!(catalog.get(id), None);
            assert!(catalog.overlaps(id, 0.0, 0.0, 0.9));
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn installed_level17_roof_and_modern_gltf_models_load_when_available() {
        let explicit = std::env::var_os("OPENCHASM_TEST_CONTENT");
        let path = explicit
            .as_ref()
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from("assets/csm.bin"));
        if !path.exists() && explicit.is_none() {
            return;
        }
        let mut resources = ResourceSet::open(&path).unwrap();
        let inf = resources.read("chasm.inf").unwrap();
        let definitions =
            ObjectDefinitions::parse(&inf, &resources.read("map/resource.17").unwrap()).unwrap();
        assert_eq!(definitions.level_objects[2].model_name, "18_cl2.3o");
        let catalog = load_object_heights(&mut resources, &definitions);
        let bounds = catalog.get(165).expect("roof bounds");
        assert_eq!(bounds.bottom, 2.0);
        assert_eq!(bounds.top, 9626.0 / 2048.0);
        assert!(!catalog.overlaps(165, 0.0, 0.0, 0.9));
        let definitions =
            ObjectDefinitions::parse(&inf, &resources.read("map/resource.10").unwrap()).unwrap();
        assert_eq!(definitions.level_objects[33].model_name, "bridge_2");
        let catalog = load_object_heights(&mut resources, &definitions);
        assert!(catalog.get(196).is_some());
    }
}
