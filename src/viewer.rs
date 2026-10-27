//! Software-rendered geometry viewer. Game rules live outside the window layer.
//!
//! F5/F9 provide a same-level in-memory quicksave only. The snapshot includes
//! simulation and scene instance/animation state; transient explosion sprites
//! and one-shot sounds are cleared on load; music continues. No data is
//! written to disk; the checkpoint is lost on leaving the viewer/level.
#[path = "minimap.rs"]
mod minimap;
use crate::audio::{Audio, PlayerSound};
use crate::color_pipeline::{ColorPipeline, LinearRgb, RenderPixel};
use crate::graphics_settings::{GraphicsMenu, GraphicsSettings, MenuAction, SettingsStore};
use crate::lighting_worker::{LightingWorker, OwnedShadowTexture};
use crate::object_render::{
    ActorAnimationMode, CameraView, ObjectScene, ObjectSceneSnapshot, Translucency,
};
use crate::planes::PlaneRenderer;
use crate::presentation::{BackendChoice, Presentation};
use crate::projectile_render::{ProjectileRenderer, ProjectileVisual};
use crate::remaster_light::{LightingError, RemasterLightMap, ShadowTexture};
use crate::texture::CelImage;
use crate::ui::{OverlayPixel, UiPixel};
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
};
use std::collections::{BTreeSet, HashSet};
use std::error::Error;
use std::sync::Arc;
use std::time::{Duration, Instant};

const WIDTH: usize = 640;
const HEIGHT: usize = 400;
const STEP: f32 = 1.0 / 60.0;
const MOUSE_SENSITIVITY: f32 = 0.002;
fn fps_limit(value: u32) -> Option<Duration> {
    (value != 0).then(|| Duration::from_secs_f64(1.0 / f64::from(value)))
}
fn effective_fps_limit(stored: u32, uncapped_override: bool) -> Option<Duration> {
    if uncapped_override {
        None
    } else {
        fps_limit(stored)
    }
}

#[derive(Default)]
struct RelativeLook {
    focused: bool,
    captured: bool,
    discard_capture_motion: bool,
    pending_dx: i64,
    pending_dy: i64,
}
impl RelativeLook {
    fn set_focus(&mut self, focused: bool) {
        self.focused = focused;
        if !focused {
            self.release();
        }
    }
    /// Attempt capture only for a focused gameplay window. Drop stale deltas
    /// when switching modes so cursor repositioning does not turn the player.
    fn capture_with(&mut self, enable: impl FnOnce() -> bool) -> bool {
        if !self.focused {
            return false;
        }
        self.pending_dx = 0;
        self.pending_dy = 0;
        self.captured = enable();
        self.discard_capture_motion = self.captured;
        self.captured
    }
    fn release(&mut self) {
        self.captured = false;
        self.discard_capture_motion = false;
        self.pending_dx = 0;
        self.pending_dy = 0;
    }
    fn relative_motion(&mut self, dx: i32, dy: i32) {
        if !self.focused || !self.captured {
            return;
        }
        // Native pointer locking may deliver an initial recentering delta
        // after the mode switch, despite clearing previously queued deltas.
        if std::mem::take(&mut self.discard_capture_motion) {
            return;
        }
        self.motion(dx);
        self.motion_y(dy);
    }
    fn motion(&mut self, dx: i32) {
        if self.focused && self.captured {
            self.pending_dx = self.pending_dx.saturating_add(i64::from(dx));
        }
    }
    fn take_yaw(&mut self) -> f32 {
        let dx = std::mem::take(&mut self.pending_dx).clamp(-100_000, 100_000);
        -(dx as f32) * MOUSE_SENSITIVITY
    }
    fn motion_y(&mut self, dy: i32) {
        if self.focused && self.captured {
            self.pending_dy = self.pending_dy.saturating_add(i64::from(dy));
        }
    }
    fn take_pitch(&mut self) -> f32 {
        let dy = std::mem::take(&mut self.pending_dy).clamp(-100_000, 100_000);
        -(dy as f32) * MOUSE_SENSITIVITY
    }
}

