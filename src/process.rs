//! Parser for level process scripts. The parser retains unknown commands so
//! additional game behavior can be implemented without losing source data.
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq)]
pub struct Command {
    pub name: String,
    pub arguments: Vec<String>,
    pub line: usize,
}
#[derive(Debug, Clone, PartialEq)]
pub struct Process {
    pub id: u16,
    pub properties: Vec<Command>,
    pub actions: Vec<Command>,
}
#[derive(Debug, Clone, PartialEq)]
pub struct Trigger {
    pub kind: String,
    pub x: u8,
    pub y: u8,
    pub process: u16,
}
#[derive(Debug, Default)]
pub struct Script {
    pub processes: BTreeMap<u16, Process>,
    pub triggers: Vec<Trigger>,
    /// Malformed recognized sections are reported without discarding parsed commands.
    pub diagnostics: Vec<String>,
}

#[derive(Clone, Copy)]
enum Section {
    Other,
    Process(u16),
    Actions(u16),
    Links,
}

impl Script {
    pub fn parse(text: &str) -> Self {
        let mut result = Self::default();
        let mut section = Section::Other;
        for (line_number, raw) in text.lines().enumerate() {
            // Process command lines do not use quoted strings; messages are kept
            // out of these sections, so a semicolon starts a comment here.
            let line = raw.split(';').next().unwrap_or("").trim();
            if line.is_empty() || line == "." {
                continue;
            }
            let mut words = line.split_whitespace();
            let first = words.next().unwrap_or("");
            let lower = first.to_ascii_lowercase();
            if let Some(suffix) = lower.strip_prefix("#proc") {
                let number = if suffix.is_empty() {
                    words.next().unwrap_or("")
                } else {
                    suffix
                };
                if let Ok(id) = number.parse::<u16>() {
                    if result.processes.contains_key(&id) {
                        result
                            .diagnostics
                            .push(format!("line {}: duplicate process {id}", line_number + 1));
                    }
                    result.processes.insert(
                        id,
                        Process {
                            id,
                            properties: Vec::new(),
                            actions: Vec::new(),
                        },
                    );
                    section = Section::Process(id);
                } else {
                    result
                        .diagnostics
                        .push(format!("line {}: invalid process ID", line_number + 1));
                    section = Section::Other;
                }
                continue;
            }
            match lower.as_str() {
                "#action" => {
                    if let Section::Process(id) | Section::Actions(id) = section {
                        section = Section::Actions(id);
                    } else {
                        result
                            .diagnostics
                            .push(format!("line {}: action outside process", line_number + 1));
                    }
                    continue;
                }
                "#links" => {
                    section = Section::Links;
                    continue;
                }
                "#end" | "#end." => {
                    section = Section::Other;
                    continue;
                }
                _ if lower.starts_with('#') => {
                    section = Section::Other;
                    continue;
                }
                _ => {}
            }
            let args: Vec<String> = words.map(str::to_owned).collect();
            match section {
                Section::Process(id) | Section::Actions(id) => {
                    let cmd = Command {
                        name: lower,
                        arguments: args,
                        line: line_number + 1,
                    };
                    if let Some(process) = result.processes.get_mut(&id) {
                        if matches!(section, Section::Actions(_)) {
                            process.actions.push(cmd);
                        } else {
                            process.properties.push(cmd);
                        }
                    }
                }
                Section::Links => {
                    let parsed = (|| {
                        Some(Trigger {
                            kind: lower,
                            x: args.first()?.parse().ok()?,
                            y: args.get(1)?.parse().ok()?,
                            process: args.get(2)?.parse().ok()?,
                        })
                    })();
                    if let Some(trigger) = parsed {
                        if trigger.x < 64 && trigger.y < 64 {
                            result.triggers.push(trigger);
                        } else {
                            result.diagnostics.push(format!(
                                "line {}: trigger coordinate outside map",
                                line_number + 1
                            ));
                        }
                    } else {
                        result
                            .diagnostics
                            .push(format!("line {}: invalid trigger", line_number + 1));
                    }
                }
                Section::Other => {}
            }
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_spaced_and_compact_processes_and_links() {
        let script = Script::parse("#proc 1\nSpeed 8\n#action\nmove 24 52 0 -128 ; door\n#end\n#proc02\nLock\n#action\nunlock 1\n#end.\n#links\nFloor 25 52 1\nlink 20 10 2\n#end\n");
        assert!(script.diagnostics.is_empty());
        assert_eq!(script.processes.len(), 2);
        assert_eq!(script.processes[&1].properties[0].name, "speed");
        assert_eq!(
            script.processes[&1].actions[0].arguments,
            ["24", "52", "0", "-128"]
        );
        assert_eq!(script.processes[&2].actions[0].name, "unlock");
        assert_eq!(
            script.triggers[0],
            Trigger {
                kind: "floor".into(),
                x: 25,
                y: 52,
                process: 1
            }
        );
    }
    #[test]
    fn reports_bad_links_and_duplicate_ids() {
        let script =
            Script::parse("#proc1\n#end\n#proc 1\n#end\n#links\nfloor 64 0 1\nlink x 2 1\n#end\n");
        assert_eq!(script.diagnostics.len(), 3);
        assert!(script.triggers.is_empty());
    }
}
