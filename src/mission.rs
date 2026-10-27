//! Mission completion timers and script message definitions.
//!
//! Process activation/locking remains the process runtime's responsibility.
//! This module consumes accepted/denied activation events, schedules level
//! completion, and produces presentation-neutral message IDs.
use crate::process::Script;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageLine {
    pub slot: char,
    pub x: i32,
    pub y: i32,
    pub text: String,
}
#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    pub id: u16,
    pub duration_seconds: f32,
    pub lines: Vec<MessageLine>,
}
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ProcessMission {
    /// `None` means the process does not end the level.
    pub end_delay_seconds: Option<f32>,
    pub on_message: Option<u16>,
    pub first_message: Option<u16>,
    pub lock_message: Option<u16>,
}
#[derive(Debug, Clone, PartialEq, Default)]
pub struct MissionPlan {
    pub messages: BTreeMap<u16, Message>,
    pub processes: BTreeMap<u16, ProcessMission>,
    pub diagnostics: Vec<String>,
}

fn message_id(command: &crate::process::Command, diagnostics: &mut Vec<String>) -> Option<u16> {
    let value = command
        .arguments
        .first()
        .and_then(|s| s.parse::<u16>().ok());
    match value {
        Some(0) => None, // zero is the no-message sentinel
        Some(value) if command.arguments.len() == 1 => Some(value),
        _ => {
            diagnostics.push(format!(
                "line {}: invalid {} message ID",
                command.line, command.name
            ));
            None
        }
    }
}

/// Read the first quoted text without treating a semicolon inside it as a
/// comment. The installed resources use plain quoted text and do not require
/// escape sequences; preserve backslashes rather than inventing semantics.
fn quoted(input: &str) -> Option<(&str, &str)> {
    let start = input.find('"')?;
    let end = input[start + 1..].find('"')? + start + 1;
    Some((&input[..start], &input[start + 1..end]))
}

