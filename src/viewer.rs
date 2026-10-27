//! Software-rendered geometry viewer. Game rules live outside the window layer.
//!
//! F5/F9 provide a same-level in-memory quicksave only. The snapshot includes
//! simulation and scene instance/animation state; transient explosion sprites
//! and one-shot sounds are cleared on load; music continues. No data is
//! written to disk; the checkpoint is lost on leaving the viewer/level.
use crate::audio::{Audio, PlayerSound};
use crate::object_render::{ActorAnimationMode, CameraView, ObjectScene, ObjectSceneSnapshot};
use crate::planes::PlaneRenderer;
use crate::projectile_render::{ProjectileRenderer, ProjectileVisual};
use crate::texture::CelImage;
use crate::weapon_render::{WeaponAnimationClock, WeaponPose, WeaponRenderer};
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

fn wheel_weapon(
    look: &RelativeLook,
    inventory: &openchasm::inventory::Inventory,
    current: usize,
    delta: i32,
) -> Option<usize> {
    if look.focused {
        cycle_weapon(inventory, current, delta)
    } else {
        None
    }
}

fn held_movement(held: &HashSet<Scancode>) -> Input {
    let key = |code| if held.contains(&code) { 1.0 } else { 0.0 };
    Input {
        forward: key(Scancode::W) - key(Scancode::S),
        strafe: key(Scancode::D) - key(Scancode::A),
        turn: key(Scancode::Right) - key(Scancode::Left),
    }
}

// Clear held action state on restart/load without changing mouse capture.
fn clear_held_controls(
    held: &mut HashSet<Scancode>,
    fire: &mut bool,
    look: &mut RelativeLook,
    accumulator: &mut f32,
) {
    held.clear();
    *fire = false;
    look.pending_dx = 0;
    *accumulator = 0.0;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PauseAction {
    Resume,
    Save,
    Load,
    Restart,
    Exit,
}

#[derive(Default)]
struct PauseMenu {
    open: bool,
    selected: usize,
    confirm: Option<PauseAction>,
}
impl PauseMenu {
    fn show(&mut self, confirm: Option<PauseAction>) {
        self.open = true;
        self.confirm = confirm;
        self.selected = 0;
    }
    fn key(&mut self, code: Scancode) -> Option<PauseAction> {
        if let Some(action) = self.confirm {
            match code {
                Scancode::Escape => self.confirm = None,
                Scancode::Return => {
                    self.confirm = None;
                    return Some(action);
                }
                _ => {}
            }
            return None;
        }
        match code {
            Scancode::Escape => return Some(PauseAction::Resume),
            Scancode::Up => self.selected = (self.selected + 4) % 5,
            Scancode::Down => self.selected = (self.selected + 1) % 5,
            Scancode::F5 => return Some(PauseAction::Save),
            Scancode::F9 => return Some(PauseAction::Load),
            Scancode::Return => match self.selected {
                0 => return Some(PauseAction::Resume),
                1 => return Some(PauseAction::Save),
                2 => return Some(PauseAction::Load),
                3 => self.confirm = Some(PauseAction::Restart),
                _ => self.confirm = Some(PauseAction::Exit),
            },
            _ => {}
        }
        None
    }

    fn event(&mut self, event: &Event, focused: bool) -> Option<PauseAction> {
        match event {
            Event::KeyDown {
                scancode: Some(code),
                repeat: false,
                ..
            } if focused => self.key(*code),
            _ => None,
        }
    }
}

fn simulation_active(focused: bool, menu_open: bool, suppress_gameplay: bool) -> bool {
    focused && !menu_open && !suppress_gameplay
}

fn draw_pause_menu(frame: &mut [u32], menu: &PauseMenu) {
    for y in 85..315 {
        for x in 125..515 {
            frame[y * WIDTH + x] = 0x101820;
        }
    }
    text(frame, 155, 100, "PAUSED");
    if let Some(action) = menu.confirm {
        let question = if action == PauseAction::Restart {
            "RESTART LEVEL?"
        } else {
            "EXIT LEVEL?"
        };
        text(frame, 155, 145, question);
        text(frame, 155, 165, "UNSAVED PROGRESS MAY BE LOST");
        text(frame, 155, 185, "ENTER CONFIRMS   ESC CANCELS");
    } else {
        for (index, label) in [
            "RESUME",
            "MEMORY QUICK SAVE",
            "MEMORY QUICK LOAD",
            "RESTART LEVEL",
            "EXIT LEVEL",
        ]
        .iter()
        .enumerate()
        {
            text(
                frame,
                155,
                130 + index * 25,
                if menu.selected == index { ">" } else { " " },
            );
            text(frame, 175, 130 + index * 25, label);
        }
        text(frame, 155, 280, "CHECKPOINT IS MEMORY ONLY");
        text(frame, 155, 295, "UP/DOWN  ENTER  ESC TO RESUME");
    }
}

fn fire_feedback(result: Option<FireResult>) -> Option<&'static str> {
    match result {
        Some(FireResult::OutOfAmmo) => Some("OUT OF AMMO"),
        Some(FireResult::Unsupported) => Some("WEAPON NOT AVAILABLE"),
        _ => None,
    }
}

