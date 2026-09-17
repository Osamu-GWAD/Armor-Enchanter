//! Turn repeated Azalea scheduler warnings into a deferred reconnect request.
//! One account runs per process, so the cooldown survives its reconnects.
use std::collections::VecDeque;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::{Context, Layer};

const WARNING: &str = "GameTick is more than 10 ticks behind";
const WINDOW: Duration = Duration::from_secs(10);
const COOLDOWN: Duration = Duration::from_secs(60);
const WARNING_LIMIT: usize = 3;

pub static LAG_RECOVERY: LazyLock<LagRecovery> = LazyLock::new(LagRecovery::default);

#[derive(Default)]
struct Policy {
    connected: bool,
    warnings: VecDeque<Instant>,
    pending: bool,
    last_reconnect: Option<Instant>,
}

impl Policy {
    fn set_connected(&mut self, connected: bool) {
        self.connected = connected;
        self.pending = false;
        self.warnings.clear();
        // Keep last_reconnect across disconnect/spawn events.
    }

    fn record(&mut self, now: Instant) {
        if !self.connected || self.pending {
            return;
        }
        if self
            .last_reconnect
            .is_some_and(|last| now.duration_since(last) < COOLDOWN)
        {
            return;
        }
        while self
            .warnings
            .front()
            .is_some_and(|first| now.duration_since(*first) > WINDOW)
        {
            self.warnings.pop_front();
        }
        self.warnings.push_back(now);
        if self.warnings.len() >= WARNING_LIMIT {
            self.pending = true;
            self.warnings.clear();
        }
    }

    fn take_reconnect(&mut self, now: Instant) -> bool {
        if !self.connected || !self.pending {
            return false;
        }
        self.set_connected(false);
        self.last_reconnect = Some(now);
        true
    }
}

#[derive(Clone, Default)]
pub struct LagRecovery {
    policy: Arc<Mutex<Policy>>,
}

impl LagRecovery {
    pub fn set_connected(&self, connected: bool) {
        self.policy.lock().unwrap().set_connected(connected);
    }

    pub fn take_reconnect(&self) -> bool {
        self.policy.lock().unwrap().take_reconnect(Instant::now())
    }
}

#[derive(Default)]
struct WarningVisitor(bool);

impl Visit for WarningVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" && value.starts_with(WARNING) {
            self.0 = true;
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" && format!("{value:?}").contains(WARNING) {
            self.0 = true;
        }
    }
}

impl<S: Subscriber> Layer<S> for LagRecovery {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let metadata = event.metadata();
        if metadata.target() != "azalea_client::client" || *metadata.level() != Level::WARN {
            return;
        }
        let mut visitor = WarningVisitor::default();
        event.record(&mut visitor);
        if visitor.0 {
            // Only touch our short-lived policy lock here, never Azalea's ECS.
            self.policy.lock().unwrap().record(Instant::now());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing_subscriber::prelude::*;

    #[test]
    fn three_recent_warnings_request_exactly_one_reconnect() {
        let start = Instant::now();
        let mut policy = Policy::default();
        policy.set_connected(true);
        for seconds in [0, 2] {
            policy.record(start + Duration::from_secs(seconds));
            assert!(!policy.take_reconnect(start + Duration::from_secs(seconds)));
        }
        policy.record(start + Duration::from_secs(3));
        assert!(policy.take_reconnect(start + Duration::from_secs(3)));
        assert!(!policy.take_reconnect(start + Duration::from_secs(3)));
    }

    #[test]
    fn isolated_warnings_and_disconnected_warnings_do_not_trigger() {
        let start = Instant::now();
        let mut policy = Policy::default();
        for _ in 0..3 {
            policy.record(start);
        }
        policy.set_connected(true);
        assert!(!policy.take_reconnect(start));
        for seconds in [0, 11, 22] {
            policy.record(start + Duration::from_secs(seconds));
        }
        assert!(!policy.take_reconnect(start + Duration::from_secs(22)));
    }

    #[test]
    fn cooldown_survives_rejoin_and_requires_fresh_warnings() {
        let start = Instant::now();
        let mut policy = Policy::default();
        policy.set_connected(true);
        for _ in 0..3 {
            policy.record(start);
        }
        assert!(policy.take_reconnect(start));
        policy.set_connected(false);
        policy.set_connected(true);
        for _ in 0..10 {
            policy.record(start + Duration::from_secs(59));
        }
        assert!(!policy.take_reconnect(start + COOLDOWN));
        for _ in 0..2 {
            policy.record(start + COOLDOWN);
        }
        assert!(!policy.take_reconnect(start + COOLDOWN));
        policy.record(start + COOLDOWN);
        assert!(policy.take_reconnect(start + COOLDOWN));
    }

    #[test]
    fn disconnect_clears_pending_request_and_warning_history() {
        let start = Instant::now();
        let mut policy = Policy::default();
        policy.set_connected(true);
        for _ in 0..3 {
            policy.record(start);
        }
        policy.set_connected(false);
        policy.set_connected(true);
        assert!(!policy.take_reconnect(start));
        policy.record(start);
        assert!(!policy.take_reconnect(start));
    }

    #[test]
    fn tracing_layer_only_matches_azalea_tick_lag_warnings() {
        let recovery = LagRecovery::default();
        recovery.set_connected(true);
        let subscriber = tracing_subscriber::registry().with(recovery.clone());
        tracing::subscriber::with_default(subscriber, || {
            for _ in 0..3 {
                tracing::warn!(target: "other_client", "{WARNING}");
                tracing::info!(target: "azalea_client::client", "{WARNING}");
                tracing::warn!(target: "azalea_client::client", "Unrelated warning");
            }
            assert!(!recovery.take_reconnect());
            for _ in 0..3 {
                tracing::warn!(target: "azalea_client::client", "GameTick is more than 10 ticks behind, skipping ticks so we don't have to burst too much");
            }
            assert!(recovery.take_reconnect());
            assert!(!recovery.take_reconnect());
        });
    }
}
