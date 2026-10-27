//! Presentation timings measured against wall time, independent of game ticks.
use std::time::{Duration, Instant};

const SAMPLE_PERIOD: Duration = Duration::from_millis(500);

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
