//! Cage lifecycle policy for the Cygnus supervisor.
//!
//! The supervisor owns each app's `cold -> booting -> ready -> draining -> cold`
//! lifecycle (spec §5). This crate is the pure policy that drives it —
//! scale-to-zero, restart backoff, and crash-loop detection — kept separate
//! from the live cage management that wires these decisions to real boots. Every
//! method takes the current time explicitly, so the policy is deterministic and
//! testable without sleeping.
//!
//! Backoff and crash-loop detection share one signal: the number of crashes
//! inside a sliding window. As crashes age out of the window the effective
//! backoff shrinks and a looping app can recover on its own, so there is no
//! separate counter to reset.

mod runtime;

pub use cygnus_cage::InstanceStatus;
pub use runtime::{AcquireError, Instance, Supervisor};

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Idle time before a scale-to-zero app is reaped (spec §5 default: 10 minutes).
pub const DEFAULT_IDLE_TTL: Duration = Duration::from_secs(600);
/// First restart delay after a single crash.
pub const DEFAULT_BACKOFF_BASE: Duration = Duration::from_millis(200);
/// Ceiling on the restart delay, however many crashes have accrued.
pub const DEFAULT_BACKOFF_MAX: Duration = Duration::from_secs(30);
/// Sliding window over which crashes are counted.
pub const DEFAULT_CRASH_WINDOW: Duration = Duration::from_secs(60);
/// Crashes within the window that mark an app as crash-looping.
pub const DEFAULT_CRASH_LOOP_THRESHOLD: u32 = 5;

/// Where an app's cage is in its lifecycle.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LifecycleState {
    /// No process; the artifact rests on disk and the app revives on demand.
    Cold,
    /// Namespaces and exec are in flight.
    Booting,
    /// The cage is up and serving.
    Ready,
    /// Deploy or reap in progress; in-flight work is finishing.
    Draining,
    /// Crash-looping: parked and serving errors, not auto-restarting until an
    /// operator or a fresh deploy clears it.
    Failed,
}

/// Tunable lifecycle policy for one app.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LifecycleConfig {
    /// Idle time before a scale-to-zero app is reaped.
    pub idle_ttl: Duration,
    /// Warm instances to keep pinned: `0` scales to zero, `>= 1` stays warm.
    pub min_instances: u32,
    /// First restart delay after a crash.
    pub backoff_base: Duration,
    /// Ceiling on the restart delay.
    pub backoff_max: Duration,
    /// Sliding window for crash counting.
    pub crash_window: Duration,
    /// Crashes within the window that trip the crash-loop guard.
    pub crash_loop_threshold: u32,
}

impl Default for LifecycleConfig {
    fn default() -> Self {
        Self {
            idle_ttl: DEFAULT_IDLE_TTL,
            min_instances: 0,
            backoff_base: DEFAULT_BACKOFF_BASE,
            backoff_max: DEFAULT_BACKOFF_MAX,
            crash_window: DEFAULT_CRASH_WINDOW,
            crash_loop_threshold: DEFAULT_CRASH_LOOP_THRESHOLD,
        }
    }
}

/// What the supervisor should do after a cage crash.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CrashOutcome {
    /// Restart the cage after this backoff delay.
    Restart { after: Duration },
    /// Too many crashes in the window; the app is parked as [`LifecycleState::Failed`]
    /// and must not be auto-restarted.
    CrashLooping,
}

/// The lifecycle state and recent history of one app's cage.
#[derive(Clone, Debug)]
pub struct AppLifecycle {
    state: LifecycleState,
    config: LifecycleConfig,
    last_active: Instant,
    crashes: VecDeque<Instant>,
}

impl AppLifecycle {
    /// A newly registered app, cold and idle as of `now`.
    pub fn new(config: LifecycleConfig, now: Instant) -> Self {
        Self {
            state: LifecycleState::Cold,
            config,
            last_active: now,
            crashes: VecDeque::new(),
        }
    }