fn capture_mouse(look: &mut RelativeLook, mouse: &sdl2::mouse::MouseUtil) -> bool {
    look.capture_with(|| {
        mouse.set_relative_mouse_mode(true);
        mouse.relative_mouse_mode()
    })
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
        turn: key(Scancode::Left) - key(Scancode::Right),
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
    look.pending_dy = 0;
    *accumulator = 0.0;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PauseAction {
    Resume,
    Save,
    Load,
    Options,
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
            Scancode::Up => self.selected = (self.selected + 5) % 6,
            Scancode::Down => self.selected = (self.selected + 1) % 6,
            Scancode::F5 => return Some(PauseAction::Save),
            Scancode::F9 => return Some(PauseAction::Load),
            Scancode::Return => match self.selected {
                0 => return Some(PauseAction::Resume),
                1 => return Some(PauseAction::Save),
                2 => return Some(PauseAction::Load),
                3 => return Some(PauseAction::Options),
                4 => self.confirm = Some(PauseAction::Restart),
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

fn draw_pause_menu<C: UiPixel>(frame: &mut [C], menu: &PauseMenu) {
    for y in 85..315 {
        for x in 125..515 {
            frame[y * WIDTH + x] = C::from_rgb(0x101820);
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
            "GRAPHICS OPTIONS",
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

// Preserve committed event order and hit-time sound/position, even when a
// replacement changes the definition at the same map cell.
fn dispatch_object_breaks(
    events: &[openchasm::session::ObjectDestructionEvent],
    mut play: impl FnMut(i32, [f32; 2]),
) {
    for event in events {
        play(event.sound_id, event.position);
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

fn apply_damage_tint<C: UiPixel>(frame: &mut [C], alpha: u8) {
    if alpha == 0 {
        return;
    }
    for pixel in frame {
        *pixel = pixel.tint_rgb(0xff0000, alpha.min(128));
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

fn text<C: UiPixel>(frame: &mut [C], x: usize, y: usize, message: &str) {
    for (i, ch) in message.chars().enumerate() {
        if let Some(glyph) = BASIC_FONTS.get(ch) {
            for (dy, row) in glyph.iter().enumerate() {
                for dx in 0..8 {
                    let px = x + i * 8 + dx;
                    let py = y + dy;
                    if px < WIDTH && py < HEIGHT && row & (1 << dx) != 0 {
                        frame[py * WIDTH + px] = C::from_rgb(0xffffff);
                    }
                }
            }
        }
    }
}

#[derive(Clone, Copy)]
struct RenderWorld<'a> {
    world: &'a World,
    elevations: &'a [f32],
    eye_height: f32,
    yaw_offset: f32,
    pitch: f32,
    mines: &'a [openchasm::explosives::Mine],
    lights: Option<&'a openchasm::dynamic_light::DynamicLightSampler>,
    remaster: Option<(&'a RemasterLightMap, f32)>,
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
            pitch: 0.0,
        },
        yaw_offset,
    )
}

#[derive(Default, Clone, Copy)]
struct RenderTimings {
    plane: Duration,
    wall: Duration,
    model: Duration,
    effects: Duration,
    weapon: Duration,
    post: Duration,
}
impl RenderTimings {
    fn accumulate(&mut self, other: Self) {
        self.plane += other.plane;
        self.wall += other.wall;
        self.model += other.model;
        self.effects += other.effects;
        self.weapon += other.weapon;
        self.post += other.post;
    }
    fn ms_per_frame(value: Duration, frames: u32) -> f64 {
        if frames == 0 {
            0.0
        } else {
            value.as_secs_f64() * 1000.0 / f64::from(frames)
        }
    }
}
fn profile_start(enabled: bool) -> Option<Instant> {
    enabled.then(Instant::now)
}
fn profile_elapsed(start: Option<Instant>) -> Duration {
    start.map_or(Duration::ZERO, |time| time.elapsed())
}

#[cfg(test)]
fn render(
    frame: &mut [u32],
    depth: &mut [f32],
    scene: RenderWorld<'_>,
    textures: &[Option<ViewerTexture>],
    planes: Option<&PlaneRenderer>,
    objects: Option<&ObjectScene>,
) {
    let mut transparency = Translucency::new(frame.len());
    let _ = render_profiled::<true, u32>(
        SceneTarget {
            frame,
            depth,
            transparency: &mut transparency,
        },
        scene,
        textures,
        planes,
        objects,
        false,
    );
    transparency.composite(frame, depth);
}

struct SceneTarget<'a, C: RenderPixel> {
    frame: &'a mut [C],
    depth: &'a mut [f32],
    transparency: &'a mut Translucency<C>,
}

fn render_profiled<const NEAR_FIRST: bool, C: RenderPixel>(
    target: SceneTarget<'_, C>,
    scene: RenderWorld<'_>,
    textures: &[Option<ViewerTexture>],
    planes: Option<&PlaneRenderer>,
    objects: Option<&ObjectScene>,
    profile: bool,
) -> RenderTimings {
    let SceneTarget {
        frame,
        depth,
        transparency,
    } = target;
    let mut timings = RenderTimings::default();
    let started = profile_start(profile);
    let world = scene.world;
    let mut camera_player = world.player;
    camera_player.angle += scene.yaw_offset;
    let camera = CameraView {
        player: camera_player,
        eye_height: scene.eye_height,
        pitch: scene.pitch,
    };
    depth.fill(f32::INFINITY);
    if let Some(planes) = planes {
        planes.render_with_remaster(
            frame,
            depth,
            WIDTH,
            HEIGHT,
            camera,
            scene.lights,
            scene.remaster,
        );
    } else {
        for (y, row) in frame.chunks_mut(WIDTH).enumerate() {
            row.fill(C::from_srgb8(
                if (y as f32 + 0.5) < camera.horizon(HEIGHT) {
                    0x303847
                } else {
                    0x38312b
                },
            ));
        }
    }
    timings.plane = profile_elapsed(started);
    let started = profile_start(profile);
    let [cos, sin] = camera.player.forward();
    let [right_x, right_y] = camera.player.right();
    let mut hits = Vec::new();
    for x in 0..WIDTH {
        // Direction is unnormalized, so hit distance is perpendicular to camera plane.
        let camera_x =
            (2.0 * (x as f32 + 0.5) / WIDTH as f32 - 1.0) * CameraView::half_width(WIDTH, HEIGHT);
        let direction = [cos + right_x * camera_x, sin + right_y * camera_x];
        world.ray_hits(direction, &mut hits);
        if NEAR_FIRST {
            // Keep lower wall indices first at equal depth, as in the original
            // far-to-near order: the depth test rejects later equal hits.
            hits.reverse();
            let mut start = 0;
            while start < hits.len() {
                let mut end = start + 1;
                while end < hits.len() && hits[end].0 == hits[start].0 {
                    end += 1;
                }
                hits[start..end].reverse();
                start = end;
            }
        }
        for &(distance, index, along) in &hits {
            let texture = world.walls[index].texture as u32;
            if texture == 120 {
                continue;
            }
            let half = (CameraView::focal_length(HEIGHT) / distance).min((HEIGHT * 4) as f32);
            let offset = scene.elevations.get(index).copied().unwrap_or(0.0) / 64.0;
            let origin = camera.horizon(HEIGHT) - (2.0 + offset - camera.eye_height) * half;
            let top = origin.max(0.0).min(HEIGHT as f32) as usize;
            let bottom = (origin + half * 2.0).max(0.0).min(HEIGHT as f32) as usize;
            let point = [
                camera.player.position[0] + direction[0] * distance,
                camera.player.position[1] + direction[1] * distance,
            ];
            // Authored wall samples use the independent reference's provisional
            // offset and conversion. Unmapped walls retain distance shading.
            let attenuation = planes
                .and_then(|p| {
                    world.walls[index]
                        .cell
                        .and_then(|cell| p.static_light().wall_sample(cell, along))
                })
                .unwrap_or_else(|| (1.0 / (1.0 + distance * 0.08)).clamp(0.25, 1.0));
            let light = scene.lights.map_or(0.0, |lights| lights.sample(point));
            let remaster = scene.remaster.map(|(map, fade)| {
                let wall = &world.walls[index];
                let mut normal = [wall.a[1] - wall.b[1], wall.b[0] - wall.a[0]];
                if texture >= 86
                    && normal[0] * (camera.player.position[0] - point[0])
                        + normal[1] * (camera.player.position[1] - point[1])
                        < 0.0
                {
                    normal = normal.map(|v| -v);
                }
                (map.sample_wall(point, normal), fade)
            });
            let lighting = attenuation * (1.0 + light);
            let color = C::shade_srgb8(
                ((75 + texture * 37 % 110) << 16)
                    | ((70 + texture * 23 % 100) << 8)
                    | (65 + texture * 13 % 90),
                lighting,
            );
            let image = textures.get(texture as usize).and_then(Option::as_ref);
            for y in top..bottom {
                if distance >= depth[y * WIDTH + x] {
                    continue;
                }
                let pixel = if let Some(image) = image {
                    let tx = ((along * image.width as f32) as usize).min(image.width - 1);
                    let ty = (((y as f32 - origin) / (half * 2.0).max(1.0) * image.height as f32)
                        as usize)
                        .min(image.height - 1);
                    let rgb = image.pixels[ty * image.width + tx];
                    if rgb >> 24 < 128 {
                        continue;
                    }
                    let lighting = remaster.map_or(lighting, |(static_light, fade)| {
                        let slope = (camera.horizon(HEIGHT) - (y as f32 + 0.5))
                            / CameraView::focal_length(HEIGHT);
                        let camera_distance = distance
                            * (direction[0] * direction[0]
                                + direction[1] * direction[1]
                                + slope * slope)
                                .sqrt();
                        static_light
                            * crate::remaster_light::distance_multiplier(camera_distance, fade)
                            * (1.0 + light)
                    });
                    C::shade_srgb8(rgb, lighting)
                } else {
                    color
                };
                frame[y * WIDTH + x] = pixel;
                depth[y * WIDTH + x] = distance;
            }
        }
    }
    timings.wall = profile_elapsed(started);
    let started = profile_start(profile);
    if let Some(objects) = objects {
        objects.render_deferred_with_remaster(
            frame,
            depth,
            WIDTH,
            HEIGHT,
            camera,
            scene.mines,
            scene.lights,
            planes.map(PlaneRenderer::static_light),
            transparency,
            scene.remaster,
        );
    }
    timings.model = profile_elapsed(started);
    timings
}

struct FrameScene<'a> {
    world: RenderWorld<'a>,
    textures: &'a [Option<ViewerTexture>],
    planes: &'a PlaneRenderer,
    objects: &'a ObjectScene,
    camera: CameraView,
    projectiles: &'a ProjectileRenderer,
    projectile_visuals: &'a [ProjectileVisual],
    explosions: Option<&'a crate::explosion_render::ExplosionRenderer>,
    impacts: Option<&'a crate::impact_render::ImpactRenderer>,
    trails: Option<&'a crate::projectile_trails::ProjectileTrails>,
    weapon: &'a WeaponRenderer,
    weapon_id: usize,
    weapon_pose: WeaponPose,
    weapon_frame: usize,
}
impl FrameScene<'_> {
    fn draw<C: RenderPixel>(
        &self,
        frame: &mut [C],
        depth: &mut [f32],
        transparency: &mut Translucency<C>,
        profile: bool,
    ) -> RenderTimings {
        transparency.clear();
        let mut timings = render_profiled::<true, C>(
            SceneTarget {
                frame,
                depth,
                transparency,
            },
            self.world,
            self.textures,
            Some(self.planes),
            Some(self.objects),
            profile,
        );
        let started = profile_start(profile);
        self.projectiles.render_deferred(
            frame,
            depth,
            WIDTH,
            HEIGHT,
            self.camera,
            self.projectile_visuals,
            Some(self.planes.static_light()),
            transparency,
        );
        if let Some(renderer) = self.explosions {
            renderer.render_deferred(frame, depth, WIDTH, HEIGHT, self.camera, transparency);
        }
        if let Some(renderer) = self.impacts {
            renderer.render_deferred(frame, depth, WIDTH, HEIGHT, self.camera, transparency);
        }
        if let Some(renderer) = self.trails {
            renderer.render_deferred(frame, depth, WIDTH, HEIGHT, self.camera, transparency);
        }
        transparency.composite(frame, depth);
        timings.effects = profile_elapsed(started);
        let started = profile_start(profile);
        self.weapon.render_to(
            frame,
            WIDTH,
            HEIGHT,
            self.weapon_id,
            self.weapon_pose,
            self.weapon_frame,
            self.world.remaster.map_or_else(
                || {
                    self.planes
                        .static_light()
                        .sample(self.camera.player.position)
                        .unwrap_or(1.0)
                },
                |(map, _)| map.sample_horizontal(self.camera.player.position),
            ),
        );
        timings.weapon = profile_elapsed(started);
        timings
    }
}

fn draw_header<C: UiPixel>(frame: &mut [C], label: &str) {
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
        "TAB MAP  F5 QUICKSAVE  F9 QUICKLOAD  R RESTART  ESC PAUSE",
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
    minimap: minimap::Minimap,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CheckpointCommand {
    Save,
    Load,
}
fn checkpoint_command_for_key(key: Scancode) -> Option<CheckpointCommand> {
    match key {
        Scancode::F5 => Some(CheckpointCommand::Save),
        Scancode::F9 => Some(CheckpointCommand::Load),
        _ => None,
    }
}
fn apply_checkpoint_commands(
    commands: Vec<CheckpointCommand>,
    mut apply: impl FnMut(CheckpointCommand),
) {
    for command in commands {
        apply(command);
    }
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
        objects.set_actor_shooting(
            actor.source_index,
            session.ai.ranged_fire_visible(actor.source_index),
        );
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

#[derive(Clone, Copy, PartialEq)]
struct LightingGeometry {
    a: [f32; 2],
    b: [f32; 2],
    texture: u8,
    cell: Option<[u8; 2]>,
    elevation: f32,
}
impl LightingGeometry {
    fn from_wall(wall: &crate::world::Wall, elevation: f32) -> Self {
        Self {
            a: wall.a,
            b: wall.b,
            texture: wall.texture,
            cell: wall.cell,
            elevation,
        }
    }
}
#[derive(Default)]
struct LightingCache {
    map: Option<RemasterLightMap>,
    geometry: Option<Vec<LightingGeometry>>,
    worker: Option<LightingWorker>,
    generation: u64,
}
impl LightingCache {
    fn with_worker(
        map: Arc<crate::map::Map>,
        textures: &[Option<ViewerTexture>],
    ) -> std::io::Result<Self> {
        let textures = textures
            .iter()
            .map(|texture| {
                texture.as_ref().map(|texture| {
                    OwnedShadowTexture::from_argb(texture.width, texture.height, &texture.pixels)
                })
            })
            .collect();
        Ok(Self {
            worker: Some(LightingWorker::new(map, textures)?),
            ..Self::default()
        })
    }
    fn install(
        &mut self,
        result: Result<RemasterLightMap, LightingError>,
    ) -> Result<(), LightingError> {
        match result {
            Ok(field) => {
                if field.skipped_sources() != 0 {
                    eprintln!(
                        "ignored {} malformed static light sources",
                        field.skipped_sources()
                    );
                }
                self.map = Some(field);
                Ok(())
            }
            Err(error) => {
                self.map = None;
                Err(error)
            }
        }
    }

    fn update(
        &mut self,
        map: &crate::map::Map,
        world: &World,
        elevations: &[f32],
        textures: &[Option<ViewerTexture>],
    ) -> Result<(), LightingError> {
        let mut completion_error = None;
        if let Some(result) = self.worker.as_mut().and_then(LightingWorker::try_result) {
            if result.generation > self.generation {
                self.generation = result.generation;
                completion_error = self.install(result.result).err();
            }
        }
        let current = || {
            world.walls.iter().enumerate().map(|(index, wall)| {
                LightingGeometry::from_wall(wall, elevations.get(index).copied().unwrap_or(0.0))
            })
        };
        if self
            .geometry
            .as_ref()
            .is_some_and(|previous| previous.iter().copied().eq(current()))
        {
            return completion_error.map_or(Ok(()), Err);
        }
        let initial = self.geometry.is_none();
        self.geometry = Some(current().collect());
        let elevations: Vec<_> = elevations.iter().map(|value| value / 64.0).collect();
        // Build the first field during loading; subsequent geometry changes
        // retain the latest completed field while the worker catches up.
        if !initial {
            if let Some(worker) = &mut self.worker {
                worker.submit(world.clone(), elevations);
                return completion_error.map_or(Ok(()), Err);
            }
        }
        let textures: Vec<_> = textures
            .iter()
            .map(|texture| {
                texture.as_ref().map(|texture| ShadowTexture {
                    width: texture.width,
                    height: texture.height,
                    pixels: &texture.pixels,
                })
            })
            .collect();
        self.install(RemasterLightMap::build(map, world, &elevations, &textures))
    }
}

struct UiFrame<'a> {
    hud: Option<&'a crate::hud::Hud>,
    session: &'a Session,
    damage_flash: u8,
    minimap: &'a minimap::Minimap,
    show_help: bool,
    label: &'a str,
    reflection_ticks: u8,
    current_message: Option<&'a [String]>,
    teleport_notice: Option<&'a str>,
    checkpoint_notice: Option<&'a str>,
    shot_feedback: Option<&'a str>,
    completed: bool,
    pause: &'a PauseMenu,
    look: &'a RelativeLook,
    console: &'a crate::developer_console::Console,
    graphics_menu: &'a GraphicsMenu,
    graphics: &'a GraphicsSettings,
    show_fps: bool,
    metrics: crate::frame_stats::Metrics,
}
impl UiFrame<'_> {
    fn draw<C: UiPixel>(&self, frame: &mut [C]) {
        let Self {
            hud,
            session,
            damage_flash,
            minimap,
            show_help,
            label,
            reflection_ticks,
            current_message,
            teleport_notice,
            checkpoint_notice,
            shot_feedback,
            completed,
            pause,
            look,
            console,
            graphics_menu,
            graphics,
            show_fps,
            metrics,
        } = *self;
        apply_damage_tint(frame, damage_flash);
        minimap.draw(frame, WIDTH, HEIGHT, &session.world);
        if show_help {
            draw_header(frame, label);
        }
        let cx = WIDTH / 2;
        let cy = HEIGHT / 2;
        for offset in -3isize..=3 {
            frame[cy * WIDTH + (cx as isize + offset) as usize] = C::from_rgb(0xffffff);
            frame[(cy as isize + offset) as usize * WIDTH + cx] = C::from_rgb(0xffffff);
        }
        let inventory = &session.game.inventory;
        let hud_top = if let Some(hud) = &hud {
            hud.render(
                frame,
                WIDTH,
                HEIGHT,
                crate::hud::HudState {
                    health: u32::from(inventory.health),
                    armor: u32::from(inventory.armor),
                    ammo: u32::from(inventory.ammo[session.selected_weapon]),
                    weapon: session.selected_weapon,
                },
            );
            if inventory.keys.iter().any(|key| *key) {
                text(
                    frame,
                    WIDTH - 96,
                    HEIGHT - 76,
                    &format!(
                        "KEYS {}{}{}",
                        if inventory.keys[0] { "R" } else { "-" },
                        if inventory.keys[1] { "G" } else { "-" },
                        if inventory.keys[2] { "B" } else { "-" }
                    ),
                );
            }
            HEIGHT - 64
        } else {
            text(
                frame,
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
            HEIGHT
        };
        let effects = &session.game.protective_powerups;
        let status = powerup_status(
            inventory.invisibility_ticks_remaining,
            effects.reflector_ticks_remaining,
            effects.chojin_ticks_remaining,
        );
        if !status.is_empty() {
            text(frame, 8, hud_top.saturating_sub(12), &status);
        }
        if reflection_ticks > 0 {
            text(
                frame,
                WIDTH / 2,
                hud_top.saturating_sub(24),
                "PROJECTILE REFLECTED",
            );
        }
        if let Some(lines) = current_message {
            for (row, line) in lines.iter().enumerate().take(6) {
                text(frame, 8, 52 + row * 10, line);
            }
        }
        if let Some(message) = teleport_notice {
            text(frame, 8, hud_top.saturating_sub(48), message);
        }
        if let Some(message) = checkpoint_notice {
            text(frame, 8, hud_top.saturating_sub(36), message);
        }
        if let Some(message) = shot_feedback {
            text(frame, 8, hud_top.saturating_sub(24), message);
        }
        if completed {
            text(
                frame,
                WIDTH / 2 - 112,
                HEIGHT / 2 + 35,
                "LEVEL COMPLETE - ENTER TO CONTINUE",
            );
        }
        if inventory.health == 0 {
            text(
                frame,
                WIDTH / 2 - 80,
                HEIGHT / 2 + 35,
                "YOU DIED - R TO RESTART",
            );
        }
        if pause.open {
            draw_pause_menu(frame, pause);
        } else if !look.focused {
            text(frame, WIDTH / 2 - 28, HEIGHT / 2 + 50, "PAUSED");
        } else if !look.captured && !console.is_open() && !graphics_menu.is_open() {
            text(frame, 8, HEIGHT - 66, "CLICK TO CAPTURE MOUSE");
        }
        if show_fps {
            text(
                frame,
                WIDTH - 184,
                8,
                &format!("FPS {:5.1}  {:5.1} MS", metrics.fps, metrics.frame_ms),
            );
        }
        graphics_menu.render(frame, WIDTH, HEIGHT, graphics);
        console.render(frame, WIDTH, HEIGHT);
    }
}

pub struct ClientFeedback {
    pub hud: Option<crate::hud::Hud>,
    pub map: crate::map::Map,
    pub session: Session,
    pub audio: Audio,
    pub weapon_overlay: WeaponRenderer,
    pub projectile_renderer: ProjectileRenderer,
    pub explosion_renderer: Option<crate::explosion_render::ExplosionRenderer>,
    pub impact_renderer: Option<crate::impact_render::ImpactRenderer>,
    pub projectile_trails: Option<crate::projectile_trails::ProjectileTrails>,
    pub difficulty: openchasm::pickups::Difficulty,
    pub teleport_load_error: Option<String>,
    /// Verified identity of the loaded gameplay inputs, if fingerprinting succeeded.
    pub content_digest: Option<openchasm::save_format::ContentDigest>,
    /// Checked-read ledger retained for future verified scene staging.
    pub verified_resources: Option<crate::content::VerifiedResources>,
}

pub fn run(
    textures: Vec<Option<CelImage>>,
    planes: PlaneRenderer,
    mut objects: ObjectScene,
    feedback: ClientFeedback,
    label: &str,
) -> Result<Option<openchasm::inventory::Inventory>, Box<dyn Error>> {
    let ClientFeedback {
        hud,
        map,
        mut session,
        mut audio,
        weapon_overlay,
        projectile_renderer,
        mut explosion_renderer,
        mut impact_renderer,
        mut projectile_trails,
        difficulty,
        teleport_load_error,
        content_digest: _content_digest,
        verified_resources: _verified_resources,
    } = feedback;
    let map = Arc::new(map);
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
    let mut window_builder = video.window(
        "OpenChasm - early gameplay preview",
        WIDTH as u32,
        HEIGHT as u32,
    );
    window_builder
        .resizable()
        .position_centered()
        .allow_highdpi();
    #[cfg(all(feature = "gpu", target_os = "macos"))]
    window_builder.metal_view();
    let window = window_builder.build()?;
    let mut presentation =
        Presentation::new(&window, WIDTH, HEIGHT, BackendChoice::from_environment()?)?;
    let mut event_pump = sdl.event_pump()?;
    video.text_input().stop();
    let mut look = RelativeLook {
        focused: window.has_input_focus(),
        ..RelativeLook::default()
    };
    if look.focused && !capture_mouse(&mut look, &sdl.mouse()) {
        eprintln!("relative mouse capture unavailable; click to retry or use arrow keys to turn");
    }
    let mut held = HashSet::new();
    let mut mouse_fire = false;
    let mut frame = vec![0u32; WIDTH * HEIGHT];
    let mut ui_overlay = Vec::<OverlayPixel>::new();
    let mut hdr_frame = vec![LinearRgb::default(); WIDTH * HEIGHT];
    let mut rgb_transparency = Translucency::new(WIDTH * HEIGHT);
    let mut hdr_transparency = Translucency::new(WIDTH * HEIGHT);
    let mut remaster_lighting = LightingCache::with_worker(Arc::clone(&map), &textures)
        .unwrap_or_else(|error| {
            eprintln!("background lighting unavailable; rebuilding synchronously: {error}");
            LightingCache::default()
        });
    let mut bloom = crate::bloom::Bloom::new(WIDTH, HEIGHT)?;
    let settings_store = match SettingsStore::for_sdl() {
        Ok(store) => Some(store),
        Err(error) => {
            eprintln!("graphics preference directory unavailable: {error}");
            None
        }
    };
    let mut graphics = match settings_store.as_ref().map(SettingsStore::load) {
        Some(Ok(settings)) => settings,
        Some(Err(error)) => {
            eprintln!("graphics preferences unavailable: {error}");
            GraphicsSettings::default()
        }
        None => GraphicsSettings::default(),
    };
    let mut uncapped_override = std::env::var("OPENCHASM_UNCAPPED").as_deref() == Ok("1");
    let mut color_pipeline = ColorPipeline::new(graphics.color_settings())?;
    let mut graphics_menu = GraphicsMenu::default();
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
    let mut console = crate::developer_console::Console::default();
    if let Some(notice) = presentation.take_notice() {
        eprintln!("{notice}");
        console.push(&notice);
    }
    let mut show_fps = graphics.show_fps;
    let mut mouse_sensitivity = graphics.sensitivity;
    let frame_limit = effective_fps_limit(graphics.max_fps, uncapped_override);
    let mut frame_pacer = crate::frame_stats::FramePacer::new(frame_limit, Instant::now());
    let mut pending_simulation_time = Duration::ZERO;
    let mut frame_stats = crate::frame_stats::FrameStats::new(Instant::now());
    let profile = std::env::var("OPENCHASM_PROFILE").as_deref() == Ok("1");
    let mut profile_samples = 0_u32;
    let mut stage_totals = RenderTimings::default();
    let mut stage_frames = 0_u32;
    let mut suppress_toggle_text = false;
    let mut minimap = minimap::Minimap::new(&session.world);
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
        let graphics_before = graphics.clone();
        let override_before = uncapped_override;
        let mut graphics_changed = false;
        let mut restart_requested = false;
        let mut advance_requested = false;
        let mut checkpoint_commands = Vec::new();
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
                console.close();
                graphics_menu.close();
                video.text_input().stop();
                pause.show(Some(PauseAction::Exit));
                sdl.mouse().set_relative_mouse_mode(false);
                look.captured = false;
                clear_held_controls(&mut held, &mut mouse_fire, &mut look, &mut accumulator);
                pending_interact = false;
                pending_jump = false;
                pending_weapon = None;
                advance_requested = false;
                restart_requested = false;
                checkpoint_commands.clear();
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
                if !pause.open && !console.is_open() && !graphics_menu.is_open() {
                    pause.show(None);
                }
                sdl.mouse().set_relative_mouse_mode(false);
                clear_held_controls(&mut held, &mut mouse_fire, &mut look, &mut accumulator);
                pending_interact = false;
                pending_jump = false;
                pending_weapon = None;
                advance_requested = false;
                restart_requested = false;
                checkpoint_commands.clear();
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
                if !pause.open
                    && !console.is_open()
                    && !graphics_menu.is_open()
                    && !capture_mouse(&mut look, &sdl.mouse())
                {
                    eprintln!("relative mouse capture unavailable; click to retry or use arrow keys to turn");
                }
                continue;
            }
            let console_toggle = matches!(
                event,
                Event::KeyDown {
                    scancode: Some(Scancode::Grave),
                    repeat: false,
                    ..
                }
            );
            let console_escape = console.is_open()
                && matches!(
                    event,
                    Event::KeyDown {
                        scancode: Some(Scancode::Escape),
                        repeat: false,
                        ..
                    }
                );
            if console.is_open()
                && matches!(
                    event,
                    Event::KeyDown {
                        scancode: Some(Scancode::Grave),
                        repeat: true,
                        ..
                    }
                )
            {
                suppress_toggle_text = true;
                continue;
            }
            if look.focused && (console_toggle || console_escape) {
                if console.is_open() {
                    console.close();
                    video.text_input().stop();
                    if !pause.open && !graphics_menu.is_open() {
                        capture_mouse(&mut look, &sdl.mouse());
                    }
                } else {
                    console.toggle();
                    video.text_input().start();
                    sdl.mouse().set_relative_mouse_mode(false);
                    look.release();
                }
                suppress_toggle_text = console_toggle;
                clear_held_controls(&mut held, &mut mouse_fire, &mut look, &mut accumulator);
                pending_interact = false;
                pending_jump = false;
                pending_weapon = None;
                advance_requested = false;
                restart_requested = false;
                checkpoint_commands.clear();
                frame_clock.rebase(Instant::now());
                suppress_gameplay = true;
                continue;
            }
            if console.is_open() {
                if !look.focused {
                    continue;
                }
                if let Event::TextInput { ref text, .. } = event {
                    if suppress_toggle_text && matches!(text.as_str(), "`" | "~") {
                        suppress_toggle_text = false;
                        continue;
                    }
                    suppress_toggle_text = false;
                }
                if let Some(command) = console.event(&event) {
                    use crate::developer_console::Command;
                    match command {
                        Command::Fps(value) => {
                            if let Some(value) = value {
                                show_fps = value;
                                graphics.show_fps = value;
                                graphics_changed = true;
                            }
                            console.push(&format!("fps {}", if show_fps { "on" } else { "off" }));
                        }
                        Command::HelpOverlay(value) => {
                            if let Some(value) = value {
                                graphics.show_help = value;
                                graphics_changed = true;
                            }
                            console.push(if graphics.show_help {
                                "showhelp on"
                            } else {
                                "showhelp off"
                            });
                        }
                        Command::MaxFps(value) => {
                            if let Some(value) = value {
                                graphics.max_fps = value;
                                uncapped_override = false;
                                graphics_changed = true;
                            }
                            let active_limit = if value.is_some() {
                                effective_fps_limit(graphics.max_fps, uncapped_override)
                            } else {
                                frame_pacer.limit()
                            };
                            console.push(&match active_limit {
                                Some(limit) => format!("maxfps {:.0}", 1.0 / limit.as_secs_f64()),
                                None => "maxfps 0 (uncapped)".to_owned(),
                            });
                        }
                        Command::Sensitivity(value) => {
                            if let Some(value) = value {
                                mouse_sensitivity = value;
                                graphics.sensitivity = value;
                                graphics_changed = true;
                            }
                            console
                                .push(&format!("sensitivity {mouse_sensitivity:.5} radians/pixel"));
                        }
                        Command::Options => {
                            graphics_menu.open();
                            console.close();
                            video.text_input().stop();
                            sdl.mouse().set_relative_mouse_mode(false);
                            look.release();
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
                            checkpoint_commands.clear();
                            suppress_gameplay = true;
                            frame_clock.rebase(Instant::now());
                        }
                        Command::Graphics(value) => {
                            if let Some(value) = value {
                                graphics.set_preset(value);
                                graphics_changed = true;
                            }
                            console.push(&format!("graphics {:?}", graphics.preset));
                        }
                        Command::Renderer(value) => {
                            if let Some(choice) = value {
                                presentation.set_backend(choice)?;
                                if let Some(notice) = presentation.take_notice() {
                                    eprintln!("{notice}");
                                    console.push(&notice);
                                }
                                frame_clock.rebase(Instant::now());
                                accumulator = 0.0;
                                pending_simulation_time = Duration::ZERO;
                            }
                            console.push(&format!("renderer {}", presentation.backend_name()));
                        }
                        Command::ToneMap(value) => {
                            if let Some(value) = value {
                                graphics.tone_mapping = value;
                                graphics_changed = true;
                            }
                            console.push(&format!(
                                "tonemap {}",
                                if graphics.tone_mapping { "on" } else { "off" }
                            ));
                        }
                        Command::Gamma(value)
                        | Command::Brightness(value)
                        | Command::Saturation(value)
                        | Command::Bloom(value) => {
                            let (name, field) = match command {
                                Command::Gamma(_) => ("gamma", &mut graphics.gamma),
                                Command::Brightness(_) => ("brightness", &mut graphics.brightness),
                                Command::Saturation(_) => ("saturation", &mut graphics.saturation),
                                Command::Bloom(_) => ("bloom", &mut graphics.bloom),
                                _ => unreachable!(),
                            };
                            if let Some(value) = value {
                                *field = value;
                                graphics_changed = true;
                            }
                            console.push(&format!("{name} {field:.3}"));
                            if !graphics.uses_hdr() {
                                console.push("Colour controls apply in remaster/enhanced modes");
                            }
                        }
                        Command::Status => {
                            let p = session.world.player;
                            let metrics = frame_stats.metrics;
                            console.push(&format!(
                                "pos {:.3} {:.3} feet {:.3} yaw {:.3} pitch {:.3}",
                                p.position[0],
                                p.position[1],
                                session.vertical.feet(),
                                p.angle,
                                session.pitch()
                            ));
                            console.push(&format!(
                                "{:.1} fps / {:.2} ms per frame; max {:.2} ms",
                                metrics.fps, metrics.frame_ms, metrics.max_frame_ms
                            ));
                            console.push(&format!(
                                "sim {:.2} ms; render {:.2} ms; present {:.2} ms",
                                metrics.simulation_ms, metrics.render_ms, metrics.present_ms
                            ));
                        }
                        Command::Restart => {
                            restart_requested = true;
                            console.push("Restarting level");
                            console.close();
                            video.text_input().stop();
                            if !pause.open && !graphics_menu.is_open() {
                                capture_mouse(&mut look, &sdl.mouse());
                            }
                            suppress_gameplay = true;
                        }
                        Command::Quit => {
                            running = false;
                        }
                    }
                }
                continue;
            }
            if graphics_menu.is_open() {
                if look.focused && !suppress_gameplay {
                    match graphics_menu.event(&event, &mut graphics) {
                        MenuAction::None => {}
                        MenuAction::Changed => graphics_changed = true,
                        MenuAction::Closed => {
                            clear_held_controls(
                                &mut held,
                                &mut mouse_fire,
                                &mut look,
                                &mut accumulator,
                            );
                            frame_clock.rebase(Instant::now());
                            suppress_gameplay = true;
                            if !pause.open {
                                capture_mouse(&mut look, &sdl.mouse());
                            }
                        }
                    }
                }
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
                            if !capture_mouse(&mut look, &sdl.mouse()) {
                                eprintln!("relative mouse capture unavailable; click to retry or use arrow keys to turn");
                            }
                            frame_clock.rebase(Instant::now());
                            suppress_gameplay = true;
                        }
                        PauseAction::Save => {
                            checkpoint_commands.push(CheckpointCommand::Save);
                            suppress_gameplay = true;
                        }
                        PauseAction::Load => {
                            checkpoint_commands.push(CheckpointCommand::Load);
                            suppress_gameplay = true;
                        }
                        PauseAction::Options => {
                            graphics_menu.open();
                            clear_held_controls(
                                &mut held,
                                &mut mouse_fire,
                                &mut look,
                                &mut accumulator,
                            );
                            pending_interact = false;
                            pending_jump = false;
                            pending_weapon = None;
                            checkpoint_commands.clear();
                            advance_requested = false;
                            frame_clock.rebase(Instant::now());
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
                        } else if let Some(command) = checkpoint_command_for_key(code) {
                            checkpoint_commands.push(command);
                        } else if code == Scancode::Tab {
                            minimap.open = !minimap.open;
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
                        if !capture_mouse(&mut look, &sdl.mouse()) {
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
                Event::MouseMotion { xrel, yrel, .. } => {
                    look.relative_motion(xrel, yrel);
                }
                _ => {}
            }
        }
        if graphics_changed {
            match graphics
                .validate()
                .and_then(|_| ColorPipeline::new(graphics.color_settings()))
            {
                Ok(pipeline) => {
                    if graphics.max_fps != graphics_before.max_fps {
                        uncapped_override = false;
                    }
                    color_pipeline = pipeline;
                    if let Some(store) = &settings_store {
                        if let Err(error) = store.save(&graphics) {
                            console.push(&format!("Could not save graphics settings: {error}"));
                            checkpoint_notice = Some(("GRAPHICS SETTINGS NOT SAVED", 3.0));
                        }
                    } else {
                        console.push("Graphics settings apply for this session only");
                    }
                }
                Err(error) => {
                    graphics = graphics_before;
                    uncapped_override = override_before;
                    console.push(&format!("Graphics change rejected: {error}"));
                }
            }
            show_fps = graphics.show_fps;
            mouse_sensitivity = graphics.sensitivity;
            frame_pacer.set_limit(
                effective_fps_limit(graphics.max_fps, uncapped_override),
                Instant::now(),
            );
        }
        if !running {
            break;
        }
        let active = simulation_active(
            look.focused,
            pause.open || console.is_open() || graphics_menu.is_open(),
            suppress_gameplay,
        );
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
            minimap = minimap::Minimap::new(&session.world);
            if let Some(renderer) = &mut explosion_renderer {
                renderer.clear();
            }
            if let Some(renderer) = &mut impact_renderer {
                renderer.clear();
            }
            if let Some(renderer) = &mut projectile_trails {
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
        apply_checkpoint_commands(checkpoint_commands, |command| {
            match command {
                CheckpointCommand::Save => {
                    quicksave = Some(QuickSave {
                        session: session.clone(),
                        scene: objects.snapshot(),
                        minimap: minimap.clone(),
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
                            minimap = saved.minimap.clone();
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
                            if let Some(renderer) = &mut impact_renderer {
                                renderer.clear();
                            }
                            if let Some(renderer) = &mut projectile_trails {
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
        });
        if completed && advance_requested && !pause.open {
            sdl.mouse().set_relative_mouse_mode(false);
            return Ok(Some(session.game.inventory.clone()));
        }
        let simulation_started = Instant::now();
        let movement = held_movement(&held);
        let firing = mouse_fire;
        while accumulator >= STEP {
            // Deltas are motion, not velocity: consume exactly once per tick.
            let yaw_delta = look.take_yaw() * (mouse_sensitivity / MOUSE_SENSITIVITY);
            let pitch_delta = look.take_pitch() * (mouse_sensitivity / MOUSE_SENSITIVITY);
            animation_tick = animation_tick.saturating_add(1);
            let old_weapon = session.selected_weapon;
            let health_before = session.game.inventory.health;
            let enemy_health = crate::audio::enemy_health_before(&session);
            let trail_before = projectile_trails
                .as_ref()
                .map(|_| crate::trail_motion::TrailSnapshot::capture(&session));
            let events = session.step(SessionInput {
                movement,
                yaw_delta,
                pitch_delta,
                interact: pending_interact,
                jump: pending_jump,
                fire: firing,
                select_weapon: pending_weapon,
            });
            if let (Some(renderer), Some(before)) = (&mut projectile_trails, trail_before) {
                renderer.tick();
                crate::trail_motion::emit(renderer, &before, &session, &events);
            }
            minimap.observe(&session.world);
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
            if let Some(renderer) = &mut impact_renderer {
                renderer.tick();
                if let Ok(rocket_id) = usize::try_from(rocket_type) {
                    renderer.consume(
                        &events,
                        CameraView {
                            player: session.world.player,
                            eye_height: session.vertical.eye(),
                            pitch: session.pitch(),
                        },
                        rocket_id,
                    );
                }
            }
            for id in crate::audio::gameplay_sound_ids(&events, rocket_type) {
                audio.play(id);
            }
            dispatch_object_breaks(&events.object_destructions, |sound, position| {
                audio.play_object_break(sound, position, session.world.player.position);
            });
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
        pending_simulation_time += simulation_started.elapsed();
        if !frame_pacer.should_render(Instant::now()) {
            let delay = frame_pacer.wait_hint(Instant::now());
            if !delay.is_zero() {
                std::thread::sleep(delay);
            }
            continue;
        }
        let simulation_time = std::mem::take(&mut pending_simulation_time);
        if graphics.uses_hdr() {
            if let Err(error) =
                remaster_lighting.update(&map, &session.world, &session.wall_elevations, &textures)
            {
                eprintln!("remaster lighting unavailable: {error}");
                console.push(&format!("Remaster lighting unavailable: {error}"));
                checkpoint_notice = Some(("REMASTER LIGHTING UNAVAILABLE", 4.0));
            }
        }
        let render_started = Instant::now();
        audio.update_ambient(session.ambient_grid(), session.world.player.position, 0.0);
        let quake_power = session
            .scripted_objects
            .as_ref()
            .and_then(|state| state.quake.sample(session.world.player.position))
            .unwrap_or(0);
        let (mut camera, yaw_offset) = quake_camera(
            session.world.player,
            session.vertical.eye(),
            quake_power,
            animation_tick,
        );
        camera.pitch = session.pitch();
        let dynamic_lights = session.light_sources.as_ref().map(|sources| {
            openchasm::dynamic_light::DynamicLightSampler::from_snapshot(&sources.snapshot())
        });
        let projectile_visuals: Vec<_> = session
            .projectiles
            .active()
            .iter()
            .map(|projectile| ProjectileVisual {
                rocket_type: projectile.rocket_id,
                elapsed_seconds: projectile.elapsed_seconds(),
                position: [
                    projectile.position[0],
                    projectile.position[1],
                    session.projectile_height(projectile.id).unwrap_or(1.0),
                ],
                yaw: projectile.velocity[1].atan2(projectile.velocity[0]),
                pitch: session.projectile_pitch(projectile.id).unwrap_or(0.0),
            })
            .collect();
        let (pose, frame_index) = weapon_clock
            .pose(weapon_overlay.frame_count(session.selected_weapon, WeaponPose::Idle));
        let scene = FrameScene {
            world: RenderWorld {
                world: &session.world,
                elevations: &session.wall_elevations,
                eye_height: camera.eye_height,
                yaw_offset,
                pitch: camera.pitch,
                mines: session.mines(),
                lights: dynamic_lights.as_ref(),
                remaster: graphics
                    .uses_hdr()
                    .then(|| {
                        remaster_lighting
                            .map
                            .as_ref()
                            .map(|map| (map, if graphics.tone_mapping { 10.0 } else { 5.0 }))
                    })
                    .flatten(),
            },
            textures: &textures,
            planes: &planes,
            objects: &objects,
            camera,
            projectiles: &projectile_renderer,
            projectile_visuals: &projectile_visuals,
            explosions: explosion_renderer.as_ref(),
            impacts: impact_renderer.as_ref(),
            trails: projectile_trails.as_ref(),
            weapon: &weapon_overlay,
            weapon_id: session.selected_weapon,
            weapon_pose: pose,
            weapon_frame: frame_index,
        };
        let mut stage_timings = if graphics.uses_hdr() {
            scene.draw(&mut hdr_frame, &mut depth, &mut hdr_transparency, profile)
        } else {
            scene.draw(&mut frame, &mut depth, &mut rgb_transparency, profile)
        };
        let use_gpu = presentation.is_gpu();
        if graphics.uses_hdr() && !use_gpu {
            let started = profile_start(profile);
            if graphics.bloom > 0.0 {
                bloom.apply(&mut hdr_frame, graphics.bloom, color_pipeline.exposure())?;
            }
            color_pipeline.encode_frame(&hdr_frame, &mut frame)?;
            stage_timings.post = profile_elapsed(started);
        }
        if profile {
            stage_totals.accumulate(stage_timings);
            stage_frames = stage_frames.saturating_add(1);
        }
        let ui = UiFrame {
            hud: hud.as_ref(),
            session: &session,
            damage_flash: damage_feedback.flash,
            minimap: &minimap,
            show_help: graphics.show_help,
            label,
            reflection_ticks,
            current_message: current_message.as_ref().map(|(lines, _)| lines.as_slice()),
            teleport_notice: teleport_notice
                .as_ref()
                .map(|(message, _)| message.as_str()),
            checkpoint_notice: checkpoint_notice.map(|(message, _)| message),
            shot_feedback: shot_feedback.map(|(message, _)| message),
            completed,
            pause: &pause,
            look: &look,
            console: &console,
            graphics_menu: &graphics_menu,
            graphics: &graphics,
            show_fps,
            metrics: frame_stats.metrics,
        };
        if use_gpu {
            ui_overlay.resize(WIDTH * HEIGHT, OverlayPixel::default());
            ui_overlay.fill(OverlayPixel::default());
            ui.draw(&mut ui_overlay);
        } else {
            ui.draw(&mut frame);
        }
        let render_time = render_started.elapsed();
        let present_started = Instant::now();
        let mut backend_notice = None;
        let presented = if use_gpu {
            #[cfg(feature = "gpu")]
            let packed_overlay = OverlayPixel::as_packed(&ui_overlay);
            #[cfg(not(feature = "gpu"))]
            let packed_overlay: &[[f32; 4]] = &[];
            let result = if graphics.uses_hdr() {
                presentation.present_hdr(
                    &hdr_frame,
                    graphics.color_settings(),
                    graphics.bloom,
                    packed_overlay,
                )
            } else {
                presentation.present_rgb(&frame, packed_overlay)
            };
            match result {
                Ok(presented) => presented,
                Err(error) => {
                    backend_notice = Some(format!("GPU presentation failed; using SDL: {error}"));
                    presentation.fallback_to_sdl()?;
                    if graphics.uses_hdr() {
                        if graphics.bloom > 0.0 {
                            bloom.apply(
                                &mut hdr_frame,
                                graphics.bloom,
                                color_pipeline.exposure(),
                            )?;
                        }
                        color_pipeline.encode_frame(&hdr_frame, &mut frame)?;
                    }
                    ui.draw(&mut frame);
                    presentation.present_sdl(&frame)?;
                    true
                }
            }
        } else {
            presentation.present_sdl(&frame)?;
            true
        };
        if let Some(notice) = backend_notice {
            eprintln!("{notice}");
            console.push(&notice);
        }
        if let Some(metrics) = presented
            .then(|| {
                frame_stats.record(
                    Instant::now(),
                    simulation_time,
                    render_time,
                    present_started.elapsed(),
                )
            })
            .flatten()
        {
            profile_samples = profile_samples.wrapping_add(1);
            if profile && profile_samples.is_multiple_of(4) {
                let ms = |value| RenderTimings::ms_per_frame(value, stage_frames);
                eprintln!("frame profile: {:.1} fps, {:.2} ms/frame; sim {:.2}, render {:.2}, present {:.2}, max {:.2} ms; plane {:.2}, wall {:.2}, model {:.2}, effects {:.2}, weapon {:.2}, post {:.2} ms", metrics.fps, metrics.frame_ms, metrics.simulation_ms, metrics.render_ms, metrics.present_ms, metrics.max_frame_ms, ms(stage_totals.plane), ms(stage_totals.wall), ms(stage_totals.model), ms(stage_totals.effects), ms(stage_totals.weapon), ms(stage_totals.post));
            }
            stage_totals = RenderTimings::default();
            stage_frames = 0;
        }
        frame_pacer.presented(render_started);
        let delay = frame_pacer.wait_hint(Instant::now());
        if !delay.is_zero() {
            std::thread::sleep(delay);
        }
    }
    sdl.mouse().set_relative_mouse_mode(false);
    video.text_input().stop();
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn capture_recenter_does_not_turn_the_camera() {
        let mut look = RelativeLook {
            focused: true,
            ..RelativeLook::default()
        };
        assert!(look.capture_with(|| true));
        look.relative_motion(-341, -185);
        assert_eq!(look.take_yaw(), 0.0);
        assert_eq!(look.take_pitch(), 0.0);
        look.relative_motion(10, -4);
        assert!((look.take_yaw() + 10.0 * MOUSE_SENSITIVITY).abs() < 1e-6);
        assert!((look.take_pitch() - 4.0 * MOUSE_SENSITIVITY).abs() < 1e-6);
        look.release();
        look.relative_motion(500, 500);
        assert!(look.capture_with(|| true));
        look.relative_motion(200, 100);
        assert_eq!(look.take_yaw(), 0.0);
        assert_eq!(look.take_pitch(), 0.0);
    }

    #[test]
    fn environment_uncapped_override_leaves_saved_cap_unchanged() {
        let mut settings = GraphicsSettings::default();
        let mut uncapped_override = true;
        assert_eq!(
            effective_fps_limit(settings.max_fps, uncapped_override),
            None
        );
        settings.gamma = 1.3;
        assert_eq!(settings.max_fps, 60);
        assert_eq!(
            effective_fps_limit(settings.max_fps, uncapped_override),
            None
        );
        // An explicit user cap (including the same saved value) cancels override.
        settings.max_fps = 60;
        uncapped_override = false;
        assert_eq!(
            effective_fps_limit(settings.max_fps, uncapped_override),
            fps_limit(60)
        );
        settings.max_fps = 0;
        assert_eq!(effective_fps_limit(settings.max_fps, false), None);
    }

    #[test]
    fn render_timing_averages_and_disabled_clock() {
        let mut total = RenderTimings::default();
        total.accumulate(RenderTimings {
            plane: Duration::from_millis(3),
            wall: Duration::from_millis(8),
            ..RenderTimings::default()
        });
        total.accumulate(RenderTimings {
            plane: Duration::from_millis(5),
            wall: Duration::from_millis(10),
            ..RenderTimings::default()
        });
        assert_eq!(RenderTimings::ms_per_frame(total.plane, 2), 4.0);
        assert_eq!(RenderTimings::ms_per_frame(total.wall, 2), 9.0);
        assert_eq!(RenderTimings::ms_per_frame(total.wall, 0), 0.0);
        assert!(profile_start(false).is_none());
        assert_eq!(profile_elapsed(None), Duration::ZERO);
    }
    #[test]
    fn same_batch_checkpoint_keys_preserve_save_then_load_order() {
        let commands: Vec<_> = [Scancode::F5, Scancode::F9]
            .into_iter()
            .filter_map(checkpoint_command_for_key)
            .collect();
        assert_eq!(commands, [CheckpointCommand::Save, CheckpointCommand::Load]);
        let mut live_state = 42;
        let mut saved = Some(7);
        apply_checkpoint_commands(commands, |command| match command {
            CheckpointCommand::Save => saved = Some(live_state),
            CheckpointCommand::Load => live_state = saved.unwrap(),
        });
        assert_eq!(live_state, 42);
        assert_eq!(saved, Some(42));
    }
    #[test]
    fn committed_break_sounds_use_event_order_and_pretransition_metadata() {
        use openchasm::session::{ObjectDestructionCause, ObjectDestructionEvent};
        let events = [
            ObjectDestructionEvent {
                cell: [3, 4],
                raw_type: 163,
                position: [3.5, 4.5],
                sound_id: 81,
                cause: ObjectDestructionCause::PlayerHitscan,
            },
            ObjectDestructionEvent {
                cell: [3, 4],
                raw_type: 164,
                position: [4.0, 4.5],
                sound_id: 70,
                cause: ObjectDestructionCause::Script { process_id: 7 },
            },
        ];
        let mut played = Vec::new();
        dispatch_object_breaks(&events, |id, position| played.push((id, position)));
        assert_eq!(played, vec![(81, [3.5, 4.5]), (70, [4.0, 4.5])]);
        played.clear();
        dispatch_object_breaks(&[], |id, position| played.push((id, position)));
        assert!(played.is_empty());
    }
    #[test]
    fn pause_menu_navigation_confirm_and_checkpoint_shortcuts() {
        let mut menu = PauseMenu::default();
        menu.show(None);
        assert_eq!(menu.key(Scancode::F5), Some(PauseAction::Save));
        assert_eq!(menu.key(Scancode::F9), Some(PauseAction::Load));
        assert_eq!(menu.key(Scancode::W), None);
        menu.key(Scancode::Up);
        assert_eq!(menu.selected, 5);
        assert_eq!(menu.key(Scancode::Return), None);
        assert_eq!(menu.confirm, Some(PauseAction::Exit));
        assert_eq!(menu.key(Scancode::F5), None);
        assert_eq!(menu.key(Scancode::Escape), None);
        assert_eq!(menu.confirm, None);
        menu.selected = 3;
        assert_eq!(menu.key(Scancode::Return), Some(PauseAction::Options));
        menu.selected = 4;
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
    fn mouse_capture_follows_focus_resume_and_resets_old_motion() {
        let mut look = RelativeLook::default();
        assert!(!look.capture_with(|| panic!("unfocused capture must not run")));
        look.set_focus(true);
        assert!(look.capture_with(|| true));
        look.motion(20);
        assert!((look.take_yaw() + 0.04).abs() < 1e-6);
        look.motion(80);
        look.set_focus(false);
        assert_eq!(look.take_yaw(), 0.0);
        look.set_focus(true);
        assert!(!look.capture_with(|| false));
        look.motion(10);
        assert_eq!(look.take_yaw(), 0.0);
        assert!(look.capture_with(|| true));
        look.motion(-10);
        assert!((look.take_yaw() - 0.02).abs() < 1e-6);
        look.motion(100);
        assert!(look.capture_with(|| true));
        assert_eq!(look.take_yaw(), 0.0);
    }

    #[test]
    fn relative_motion_is_consumed_once_and_cleared_on_focus_loss() {
        let mut look = RelativeLook::default();
        look.motion(100);
        assert_eq!(look.take_yaw(), 0.0);
        look.set_focus(true);
        look.captured = true;
        look.motion(10);
        look.motion(-4);
        assert!((look.take_yaw() + 0.012).abs() < 1e-6);
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
    fn vertical_mouse_motion_is_up_positive_and_discarded_on_focus_loss() {
        let mut look = RelativeLook::default();
        look.set_focus(true);
        look.captured = true;
        look.motion_y(-12);
        look.motion_y(2);
        assert!((look.take_pitch() - 0.02).abs() < 1e-6);
        assert_eq!(look.take_pitch(), 0.0);
        look.motion_y(-30);
        look.set_focus(false);
        assert_eq!(look.take_pitch(), 0.0);
    }

    #[test]
    fn near_first_walls_match_original_compositing() {
        let mut bytes = vec![0; 0x27001 + 4];
        bytes[0x23001..0x25001].fill(0);
        let map = crate::map::Map::parse(&bytes).unwrap();
        let floors = crate::map::Floors::parse(&vec![0; 64 + 64 * 64 * 86]).unwrap();
        let palette = crate::texture::Palette::parse(&[0; 768]).unwrap();
        let planes = PlaneRenderer::new(&map, &floors, &palette);
        let wall = |distance, texture| Wall {
            cell: None,
            a: [distance, 15.0],
            b: [distance, 18.0],
            texture,
            blocks_movement: true,
        };
        let world = World {
            walls: vec![
                wall(18.5, 87),
                wall(18.5, 1),
                wall(19.5, 2),
                wall(20.5, 1),
                wall(17.5, 120),
            ],
            player: Player {
                position: [16.5, 16.5],
                angle: 0.0,
            },
        };
        let mut textures: Vec<Option<ViewerTexture>> = (0..128).map(|_| None).collect();
        textures[1] = Some(ViewerTexture {
            width: 1,
            height: 1,
            pixels: vec![0xffff0000],
        });
        textures[2] = Some(ViewerTexture {
            width: 1,
            height: 1,
            pixels: vec![0xff00ff00],
        });
        textures[87] = Some(ViewerTexture {
            width: 1,
            height: 2,
            pixels: vec![0, 0xff0000ff],
        });
        for pitch in [-0.25, 0.0, 0.3] {
            let scene = || RenderWorld {
                world: &world,
                elevations: &[0.0, 0.0, 48.0, -16.0, 0.0],
                eye_height: 1.0,
                pitch,
                yaw_offset: 0.0,
                mines: &[],
                lights: None,
                remaster: None,
            };
            let mut old_frame = vec![0; WIDTH * HEIGHT];
            let mut old_depth = vec![0.0; old_frame.len()];
            let mut new_frame = old_frame.clone();
            let mut new_depth = old_depth.clone();
            render_profiled::<false, u32>(
                SceneTarget {
                    frame: &mut old_frame,
                    depth: &mut old_depth,
                    transparency: &mut Translucency::new(WIDTH * HEIGHT),
                },
                scene(),
                &textures,
                Some(&planes),
                None,
                false,
            );
            render_profiled::<true, u32>(
                SceneTarget {
                    frame: &mut new_frame,
                    depth: &mut new_depth,
                    transparency: &mut Translucency::new(WIDTH * HEIGHT),
                },
                scene(),
                &textures,
                Some(&planes),
                None,
                false,
            );
            assert_eq!(old_frame, new_frame, "colors differ at pitch {pitch}");
            assert_eq!(old_depth, new_depth, "depth differs at pitch {pitch}");
        }
    }

    #[test]
    fn every_wall_material_uses_the_authored_alpha_cutoff() {
        let wall = |distance, texture| Wall {
            a: [distance, -2.0],
            b: [distance, 2.0],
            texture,
            blocks_movement: true,
            cell: None,
        };
        for texture in [1usize, 85, 86, 87, 119] {
            for alpha in [0u32, 127, 128, 255] {
                let world = World {
                    walls: vec![wall(1.0, texture as u8), wall(2.0, 2)],
                    player: Player {
                        position: [0.0, 0.0],
                        angle: 0.0,
                    },
                };
                let mut textures: Vec<Option<ViewerTexture>> = (0..120).map(|_| None).collect();
                textures[texture] = Some(ViewerTexture {
                    width: 1,
                    height: 1,
                    pixels: vec![alpha << 24 | 0xff0000],
                });
                textures[2] = Some(ViewerTexture {
                    width: 1,
                    height: 1,
                    pixels: vec![0xff0000ff],
                });
                let mut frame = vec![0; WIDTH * HEIGHT];
                let mut depth = vec![f32::INFINITY; frame.len()];
                render(
                    &mut frame,
                    &mut depth,
                    RenderWorld {
                        world: &world,
                        elevations: &[],
                        eye_height: 1.0,
                        pitch: 0.0,
                        yaw_offset: 0.0,
                        mines: &[],
                        lights: None,
                        remaster: None,
                    },
                    &textures,
                    None,
                    None,
                );
                let pixel = frame[(HEIGHT / 2) * WIDTH + WIDTH / 2];
                if alpha < 128 {
                    assert_eq!(pixel & 0xff0000, 0);
                    assert_ne!(pixel & 255, 0);
                } else {
                    assert_ne!(pixel & 0xff0000, 0);
                    assert_eq!(pixel & 255, 0);
                }
            }
        }
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
                pitch: 0.0,
                yaw_offset: 0.0,
                mines: &[],
                lights: None,
                remaster: None,
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
            pitch: 0.0,
            yaw_offset: 0.0,
            mines: &[],
            lights,
            remaster: None,
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
    fn looking_up_shifts_wall_projection_without_changing_distance() {
        let world = World {
            walls: vec![Wall {
                cell: None,
                a: [4.0, -1.0],
                b: [4.0, 1.0],
                texture: 1,
                blocks_movement: true,
            }],
            player: Player {
                position: [0.0, 0.0],
                angle: 0.0,
            },
        };
        let mut frame = vec![0; WIDTH * HEIGHT];
        let mut depth = vec![f32::INFINITY; frame.len()];
        let scene = |pitch| RenderWorld {
            world: &world,
            elevations: &[],
            eye_height: 1.0,
            yaw_offset: 0.0,
            pitch,
            mines: &[],
            lights: None,
            remaster: None,
        };
        render(&mut frame, &mut depth, scene(0.0), &[], None, None);
        let top = |depth: &[f32]| {
            (0..HEIGHT)
                .find(|&y| depth[y * WIDTH + WIDTH / 2].is_finite())
                .unwrap()
        };
        let level_top = top(&depth);
        let original_distance = depth[(level_top + 25) * WIDTH + WIDTH / 2];
        render(
            &mut frame,
            &mut depth,
            scene(0.2_f32.atan()),
            &[],
            None,
            None,
        );
        let up_top = top(&depth);
        assert!(
            ((up_top - level_top) as f32 - CameraView::focal_length(HEIGHT) * 0.2).abs() <= 1.0
        );
        assert_eq!(depth[(up_top + 25) * WIDTH + WIDTH / 2], original_distance);
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
                pitch: 0.0,
                yaw_offset: 0.0,
                mines: &[],
                lights: None,
                remaster: None,
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
                pitch: 0.0,
                yaw_offset: 0.0,
                mines: &[],
                lights: None,
                remaster: None,
            },
            &[],
            None,
            None,
        );
        assert!(top(&depth) > lower_camera_top + 10);
    }
    #[test]
    fn wall_to_world_right_renders_on_screen_right() {
        let world = World {
            walls: vec![Wall {
                a: [29.25, 54.5],
                b: [29.75, 54.5],
                texture: 1,
                blocks_movement: true,
                cell: None,
            }],
            player: Player {
                position: [31.5, 58.5],
                angle: 3.0 * std::f32::consts::FRAC_PI_2,
            },
        };
        let mut frame = vec![0; WIDTH * HEIGHT];
        let mut depth = vec![f32::INFINITY; frame.len()];
        render(
            &mut frame,
            &mut depth,
            RenderWorld {
                world: &world,
                elevations: &[],
                eye_height: 1.0,
                yaw_offset: 0.0,
                pitch: 0.0,
                mines: &[],
                lights: None,
                remaster: None,
            },
            &[],
            None,
            None,
        );
        let columns: Vec<_> = depth
            .iter()
            .enumerate()
            .filter(|(_, d)| d.is_finite())
            .map(|(i, _)| i % WIDTH)
            .collect();
        assert!(!columns.is_empty());
        assert!(columns.iter().all(|&x| x > WIDTH / 2));
    }

    #[test]
    fn authored_wall_light_changes_shading_without_changing_depth() {
        let make_planes = |value| {
            let mut bytes = vec![0; 0x27001 + 4];
            bytes[0x10001 - 2..0x18001 - 2].fill(value);
            bytes[0x23001..0x25001].fill(63);
            let map = crate::map::Map::parse(&bytes).unwrap();
            let floors = crate::map::Floors::parse(&vec![0; 64 + 64 * 64 * 86]).unwrap();
            let palette = crate::texture::Palette::parse(&[0; 768]).unwrap();
            PlaneRenderer::new(&map, &floors, &palette)
        };
        let dark = make_planes(35);
        let bright = make_planes(5);
        let world = World {
            walls: vec![Wall {
                a: [2.0, -1.0],
                b: [2.0, 1.0],
                texture: 1,
                blocks_movement: true,
                cell: Some([2, 2]),
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
                pixels: vec![0xff808080],
            }),
        ];
        let mut frame = vec![0; WIDTH * HEIGHT];
        let mut depth = vec![f32::INFINITY; frame.len()];
        let scene = || RenderWorld {
            world: &world,
            elevations: &[],
            eye_height: 1.0,
            yaw_offset: 0.0,
            pitch: 0.0,
            mines: &[],
            lights: None,
            remaster: None,
        };
        render(
            &mut frame,
            &mut depth,
            scene(),
            &textures,
            Some(&dark),
            None,
        );
        let pixel = HEIGHT / 2 * WIDTH + WIDTH / 2;
        let dark_color = frame[pixel];
        let wall_depth = depth[pixel];
        render(
            &mut frame,
            &mut depth,
            scene(),
            &textures,
            Some(&bright),
            None,
        );
        assert!(frame[pixel] > dark_color);
        assert_eq!(depth[pixel], wall_depth);
    }

    #[test]
    fn pitched_ceiling_occludes_the_top_of_an_elevated_wall() {
        let mut bytes = vec![0; 0x27001 + 4];
        bytes[0x23001..0x25001].fill(0);
        let map = crate::map::Map::parse(&bytes).unwrap();
        let mut floor_bytes = vec![0; 64 + 64 * 64 * 86];
        floor_bytes[64..64 + 4096].fill(1);
        let floors = crate::map::Floors::parse(&floor_bytes).unwrap();
        let mut palette_bytes = vec![0; 768];
        palette_bytes[4] = 63;
        let palette = crate::texture::Palette::parse(&palette_bytes).unwrap();
        let planes = PlaneRenderer::new(&map, &floors, &palette);
        let world = World {
            walls: vec![Wall {
                a: [20.5, 15.5],
                b: [20.5, 17.5],
                texture: 1,
                blocks_movement: true,
                cell: None,
            }],
            player: Player {
                position: [16.5, 16.5],
                angle: 0.0,
            },
        };
        let textures = vec![
            None,
            Some(ViewerTexture {
                width: 1,
                height: 1,
                pixels: vec![0xffff0000],
            }),
        ];
        let mut frame = vec![0; WIDTH * HEIGHT];
        let mut depth = vec![f32::INFINITY; frame.len()];
        render(
            &mut frame,
            &mut depth,
            RenderWorld {
                world: &world,
                elevations: &[64.0],
                eye_height: 1.0,
                yaw_offset: 0.0,
                pitch: 0.2_f32.atan(),
                mines: &[],
                lights: None,
                remaster: None,
            },
            &textures,
            Some(&planes),
            None,
        );
        let upper = 150 * WIDTH + WIDTH / 2;
        let middle = 225 * WIDTH + WIDTH / 2;
        assert!(depth[upper] < 4.0);
        assert_eq!(frame[upper] & 0xff0000, 0);
        assert_eq!(depth[middle], 4.0);
        assert!(frame[middle] & 0xff0000 > 0);
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
                pitch: 0.0,
                yaw_offset: 0.0,
                mines: &[],
                lights: None,
                remaster: None,
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
                pixels: vec![0xffff0000],
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
                pitch: 0.0,
                yaw_offset: 0.0,
                mines: &[],
                lights: None,
                remaster: None,
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
    fn right_controls_match_original_screen_right_basis() {
        let movement = held_movement(&HashSet::from([Scancode::D, Scancode::Right]));
        assert_eq!(movement.strafe, 1.0);
        assert_eq!(movement.turn, -1.0);
        let movement = held_movement(&HashSet::from([Scancode::A, Scancode::Left]));
        assert_eq!(movement.strafe, -1.0);
        assert_eq!(movement.turn, 1.0);
        let mut look = RelativeLook {
            focused: true,
            captured: true,
            ..Default::default()
        };
        look.motion(10);
        assert!(look.take_yaw() < 0.0);
    }

    #[test]
    fn restart_clears_movement_fire_yaw_and_time_without_releasing_capture() {
        let mut held = HashSet::from([Scancode::W, Scancode::D, Scancode::Right]);
        let mut fire = true;
        let mut look = RelativeLook {
            focused: true,
            captured: true,
            pending_dx: 42,
            pending_dy: 0,
            ..RelativeLook::default()
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
            ..RelativeLook::default()
        };
        assert_eq!(wheel_weapon(&look, &inventory, 0, 1), Some(1));
        look.captured = true;
        assert_eq!(wheel_weapon(&look, &inventory, 0, 1), Some(1));
        look.set_focus(false);
        assert_eq!(wheel_weapon(&look, &inventory, 0, 1), None);
    }
}
