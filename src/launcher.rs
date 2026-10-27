//! Small SDL launcher. Gameplay stays in the existing view path.
use crate::content::{self, ResourceSet, Source};
use font8x8::{UnicodeFonts, BASIC_FONTS};
use openchasm::pickups::Difficulty;
use sdl2::{
    event::Event,
    keyboard::{Keycode, Mod},
    mouse::MouseButton,
    pixels::Color,
    rect::Point,
    render::Canvas,
    video::Window,
};
use std::{
    error::Error,
    path::{Path, PathBuf},
    time::Duration,
};

pub struct Choice {
    pub path: PathBuf,
    pub level: String,
    pub difficulty: Difficulty,
}

struct Menu {
    sources: Vec<Source>,
    source: usize, // The final entry is a user-entered path.
    custom: String,
    levels: Vec<(String, String)>,
    level: usize,
    difficulty: Difficulty,
    row: usize,
    error: Option<String>,
}

impl Menu {
    fn new(sources: Vec<Source>, error: Option<String>) -> Self {
        let mut menu = Self {
            sources,
            source: 0,
            custom: String::new(),
            levels: Vec::new(),
            level: 0,
            difficulty: Difficulty::Normal,
            row: 0,
            error: None,
        };
        menu.refresh();
        if error.is_some() {
            menu.error = error;
        }
        menu
    }
    fn custom_selected(&self) -> bool {
        self.source == self.sources.len()
    }
    fn path(&self) -> PathBuf {
        if self.custom_selected() {
            PathBuf::from(&self.custom)
        } else {
            self.sources[self.source].path.clone()
        }
    }
    fn refresh(&mut self) {
        self.level = 0;
        self.levels.clear();
        if self.custom_selected() && self.custom.is_empty() {
            self.error = Some("Enter an archive or loose content directory path".into());
            return;
        }
        match levels_for(&self.path()) {
            Ok(levels) if !levels.is_empty() => {
                self.levels = levels;
                self.error = None;
            }
            Ok(_) => self.error = Some("No complete playable level resources found".into()),
            Err(error) => self.error = Some(format!("Cannot open content: {error}")),
        }
    }
    fn change(&mut self, direction: i32) {
        match self.row {
            0 => {
                self.source = cycle(self.source, self.sources.len() + 1, direction);
                self.refresh();
            }
            2 if !self.levels.is_empty() => {
                self.level = cycle(self.level, self.levels.len(), direction)
            }
            3 => {
                let current = match self.difficulty {
                    Difficulty::Easy => 0,
                    Difficulty::Normal => 1,
                    Difficulty::Hard => 2,
                };
                self.difficulty = [Difficulty::Easy, Difficulty::Normal, Difficulty::Hard]
                    [cycle(current, 3, direction)];
            }
            _ => {}
        }
    }
    fn choice(&mut self) -> Option<Choice> {
        // Revalidate custom paths and content at start, including external changes.
        let selected = self.levels.get(self.level).map(|level| level.0.clone());
        self.refresh();
        let id = selected
            .and_then(|id| {
                self.levels
                    .iter()
                    .find(|entry| entry.0 == id)
                    .map(|entry| entry.0.clone())
            })
            .or_else(|| self.levels.first().map(|entry| entry.0.clone()))?;
        Some(Choice {
            path: self.path(),
            level: id,
            difficulty: self.difficulty,
        })
    }
}
fn cycle(index: usize, count: usize, direction: i32) -> usize {
    if direction < 0 {
        (index + count - 1) % count
    } else {
        (index + 1) % count
    }
}
fn levels_for(path: &Path) -> Result<Vec<(String, String)>, Box<dyn Error>> {
    let mut resources = ResourceSet::open(path)?;
    let ids: Vec<String> = resources
        .names()
        .filter_map(|name| {
            let id = name.strip_prefix("map/resource.")?;
            (id.len() == 2 && id.bytes().all(|b| b.is_ascii_digit())).then(|| id.to_owned())
        })
        .collect();
    let mut result = Vec::new();
    for id in ids {
        let complete = ["map", "floors", "process"].iter().all(|kind| {
            resources
                .names()
                .any(|name| name == format!("map/{kind}.{id}"))
        });
        if complete {
            let bytes = resources.read(&format!("map/resource.{id}"))?;
            result.push((id, crate::level_name(&bytes)));
        }
    }
    result.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(result)
}

