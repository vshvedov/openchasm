//! Software-rendered geometry viewer. Game rules live outside the window layer.
use crate::audio::Audio;
use crate::object_render::{ActorAnimationMode, CameraView, ObjectScene};
use crate::planes::PlaneRenderer;
use crate::projectile_render::{ProjectileRenderer, ProjectileVisual};
use crate::texture::CelImage;
use crate::weapon_render::{WeaponPose, WeaponRenderer};
use crate::world::{Input, World};
use font8x8::{UnicodeFonts, BASIC_FONTS};
use openchasm::ai::Mode;
use openchasm::combat::FireResult;
use openchasm::mission::MissionEvent;
use openchasm::session::{Session, SessionInput};
use sdl2::{
    event::{Event, WindowEvent},
    keyboard::Scancode,
    mouse::{MouseButton, MouseWheelDirection},
    pixels::PixelFormatEnum,
};
use std::collections::{BTreeSet, HashSet};
use std::error::Error;
use std::time::{Duration, Instant};

const WIDTH: usize = 640;
const HEIGHT: usize = 400;
const STEP: f32 = 1.0 / 60.0;
const MOUSE_SENSITIVITY: f32 = 0.002;
const FRAME_DURATION: Duration = Duration::from_micros(16_667);

#[derive(Default)]
struct RelativeLook {
    focused: bool,
    captured: bool,
    pending_dx: i64,
}
impl RelativeLook {
    fn set_focus(&mut self, focused: bool) {
        self.focused = focused;
        if !focused {
            self.release();
        }
    }
    fn release(&mut self) {
        self.captured = false;
        self.pending_dx = 0;
    }
    fn motion(&mut self, dx: i32) {
        if self.focused && self.captured {
            self.pending_dx = self.pending_dx.saturating_add(i64::from(dx));
        }
    }
    fn take_yaw(&mut self) -> f32 {
        let dx = std::mem::take(&mut self.pending_dx).clamp(-100_000, 100_000);
        dx as f32 * MOUSE_SENSITIVITY
    }
}

/// Return the next owned weapon with ammunition; simulation still validates
/// the requested selection. Repeated wheel steps wrap through eligible slots.
fn cycle_weapon(
    inventory: &openchasm::inventory::Inventory,
    current: usize,
    delta: i32,
) -> Option<usize> {
    let count = inventory.weapons.len();
    if current >= count || delta == 0 {
        return None;
    }
    let eligible = inventory
        .weapons
        .iter()
        .zip(inventory.ammo)
        .filter(|(owned, ammo)| **owned && *ammo > 0)
        .count();
    if eligible == 0 {
        return None;
    }
    let steps = (delta.unsigned_abs() - 1) % eligible as u32 + 1;
    let mut selection = current;
    for _ in 0..steps {
        let mut next = None;
        for offset in 1..=count {
            let index = if delta > 0 {
                (selection + offset) % count
            } else {
                (selection + count - offset) % count
            };
            if inventory.weapons[index] && inventory.ammo[index] > 0 {
                next = Some(index);
                break;
            }
        }
        selection = next?;
    }
    Some(selection)
}

fn fire_feedback(result: Option<FireResult>) -> Option<&'static str> {
    match result {
        Some(FireResult::OutOfAmmo) => Some("OUT OF AMMO"),
        Some(FireResult::Unsupported) => Some("WEAPON NOT AVAILABLE"),
        _ => None,
    }
}

struct ViewerTexture {
    width: usize,
    height: usize,
    pixels: Vec<u32>,
}
impl From<CelImage> for ViewerTexture {
    fn from(image: CelImage) -> Self {
        let pixels = image
            .rgba8(Some(255))
            .chunks(4)
            .map(|p| {
                (u32::from(p[3]) << 24)
                    | (u32::from(p[0]) << 16)
                    | (u32::from(p[1]) << 8)
                    | u32::from(p[2])
            })
            .collect();
        Self {
            width: image.width as usize,
            height: image.height as usize,
            pixels,
        }
    }
}

fn text(frame: &mut [u32], x: usize, y: usize, message: &str) {
    for (i, ch) in message.chars().enumerate() {
        if let Some(glyph) = BASIC_FONTS.get(ch) {
            for (dy, row) in glyph.iter().enumerate() {
                for dx in 0..8 {
                    let px = x + i * 8 + dx;
                    let py = y + dy;
                    if px < WIDTH && py < HEIGHT && row & (1 << dx) != 0 {
                        frame[py * WIDTH + px] = 0xffffff;
                    }
                }
            }
        }
    }
}

struct RenderWorld<'a> {
    world: &'a World,
    elevations: &'a [f32],
    eye_height: f32,
    yaw_offset: f32,
    mines: &'a [openchasm::explosives::Mine],
}

