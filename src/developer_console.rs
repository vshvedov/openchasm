//! Bounded client-side developer console. The host owns mouse/text-input routing
//! and applies returned commands; no console command mutates simulation directly.
use crate::ui::UiPixel;
use openchasm::movement::MovementMode;
use std::collections::VecDeque;

use font8x8::{UnicodeFonts, BASIC_FONTS};
use sdl2::{event::Event, keyboard::Keycode};

const MAX_LINES: usize = 256;
const MAX_LINE_BYTES: usize = 1024;
const MAX_HISTORY: usize = 64;
const COMMANDS: &[&str] = &[
    "help",
    "cmdlist",
    "clear",
    "echo",
    "version",
    "fps",
    "showhelp",
    "maxfps",
    "status",
    "restart",
    "quit",
    "sensitivity",
    "autorun",
    "physics",
    "options",
    "graphics",
    "renderer",
    "tonemap",
    "gamma",
    "brightness",
    "saturation",
    "bloom",
    "god",
    "giveall",
    "allweapons",
    "give",
    "map",
    "level",
    "changelevel",
    "fullscreen",
    "resolution",
];

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Command {
    Fps(Option<bool>),
    HelpOverlay(Option<bool>),
    /// Frames per second cap; zero means uncapped, None queries.
    MaxFps(Option<u32>),
    Status,
    God(Option<bool>),
    GiveAll,
    Map(Option<u8>),
    Fullscreen(Option<bool>),
    Resolution(Option<crate::display_settings::Resolution>),
    Restart,
    Quit,
    /// Radians per relative mouse count; None requests the current value.
    Sensitivity(Option<f32>),
    Autorun(Option<bool>),
    Physics(Option<MovementMode>),
    Options,
    Graphics(Option<crate::graphics_settings::VisualPreset>),
    Renderer(Option<crate::presentation::BackendChoice>),
    ToneMap(Option<bool>),
    Gamma(Option<f32>),
    Brightness(Option<f32>),
    Saturation(Option<f32>),
    Bloom(Option<f32>),
}

#[derive(Default)]
pub struct Console {
    open: bool,
    lines: VecDeque<String>,
    input: String,
    cursor: usize, // UTF-8 byte boundary
    history: VecDeque<String>,
    history_index: Option<usize>,
    draft: String,
    scroll: usize,
}

impl Console {
    pub fn toggle(&mut self) -> bool {
        self.open = !self.open;
        self.open
    }
    pub fn is_open(&self) -> bool {
        self.open
    }
    pub fn close(&mut self) {
        self.open = false;
    }

    /// Add bounded text to scrollback. Each newline creates a separate line.
    pub fn push(&mut self, text: &str) {
        for line in text.split('\n') {
            let mut end = line.len().min(MAX_LINE_BYTES);
            while !line.is_char_boundary(end) {
                end -= 1;
            }
            let clean: String = line[..end]
                .chars()
                .map(|c| if c.is_control() { ' ' } else { c })
                .collect();
            if self.lines.len() == MAX_LINES {
                self.lines.pop_front();
            }
            self.lines.push_back(clean);
        }
        self.scroll = 0;
    }

    /// Consume input events only while open. Toggle and escape belong to the host.
    pub fn event(&mut self, event: &Event) -> Option<Command> {
        if !self.open {
            return None;
        }
        match event {
            Event::TextInput { text, .. } => {
                self.insert(text);
                None
            }
            Event::KeyDown {
                keycode: Some(key), ..
            } => self.key(*key),
            _ => None,
        }
    }

