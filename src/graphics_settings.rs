//! Validated client graphics settings and keyboard options menu.
use crate::color_pipeline::ColorSettings;
use crate::ui::UiPixel;
use font8x8::{UnicodeFonts, BASIC_FONTS};
use sdl2::{event::Event, keyboard::Keycode};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

const MAX_BYTES: u64 = 16 * 1024;
static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VisualPreset {
    Legacy,
    Remaster,
    Enhanced,
}
impl VisualPreset {
    fn name(self) -> &'static str {
        match self {
            Self::Legacy => "legacy",
            Self::Remaster => "remaster",
            Self::Enhanced => "enhanced",
        }
    }
    fn shifted(self, step: i32) -> Self {
        let index = match self {
            Self::Legacy => 0,
            Self::Remaster => 1,
            Self::Enhanced => 2,
        };
        match (index + step).rem_euclid(3) {
            0 => Self::Legacy,
            1 => Self::Remaster,
            _ => Self::Enhanced,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GraphicsSettings {
    version: u32,
    pub preset: VisualPreset,
    pub tone_mapping: bool,
    pub gamma: f32,
    pub brightness: f32,
    pub saturation: f32,
    pub bloom: f32,
    pub show_fps: bool,
    #[serde(default)]
    pub show_help: bool,
    /// Zero means uncapped.
    pub max_fps: u32,
    pub sensitivity: f32,
}
impl Default for GraphicsSettings {
    fn default() -> Self {
        Self {
            version: 1,
            preset: VisualPreset::Remaster,
            tone_mapping: true,
            gamma: 1.0,
            brightness: 1.0,
            saturation: 1.0,
            bloom: 0.0,
            show_fps: true,
            show_help: false,
            max_fps: 60,
            sensitivity: 0.002,
        }
    }
}
impl GraphicsSettings {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.version != 1 {
            return Err("unsupported settings version");
        }
        self.color_settings().validate()?;
        if !self.bloom.is_finite() || !(0.0..=1.0).contains(&self.bloom) {
            return Err("bloom outside 0..=1");
        }
        if self.max_fps > 1000 {
            return Err("maxfps outside 0..=1000");
        }
        if !self.sensitivity.is_finite() || !(0.0001..=1.0).contains(&self.sensitivity) {
            return Err("sensitivity outside 0.0001..=1");
        }
        Ok(())
    }
    pub fn color_settings(&self) -> ColorSettings {
        ColorSettings {
            gamma: self.gamma,
            brightness: self.brightness,
            saturation: self.saturation,
            fade: 1.0,
            tone_mapping: self.tone_mapping,
        }
    }
    pub fn uses_hdr(&self) -> bool {
        self.preset != VisualPreset::Legacy
    }
    /// Applying a preset resets its visual controls but leaves input/FPS options.
    pub fn set_preset(&mut self, preset: VisualPreset) {
        self.preset = preset;
        self.tone_mapping = true;
        self.gamma = 1.0;
        self.brightness = 1.0;
        self.saturation = 1.0;
        self.bloom = if preset == VisualPreset::Enhanced {
            0.35
        } else {
            0.0
        };
    }
}

pub struct SettingsStore {
    directory: PathBuf,
}
impl SettingsStore {
    pub fn new(directory: PathBuf) -> Self {
        Self { directory }
    }
    pub fn for_sdl() -> io::Result<Self> {
        let path =
            sdl2::filesystem::pref_path("OpenChasm", "OpenChasm").map_err(io::Error::other)?;
        Ok(Self::new(PathBuf::from(path)))
    }
    /// Missing file gives defaults; malformed, oversized or unreadable settings
    /// return an error so the host can warn and fall back deliberately.
    pub fn load(&self) -> io::Result<GraphicsSettings> {
        let file = match File::open(self.directory.join("graphics.json")) {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(GraphicsSettings::default()),
            Err(e) => return Err(e),
        };
        if !file.metadata()?.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "settings is not a file",
            ));
        }
        let mut bytes = Vec::new();
        file.take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "settings exceeds 16 KiB",
            ));
        }
        let settings: GraphicsSettings = serde_json::from_slice(&bytes)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        settings
            .validate()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        Ok(settings)
    }
    /// Write only after an explicit user change. Temporary file and destination
    /// share a directory; unsuccessful writes leave the previous file intact.
    pub fn save(&self, settings: &GraphicsSettings) -> io::Result<()> {
        self.save_inner(settings, || Ok(()))
    }
    fn save_inner(
        &self,
        settings: &GraphicsSettings,
        before_rename: impl FnOnce() -> io::Result<()>,
    ) -> io::Result<()> {
        settings
            .validate()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        let bytes = serde_json::to_vec_pretty(settings).map_err(io::Error::other)?;
        if bytes.len() as u64 > MAX_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "settings too large",
            ));
        }
        fs::create_dir_all(&self.directory)?;
        for _ in 0..128 {
            let n = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
            let path = self
                .directory
                .join(format!(".graphics-{}-{n}.tmp", std::process::id()));
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = match options.open(&path) {
                Ok(f) => f,
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            };
            let result = (|| {
                file.write_all(&bytes)?;
                file.sync_all()?;
                drop(file);
                before_rename()?;
                fs::rename(&path, self.directory.join("graphics.json"))
            })();
            if result.is_err() {
                let _ = fs::remove_file(&path);
            }
            return result;
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not create temporary settings file",
        ))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuAction {
    None,
    Changed,
    Closed,
}
#[derive(Default)]
pub struct GraphicsMenu {
    open: bool,
    selected: usize,
}
impl GraphicsMenu {
    pub fn open(&mut self) {
        self.open = true;
    }
    pub fn close(&mut self) {
        self.open = false;
    }
    pub fn is_open(&self) -> bool {
        self.open
    }
    pub fn event(&mut self, event: &Event, settings: &mut GraphicsSettings) -> MenuAction {
        if !self.open {
            return MenuAction::None;
        }
        let Event::KeyDown {
            keycode: Some(key),
            repeat: false,
            ..
        } = event
        else {
            return MenuAction::None;
        };
        match *key {
            Keycode::Escape => {
                self.close();
                MenuAction::Closed
            }
            Keycode::Up => {
                self.selected = (self.selected + 7) % 8;
                MenuAction::None
            }
            Keycode::Down => {
                self.selected = (self.selected + 1) % 8;
                MenuAction::None
            }
            Keycode::Left | Keycode::Right | Keycode::Return | Keycode::KpEnter => {
                let step = if *key == Keycode::Left { -1 } else { 1 };
                let old = settings.clone();
                match self.selected {
                    0 => settings.set_preset(settings.preset.shifted(step)),
                    1 => settings.tone_mapping = !settings.tone_mapping,
                    2 => settings.gamma = (settings.gamma + step as f32 * 0.1).clamp(0.05, 16.0),
                    3 => {
                        settings.brightness =
                            (settings.brightness + step as f32 * 0.1).clamp(0.0, 16.0)
                    }
                    4 => {
                        settings.saturation =
                            (settings.saturation + step as f32 * 0.1).clamp(0.0, 16.0)
                    }
                    5 => settings.bloom = (settings.bloom + step as f32 * 0.05).clamp(0.0, 1.0),
                    6 => settings.show_fps = !settings.show_fps,
                    7 => {
                        let choices = [0, 30, 60, 90, 120, 144, 240, 1000];
                        let index = choices
                            .iter()
                            .position(|x| *x == settings.max_fps)
                            .unwrap_or(2);
                        settings.max_fps = choices
                            [(index as i32 + step).rem_euclid(choices.len() as i32) as usize];
                    }
                    _ => unreachable!(),
                }
                if *settings != old {
                    MenuAction::Changed
                } else {
                    MenuAction::None
                }
            }
            _ => MenuAction::None,
        }
    }
    pub fn render<C: UiPixel>(
        &self,
        frame: &mut [C],
        width: usize,
        height: usize,
        settings: &GraphicsSettings,
    ) {
        if !self.open || width.checked_mul(height) != Some(frame.len()) {
            return;
        }
        for pixel in frame.iter_mut() {
            *pixel = pixel.dim_shift_bias(1, 0);
        }
        let rows = [
            format!("PRESET: {}", settings.preset.name()),
            format!("TONE MAPPING: {}", onoff(settings.tone_mapping)),
            format!("GAMMA: {:.2}", settings.gamma),
            format!("BRIGHTNESS: {:.2}", settings.brightness),
            format!("SATURATION: {:.2}", settings.saturation),
            format!("BLOOM: {:.2}", settings.bloom),
            format!("FPS DISPLAY: {}", onoff(settings.show_fps)),
            format!(
                "FRAME CAP: {}",
                if settings.max_fps == 0 {
                    "UNLIMITED".to_owned()
                } else {
                    settings.max_fps.to_string()
                }
            ),
        ];
        let x = width.saturating_sub(250) / 2;
        let y = height.saturating_sub(125) / 2;
        draw(frame, width, height, x, y, "GRAPHICS OPTIONS");
        for (i, row) in rows.iter().enumerate() {
            draw(
                frame,
                width,
                height,
                x,
                y + 15 + i * 10,
                &format!("{} {row}", if i == self.selected { ">" } else { " " }),
            );
        }
        draw(frame, width, height, x, y + 105, "ARROWS CHANGE  ESC CLOSE");
        if !settings.uses_hdr() {
            draw(
                frame,
                width,
                height,
                x,
                y + 115,
                "LEGACY: HDR CONTROLS NOT APPLIED",
            );
        }
    }
}
fn onoff(value: bool) -> &'static str {
    if value {
        "ON"
    } else {
        "OFF"
    }
}
fn draw<C: UiPixel>(frame: &mut [C], width: usize, height: usize, x: usize, y: usize, text: &str) {
    for (i, ch) in text.chars().enumerate() {
        if let Some(glyph) = BASIC_FONTS.get(ch.to_ascii_uppercase()) {
            for (dy, row) in glyph.iter().enumerate() {
                for dx in 0..8 {
                    let px = x + i * 8 + dx;
                    let py = y + dy;
                    if px < width && py < height && row & (1 << dx) != 0 {
                        frame[py * width + px] = C::from_rgb(0xffffff);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn key(keycode: Keycode) -> Event {
        Event::KeyDown {
            timestamp: 0,
            window_id: 0,
            keycode: Some(keycode),
            scancode: None,
            keymod: sdl2::keyboard::Mod::NOMOD,
            repeat: false,
        }
    }
    fn directory() -> PathBuf {
        std::env::temp_dir().join(format!(
            "openchasm-graphics-test-{}-{}",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ))
    }
    #[test]
    fn presets_are_valid_and_preserve_gameplay_options() {
        let mut settings = GraphicsSettings::default();
        assert!(settings.uses_hdr());
        assert_eq!(settings.bloom, 0.0);
        settings.sensitivity = 0.01;
        settings.max_fps = 144;
        settings.set_preset(VisualPreset::Enhanced);
        assert_eq!(settings.bloom, 0.35);
        assert!(settings.uses_hdr());
        assert_eq!(settings.max_fps, 144);
        assert_eq!(settings.sensitivity, 0.01);
        settings.set_preset(VisualPreset::Legacy);
        assert!(!settings.uses_hdr());
        assert_eq!(settings.bloom, 0.0);
        assert!(settings.validate().is_ok());
        settings.gamma = f32::NAN;
        assert!(settings.validate().is_err());
    }
    #[test]
    fn pre_hud_preferences_keep_their_saved_values() {
        let settings = GraphicsSettings {
            max_fps: 144,
            gamma: 1.2,
            ..GraphicsSettings::default()
        };
        let mut old = serde_json::to_value(&settings).unwrap();
        old.as_object_mut().unwrap().remove("show_help");
        let restored: GraphicsSettings = serde_json::from_value(old).unwrap();
        assert_eq!(restored, settings);
        assert!(!restored.show_help);
    }
    #[test]
    fn store_roundtrips_and_invalid_write_preserves_existing_file() {
        let dir = directory();
        let store = SettingsStore::new(dir.clone());
        assert_eq!(store.load().unwrap(), GraphicsSettings::default());
        let mut settings = GraphicsSettings::default();
        settings.set_preset(VisualPreset::Enhanced);
        store.save(&settings).unwrap();
        assert_eq!(store.load().unwrap(), settings);
        let bytes = fs::read(dir.join("graphics.json")).unwrap();
        settings.bloom = f32::INFINITY;
        assert!(store.save(&settings).is_err());
        assert_eq!(fs::read(dir.join("graphics.json")).unwrap(), bytes);
        let next = GraphicsSettings {
            gamma: 1.4,
            ..GraphicsSettings::default()
        };
        assert!(store
            .save_inner(&next, || Err(io::Error::other("injected failure")))
            .is_err());
        assert_eq!(fs::read(dir.join("graphics.json")).unwrap(), bytes);
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
        store.save(&next).unwrap();
        assert_eq!(store.load().unwrap(), next);
        fs::write(
            dir.join("graphics.json"),
            vec![b' '; MAX_BYTES as usize + 1],
        )
        .unwrap();
        assert!(store.load().is_err());
        let mut unsupported = next.clone();
        unsupported.version = 2;
        fs::write(
            dir.join("graphics.json"),
            serde_json::to_vec(&unsupported).unwrap(),
        )
        .unwrap();
        assert!(store.load().is_err());
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn menu_changes_selected_values_and_closes() {
        let mut settings = GraphicsSettings::default();
        let mut menu = GraphicsMenu::default();
        assert_eq!(
            menu.event(&key(Keycode::Right), &mut settings),
            MenuAction::None
        );
        menu.open();
        assert_eq!(
            menu.event(&key(Keycode::Right), &mut settings),
            MenuAction::Changed
        );
        assert_eq!(settings.preset, VisualPreset::Enhanced);
        menu.event(&key(Keycode::Down), &mut settings);
        assert_eq!(
            menu.event(&key(Keycode::Return), &mut settings),
            MenuAction::Changed
        );
        assert!(!settings.tone_mapping);
        let mut frame = vec![0xffffffu32; 320 * 200];
        menu.render(&mut frame, 320, 200, &settings);
        assert_ne!(frame[0], 0xffffff);
        assert_eq!(
            menu.event(&key(Keycode::Escape), &mut settings),
            MenuAction::Closed
        );
        assert!(!menu.is_open());
    }
}