/// Provisional bounded red damage flash and deterministic pain throttling.
/// State is presentation-only and cloned with the same-level checkpoint.
#[derive(Clone, Copy, Default)]
struct DamageFeedback {
    flash: u8,
    pain_cooldown: u8,
    next_pain_variant: bool,
}
impl DamageFeedback {
    fn tick(&mut self, before: u16, after: u16, damage: u16) -> Option<PlayerSound> {
        self.flash = self.flash.saturating_sub(12);
        self.pain_cooldown = self.pain_cooldown.saturating_sub(1);
        if before == 0 || damage == 0 {
            return None;
        }
        self.flash = self.flash.max((u32::from(damage) * 3 + 20).min(128) as u8);
        if after == 0 {
            return Some(PlayerSound::Death);
        }
        if self.pain_cooldown != 0 {
            return None;
        }
        self.pain_cooldown = 30;
        let sound = if self.next_pain_variant {
            PlayerSound::Pain1
        } else {
            PlayerSound::Pain0
        };
        self.next_pain_variant = !self.next_pain_variant;
        Some(sound)
    }
}

fn apply_damage_tint(frame: &mut [u32], alpha: u8) {
    if alpha == 0 {
        return;
    }
    let alpha = u32::from(alpha.min(128));
    for pixel in frame {
        let red = ((*pixel >> 16) & 255) * (255 - alpha) / 255 + alpha;
        let green = ((*pixel >> 8) & 255) * (255 - alpha) / 255;
        let blue = (*pixel & 255) * (255 - alpha) / 255;
        *pixel = (red << 16) | (green << 8) | blue;
    }
}

/// Text timers derive directly from authoritative fixed-tick state.
fn powerup_status(invisibility: u32, reflector: u32, chojin: u32) -> String {
    let mut parts = Vec::new();
    for (label, ticks) in [
        ("INVISIBILITY", invisibility),
        ("REFLECTOR", reflector),
        ("CHOJIN", chojin),
    ] {
        if ticks > 0 {
            parts.push(format!("{label} {}S", ticks.div_ceil(60)));
        }
    }
    parts.join("  ")
}

fn advance_reflection_indicator(ticks: u8, reflection_count: usize) -> u8 {
    if reflection_count > 0 {
        30
    } else {
        ticks.saturating_sub(1)
    }
}

fn teleport_failure_message(failure: openchasm::teleport::TeleportFailure) -> String {
    use openchasm::teleport::TeleportFailure;
    match failure {
        TeleportFailure::DestinationBlocked { index } => {
            format!("TELEPORT {} DESTINATION BLOCKED", index + 1)
        }
        TeleportFailure::InvalidDefinition { index } => format!("TELEPORT {} INVALID", index + 1),
    }
}

// Return true only when this is a new continuous failure, for log deduplication.
fn update_teleport_notice(
    notice: &mut Option<(String, f32)>,
    last_failure: &mut Option<openchasm::teleport::TeleportFailure>,
    failure: Option<openchasm::teleport::TeleportFailure>,
) -> bool {
    if let Some((_, remaining)) = notice {
        *remaining -= STEP;
    }
    if notice
        .as_ref()
        .is_some_and(|(_, remaining)| *remaining <= 0.0)
    {
        *notice = None;
    }
    let changed = failure.is_some() && failure != *last_failure;
    if let Some(failure) = failure {
        if changed || notice.is_none() {
            *notice = Some((teleport_failure_message(failure), 3.0));
        } else if let Some((_, remaining)) = notice {
            *remaining = 3.0;
        }
    }
    *last_failure = failure;
    changed
}

