//! Bounded client-side developer console. The host owns mouse/text-input routing
//! and applies returned commands; no console command mutates simulation directly.
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
    "maxfps",
    "status",
    "restart",
    "quit",
    "sensitivity",
];

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Command {
    Fps(Option<bool>),
    /// Frames per second cap; zero means uncapped, None queries.
    MaxFps(Option<u32>),
    Status,
    Restart,
    Quit,
    /// Radians per mouse pixel; None requests the current value.
    Sensitivity(Option<f32>),
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
                .push("help cmdlist clear echo version fps maxfps status restart quit sensitivity"),
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
                    _ => self.push("usage: sensitivity [0.0001..1.0] (radians/pixel)"),
                },
                _ => self.push("usage: sensitivity [0.0001..1.0] (radians/pixel)"),
            },
            "status" | "restart" | "quit" if args.is_empty() => {
                return Some(match command.as_str() {
                    "status" => Command::Status,
                    "restart" => Command::Restart,
                    _ => Command::Quit,
                })
            }
            "help" | "cmdlist" | "clear" | "version" | "status" | "restart" | "quit" => {
                self.push(&format!("usage: {command}"))
            }
            _ => self.push(&format!("unknown command: {command}")),
        }
        None
    }

    /// Dark overlay occupying the top half of the frame, with bounded drawing.
    pub fn render(&self, frame: &mut [u32], width: usize, height: usize) {
        if !self.open
            || width == 0
            || height < 2
            || width.checked_mul(height).is_none_or(|n| n > frame.len())
        {
            return;
        }
        let overlay = height / 2;
        for pixel in frame.iter_mut().take(width * overlay) {
            *pixel = ((*pixel & 0xfefefe) >> 1) & 0x7f7f7f;
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
fn put(frame: &mut [u32], width: usize, height: usize, x: usize, y: usize, color: u32) {
    if x < width && y < height {
        frame[y * width + x] = color;
    }
}
fn draw_text(frame: &mut [u32], width: usize, height: usize, x: usize, y: usize, text: &str) {
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
        let mut frame = vec![0xffffff; 80 * 40];
        c.render(&mut frame, 80, 40);
        assert_eq!(frame[39 * 80], 0xffffff);
        assert_ne!(frame[0], 0xffffff);
        c.render(&mut [], usize::MAX, 10);
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
        assert!(c.lines.back().unwrap().contains("sensitivity"));
    }
}
