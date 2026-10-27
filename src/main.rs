mod archive;
mod audio;
mod content;
mod gltf_model;
mod object_render;
mod planes;
mod projectile_render;
mod texture;
mod viewer;
mod weapon_render;

use content::{ResourceSet, SourceKind};
use map::{Map, MAP_SIDE, UNITS_PER_TILE};
use openchasm::{actors, map, model, monster, objects, process, process_runtime, world};
use std::env;
use std::error::Error;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use world::{Player, Wall, World};

fn usage() {
    eprintln!("Usage: openchasm [--source full|demo|local|PATH] <sources|info|list [PREFIX]|levels|check|objects [LEVEL]|gltf-info RESOURCE|model-info MODEL [ANI]|view [LEVEL]|cat RESOURCE>\n\nUse 'sources' to see available content. Default: local archive, then Steam full, then demo.");
}
fn resource_paths(resources: &ResourceSet) -> Vec<String> {
    resources
        .names()
        .filter(|name| name.starts_with("map/resource."))
        .map(str::to_owned)
        .collect()
}
fn next_campaign_level(id: &str) -> Option<String> {
    let level = id.parse::<u8>().ok()?;
    (1..19)
        .contains(&level)
        .then(|| format!("{:02}", level + 1))
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
    disable_shoot_collision: bool,
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
            if let Some(flags) = fields.next() {
                let flags = flags.to_ascii_lowercase();
                graphic.disable_walk_collision = flags.starts_with('g');
                graphic.disable_shoot_collision = flags.as_bytes().get(1) == Some(&b's');
            }
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
/// Provisional circle for represented level objects. Collectibles remain
/// nonblocking so their existing pickup overlap rules can collect them.
fn process_blocking_radius(kind: objects::ObjectKind, action_code: i32, radius: i32) -> f32 {
    // Pickups::new recognizes keys as collectible level objects; ordinary
    // pickups use the separate Item kind.
    let collectible = (141..=143).contains(&action_code);
    if kind == objects::ObjectKind::LevelObject && !collectible {
        (radius.max(0) as f32 / 256.0).min(2.0)
    } else {
        0.0
    }
}

fn raw_cells(map: &Map) -> [u8; 4096] {
    std::array::from_fn(|index| {
        map.wall(index / MAP_SIDE, index % MAP_SIDE)
            .expect("valid map cell")
            .texture_id
    })
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
            println!("OpenChasm: experimental gameplay preview (campaign incomplete)");
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
                let cells = raw_cells(&map);
                let supported_processes = process_runtime::ProcessRuntime::with_cell_data(
                    &script,
                    &world,
                    process_runtime::ProcessCapabilities {
                        wall_elevation: true,
                        initial_cell_types: None,
                    },
                    &cells,
                )
                .usable_count();
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
            let mut id = args.next().unwrap_or_else(|| "01".to_owned());
            if args.next().is_some() || id.len() != 2 || !id.bytes().all(|b| b.is_ascii_digit()) {
                return Err("view expects a two-digit level ID, for example 01".into());
            }
            let mut carry: Option<openchasm::inventory::Inventory> = None;
            loop {
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
                let cells = raw_cells(&map);
                let definitions =
                    objects::ObjectDefinitions::parse(&resources.read("chasm.inf")?, &resource)?;
                let mut object_scene =
                    object_render::ObjectScene::load(&mut resources, &map, &definitions, &palette);
                let actor_definitions =
                    actors::ActorDefinitions::parse(&resources.read("chasm.inf")?)?;
                let actor_state =
                    actors::ActorState::from_placements(&map.monsters, &actor_definitions);
                object_scene.add_actors(&mut resources, &actor_state, &actor_definitions);
                let instances = objects::instances(&map);
                // Only opt in for visible, difficulty-enabled object placements
                // with a representable render instance; other targets stay closed.
                let process_objects: Vec<_> = instances
                    .iter()
                    .filter_map(|instance| {
                        if instance.difficulty_flags & 2 == 0
                            || !object_scene.has_cell(instance.grid_cell)
                        {
                            return None;
                        }
                        let definition = definitions.definition(instance)?;
                        let x = u8::try_from(instance.grid_cell[0]).ok()?;
                        let y = u8::try_from(instance.grid_cell[1]).ok()?;
                        // GoRad shapes are provisionally approximated as circles;
                        // pickups must remain nonblocking so they can be collected.
                        let blocking_radius = process_blocking_radius(
                            instance.kind,
                            definition.action_code,
                            definition.collision_radius,
                        );
                        Some(process_runtime::ProcessObject {
                            cell: [x, y],
                            position: instance.position,
                            angle_radians: instance.angle_radians,
                            elevation_raw: 0.0,
                            blocking_radius,
                        })
                    })
                    .collect();
                let processes = process_runtime::ProcessRuntime::with_wind(
                    &script,
                    &world,
                    process_runtime::ProcessCapabilities {
                        wall_elevation: true,
                        initial_cell_types: None,
                    },
                    &cells,
                    &process_objects,
                )?;
                let info = resources.read("chasm.inf")?;
                let weapons = openchasm::inventory::WeaponDefinitions::parse(&info)?;
                let mut audio = audio::Audio::new(&mut resources, &info);
                audio.play_level_music(&mut resources, &resource);
                let weapon_overlay =
                    weapon_render::WeaponRenderer::load(&mut resources, &info, &palette)?;
                let mut game = openchasm::game::GameInventory::new(
                    weapons,
                    &instances,
                    &definitions,
                    openchasm::pickups::Difficulty::Normal,
                );
                if let Some(mut inventory) = carry.take() {
                    inventory.keys = [false; 3];
                    game.inventory = inventory;
                }
                let rockets = openchasm::combat::RocketDefinitions::parse(&info)?;
                let projectile_renderer =
                    projectile_render::ProjectileRenderer::load(&mut resources, &rockets, &palette);
                let shot_blocking =
                    std::array::from_fn(|index| !graphics[index].disable_shoot_collision);
                let mut session =
                    openchasm::session::Session::new(openchasm::session::LevelSetup {
                        world,
                        processes,
                        actors: actor_state,
                        actor_definitions,
                        game,
                        rockets,
                        shot_blocking,
                        difficulty: openchasm::pickups::Difficulty::Normal,
                    });
                session.set_objects(openchasm::object_combat::ObjectCombat::new(
                    &instances,
                    &definitions,
                    &script,
                    openchasm::pickups::Difficulty::Normal,
                ));
                session.set_mission(openchasm::mission::MissionPlan::parse(
                    &script,
                    &String::from_utf8_lossy(&script_data),
                ));
                let next_level = viewer::run(
                    textures,
                    planes,
                    object_scene,
                    viewer::ClientFeedback {
                        session,
                        audio,
                        weapon_overlay,
                        projectile_renderer,
                    },
                    &format!("{id}: {name}"),
                )?;
                if let (Some(inventory), Some(next)) = (next_level, next_campaign_level(&id)) {
                    carry = Some(inventory);
                    id = next;
                } else {
                    break;
                }
            }
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
        "gltf-info" => {
            let path = args
                .next()
                .ok_or("gltf-info requires a glTF resource path")?;
            if args.next().is_some() {
                return Err("gltf-info accepts one resource path".into());
            }
            let model = gltf_model::GltfModel::load(&mut resources, &path)?;
            let vertices: usize = model
                .primitives
                .iter()
                .filter_map(|primitive| primitive.frame_positions(0))
                .map(|frame| frame.len())
                .sum();
            println!(
                "{path}: {} primitives, {vertices} vertices, {} images, {} frames",
                model.primitives.len(),
                model.images.len(),
                model.frame_count
            );
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
    fn process_object_collision_excludes_pickups_and_preserves_authored_radius() {
        use objects::ObjectKind::{Item, LevelObject};
        for code in [141, 142, 143] {
            assert_eq!(
                process_blocking_radius(LevelObject, code, 64),
                0.0,
                "{code}"
            );
        }
        for code in [1, 10, 102, 120, 124, 128] {
            assert_eq!(
                process_blocking_radius(LevelObject, code, 64),
                0.25,
                "{code}"
            );
        }
        assert_eq!(process_blocking_radius(Item, 0, 64), 0.0);
        assert_eq!(process_blocking_radius(LevelObject, 0, 64), 0.25);
        assert_eq!(process_blocking_radius(LevelObject, 0, -1), 0.0);
    }
    #[test]
    fn campaign_sequence_stops_after_last_map() {
        assert_eq!(next_campaign_level("01").as_deref(), Some("02"));
        assert_eq!(next_campaign_level("18").as_deref(), Some("19"));
        assert_eq!(next_campaign_level("19"), None);
        assert_eq!(next_campaign_level("99"), None);
        assert_eq!(next_campaign_level("invalid"), None);
    }
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
