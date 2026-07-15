//! The live supervisor: on-demand cage boot with request coalescing, idle
//! scale-to-zero, and boot-failure backoff, applying the [`AppLifecycle`]
//! policy to real instances (spec §5).
//!
//! The supervisor is generic over the instance type behind the [`Instance`]
//! trait and takes an injected boot closure, so the coalescing and lifecycle
//! logic is exercised in tests with a fake instance and no privilege; the
//! daemon wires it to `cygnus_cage::Cage`.
//!
//! Concurrency model: one `Mutex`-guarded state record per app, with a
//! `Condvar` for boot/drain progress. A cold request transitions the app to
//! `Booting`, drops the lock for the slow boot, then re-takes it to record the
//! result; concurrent requests for the same cold app see `Booting` and wait on
//! the condvar rather than each starting a boot — so N simultaneous callers
//! trigger exactly one boot. No lock is ever held across a boot or a shutdown.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use cygnus_cage::CageSpec;

use crate::{AppLifecycle, CrashOutcome, InstanceStatus, LifecycleConfig, LifecycleState};

fn recover<T>(result: std::sync::LockResult<T>) -> T {
    result.unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// An app instance the supervisor owns and can poll or shut down. `cygnus_cage::Cage`
/// implements this; tests substitute a fake.
pub trait Instance: Send {
    /// Nonblocking process liveness poll. An exited process must be reaped by
    /// this call so a later shutdown only releases remaining resources.
    fn try_status(&mut self) -> Result<InstanceStatus, String>;
    /// Release the instance's resources (kill, reap, tear down).
    fn shutdown(self) -> Result<(), String>;
}

impl Instance for cygnus_cage::Cage {
    fn try_status(&mut self) -> Result<InstanceStatus, String> {
        cygnus_cage::Cage::try_status(self).map_err(|error| error.to_string())
    }

    fn shutdown(self) -> Result<(), String> {
        self.teardown().map_err(|error| error.to_string())
    }
}

/// Why acquiring a ready instance failed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AcquireError {
    /// No app is registered under this name.
    Unknown,
    /// The daemon is shutting down and will not start or retain a cage.
    ShuttingDown,
    /// The app is crash-looping and parked; do not retry until it is cleared.
    CrashLooping,
    /// A boot is backing off after a recent failure; retry after this delay.
    BackingOff { retry_after: Duration },
    /// The boot this call attempted failed.
    BootFailed(String),
}

/// Boot function: turn a spec into a running instance, or an error string.
type BootFn<I> = dyn Fn(&CageSpec) -> Result<I, String> + Send + Sync;

struct SlotState<I> {
    lifecycle: AppLifecycle,
    instance: Option<I>,
    retry_after: Option<Instant>,
}

struct Slot<I> {
    spec: CageSpec,
    state: Mutex<SlotState<I>>,
    progress: Condvar,
}

/// Supervises the per-app cage lifecycle for a node.
pub struct Supervisor<I> {
    boot: Box<BootFn<I>>,
    apps: Mutex<HashMap<String, Arc<Slot<I>>>>,
    shutting_down: AtomicBool,
}

impl<I: Instance> Supervisor<I> {
    /// Build a supervisor with the given boot function.
    pub fn new(boot: impl Fn(&CageSpec) -> Result<I, String> + Send + Sync + 'static) -> Self {
        Self {
            boot: Box::new(boot),
            apps: Mutex::new(HashMap::new()),
            shutting_down: AtomicBool::new(false),
        }
    }

    /// Register an app with its boot spec and lifecycle policy. Replaces any
    /// existing registration (the previous instance, if any, is dropped, which
    /// tears it down). Starts cold.
    pub fn register(&self, name: impl Into<String>, spec: CageSpec, config: LifecycleConfig) {
        let slot = Arc::new(Slot {
            spec,
            state: Mutex::new(SlotState {
                lifecycle: AppLifecycle::new(config, Instant::now()),
                instance: None,
                retry_after: None,
            }),
            progress: Condvar::new(),
        });
        recover(self.apps.lock()).insert(name.into(), slot);
    }

