mod archive;
mod content;
mod map;
mod model;
mod object_render;
mod objects;
mod planes;
mod process;
mod process_runtime;
mod texture;
mod viewer;
mod world;

use content::{ResourceSet, SourceKind};
use map::{Map, MAP_SIDE, UNITS_PER_TILE};
use std::env;
use std::error::Error;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use world::{Player, Wall, World};

fn usage() {
    eprintln!("Usage: openchasm [--source full|demo|local|PATH] <sources|info|list [PREFIX]|levels|check|objects [LEVEL]|model-info MODEL [ANI]|view [LEVEL]|cat RESOURCE>\n\nUse 'sources' to see available content. Default: local archive, then Steam full, then demo.");
}
fn resource_paths(resources: &ResourceSet) -> Vec<String> {
    resources
        .names()
        .filter(|name| name.starts_with("map/resource."))
        .map(str::to_owned)
        .collect()
}
fn level_id(path: &str) -> &str {
    path.strip_prefix("map/resource.").unwrap_or(path)
}
fn level_name(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .lines()
        .find_map(|line| line.trim().strip_prefix("#name="))
        .unwrap_or("(unnamed)")
        .trim()
        .to_owned()
}
fn select_source(selection: Option<&str>) -> Result<PathBuf, Box<dyn Error>> {
    let sources = content::discover();
    let kind = match selection {
        Some("full") => Some(SourceKind::SteamFull),
        Some("demo") => Some(SourceKind::SteamDemo),
        Some("local") => Some(SourceKind::LocalArchive),
        Some(path) => return Ok(PathBuf::from(path)),
        None => None,
    };
    sources.into_iter().find(|s| kind.is_none() || Some(s.kind) == kind).map(|s| s.path)
        .ok_or_else(|| format!("no matching content source found for {}; run 'openchasm sources' or pass --source PATH", selection.unwrap_or("default")).into())
}
#[derive(Clone, Default, Debug)]
struct WallGraphic {
    filename: Option<String>,
    disable_walk_collision: bool,
}

