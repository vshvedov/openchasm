//! Software-rendered geometry viewer. Game rules live outside the window layer.
use crate::planes::PlaneRenderer;
use crate::texture::CelImage;
use crate::world::{Input, World};
use font8x8::{UnicodeFonts, BASIC_FONTS};
use minifb::{Key, ScaleMode, Window, WindowOptions};
use std::error::Error;
use std::time::{Duration, Instant};

const WIDTH: usize = 640;
const HEIGHT: usize = 400;
const STEP: f32 = 1.0 / 60.0;

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
    world: &World,
    textures: &[Option<ViewerTexture>],
    planes: Option<&PlaneRenderer>,
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
            }
        }
    }
    text(frame, 8, 8, label);
    text(frame, 8, 20, "WASD: MOVE   LEFT/RIGHT: TURN   ESC: QUIT");
    text(frame, 8, 32, "GEOMETRY PREVIEW - GAMEPLAY NOT IMPLEMENTED");
}

pub fn run(
    mut world: World,
    textures: Vec<Option<CelImage>>,
    planes: PlaneRenderer,
    label: &str,
) -> Result<(), Box<dyn Error>> {
    let textures: Vec<Option<ViewerTexture>> = textures
        .into_iter()
        .map(|item| item.map(ViewerTexture::from))
        .collect();
    let mut window = Window::new(
        "OpenChasm - geometry preview",
        WIDTH,
        HEIGHT,
        WindowOptions {
            resize: true,
            scale_mode: ScaleMode::AspectRatioStretch,
            ..WindowOptions::default()
        },
    )?;
    window.set_target_fps(60);
    let mut frame = vec![0; WIDTH * HEIGHT];
    let mut last = Instant::now();
    let mut accumulator = 0.0f32;
    while window.is_open() && !window.is_key_down(Key::Escape) {
        let now = Instant::now();
        accumulator += now
            .duration_since(last)
            .min(Duration::from_millis(100))
            .as_secs_f32();
        last = now;
        let key = |k| if window.is_key_down(k) { 1.0 } else { 0.0 };
        let input = Input {
            forward: key(Key::W) - key(Key::S),
            strafe: key(Key::D) - key(Key::A),
            turn: key(Key::Right) - key(Key::Left),
        };
        while accumulator >= STEP {
            world.step(input, STEP);
            accumulator -= STEP;
        }
        render(&mut frame, &world, &textures, Some(&planes), label);
        window.update_with_buffer(&frame, WIDTH, HEIGHT)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::{Player, Wall};

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
        render(&mut frame, &world, &textures, None, "");
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
        render(&mut frame, &world, &textures, None, "");
        let center = frame[(HEIGHT / 2) * WIDTH + WIDTH / 2];
        assert!(center & 0xff0000 > 0);
        assert_eq!(center & 0x00ffff, 0);
    }
}