    fn insert(&mut self, text: &str) {
        for ch in text.chars().filter(|c| !c.is_control()) {
            if self.input.len() + ch.len_utf8() > MAX_LINE_BYTES {
                break;
            }
            self.input.insert(self.cursor, ch);
            self.cursor += ch.len_utf8();
        }
        self.history_index = None;
    }
    fn previous_boundary(&self) -> usize {
        self.input[..self.cursor]
            .char_indices()
            .next_back()
            .map_or(0, |(i, _)| i)
    }
    fn next_boundary(&self) -> usize {
        self.input[self.cursor..]
            .chars()
            .next()
            .map_or(self.cursor, |c| self.cursor + c.len_utf8())
    }
    fn key(&mut self, key: Keycode) -> Option<Command> {
        match key {
            Keycode::Backspace if self.cursor > 0 => {
                let prev = self.previous_boundary();
                self.input.drain(prev..self.cursor);
                self.cursor = prev;
                self.history_index = None;
            }
            Keycode::Delete if self.cursor < self.input.len() => {
                let next = self.next_boundary();
                self.input.drain(self.cursor..next);
                self.history_index = None;
            }
            Keycode::Left => self.cursor = self.previous_boundary(),
            Keycode::Right => self.cursor = self.next_boundary(),
            Keycode::Home => self.cursor = 0,
            Keycode::End => self.cursor = self.input.len(),
            Keycode::Up => self.history_up(),
            Keycode::Down => self.history_down(),
            Keycode::PageUp => {
                self.scroll = (self.scroll + 8).min(self.lines.len().saturating_sub(1))
            }
            Keycode::PageDown => self.scroll = self.scroll.saturating_sub(8),
            Keycode::Tab => self.complete(),
            Keycode::Return | Keycode::KpEnter => return self.submit(),
            _ => {}
        }
        None
    }
    fn history_up(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let index = match self.history_index {
            None => {
                self.draft.clone_from(&self.input);
                self.history.len() - 1
            }
            Some(index) => index.saturating_sub(1),
        };
        self.history_index = Some(index);
        self.input.clone_from(&self.history[index]);
        self.cursor = self.input.len();
    }
    fn history_down(&mut self) {
        let Some(index) = self.history_index else {
            return;
        };
        if index + 1 < self.history.len() {
            self.history_index = Some(index + 1);
            self.input.clone_from(&self.history[index + 1]);
        } else {
            self.history_index = None;
            self.input.clone_from(&self.draft);
        }
        self.cursor = self.input.len();
    }
    fn complete(&mut self) {
        if self.cursor != self.input.len() || self.input.chars().any(char::is_whitespace) {
            return;
        }
        let prefix = self.input.to_ascii_lowercase();
        let choices: Vec<_> = COMMANDS
            .iter()
            .copied()
            .filter(|cmd| cmd.starts_with(&prefix))
            .collect();
        if choices.len() == 1 {
            self.input = format!("{} ", choices[0]);
            self.cursor = self.input.len();
            self.history_index = None;
        } else if !choices.is_empty() {
            self.push(&choices.join("  "));
        }
    }
    fn submit(&mut self) -> Option<Command> {
        let line = std::mem::take(&mut self.input);
        self.cursor = 0;
        self.history_index = None;
        self.draft.clear();
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return None;
        }
        self.push(&format!("> {trimmed}"));
        if self.history.back().is_none_or(|last| last != trimmed) {
            if self.history.len() == MAX_HISTORY {
                self.history.pop_front();
            }
            self.history.push_back(trimmed.to_owned());
        }
        let mut parts = trimmed.split_whitespace();
        let command = parts.next().unwrap_or("").to_ascii_lowercase();
        let args: Vec<_> = parts.collect();
        match command.as_str() {
            "help" | "cmdlist" if args.is_empty() => self
                .push("help cmdlist clear echo version status\nrestart quit god giveall allweapons\ngive all | map [01] | level [01]\nchangelevel [01] | fullscreen\nfps showhelp maxfps options graphics\nrenderer resolution\nsensitivity autorun physics\ntonemap gamma brightness\nsaturation bloom"),
            "clear" if args.is_empty() => self.lines.clear(),
            "echo" => self.push(
                trimmed
                    .split_once(char::is_whitespace)
                    .map_or("", |(_, text)| text.trim_start()),
            ),
            "version" if args.is_empty() => {
                self.push(concat!("OpenChasm ", env!("CARGO_PKG_VERSION")))
            }
            "fps" => match args.as_slice() {
                [] => return Some(Command::Fps(None)),
                [v] if v.eq_ignore_ascii_case("on") || *v == "1" => {
                    return Some(Command::Fps(Some(true)))
                }
                [v] if v.eq_ignore_ascii_case("off") || *v == "0" => {
                    return Some(Command::Fps(Some(false)))
                }
                _ => self.push("usage: fps [on|off|1|0]"),
            },
            "showhelp" => match args.as_slice() {
                [] => return Some(Command::HelpOverlay(None)),
                [v] if v.eq_ignore_ascii_case("on") || *v == "1" => return Some(Command::HelpOverlay(Some(true))),
                [v] if v.eq_ignore_ascii_case("off") || *v == "0" => return Some(Command::HelpOverlay(Some(false))),
                _ => self.push("usage: showhelp [on|off|1|0]"),
            },
            "maxfps" => match args.as_slice() {
                [] => return Some(Command::MaxFps(None)),
                [value] => match value.parse::<u32>() {
                    Ok(value) if value <= 1000 => return Some(Command::MaxFps(Some(value))),
                    _ => self.push("usage: maxfps [0..1000] (0 uncapped)"),
                },
                _ => self.push("usage: maxfps [0..1000] (0 uncapped)"),
            },
            "sensitivity" => match args.as_slice() {
                [] => return Some(Command::Sensitivity(None)),
                [v] => match v.parse::<f32>() {
                    Ok(v) if v.is_finite() && (0.0001..=1.0).contains(&v) => {
                        return Some(Command::Sensitivity(Some(v)))
                    }
                    _ => self.push("usage: sensitivity [0.0001..1.0] (radians/count)"),
                },
                _ => self.push("usage: sensitivity [0.0001..1.0] (radians/count)"),
            },
            "autorun" => match args.as_slice() {
                [] => return Some(Command::Autorun(None)),
                [v] if v.eq_ignore_ascii_case("on") || *v == "1" => return Some(Command::Autorun(Some(true))),
                [v] if v.eq_ignore_ascii_case("off") || *v == "0" => return Some(Command::Autorun(Some(false))),
                _ => self.push("usage: autorun [on|off|1|0]"),
            },
            "physics" => match args.as_slice() {
                [] => return Some(Command::Physics(None)),
                [v] if v.eq_ignore_ascii_case("original") => return Some(Command::Physics(Some(MovementMode::Original))),
                [v] if v.eq_ignore_ascii_case("quake1") => return Some(Command::Physics(Some(MovementMode::Quake1))),
                _ => self.push("usage: physics [original|quake1]"),
            },
            "god" | "fullscreen" => {
                let value = match args.as_slice() {
                    [] => Some(None),
                    [v] if v.eq_ignore_ascii_case("on") || *v == "1" => Some(Some(true)),
                    [v] if v.eq_ignore_ascii_case("off") || *v == "0" => Some(Some(false)),
                    _ => None,
                };
                if let Some(value) = value {
                    return Some(if command == "god" { Command::God(value) } else { Command::Fullscreen(value) });
                }
                self.push(&format!("usage: {command} [on|off|1|0] (no argument toggles)"));
            }
            "giveall" | "allweapons" if args.is_empty() => return Some(Command::GiveAll),
            "give" if args.len() == 1 && args[0].eq_ignore_ascii_case("all") => return Some(Command::GiveAll),
            "giveall" | "allweapons" | "give" => self.push("usage: giveall (or give all)"),
            "map" | "level" | "changelevel" => {
                match args.as_slice() {
                    [] => return Some(Command::Map(None)),
                    [id] => {
                        let id = id.strip_prefix("map").or_else(|| id.strip_prefix("MAP")).unwrap_or(id);
                        if id.len() <= 2 && id.bytes().all(|b| b.is_ascii_digit()) {
                            if let Ok(id @ 1..=99) = id.parse::<u8>() { return Some(Command::Map(Some(id))); }
                        }
                    }
                    _ => {}
                }
                self.push("usage: map [01..99] (no argument lists available levels)");
            }
            "resolution" => {
                let value = match args.as_slice() {
                    [] => Some(None),
                    [size] => size.parse::<crate::display_settings::Resolution>().ok().map(Some),
                    [width, height] => format!("{width}x{height}").parse::<crate::display_settings::Resolution>().ok().map(Some),
                    _ => None,
                };
                if let Some(value) = value { return Some(Command::Resolution(value)); }
                self.push("usage: resolution [WIDTHxHEIGHT | WIDTH HEIGHT]");
            }
            "options" if args.is_empty() => return Some(Command::Options),
            "graphics" => {
                use crate::graphics_settings::VisualPreset;
                let value = match args.as_slice() {
                    [] => Some(None),
                    [v] if v.eq_ignore_ascii_case("legacy") => Some(Some(VisualPreset::Legacy)),
                    [v] if v.eq_ignore_ascii_case("remaster") => Some(Some(VisualPreset::Remaster)),
                    [v] if v.eq_ignore_ascii_case("enhanced") => Some(Some(VisualPreset::Enhanced)),
                    _ => None,
                };
                if let Some(value) = value { return Some(Command::Graphics(value)); }
                self.push("usage: graphics [legacy|remaster|enhanced]");
            }
            "renderer" => {
                use crate::presentation::BackendChoice;
                let value = match args.as_slice() {
                    [] => Some(None),
                    [v] if v.eq_ignore_ascii_case("auto") => Some(Some(BackendChoice::Auto)),
                    [v] if v.eq_ignore_ascii_case("gpu") => Some(Some(BackendChoice::Gpu)),
                    [v] if v.eq_ignore_ascii_case("software") => Some(Some(BackendChoice::Software)),
                    _ => None,
                };
                if let Some(value) = value { return Some(Command::Renderer(value)); }
                self.push("usage: renderer [auto|gpu|software]");
            }
            "tonemap" => {
                let value = match args.as_slice() {
                    [] => Some(None),
                    [v] if v.eq_ignore_ascii_case("on") || *v == "1" => Some(Some(true)),
                    [v] if v.eq_ignore_ascii_case("off") || *v == "0" => Some(Some(false)),
                    _ => None,
                };
                if let Some(value) = value { return Some(Command::ToneMap(value)); }
                self.push("usage: tonemap [on|off|1|0]");
            }
            "gamma" | "brightness" | "saturation" | "bloom" => {
                let (min, max) = match command.as_str() { "gamma" => (0.05, 16.0), "bloom" => (0.0, 1.0), _ => (0.0, 16.0) };
                let value = match args.as_slice() {
                    [] => Some(None),
                    [v] => v.parse::<f32>().ok().filter(|v| v.is_finite() && (*v >= min && *v <= max)).map(Some),
                    _ => None,
                };
                if let Some(value) = value { return Some(match command.as_str() { "gamma" => Command::Gamma(value), "brightness" => Command::Brightness(value), "saturation" => Command::Saturation(value), _ => Command::Bloom(value) }); }
                self.push(&format!("usage: {command} [{min}..{max}]"));
            }
            "status" | "restart" | "quit" if args.is_empty() => {
                return Some(match command.as_str() {
                    "status" => Command::Status,
                    "restart" => Command::Restart,
                    _ => Command::Quit,
                })
            }
            "help" | "cmdlist" | "clear" | "version" | "status" | "restart" | "quit" | "options" => {
                self.push(&format!("usage: {command}"))
            }
            _ => self.push(&format!("unknown command: {command}")),
        }
        None
    }

    /// Dark overlay occupying the top half of the frame, with bounded drawing.
    pub fn render<C: UiPixel>(&self, frame: &mut [C], width: usize, height: usize) {
        if !self.open
            || width == 0
            || height < 2
            || width.checked_mul(height).is_none_or(|n| n > frame.len())
        {
            return;
        }
        let overlay = height / 2;
        for pixel in frame.iter_mut().take(width * overlay) {
            // Keep the scene faintly visible without letting HUD text compete
            // with the console's output at the same pixel positions.
            *pixel = pixel.dim_shift_bias(4, 0x0c1014);
        }
        let prompt_y = overlay.saturating_sub(10);
        let rows = prompt_y.saturating_sub(2) / 9;
        let end = self.lines.len().saturating_sub(self.scroll);
        let start = end.saturating_sub(rows);
        for (row, line) in self.lines.iter().skip(start).take(end - start).enumerate() {
            draw_text(frame, width, overlay, 4, 2 + row * 9, line);
        }
        let columns = width.saturating_sub(12) / 8;
        let cursor_chars = self.input[..self.cursor].chars().count();
        let start_char = cursor_chars.saturating_sub(columns.saturating_sub(1));
        let visible: String = self.input.chars().skip(start_char).take(columns).collect();
        draw_text(frame, width, overlay, 4, prompt_y, ">");
        draw_text(frame, width, overlay, 12, prompt_y, &visible);
        let cursor_x = 12 + (cursor_chars - start_char) * 8;
        for dx in 0..7 {
            put(frame, width, overlay, cursor_x + dx, prompt_y + 7, 0xffffff);
        }
    }
}
fn put<C: UiPixel>(frame: &mut [C], width: usize, height: usize, x: usize, y: usize, color: u32) {
    if x < width && y < height {
        frame[y * width + x] = C::from_rgb(color);
    }
}
fn draw_text<C: UiPixel>(
    frame: &mut [C],
    width: usize,
    height: usize,
    x: usize,
    y: usize,
    text: &str,
) {
    for (i, ch) in text.chars().enumerate().take(width / 8 + 1) {
        let glyph = BASIC_FONTS
            .get(ch)
            .or_else(|| BASIC_FONTS.get('?'))
            .unwrap_or([0; 8]);
        for (dy, row) in glyph.iter().enumerate() {
            for dx in 0..8 {
                if row & (1 << dx) != 0 {
                    put(frame, width, height, x + i * 8 + dx, y + dy, 0xffffff);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn submit(console: &mut Console, text: &str) -> Option<Command> {
        console.insert(text);
        console.key(Keycode::Return)
    }
    #[test]
    fn cheats_levels_and_display_commands_validate_without_side_effects() {
        let mut c = Console::default();
        for (input, command) in [
            ("god", Command::God(None)),
            ("god on", Command::God(Some(true))),
            ("god 0", Command::God(Some(false))),
            ("giveall", Command::GiveAll),
            ("allweapons", Command::GiveAll),
            ("give ALL", Command::GiveAll),
            ("map", Command::Map(None)),
            ("map 01", Command::Map(Some(1))),
            ("level 19", Command::Map(Some(19))),
            ("changelevel MAP02", Command::Map(Some(2))),
            ("fullscreen", Command::Fullscreen(None)),
            ("fullscreen off", Command::Fullscreen(Some(false))),
            ("resolution", Command::Resolution(None)),
            (
                "resolution 1280x720",
                Command::Resolution(Some(crate::display_settings::Resolution {
                    width: 1280,
                    height: 720,
                })),
            ),
            (
                "resolution 800 600",
                Command::Resolution(Some(crate::display_settings::Resolution {
                    width: 800,
                    height: 600,
                })),
            ),
        ] {
            assert_eq!(submit(&mut c, input), Some(command), "{input}");
        }
        for input in [
            "god maybe",
            "god on off",
            "give weapon",
            "giveall now",
            "map 00",
            "map 100",
            "map ../01",
            "map 01 extra",
            "fullscreen yes",
            "resolution 0x0",
            "resolution 999999x999999",
            "resolution 640 nan",
        ] {
            assert_eq!(submit(&mut c, input), None, "{input}");
            assert!(c.lines.back().unwrap().starts_with("usage:"), "{input}");
        }
        c.insert("fullsc");
        c.key(Keycode::Tab);
        assert_eq!(c.input, "fullscreen ");
        c.input.clear();
        c.cursor = 0;
        submit(&mut c, "help");
        assert!(c.lines.iter().any(|line| line.contains("giveall")));
    }
    #[test]
    fn editing_preserves_utf8_boundaries_and_supports_navigation() {
        let mut c = Console::default();
        c.insert("a猫🙂z");
        c.key(Keycode::Left);
        c.key(Keycode::Backspace);
        assert_eq!(c.input, "a猫z");
        c.key(Keycode::Home);
        c.key(Keycode::Right);
        c.key(Keycode::Delete);
        assert_eq!(c.input, "az");
        c.key(Keycode::End);
        c.insert("é");
        assert_eq!(c.input, "azé");
        assert!(c.input.is_char_boundary(c.cursor));
    }
    #[test]
    fn history_restores_draft_and_completion_is_bounded() {
        let mut c = Console::default();
        submit(&mut c, "echo one");
        submit(&mut c, "echo two");
        c.insert("draft");
        c.key(Keycode::Up);
        assert_eq!(c.input, "echo two");
        c.key(Keycode::Up);
        assert_eq!(c.input, "echo one");
        c.key(Keycode::Down);
        c.key(Keycode::Down);
        assert_eq!(c.input, "draft");
        c.input.clear();
        c.cursor = 0;
        c.insert("sens");
        c.key(Keycode::Tab);
        assert_eq!(c.input, "sensitivity ");
        c.input.clear();
        c.cursor = 0;
        c.insert("s");
        c.key(Keycode::Tab);
        assert!(c.lines.back().unwrap().contains("status"));
    }
    #[test]
    fn validates_commands_and_limits_memory() {
        let mut c = Console::default();
        assert_eq!(submit(&mut c, "fps on"), Some(Command::Fps(Some(true))));
        assert_eq!(submit(&mut c, "fps"), Some(Command::Fps(None)));
        assert_eq!(submit(&mut c, "renderer"), Some(Command::Renderer(None)));
        assert_eq!(
            submit(&mut c, "renderer gpu"),
            Some(Command::Renderer(Some(
                crate::presentation::BackendChoice::Gpu
            )))
        );
        assert_eq!(
            submit(&mut c, "showhelp on"),
            Some(Command::HelpOverlay(Some(true)))
        );
        assert_eq!(submit(&mut c, "showhelp"), Some(Command::HelpOverlay(None)));
        assert_eq!(
            submit(&mut c, "graphics enhanced"),
            Some(Command::Graphics(Some(
                crate::graphics_settings::VisualPreset::Enhanced
            )))
        );
        assert_eq!(submit(&mut c, "options"), Some(Command::Options));
        assert_eq!(
            submit(&mut c, "tonemap off"),
            Some(Command::ToneMap(Some(false)))
        );
        assert_eq!(submit(&mut c, "gamma 2"), Some(Command::Gamma(Some(2.0))));
        assert_eq!(submit(&mut c, "bloom 1.01"), None);
        assert_eq!(submit(&mut c, "gamma nan"), None);

        assert_eq!(submit(&mut c, "maxfps"), Some(Command::MaxFps(None)));
        assert_eq!(submit(&mut c, "maxfps 0"), Some(Command::MaxFps(Some(0))));
        assert_eq!(
            submit(&mut c, "maxfps 1000"),
            Some(Command::MaxFps(Some(1000)))
        );
        assert_eq!(submit(&mut c, "maxfps 1001"), None);
        assert_eq!(
            submit(&mut c, "sensitivity 0.002"),
            Some(Command::Sensitivity(Some(0.002)))
        );
        assert_eq!(submit(&mut c, "sensitivity NaN"), None);
        assert!(c.lines.back().unwrap().starts_with("usage:"));
        assert_eq!(submit(&mut c, "quit now"), None);
        assert_eq!(submit(&mut c, "restart"), Some(Command::Restart));
        for i in 0..300 {
            submit(&mut c, &format!("echo {i}"));
        }
        assert_eq!(c.lines.len(), MAX_LINES);
        assert_eq!(c.history.len(), MAX_HISTORY);
        c.insert(&"é".repeat(MAX_LINE_BYTES));
        assert!(c.input.len() <= MAX_LINE_BYTES);
        c.push(&"猫".repeat(MAX_LINE_BYTES));
        assert!(c.lines.back().unwrap().len() <= MAX_LINE_BYTES);
    }
    #[test]
    fn control_commands_validate_values_queries_help_and_completion() {
        let mut c = Console::default();
        for (input, expected) in [
            ("autorun", Command::Autorun(None)),
            ("autorun ON", Command::Autorun(Some(true))),
            ("autorun off", Command::Autorun(Some(false))),
            ("autorun 1", Command::Autorun(Some(true))),
            ("autorun 0", Command::Autorun(Some(false))),
            ("physics", Command::Physics(None)),
            (
                "physics ORIGINAL",
                Command::Physics(Some(MovementMode::Original)),
            ),
            (
                "physics quake1",
                Command::Physics(Some(MovementMode::Quake1)),
            ),
        ] {
            assert_eq!(submit(&mut c, input), Some(expected));
        }
        for input in [
            "autorun maybe",
            "autorun on off",
            "physics quake",
            "physics quake1 original",
        ] {
            assert_eq!(submit(&mut c, input), None);
            assert!(c.lines.back().unwrap().starts_with("usage:"));
        }
        submit(&mut c, "help");
        assert!(c.lines.iter().any(|line| line.contains("autorun physics")));
        for (prefix, completed) in [("aut", "autorun "), ("phy", "physics ")] {
            c.insert(prefix);
            c.key(Keycode::Tab);
            assert_eq!(c.input, completed);
            c.input.clear();
            c.cursor = 0;
        }
    }
    #[test]
    fn sdl_text_input_only_works_open_and_overlay_is_clipped() {
        let mut c = Console::default();
        let event = Event::TextInput {
            timestamp: 0,
            window_id: 0,
            text: "hello".into(),
        };
        c.event(&event);
        assert!(c.input.is_empty());
        assert!(c.toggle());
        c.event(&event);
        assert_eq!(c.input, "hello");
        let mut frame = vec![0xffffffu32; 80 * 40];
        c.render(&mut frame, 80, 40);
        assert_eq!(frame[39 * 80], 0xffffff);
        assert_ne!(frame[0], 0xffffff);
        c.render::<u32>(&mut [], usize::MAX, 10);
        c.close();
        assert!(!c.is_open());
    }
    #[test]
    fn page_scroll_and_internal_clear_are_consistent() {
        let mut c = Console::default();
        for i in 0..20 {
            c.push(&format!("line {i}"));
        }
        c.key(Keycode::PageUp);
        assert_eq!(c.scroll, 8);
        c.key(Keycode::PageDown);
        assert_eq!(c.scroll, 0);
        submit(&mut c, "clear");
        assert!(c.lines.is_empty());
        submit(&mut c, "help");
        assert!(c.lines.iter().any(|line| line.contains("sensitivity")));
    }
}