    /// Ensure the named app is booted and ready, booting it on demand. Callers
    /// racing on a cold app coalesce onto a single boot.
    pub fn acquire(&self, name: &str) -> Result<(), AcquireError> {
        if self.shutting_down.load(Ordering::Acquire) {
            return Err(AcquireError::ShuttingDown);
        }
        let slot = {
            let apps = recover(self.apps.lock());
            apps.get(name).cloned().ok_or(AcquireError::Unknown)?
        };

        let mut state = recover(slot.state.lock());
        loop {
            if self.shutting_down.load(Ordering::Acquire) {
                return Err(AcquireError::ShuttingDown);
            }
            match state.lifecycle.state() {
                LifecycleState::Ready => {
                    state.lifecycle.note_request(Instant::now());
                    return Ok(());
                }
                LifecycleState::Failed => return Err(AcquireError::CrashLooping),
                LifecycleState::Booting | LifecycleState::Draining => {
                    // Another caller is booting this app, or it is draining;
                    // wait for that to finish, then re-evaluate.
                    state = recover(slot.progress.wait(state));
                }
                LifecycleState::Cold => {
                    if let Some(retry_after) = state.retry_after {
                        let now = Instant::now();
                        if now < retry_after {
                            return Err(AcquireError::BackingOff {
                                retry_after: retry_after - now,
                            });
                        }
                    }
                    // This caller owns the boot. Mark Booting and release the
                    // lock so the slow boot does not block other callers (which
                    // will wait on the condvar).
                    state.lifecycle.begin_boot(Instant::now());
                    drop(state);
                    let result = (self.boot)(&slot.spec);
                    state = recover(slot.state.lock());
                    if self.shutting_down.load(Ordering::Acquire) {
                        let instance = result.ok();
                        state.lifecycle.mark_cold();
                        state.retry_after = None;
                        slot.progress.notify_all();
                        drop(state);
                        if let Some(instance) = instance {
                            let _ = instance.shutdown();
                        }
                        return Err(AcquireError::ShuttingDown);
                    }
                    match result {
                        Ok(instance) => {
                            state.instance = Some(instance);
                            state.lifecycle.mark_ready(Instant::now());
                            state.retry_after = None;
                            slot.progress.notify_all();
                            return Ok(());
                        }
                        Err(error) => {
                            let outcome = state.lifecycle.note_crash(Instant::now());
                            slot.progress.notify_all();
                            return match outcome {
                                CrashOutcome::Restart { after } => {
                                    state.retry_after = Some(Instant::now() + after);
                                    Err(AcquireError::BootFailed(error))
                                }
                                CrashOutcome::CrashLooping => Err(AcquireError::CrashLooping),
                            };
                        }
                    }
                }
            }
        }
    }
    /// Poll every ready instance and reconcile exits through the lifecycle
    /// crash policy. Exited instances are detached before shutdown so callers
    /// racing an asynchronous maintenance pass wait on `Draining` rather than
    /// observing a stale `Ready` slot. Returns the names that crashed.
    pub fn reconcile(&self, now: Instant) -> Vec<String> {
        let candidates: Vec<(String, Arc<Slot<I>>)> = {
            let apps = recover(self.apps.lock());
            apps.iter()
                .map(|(name, slot)| (name.clone(), Arc::clone(slot)))
                .collect()
        };

        let mut crashed = Vec::new();
        for (name, slot) in candidates {
            let mut state = recover(slot.state.lock());
            if state.lifecycle.state() != LifecycleState::Ready {
                continue;
            }

            let status = match state.instance.as_mut() {
                Some(instance) => instance.try_status(),
                None => Err("ready slot has no instance".to_owned()),
            };
            if matches!(status, Ok(InstanceStatus::Running)) {
                continue;
            }

            state.lifecycle.begin_drain();
            let instance = state.instance.take();
            drop(state);

            if let Some(instance) = instance {
                let _ = instance.shutdown();
            }

            let mut state = recover(slot.state.lock());
            match state.lifecycle.note_crash(now) {
                CrashOutcome::Restart { after } => {
                    state.retry_after = Some(now + after);
                }
                CrashOutcome::CrashLooping => {
                    state.retry_after = None;
                }
            }
            slot.progress.notify_all();
            crashed.push(name);
        }
        crashed
    }

    /// Reap every ready app that has been idle past its TTL (and is not
    /// pinned). Returns the names reaped. `now` is passed for testability.
    pub fn reap_idle(&self, now: Instant) -> Vec<String> {
        let candidates: Vec<(String, Arc<Slot<I>>)> = {
            let apps = recover(self.apps.lock());
            apps.iter()
                .map(|(name, slot)| (name.clone(), Arc::clone(slot)))
                .collect()
        };

        let mut reaped = Vec::new();
        for (name, slot) in candidates {
            let mut state = recover(slot.state.lock());
            if !state.lifecycle.should_reap_idle(now) {
                continue;
            }
            // Move to Draining and take the instance out, then release the lock
            // for the slow shutdown so acquires only wait, never block on it.
            state.lifecycle.begin_drain();
            let instance = state.instance.take();
            drop(state);

            if let Some(instance) = instance {
                let _ = instance.shutdown();
            }

            let mut state = recover(slot.state.lock());
            state.lifecycle.mark_cold();
            slot.progress.notify_all();
            reaped.push(name);
        }
        reaped
    }