// Authored quake power has no documented unit. Scale linearly up to a small
// visual cap, using fixed-tick periodic motion rather than nondeterministic
// randomness. This changes only presentation, never the simulation player.
fn quake_camera(
    player: crate::world::Player,
    eye_height: f32,
    power: u32,
    tick: u64,
) -> (CameraView, f32) {
    let strength = (power.min(10_000) as f32) / 10_000.0;
    let phase = (tick % 120) as f32;
    let yaw_offset = strength * 0.012 * (phase * 1.7).sin();
    let vertical_offset = strength * 0.012 * (phase * 2.3 + 0.7).sin();
    let mut camera_player = player;
    camera_player.angle += yaw_offset;
    (
        CameraView {
            player: camera_player,
            eye_height: (eye_height + vertical_offset).max(0.0),
        },
        yaw_offset,
    )
}

fn render(
    frame: &mut [u32],
    depth: &mut [f32],
    scene: RenderWorld<'_>,
    textures: &[Option<ViewerTexture>],
    planes: Option<&PlaneRenderer>,
    objects: Option<&ObjectScene>,
) {
    let world = scene.world;
    let mut camera_player = world.player;
    camera_player.angle += scene.yaw_offset;
    let camera = CameraView {
        player: camera_player,
        eye_height: scene.eye_height,
    };
    if let Some(planes) = planes {
        planes.render(frame, WIDTH, HEIGHT, camera);
    } else {
        for (y, row) in frame.chunks_mut(WIDTH).enumerate() {
            row.fill(if y < HEIGHT / 2 { 0x303847 } else { 0x38312b });
        }
    }
    depth.fill(f32::INFINITY);
    let (sin, cos) = camera.player.angle.sin_cos();
    let mut hits = Vec::new();
    for x in 0..WIDTH {
        // Direction is unnormalized, so hit distance is perpendicular to camera plane.
        let camera_x = (2.0 * x as f32 / WIDTH as f32 - 1.0) * 0.66;
        let direction = [cos - sin * camera_x, sin + cos * camera_x];
        world.ray_hits(direction, &mut hits);
        for &(distance, index, along) in &hits {
            let texture = world.walls[index].texture as u32;
            if texture == 120 {
                continue;
            }
            let half = (HEIGHT as f32 * 0.5 / distance).min((HEIGHT * 4) as f32);
            let offset = scene.elevations.get(index).copied().unwrap_or(0.0) / 64.0;
            let origin = HEIGHT as f32 * 0.5 - (2.0 + offset - camera.eye_height) * half;
            let top = origin.max(0.0).min(HEIGHT as f32) as usize;
            let bottom = (origin + half * 2.0).max(0.0).min(HEIGHT as f32) as usize;
            let attenuation = (1.0 / (1.0 + distance * 0.08)).clamp(0.25, 1.0);
            let channel = |base: u32| ((base as f32 * attenuation) as u32).min(255);
            let color = (channel(75 + texture * 37 % 110) << 16)
                | (channel(70 + texture * 23 % 100) << 8)
                | channel(65 + texture * 13 % 90);
            let image = textures.get(texture as usize).and_then(Option::as_ref);
            for y in top..bottom {
                let pixel = if let Some(image) = image {
                    let tx = ((along * image.width as f32) as usize).min(image.width - 1);
                    let ty = (((y as f32 - origin) / (half * 2.0).max(1.0) * image.height as f32)
                        as usize)
                        .min(image.height - 1);
                    let rgb = image.pixels[ty * image.width + tx];
                    if (87..=119).contains(&texture) && rgb >> 24 == 0 {
                        continue;
                    }
                    (channel((rgb >> 16) & 255) << 16)
                        | (channel((rgb >> 8) & 255) << 8)
                        | channel(rgb & 255)
                } else {
                    color
                };
                frame[y * WIDTH + x] = pixel;
                depth[y * WIDTH + x] = distance;
            }
        }
    }
    if let Some(objects) = objects {
        if scene.mines.is_empty() {
            objects.render(frame, depth, WIDTH, HEIGHT, camera);
        } else {
            objects.render_with_mines(frame, depth, WIDTH, HEIGHT, camera, scene.mines);
        }
    }
}

fn draw_header(frame: &mut [u32], label: &str) {
    text(frame, 8, 8, label);
    text(
        frame,
        8,
        20,
        "WASD MOVE  MOUSE TURN/FIRE  WHEEL/1-8 WEAPON  SPACE JUMP  E USE",
    );
    text(
        frame,
        8,
        32,
        "EARLY GAMEPLAY PREVIEW - BEHAVIOR IS PROVISIONAL",
    );
}

