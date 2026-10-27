//! Typed teleport definitions and deterministic, headless trigger state.
//! Destination clearance, vertical state and network effects remain host-owned.
use std::error::Error;
use std::fmt;

const MAP_SIDE: u16 = 64;
const RAW_LIMIT: u16 = MAP_SIDE * 256;
const RADIUS: f32 = 0.5;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Teleport {
    /// Authored coordinates, retained to distinguish cell and position units.
    pub from_raw: [u16; 2],
    pub to_raw: [u16; 2],
    pub direction: u8,
    /// The installed expansion uses this sixth field; its meaning is unknown.
    pub extra: Option<u16>,
}
impl Teleport {
    /// Per the 2022 manual, each component below 64 names a cell center;
    /// 64..16383 is a position in units of 1/256 map tile.
    fn coordinate(value: u16) -> f32 {
        if value < MAP_SIDE {
            f32::from(value) + 0.5
        } else {
            f32::from(value) / 256.0
        }
    }
    pub fn source(&self) -> [f32; 2] {
        self.from_raw.map(Self::coordinate)
    }
    pub fn destination(&self) -> [f32; 2] {
        self.to_raw.map(Self::coordinate)
    }
    /// Direction conversion from the independent PanzerChasm reconstruction.
    /// The installed campaign uses 4 as well as the documented 0–3.
    pub fn angle_radians(&self) -> f32 {
        (-f32::from(self.direction) * std::f32::consts::FRAC_PI_2 - std::f32::consts::FRAC_PI_2)
            .rem_euclid(std::f32::consts::TAU)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TeleportParseError {
    pub line: usize,
    pub reason: &'static str,
}
impl fmt::Display for TeleportParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}: {}", self.line, self.reason)
    }
}
impl Error for TeleportParseError {}

/// Parse complete `#teleports` sections. Any invalid row rejects the whole
/// definition set; six-field rows are retained but are not executable yet.
pub fn parse(source: &str) -> Result<Vec<Teleport>, TeleportParseError> {
    let mut result = Vec::new();
    let mut inside = false;
    for (i, raw) in source.lines().enumerate() {
        let line = raw.split(';').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        if line.eq_ignore_ascii_case("#teleports") {
            if inside {
                return Err(TeleportParseError {
                    line: i + 1,
                    reason: "nested teleport section",
                });
            }
            inside = true;
            continue;
        }
        if !inside {
            continue;
        }
        if line.eq_ignore_ascii_case("#end") || line.eq_ignore_ascii_case("#end.") {
            inside = false;
            continue;
        }
        let words: Vec<_> = line.split_whitespace().collect();
        if !words[0].eq_ignore_ascii_case("tcenter") || !(words.len() == 6 || words.len() == 7) {
            return Err(TeleportParseError {
                line: i + 1,
                reason: "unsupported teleport row",
            });
        }
        let values: Vec<u16> = words[1..]
            .iter()
            .map(|w| {
                w.parse().map_err(|_| TeleportParseError {
                    line: i + 1,
                    reason: "invalid numeric teleport value",
                })
            })
            .collect::<Result<_, _>>()?;
        if values[..4].iter().any(|v| *v >= RAW_LIMIT) {
            return Err(TeleportParseError {
                line: i + 1,
                reason: "coordinate outside map",
            });
        }
        if values[4] > 4 {
            return Err(TeleportParseError {
                line: i + 1,
                reason: "unsupported direction",
            });
        }
        result.push(Teleport {
            from_raw: [values[0], values[1]],
            to_raw: [values[2], values[3]],
            direction: values[4] as u8,
            extra: values.get(5).copied(),
        });
    }
    if inside {
        return Err(TeleportParseError {
            line: source.lines().count(),
            reason: "unterminated teleport section",
        });
    }
    Ok(result)
}