    /// Current lifecycle state.
    pub fn state(&self) -> LifecycleState {
        self.state
    }

    /// Whether this app is pinned warm and so exempt from idle reaping.
    pub fn is_pinned(&self) -> bool {
        self.config.min_instances >= 1
    }

    /// Record a request against the app and report whether it must be booted
    /// (i.e. it was cold). Also refreshes the idle clock.
    pub fn note_request(&mut self, now: Instant) -> bool {
        self.last_active = now;
        self.state == LifecycleState::Cold
    }

    /// Transition into `Booting`. Valid from `Cold`; also used to retry a
    /// `Failed` app once its failure has been cleared.
    pub fn begin_boot(&mut self, now: Instant) {
        self.state = LifecycleState::Booting;
        self.last_active = now;
    }

    /// The cage reached readiness.
    pub fn mark_ready(&mut self, now: Instant) {
        self.state = LifecycleState::Ready;
        self.last_active = now;
    }

    /// Begin draining a ready cage (deploy or idle reap).
    pub fn begin_drain(&mut self) {
        self.state = LifecycleState::Draining;
    }

    /// The cage is gone and its resources are released.
    pub fn mark_cold(&mut self) {
        self.state = LifecycleState::Cold;
    }

    /// Clear a crash-loop parking so the app is eligible to boot again.
    pub fn clear_failure(&mut self, now: Instant) {
        if self.state == LifecycleState::Failed {
            self.state = LifecycleState::Cold;
        }
        self.crashes.clear();
        self.last_active = now;
    }

    /// Whether a ready, scale-to-zero app has been idle long enough to reap.
    pub fn should_reap_idle(&self, now: Instant) -> bool {
        self.state == LifecycleState::Ready
            && !self.is_pinned()
            && now.duration_since(self.last_active) >= self.config.idle_ttl
    }

    /// Record a crash and decide whether to restart (and after how long) or to
    /// park the app as crash-looping.
    pub fn note_crash(&mut self, now: Instant) -> CrashOutcome {
        self.crashes.push_back(now);
        self.prune(now);
        let count = self.crashes.len() as u32;
        if count >= self.config.crash_loop_threshold {
            self.state = LifecycleState::Failed;
            CrashOutcome::CrashLooping
        } else {
            self.state = LifecycleState::Cold;
            CrashOutcome::Restart {
                after: self.backoff_delay(count),
            }
        }
    }

    /// Crashes still inside the window as of `now`.
    pub fn recent_crashes(&self, now: Instant) -> usize {
        self.crashes
            .iter()
            .filter(|&&at| now.duration_since(at) < self.config.crash_window)
            .count()
    }

    fn prune(&mut self, now: Instant) {
        while let Some(&front) = self.crashes.front() {
            if now.duration_since(front) >= self.config.crash_window {
                self.crashes.pop_front();
            } else {
                break;
            }
        }
    }