struct FrameClock {
    last: Instant,
}
impl FrameClock {
    fn sample(&mut self, now: Instant, focused: bool) -> f32 {
        let elapsed = now
            .saturating_duration_since(self.last)
            .min(Duration::from_millis(100));
        self.last = now;
        if focused {
            elapsed.as_secs_f32()
        } else {
            0.0
        }
    }
    fn rebase(&mut self, now: Instant) {
        self.last = now;
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
    lights: Option<&'a openchasm::dynamic_light::DynamicLightSampler>,
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
        if scene.lights.is_some() {
            planes.render_lit(frame, WIDTH, HEIGHT, camera, scene.lights);
        } else {
            planes.render(frame, WIDTH, HEIGHT, camera);
        }
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
            let point = [
                camera.player.position[0] + direction[0] * distance,
                camera.player.position[1] + direction[1] * distance,
            ];
            // Provisional linear XY falloff brightens the existing distance
            // shading; it does not reproduce original lightmaps or occlusion.
            let light = scene.lights.map_or(0.0, |lights| lights.sample(point));
            let channel = |base: u32| ((base as f32 * attenuation * (1.0 + light)) as u32).min(255);
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
        if scene.lights.is_some() {
            objects.render_with_mines_and_lights(
                frame,
                depth,
                WIDTH,
                HEIGHT,
                camera,
                scene.mines,
                scene.lights,
            );
        } else if scene.mines.is_empty() {
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
        "PREVIEW: F5 QUICKSAVE  F9 QUICKLOAD  R RESTART  ESC PAUSE",
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

// In-memory checkpoint scoped to this viewer's current level and assets.
struct QuickSave {
    session: Session,
    scene: ObjectSceneSnapshot,
    animation_tick: u64,
    weapon_clock: WeaponAnimationClock,
    damage_feedback: DamageFeedback,
    reflection_ticks: u8,
    teleport_notice: Option<(String, f32)>,
    last_teleport_failure: Option<openchasm::teleport::TeleportFailure>,
    current_message: Option<(Vec<String>, f32)>,
    completed: bool,
    shot_feedback: Option<(&'static str, f32)>,
}

#[derive(Clone, Copy)]
enum CheckpointCommand {
    Save,
    Load,
}

fn sync_actor_scene(
    objects: &mut ObjectScene,
    session: &Session,
    delta_seconds: f32,
    warned_actor_clips: &mut HashSet<usize>,
) {
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
        objects.set_actor_animation(actor.source_index, animation, delta_seconds);
        let attack_frame = session
            .ai
            .attack_animation(actor.source_index)
            .map(|phase| (phase.slot, phase.frame));
        if !objects.set_actor_attack_frame(actor.source_index, attack_frame)
            && attack_frame.is_some()
            && warned_actor_clips.insert(actor.source_index)
        {
            eprintln!(
                "actor {} attack animation is unavailable or inconsistent with simulation metadata",
                actor.source_index
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
    pub teleport_load_error: Option<String>,
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
        mut audio,
        weapon_overlay,
        projectile_renderer,
        mut explosion_renderer,
        difficulty,
        teleport_load_error,
    } = feedback;
    let difficulty_mask = crate::difficulty_mask(difficulty);
    let initial_session = session.clone();
    let mut warned_actor_clips = HashSet::new();
    if let Some(targets) = &session.objects {
        for (cell, visible) in targets.visibility() {
            objects.set_visible(cell, visible);
        }
    }
    restore_level_visibility(&mut objects, &session, difficulty_mask);
    for cell in session.game.pickups.hidden_cells() {
        objects.set_visible(cell, false);
    }
    sync_actor_scene(&mut objects, &session, 0.0, &mut warned_actor_clips);
    let initial_scene = objects.snapshot();
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
    let mut mouse_fire = false;
    let mut frame = vec![0; WIDTH * HEIGHT];
    let mut depth = vec![f32::INFINITY; WIDTH * HEIGHT];
    let mut frame_clock = FrameClock {
        last: Instant::now(),
    };
    let mut accumulator = 0.0f32;
    let mut animation_tick = 0_u64;
    let mut pending_interact = false;
    let mut pending_jump = false;
    let mut pending_weapon = None;
    let mut weapon_clock = WeaponAnimationClock::default();
    let mut damage_feedback = DamageFeedback::default();
    let mut reflection_ticks = 0_u8;
    let startup_teleport_error = teleport_load_error;
    let mut teleport_notice = startup_teleport_error.clone().map(|message| (message, 5.0));
    let mut last_teleport_failure = None;
    let mut current_message: Option<(Vec<String>, f32)> = None;
    let mut shot_feedback: Option<(&'static str, f32)> = None;
    let mut quicksave: Option<QuickSave> = None;
    let mut checkpoint_notice: Option<(&'static str, f32)> = None;
    let mut completed = false;
    let mut pause = PauseMenu::default();
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
        let mut checkpoint_command = None;
        let mut suppress_gameplay = false;
        for event in event_pump.poll_iter() {
            if matches!(
                event,
                Event::Quit { .. }
                    | Event::Window {
                        win_event: WindowEvent::Close,
                        ..
                    }
            ) {
                pause.show(Some(PauseAction::Exit));
                sdl.mouse().set_relative_mouse_mode(false);
                look.captured = false;
                clear_held_controls(&mut held, &mut mouse_fire, &mut look, &mut accumulator);
                pending_interact = false;
                pending_jump = false;
                pending_weapon = None;
                advance_requested = false;
                frame_clock.rebase(Instant::now());
                suppress_gameplay = true;
                continue;
            }
            if matches!(
                event,
                Event::Window {
                    win_event: WindowEvent::FocusLost,
                    ..
                }
            ) {
                look.set_focus(false);
                if !pause.open {
                    pause.show(None);
                }
                sdl.mouse().set_relative_mouse_mode(false);
                clear_held_controls(&mut held, &mut mouse_fire, &mut look, &mut accumulator);
                pending_interact = false;
                pending_jump = false;
                pending_weapon = None;
                advance_requested = false;
                frame_clock.rebase(Instant::now());
                suppress_gameplay = true;
                continue;
            }
            if matches!(
                event,
                Event::Window {
                    win_event: WindowEvent::FocusGained,
                    ..
                }
            ) {
                look.set_focus(true);
                continue;
            }
            if pause.open {
                if suppress_gameplay {
                    continue;
                }
                if let Some(action) = pause.event(&event, look.focused) {
                    match action {
                        PauseAction::Resume => {
                            pause.open = false;
                            clear_held_controls(
                                &mut held,
                                &mut mouse_fire,
                                &mut look,
                                &mut accumulator,
                            );
                            pending_interact = false;
                            pending_jump = false;
                            pending_weapon = None;
                            frame_clock.rebase(Instant::now());
                            suppress_gameplay = true;
                        }
                        PauseAction::Save => {
                            checkpoint_command = Some(CheckpointCommand::Save);
                            suppress_gameplay = true;
                        }
                        PauseAction::Load => {
                            checkpoint_command = Some(CheckpointCommand::Load);
                            suppress_gameplay = true;
                        }
                        PauseAction::Restart => {
                            restart_requested = true;
                            suppress_gameplay = true;
                        }
                        PauseAction::Exit => running = false,
                    }
                }
                continue;
            }
            if suppress_gameplay {
                continue;
            }
            match event {
                Event::KeyDown {
                    scancode: Some(code),
                    repeat,
                    ..
                } if look.focused => {
                    if !repeat {
                        held.insert(code);
                        if matches!(code, Scancode::Escape | Scancode::R) {
                            pause.show((code == Scancode::R).then_some(PauseAction::Restart));
                            sdl.mouse().set_relative_mouse_mode(false);
                            look.captured = false;
                            clear_held_controls(
                                &mut held,
                                &mut mouse_fire,
                                &mut look,
                                &mut accumulator,
                            );
                            pending_interact = false;
                            pending_jump = false;
                            pending_weapon = None;
                            advance_requested = false;
                            frame_clock.rebase(Instant::now());
                            suppress_gameplay = true;
                        } else if code == Scancode::Return {
                            advance_requested = true;
                        } else if code == Scancode::F5 {
                            checkpoint_command = Some(CheckpointCommand::Save);
                        } else if code == Scancode::F9 {
                            checkpoint_command = Some(CheckpointCommand::Load);
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
                Event::MouseWheel { y, direction, .. } => {
                    let delta = if direction == MouseWheelDirection::Flipped {
                        y.saturating_neg()
                    } else {
                        y
                    };
                    let current = pending_weapon.unwrap_or(session.selected_weapon);
                    if let Some(next) = wheel_weapon(&look, &session.game.inventory, current, delta)
                    {
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
        let active = simulation_active(look.focused, pause.open, suppress_gameplay);
        let elapsed = frame_clock.sample(Instant::now(), active);
        if active {
            accumulator += elapsed;
        } else {
            accumulator = 0.0;
        }
        if restart_requested {
            audio.stop_one_shots();
            audio.reset_ambient();
            session = initial_session.clone();
            if let Some(renderer) = &mut explosion_renderer {
                renderer.clear();
            }
            if !objects.restore(&initial_scene) {
                return Err("initial scene snapshot no longer matches the level".into());
            }
            clear_held_controls(&mut held, &mut mouse_fire, &mut look, &mut accumulator);
            frame_clock.rebase(Instant::now());
            pending_interact = false;
            pending_jump = false;
            pending_weapon = None;
            weapon_clock = WeaponAnimationClock::default();
            damage_feedback = DamageFeedback::default();
            reflection_ticks = 0;
            teleport_notice = startup_teleport_error.clone().map(|message| (message, 5.0));
            last_teleport_failure = None;
            animation_tick = 0;
            current_message = None;
            shot_feedback = None;
            completed = false;
            look.pending_dx = 0;
        }
        if let Some(command) = checkpoint_command {
            match command {
                CheckpointCommand::Save => {
                    quicksave = Some(QuickSave {
                        session: session.clone(),
                        scene: objects.snapshot(),
                        animation_tick,
                        weapon_clock,
                        damage_feedback,
                        reflection_ticks,
                        teleport_notice: teleport_notice.clone(),
                        last_teleport_failure,
                        current_message: current_message.clone(),
                        completed,
                        shot_feedback,
                    });
                    frame_clock.rebase(Instant::now());
                    checkpoint_notice = Some(("QUICKSAVED (MEMORY ONLY)", 2.0));
                }
                CheckpointCommand::Load => {
                    if let Some(saved) = &quicksave {
                        if objects.restore(&saved.scene) {
                            audio.stop_one_shots();
                            audio.reset_ambient();
                            session = saved.session.clone();
                            animation_tick = saved.animation_tick;
                            weapon_clock = saved.weapon_clock;
                            damage_feedback = saved.damage_feedback;
                            reflection_ticks = saved.reflection_ticks;
                            teleport_notice = saved.teleport_notice.clone();
                            last_teleport_failure = saved.last_teleport_failure;
                            current_message = saved.current_message.clone();
                            completed = saved.completed;
                            shot_feedback = saved.shot_feedback;
                            if let Some(renderer) = &mut explosion_renderer {
                                renderer.clear();
                            }
                            // Leave window focus/capture unchanged; discard all
                            // inputs accumulated before the restored state.
                            clear_held_controls(
                                &mut held,
                                &mut mouse_fire,
                                &mut look,
                                &mut accumulator,
                            );
                            pending_interact = false;
                            pending_jump = false;
                            pending_weapon = None;
                            frame_clock.rebase(Instant::now());
                            advance_requested = false;
                            checkpoint_notice = Some(("QUICKLOADED", 2.0));
                        } else {
                            checkpoint_notice = Some(("QUICKLOAD UNAVAILABLE", 2.0));
                        }
                    } else {
                        checkpoint_notice = Some(("NO QUICKSAVE", 2.0));
                    }
                }
            }
        }
        if completed && advance_requested && !pause.open {
            sdl.mouse().set_relative_mouse_mode(false);
            return Ok(Some(session.game.inventory.clone()));
        }
        let movement = held_movement(&held);
        let firing = mouse_fire;
        while accumulator >= STEP {
            // Deltas are motion, not velocity: consume exactly once per tick.
            let yaw_delta = look.take_yaw();
            animation_tick = animation_tick.saturating_add(1);
            let old_weapon = session.selected_weapon;
            let health_before = session.game.inventory.health;
            let enemy_health = crate::audio::enemy_health_before(&session);
            let events = session.step(SessionInput {
                movement,
                yaw_delta,
                interact: pending_interact,
                jump: pending_jump,
                fire: firing,
                select_weapon: pending_weapon,
            });
            audio.update_ambient(session.ambient_grid(), session.world.player.position, STEP);
            pending_interact = false;
            pending_jump = false;
            pending_weapon = None;
            if session.selected_weapon != old_weapon {
                shot_feedback = None;
            }
            if let Some(sound) =
                damage_feedback.tick(health_before, session.game.inventory.health, events.damage)
            {
                audio.play_player_sound(sound);
            }
            reflection_ticks =
                advance_reflection_indicator(reflection_ticks, events.projectile_reflections.len());
            let fired = matches!(events.shot, Some(FireResult::Fired(_)));
            weapon_clock.tick(
                session.selected_weapon != old_weapon,
                fired,
                weapon_overlay.frame_count(session.selected_weapon, WeaponPose::Shoot),
            );
            if update_teleport_notice(
                &mut teleport_notice,
                &mut last_teleport_failure,
                events.teleport_failure,
            ) {
                if let Some((message, _)) = &teleport_notice {
                    eprintln!("{message}");
                }
            }
            if let Some((_, remaining)) = &mut checkpoint_notice {
                *remaining -= STEP;
            }
            if checkpoint_notice.is_some_and(|(_, remaining)| remaining <= 0.0) {
                checkpoint_notice = None;
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
            audio.play_enemy_reactions(
                &enemy_health,
                &session.actors,
                session.world.player.position,
                animation_tick,
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
            if fired {
                audio.play(21 + session.selected_weapon as u8);
            }
            sync_actor_scene(&mut objects, &session, STEP, &mut warned_actor_clips);
            accumulator -= STEP;
        }
        // Initialize or resynchronize even when no simulation tick ran.
        audio.update_ambient(session.ambient_grid(), session.world.player.position, 0.0);
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
        let dynamic_lights = session.light_sources.as_ref().map(|sources| {
            openchasm::dynamic_light::DynamicLightSampler::from_snapshot(&sources.snapshot())
        });
        render(
            &mut frame,
            &mut depth,
            RenderWorld {
                world: &session.world,
                elevations: &session.wall_elevations,
                eye_height: camera.eye_height,
                yaw_offset,
                mines: session.mines(),
                lights: dynamic_lights.as_ref(),
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
        let (pose, frame_index) = weapon_clock
            .pose(weapon_overlay.frame_count(session.selected_weapon, WeaponPose::Idle));
        weapon_overlay.render(
            &mut frame,
            WIDTH,
            HEIGHT,
            session.selected_weapon,
            pose,
            frame_index,
        );
        apply_damage_tint(&mut frame, damage_feedback.flash);
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
        let effects = &session.game.protective_powerups;
        let status = powerup_status(
            inventory.invisibility_ticks_remaining,
            effects.reflector_ticks_remaining,
            effects.chojin_ticks_remaining,
        );
        if !status.is_empty() {
            text(&mut frame, 8, HEIGHT - 30, &status);
        }
        if reflection_ticks > 0 {
            text(&mut frame, WIDTH / 2, HEIGHT - 42, "PROJECTILE REFLECTED");
        }
        if let Some((lines, _)) = &current_message {
            for (row, line) in lines.iter().enumerate().take(6) {
                text(&mut frame, 8, 52 + row * 10, line);
            }
        }
        if let Some((message, _)) = &teleport_notice {
            text(&mut frame, 8, HEIGHT - 78, message);
        }
        if let Some((message, _)) = checkpoint_notice {
            text(&mut frame, 8, HEIGHT - 54, message);
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
        if pause.open {
            draw_pause_menu(&mut frame, &pause);
        } else if !look.focused {
            text(&mut frame, WIDTH / 2 - 28, HEIGHT / 2 + 50, "PAUSED");
        } else if !look.captured {
            text(&mut frame, 8, HEIGHT - 66, "CLICK TO CAPTURE MOUSE");
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
    #[test]
    fn pause_menu_navigation_confirm_and_checkpoint_shortcuts() {
        let mut menu = PauseMenu::default();
        menu.show(None);
        assert_eq!(menu.key(Scancode::F5), Some(PauseAction::Save));
        assert_eq!(menu.key(Scancode::F9), Some(PauseAction::Load));
        assert_eq!(menu.key(Scancode::W), None);
        menu.key(Scancode::Up);
        assert_eq!(menu.selected, 4);
        assert_eq!(menu.key(Scancode::Return), None);
        assert_eq!(menu.confirm, Some(PauseAction::Exit));
        assert_eq!(menu.key(Scancode::F5), None);
        assert_eq!(menu.key(Scancode::Escape), None);
        assert_eq!(menu.confirm, None);
        menu.selected = 3;
        menu.key(Scancode::Return);
        assert_eq!(menu.key(Scancode::Return), Some(PauseAction::Restart));
        assert_eq!(menu.key(Scancode::Escape), Some(PauseAction::Resume));
    }
    #[test]
    fn pause_routes_sdl_events_only_when_focused_and_freezes_clock() {
        use sdl2::keyboard::Mod;
        fn key(code: Scancode, repeat: bool) -> Event {
            Event::KeyDown {
                timestamp: 0,
                window_id: 1,
                keycode: None,
                scancode: Some(code),
                keymod: Mod::NOMOD,
                repeat,
            }
        }
        let mut menu = PauseMenu::default();
        menu.show(Some(PauseAction::Exit));
        assert_eq!(menu.event(&key(Scancode::Return, false), false), None);
        assert_eq!(menu.confirm, Some(PauseAction::Exit));
        assert_eq!(menu.event(&key(Scancode::Return, true), true), None);
        assert_eq!(menu.event(&key(Scancode::Escape, false), true), None);
        assert_eq!(
            menu.event(&key(Scancode::F5, false), true),
            Some(PauseAction::Save)
        );
        assert_eq!(
            menu.event(&key(Scancode::F9, false), true),
            Some(PauseAction::Load)
        );
        assert_eq!(menu.event(&key(Scancode::W, false), true), None);
        assert!(!simulation_active(true, menu.open, false));
        assert!(!simulation_active(false, false, false));
        assert!(!simulation_active(true, false, true));
        assert!(simulation_active(true, false, false));
        let start = Instant::now();
        let mut clock = FrameClock { last: start };
        assert_eq!(clock.sample(start + Duration::from_secs(10), false), 0.0);
        clock.rebase(start + Duration::from_secs(20));
        assert!(
            clock.sample(
                start + Duration::from_secs(20) + Duration::from_millis(16),
                true
            ) < STEP
        );
        menu.show(Some(PauseAction::Exit));
        assert_eq!(
            menu.event(&key(Scancode::Return, false), true),
            Some(PauseAction::Exit)
        );
    }
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
                lights: None,
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
    fn wall_lighting_uses_hit_position_and_preserves_depth() {
        use openchasm::light_sources::{
            LightSourceProcessSnapshot, LightSourcesSnapshot, SourceLight,
        };
        let world = World {
            walls: vec![Wall {
                cell: None,
                a: [3.0, 1.0],
                b: [3.0, 3.0],
                texture: 1,
                blocks_movement: true,
            }],
            player: Player {
                position: [1.0, 2.0],
                angle: 0.0,
            },
        };
        let lights =
            openchasm::dynamic_light::DynamicLightSampler::from_snapshot(&LightSourcesSnapshot {
                active: vec![LightSourceProcessSnapshot {
                    process_id: 1,
                    elapsed_ticks: 0,
                    sources: vec![SourceLight {
                        position_raw: [3 * 256, 2 * 256],
                        radius_raw: 256,
                        brightness: 255,
                        delay_ms: 0,
                    }],
                }],
            });
        let mut base = vec![0; WIDTH * HEIGHT];
        let mut lit = base.clone();
        let mut base_depth = vec![f32::INFINITY; base.len()];
        let mut lit_depth = base_depth.clone();
        let scene = |lights| RenderWorld {
            world: &world,
            elevations: &[],
            eye_height: 1.0,
            yaw_offset: 0.0,
            mines: &[],
            lights,
        };
        render(&mut base, &mut base_depth, scene(None), &[], None, None);
        render(
            &mut lit,
            &mut lit_depth,
            scene(Some(&lights)),
            &[],
            None,
            None,
        );
        let pixel = HEIGHT / 2 * WIDTH + WIDTH / 2;
        assert!(lit[pixel] > base[pixel]);
        assert_eq!(lit_depth, base_depth);
        assert_eq!(lit[0], base[0]);
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
                lights: None,
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
                lights: None,
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
                lights: None,
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
                lights: None,
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
            invisibility_ticks_remaining: 0,
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
    #[test]
    fn frame_clock_excludes_checkpoint_copy_time() {
        let start = Instant::now();
        let mut clock = FrameClock { last: start };
        assert!((clock.sample(start + Duration::from_millis(10), true) - 0.01).abs() < 0.00001);
        // A slow save/load took 250ms; rebase before the next frame.
        clock.rebase(start + Duration::from_millis(260));
        assert!((clock.sample(start + Duration::from_millis(276), true) - 0.016).abs() < 0.00001);
        assert_eq!(clock.sample(start + Duration::from_millis(300), false), 0.0);
        assert!((clock.sample(start + Duration::from_millis(310), true) - 0.01).abs() < 0.00001);
    }
    #[test]
    fn damage_feedback_throttles_pain_emits_death_once_and_snapshots() {
        let mut feedback = DamageFeedback::default();
        assert_eq!(feedback.tick(100, 90, 10), Some(PlayerSound::Pain0));
        let saved = feedback;
        assert!(feedback.flash > 0 && feedback.flash <= 128);
        assert_eq!(feedback.tick(90, 85, 5), None);
        for _ in 0..29 {
            feedback.tick(85, 85, 0);
        }
        assert_eq!(feedback.tick(85, 80, 5), Some(PlayerSound::Pain1));
        assert_eq!(feedback.tick(80, 0, 100), Some(PlayerSound::Death));
        assert_eq!(feedback.tick(0, 0, 100), None);
        feedback = saved;
        assert_eq!(feedback.tick(90, 89, 1), None); // restored throttle
        feedback = DamageFeedback::default(); // restart
        assert_eq!(feedback.tick(100, 99, 1), Some(PlayerSound::Pain0));
    }

    #[test]
    fn damage_tint_is_bounded_and_zero_is_identity() {
        let mut pixels = [0x000000, 0xffffff, 0x004080];
        let before = pixels;
        apply_damage_tint(&mut pixels, 0);
        assert_eq!(pixels, before);
        apply_damage_tint(&mut pixels, 255);
        assert_eq!(pixels[0], 0x800000);
        assert_eq!(pixels[1], 0xff7f7f);
        assert!(pixels[2] >> 16 > (before[2] >> 16));
    }
    #[test]
    fn powerup_timers_round_up_and_hide_expired_effects() {
        assert_eq!(powerup_status(0, 0, 0), "");
        assert_eq!(
            powerup_status(61, 2400, 1),
            "INVISIBILITY 2S  REFLECTOR 40S  CHOJIN 1S"
        );
        assert_eq!(powerup_status(0, 60, 0), "REFLECTOR 1S");
        let active = advance_reflection_indicator(0, 1);
        assert_eq!(active, 30);
        assert_eq!(advance_reflection_indicator(active, 0), 29);
        assert_eq!(advance_reflection_indicator(1, 2), 30);
        assert_eq!(advance_reflection_indicator(0, 0), 0);
    }
    #[test]
    fn teleport_errors_are_actionable() {
        use openchasm::teleport::TeleportFailure;
        assert_eq!(
            teleport_failure_message(TeleportFailure::DestinationBlocked { index: 4 }),
            "TELEPORT 5 DESTINATION BLOCKED"
        );
        assert_eq!(
            teleport_failure_message(TeleportFailure::InvalidDefinition { index: 1 }),
            "TELEPORT 2 INVALID"
        );
    }
    #[test]
    fn blocked_teleport_notice_persists_and_deduplicates_across_snapshot() {
        use openchasm::teleport::TeleportFailure;
        let failure = Some(TeleportFailure::DestinationBlocked { index: 2 });
        let mut notice = None;
        let mut last = None;
        assert!(update_teleport_notice(&mut notice, &mut last, failure));
        for _ in 0..600 {
            assert!(!update_teleport_notice(&mut notice, &mut last, failure));
        }
        assert_eq!(notice.as_ref().unwrap().0, "TELEPORT 3 DESTINATION BLOCKED");
        let (mut restored_notice, mut restored_last) = (notice.clone(), last);
        assert!(!update_teleport_notice(
            &mut restored_notice,
            &mut restored_last,
            failure
        ));
        for _ in 0..181 {
            assert!(!update_teleport_notice(&mut notice, &mut last, None));
        }
        assert!(notice.is_none());
        assert!(update_teleport_notice(&mut notice, &mut last, failure));
        assert!(update_teleport_notice(
            &mut restored_notice,
            &mut restored_last,
            Some(TeleportFailure::DestinationBlocked { index: 3 }),
        ));
        assert_eq!(restored_notice.unwrap().0, "TELEPORT 4 DESTINATION BLOCKED");
    }
    #[test]
    fn restart_clears_movement_fire_yaw_and_time_without_releasing_capture() {
        let mut held = HashSet::from([Scancode::W, Scancode::D, Scancode::Right]);
        let mut fire = true;
        let mut look = RelativeLook {
            focused: true,
            captured: true,
            pending_dx: 42,
        };
        let mut accumulator = 0.1;
        assert_eq!(held_movement(&held).forward, 1.0);
        clear_held_controls(&mut held, &mut fire, &mut look, &mut accumulator);
        let movement = held_movement(&held);
        assert_eq!(
            (movement.forward, movement.strafe, movement.turn),
            (0.0, 0.0, 0.0)
        );
        assert!(!fire);
        assert_eq!(look.take_yaw(), 0.0);
        assert_eq!(accumulator, 0.0);
        assert!(look.focused && look.captured);
    }

    #[test]
    fn wheel_selection_works_without_capture_but_not_without_focus() {
        let inventory = openchasm::inventory::Inventory {
            health: 100,
            armor: 0,
            weapons: [true, true, false, false, false, false, false, false],
            ammo: [1, 1, 0, 0, 0, 0, 0, 0],
            keys: [false; 3],
            invisibility_ticks_remaining: 0,
        };
        let mut look = RelativeLook {
            focused: true,
            captured: false,
            pending_dx: 0,
        };
        assert_eq!(wheel_weapon(&look, &inventory, 0, 1), Some(1));
        look.captured = true;
        assert_eq!(wheel_weapon(&look, &inventory, 0, 1), Some(1));
        look.set_focus(false);
        assert_eq!(wheel_weapon(&look, &inventory, 0, 1), None);
    }
}