// Script state retains decorative and dummy placements that combat visibility omits.
fn level_object_visible(
    session: &Session,
    cell: [usize; 2],
    fallback: bool,
    difficulty_mask: u16,
) -> bool {
    let Some(state) = session
        .scripted_objects
        .as_ref()
        .and_then(|scripted| scripted.states.get(cell))
    else {
        return fallback;
    };
    let active = session
        .objects
        .as_ref()
        .and_then(|combat| combat.is_active(cell));
    scripted_object_visible(state.placement.difficulty_flags, active, difficulty_mask)
}

fn scripted_object_visible(
    difficulty_flags: u16,
    active: Option<bool>,
    difficulty_mask: u16,
) -> bool {
    difficulty_flags & difficulty_mask != 0 && active.unwrap_or(true)
}

fn restore_level_visibility(objects: &mut ObjectScene, session: &Session, difficulty_mask: u16) {
    if let Some(scripted) = &session.scripted_objects {
        for state in scripted.states.iter() {
            let cell = state.placement.grid_cell;
            objects.set_visible(
                cell,
                level_object_visible(session, cell, true, difficulty_mask),
            );
        }
    }
}

pub struct ClientFeedback {
    pub session: Session,
    pub audio: Audio,
    pub weapon_overlay: WeaponRenderer,
    pub projectile_renderer: ProjectileRenderer,
    pub explosion_renderer: Option<crate::explosion_render::ExplosionRenderer>,
    pub difficulty: openchasm::pickups::Difficulty,
}

