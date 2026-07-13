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
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use cygnus_cage::CageSpec;

use crate::{AppLifecycle, CrashOutcome, LifecycleConfig, LifecycleState};

/// An app instance the supervisor owns and can shut down. `cygnus_cage::Cage`
/// implements this; tests substitute a fake.
pub trait Instance: Send {
    /// Release the instance's resources (kill, reap, tear down).
    fn shutdown(self) -> Result<(), String>;
}

impl Instance for cygnus_cage::Cage {
    fn shutdown(self) -> Result<(), String> {
        self.teardown().map_err(|error| error.to_string())
    }
}

/// Why acquiring a ready instance failed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AcquireError {
    /// No app is registered under this name.
    Unknown,
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
}

impl<I: Instance> Supervisor<I> {
    /// Build a supervisor with the given boot function.
    pub fn new(boot: impl Fn(&CageSpec) -> Result<I, String> + Send + Sync + 'static) -> Self {
        Self {
            boot: Box::new(boot),
            apps: Mutex::new(HashMap::new()),
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
        self.apps.lock().unwrap().insert(name.into(), slot);
    }

    /// Ensure the named app is booted and ready, booting it on demand. Callers
    /// racing on a cold app coalesce onto a single boot.
    pub fn acquire(&self, name: &str) -> Result<(), AcquireError> {
        let slot = {
            let apps = self.apps.lock().unwrap();
            apps.get(name).cloned().ok_or(AcquireError::Unknown)?
        };

        let mut state = slot.state.lock().unwrap();
        loop {
            match state.lifecycle.state() {
                LifecycleState::Ready => {
                    state.lifecycle.note_request(Instant::now());
                    return Ok(());
                }
                LifecycleState::Failed => return Err(AcquireError::CrashLooping),
                LifecycleState::Booting | LifecycleState::Draining => {
                    // Another caller is booting this app, or it is draining;
                    // wait for that to finish, then re-evaluate.
                    state = slot.progress.wait(state).unwrap();
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
                    state = slot.state.lock().unwrap();
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

    /// Reap every ready app that has been idle past its TTL (and is not
    /// pinned). Returns the names reaped. `now` is passed for testability.
    pub fn reap_idle(&self, now: Instant) -> Vec<String> {
        let candidates: Vec<(String, Arc<Slot<I>>)> = {
            let apps = self.apps.lock().unwrap();
            apps.iter()
                .map(|(name, slot)| (name.clone(), Arc::clone(slot)))
                .collect()
        };

        let mut reaped = Vec::new();
        for (name, slot) in candidates {
            let mut state = slot.state.lock().unwrap();
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

            let mut state = slot.state.lock().unwrap();
            state.lifecycle.mark_cold();
            slot.progress.notify_all();
            reaped.push(name);
        }
        reaped
    }

    /// Current lifecycle state of an app, if registered.
    pub fn state(&self, name: &str) -> Option<LifecycleState> {
        let slot = self.apps.lock().unwrap().get(name).cloned()?;
        let state = slot.state.lock().unwrap();
        Some(state.lifecycle.state())
    }

    /// Clear a crash-looping app so it can boot again on the next acquire.
    pub fn clear_failure(&self, name: &str) {
        if let Some(slot) = self.apps.lock().unwrap().get(name).cloned() {
            let mut state = slot.state.lock().unwrap();
            state.lifecycle.clear_failure(Instant::now());
            state.retry_after = None;
            slot.progress.notify_all();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::thread;

    struct FakeInstance;

    impl Instance for FakeInstance {
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
