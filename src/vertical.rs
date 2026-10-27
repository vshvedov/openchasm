//! Independent vertical state with verified remaster default constants.
//!
//! Units are map tiles and seconds. The session supplies the relevant floor
//! and ceiling planes; horizontal collision, platform selection, moving-wall
//! crushing, and input bindings are separate responsibilities. `step` retains
//! the generic trapezoidal integrator; Session's normal movement uses the
//! separately reconstructed 500-microsecond integrator in `movement`.
use std::error::Error;
use std::fmt;

const MAX_SUBSTEP: f32 = 1.0 / 120.0;
const MAX_DELTA: f32 = 1.0;
const GROUND_EPSILON: f32 = 0.00001;
// Maximum permitted step height remains provisional; only the rise speed is
// established by the original post-collision stepping calculation.
const PLATFORM_PULL_DISTANCE: f32 = 0.4;
const PLATFORM_PULL_SPEED: f32 = 3.0;

/// Defaults follow the installed remaster's normal player at scale one;
/// see docs/movement-evidence.md. Alternate movement modes can override forces
/// without changing the independently represented body and eye dimensions.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VerticalConfig {
    pub gravity: f32,
    pub jump_speed: f32,
    /// Optional vertical speed bound; the default introduces no terminal cap.
    pub max_speed: f32,
    pub body_height: f32,
    pub eye_height: f32,
}
impl Default for VerticalConfig {
    fn default() -> Self {
        Self {
            gravity: 8.7,
            jump_speed: 2.75,
            max_speed: f32::MAX,
            body_height: 0.9375,
            eye_height: 0.75,
        }
    }
}
impl VerticalConfig {
    fn valid(self) -> bool {
        [
            self.gravity,
            self.jump_speed,
            self.max_speed,
            self.body_height,
            self.eye_height,
        ]
        .iter()
        .all(|x| x.is_finite())
            && self.gravity > 0.0
            && self.jump_speed > 0.0
            && self.max_speed > 0.0
            && self.body_height > 0.0
            && self.eye_height >= 0.0
            && self.eye_height <= self.body_height
    }
}

/// Height of floor and optional ceiling in world tile units. `None` represents
/// an open ceiling, not a guessed interpretation of a map texture index.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VerticalBounds {
    pub floor: f32,
    pub ceiling: Option<f32>,
}
impl VerticalBounds {
    /// Provisional fixed planes consistent with a two-tile-high room.
    pub const fn flat_room() -> Self {
        Self {
            floor: 0.0,
            ceiling: Some(2.0),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerticalError {
    InvalidConfiguration,
    InvalidBounds,
    InsufficientClearance,
    InvalidDelta,
}
impl fmt::Display for VerticalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConfiguration => f.write_str("invalid vertical physics configuration"),
            Self::InvalidBounds => f.write_str("invalid vertical plane bounds"),
            Self::InsufficientClearance => {
                f.write_str("player does not fit between floor and ceiling")
            }
            Self::InvalidDelta => f.write_str("invalid vertical simulation timestep"),
        }
    }
}
impl Error for VerticalError {}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct VerticalEvents {
    pub jumped: bool,
    pub landed: bool,
    pub hit_ceiling: bool,
}

/// Authoritative player feet elevation and vertical speed. Cloning preserves
/// state for snapshots and restart. No platform or networking assumptions.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VerticalState {
    feet: f32,
    velocity: f32,
    grounded: bool,
    config: VerticalConfig,
}
/// Mutable vertical values. Physics configuration is supplied by the freshly
/// reconstructed level and deliberately is not stored here.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerticalSnapshot {
    pub feet: f32,
    pub velocity: f32,
    pub grounded: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerticalSnapshotError {
    InvalidValue,
    InvalidBounds,
    OutsideBounds,
    InvalidGroundedState,
}

