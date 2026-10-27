//! Fixed-tick remaster normal movement and optional Quake 1 movement.
//!
//! Normal-mode constants, command quantization, integration, and jump rules
//! follow the owned remaster evidence in `docs/movement-evidence.md`.
//!
//! Independently implemented from mathematical behavior in id Software's
//! original sources, without incorporating their GPL implementation:
//! - https://github.com/id-Software/Quake/blob/master/WinQuake/sv_user.c
//! - https://github.com/id-Software/Quake/blob/master/WinQuake/sv_phys.c
//! - https://github.com/id-Software/Quake/blob/master/WinQuake/cl_input.c
//! - https://github.com/id-Software/Quake-Tools/blob/master/qcc/v101qc/client.qc
//!
//! Quake 1 is a compatibility option, not a claim about original Chasm.
//! The scale of 64 Quake units per tile is an explicit adaptation. World
//! collision remains Chasm's swept circles and vertical planes; BSP stairs,
//! slopes, water, and pitch-dependent wish directions are not implemented.
//! Fixed 60 Hz ticks give repeatable authoritative replay, not a promise of
//! bit-identical floating-point trigonometry across different architectures.
use crate::vertical::{
    VerticalBounds, VerticalConfig, VerticalError, VerticalEvents, VerticalSnapshot,
};

pub const TICK_SECONDS: f32 = 1.0 / 60.0;
pub const QUAKE_UNITS_PER_TILE: f32 = 64.0;
pub const QUAKE_MAX_SPEED: f32 = 320.0 / QUAKE_UNITS_PER_TILE;
pub const QUAKE_GRAVITY: f32 = 800.0 / QUAKE_UNITS_PER_TILE;
pub const QUAKE_JUMP_SPEED: f32 = 270.0 / QUAKE_UNITS_PER_TILE;
pub const QUAKE_MAX_VELOCITY: f32 = 2000.0 / QUAKE_UNITS_PER_TILE;
/// Installed-remaster normal-mode defaults; see docs/movement-evidence.md.
pub const ORIGINAL_RUN_SPEED: f32 = 5.1975;
pub const ORIGINAL_GRAVITY: f32 = 8.7;
pub const ORIGINAL_JUMP_SPEED: f32 = 2.75;
const ORIGINAL_STEP_US: u32 = 500;
const ORIGINAL_STEP_SECONDS: f32 = 0.0005;
const ORIGINAL_JUMP_COOLDOWN_US: u32 = 400_000;
const AIR_DIRECTION_CAP: f32 = 30.0 / QUAKE_UNITS_PER_TILE;
const STOP_SPEED: f32 = 100.0 / QUAKE_UNITS_PER_TILE;
const ACCELERATION: f32 = 10.0;
const FRICTION: f32 = 4.0;
const GROUND_EPSILON: f32 = 0.00001;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MovementMode {
    #[default]
    Original,
    Quake1,
}

/// Run is the resolved gameplay intent after the client's autorun/key binding
/// policy. Jump is a held state, not a one-tick press event.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct MovementCommand {
    pub forward: f32,
    pub strafe: f32,
    pub running: bool,
    pub jump_held: bool,
}

/// Authored developer-setting ranges reconstructed from the remaster. The
/// normal user's walk/run choice is separate from these physics settings.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OriginalConfig {
    pub movement_speed: f32,
    pub friction: f32,
}
impl Default for OriginalConfig {
    fn default() -> Self {
        Self {
            movement_speed: 1.0,
            friction: 1.0,
        }
    }
}
impl OriginalConfig {
    fn valid(self) -> bool {
        self.movement_speed.is_finite()
            && (0.0..=4.0).contains(&self.movement_speed)
            && self.friction.is_finite()
            && (0.0..=1.0).contains(&self.friction)
    }
}