/// Decode level-specific #GFX rows. Flag slots are g/s/o; only g has
/// movement semantics here. Preserve the default for absent rows.
fn parse_wall_graphics(resource: &[u8]) -> Vec<WallGraphic> {
    let text = String::from_utf8_lossy(resource);
    let mut graphics = vec![WallGraphic::default(); 128];
    let mut in_gfx = false;
    for line in text.lines().map(str::trim) {
        if line.eq_ignore_ascii_case("#gfx") {
            in_gfx = true;
            continue;
        }
        if in_gfx && line.to_ascii_lowercase().starts_with("#end") {
            break;
        }
        if !in_gfx {
            continue;
        }
        let Some((number, value)) = line.split_once(':') else {
            continue;
        };
        let Ok(id) = number.trim().parse::<usize>() else {
            continue;
        };
        let Some(graphic) = graphics.get_mut(id) else {
            continue;
        };
        let mut fields = value.split_whitespace();
        let Some(name) = fields.next() else {
            continue;
        };
        if !name.to_ascii_lowercase().ends_with(".cel") {
            continue;
        }
        graphic.filename = Some(name.to_ascii_lowercase());
        if (87..=119).contains(&id) {
            graphic.disable_walk_collision = fields
                .next()
                .is_some_and(|flags| flags.starts_with('g') || flags.starts_with('G'));
        }
    }
    graphics
}
fn load_wall_textures(
    resources: &mut ResourceSet,
    graphics: &[WallGraphic],
) -> Vec<Option<texture::CelImage>> {
    graphics
        .iter()
        .map(|graphic| {
            let path = format!("texture/{}", graphic.filename.as_ref()?);
            let bytes = resources.read(&path).ok()?;
            texture::CelImage::parse(&bytes).ok()
        })
        .collect()
}
fn make_world(map: &Map, graphics: &[WallGraphic]) -> Result<World, Box<dyn Error>> {
    let spawn = map
        .monsters
        .iter()
        .find(|m| m.is_player_spawn() && m.flags == 0)
        .ok_or("map has no default player spawn")?;
    let convert = |p: [u16; 2]| {
        [
            f32::from(p[0]) / UNITS_PER_TILE,
            f32::from(p[1]) / UNITS_PER_TILE,
        ]
    };
    let mut walls = Vec::new();
    for x in 0..MAP_SIDE {
        for y in 0..MAP_SIDE {
            let wall = map.wall(x, y).expect("valid grid coordinates");
            if wall.is_wall() && wall.start != wall.end {
                walls.push(Wall {
                    cell: Some([x as u8, y as u8]),
                    a: convert(wall.start),
                    b: convert(wall.end),
                    texture: wall.texture_id,
                    blocks_movement: !graphics
                        .get(wall.texture_id as usize)
                        .is_some_and(|g| g.disable_walk_collision),
                });
            }
        }
    }
    if walls.is_empty() {
        return Err("map contains no supported wall geometry".into());
    }
    Ok(World {
        walls,
        player: Player {
            position: convert(spawn.position),
            angle: spawn.direction_radians(),
        },
    })
}
fn run() -> Result<(), Box<dyn Error>> {
    let mut args = env::args().skip(1).peekable();
    let mut selection = None;
    if matches!(
        args.peek().map(String::as_str),
        Some("--source" | "--archive")
    ) {
        args.next();
        selection = Some(
            args.next()
                .ok_or("--source requires a path or source name")?,
        );
    }
    let command = args.next().unwrap_or_else(|| "info".to_owned());
    if command == "--help" || command == "-h" {
        usage();
        return Ok(());
    }
    if command == "sources" {
        if args.next().is_some() {
            return Err("sources takes no arguments".into());
        }
        let sources = content::discover();
        if sources.is_empty() {
            println!("No installed content found. Use --source PATH for an archive or loose content directory.");
        }
        for source in sources {
            println!("{}: {}", source.kind.label(), source.path.display());
        }
        return Ok(());
    }
    let source_path = select_source(selection.as_deref())?;
    let mut resources = ResourceSet::open(Path::new(&source_path))
        .map_err(|e| format!("{}: {e}", source_path.display()))?;
    match command.as_str() {
        "info" if args.peek().is_none() => {
            println!("OpenChasm: experimental geometry viewer (gameplay not implemented)");
            println!("Content: {}", source_path.display());
            println!("Resources: {}", resources.names().count());
            println!("Use 'sources', 'levels', or 'view 01'.");
        }
        "list" => {
            let prefix = args.next().unwrap_or_default();
            if args.next().is_some() {
                return Err("list accepts at most one prefix".into());
            }
            for name in resources.names().filter(|name| name.starts_with(&prefix)) {
                println!("{name}");
            }
        }
        "levels" if args.peek().is_none() => {
            for path in resource_paths(&resources) {
                println!(
                    "{}: {}",
                    level_id(&path),
                    level_name(&resources.read(&path)?)
                );
            }
        }
        "check" if args.peek().is_none() => {
            let paths = resource_paths(&resources);
            if paths.is_empty() {
                return Err("no map/resource.* files found".into());
            }
            for path in &paths {
                let id = level_id(path);
                let map = Map::parse(&resources.read(&format!("map/map.{id}"))?)?;
                let floor = map::Floors::parse(&resources.read(&format!("map/floors.{id}"))?)?;
                let graphic_resource = resources.read(path)?;
                let graphics = parse_wall_graphics(&graphic_resource);
                let world = make_world(&map, &graphics)?;
                let script_bytes = resources.read(&format!("map/process.{id}"))?;
                let script_text = String::from_utf8_lossy(&script_bytes);
                let script = process::Script::parse(&script_text);
                let supported_processes =
                    process_runtime::ProcessRuntime::new(&script, &world).usable_count();
                for diagnostic in &script.diagnostics {
                    eprintln!("process.{id}: {diagnostic}");
                }
                // Exercise parsed grids and the floor texture index, too.
                let _ = (
                    map.floor(0, 0),
                    map.ceiling(0, 0),
                    map.ambient_light(0, 0),
                    map.ambient_sound(0, 0),
                    floor.texture(0),
                    map.lights.len(),
                    map.trailing_data.len(),
                );
                println!(
                    "{id}: {} walls, {} actors, {supported_processes} supported moving processes",
                    world
                        .walls
                        .iter()
                        .filter(|wall| wall.cell.is_some())
                        .count(),
                    map.monsters.len()
                );
            }
            println!("Checked {} maps.", paths.len());
        }
        "view" => {
            let id = args.next().unwrap_or_else(|| "01".to_owned());
            if args.next().is_some() || id.len() != 2 || !id.bytes().all(|b| b.is_ascii_digit()) {
                return Err("view expects a two-digit level ID, for example 01".into());
            }
            let map = Map::parse(&resources.read(&format!("map/map.{id}"))?)?;
            let resource = resources.read(&format!("map/resource.{id}"))?;
            let name = level_name(&resource);
            let graphics = parse_wall_graphics(&resource);
            let textures = load_wall_textures(&mut resources, &graphics);
            let world = make_world(&map, &graphics)?;
            let floors = map::Floors::parse(&resources.read(&format!("map/floors.{id}"))?)?;
            let palette = texture::Palette::parse(&resources.read("chasm2.pal")?)?;
            let planes = planes::PlaneRenderer::new(&map, &floors, &palette);
            let script_data = resources.read(&format!("map/process.{id}"))?;
            let script = process::Script::parse(&String::from_utf8_lossy(&script_data));
            let processes = process_runtime::ProcessRuntime::new(&script, &world);
            let definitions =
                objects::ObjectDefinitions::parse(&resources.read("chasm.inf")?, &resource)?;
            let object_scene =
                object_render::ObjectScene::load(&mut resources, &map, &definitions, &palette);
            viewer::run(
                world,
                textures,
                planes,
                processes,
                object_scene,
                &format!("{id}: {name}"),
            )?;
        }
        "objects" => {
            let id = args.next().unwrap_or_else(|| "01".to_owned());
            if args.next().is_some() || id.len() != 2 || !id.bytes().all(|b| b.is_ascii_digit()) {
                return Err("objects expects a two-digit level ID".into());
            }
            let map = Map::parse(&resources.read(&format!("map/map.{id}"))?)?;
            let inf = resources.read("chasm.inf")?;
            let level_data = resources.read(&format!("map/resource.{id}"))?;
            let definitions = objects::ObjectDefinitions::parse(&inf, &level_data)?;
            let instances = objects::instances(&map);
            for instance in &instances {
                let def = definitions.definition(instance).ok_or_else(|| {
                    format!("missing object definition at {:?}", instance.grid_cell)
                })?;
                println!("{:?} #{} cell {:?} pos {:?} angle {:.3} flags {:02x} difficulty {} extra {} size {}; model {} alternate {} animation {:?}; metadata {:?}", instance.kind, instance.definition_index, instance.grid_cell, instance.position, instance.angle_radians, instance.flags, instance.difficulty_flags, instance.extra, instance.size, def.model_3o_path().unwrap_or_else(|| "(none)".into()), def.model_gltf_path(), def.animation_path(), [def.collision_radius, def.shadow, def.bmp_object, def.bmp_z, def.action_code, def.blow_up, def.break_limit, def.ambient_sound, def.break_sound]);
            }
            println!("{} objects in level {id}.", instances.len());
        }
        "model-info" => {
            let name = args
                .next()
                .ok_or("model-info requires a model resource path")?;
            let model = model::Model::parse(&resources.read(&name)?)?;
            let triangles: usize = model.polygons.iter().map(|p| p.vertex_count() - 2).sum();
            println!(
                "{name}: {} vertices, {} polygons ({triangles} triangles), skin {}x{}",
                model.vertices.len(),
                model.polygons.len(),
                model::SKIN_WIDTH,
                model.skin_height
            );
            if let Some(animation_path) = args.next() {
                let animation = model::Animation::parse(&resources.read(&animation_path)?)?;
                println!(
                    "{animation_path}: {} frames, {} vertices per frame, compatible: {}",
                    animation.frames.len(),
                    animation.vertex_count,
                    animation.matches_model(&model)
                );
            }
            if args.next().is_some() {
                return Err("model-info accepts a model and optional animation".into());
            }
        }
        "cat" => {
            let path = args.next().ok_or("cat requires a resource name")?;
            if args.next().is_some() {
                return Err("cat accepts one resource name".into());
            }
            io::stdout().write_all(&resources.read(&path)?)?;
        }
        _ => {
            usage();
            return Err(format!("invalid command or arguments: {command}").into());
        }
    }
    Ok(())
}
fn main() {
    if let Err(error) = run() {
        eprintln!("openchasm: {error}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod wall_graphics_tests {
    use super::*;
    #[test]
    fn reads_transparent_wall_flags_without_affecting_invisible_or_opaque_walls() {
        let graphics = parse_wall_graphics(b"#GFX\n001: wall.cel gso\n087: grate.cel gs.\n088: fence.cel .s.\n120: hidden.cel gso\n#end.\n089: after.cel gso\n");
        assert_eq!(graphics[87].filename.as_deref(), Some("grate.cel"));
        assert!(graphics[87].disable_walk_collision);
        assert!(!graphics[88].disable_walk_collision);
        assert!(!graphics[1].disable_walk_collision);
        assert!(!graphics[120].disable_walk_collision);
        assert_eq!(graphics[89].filename, None);
    }
}