    /// Stop admitting boots, wait for in-flight lifecycle transitions, and
    /// release every ready instance. Returns per-app teardown failures.
    pub fn shutdown_all(&self) -> Vec<(String, String)> {
        self.shutting_down.store(true, Ordering::Release);
        let candidates: Vec<(String, Arc<Slot<I>>)> = {
            let apps = recover(self.apps.lock());
            apps.iter()
                .map(|(name, slot)| (name.clone(), Arc::clone(slot)))
                .collect()
        };

        let mut failures = Vec::new();
        for (name, slot) in candidates {
            let mut state = recover(slot.state.lock());
            while matches!(
                state.lifecycle.state(),
                LifecycleState::Booting | LifecycleState::Draining
            ) {
                state = recover(slot.progress.wait(state));
            }
            if state.lifecycle.state() != LifecycleState::Ready {
                continue;
            }
            state.lifecycle.begin_drain();
            let instance = state.instance.take();
            drop(state);

            if let Some(instance) = instance
                && let Err(error) = instance.shutdown()
            {
                failures.push((name, error));
            }

            let mut state = recover(slot.state.lock());
            state.lifecycle.mark_cold();
            state.retry_after = None;
            slot.progress.notify_all();
        }
        failures
    }

    /// Current lifecycle state of an app, if registered.
    pub fn state(&self, name: &str) -> Option<LifecycleState> {
        let slot = recover(self.apps.lock()).get(name).cloned()?;
        let state = recover(slot.state.lock());
        Some(state.lifecycle.state())
    }

    /// Clear a crash-looping app so it can boot again on the next acquire.
    pub fn clear_failure(&self, name: &str) {
        if let Some(slot) = recover(self.apps.lock()).get(name).cloned() {
            let mut state = recover(slot.state.lock());
            state.lifecycle.clear_failure(Instant::now());
            state.retry_after = None;
            slot.progress.notify_all();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Barrier;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::thread;

    struct FakeInstance;

    impl Instance for FakeInstance {
        fn try_status(&mut self) -> Result<InstanceStatus, String> {
            Ok(InstanceStatus::Running)
        }

        fn shutdown(self) -> Result<(), String> {
            Ok(())
        }
    }

    fn spec() -> CageSpec {
        CageSpec::new("app", "/bin/true")
    }

    #[test]
    fn acquire_boots_a_cold_app_once_then_reuses_it() {
        let boots = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&boots);
        let supervisor = Supervisor::new(move |_spec| {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(FakeInstance)
        });
        supervisor.register("app", spec(), LifecycleConfig::default());

        assert_eq!(supervisor.acquire("app"), Ok(()));
        assert_eq!(supervisor.state("app"), Some(LifecycleState::Ready));
        assert_eq!(supervisor.acquire("app"), Ok(()));
        assert_eq!(boots.load(Ordering::SeqCst), 1, "a ready app is reused");
    }

    #[test]
    fn unknown_app_is_rejected() {
        let supervisor = Supervisor::new(|_spec| Ok(FakeInstance));
        assert_eq!(supervisor.acquire("nope"), Err(AcquireError::Unknown));
    }

    #[test]
    fn concurrent_cold_acquires_coalesce_onto_one_boot() {
        let boots = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&boots);
        // A slow boot widens the window in which callers must coalesce.
        let supervisor = Arc::new(Supervisor::new(move |_spec| {
            counter.fetch_add(1, Ordering::SeqCst);
            thread::sleep(Duration::from_millis(50));
            Ok(FakeInstance)
        }));
        supervisor.register("app", spec(), LifecycleConfig::default());

        let mut handles = Vec::new();
        for _ in 0..8 {
            let supervisor = Arc::clone(&supervisor);
            handles.push(thread::spawn(move || supervisor.acquire("app")));
        }
        for handle in handles {
            assert_eq!(handle.join().unwrap(), Ok(()));
        }
        assert_eq!(
            boots.load(Ordering::SeqCst),
            1,
            "eight racing callers should trigger exactly one boot"
        );
    }

    struct StatusInstance {
        status: InstanceStatus,
        shutdowns: Arc<AtomicUsize>,
    }