impl VerticalState {
    pub fn snapshot(&self) -> VerticalSnapshot {
        VerticalSnapshot {
            feet: self.feet,
            velocity: self.velocity,
            grounded: self.grounded,
        }
    }
    /// Validate intrinsic state and finite bounds. The head may overlap a
    /// newly lowered ceiling: Session keeps its prior vertical state until
    /// clearance returns. The next physics step resolves ordinary overlap.
    pub fn restore(
        &mut self,
        value: VerticalSnapshot,
        bounds: VerticalBounds,
    ) -> Result<(), VerticalSnapshotError> {
        if !value.feet.is_finite()
            || value.feet.abs() > 4096.0
            || !value.velocity.is_finite()
            || value.velocity.abs() > self.config.max_speed + GROUND_EPSILON
        {
            return Err(VerticalSnapshotError::InvalidValue);
        }
        if !bounds.floor.is_finite()
            || bounds
                .ceiling
                .is_some_and(|z| !z.is_finite() || z < bounds.floor)
        {
            return Err(VerticalSnapshotError::InvalidBounds);
        }
        if value.feet < bounds.floor - GROUND_EPSILON
            || bounds
                .ceiling
                .is_some_and(|z| value.feet > z + GROUND_EPSILON)
        {
            return Err(VerticalSnapshotError::OutsideBounds);
        }
        if value.grounded
            && ((value.feet - bounds.floor).abs() > GROUND_EPSILON
                || value.velocity.abs() > GROUND_EPSILON)
        {
            return Err(VerticalSnapshotError::InvalidGroundedState);
        }
        self.feet = value.feet;
        self.velocity = value.velocity;
        self.grounded = value.grounded;
        Ok(())
    }
    pub fn new(floor: f32) -> Result<Self, VerticalError> {
        Self::with_config(floor, VerticalConfig::default())
    }
    pub fn with_config(floor: f32, config: VerticalConfig) -> Result<Self, VerticalError> {
        if !config.valid() {
            return Err(VerticalError::InvalidConfiguration);
        }
        if !floor.is_finite() {
            return Err(VerticalError::InvalidBounds);
        }
        Ok(Self {
            feet: floor,
            velocity: 0.0,
            grounded: true,
            config,
        })
    }
    pub fn feet(&self) -> f32 {
        self.feet
    }
    pub fn velocity(&self) -> f32 {
        self.velocity
    }
    pub fn grounded(&self) -> bool {
        self.grounded
    }
    pub fn head(&self) -> f32 {
        self.feet + self.config.body_height
    }
    pub fn eye(&self) -> f32 {
        self.feet + self.config.eye_height
    }
    pub fn config(&self) -> VerticalConfig {
        self.config
    }

    /// Pull the feet toward an accessible nearby platform top. The caller
    /// must first confirm horizontal contact and supply current headroom.
    /// Returns false without changes when the top is too high or obstructed.
    pub fn pull_up(&mut self, top: f32, ceiling: Option<f32>, dt: f32) -> bool {
        if !top.is_finite()
            || !dt.is_finite()
            || !(0.0..=MAX_DELTA).contains(&dt)
            || dt == 0.0
            || top <= self.feet
            || top - self.feet > PLATFORM_PULL_DISTANCE
            || ceiling.is_some_and(|z| !z.is_finite() || top + self.config.body_height > z)
        {
            return false;
        }
        self.feet = (self.feet + PLATFORM_PULL_SPEED * dt).min(top);
        self.velocity = 0.0;
        self.grounded = false;
        true
    }

