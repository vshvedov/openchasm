//! Provisional, client-only world-space sampling of scripted source lights.
//!
//! Inputs come from a headless `LightSourcesSnapshot`; this module owns no
//! simulation state. It does not implement baked map lighting, occlusion,
//! light height, original falloff, or startup blinking. Delay is intentionally
//! ignored until the remaster's blink behavior is understood.
use crate::light_sources::LightSourcesSnapshot;

/// Bound per-sample work for independently authored levels. Excess eligible
/// sources are ignored in the snapshot's process/action iteration order.
pub const MAX_RENDER_LIGHTS: usize = 256;

#[derive(Debug, Clone, Copy)]
struct Light {
    position: [f32; 2],
    radius: f32,
    intensity: f32,
}

#[derive(Debug, Clone, Default)]
pub struct DynamicLightSampler {
    lights: Vec<Light>,
    skipped_for_limit: usize,
}
impl DynamicLightSampler {
    pub fn from_snapshot(snapshot: &LightSourcesSnapshot) -> Self {
        let mut result = Self::default();
        for source in snapshot.active.iter().flat_map(|process| &process.sources) {
            if source
                .position_raw
                .iter()
                .any(|&coordinate| coordinate >= 64 * 256)
                || source.radius_raw == 0
                || source.radius_raw > 511 * 64
                || !source.radius_raw.is_multiple_of(64)
                || source.brightness == 0
            {
                continue;
            }
            if result.lights.len() == MAX_RENDER_LIGHTS {
                result.skipped_for_limit = result.skipped_for_limit.saturating_add(1);
                continue;
            }
            result.lights.push(Light {
                position: source.position_raw.map(|value| f32::from(value) / 256.0),
                radius: f32::from(source.radius_raw) / 256.0,
                intensity: f32::from(source.brightness) / 255.0,
            });
        }
        result
    }
    /// Number of otherwise valid lights dropped at the rendering cap.
    pub fn skipped_for_limit(&self) -> usize {
        self.skipped_for_limit
    }

    /// A bounded additive intensity in 0–1 at a finite world XY coordinate.
    /// Linear radial falloff and saturating addition are visual approximations.
    /// Off-map or non-finite positions have no contribution.
    pub fn sample(&self, position: [f32; 2]) -> f32 {
        if !position
            .iter()
            .all(|v| v.is_finite() && *v >= 0.0 && *v < 64.0)
        {
            return 0.0;
        }
        let mut intensity = 0.0f32;
        for light in &self.lights {
            let dx = position[0] - light.position[0];
            let dy = position[1] - light.position[1];
            let distance_squared = dx * dx + dy * dy;
            if distance_squared < light.radius * light.radius {
                intensity += light.intensity * (1.0 - distance_squared.sqrt() / light.radius);
                if intensity >= 1.0 {
                    return 1.0;
                }
            }
        }
        intensity
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::light_sources::{LightSourceProcessSnapshot, SourceLight};
    fn source(position_raw: [u16; 2], radius_raw: u16, brightness: u8) -> SourceLight {
        SourceLight {
            position_raw,
            radius_raw,
            brightness,
            delay_ms: 100,
        }
    }
    fn snapshot(sources: Vec<SourceLight>) -> LightSourcesSnapshot {
        LightSourcesSnapshot {
            active: vec![LightSourceProcessSnapshot {
                process_id: 1,
                sources,
                elapsed_ticks: 0,
            }],
        }
    }
    #[test]
    fn radius_falloff_overlap_and_invalid_coordinates_are_bounded() {
        let state = snapshot(vec![source([640, 896], 512, 128)]);
        let sampler = DynamicLightSampler::from_snapshot(&state);
        let center = sampler.sample([2.5, 3.5]);
        assert!((center - 128.0 / 255.0).abs() < 1e-6);
        assert!((sampler.sample([3.5, 3.5]) - center / 2.0).abs() < 1e-6);
        assert_eq!(sampler.sample([4.5, 3.5]), 0.0);
        assert_eq!(sampler.sample([f32::NAN, 3.5]), 0.0);
        assert_eq!(sampler.sample([64.0, 3.5]), 0.0);
        let both =
            DynamicLightSampler::from_snapshot(&snapshot(vec![source([640, 896], 512, 255); 2]));
        assert_eq!(both.sample([2.5, 3.5]), 1.0);
    }
    #[test]
    fn skips_invalid_and_bounds_excess_work() {
        let mut sources = vec![
            source([0, 0], 0, 255),
            source([16384, 0], 64, 255),
            source([0, 0], 65, 255),
            source([0, 0], 64, 0),
        ];
        sources.extend(vec![source([128, 128], 256, 1); MAX_RENDER_LIGHTS + 3]);
        let sampler = DynamicLightSampler::from_snapshot(&snapshot(sources));
        assert_eq!(sampler.lights.len(), MAX_RENDER_LIGHTS);
        assert_eq!(sampler.skipped_for_limit(), 3);
        assert_eq!(sampler.sample([0.5, 0.5]), 1.0);
    }
    #[test]
    fn snapshot_reconstruction_tracks_activated_and_removed_sources() {
        use crate::light_sources::LightSources;
        let mut sources = LightSources::default();
        sources
            .activate(7, &[source([128, 128], 256, 255)])
            .unwrap();
        let saved = sources.snapshot();
        sources.deactivate(7);
        assert_eq!(
            DynamicLightSampler::from_snapshot(&sources.snapshot()).sample([0.5, 0.5]),
            0.0
        );
        sources.restore(&saved).unwrap();
        assert_eq!(
            DynamicLightSampler::from_snapshot(&sources.snapshot()).sample([0.5, 0.5]),
            1.0
        );
    }
}
