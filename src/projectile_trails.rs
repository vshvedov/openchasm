//! Provisional client-only projectile trails. The independent reference emits
//! one authored BLOWS sprite per half tile of 3D travel. Timing and sprite
//! dynamics use the existing provisional world-effect renderer.
use std::collections::{BTreeMap, HashMap};
use std::io::{self, ErrorKind};

use crate::content::ResourceSet;
use crate::explosion_render::ExplosionRenderer;
use crate::object_render::CameraView;
use crate::texture::Palette;
use openchasm::combat::RocketDefinitions;

const MAX_TYPES: usize = 1024;
const MAX_TRACKS: usize = 4096;
const MAX_EMISSIONS_PER_SEGMENT: usize = 256;

#[derive(Debug)]
struct Blow {
    half_size: bool,
    looping: bool,
    gravity: bool,
    bounce: bool,
    path: String,
}

fn parse_blows(inf: &[u8]) -> io::Result<Vec<Blow>> {
    let invalid = || io::Error::new(ErrorKind::InvalidData, "invalid [BLOWS] section");
    let text = std::str::from_utf8(inf).map_err(|_| invalid())?;
    let mut lines = text
        .lines()
        .map(|line| line.split(';').next().unwrap_or("").trim());
    if !lines
        .by_ref()
        .any(|line| line.eq_ignore_ascii_case("[BLOWS]"))
    {
        return Err(invalid());
    }
    let count: usize = lines
        .by_ref()
        .find(|line| !line.is_empty())
        .ok_or_else(invalid)?
        .split_whitespace()
        .next()
        .ok_or_else(invalid)?
        .parse()
        .map_err(|_| invalid())?;
    if count > MAX_TYPES {
        return Err(invalid());
    }
    let mut blows = Vec::with_capacity(count);
    for line in lines.filter(|line| !line.is_empty()) {
        if blows.len() == count {
            break;
        }
        if line.starts_with('[') {
            break;
        }
        let fields: Vec<_> = line.split_whitespace().collect();
        if fields.len() != 8 {
            return Err(invalid());
        }
        let values: Vec<i32> = fields[..7]
            .iter()
            .map(|v| v.parse().map_err(|_| invalid()))
            .collect::<io::Result<_>>()?;
        let name = fields[7].replace('\\', "/");
        let path = if name.contains('/') {
            name
        } else {
            format!("sprite_effects/{name}")
        };
        if path.starts_with('/')
            || path.split('/').any(|p| p == ".." || p.is_empty())
            || path.contains(':')
        {
            return Err(invalid());
        }
        blows.push(Blow {
            half_size: values[1] != 0,
            looping: values[3] != 0,
            gravity: values[4] != 0,
            bounce: values[5] != 0,
            path,
        });
    }
    if blows.len() != count {
        return Err(invalid());
    }
    Ok(blows)
}

struct TrailSprite {
    renderer: ExplosionRenderer,
    looping: bool,
    gravity: bool,
    bounce: bool,
}