    /// Generic trapezoidal integration with bounded substeps for plane hits.
    /// This convenience API does not implement the remaster's 500 μs timing,
    /// held-jump cooldown or low-clearance jump; Session uses `movement` for it.
    /// `jump_pressed` is a press edge, not a held input. An airborne press is
    /// ignored; no buffering or automatic repeat is applied. Bounds must be
    /// the planes at the player's current horizontal position. If the player
    /// cannot fit, return an error without modifying the state; the session
    /// should resolve crushing or reject the horizontal move separately.
    pub fn step(
        &mut self,
        dt: f32,
        jump_pressed: bool,
        bounds: VerticalBounds,
    ) -> Result<VerticalEvents, VerticalError> {
        if !dt.is_finite() || !(0.0..=MAX_DELTA).contains(&dt) {
            return Err(VerticalError::InvalidDelta);
        }
        if !bounds.floor.is_finite() || bounds.ceiling.is_some_and(|z| !z.is_finite()) {
            return Err(VerticalError::InvalidBounds);
        }
        if bounds
            .ceiling
            .is_some_and(|z| z - bounds.floor < self.config.body_height)
        {
            return Err(VerticalError::InsufficientClearance);
        }
        if dt == 0.0 {
            return Ok(VerticalEvents::default());
        }
        let mut next = *self;
        let mut events = VerticalEvents::default();
        // The caller handles horizontal motion/platform collision. If a plane
        // rises under the player, place the feet on it rather than embedding.
        if next.feet < bounds.floor {
            next.feet = bounds.floor;
            if next.velocity < 0.0 {
                next.velocity = 0.0;
            }
        }
        if let Some(ceiling) = bounds.ceiling {
            let max_feet = ceiling - next.config.body_height;
            if next.feet > max_feet {
                next.feet = max_feet;
                if next.velocity > 0.0 {
                    next.velocity = 0.0;
                }
                events.hit_ceiling = true;
            }
        }
        next.grounded = next.feet <= bounds.floor + GROUND_EPSILON && next.velocity <= 0.0;
        if jump_pressed && next.grounded {
            next.velocity = next.config.jump_speed.min(next.config.max_speed);
            next.grounded = false;
            events.jumped = true;
        }
        let steps = (dt / MAX_SUBSTEP).ceil() as usize;
        let h = dt / steps as f32;
        for _ in 0..steps {
            // Trapezoidal integration and bounded substeps keep the arc close
            // across frame rates, including the terminal velocity transition.
            let previous_velocity = next.velocity;
            let velocity =
                (previous_velocity - next.config.gravity * h).max(-next.config.max_speed);
            let position = next.feet + (previous_velocity + velocity) * 0.5 * h;
            next.velocity = velocity;
            next.feet = position;
            if let Some(ceiling) = bounds.ceiling {
                let max_feet = ceiling - next.config.body_height;
                if next.feet > max_feet {
                    next.feet = max_feet;
                    if next.velocity > 0.0 {
                        next.velocity = 0.0;
                    }
                    events.hit_ceiling = true;
                }
            }
            if next.feet <= bounds.floor {
                if !next.grounded {
                    events.landed = true;
                }
                next.feet = bounds.floor;
                next.velocity = 0.0;
                next.grounded = true;
            } else {
                next.grounded = false;
            }
        }
        *self = next;
        Ok(events)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn platform_pull_requires_reachable_top_and_clear_headroom() {
        let mut state = VerticalState::new(0.0).unwrap();
        assert!(!state.pull_up(0.875, Some(2.0), 1.0 / 60.0));
        state.step(0.2, true, VerticalBounds::flat_room()).unwrap();
        assert!(!state.pull_up(0.7, Some(1.5), 1.0 / 60.0));
        let before = state.feet();
        assert!(state.pull_up(0.7, Some(2.0), 1.0 / 60.0));
        assert!((state.feet() - before - 3.0 / 60.0).abs() < 1e-5);
        for _ in 0..20 {
            state.pull_up(0.7, Some(2.0), 1.0 / 60.0);
        }
        assert_eq!(state.feet(), 0.7);
        assert_eq!(state.velocity(), 0.0);
    }
    #[test]
    fn snapshot_restores_jump_and_rejects_impossible_state_atomically() {
        let bounds = VerticalBounds::flat_room();
        let mut source = VerticalState::new(0.0).unwrap();
        source.step(1.0 / 60.0, true, bounds).unwrap();
        let snapshot = source.snapshot();
        let mut restored = VerticalState::new(0.0).unwrap();
        restored.restore(snapshot, bounds).unwrap();
        assert_eq!(restored, source);
        for _ in 0..20 {
            source.step(1.0 / 60.0, false, bounds).unwrap();
            restored.step(1.0 / 60.0, false, bounds).unwrap();
        }
        assert_eq!(restored, source);
        let before = restored;
        let mut invalid = snapshot;
        invalid.feet = f32::NAN;
        assert_eq!(
            restored.restore(invalid, bounds),
            Err(VerticalSnapshotError::InvalidValue)
        );
        assert_eq!(restored, before);
        invalid = snapshot;
        invalid.grounded = true;
        assert_eq!(
            restored.restore(invalid, bounds),
            Err(VerticalSnapshotError::InvalidGroundedState)
        );
        assert_eq!(restored, before);
    }
    #[test]
    fn default_has_verified_dimensions_and_accepts_finite_unclamped_fall_speed() {
        let mut state = VerticalState::new(0.0).unwrap();
        assert_eq!(state.head(), 0.9375);
        assert_eq!(state.eye(), 0.75);
        assert_eq!(state.config().gravity, 8.7);
        assert_eq!(state.config().jump_speed, 2.75);
        assert_eq!(state.config().max_speed, f32::MAX);
        let bounds = VerticalBounds {
            floor: 0.0,
            ceiling: None,
        };
        let falling = VerticalSnapshot {
            feet: 100.0,
            velocity: -100.0,
            grounded: false,
        };
        state.restore(falling, bounds).unwrap();
        state.step(1.0 / 60.0, false, bounds).unwrap();
        assert!(state.velocity() < -100.0);
        let before = state;
        for velocity in [f32::NEG_INFINITY, f32::INFINITY, f32::NAN] {
            assert_eq!(
                state.restore(
                    VerticalSnapshot {
                        velocity,
                        ..falling
                    },
                    bounds
                ),
                Err(VerticalSnapshotError::InvalidValue)
            );
            assert_eq!(state, before);
        }
        for feet in [-0.01, 4097.0, f32::INFINITY, f32::NAN] {
            assert!(state
                .restore(VerticalSnapshot { feet, ..falling }, bounds)
                .is_err());
            assert_eq!(state, before);
        }
        // A caller-selected capped mode still gets its requested validation.
        let mut capped = VerticalState::with_config(
            0.0,
            VerticalConfig {
                max_speed: 5.0,
                ..VerticalConfig::default()
            },
        )
        .unwrap();
        assert_eq!(
            capped.restore(falling, bounds),
            Err(VerticalSnapshotError::InvalidValue)
        );
    }
    #[test]
    fn restore_accepts_state_held_during_insufficient_ceiling_clearance() {
        let mut source = VerticalState::new(0.0).unwrap();
        let blocked = VerticalBounds {
            floor: 0.0,
            ceiling: Some(0.2),
        };
        let before = source.snapshot();
        assert_eq!(
            source.step(1.0 / 60.0, false, blocked),
            Err(VerticalError::InsufficientClearance)
        );
        assert_eq!(source.snapshot(), before);
        let mut restored = VerticalState::new(0.0).unwrap();
        restored.restore(before, blocked).unwrap();
        assert_eq!(restored, source);
        let clear = VerticalBounds::flat_room();
        assert_eq!(
            restored.step(1.0 / 60.0, true, clear),
            source.step(1.0 / 60.0, true, clear)
        );
        assert_eq!(restored, source);
        // A horizontal move after the vertical tick may enter a lower but
        // otherwise valid ceiling before the next vertical correction.
        let airborne = source.snapshot();
        let low = VerticalBounds {
            floor: 0.0,
            ceiling: Some(0.9375),
        };
        assert!(airborne.feet + source.config().body_height > low.ceiling.unwrap());
        restored.restore(airborne, low).unwrap();
        assert_eq!(restored.snapshot(), airborne);
        let prior = restored;
        assert_eq!(
            restored.restore(
                before,
                VerticalBounds {
                    floor: 1.0,
                    ceiling: Some(0.2)
                }
            ),
            Err(VerticalSnapshotError::InvalidBounds)
        );
        assert_eq!(restored, prior);
    }
    #[test]
    fn generic_jump_arc_uses_verified_default_forces_and_camera_height() {
        let mut player = VerticalState::new(0.0).unwrap();
        assert_eq!(player.eye(), 0.75);
        assert!(
            player
                .step(1.0 / 60.0, true, VerticalBounds::flat_room())
                .unwrap()
                .jumped
        );
        let mut max_height = player.feet();
        let mut landed = false;
        for _ in 0..120 {
            let event = player
                .step(1.0 / 60.0, false, VerticalBounds::flat_room())
                .unwrap();
            max_height = max_height.max(player.feet());
            landed |= event.landed;
        }
        assert!(
            (max_height - (2.75f32 * 2.75 / (2.0 * 8.7))).abs() < 0.01,
            "{max_height}"
        );
        assert!(landed && player.grounded());
        assert_eq!(player.feet(), 0.0);
        assert_eq!(player.velocity(), 0.0);
    }
    #[test]
    fn jumping_hits_ceiling_without_crossing_it() {
        let mut player = VerticalState::new(0.0).unwrap();
        let room = VerticalBounds {
            floor: 0.0,
            ceiling: Some(1.0),
        };
        let mut hit = player.step(1.0 / 60.0, true, room).unwrap().hit_ceiling;
        for _ in 0..60 {
            hit |= player.step(1.0 / 60.0, false, room).unwrap().hit_ceiling;
            assert!(player.head() <= 1.0 + 1e-6);
        }
        assert!(hit && player.grounded());
    }
    #[test]
    fn airborne_press_does_not_double_jump_and_open_sky_is_supported() {
        let mut player = VerticalState::new(0.0).unwrap();
        let open = VerticalBounds {
            floor: 0.0,
            ceiling: None,
        };
        player.step(1.0 / 60.0, true, open).unwrap();
        let previous = player.velocity();
        assert!(!player.step(1.0 / 60.0, true, open).unwrap().jumped);
        assert!(player.velocity() < previous);
    }
    #[test]
    fn dropping_and_raising_floor_updates_ground_state() {
        let mut player = VerticalState::new(0.0).unwrap();
        let lowered = VerticalBounds {
            floor: -1.0,
            ceiling: Some(2.0),
        };
        player.step(1.0 / 60.0, false, lowered).unwrap();
        assert!(!player.grounded() && player.feet() < 0.0);
        let raised = VerticalBounds {
            floor: 0.25,
            ceiling: Some(2.0),
        };
        player.step(1.0 / 60.0, false, raised).unwrap();
        assert!(player.grounded());
        assert_eq!(player.feet(), 0.25);
    }
    #[test]
    fn invalid_contexts_are_rejected_without_corrupting_state() {
        let mut player = VerticalState::new(0.0).unwrap();
        let before = player;
        assert_eq!(
            player.step(
                0.1,
                true,
                VerticalBounds {
                    floor: 0.0,
                    ceiling: Some(0.5)
                }
            ),
            Err(VerticalError::InsufficientClearance)
        );
        assert_eq!(
            player.step(f32::NAN, true, VerticalBounds::flat_room()),
            Err(VerticalError::InvalidDelta)
        );
        assert_eq!(
            player.step(2.0, true, VerticalBounds::flat_room()),
            Err(VerticalError::InvalidDelta)
        );
        assert_eq!(
            player.step(
                0.1,
                true,
                VerticalBounds {
                    floor: f32::NAN,
                    ceiling: None
                }
            ),
            Err(VerticalError::InvalidBounds)
        );
        assert_eq!(player, before);
        let cfg = VerticalConfig {
            gravity: 0.0,
            ..VerticalConfig::default()
        };
        assert_eq!(
            VerticalState::with_config(0.0, cfg),
            Err(VerticalError::InvalidConfiguration)
        );
    }
    #[test]
    fn equivalent_small_steps_produce_equivalent_height() {
        let mut fixed = VerticalState::new(0.0).unwrap();
        let mut combined = fixed;
        let bounds = VerticalBounds::flat_room();
        fixed.step(1.0 / 60.0, true, bounds).unwrap();
        for _ in 0..5 {
            fixed.step(1.0 / 60.0, false, bounds).unwrap();
        }
        combined.step(0.1, true, bounds).unwrap();
        assert!((fixed.feet() - combined.feet()).abs() < 1e-5);
        assert!((fixed.velocity() - combined.velocity()).abs() < 1e-5);
    }
}