/// Explicit trigger policy; original cooldown rules are unknown.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RearmPolicy {
    /// PanzerChasm tests every entity every tick and has no teleport cooldown.
    #[default]
    ReferenceContinuous,
    /// Provisional loop prevention: rearm after leaving all source circles.
    AfterLeavingSources,
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TeleportEvent {
    pub index: usize,
    pub source: [f32; 2],
    pub destination: [f32; 2],
    pub angle_radians: f32,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TeleportFailure {
    UnsupportedExtra { index: usize, value: u16 },
    DestinationBlocked { index: usize },
    InvalidDefinition { index: usize },
}
#[derive(Clone, Debug, Default)]
pub struct TeleportState {
    policy: RearmPolicy,
    waiting_for_exit: bool,
}
impl TeleportState {
    pub fn new(policy: RearmPolicy) -> Self {
        Self {
            policy,
            waiting_for_exit: false,
        }
    }
    /// Check one entity after movement, in authored order. The host's clearance
    /// predicate must cover its collision shape and destination occupancy.
    /// Failed or unsupported attempts do not modify the rearm state.
    pub fn tick(
        &mut self,
        definitions: &[Teleport],
        position: [f32; 2],
        is_clear: impl FnOnce([f32; 2]) -> bool,
    ) -> Result<Option<TeleportEvent>, TeleportFailure> {
        if let Some((index, _)) = definitions.iter().enumerate().find(|(_, teleport)| {
            teleport
                .from_raw
                .iter()
                .chain(teleport.to_raw.iter())
                .any(|v| *v >= RAW_LIMIT)
                || teleport.direction > 4
        }) {
            return Err(TeleportFailure::InvalidDefinition { index });
        }
        if !position
            .iter()
            .all(|v| v.is_finite() && *v >= 0.0 && *v < f32::from(MAP_SIDE))
        {
            return Ok(None);
        }
        let within = |p: &Teleport| {
            let c = p.source();
            let d = [c[0] - position[0], c[1] - position[1]];
            d[0] * d[0] + d[1] * d[1] < RADIUS * RADIUS
        };
        let found = definitions.iter().enumerate().find(|(_, p)| within(p));
        if self.waiting_for_exit {
            if found.is_none() {
                self.waiting_for_exit = false;
            }
            return Ok(None);
        }
        let Some((index, p)) = found else {
            return Ok(None);
        };
        if let Some(value) = p.extra {
            return Err(TeleportFailure::UnsupportedExtra { index, value });
        }
        let destination = p.destination();
        if !is_clear(destination) {
            return Err(TeleportFailure::DestinationBlocked { index });
        }
        self.waiting_for_exit = self.policy == RearmPolicy::AfterLeavingSources;
        Ok(Some(TeleportEvent {
            index,
            source: p.source(),
            destination,
            angle_radians: p.angle_radians(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_mixed_units_and_preserves_unknown_sixth_field() {
        let p=parse("#teleports\ntcenter 16 44 5 32 3\ntcenter 8 15 2220 7168 4\ntcenter 41 53 37 16 2 2\n#end\n").unwrap();
        assert_eq!(p[0].source(), [16.5, 44.5]);
        assert_eq!(p[0].destination(), [5.5, 32.5]);
        assert_eq!(p[1].destination(), [2220.0 / 256.0, 28.0]);
        assert_eq!(p[2].extra, Some(2));
        assert_eq!(p[0].angle_radians(), 0.0);
        assert!((p[1].angle_radians() - std::f32::consts::FRAC_PI_2 * 3.0).abs() < 1e-5);
    }
    #[test]
    fn malformed_sections_fail_closed() {
        for s in [
            "#teleports\ntcenter 64 x 1 1 0\n#end",
            "#teleports\ntcenter 1 1 16384 1 0\n#end",
            "#teleports\ntcenter 1 1 1 1 5\n#end",
            "#teleports\ntcenter 1 1 1 1 0",
        ] {
            assert!(parse(s).is_err(), "{s}");
        }
    }
    #[test]
    fn radius_order_clearance_and_unknown_flags_are_enforced() {
        let p =
            parse("#teleports\ntcenter 1 2 5 6 0\ntcenter 1 2 8 9 1\ntcenter 3 2 1 1 0 2\n#end")
                .unwrap();
        let mut state = TeleportState::default();
        assert!(state.tick(&p, [1.0, 2.5], |_| true).unwrap().is_none());
        assert_eq!(
            state.tick(&p, [1.5, 2.5], |_| false),
            Err(TeleportFailure::DestinationBlocked { index: 0 })
        );
        assert_eq!(
            state.tick(&p, [1.5, 2.5], |_| true).unwrap().unwrap().index,
            0
        );
        assert_eq!(
            state.tick(&p, [3.5, 2.5], |_| true),
            Err(TeleportFailure::UnsupportedExtra { index: 2, value: 2 })
        );
        assert!(state.tick(&p, [f32::NAN, 0.0], |_| true).unwrap().is_none());
        let mut invalid = p.clone();
        invalid[0].to_raw[0] = RAW_LIMIT;
        assert_eq!(
            state.tick(&invalid, [1.5, 2.5], |_| true),
            Err(TeleportFailure::InvalidDefinition { index: 0 })
        );
    }
    #[test]
    fn explicit_rearm_and_continuous_reference_policy() {
        let p = parse("#teleports\ntcenter 1 1 1 1 0\n#end").unwrap();
        let mut reference = TeleportState::default();
        assert!(reference.tick(&p, [1.5, 1.5], |_| true).unwrap().is_some());
        assert!(reference.tick(&p, [1.5, 1.5], |_| true).unwrap().is_some());
        let mut gated = TeleportState::new(RearmPolicy::AfterLeavingSources);
        assert!(gated.tick(&p, [1.5, 1.5], |_| true).unwrap().is_some());
        assert!(gated.tick(&p, [1.5, 1.5], |_| true).unwrap().is_none());
        assert!(gated.tick(&p, [2.5, 1.5], |_| true).unwrap().is_none());
        assert!(gated.tick(&p, [1.5, 1.5], |_| true).unwrap().is_some());
    }
}