    /// Exponential backoff for the n-th recent crash: `base * 2^(n-1)`, capped.
    fn backoff_delay(&self, crash_count: u32) -> Duration {
        let exponent = crash_count.saturating_sub(1).min(31);
        let factor = 1_u32 << exponent;
        self.config
            .backoff_base
            .saturating_mul(factor)
            .min(self.config.backoff_max)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> LifecycleConfig {
        LifecycleConfig::default()
    }

    #[test]
    fn a_cold_request_signals_a_boot_is_needed() {
        let start = Instant::now();
        let mut app = AppLifecycle::new(config(), start);
        assert!(app.note_request(start), "a cold app needs a boot");
        app.begin_boot(start);
        app.mark_ready(start);
        assert!(!app.note_request(start), "a ready app does not need a boot");
    }

    #[test]
    fn scale_to_zero_respects_ttl_and_pinning() {
        let start = Instant::now();
        let mut app = AppLifecycle::new(config(), start);
        app.begin_boot(start);
        app.mark_ready(start);

        let within = start + DEFAULT_IDLE_TTL - Duration::from_secs(1);
        assert!(!app.should_reap_idle(within), "still within the idle TTL");

        let past = start + DEFAULT_IDLE_TTL + Duration::from_secs(1);
        assert!(app.should_reap_idle(past), "idle past the TTL should reap");

        // A pinned app is never idle-reaped.
        let mut pinned = AppLifecycle::new(
            LifecycleConfig {
                min_instances: 1,
                ..config()
            },
            start,
        );
        pinned.begin_boot(start);
        pinned.mark_ready(start);
        assert!(!pinned.should_reap_idle(past), "pinned apps stay warm");
    }

    #[test]
    fn only_ready_apps_are_reaped() {
        let start = Instant::now();
        let mut app = AppLifecycle::new(config(), start);
        let past = start + DEFAULT_IDLE_TTL + Duration::from_secs(1);
        // Cold, booting, and draining apps are not idle-reap candidates.
        assert!(!app.should_reap_idle(past));
        app.begin_boot(start);
        assert!(!app.should_reap_idle(past));
    }

    #[test]
    fn backoff_grows_and_caps() {
        let start = Instant::now();
        let mut app = AppLifecycle::new(config(), start);

        let delays: Vec<Duration> = (0..4)
            .map(|i| {
                let now = start + Duration::from_millis(i);
                match app.note_crash(now) {
                    CrashOutcome::Restart { after } => after,
                    CrashOutcome::CrashLooping => panic!("looped too early at crash {i}"),
                }
            })
            .collect();

        assert_eq!(delays[0], DEFAULT_BACKOFF_BASE);
        assert_eq!(delays[1], DEFAULT_BACKOFF_BASE * 2);
        assert_eq!(delays[2], DEFAULT_BACKOFF_BASE * 4);
        assert_eq!(delays[3], DEFAULT_BACKOFF_BASE * 8);
        assert!(delays.iter().all(|d| *d <= DEFAULT_BACKOFF_MAX));
    }

    #[test]
    fn crash_loop_trips_at_the_threshold() {
        let start = Instant::now();
        let mut app = AppLifecycle::new(config(), start);

        // The first threshold-1 crashes ask for a restart.
        for i in 0..DEFAULT_CRASH_LOOP_THRESHOLD - 1 {
            let now = start + Duration::from_millis(u64::from(i));
            assert!(matches!(
                app.note_crash(now),
                CrashOutcome::Restart { .. }
            ));
        }
        // The threshold-th crash within the window parks the app.
        let now = start + Duration::from_millis(u64::from(DEFAULT_CRASH_LOOP_THRESHOLD));
        assert_eq!(app.note_crash(now), CrashOutcome::CrashLooping);
        assert_eq!(app.state(), LifecycleState::Failed);
    }

    #[test]
    fn crashes_age_out_of_the_window() {
        let start = Instant::now();
        let mut app = AppLifecycle::new(config(), start);
        for i in 0..DEFAULT_CRASH_LOOP_THRESHOLD - 1 {
            app.note_crash(start + Duration::from_millis(u64::from(i)));
        }
        // Long after the window, the old crashes no longer count.
        let later = start + DEFAULT_CRASH_WINDOW + Duration::from_secs(1);
        assert_eq!(app.recent_crashes(later), 0);
        // A fresh crash is treated as the first again: base backoff.
        assert_eq!(
            app.note_crash(later),
            CrashOutcome::Restart {
                after: DEFAULT_BACKOFF_BASE
            }
        );
    }

    #[test]
    fn clearing_a_failure_reenables_booting() {
        let start = Instant::now();
        let mut app = AppLifecycle::new(config(), start);
        for i in 0..DEFAULT_CRASH_LOOP_THRESHOLD {
            app.note_crash(start + Duration::from_millis(u64::from(i)));
        }
        assert_eq!(app.state(), LifecycleState::Failed);

        let now = start + Duration::from_secs(1);
        app.clear_failure(now);
        assert_eq!(app.state(), LifecycleState::Cold);
        assert_eq!(app.recent_crashes(now), 0);
    }
}