/// Persist alongside the session's position and VerticalSnapshot. Mode and
/// momentum are restored together, independent of local client preferences.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MovementSnapshot {
    pub mode: MovementMode,
    pub velocity: [f32; 2],
    pub jump_released: bool,
    pub tick: u64,
    pub original_remainder_us: u16,
    pub jump_cooldown_us: u32,
}
impl Default for MovementSnapshot {
    fn default() -> Self {
        Self {
            mode: MovementMode::Original,
            velocity: [0.0; 2],
            jump_released: true,
            tick: 0,
            original_remainder_us: 0,
            jump_cooldown_us: 0,
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct MovementState {
    state: MovementSnapshot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MovementError {
    InvalidInput,
    InvalidVelocity,
    TickOverflow,
    InvalidConfiguration,
    InvalidClock,
    Vertical(VerticalError),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MovementStep {
    /// Sweep this displacement together with wind/platform contributions.
    pub displacement: [f32; 2],
    /// A permitted grounded jump. Pass to quake_vertical_step this tick.
    pub jump: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OriginalStep {
    /// Sum of attempted displacements. The contact callback owns actual world
    /// positions, so this is not a report of distance traveled after collisions.
    pub displacement: [f32; 2],
    pub vertical: VerticalSnapshot,
    pub events: VerticalEvents,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OriginalContact {
    pub velocity: [f32; 2],
    pub vertical: VerticalSnapshot,
    pub bounds: VerticalBounds,
}

impl MovementState {
    pub fn snapshot(&self) -> MovementSnapshot {
        self.state
    }

    pub fn restore(&mut self, snapshot: MovementSnapshot) -> Result<(), MovementError> {
        validate_velocity(snapshot.velocity)?;
        if u32::from(snapshot.original_remainder_us) >= ORIGINAL_STEP_US
            || snapshot.jump_cooldown_us > ORIGINAL_JUMP_COOLDOWN_US
        {
            return Err(MovementError::InvalidClock);
        }
        self.state = snapshot;
        Ok(())
    }

    pub fn velocity(&self) -> [f32; 2] {
        self.state.velocity
    }

    pub fn mode(&self) -> MovementMode {
        self.state.mode
    }

    /// A user-selected mode change starts without the prior mode's momentum
    /// or held-jump latch. The authoritative tick counter remains monotonic.
    pub fn set_mode(&mut self, mode: MovementMode) {
        if self.state.mode != mode {
            self.state.mode = mode;
            self.state.velocity = [0.0; 2];
            self.state.jump_released = true;
            self.state.original_remainder_us = 0;
            self.state.jump_cooldown_us = 0;
        }
    }

    /// Publish the final velocity from the collision solver, after clipping
    /// against contact planes. Do not use total distance traveled / tick time:
    /// a collision late in the tick must still cancel the blocked velocity.
    /// Wind displacement is not persistent player acceleration.
    pub fn set_collision_velocity(&mut self, velocity: [f32; 2]) -> Result<(), MovementError> {
        validate_velocity(velocity)?;
        self.state.velocity = velocity;
        Ok(())
    }

    pub fn commit_collision_velocity(&mut self, velocity: [f32; 2]) -> Result<(), MovementError> {
        self.set_collision_velocity(velocity)
    }

    /// Normal remaster motion, including its carried 500-microsecond steps.
    /// This convenience form has no horizontal contacts; the session should
    /// use step_original_with_contacts for continuous geometry interaction.
    /// The 60 Hz clock distributes microseconds as 16666,16667,16667, so no
    /// simulation time is lost through repeated integer truncation.
    pub fn step_original(
        &mut self,
        command: MovementCommand,
        yaw: f32,
        current_vertical: VerticalSnapshot,
        bounds: VerticalBounds,
        body_height: f32,
    ) -> Result<OriginalStep, MovementError> {
        self.step_original_with_contacts(
            command,
            yaw,
            current_vertical,
            bounds,
            body_height,
            |_, velocity, vertical| OriginalContact {
                velocity,
                vertical,
                bounds,
            },
        )
    }

    /// Invoke the caller's continuous sweep after every 500-microsecond
    /// gravity/motion proposal. Returned bounds apply to the next substep.
    /// State commits only on success; callers must roll back their callback's
    /// world/vertical side effects if a later contact returns invalid data.
    #[allow(clippy::too_many_arguments)]
    pub fn step_original_with_contacts<F>(
        &mut self,
        command: MovementCommand,
        yaw: f32,
        current_vertical: VerticalSnapshot,
        bounds: VerticalBounds,
        body_height: f32,
        contacts: F,
    ) -> Result<OriginalStep, MovementError>
    where
        F: FnMut([f32; 2], [f32; 2], VerticalSnapshot) -> OriginalContact,
    {
        self.step_original_configured(
            command,
            yaw,
            current_vertical,
            bounds,
            body_height,
            OriginalConfig::default(),
            contacts,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn step_original_configured<F>(
        &mut self,
        command: MovementCommand,
        yaw: f32,
        current_vertical: VerticalSnapshot,
        mut bounds: VerticalBounds,
        body_height: f32,
        config: OriginalConfig,
        mut contacts: F,
    ) -> Result<OriginalStep, MovementError>
    where
        F: FnMut([f32; 2], [f32; 2], VerticalSnapshot) -> OriginalContact,
    {
        if ![command.forward, command.strafe, yaw]
            .iter()
            .all(|v| v.is_finite())
        {
            return Err(MovementError::InvalidInput);
        }
        if !config.valid() {
            return Err(MovementError::InvalidConfiguration);
        }
        validate_vertical(current_vertical, bounds, body_height)
            .map_err(MovementError::Vertical)?;
        let mut next = self.state;
        next.tick = next
            .tick
            .checked_add(1)
            .ok_or(MovementError::TickOverflow)?;
        next.mode = MovementMode::Original;
        let duration_us = if self.state.tick.is_multiple_of(3) {
            16_666
        } else {
            16_667
        };
        next.jump_cooldown_us = next.jump_cooldown_us.saturating_sub(duration_us);
        let accumulated = u32::from(next.original_remainder_us) + duration_us;
        let steps = accumulated / ORIGINAL_STEP_US;
        next.original_remainder_us = (accumulated % ORIGINAL_STEP_US) as u16;
        let (sin, cos) = yaw.sin_cos();
        let forward = command.forward.clamp(-1.0, 1.0);
        let strafe = command.strafe.clamp(-1.0, 1.0);
        let direction = [cos * forward + sin * strafe, sin * forward - cos * strafe];
        let length = (direction[0] * direction[0] + direction[1] * direction[1]).sqrt();
        let acceleration_scale = 15.0 * config.movement_speed;
        let speed = f32::from_bits(0x3eb16873)
            * acceleration_scale
            * if command.running { 1.0 } else { 0.6 };
        let target = if length > 0.0 {
            // Preserve command packing's multiplication/division order and
            // signed 16-bit decode, including +pi wrapping to -pi.
            let theta = direction[1].atan2(direction[0]);
            let encoded = (theta * 65536.0 / std::f32::consts::TAU) as i32 as i16;
            let theta = f32::from(encoded) * (1.0 / 65536.0) * std::f32::consts::TAU;
            let (sin, cos) = theta.sin_cos();
            [cos * speed, sin * speed]
        } else {
            [0.0; 2]
        };
        let acceleration = acceleration_scale * (1.5 + 2.0 * config.friction);
        let mut vertical = current_vertical;
        let mut events = VerticalEvents::default();
        let mut displacement = [0.0; 2];
        for _ in 0..steps {
            let delta = [target[0] - next.velocity[0], target[1] - next.velocity[1]];
            let difference = (delta[0] * delta[0] + delta[1] * delta[1]).sqrt();
            if difference > 0.0 {
                let change = difference.min(acceleration * ORIGINAL_STEP_SECONDS);
                for (velocity, delta) in next.velocity.iter_mut().zip(delta) {
                    *velocity += (delta / difference) * change;
                }
            }
            let substep_displacement = next.velocity.map(|v| v * ORIGINAL_STEP_SECONDS);
            for (distance, delta) in displacement.iter_mut().zip(substep_displacement) {
                *distance += delta;
            }
            correct_vertical_overlap(&mut vertical, bounds, body_height, &mut events);
            vertical.grounded = vertical.feet == bounds.floor && vertical.velocity <= 0.0;
            if command.jump_held && vertical.grounded && next.jump_cooldown_us == 0 {
                let clearance = bounds
                    .ceiling
                    .map_or(f32::INFINITY, |ceiling| ceiling - bounds.floor);
                vertical.velocity += if clearance < 1.328125 {
                    1.0
                } else {
                    ORIGINAL_JUMP_SPEED
                };
                vertical.grounded = false;
                events.jumped = true;
                next.jump_cooldown_us = ORIGINAL_JUMP_COOLDOWN_US;
            }
            vertical.velocity -= ORIGINAL_GRAVITY * ORIGINAL_STEP_SECONDS;
            vertical.feet += vertical.velocity * ORIGINAL_STEP_SECONDS;
            correct_vertical_overlap(&mut vertical, bounds, body_height, &mut events);
            if vertical.feet <= bounds.floor {
                events.landed |= !vertical.grounded;
                vertical.feet = bounds.floor;
                vertical.velocity = 0.0;
                vertical.grounded = true;
            } else {
                vertical.grounded = false;
            }
            let contact = contacts(substep_displacement, next.velocity, vertical);
            validate_velocity(contact.velocity)?;
            validate_vertical(contact.vertical, contact.bounds, body_height)
                .map_err(MovementError::Vertical)?;
            next.velocity = contact.velocity;
            vertical = contact.vertical;
            bounds = contact.bounds;
            correct_vertical_overlap(&mut vertical, bounds, body_height, &mut events);
            if vertical.feet <= bounds.floor && vertical.velocity <= 0.0 {
                events.landed |= !vertical.grounded;
                vertical.feet = bounds.floor;
                vertical.velocity = 0.0;
                vertical.grounded = true;
            } else {
                vertical.grounded = false;
            }
        }
        next.jump_released = !command.jump_held;
        self.state = next;
        Ok(OriginalStep {
            displacement,
            vertical,
            events,
        })
    }

    /// Advance horizontal physics and the jump latch exactly once. `grounded`
    /// must describe the state BEFORE this tick's jump/vertical step: NetQuake
    /// applies ground friction/acceleration before PlayerPreThink jumps.
    /// `at_dropoff` is an authoritative ground probe (16 Quake units ahead,
    /// 34 units down); flat-room callers use false. It doubles ground friction.
    pub fn step_quake(
        &mut self,
        command: MovementCommand,
        yaw: f32,
        grounded: bool,
        at_dropoff: bool,
    ) -> Result<MovementStep, MovementError> {
        if ![command.forward, command.strafe, yaw]
            .iter()
            .all(|v| v.is_finite())
        {
            return Err(MovementError::InvalidInput);
        }
        let tick = self
            .state
            .tick
            .checked_add(1)
            .ok_or(MovementError::TickOverflow)?;
        let mut next = self.state;
        next.mode = MovementMode::Quake1;
        let (sin, cos) = yaw.sin_cos();
        // Original client command scales are intentionally asymmetric. The
        // server caps the combined vector, not each axis independently.
        let run = if command.running { 2.0 } else { 1.0 };
        let forward = command.forward.clamp(-1.0, 1.0) * 200.0 / QUAKE_UNITS_PER_TILE * run;
        let strafe = command.strafe.clamp(-1.0, 1.0) * 350.0 / QUAKE_UNITS_PER_TILE * run;
        let wish = [cos * forward + sin * strafe, sin * forward - cos * strafe];
        let length = (wish[0] * wish[0] + wish[1] * wish[1]).sqrt();
        let speed = length.min(QUAKE_MAX_SPEED);
        if grounded {
            let old_speed =
                (next.velocity[0] * next.velocity[0] + next.velocity[1] * next.velocity[1]).sqrt();
            if old_speed > 0.0 {
                let friction = FRICTION * if at_dropoff { 2.0 } else { 1.0 };
                let new_speed =
                    (old_speed - TICK_SECONDS * old_speed.max(STOP_SPEED) * friction).max(0.0);
                next.velocity = next.velocity.map(|v| v * (new_speed / old_speed));
            }
        }
        if length > 0.0 {
            let direction = wish.map(|v| v / length);
            let current = next.velocity[0] * direction[0] + next.velocity[1] * direction[1];
            let target = if grounded {
                speed
            } else {
                speed.min(AIR_DIRECTION_CAP)
            };
            // Air projection has a 30-unit cap, but acceleration uses the
            // full requested speed. Capping this multiplier changes air control.
            let acceleration = if grounded {
                ACCELERATION * TICK_SECONDS * speed
            } else {
                ACCELERATION * speed * TICK_SECONDS
            };
            let addition = (target - current).max(0.0).min(acceleration);
            for (velocity, component) in next.velocity.iter_mut().zip(direction) {
                *velocity += component * addition;
            }
        }
        next.velocity = next
            .velocity
            .map(|v| v.clamp(-QUAKE_MAX_VELOCITY, QUAKE_MAX_VELOCITY));
        if !command.jump_held {
            next.jump_released = true;
        }
        let jump = grounded && command.jump_held && next.jump_released;
        if jump {
            next.jump_released = false;
        }
        next.tick = tick;
        self.state = next;
        Ok(MovementStep {
            displacement: next.velocity.map(|v| v * TICK_SECONDS),
            jump,
        })
    }
}

fn validate_velocity(velocity: [f32; 2]) -> Result<(), MovementError> {
    // Quake clamps each axis before collision. A tangent projection reduces
    // vector length but can increase one axis beyond that earlier bound.
    // Allow the diagonal's length plus floating-point projection roundoff.
    let stored_bound = QUAKE_MAX_VELOCITY * std::f32::consts::SQRT_2 + 0.001;
    if velocity
        .iter()
        .any(|v| !v.is_finite() || v.abs() > stored_bound)
    {
        Err(MovementError::InvalidVelocity)
    } else {
        Ok(())
    }
}

/// Normal mode has no terminal-speed clamp. Retain a finite storage bound
/// while preserving the session's existing body and eye dimensions.
pub fn original_vertical_config(base: VerticalConfig) -> VerticalConfig {
    VerticalConfig {
        gravity: ORIGINAL_GRAVITY,
        jump_speed: ORIGINAL_JUMP_SPEED,
        max_speed: f32::MAX,
        ..base
    }
}

fn validate_vertical(
    current: VerticalSnapshot,
    bounds: VerticalBounds,
    body_height: f32,
) -> Result<(), VerticalError> {
    if !body_height.is_finite() || body_height <= 0.0 {
        return Err(VerticalError::InvalidConfiguration);
    }
    if !bounds.floor.is_finite()
        || bounds.ceiling.is_some_and(|z| !z.is_finite())
        || !current.feet.is_finite()
        || !current.velocity.is_finite()
    {
        return Err(VerticalError::InvalidBounds);
    }
    if bounds
        .ceiling
        .is_some_and(|z| z - bounds.floor < body_height)
    {
        return Err(VerticalError::InsufficientClearance);
    }
    Ok(())
}

fn correct_vertical_overlap(
    vertical: &mut VerticalSnapshot,
    bounds: VerticalBounds,
    body_height: f32,
    events: &mut VerticalEvents,
) {
    if vertical.feet < bounds.floor {
        vertical.feet = bounds.floor;
        vertical.velocity = vertical.velocity.max(0.0);
    }
    if let Some(ceiling) = bounds.ceiling {
        if vertical.feet + body_height > ceiling {
            vertical.feet = ceiling - body_height;
            vertical.velocity = vertical.velocity.min(0.0);
            events.hit_ceiling = true;
        }
    }
}

/// Preserve Chasm's body/eye dimensions while selecting Quake force constants.
/// The original velocity bound is applied BEFORE gravity, so the downward
/// post-tick value can exceed 2000 by one tick's gravity increment.
pub fn quake_vertical_config(base: VerticalConfig) -> VerticalConfig {
    VerticalConfig {
        gravity: QUAKE_GRAVITY,
        jump_speed: QUAKE_JUMP_SPEED,
        max_speed: QUAKE_MAX_VELOCITY + QUAKE_GRAVITY * TICK_SECONDS,
        ..base
    }
}

/// Fixed-tick gravity-before-displacement, resolving the session's supplied
/// floor and ceiling planes. This is distinct from the legacy trapezoidal
/// integrator. The caller commits the returned VerticalSnapshot atomically.
pub fn quake_vertical_step(
    current: VerticalSnapshot,
    bounds: VerticalBounds,
    body_height: f32,
    jump: bool,
) -> Result<(VerticalSnapshot, VerticalEvents), VerticalError> {
    validate_vertical(current, bounds, body_height)?;
    let mut next = current;
    let mut events = VerticalEvents::default();
    if next.feet < bounds.floor {
        next.feet = bounds.floor;
        next.velocity = next.velocity.max(0.0);
    }
    if let Some(ceiling) = bounds.ceiling {
        if next.feet + body_height > ceiling {
            next.feet = ceiling - body_height;
            next.velocity = next.velocity.min(0.0);
            events.hit_ceiling = true;
        }
    }
    next.grounded = next.feet <= bounds.floor + GROUND_EPSILON && next.velocity <= 0.0;
    if jump && next.grounded {
        next.velocity += QUAKE_JUMP_SPEED;
        next.grounded = false;
        events.jumped = true;
    }
    next.velocity =
        next.velocity.clamp(-QUAKE_MAX_VELOCITY, QUAKE_MAX_VELOCITY) - QUAKE_GRAVITY * TICK_SECONDS;
    next.feet += next.velocity * TICK_SECONDS;
    if let Some(ceiling) = bounds.ceiling {
        if next.feet + body_height > ceiling {
            next.feet = ceiling - body_height;
            next.velocity = next.velocity.min(0.0);
            events.hit_ceiling = true;
        }
    }
    if next.feet <= bounds.floor {
        events.landed = !next.grounded;
        next.feet = bounds.floor;
        next.velocity = 0.0;
        next.grounded = true;
    } else {
        next.grounded = false;
    }
    Ok((next, events))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vertical::VerticalState;

    fn running() -> MovementCommand {
        MovementCommand {
            forward: 1.0,
            running: true,
            ..MovementCommand::default()
        }
    }
    fn close(a: f32, b: f32) {
        assert!((a - b).abs() < 1e-6, "{a} != {b}");
    }

    #[test]
    fn ground_acceleration_client_scales_and_combined_speed_cap() {
        let mut state = MovementState::default();
        let first = state.step_quake(running(), 0.0, true, false).unwrap();
        close(state.velocity()[0], 50.0 / 60.0);
        close(first.displacement[0], 50.0 / 3600.0);
        for _ in 0..60 {
            state.step_quake(running(), 0.0, true, false).unwrap();
        }
        close(state.velocity()[0], 5.0);
        let mut walking = MovementState::default();
        let walk = MovementCommand {
            running: false,
            ..running()
        };
        for _ in 0..60 {
            walking.step_quake(walk, 0.0, true, false).unwrap();
        }
        close(walking.velocity()[0], 200.0 / 64.0);
        let mut diagonal = MovementState::default();
        let diagonal_input = MovementCommand {
            strafe: 1.0,
            ..running()
        };
        for _ in 0..60 {
            diagonal
                .step_quake(diagonal_input, 0.0, true, false)
                .unwrap();
        }
        let [x, y] = diagonal.velocity();
        close((x * x + y * y).sqrt(), 5.0);
        close(-y / x, 350.0 / 200.0);
    }

    #[test]
    fn friction_stop_speed_edge_probe_and_air_momentum() {
        let mut state = MovementState::default();
        state.set_collision_velocity([5.0, 0.0]).unwrap();
        state
            .step_quake(MovementCommand::default(), 0.0, true, false)
            .unwrap();
        close(state.velocity()[0], 5.0 - 20.0 / 60.0);
        state.set_collision_velocity([5.0, 0.0]).unwrap();
        state
            .step_quake(MovementCommand::default(), 0.0, true, true)
            .unwrap();
        close(state.velocity()[0], 5.0 - 40.0 / 60.0);
        state.set_collision_velocity([1.0, 0.0]).unwrap();
        state
            .step_quake(MovementCommand::default(), 0.0, true, false)
            .unwrap();
        close(state.velocity()[0], 1.0 - STOP_SPEED * FRICTION / 60.0);
        for _ in 0..20 {
            state
                .step_quake(MovementCommand::default(), 0.0, true, false)
                .unwrap();
        }
        assert_eq!(state.velocity(), [0.0; 2]);
        state.set_collision_velocity([5.0, -2.0]).unwrap();
        for _ in 0..20 {
            state
                .step_quake(MovementCommand::default(), 0.0, false, false)
                .unwrap();
        }
        assert_eq!(state.velocity(), [5.0, -2.0]);
    }

    #[test]
    fn air_limit_caps_projection_not_total_speed_or_acceleration_scale() {
        let mut state = MovementState::default();
        state.step_quake(running(), 0.0, false, false).unwrap();
        // Full-speed acceleration reaches 30/64 in one tick. Incorrectly
        // using capped speed for the multiplier would give only 5/64.
        close(state.velocity()[0], 30.0 / 64.0);
        for _ in 0..20 {
            state.step_quake(running(), 0.0, false, false).unwrap();
        }
        close(state.velocity()[0], 30.0 / 64.0);
        state.set_collision_velocity([5.0, 0.0]).unwrap();
        state.step_quake(running(), 0.0, false, false).unwrap();
        assert_eq!(state.velocity(), [5.0, 0.0]);
        state
            .step_quake(
                MovementCommand {
                    forward: 0.0,
                    strafe: 1.0,
                    ..running()
                },
                0.0,
                false,
                false,
            )
            .unwrap();
        assert_eq!(state.velocity(), [5.0, -30.0 / 64.0]);
        assert!((state.velocity()[0].powi(2) + state.velocity()[1].powi(2)).sqrt() > 5.0);
    }

    #[test]
    fn clipped_velocity_can_exceed_precontact_component_cap() {
        let velocity = [QUAKE_MAX_VELOCITY; 2];
        let angle = std::f32::consts::PI / 8.0;
        let normal = [-angle.cos(), angle.sin()];
        let dot = velocity[0] * normal[0] + velocity[1] * normal[1];
        let clipped = [velocity[0] - normal[0] * dot, velocity[1] - normal[1] * dot];
        assert!(clipped[1] > QUAKE_MAX_VELOCITY);
        let mut state = MovementState::default();
        state.commit_collision_velocity(clipped).unwrap();
        let saved = state.snapshot();
        state.restore(saved).unwrap();
        state
            .step_quake(MovementCommand::default(), 0.0, false, false)
            .unwrap();
        assert_eq!(state.velocity()[1], QUAKE_MAX_VELOCITY);
    }

    #[test]
    fn held_jump_requires_release_but_airborne_press_stays_armed() {
        let mut state = MovementState::default();
        let held = MovementCommand {
            jump_held: true,
            ..running()
        };
        assert!(state.step_quake(held, 0.0, true, false).unwrap().jump);
        // Ground acceleration applies even on the takeoff tick.
        close(state.velocity()[0], 50.0 / 60.0);
        for grounded in [false, false, true, true] {
            assert!(!state.step_quake(held, 0.0, grounded, false).unwrap().jump);
        }
        state.step_quake(running(), 0.0, false, false).unwrap();
        assert!(!state.step_quake(held, 0.0, false, false).unwrap().jump);
        assert!(state.step_quake(held, 0.0, true, false).unwrap().jump);
        assert!(!state.step_quake(held, 0.0, true, false).unwrap().jump);
    }

    #[test]
    fn vertical_jump_uses_gravity_before_motion_and_clips_planes() {
        let config = quake_vertical_config(VerticalConfig::default());
        let mut vertical = VerticalState::with_config(0.0, config).unwrap();
        let bounds = VerticalBounds {
            floor: 0.0,
            ceiling: None,
        };
        let (first, event) =
            quake_vertical_step(vertical.snapshot(), bounds, config.body_height, true).unwrap();
        assert!(event.jumped && !first.grounded);
        close(first.velocity, (270.0 - 800.0 / 60.0) / 64.0);
        close(first.feet, first.velocity / 60.0);
        vertical.restore(first, bounds).unwrap();
        let mut apex = first.feet;
        let mut landed = false;
        for _ in 0..120 {
            let (next, event) =
                quake_vertical_step(vertical.snapshot(), bounds, config.body_height, false)
                    .unwrap();
            apex = apex.max(next.feet);
            landed |= event.landed;
            vertical.restore(next, bounds).unwrap();
        }
        assert!(landed && vertical.grounded());
        assert!((0.65..0.72).contains(&apex), "{apex}");
        let low = VerticalBounds {
            floor: 0.0,
            ceiling: Some(config.body_height + 0.01),
        };
        let (blocked, events) =
            quake_vertical_step(vertical.snapshot(), low, config.body_height, true).unwrap();
        assert!(events.jumped && events.hit_ceiling);
        close(blocked.velocity, 0.0);
        close(blocked.feet, 0.01);
        let falling = VerticalSnapshot {
            feet: 100.0,
            velocity: -QUAKE_MAX_VELOCITY,
            grounded: false,
        };
        let (terminal, _) =
            quake_vertical_step(falling, bounds, config.body_height, false).unwrap();
        close(terminal.velocity, -config.max_speed);
        vertical.restore(terminal, bounds).unwrap();
    }

    #[test]
    fn snapshots_resume_exactly_and_reject_invalid_state_atomically() {
        let mut state = MovementState::default();
        for i in 0..45 {
            state
                .step_quake(
                    MovementCommand {
                        jump_held: true,
                        ..running()
                    },
                    i as f32 * 0.01,
                    i == 0,
                    false,
                )
                .unwrap();
        }
        let json = serde_json::to_string(&state.snapshot()).unwrap();
        let mut restored = MovementState::default();
        restored
            .restore(serde_json::from_str(&json).unwrap())
            .unwrap();
        for i in 0..100 {
            let command = MovementCommand {
                jump_held: i % 40 != 0,
                ..running()
            };
            assert_eq!(
                state.step_quake(command, 0.37, i % 30 == 0, false),
                restored.step_quake(command, 0.37, i % 30 == 0, false)
            );
            assert_eq!(state.snapshot(), restored.snapshot());
        }
        let before = state.snapshot();
        let bad = MovementSnapshot {
            velocity: [f32::NAN, 0.0],
            ..before
        };
        assert_eq!(state.restore(bad), Err(MovementError::InvalidVelocity));
        assert_eq!(state.snapshot(), before);
        assert_eq!(
            state.step_quake(running(), f32::INFINITY, true, false),
            Err(MovementError::InvalidInput)
        );
        assert_eq!(state.snapshot(), before);
        state
            .restore(MovementSnapshot {
                tick: u64::MAX,
                ..before
            })
            .unwrap();
        assert_eq!(
            state.step_quake(running(), 0.0, true, false),
            Err(MovementError::TickOverflow)
        );
        assert_eq!(state.snapshot().tick, u64::MAX);
    }

    fn floor_state() -> VerticalSnapshot {
        VerticalSnapshot {
            feet: 0.0,
            velocity: 0.0,
            grounded: true,
        }
    }
    fn open_bounds() -> VerticalBounds {
        VerticalBounds {
            floor: 0.0,
            ceiling: None,
        }
    }

    #[test]
    fn original_fixed_microsteps_carry_time_and_accelerate_before_displacement() {
        let mut state = MovementState::default();
        for (expected_calls, expected_remainder) in [(33, 166), (33, 333), (34, 0)] {
            let mut calls = 0;
            let result = state
                .step_original_with_contacts(
                    running(),
                    0.0,
                    floor_state(),
                    open_bounds(),
                    0.9,
                    |_, velocity, vertical| {
                        calls += 1;
                        OriginalContact {
                            velocity,
                            vertical,
                            bounds: open_bounds(),
                        }
                    },
                )
                .unwrap();
            assert_eq!(calls, expected_calls);
            assert_eq!(state.snapshot().original_remainder_us, expected_remainder);
            if state.snapshot().tick == 1 {
                close(
                    result.displacement[0],
                    52.5 * 0.0005 * 0.0005 * (33.0 * 34.0 / 2.0),
                );
                close(state.velocity()[0], 52.5 * 0.0165);
            }
        }
        assert!((state.velocity()[0] - 52.5 * 0.05).abs() < 3e-6);
    }

    #[test]
    fn original_target_speed_is_normalized_and_air_control_matches_ground() {
        let mut ground = MovementState::default();
        let mut air = ground;
        let command = MovementCommand {
            strafe: 1.0,
            ..running()
        };
        for _ in 0..20 {
            ground
                .step_original(command, 0.0, floor_state(), open_bounds(), 0.9)
                .unwrap();
            air.step_original(
                command,
                0.0,
                VerticalSnapshot {
                    feet: 2.0,
                    velocity: 0.0,
                    grounded: false,
                },
                open_bounds(),
                0.9,
            )
            .unwrap();
            assert_eq!(ground.velocity(), air.velocity());
        }
        let [x, y] = ground.velocity();
        close((x * x + y * y).sqrt(), ORIGINAL_RUN_SPEED);
        close(x, -y);
        let mut walking = MovementState::default();
        for _ in 0..20 {
            walking
                .step_original(
                    MovementCommand {
                        running: false,
                        ..running()
                    },
                    0.0,
                    floor_state(),
                    open_bounds(),
                    0.9,
                )
                .unwrap();
        }
        close(walking.velocity()[0], 3.1185);
        walking.set_collision_velocity([5.0, 0.0]).unwrap();
        let previous = walking.velocity()[0];
        walking
            .step_original(
                MovementCommand::default(),
                0.0,
                floor_state(),
                open_bounds(),
                0.9,
            )
            .unwrap();
        assert!((previous - walking.velocity()[0] - 52.5 * 0.017).abs() < 1e-5);
    }

    #[test]
    fn original_direction_quantization_and_zero_speed_setting_match_command_rules() {
        let mut a = MovementState::default();
        let mut b = a;
        a.step_original(running(), 0.001, floor_state(), open_bounds(), 0.9)
            .unwrap();
        b.step_original(running(), 0.001001, floor_state(), open_bounds(), 0.9)
            .unwrap();
        assert_eq!(a.snapshot(), b.snapshot());
        let mut next_bin = MovementState::default();
        next_bin
            .step_original(running(), 0.0011, floor_state(), open_bounds(), 0.9)
            .unwrap();
        assert_ne!(a.velocity(), next_bin.velocity());
        let mut zero = MovementState::default();
        zero.set_collision_velocity([4.0, 1.0]).unwrap();
        let result = zero
            .step_original_configured(
                running(),
                0.0,
                floor_state(),
                open_bounds(),
                0.9,
                OriginalConfig {
                    movement_speed: 0.0,
                    friction: 1.0,
                },
                |_, velocity, vertical| OriginalContact {
                    velocity,
                    vertical,
                    bounds: open_bounds(),
                },
            )
            .unwrap();
        assert_eq!(zero.velocity(), [4.0, 1.0]);
        close(result.displacement[0], 4.0 * 0.0165);
    }

    #[test]
    fn original_held_jump_repeats_with_clock_cooldown_and_handles_midtick_landing() {
        let mut state = MovementState::default();
        let held = MovementCommand {
            jump_held: true,
            ..MovementCommand::default()
        };
        let first = state
            .step_original(held, 0.0, floor_state(), open_bounds(), 0.9)
            .unwrap();
        assert!(first.events.jumped);
        assert_eq!(state.snapshot().jump_cooldown_us, 400_000);
        for _ in 0..23 {
            assert!(
                !state
                    .step_original(held, 0.0, floor_state(), open_bounds(), 0.9)
                    .unwrap()
                    .events
                    .jumped
            );
        }
        assert!(
            state
                .step_original(held, 0.0, floor_state(), open_bounds(), 0.9)
                .unwrap()
                .events
                .jumped
        );
        let mut landing = MovementState::default();
        let result = landing
            .step_original(
                held,
                0.0,
                VerticalSnapshot {
                    feet: 0.0001,
                    velocity: -1.0,
                    grounded: false,
                },
                open_bounds(),
                0.9,
            )
            .unwrap();
        assert!(result.events.landed && result.events.jumped);
        assert!(result.vertical.feet > 0.0 && result.vertical.velocity > 0.0);
        let mut low = MovementState::default();
        let low_jump = low
            .step_original(
                held,
                0.0,
                floor_state(),
                VerticalBounds {
                    floor: 0.0,
                    ceiling: Some(1.2),
                },
                0.9,
            )
            .unwrap();
        assert!((first.vertical.velocity - low_jump.vertical.velocity - 1.75).abs() < 5e-6);
    }

    #[test]
    fn original_contacts_clip_each_substep_and_propagate_support_changes() {
        let mut state = MovementState::default();
        let mut position = 0.0f32;
        let mut collisions = 0;
        state
            .step_original_with_contacts(
                running(),
                0.0,
                floor_state(),
                open_bounds(),
                0.9,
                |delta, mut velocity, vertical| {
                    position += delta[0];
                    if position >= 0.002 {
                        position = 0.002;
                        velocity[0] = 0.0;
                        collisions += 1;
                    }
                    OriginalContact {
                        velocity,
                        vertical,
                        bounds: open_bounds(),
                    }
                },
            )
            .unwrap();
        close(position, 0.002);
        assert!(collisions > 1);
        assert_eq!(state.velocity(), [0.0; 2]);
        let mut drop = MovementState::default();
        let result = drop
            .step_original_with_contacts(
                MovementCommand::default(),
                0.0,
                floor_state(),
                open_bounds(),
                0.9,
                |_, velocity, vertical| OriginalContact {
                    velocity,
                    vertical,
                    bounds: VerticalBounds {
                        floor: -1.0,
                        ceiling: None,
                    },
                },
            )
            .unwrap();
        assert!(
            !result.vertical.grounded
                && result.vertical.feet < 0.0
                && result.vertical.velocity < 0.0
        );
        let before = drop.snapshot();
        let error = drop.step_original_with_contacts(
            running(),
            0.0,
            floor_state(),
            open_bounds(),
            0.9,
            |_, _, vertical| OriginalContact {
                velocity: [f32::NAN, 0.0],
                vertical,
                bounds: open_bounds(),
            },
        );
        assert_eq!(error, Err(MovementError::InvalidVelocity));
        assert_eq!(drop.snapshot(), before);
    }

    #[test]
    fn original_snapshot_preserves_substep_phase_cooldown_and_replay() {
        let held = MovementCommand {
            jump_held: true,
            ..running()
        };
        let mut state = MovementState::default();
        let first = state
            .step_original(held, 0.4, floor_state(), open_bounds(), 0.9)
            .unwrap();
        let json = serde_json::to_string(&state.snapshot()).unwrap();
        let mut restored = MovementState::default();
        restored
            .restore(serde_json::from_str(&json).unwrap())
            .unwrap();
        let mut vertical = first.vertical;
        for _ in 0..100 {
            let next = state
                .step_original(held, 0.4, vertical, open_bounds(), 0.9)
                .unwrap();
            let repeated = restored
                .step_original(held, 0.4, vertical, open_bounds(), 0.9)
                .unwrap();
            assert_eq!(next, repeated);
            assert_eq!(state.snapshot(), restored.snapshot());
            vertical = next.vertical;
        }
        let before = restored.snapshot();
        assert_eq!(
            restored.restore(MovementSnapshot {
                original_remainder_us: 500,
                ..before
            }),
            Err(MovementError::InvalidClock)
        );
        assert_eq!(restored.snapshot(), before);
        restored.set_mode(MovementMode::Quake1);
        assert_eq!(restored.velocity(), [0.0; 2]);
        assert_eq!(restored.snapshot().tick, before.tick);
        assert_eq!(restored.snapshot().jump_cooldown_us, 0);
        assert_eq!(restored.snapshot().original_remainder_us, 0);
    }
}
