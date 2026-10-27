//! Presentation timings measured against wall time, independent of game ticks.
use std::time::{Duration, Instant};

const SAMPLE_PERIOD: Duration = Duration::from_millis(500);

/// Independent rendering deadline; the host continues simulation and input
/// while waiting. A zero period is treated as uncapped.
pub struct FramePacer {
    limit: Option<Duration>,
    deadline: Instant,
}
impl FramePacer {
    pub fn new(limit: Option<Duration>, now: Instant) -> Self {
        Self {
            limit: limit.filter(|duration| !duration.is_zero()),
            deadline: now,
        }
    }
    pub fn limit(&self) -> Option<Duration> {
        self.limit
    }
    /// A changed cap takes effect immediately, including a first render now.
    pub fn set_limit(&mut self, limit: Option<Duration>, now: Instant) {
        self.limit = limit.filter(|duration| !duration.is_zero());
        self.deadline = now;
    }
    pub fn should_render(&self, now: Instant) -> bool {
        self.limit.is_none() || now >= self.deadline
    }
    /// Pace from the actual start of this frame, not the old deadline: after a
    /// slow frame, render immediately rather than scheduling a burst of debt.
    pub fn presented(&mut self, frame_started: Instant) {
        self.deadline = self
            .limit
            .and_then(|period| frame_started.checked_add(period))
            .unwrap_or(frame_started);
    }
    /// The host may sleep at most 2 ms before checking input and ticking again.
    pub fn wait_hint(&self, now: Instant) -> Duration {
        if self.limit.is_none() {
            Duration::ZERO
        } else {
            self.deadline
                .saturating_duration_since(now)
                .min(Duration::from_millis(2))
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Metrics {
    pub fps: f64,
    pub frame_ms: f64,
    pub simulation_ms: f64,
    pub render_ms: f64,
    pub present_ms: f64,
    pub max_frame_ms: f64,
}

pub struct FrameStats {
    started: Instant,
    previous: Instant,
    frames: u32,
    simulation: Duration,
    render: Duration,
    present: Duration,
    longest: Duration,
    pub metrics: Metrics,
}
impl FrameStats {
    pub fn new(now: Instant) -> Self {
        Self {
            started: now,
            previous: now,
            frames: 0,
            simulation: Duration::ZERO,
            render: Duration::ZERO,
            present: Duration::ZERO,
            longest: Duration::ZERO,
            metrics: Metrics::default(),
        }
    }

    /// Record one presented frame. FPS includes frame limiting and scheduling;
    /// stage timings include only the measured work, not the limiter's sleep.
    pub fn record(
        &mut self,
        now: Instant,
        simulation: Duration,
        render: Duration,
        present: Duration,
    ) -> Option<Metrics> {
        self.longest = self
            .longest
            .max(now.saturating_duration_since(self.previous));
        self.previous = now;
        self.frames = self.frames.saturating_add(1);
        self.simulation += simulation;
        self.render += render;
        self.present += present;
        let elapsed = now.saturating_duration_since(self.started);
        if elapsed < SAMPLE_PERIOD {
            return None;
        }
        let frames = f64::from(self.frames);
        self.metrics = Metrics {
            fps: frames / elapsed.as_secs_f64(),
            frame_ms: elapsed.as_secs_f64() * 1000.0 / frames,
            simulation_ms: self.simulation.as_secs_f64() * 1000.0 / frames,
            render_ms: self.render.as_secs_f64() * 1000.0 / frames,
            present_ms: self.present.as_secs_f64() * 1000.0 / frames,
            max_frame_ms: self.longest.as_secs_f64() * 1000.0,
        };
        self.started = now;
        self.frames = 0;
        self.simulation = Duration::ZERO;
        self.render = Duration::ZERO;
        self.present = Duration::ZERO;
        self.longest = Duration::ZERO;
        Some(self.metrics)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn one_fps_render_cap_does_not_block_sixty_simulation_ticks() {
        let start = Instant::now();
        let mut pacer = FramePacer::new(Some(Duration::from_secs(1)), start);
        let mut frames = 0;
        let mut ticks = 0;
        for tick in 0..=60 {
            let now = start + Duration::from_nanos((tick * 1_000_000_000) / 60);
            if pacer.should_render(now) {
                frames += 1;
                pacer.presented(now);
            }
            if tick < 60 {
                ticks += 1;
            }
            assert!(pacer.wait_hint(now) <= Duration::from_millis(2));
        }
        assert_eq!(ticks, 60);
        assert_eq!(frames, 2); // initial frame and the frame at one second
    }
    #[test]
    fn cap_changes_are_immediate_and_slow_frames_do_not_accumulate_debt() {
        let start = Instant::now();
        let mut p = FramePacer::new(Some(Duration::from_millis(100)), start);
        assert!(p.should_render(start));
        p.presented(start);
        assert!(!p.should_render(start + Duration::from_millis(99)));
        assert_eq!(
            p.wait_hint(start + Duration::from_millis(99)),
            Duration::from_millis(1)
        );
        assert!(p.should_render(start + Duration::from_millis(150))); // slow render
        p.presented(start + Duration::from_millis(150));
        assert!(!p.should_render(start + Duration::from_millis(151)));
        p.set_limit(None, start + Duration::from_millis(151));
        assert_eq!(p.limit(), None);
        assert!(p.should_render(start + Duration::from_millis(151)));
        assert_eq!(p.wait_hint(start), Duration::ZERO);
        p.set_limit(
            Some(Duration::from_millis(10)),
            start + Duration::from_millis(160),
        );
        assert!(p.should_render(start + Duration::from_millis(160)));
        p.presented(start + Duration::from_millis(160));
        assert!(!p.should_render(start + Duration::from_millis(169)));
        assert!(p.should_render(start + Duration::from_millis(170)));
        p.set_limit(Some(Duration::ZERO), start);
        assert_eq!(p.limit(), None);
        assert!(p.should_render(start));
    }

    #[test]
    fn measures_wall_time_including_waits_separately_from_work() {
        let start = Instant::now();
        let mut stats = FrameStats::new(start);
        for frame in 1..=60 {
            stats.record(
                start + Duration::from_nanos(frame * 16_666_667),
                Duration::from_micros(500),
                Duration::from_millis(4),
                Duration::from_millis(1),
            );
        }
        assert!((stats.metrics.fps - 60.0).abs() < 0.001);
        assert!((stats.metrics.frame_ms - 1000.0 / 60.0).abs() < 0.001);
        assert_eq!(stats.metrics.simulation_ms, 0.5);
        assert_eq!(stats.metrics.render_ms, 4.0);
        assert_eq!(stats.metrics.present_ms, 1.0);
    }
    #[test]
    fn reports_hitch_and_recovers_without_reusing_previous_window() {
        let start = Instant::now();
        let mut stats = FrameStats::new(start);
        let hitched = stats
            .record(
                start + Duration::from_secs(1),
                Duration::ZERO,
                Duration::ZERO,
                Duration::ZERO,
            )
            .unwrap();
        assert_eq!(hitched.fps, 1.0);
        assert_eq!(hitched.max_frame_ms, 1000.0);
        let normal = stats
            .record(
                start + Duration::from_millis(1500),
                Duration::ZERO,
                Duration::ZERO,
                Duration::ZERO,
            )
            .unwrap();
        assert_eq!(normal.fps, 2.0);
        assert_eq!(normal.max_frame_ms, 500.0);
    }
}