pub struct ProjectileTrails {
    // BTreeMap makes rendering and fallback deterministic.
    sprites: BTreeMap<usize, TrailSprite>,
    rocket_effects: Vec<Option<usize>>,
    distances: HashMap<u64, f32>,
}
impl ProjectileTrails {
    /// Asset failures are reported, and leave other effect types usable.
    pub fn load(
        resources: &mut ResourceSet,
        palette: &Palette,
        rockets: &RocketDefinitions,
    ) -> io::Result<Self> {
        let blows = parse_blows(&resources.read("chasm.inf")?)?;
        let rocket_effects: Vec<_> = rockets
            .0
            .iter()
            .map(|rocket| {
                rocket
                    .raw_fields
                    .get(11)
                    .and_then(|id| usize::try_from(*id).ok())
                    .filter(|id| *id != 0 && *id < blows.len())
            })
            .collect();
        let mut sprites = BTreeMap::new();
        for id in rocket_effects.iter().flatten().copied() {
            if sprites.contains_key(&id) {
                continue;
            }
            let blow = &blows[id];
            match ExplosionRenderer::load_sprite(resources, palette, &blow.path) {
                Ok(mut renderer) => {
                    renderer.set_half_size(blow.half_size);
                    sprites.insert(
                        id,
                        TrailSprite {
                            renderer,
                            looping: blow.looping,
                            gravity: blow.gravity,
                            bounce: blow.bounce,
                        },
                    );
                }
                Err(error) => {
                    eprintln!("projectile trail sprite {} unavailable: {error}", blow.path)
                }
            }
        }
        Ok(Self {
            sprites,
            rocket_effects,
            distances: HashMap::new(),
        })
    }
    pub fn tick(&mut self) {
        for sprite in self.sprites.values_mut() {
            sprite.renderer.tick();
        }
    }
    /// Add the actual committed motion segment, in 3D map tiles. Spacing is
    /// accumulated per ID across ticks, including the final impact segment.
    pub fn segment(&mut self, id: u64, rocket_id: usize, from: [f32; 3], to: [f32; 3]) {
        let Some(effect) = self.rocket_effects.get(rocket_id).copied().flatten() else {
            return;
        };
        let Some(sprite) = self.sprites.get_mut(&effect) else {
            return;
        };
        if !from.iter().chain(to.iter()).all(|v| v.is_finite()) {
            return;
        }
        let delta: [f32; 3] = std::array::from_fn(|i| to[i] - from[i]);
        let length = delta.iter().map(|v| v * v).sum::<f32>().sqrt();
        if !length.is_finite() || length <= 0.0 {
            return;
        }
        if !self.distances.contains_key(&id) && self.distances.len() >= MAX_TRACKS {
            return;
        }
        let remainder = self.distances.entry(id).or_insert(0.0);
        let mut offset = 0.5 - *remainder;
        let mut count = 0;
        while offset <= length && count < MAX_EMISSIONS_PER_SEGMENT {
            let fraction = offset / length;
            let position = std::array::from_fn(|i| from[i] + delta[i] * fraction);
            sprite.renderer.spawn_particle(
                position,
                [0.0; 3],
                sprite.gravity,
                sprite.bounce,
                sprite.looping,
            );
            count += 1;
            offset += 0.5;
        }
        *remainder = (*remainder + length).rem_euclid(0.5);
    }
    pub fn finish(&mut self, id: u64) {
        self.distances.remove(&id);
    }
    pub fn clear(&mut self) {
        self.distances.clear();
        for sprite in self.sprites.values_mut() {
            sprite.renderer.clear();
        }
    }
    pub fn render_to<C: crate::color_pipeline::RenderPixel>(
        &self,
        frame: &mut [C],
        depth: &[f32],
        width: usize,
        height: usize,
        camera: CameraView,
    ) {
        for sprite in self.sprites.values() {
            sprite
                .renderer
                .render_to(frame, depth, width, height, camera);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openchasm::combat::RocketDefinition;
    #[test]
    fn spacing_persists_across_segments_and_terminal_motion() {
        let dir = std::env::temp_dir().join(format!("openchasm-trails-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sprite_effects")).unwrap();
        std::fs::write(
            dir.join("chasm.inf"),
            b"[BLOWS]\n2\n0 0 0 0 0 0 0 empty.obj\n0 1 0 0 0 0 0 smoke.obj\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("sprite_effects/smoke.obj"),
            [1, 0, 1, 0, 1, 0, 0, 0, 1],
        )
        .unwrap();
        let mut resources = ResourceSet::open(&dir).unwrap();
        let palette = Palette::parse(&[0; 768]).unwrap();
        let mut fields = vec![0; 12];
        fields[11] = 1;
        let rockets = RocketDefinitions(vec![RocketDefinition {
            model_name: "a.3o".into(),
            animation_name: String::new(),
            power: 0,
            raw_fields: fields,
        }]);
        let mut trails = ProjectileTrails::load(&mut resources, &palette, &rockets).unwrap();
        trails.segment(7, 0, [0., 0., 0.], [0.3, 0., 0.]);
        assert_eq!(trails.sprites[&1].renderer.effect_count(), 0);
        trails.segment(7, 0, [0.3, 0., 0.], [0.8, 0., 0.]);
        assert_eq!(trails.sprites[&1].renderer.effect_count(), 1);
        trails.segment(7, 0, [0.8, 0., 0.], [1.2, 0., 0.]);
        assert_eq!(trails.sprites[&1].renderer.effect_count(), 2);
        let positions = trails.sprites[&1].renderer.effect_positions();
        assert!((positions[0][0] - 0.5).abs() < 1e-6);
        assert!((positions[1][0] - 1.0).abs() < 1e-6);
        trails.finish(7);
        assert!(trails.distances.is_empty());
        trails.clear();
        assert_eq!(trails.sprites[&1].renderer.effect_count(), 0);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn blows_parser_rejects_unsafe_or_incomplete_data() {
        assert!(parse_blows(b"[BLOWS]\n1\n0 0 0 0 0 0 0 ../x\n").is_err());
        assert!(parse_blows(b"[BLOWS]\n2\n0 0 0 0 0 0 0 x\n").is_err());
    }
    #[test]
    fn installed_trail_assets_load_when_available() {
        let explicit = std::env::var_os("OPENCHASM_TEST_CONTENT");
        let path = explicit
            .as_ref()
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| "assets/csm.bin".into());
        if !path.exists() && explicit.is_none() {
            return;
        }
        let mut resources = ResourceSet::open(&path).unwrap();
        let inf = resources.read("chasm.inf").unwrap();
        let rockets = RocketDefinitions::parse(&inf).unwrap();
        let palette = Palette::parse(&resources.read("chasm2.pal").unwrap()).unwrap();
        let trails = ProjectileTrails::load(&mut resources, &palette, &rockets).unwrap();
        assert!(trails.rocket_effects.iter().any(Option::is_some));
        assert!(!trails.sprites.is_empty());
    }
}