impl MissionPlan {
    pub fn parse(script: &Script, source: &str) -> Self {
        let mut result = Self::default();
        for (&id, process) in &script.processes {
            let mut metadata = ProcessMission::default();
            for cmd in &process.properties {
                match cmd.name.as_str() {
                    "enddelay" => {
                        let delay = cmd.arguments.first().and_then(|s| s.parse::<f32>().ok());
                        match delay {
                            Some(value)
                                if cmd.arguments.len() == 1
                                    && value.is_finite()
                                    && value >= 0.0 =>
                            {
                                // The bundled remaster modding manual documents 13 as
                                // a special one-second completion delay.
                                metadata.end_delay_seconds = if value == 0.0 {
                                    None
                                } else if value == 13.0 {
                                    Some(1.0)
                                } else {
                                    Some(value)
                                };
                            }
                            _ => result
                                .diagnostics
                                .push(format!("line {}: invalid EndDelay", cmd.line)),
                        }
                    }
                    "onmessage" => metadata.on_message = message_id(cmd, &mut result.diagnostics),
                    "firstmessage" => {
                        metadata.first_message = message_id(cmd, &mut result.diagnostics)
                    }
                    "lockmessage" => {
                        metadata.lock_message = message_id(cmd, &mut result.diagnostics)
                    }
                    _ => {}
                }
            }
            result.processes.insert(id, metadata);
        }
        let mut active: Option<Message> = None;
        for (i, raw) in source.lines().enumerate() {
            let line = raw.trim();
            let clean = line.split(';').next().unwrap_or("").trim();
            if line.is_empty() || line.starts_with(';') {
                continue;
            }
            let lower = clean.to_ascii_lowercase();
            if let Some(suffix) = lower.strip_prefix("#mess") {
                let id_text = suffix.trim();
                if !id_text.is_empty() && id_text.bytes().all(|b| b.is_ascii_digit()) {
                    if active.is_some() {
                        result
                            .diagnostics
                            .push(format!("line {}: previous message not closed", i + 1));
                    }
                    match id_text.parse::<u16>() {
                        Ok(id) if id != 0 => {
                            active = Some(Message {
                                id,
                                duration_seconds: 0.0,
                                lines: Vec::new(),
                            })
                        }
                        _ => {
                            result
                                .diagnostics
                                .push(format!("line {}: invalid message ID", i + 1));
                            active = None;
                        }
                    }
                    continue;
                }
            }
            if lower == "#end" || lower == "#end." {
                if let Some(message) = active.take() {
                    let id = message.id;
                    if result.messages.insert(id, message).is_some() {
                        result
                            .diagnostics
                            .push(format!("line {}: duplicate message {id}", i + 1));
                    }
                }
                continue;
            }
            let Some(message) = active.as_mut() else {
                continue;
            };
            if lower.starts_with('#') {
                result
                    .diagnostics
                    .push(format!("line {}: message not closed", i + 1));
                active = None;
                continue;
            }
            if let Some((key, rest)) = line.split_once(':') {
                let key = key.trim().to_ascii_lowercase();
                if let Some(slot) = key
                    .strip_prefix("text")
                    .filter(|s| s.len() == 1)
                    .and_then(|s| s.chars().next())
                    .filter(|c| c.is_ascii_alphabetic())
                {
                    let Some((coordinates, text)) = quoted(rest) else {
                        result
                            .diagnostics
                            .push(format!("line {}: invalid message text", i + 1));
                        continue;
                    };
                    let numbers: Vec<_> = coordinates.split_whitespace().collect();
                    if numbers.len() == 2 {
                        if let (Ok(x), Ok(y)) = (numbers[0].parse(), numbers[1].parse()) {
                            message.lines.push(MessageLine {
                                slot,
                                x,
                                y,
                                text: text.to_owned(),
                            });
                            continue;
                        }
                    }
                    result
                        .diagnostics
                        .push(format!("line {}: invalid message coordinates", i + 1));
                    continue;
                }
            }
            let mut words = clean.split_whitespace();
            if words
                .next()
                .is_some_and(|word| word.eq_ignore_ascii_case("delay"))
            {
                if let Some(value) = words
                    .next()
                    .and_then(|s| s.parse::<f32>().ok())
                    .filter(|v| v.is_finite() && *v >= 0.0)
                {
                    if words.next().is_none() {
                        message.duration_seconds = value;
                        continue;
                    }
                }
                result
                    .diagnostics
                    .push(format!("line {}: invalid message Delay", i + 1));
            }
        }
        if active.is_some() {
            result
                .diagnostics
                .push("unterminated message at end of file".into());
        }
        for (&id, process) in &result.processes {
            for msg in [
                process.on_message,
                process.first_message,
                process.lock_message,
            ]
            .into_iter()
            .flatten()
            {
                if !result.messages.contains_key(&msg) {
                    result
                        .diagnostics
                        .push(format!("process {id} references missing message {msg}"));
                }
            }
        }
        result
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MissionEvent {
    ShowMessage(u16),
    LevelComplete,
}

/// State for one level and one player. The runtime must call `attempt` only for
/// actual activation attempts, passing whether the process accepted them.
#[derive(Debug, Clone)]
pub struct MissionState {
    pub plan: MissionPlan,
    first_messages_shown: BTreeSet<u16>,
    end_remaining: Option<f32>,
    completed: bool,
}
#[derive(Debug, Clone, PartialEq)]
pub struct MissionSnapshot {
    pub first_messages_shown: Vec<u16>,
    pub end_remaining: Option<f32>,
    pub completed: bool,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MissionSnapshotError {
    DuplicateProcess,
    InvalidProcess,
    InvalidTimer,
}
impl MissionState {
    pub fn snapshot(&self) -> MissionSnapshot {
        MissionSnapshot {
            first_messages_shown: self.first_messages_shown.iter().copied().collect(),
            end_remaining: self.end_remaining,
            completed: self.completed,
        }
    }
    /// Mission messages and timers are validated against the loaded plan.
    pub fn restore(&mut self, snapshot: &MissionSnapshot) -> Result<(), MissionSnapshotError> {
        if snapshot.completed && snapshot.end_remaining.is_some()
            || snapshot
                .end_remaining
                .is_some_and(|t| !t.is_finite() || t <= 0.0)
        {
            return Err(MissionSnapshotError::InvalidTimer);
        }
        let mut shown = BTreeSet::new();
        for id in &snapshot.first_messages_shown {
            if !shown.insert(*id) {
                return Err(MissionSnapshotError::DuplicateProcess);
            }
            let valid = self
                .plan
                .processes
                .get(id)
                .and_then(|p| p.first_message)
                .is_some_and(|message| self.plan.messages.contains_key(&message));
            if !valid {
                return Err(MissionSnapshotError::InvalidProcess);
            }
        }
        let max_delay = self
            .plan
            .processes
            .values()
            .filter_map(|p| p.end_delay_seconds)
            .filter(|delay| delay.is_finite() && *delay > 0.0)
            .fold(None, |max: Option<f32>, delay| {
                Some(max.map_or(delay, |m| m.max(delay)))
            });
        if (snapshot.completed || snapshot.end_remaining.is_some()) && max_delay.is_none()
            || snapshot
                .end_remaining
                .is_some_and(|remaining| max_delay.is_none_or(|max| remaining > max))
        {
            return Err(MissionSnapshotError::InvalidTimer);
        }
        self.first_messages_shown = shown;
        self.end_remaining = snapshot.end_remaining;
        self.completed = snapshot.completed;
        Ok(())
    }
    pub fn new(plan: MissionPlan) -> Self {
        Self {
            plan,
            first_messages_shown: BTreeSet::new(),
            end_remaining: None,
            completed: false,
        }
    }
    fn emit_message(&self, id: Option<u16>, events: &mut Vec<MissionEvent>) {
        if let Some(id) = id.filter(|id| self.plan.messages.contains_key(id)) {
            events.push(MissionEvent::ShowMessage(id));
        }
    }
    /// Handle a process activation or a rejected attempt (e.g. a locked door).
    /// FirstMessage is emitted on the first attempt, successful or not, per
    /// the independent reference; message throttling is not implemented.
    pub fn attempt(&mut self, process_id: u16, accepted: bool) -> Vec<MissionEvent> {
        let mut events = Vec::new();
        if self.completed {
            return events;
        }
        let Some(process) = self.plan.processes.get(&process_id).cloned() else {
            return events;
        };
        if accepted {
            self.emit_message(process.on_message, &mut events);
            if let Some(delay) = process.end_delay_seconds {
                self.end_remaining = Some(
                    self.end_remaining
                        .map_or(delay, |current| current.min(delay)),
                );
            }
        }
        if !self.first_messages_shown.contains(&process_id) {
            if let Some(id) = process
                .first_message
                .filter(|id| self.plan.messages.contains_key(id))
            {
                events.push(MissionEvent::ShowMessage(id));
                self.first_messages_shown.insert(process_id);
            }
        }
        if !accepted {
            self.emit_message(process.lock_message, &mut events);
        }
        events
    }
    /// Advance the pending completion timer by an authoritative simulation
    /// delta. Emits LevelComplete once and ignores invalid/negative times.
    pub fn advance(&mut self, dt: f32) -> Vec<MissionEvent> {
        if self.completed || !dt.is_finite() || dt <= 0.0 {
            return Vec::new();
        }
        if let Some(remaining) = &mut self.end_remaining {
            *remaining -= dt;
            if *remaining <= 0.0 {
                self.completed = true;
                self.end_remaining = None;
                return vec![MissionEvent::LevelComplete];
            }
        }
        Vec::new()
    }
    pub fn is_complete(&self) -> bool {
        self.completed
    }
    pub fn end_remaining(&self) -> Option<f32> {
        self.end_remaining
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mission_snapshot_preserves_message_once_and_timer() {
        let script = "#mess1\nDelay 1\nTextA: 0 0 \"Hello\"\n#end\n#proc1\nFirstMessage 1\nEndDelay 2\n#end\n";
        let plan = plan(script);
        let mut state = MissionState::new(plan.clone());
        assert_eq!(state.attempt(1, true), vec![MissionEvent::ShowMessage(1)]);
        state.advance(0.5);
        let saved = state.snapshot();
        let mut restored = MissionState::new(plan);
        restored.restore(&saved).unwrap();
        assert!(restored.attempt(1, true).is_empty());
        assert_eq!(state.advance(1.5), restored.advance(1.5));
        assert!(restored.is_complete());
        let before = restored.snapshot();
        let mut invalid = saved.clone();
        invalid.end_remaining = Some(f32::NAN);
        assert_eq!(
            restored.restore(&invalid),
            Err(MissionSnapshotError::InvalidTimer)
        );
        assert_eq!(restored.snapshot(), before);
        invalid = saved;
        invalid.first_messages_shown.push(99);
        assert_eq!(
            restored.restore(&invalid),
            Err(MissionSnapshotError::InvalidProcess)
        );
        assert_eq!(restored.snapshot(), before);
    }
    fn plan(text: &str) -> MissionPlan {
        MissionPlan::parse(&Script::parse(text), text)
    }
    #[test]
    fn parses_messages_and_procedure_properties() {
        let text = "#mess 1\nDelay 3\nTextA: -1 64 \"keep; moving\" ; comment\nTextB: 2 74 \"hello\"\n#end\n#proc13\nEndDelay 13\nOnMessage 1\n#action\n#end\n";
        let plan = plan(text);
        assert!(plan.diagnostics.is_empty(), "{:?}", plan.diagnostics);
        assert_eq!(plan.messages[&1].duration_seconds, 3.0);
        assert_eq!(plan.messages[&1].lines[0].text, "keep; moving");
        assert_eq!(plan.messages[&1].lines[1].x, 2);
        assert_eq!(plan.processes[&13].end_delay_seconds, Some(1.0));
        assert_eq!(plan.processes[&13].on_message, Some(1));
    }
    #[test]
    fn schedules_completion_only_after_success_and_emits_once() {
        let text = "#proc1\nEndDelay 0.25\n#action\n#end\n";
        let mut state = MissionState::new(plan(text));
        assert!(state.attempt(1, false).is_empty());
        assert_eq!(state.end_remaining(), None);
        state.attempt(1, true);
        assert!(state.advance(0.125).is_empty());
        assert!(state.advance(f32::NAN).is_empty());
        assert_eq!(state.advance(0.125), [MissionEvent::LevelComplete]);
        assert!(state.is_complete());
        assert!(state.advance(1.0).is_empty());
        assert!(state.attempt(1, true).is_empty());
    }
    #[test]
    fn first_message_and_locked_message_follow_attempts() {
        let text = "#mess 1\nDelay 1\nTextA: 0 0 \"first\"\n#end\n#mess 2\nDelay 1\nTextA: 0 0 \"locked\"\n#end\n#mess 3\nDelay 1\nTextA: 0 0 \"open\"\n#end\n#proc1\nFirstMessage 1\nLockMessage 2\nOnMessage 3\n#action\n#end\n";
        let mut state = MissionState::new(plan(text));
        assert_eq!(
            state.attempt(1, false),
            [MissionEvent::ShowMessage(1), MissionEvent::ShowMessage(2)]
        );
        assert_eq!(state.attempt(1, false), [MissionEvent::ShowMessage(2)]);
        assert_eq!(state.attempt(1, true), [MissionEvent::ShowMessage(3)]);
    }
    #[test]
    fn earliest_end_wins_and_malformed_data_is_reported() {
        let text = "#proc1\nEndDelay 2\n#end\n#proc2\nEndDelay 0.5\n#end\n#proc3\nEndDelay nan\nOnMessage 99\n#end\n";
        let plan = plan(text);
        assert_eq!(plan.diagnostics.len(), 2);
        let mut state = MissionState::new(plan);
        state.attempt(1, true);
        state.attempt(2, true);
        state.attempt(1, true);
        assert_eq!(state.end_remaining(), Some(0.5));
        assert_eq!(state.advance(0.5), [MissionEvent::LevelComplete]);
    }
}