pub fn run(
    textures: Vec<Option<CelImage>>,
    planes: PlaneRenderer,
    mut objects: ObjectScene,
    feedback: ClientFeedback,
    label: &str,
) -> Result<Option<openchasm::inventory::Inventory>, Box<dyn Error>> {
    let ClientFeedback {
        mut session,
        audio,
        weapon_overlay,
        projectile_renderer,
        mut explosion_renderer,
        difficulty,
    } = feedback;
    let difficulty_mask = crate::difficulty_mask(difficulty);
    let initial_session = session.clone();
    if let Some(targets) = &session.objects {
        for (cell, visible) in targets.visibility() {
            objects.set_visible(cell, visible);
        }
    }
    restore_level_visibility(&mut objects, &session, difficulty_mask);
    for cell in session.game.pickups.hidden_cells() {
        objects.set_visible(cell, false);
    }
    let textures: Vec<Option<ViewerTexture>> = textures
        .into_iter()
        .map(|item| item.map(ViewerTexture::from))
        .collect();
    let sdl = sdl2::init()?;
    let video = sdl.video()?;
    let window = video
        .window(
            "OpenChasm - early gameplay preview",
            WIDTH as u32,
            HEIGHT as u32,
        )
        .resizable()
        .position_centered()
        .build()?;
    let mut canvas = window.into_canvas().software().build()?;
    canvas.set_logical_size(WIDTH as u32, HEIGHT as u32)?;
    let texture_creator = canvas.texture_creator();
    let mut texture = texture_creator.create_texture_streaming(
        PixelFormatEnum::ARGB8888,
        WIDTH as u32,
        HEIGHT as u32,
    )?;
    let mut event_pump = sdl.event_pump()?;
    let mut look = RelativeLook {
        focused: canvas.window().has_input_focus(),
        ..RelativeLook::default()
    };
    let mut held = HashSet::new();
    let mut warned_actor_clips = HashSet::new();
    let mut mouse_fire = false;
    let mut frame = vec![0; WIDTH * HEIGHT];
    let mut depth = vec![f32::INFINITY; WIDTH * HEIGHT];
    let mut last = Instant::now();
    let mut accumulator = 0.0f32;
    let mut animation_tick = 0_u64;
    let mut pending_interact = false;
    let mut pending_jump = false;
    let mut pending_weapon = None;
    let mut shoot_ticks = 0usize;
    let mut current_message: Option<(Vec<String>, f32)> = None;
    let mut shot_feedback: Option<(&str, f32)> = None;
    let mut completed = false;
    let number_keys = [
        Scancode::Num1,
        Scancode::Num2,
        Scancode::Num3,
        Scancode::Num4,
        Scancode::Num5,
        Scancode::Num6,
        Scancode::Num7,
        Scancode::Num8,
    ];
    let mut running = true;
    while running {
        let frame_start = Instant::now();
        let mut restart_requested = false;
        let mut advance_requested = false;
        for event in event_pump.poll_iter() {
            match event {
                Event::Quit { .. } => running = false,
                Event::Window {
                    win_event: WindowEvent::Close,
                    ..
                } => running = false,
                Event::Window {
                    win_event: WindowEvent::FocusGained,
                    ..
                } => look.set_focus(true),
                Event::Window {
                    win_event: WindowEvent::FocusLost,
                    ..
                } => {
                    look.set_focus(false);
                    sdl.mouse().set_relative_mouse_mode(false);
                    held.clear();
                    mouse_fire = false;
                    pending_interact = false;
                    pending_jump = false;
                    pending_weapon = None;
                    accumulator = 0.0;
                }
                Event::KeyDown {
                    scancode: Some(code),
                    repeat,
                    ..
                } if look.focused => {
                    held.insert(code);
                    if !repeat {
                        if code == Scancode::Escape {
                            running = false;
                        } else if code == Scancode::R {
                            restart_requested = true;
                        } else if code == Scancode::Return {
                            advance_requested = true;
                        } else if code == Scancode::E {
                            pending_interact = true;
                        } else if code == Scancode::Space {
                            pending_jump = true;
                        }
                        if let Some(index) = number_keys.iter().position(|&key| key == code) {
                            pending_weapon = Some(index);
                        }
                    }
                }
                Event::KeyUp {
                    scancode: Some(code),
                    ..
                } => {
                    held.remove(&code);
                }
                Event::MouseButtonDown {
                    mouse_btn: MouseButton::Left,
                    ..
                } if look.focused => {
                    if look.captured {
                        mouse_fire = true;
                    } else {
                        // The click that captures the cursor must not also shoot.
                        sdl.mouse().set_relative_mouse_mode(true);
                        look.captured = sdl.mouse().relative_mouse_mode();
                        if !look.captured {
                            eprintln!("relative mouse capture unavailable; use arrow keys to turn");
                            mouse_fire = true;
                        }
                    }
                }
                Event::MouseButtonDown {
                    mouse_btn: MouseButton::Right,
                    ..
                } if look.focused => {
                    pending_jump = true;
                }
                Event::MouseButtonUp {
                    mouse_btn: MouseButton::Left,
                    ..
                } => mouse_fire = false,
                Event::MouseWheel { y, direction, .. } if look.focused && look.captured => {
                    let delta = if direction == MouseWheelDirection::Flipped {
                        y.saturating_neg()
                    } else {
                        y
                    };
                    let current = pending_weapon.unwrap_or(session.selected_weapon);
                    if let Some(next) = cycle_weapon(&session.game.inventory, current, delta) {
                        pending_weapon = Some(next);
                    }
                }
                Event::MouseMotion { xrel, .. } => look.motion(xrel),
                _ => {}
            }
        }
        if !running {
            break;
        }
        let now = Instant::now();
        if look.focused {
            accumulator += now
                .duration_since(last)
                .min(Duration::from_millis(100))
                .as_secs_f32();
        } else {
            accumulator = 0.0;
        }
        last = now;
        let key = |code| if held.contains(&code) { 1.0 } else { 0.0 };
        let movement = Input {
            forward: key(Scancode::W) - key(Scancode::S),
            strafe: key(Scancode::D) - key(Scancode::A),
            turn: key(Scancode::Right) - key(Scancode::Left),
        };
        if restart_requested {
            session = initial_session.clone();
            if let Some(renderer) = &mut explosion_renderer {
                renderer.clear();
            }
            objects.reset_level_objects();
            objects.reset_object_animations();
            for pose in session.processes.objects() {
                objects.set_pose(
                    [usize::from(pose.cell[0]), usize::from(pose.cell[1])],
                    pose.position,
                    pose.angle_radians,
                    pose.elevation_raw,
                );
            }
            if let Some(targets) = &session.objects {
                for (cell, visible) in targets.visibility() {
                    objects.set_visible(cell, visible);
                }
            }
            restore_level_visibility(&mut objects, &session, difficulty_mask);
            for (cell, visible) in session.game.pickups.visibility() {
                objects.set_visible(cell, visible);
            }
            pending_interact = false;
            pending_jump = false;
            pending_weapon = None;
            shoot_ticks = 0;
            animation_tick = 0;
            current_message = None;
            shot_feedback = None;
            completed = false;
            look.pending_dx = 0;
        }
        if completed && advance_requested {
            sdl.mouse().set_relative_mouse_mode(false);
            return Ok(Some(session.game.inventory.clone()));
        }
        let firing = mouse_fire;
        while accumulator >= STEP {
            // Deltas are motion, not velocity: consume exactly once per tick.
            let yaw_delta = look.take_yaw();
            animation_tick = animation_tick.saturating_add(1);
            let old_weapon = session.selected_weapon;
            let events = session.step(SessionInput {
                movement,
                yaw_delta,
                interact: pending_interact,
                jump: pending_jump,
                fire: firing,
                select_weapon: pending_weapon,
            });
            pending_interact = false;
            pending_jump = false;
            pending_weapon = None;
            if session.selected_weapon != old_weapon {
                shot_feedback = None;
            }
            if let Some((_, remaining)) = &mut shot_feedback {
                *remaining -= STEP;
            }
            if shot_feedback.is_some_and(|(_, remaining)| remaining <= 0.0) {
                shot_feedback = None;
            }
            if let Some(message) = fire_feedback(events.shot) {
                shot_feedback = Some((message, 1.5));
            }
            if matches!(events.shot, Some(FireResult::Fired(_))) {
                shot_feedback = None;
            }
            if let Some(renderer) = &mut explosion_renderer {
                renderer.tick();
                if let Err(error) = renderer.consume(&events) {
                    eprintln!("explosion visuals skipped: {error}");
                }
            }
            audio.play_enemy_attacks(
                &events.enemy_attacks,
                &session.actors,
                session.world.player.position,
            );
            let rocket_type = session.game.weapons.0[session.selected_weapon].rocket_type;
            for id in crate::audio::gameplay_sound_ids(&events, rocket_type) {
                audio.play(id);
            }
            if let Some((_, remaining)) = &mut current_message {
                *remaining -= STEP;
            }
            if current_message
                .as_ref()
                .is_some_and(|(_, remaining)| *remaining <= 0.0)
            {
                current_message = None;
            }
            for event in events.mission {
                match event {
                    MissionEvent::ShowMessage(id) => {
                        if let Some(message) = session
                            .mission
                            .as_ref()
                            .and_then(|mission| mission.plan.messages.get(&id))
                        {
                            current_message = Some((
                                message.lines.iter().map(|line| line.text.clone()).collect(),
                                message.duration_seconds.max(0.1),
                            ));
                            audio.play(61);
                        }
                    }
                    MissionEvent::LevelComplete => completed = true,
                }
            }
            let mut affected_cells = BTreeSet::new();
            for transition in &events.object_transitions {
                if !objects.replace_level_object(transition.cell, transition.new_type) {
                    return Err(format!(
                        "committed replacement unavailable at {:?}: type {}",
                        transition.cell, transition.new_type
                    )
                    .into());
                }
                affected_cells.insert(transition.cell);
            }
            for &(cell, destroyed) in &events.object_hits {
                if destroyed {
                    affected_cells.insert(cell);
                }
            }
            for cell in affected_cells {
                // A hit and one or more replacements may occur in one tick.
                // Final authoritative combat state decides visibility.
                let hit_destroyed = events
                    .object_hits
                    .iter()
                    .any(|&(at, destroyed)| at == cell && destroyed);
                let replaced = events
                    .object_transitions
                    .iter()
                    .any(|event| event.cell == cell);
                let fallback = !hit_destroyed || replaced;
                objects.set_visible(
                    cell,
                    level_object_visible(&session, cell, fallback, difficulty_mask),
                );
            }
            for effect in events.process_effects {
                match effect {
                    openchasm::process_runtime::ProcessEffect::PlayAnimation { object_id } => {
                        objects.play_object_animation(object_id);
                    }
                    openchasm::process_runtime::ProcessEffect::ObjectPose {
                        cell,
                        position,
                        angle_radians,
                        elevation_raw,
                    } => {
                        objects.set_pose(
                            [usize::from(cell[0]), usize::from(cell[1])],
                            position,
                            angle_radians,
                            elevation_raw,
                        );
                    }
                    _ => {}
                }
            }
            objects.tick_object_animations(STEP);
            for event in events.pickups {
                objects.set_visible(event.cell, false);
                let sound = match event.pickup {
                    openchasm::inventory::Pickup::Weapon(index) => 51 + index as u8,
                    openchasm::inventory::Pickup::Health => 62,
                    openchasm::inventory::Pickup::Key(_) => 63,
                    _ => 60,
                };
                audio.play(sound);
            }
            shoot_ticks = shoot_ticks.saturating_sub(1);
            if matches!(events.shot, Some(FireResult::Fired(_))) {
                shoot_ticks = 12;
                audio.play(21 + session.selected_weapon as u8);
            }
            for actor in &session.actors.actors {
                let mode = session
                    .ai
                    .mode(actor.source_index)
                    .unwrap_or(Mode::Inactive);
                let animation = match mode {
                    Mode::Inactive | Mode::Idle => ActorAnimationMode::Idle,
                    Mode::Chasing => ActorAnimationMode::Run,
                    Mode::Attacking => ActorAnimationMode::Attack,
                    Mode::Dead => ActorAnimationMode::Death,
                };
                objects.set_actor_pose(
                    actor.source_index,
                    actor.position,
                    actor.angle_radians,
                    mode != Mode::Inactive,
                );
                objects.set_actor_animation(actor.source_index, animation, STEP);
                let attack_frame = session
                    .ai
                    .attack_animation(actor.source_index)
                    .map(|phase| (phase.slot, phase.frame));
                if !objects.set_actor_attack_frame(actor.source_index, attack_frame)
                    && attack_frame.is_some()
                    && warned_actor_clips.insert(actor.source_index)
                {
                    eprintln!("actor {} attack animation is unavailable or inconsistent with simulation metadata", actor.source_index);
                }
            }
            accumulator -= STEP;
        }
        let quake_power = session
            .scripted_objects
            .as_ref()
            .and_then(|state| state.quake.sample(session.world.player.position))
            .unwrap_or(0);
        let (camera, yaw_offset) = quake_camera(
            session.world.player,
            session.vertical.eye(),
            quake_power,
            animation_tick,
        );
        render(
            &mut frame,
            &mut depth,
            RenderWorld {
                world: &session.world,
                elevations: &session.wall_elevations,
                eye_height: camera.eye_height,
                yaw_offset,
                mines: session.mines(),
            },
            &textures,
            Some(&planes),
            Some(&objects),
        );
        let projectile_visuals: Vec<_> = session
            .projectiles
            .active()
            .iter()
            .map(|projectile| ProjectileVisual {
                rocket_type: projectile.rocket_id,
                position: [projectile.position[0], projectile.position[1], 1.0],
                yaw: projectile.velocity[1].atan2(projectile.velocity[0]),
                pitch: 0.0,
            })
            .collect();
        projectile_renderer.render(
            &mut frame,
            &mut depth,
            WIDTH,
            HEIGHT,
            camera,
            &projectile_visuals,
        );
        if let Some(renderer) = &explosion_renderer {
            renderer.render(&mut frame, &depth, WIDTH, HEIGHT, camera);
        }
        let pose = if shoot_ticks > 0 {
            WeaponPose::Shoot
        } else {
            WeaponPose::Idle
        };
        let count = weapon_overlay
            .frame_count(session.selected_weapon, pose)
            .unwrap_or(1)
            .max(1);
        let frame_index = if shoot_ticks > 0 {
            (12 - shoot_ticks) * count / 12
        } else {
            (animation_tick as usize / 6) % count
        };
        weapon_overlay.render(
            &mut frame,
            WIDTH,
            HEIGHT,
            session.selected_weapon,
            pose,
            frame_index,
        );
        draw_header(&mut frame, label);
        let cx = WIDTH / 2;
        let cy = HEIGHT / 2;
        for offset in -3isize..=3 {
            frame[cy * WIDTH + (cx as isize + offset) as usize] = 0xffffff;
            frame[(cy as isize + offset) as usize * WIDTH + cx] = 0xffffff;
        }
        let inventory = &session.game.inventory;
        text(
            &mut frame,
            8,
            HEIGHT - 18,
            &format!(
                "HEALTH {}  ARMOR {}  WEAPON {}  AMMO {}  KEYS {}{}{}",
                inventory.health,
                inventory.armor,
                session.selected_weapon + 1,
                inventory.ammo[session.selected_weapon],
                if inventory.keys[0] { "R" } else { "-" },
                if inventory.keys[1] { "G" } else { "-" },
                if inventory.keys[2] { "B" } else { "-" }
            ),
        );
        if let Some((lines, _)) = &current_message {
            for (row, line) in lines.iter().enumerate().take(6) {
                text(&mut frame, 8, 52 + row * 10, line);
            }
        }
        if let Some((message, _)) = shot_feedback {
            text(&mut frame, 8, HEIGHT - 42, message);
        }
        if completed {
            text(
                &mut frame,
                WIDTH / 2 - 112,
                HEIGHT / 2 + 35,
                "LEVEL COMPLETE - ENTER TO CONTINUE",
            );
        }
        if inventory.health == 0 {
            text(
                &mut frame,
                WIDTH / 2 - 80,
                HEIGHT / 2 + 35,
                "YOU DIED - R TO RESTART",
            );
        }
        if !look.focused {
            text(&mut frame, WIDTH / 2 - 28, HEIGHT / 2 + 50, "PAUSED");
        } else if !look.captured {
            text(&mut frame, 8, HEIGHT - 30, "CLICK TO CAPTURE MOUSE");
        }
        texture.with_lock(None, |bytes, pitch| {
            for (row, pixels) in bytes.chunks_mut(pitch).zip(frame.chunks(WIDTH)) {
                for (dst, &pixel) in row[..WIDTH * 4].chunks_mut(4).zip(pixels) {
                    dst.copy_from_slice(&(pixel | 0xff00_0000).to_ne_bytes());
                }
            }
        })?;
        canvas.clear();
        canvas.copy(&texture, None, None)?;
        canvas.present();
        if let Some(delay) = FRAME_DURATION.checked_sub(frame_start.elapsed()) {
            std::thread::sleep(delay);
        }
    }
    sdl.mouse().set_relative_mouse_mode(false);
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::{Player, Wall};

    #[test]
    fn relative_motion_is_consumed_once_and_cleared_on_focus_loss() {
        let mut look = RelativeLook::default();
        look.motion(100);
        assert_eq!(look.take_yaw(), 0.0);
        look.set_focus(true);
        look.captured = true;
        look.motion(10);
        look.motion(-4);
        assert!((look.take_yaw() - 0.012).abs() < 1e-6);
        assert_eq!(look.take_yaw(), 0.0);
        look.motion(30);
        look.set_focus(false);
        assert!(!look.captured);
        assert_eq!(look.take_yaw(), 0.0);
        look.set_focus(true);
        look.motion(50);
        assert_eq!(look.take_yaw(), 0.0);
    }

    #[test]
    fn composites_masked_walls_over_the_walls_behind_them() {
        let wall = |distance: f32, texture: u8| Wall {
            cell: None,
            a: [distance, -1.0],
            b: [distance, 1.0],
            texture,
            blocks_movement: true,
        };
        let world = World {
            walls: vec![wall(1.0, 87), wall(2.0, 1), wall(0.5, 120)],
            player: Player {
                position: [0.0, 0.0],
                angle: 0.0,
            },
        };
        let mut textures: Vec<Option<ViewerTexture>> = (0..128).map(|_| None).collect();
        textures[1] = Some(ViewerTexture {
            width: 1,
            height: 1,
            pixels: vec![0xffff0000],
        });
        textures[87] = Some(ViewerTexture {
            width: 1,
            height: 2,
            pixels: vec![0x00000000, 0xff0000ff],
        });
        let mut frame = vec![0; WIDTH * HEIGHT];
        let mut depth = vec![f32::INFINITY; WIDTH * HEIGHT];
        render(
            &mut frame,
            &mut depth,
            RenderWorld {
                world: &world,
                elevations: &[],
                eye_height: 1.0,
                yaw_offset: 0.0,
                mines: &[],
            },
            &textures,
            None,
            None,
        );
        let visible_behind = frame[(HEIGHT / 2 - 10) * WIDTH + WIDTH / 2];
        let visible_in_front = frame[(HEIGHT / 2 + 10) * WIDTH + WIDTH / 2];
        assert!(visible_behind & 0xff0000 > 0, "{visible_behind:06x}");
        assert!(visible_in_front & 0x0000ff > 0, "{visible_in_front:06x}");
    }

    #[test]
    fn raising_the_camera_moves_wall_projection_down() {
        let world = World {
            walls: vec![Wall {
                a: [4.0, -1.0],
                b: [4.0, 1.0],
                texture: 1,
                blocks_movement: true,
                cell: None,
            }],
            player: Player {
                position: [0.0, 0.0],
                angle: 0.0,
            },
        };
        let mut frame = vec![0; WIDTH * HEIGHT];
        let mut depth = vec![f32::INFINITY; WIDTH * HEIGHT];
        let top = |depth: &[f32]| {
            (0..HEIGHT)
                .find(|&y| depth[y * WIDTH + WIDTH / 2].is_finite())
                .unwrap()
        };
        render(
            &mut frame,
            &mut depth,
            RenderWorld {
                world: &world,
                elevations: &[],
                eye_height: 0.75,
                yaw_offset: 0.0,
                mines: &[],
            },
            &[],
            None,
            None,
        );
        let lower_camera_top = top(&depth);
        render(
            &mut frame,
            &mut depth,
            RenderWorld {
                world: &world,
                elevations: &[],
                eye_height: 1.15,
                yaw_offset: 0.0,
                mines: &[],
            },
            &[],
            None,
            None,
        );
        assert!(top(&depth) > lower_camera_top + 10);
    }
    #[test]
    fn raised_wall_exposes_space_below_it() {
        let world = World {
            walls: vec![Wall {
                a: [1.0, -1.0],
                b: [1.0, 1.0],
                texture: 1,
                blocks_movement: true,
                cell: None,
            }],
            player: Player {
                position: [0.0, 0.0],
                angle: 0.0,
            },
        };
        let mut textures: Vec<Option<ViewerTexture>> = (0..2).map(|_| None).collect();
        textures[1] = Some(ViewerTexture {
            width: 1,
            height: 1,
            pixels: vec![0xffff0000],
        });
        let mut frame = vec![0; WIDTH * HEIGHT];
        let mut depth = vec![f32::INFINITY; WIDTH * HEIGHT];
        render(
            &mut frame,
            &mut depth,
            RenderWorld {
                world: &world,
                elevations: &[80.0],
                eye_height: 1.0,
                yaw_offset: 0.0,
                mines: &[],
            },
            &textures,
            None,
            None,
        );
        assert_eq!(frame[(HEIGHT / 2) * WIDTH + WIDTH / 2], 0x38312b);
    }
    #[test]
    fn paints_a_textured_wall_at_screen_center() {
        let world = World {
            walls: vec![Wall {
                cell: None,
                a: [1.0, -1.0],
                b: [1.0, 1.0],
                texture: 1,
                blocks_movement: true,
            }],
            player: Player {
                position: [0.0, 0.0],
                angle: 0.0,
            },
        };
        let textures = vec![
            None,
            Some(ViewerTexture {
                width: 1,
                height: 1,
                pixels: vec![0xff0000],
            }),
        ];
        let mut frame = vec![0; WIDTH * HEIGHT];
        let mut depth = vec![f32::INFINITY; WIDTH * HEIGHT];
        render(
            &mut frame,
            &mut depth,
            RenderWorld {
                world: &world,
                elevations: &[],
                eye_height: 1.0,
                yaw_offset: 0.0,
                mines: &[],
            },
            &textures,
            None,
            None,
        );
        let center = frame[(HEIGHT / 2) * WIDTH + WIDTH / 2];
        assert!(center & 0xff0000 > 0);
        assert_eq!(center & 0x00ffff, 0);
    }
    #[test]
    fn quake_camera_is_deterministic_bounded_and_does_not_change_player() {
        let player = Player {
            position: [3.0, 4.0],
            angle: 1.0,
        };
        let (still, offset) = quake_camera(player, 0.75, 0, 13);
        assert_eq!(offset, 0.0);
        assert_eq!(still.player.angle, player.angle);
        assert_eq!(still.eye_height, 0.75);
        for tick in 0..240 {
            let (camera, yaw) = quake_camera(player, 0.75, u32::MAX, tick);
            let (same, same_yaw) = quake_camera(player, 0.75, 10_000, tick);
            assert_eq!(yaw, same_yaw);
            assert_eq!(camera.eye_height, same.eye_height);
            assert!(yaw.abs() <= 0.012);
            assert!((camera.eye_height - 0.75).abs() <= 0.01201);
            assert_eq!(camera.player.position, player.position);
        }
        assert_eq!(player.angle, 1.0);
    }
    #[test]
    fn replacement_visibility_uses_final_combat_state_and_difficulty() {
        // A destroyed object replaced by a live target is shown; a destroyed
        // final target stays hidden, and an unshootable dummy stays represented.
        assert!(scripted_object_visible(7, Some(true), 2));
        assert!(!scripted_object_visible(7, Some(false), 2));
        assert!(scripted_object_visible(7, None, 2));
        assert!(!scripted_object_visible(1, Some(true), 2));
        assert!(!scripted_object_visible(1, None, 2));
        assert!(scripted_object_visible(1, None, 1));
        assert!(!scripted_object_visible(1, None, 4));
    }
    #[test]
    fn wheel_cycles_owned_nonempty_weapons_and_wraps() {
        let inventory = openchasm::inventory::Inventory {
            health: 100,
            armor: 0,
            weapons: [true, true, false, true, false, false, false, true],
            ammo: [3, 0, 9, 2, 0, 0, 0, 1],
            keys: [false; 3],
        };
        assert_eq!(cycle_weapon(&inventory, 0, 1), Some(3));
        assert_eq!(cycle_weapon(&inventory, 0, -1), Some(7));
        assert_eq!(cycle_weapon(&inventory, 0, 2), Some(7));
        assert_eq!(cycle_weapon(&inventory, 7, 1), Some(0));
        assert_eq!(cycle_weapon(&inventory, 0, 9), Some(0));
        assert_eq!(cycle_weapon(&inventory, 0, 0), None);
        assert_eq!(cycle_weapon(&inventory, 8, 1), None);
        let mut empty = inventory;
        empty.ammo = [0; 8];
        assert_eq!(cycle_weapon(&empty, 0, 1), None);
    }

    #[test]
    fn feedback_reports_failures_without_treating_cooldown_as_failure() {
        assert_eq!(
            fire_feedback(Some(FireResult::OutOfAmmo)),
            Some("OUT OF AMMO")
        );
        assert_eq!(
            fire_feedback(Some(FireResult::Unsupported)),
            Some("WEAPON NOT AVAILABLE")
        );
        assert_eq!(fire_feedback(Some(FireResult::CoolingDown)), None);
        assert_eq!(fire_feedback(None), None);
    }
}
