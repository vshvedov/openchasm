//! Software-rendered geometry viewer. Game rules live outside the window layer.
use crate::audio::Audio;
use crate::object_render::{ActorAnimationMode, ObjectScene};
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
    mouse::MouseButton,
    pixels::PixelFormatEnum,
};
use std::collections::HashSet;
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

fn render(
    frame: &mut [u32],
    depth: &mut [f32],
    world: &World,
    textures: &[Option<ViewerTexture>],
    planes: Option<&PlaneRenderer>,
    objects: Option<&ObjectScene>,
    label: &str,
) {
    if let Some(planes) = planes {
        planes.render(
            frame,
            WIDTH,
            HEIGHT,
            world.player.position,
            world.player.angle,
        );
    } else {
        for (y, row) in frame.chunks_mut(WIDTH).enumerate() {
            row.fill(if y < HEIGHT / 2 { 0x303847 } else { 0x38312b });
        }
    }
    depth.fill(f32::INFINITY);
    let (sin, cos) = world.player.angle.sin_cos();
    let mut hits = Vec::new();
    for x in 0..WIDTH {
        // Direction is unnormalized, so hit distance is perpendicular to camera plane.
        let camera = (2.0 * x as f32 / WIDTH as f32 - 1.0) * 0.66;
        let direction = [cos - sin * camera, sin + cos * camera];
        world.ray_hits(direction, &mut hits);
        for &(distance, index, along) in &hits {
            let texture = world.walls[index].texture as u32;
            if texture == 120 {
                continue;
            }
            let half = (HEIGHT as f32 * 0.5 / distance).min(HEIGHT as f32) as usize;
            let top = (HEIGHT / 2).saturating_sub(half);
            let bottom = (HEIGHT / 2 + half).min(HEIGHT);
            let attenuation = (1.0 / (1.0 + distance * 0.08)).clamp(0.25, 1.0);
            let channel = |base: u32| ((base as f32 * attenuation) as u32).min(255);
            let color = (channel(75 + texture * 37 % 110) << 16)
                | (channel(70 + texture * 23 % 100) << 8)
                | channel(65 + texture * 13 % 90);
            let image = textures.get(texture as usize).and_then(Option::as_ref);
            for y in top..bottom {
                let pixel = if let Some(image) = image {
                    let tx = ((along * image.width as f32) as usize).min(image.width - 1);
                    let height = (half * 2).max(1);
                    let screen_origin = HEIGHT as isize / 2 - half as isize;
                    let ty = (((y as isize - screen_origin) as usize * image.height) / height)
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
        objects.render(frame, depth, WIDTH, HEIGHT, world.player);
    }
    text(frame, 8, 8, label);
    text(
        frame,
        8,
        20,
        "WASD MOVE  MOUSE/ARROWS TURN  E USE  SPACE FIRE  1-8  ESC QUIT",
    );
    text(
        frame,
        8,
        32,
        "EARLY GAMEPLAY PREVIEW - BEHAVIOR IS PROVISIONAL",
    );
}

pub struct ClientFeedback {
    pub session: Session,
    pub audio: Audio,
    pub weapon_overlay: WeaponRenderer,
    pub projectile_renderer: ProjectileRenderer,
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
    } = feedback;
    let initial_session = session.clone();
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
    let mut mouse_fire = false;
    let mut frame = vec![0; WIDTH * HEIGHT];
    let mut depth = vec![f32::INFINITY; WIDTH * HEIGHT];
    let mut last = Instant::now();
    let mut accumulator = 0.0f32;
    let mut animation_tick = 0_u64;
    let mut pending_interact = false;
    let mut pending_weapon = None;
    let mut shoot_ticks = 0usize;
    let mut current_message: Option<(Vec<String>, f32)> = None;
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
                Event::MouseButtonUp {
                    mouse_btn: MouseButton::Left,
                    ..
                } => mouse_fire = false,
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
            for (cell, visible) in session.game.pickups.visibility() {
                objects.set_visible(cell, visible);
            }
            pending_interact = false;
            pending_weapon = None;
            shoot_ticks = 0;
            animation_tick = 0;
            current_message = None;
            completed = false;
            look.pending_dx = 0;
        }
        if completed && advance_requested {
            sdl.mouse().set_relative_mouse_mode(false);
            return Ok(Some(session.game.inventory.clone()));
        }
        let firing = held.contains(&Scancode::Space) || mouse_fire;
        while accumulator >= STEP {
            // Deltas are motion, not velocity: consume exactly once per tick.
            let yaw_delta = look.take_yaw();
            animation_tick = animation_tick.saturating_add(1);
            let events = session.step(SessionInput {
                movement,
                yaw_delta,
                interact: pending_interact,
                fire: firing,
                select_weapon: pending_weapon,
            });
            pending_interact = false;
            pending_weapon = None;
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
            for (cell, destroyed) in events.object_hits {
                if destroyed {
                    objects.set_visible(cell, false);
                }
            }
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
            }
            accumulator -= STEP;
        }
        render(
            &mut frame,
            &mut depth,
            &session.world,
            &textures,
            Some(&planes),
            Some(&objects),
            label,
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
            session.world.player,
            &projectile_visuals,
        );
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
                "HEALTH {}  ARMOR {}  AMMO {}  KEYS {}{}{}",
                inventory.health,
                inventory.armor,
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
        render(&mut frame, &mut depth, &world, &textures, None, None, "");
        let visible_behind = frame[(HEIGHT / 2 - 10) * WIDTH + WIDTH / 2];
        let visible_in_front = frame[(HEIGHT / 2 + 10) * WIDTH + WIDTH / 2];
        assert!(visible_behind & 0xff0000 > 0, "{visible_behind:06x}");
        assert!(visible_in_front & 0x0000ff > 0, "{visible_in_front:06x}");
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
        render(&mut frame, &mut depth, &world, &textures, None, None, "");
        let center = frame[(HEIGHT / 2) * WIDTH + WIDTH / 2];
        assert!(center & 0xff0000 > 0);
        assert_eq!(center & 0x00ffff, 0);
    }
}