    impl Instance for StatusInstance {
        fn try_status(&mut self) -> Result<InstanceStatus, String> {
            Ok(self.status)
        }

        fn shutdown(self) -> Result<(), String> {
            self.shutdowns.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    #[test]
    fn reconcile_leaves_running_instances_untouched() {
        let shutdowns = Arc::new(AtomicUsize::new(0));
        let shutdown_counter = Arc::clone(&shutdowns);
        let supervisor = Supervisor::new(move |_spec| {
            Ok(StatusInstance {
                status: InstanceStatus::Running,
                shutdowns: Arc::clone(&shutdown_counter),
            })
        });
        supervisor.register("app", spec(), LifecycleConfig::default());

        assert_eq!(supervisor.acquire("app"), Ok(()));
        assert!(supervisor.reconcile(Instant::now()).is_empty());
        assert_eq!(supervisor.state("app"), Some(LifecycleState::Ready));
        assert_eq!(shutdowns.load(Ordering::SeqCst), 0);
    }
    #[test]
    fn shutdown_releases_ready_instances_and_rejects_new_acquires() {
        let shutdowns = Arc::new(AtomicUsize::new(0));
        let shutdown_counter = Arc::clone(&shutdowns);
        let supervisor = Supervisor::new(move |_spec| {
            Ok(StatusInstance {
                status: InstanceStatus::Running,
                shutdowns: Arc::clone(&shutdown_counter),
            })
        });
        supervisor.register("app", spec(), LifecycleConfig::default());

        assert_eq!(supervisor.acquire("app"), Ok(()));
        assert!(supervisor.shutdown_all().is_empty());
        assert_eq!(shutdowns.load(Ordering::SeqCst), 1);
        assert_eq!(supervisor.state("app"), Some(LifecycleState::Cold));
        assert_eq!(supervisor.acquire("app"), Err(AcquireError::ShuttingDown));
    }

    #[test]
    fn shutdown_does_not_retain_a_boot_that_finishes_in_flight() {
        let boot_started = Arc::new(Barrier::new(2));
        let release_boot = Arc::new(Barrier::new(2));
        let shutdowns = Arc::new(AtomicUsize::new(0));
        let supervisor = Arc::new(Supervisor::new({
            let boot_started = Arc::clone(&boot_started);
            let release_boot = Arc::clone(&release_boot);
            let shutdowns = Arc::clone(&shutdowns);
            move |_spec| {
                boot_started.wait();
                release_boot.wait();
                Ok(StatusInstance {
                    status: InstanceStatus::Running,
                    shutdowns: Arc::clone(&shutdowns),
                })
            }
        }));
        supervisor.register("app", spec(), LifecycleConfig::default());

        let acquire = {
            let supervisor = Arc::clone(&supervisor);
            thread::spawn(move || supervisor.acquire("app"))
        };
        boot_started.wait();
        let shutdown = {
            let supervisor = Arc::clone(&supervisor);
            thread::spawn(move || supervisor.shutdown_all())
        };
        while !supervisor.shutting_down.load(Ordering::Acquire) {
            thread::yield_now();
        }
        release_boot.wait();

        assert_eq!(acquire.join().unwrap(), Err(AcquireError::ShuttingDown));
        assert!(shutdown.join().unwrap().is_empty());
        assert_eq!(shutdowns.load(Ordering::SeqCst), 1);
        assert_eq!(supervisor.state("app"), Some(LifecycleState::Cold));
    }

    #[test]
    fn reconcile_records_one_exit_and_recovers_after_backoff() {
        let boots = Arc::new(AtomicUsize::new(0));
        let boot_counter = Arc::clone(&boots);
        let shutdowns = Arc::new(AtomicUsize::new(0));
        let shutdown_counter = Arc::clone(&shutdowns);
        let supervisor = Supervisor::new(move |_spec| {
            let status = if boot_counter.fetch_add(1, Ordering::SeqCst) == 0 {
                InstanceStatus::Exited
            } else {
                InstanceStatus::Running
            };
            Ok(StatusInstance {
                status,
                shutdowns: Arc::clone(&shutdown_counter),
            })
        });
        let config = LifecycleConfig {
            backoff_base: Duration::from_millis(20),
            ..LifecycleConfig::default()
        };
        supervisor.register("app", spec(), config);

        assert_eq!(supervisor.acquire("app"), Ok(()));
        assert_eq!(supervisor.reconcile(Instant::now()), vec!["app".to_owned()]);
        assert_eq!(supervisor.state("app"), Some(LifecycleState::Cold));
        assert!(supervisor.reconcile(Instant::now()).is_empty());
        assert!(matches!(
            supervisor.acquire("app"),
            Err(AcquireError::BackingOff { .. })
        ));
        thread::sleep(Duration::from_millis(25));
        assert_eq!(supervisor.acquire("app"), Ok(()));
        assert_eq!(supervisor.state("app"), Some(LifecycleState::Ready));
        assert!(supervisor.reconcile(Instant::now()).is_empty());
        assert_eq!(boots.load(Ordering::SeqCst), 2);
        assert_eq!(shutdowns.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn repeated_exits_park_a_crash_looping_app() {
        let boots = Arc::new(AtomicUsize::new(0));
        let boot_counter = Arc::clone(&boots);
        let supervisor = Supervisor::new(move |_spec| {
            boot_counter.fetch_add(1, Ordering::SeqCst);
            Ok(StatusInstance {
                status: InstanceStatus::Exited,
                shutdowns: Arc::new(AtomicUsize::new(0)),
            })
        });
        let config = LifecycleConfig {
            backoff_base: Duration::ZERO,
            backoff_max: Duration::ZERO,
            crash_loop_threshold: 3,
            ..LifecycleConfig::default()
        };
        supervisor.register("app", spec(), config);

        for _ in 0..3 {
            assert_eq!(supervisor.acquire("app"), Ok(()));
            supervisor.reconcile(Instant::now());
        }
        assert_eq!(supervisor.state("app"), Some(LifecycleState::Failed));
        assert_eq!(supervisor.acquire("app"), Err(AcquireError::CrashLooping));
        assert_eq!(boots.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn a_failed_boot_backs_off_then_loops() {
        let config = LifecycleConfig {
            backoff_base: Duration::from_millis(50),
            crash_loop_threshold: 3,
            ..LifecycleConfig::default()
        };
        let supervisor = Supervisor::new(|_spec| Err::<FakeInstance, _>("boom".to_owned()));
        supervisor.register("app", spec(), config);

        // First failure asks the caller to back off.
        assert_eq!(
            supervisor.acquire("app"),
            Err(AcquireError::BootFailed("boom".to_owned()))
        );
        // Immediately retrying is refused while the backoff is in effect.
        assert!(matches!(
            supervisor.acquire("app"),
            Err(AcquireError::BackingOff { .. })
        ));

        // After the backoff elapses, further failures accrue until the app is
        // parked as crash-looping.
        loop {
            match supervisor.acquire("app") {
                Err(AcquireError::CrashLooping) => break,
                Err(AcquireError::BootFailed(_)) | Err(AcquireError::BackingOff { .. }) => {
                    thread::sleep(Duration::from_millis(55));
                }
                other => panic!("unexpected acquire result: {other:?}"),
            }
        }
        assert_eq!(supervisor.state("app"), Some(LifecycleState::Failed));

        // Clearing the failure re-enables booting.
        supervisor.clear_failure("app");
        assert_eq!(supervisor.state("app"), Some(LifecycleState::Cold));
    }

    #[test]
    fn idle_apps_are_reaped_and_reboot_on_next_acquire() {
        let boots = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&boots);
        let supervisor = Supervisor::new(move |_spec| {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(FakeInstance)
        });
        supervisor.register("app", spec(), LifecycleConfig::default());

        assert_eq!(supervisor.acquire("app"), Ok(()));
        // Well past the idle TTL.
        let future = Instant::now() + crate::DEFAULT_IDLE_TTL + Duration::from_secs(1);
        assert_eq!(supervisor.reap_idle(future), vec!["app".to_owned()]);
        assert_eq!(supervisor.state("app"), Some(LifecycleState::Cold));

        assert_eq!(supervisor.acquire("app"), Ok(()));
        assert_eq!(boots.load(Ordering::SeqCst), 2, "a reaped app reboots");
    }

    #[test]
    fn pinned_apps_are_not_reaped() {
        let supervisor = Supervisor::new(|_spec| Ok(FakeInstance));
        let config = LifecycleConfig {
            min_instances: 1,
            ..LifecycleConfig::default()
        };
        supervisor.register("app", spec(), config);
        assert_eq!(supervisor.acquire("app"), Ok(()));

        let future = Instant::now() + crate::DEFAULT_IDLE_TTL + Duration::from_secs(3600);
        assert!(supervisor.reap_idle(future).is_empty(), "pinned stays warm");
        assert_eq!(supervisor.state("app"), Some(LifecycleState::Ready));
    }
}