fn text(canvas: &mut Canvas<Window>, x: i32, y: i32, value: &str) {
    canvas.set_draw_color(Color::RGB(235, 235, 235));
    for (i, ch) in value.chars().take(92).enumerate() {
        if let Some(glyph) = BASIC_FONTS.get(ch).or_else(|| BASIC_FONTS.get('?')) {
            for (dy, bits) in glyph.iter().enumerate() {
                for dx in 0..8 {
                    if bits & (1 << dx) != 0 {
                        let _ = canvas.draw_point(Point::new(x + i as i32 * 8 + dx, y + dy as i32));
                    }
                }
            }
        }
    }
}

pub fn run(initial_error: Option<String>) -> Result<Option<Choice>, Box<dyn Error>> {
    let mut menu = Menu::new(content::discover(), initial_error);
    let sdl = sdl2::init()?;
    let video = sdl.video()?;
    let window = video
        .window("OpenChasm launcher", 800, 400)
        .position_centered()
        .build()?;
    let mut canvas = window.into_canvas().software().build()?;
    let mut events = sdl.event_pump()?;
    video.text_input().start();
    loop {
        for event in events.poll_iter() {
            match event {
                Event::Quit { .. }
                | Event::KeyDown {
                    keycode: Some(Keycode::Escape),
                    ..
                } => return Ok(None),
                Event::TextInput { text, .. } if menu.row == 1 && menu.custom_selected() => {
                    if menu.custom.len() + text.len() <= 4096 {
                        menu.custom.push_str(&text);
                        menu.levels.clear();
                        menu.error = Some("Press Enter to load this path".into());
                    }
                }
                Event::KeyDown {
                    keycode: Some(key),
                    keymod,
                    repeat: false,
                    ..
                } => match key {
                    Keycode::Up => menu.row = (menu.row + 4) % 5,
                    Keycode::Down | Keycode::Tab => menu.row = (menu.row + 1) % 5,
                    Keycode::Left if menu.row != 1 => menu.change(-1),
                    Keycode::Right if menu.row != 1 => menu.change(1),
                    Keycode::Backspace if menu.row == 1 && menu.custom_selected() => {
                        menu.custom.pop();
                        menu.levels.clear();
                        menu.error = Some("Press Enter to load this path".into());
                    }
                    Keycode::V
                        if menu.row == 1
                            && menu.custom_selected()
                            && keymod.intersects(
                                Mod::LCTRLMOD | Mod::RCTRLMOD | Mod::LGUIMOD | Mod::RGUIMOD,
                            ) =>
                    {
                        if let Ok(value) = video.clipboard().clipboard_text() {
                            if menu.custom.len() + value.len() <= 4096 {
                                menu.custom.push_str(&value);
                                menu.levels.clear();
                                menu.error = Some("Press Enter to load this path".into());
                            }
                        }
                    }
                    Keycode::Return | Keycode::KpEnter
                        if menu.row == 1 && menu.custom_selected() =>
                    {
                        menu.refresh()
                    }
                    Keycode::Return | Keycode::KpEnter if menu.row == 4 => {
                        if let Some(choice) = menu.choice() {
                            return Ok(Some(choice));
                        }
                    }
                    Keycode::Return | Keycode::KpEnter if menu.row != 1 => menu.change(1),
                    _ => {}
                },
                Event::MouseButtonDown {
                    mouse_btn: MouseButton::Left,
                    x,
                    y,
                    ..
                } if (70..320).contains(&y) => {
                    let row = ((y - 70) / 50) as usize;
                    if row < 5 {
                        menu.row = row;
                        if row == 4 {
                            if let Some(choice) = menu.choice() {
                                return Ok(Some(choice));
                            }
                        } else if row == 1 && menu.custom_selected() {
                        } else {
                            menu.change(if x < 400 { -1 } else { 1 });
                        }
                    }
                }
                _ => {}
            }
        }
        canvas.set_draw_color(Color::RGB(18, 22, 30));
        canvas.clear();
        text(&mut canvas, 24, 20, "OPENCHASM - EARLY GAMEPLAY PREVIEW");
        text(
            &mut canvas,
            24,
            40,
            "UP/DOWN: SELECT    LEFT/RIGHT: CHANGE    ENTER: CONFIRM    ESC: QUIT",
        );
        let source = if menu.custom_selected() {
            "CUSTOM PATH".to_owned()
        } else {
            menu.sources[menu.source].kind.label().to_owned()
        };
        let path = if menu.custom_selected() {
            menu.custom.clone()
        } else {
            menu.path().to_string_lossy().into_owned()
        };
        let level = menu
            .levels
            .get(menu.level)
            .map(|(id, name)| format!("{id}  {name}"))
            .unwrap_or_else(|| "(NONE AVAILABLE)".into());
        for (index, value) in [
            format!("SOURCE: < {source} >"),
            format!("PATH: {path}"),
            format!("LEVEL: < {level} >"),
            format!("DIFFICULTY: < {:?} >", menu.difficulty),
            "START GAME".into(),
        ]
        .iter()
        .enumerate()
        {
            let y = 80 + index as i32 * 50;
            text(
                &mut canvas,
                24,
                y,
                if menu.row == index { ">" } else { " " },
            );
            text(&mut canvas, 40, y, value);
        }
        if let Some(error) = &menu.error {
            text(&mut canvas, 24, 345, error);
        }
        text(
            &mut canvas,
            24,
            370,
            "CUSTOM PATH: TYPE OR PASTE; PRESS ENTER TO LOAD",
        );
        canvas.present();
        std::thread::sleep(Duration::from_millis(16));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn menu_handles_missing_and_custom_sources() {
        let mut menu = Menu::new(vec![], None);
        assert!(menu.custom_selected());
        assert!(menu.choice().is_none());
        menu.custom = "/no/such/openchasm/content".into();
        menu.refresh();
        assert!(menu.error.is_some());
        assert!(menu.choice().is_none());
        menu.row = 3;
        menu.change(1);
        assert_eq!(menu.difficulty, Difficulty::Hard);
    }
    #[test]
    fn cycling_wraps_in_both_directions() {
        assert_eq!(cycle(0, 3, -1), 2);
        assert_eq!(cycle(2, 3, 1), 0);
    }

    #[test]
    fn custom_content_lists_complete_levels_and_preserves_selection() {
        let root = std::env::temp_dir().join(format!("openchasm-launcher-{}", std::process::id()));
        let map = root.join("map");
        std::fs::create_dir_all(&map).unwrap();
        for id in ["01", "02"] {
            for kind in ["map", "floors", "process", "resource"] {
                let bytes = if kind == "resource" {
                    b"#name=Test map\n".as_slice()
                } else {
                    b""
                };
                std::fs::write(map.join(format!("{kind}.{id}")), bytes).unwrap();
            }
        }
        std::fs::write(map.join("resource.03"), b"#name=Incomplete\n").unwrap();
        let mut menu = Menu::new(vec![], None);
        menu.custom = root.to_string_lossy().into_owned();
        menu.refresh();
        assert_eq!(menu.levels.len(), 2);
        menu.level = 1;
        let choice = menu.choice().unwrap();
        assert_eq!(choice.path, root);
        assert_eq!(choice.level, "02");
        menu.sources.push(Source {
            kind: content::SourceKind::LocalArchive,
            path: root.clone(),
        });
        menu.source = 1;
        menu.row = 0;
        menu.change(1);
        assert_eq!(menu.level, 0);
        assert_eq!(menu.levels[0].0, "01");
        menu.change(1);
        assert!(menu.custom_selected());
        assert_eq!(menu.levels.len(), 2);
        std::fs::remove_dir_all(root).unwrap();
        assert!(menu.choice().is_none());
    }
}
