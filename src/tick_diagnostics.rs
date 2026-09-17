//! Measure executed ticks and Update work without changing Azalea's scheduler.
use std::time::{Duration, Instant};

use azalea::app::{App, First, Last, Plugin};
use azalea::ecs::{self as bevy_ecs, prelude::*};
use azalea::prelude::GameTick;

const REPORT_INTERVAL: Duration = Duration::from_secs(30);

pub struct TickDiagnosticsPlugin;

impl Plugin for TickDiagnosticsPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<TickTimings>()
            .add_systems(First, start_update)
            .add_systems(Last, finish_update)
            .add_systems(GameTick, record_tick);
    }
}

#[derive(Resource)]
struct TickTimings {
    window_start: Instant,
    update_start: Option<Instant>,
    previous_tick: Option<Instant>,
    ticks: u64,
    updates: u64,
    max_update: Duration,
    max_tick_gap: Duration,
}

impl Default for TickTimings {
    fn default() -> Self {
        Self::new(Instant::now())
    }
}

impl TickTimings {
    fn new(now: Instant) -> Self {
        Self {
            window_start: now,
            update_start: None,
            previous_tick: None,
            ticks: 0,
            updates: 0,
            max_update: Duration::ZERO,
            max_tick_gap: Duration::ZERO,
        }
    }

    fn tick(&mut self, now: Instant) {
        if let Some(previous) = self.previous_tick.replace(now) {
            self.max_tick_gap = self.max_tick_gap.max(now.duration_since(previous));
        }
        self.ticks += 1;
    }

    fn finish(&mut self, now: Instant) {
        if let Some(start) = self.update_start.take() {
            self.max_update = self.max_update.max(now.duration_since(start));
            self.updates += 1;
        }
    }

    fn report(&mut self, now: Instant) -> Option<TimingReport> {
        let elapsed = now.duration_since(self.window_start);
        if elapsed < REPORT_INTERVAL {
            return None;
        }
        let report = TimingReport {
            ticks: self.ticks,
            tps: self.ticks as f64 / elapsed.as_secs_f64(),
            updates: self.updates,
            max_update_ms: self.max_update.as_millis(),
            max_tick_gap_ms: self.max_tick_gap.as_millis(),
        };
        // Keep previous_tick so a stall crossing a report boundary is measured.
        self.window_start = now;
        self.ticks = 0;
        self.updates = 0;
        self.max_update = Duration::ZERO;
        self.max_tick_gap = Duration::ZERO;
        Some(report)
    }
}

#[derive(Debug)]
struct TimingReport {
    ticks: u64,
    tps: f64,
    updates: u64,
    max_update_ms: u128,
    max_tick_gap_ms: u128,
}

fn start_update(mut timings: ResMut<TickTimings>) {
    timings.update_start = Some(Instant::now());
}

fn record_tick(mut timings: ResMut<TickTimings>) {
    timings.tick(Instant::now());
}

fn finish_update(mut timings: ResMut<TickTimings>) {
    let now = Instant::now();
    timings.finish(now);
    if let Some(report) = timings.report(now) {
        // This is local executed TPS, not the server's TPS. First..Last measures
        // the outer Update sweep, excluding GameTick and time awaiting the ECS.
        tracing::info!(
            ticks = report.ticks,
            local_tps = format_args!("{:.1}", report.tps),
            updates = report.updates,
            max_update_ms = report.max_update_ms as u64,
            max_tick_gap_ms = report.max_tick_gap_ms as u64,
            "Local scheduler timing (last 30s)"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn measures_skipped_simulation_time_and_slow_updates() {
        let start = Instant::now();
        let mut timings = TickTimings::new(start);
        for tick in 0..600 {
            let now = start + Duration::from_millis(tick * 50);
            // Simulate a one-second stall: no tick callbacks during the gap.
            if (200..220).contains(&tick) { continue; }
            timings.tick(now);
        }
        timings.update_start = Some(start);
        timings.finish(start + Duration::from_millis(650));
        assert!(timings.report(start + Duration::from_secs(29)).is_none());
        let report = timings.report(start + REPORT_INTERVAL).unwrap();
        assert_eq!(report.ticks, 580);
        assert!((report.tps - 580.0 / 30.0).abs() < 0.001);
        assert_eq!(report.max_tick_gap_ms, 1050);
        assert_eq!(report.max_update_ms, 650);
        assert_eq!(report.updates, 1);
        // A cross-window pause must not disappear when counters reset.
        timings.tick(start + Duration::from_secs(31));
        assert_eq!(timings.max_tick_gap.as_millis(), 1050);
        assert_eq!(timings.ticks, 1);
        assert_eq!(timings.max_update, Duration::ZERO);
    }

    #[test]
    fn plugin_runs_once_per_schedule_without_network_or_sleep() {
        let mut app = App::new();
        app.add_plugins(TickDiagnosticsPlugin);
        app.update();
        app.world_mut().run_schedule(GameTick);
        app.world_mut().run_schedule(GameTick);
        let timings = app.world().resource::<TickTimings>();
        assert_eq!(timings.updates, 1);
        assert_eq!(timings.ticks, 2);
        assert!(timings.update_start.is_none());
    }
}
