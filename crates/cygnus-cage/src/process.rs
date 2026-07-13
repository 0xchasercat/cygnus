//! Portable process backend: the cage API without kernel isolation.
//!
//! Non-Linux hosts boot the target as a plain child process, so the whole
//! platform builds, tests, and runs anywhere. There are no namespaces, no
//! cgroups, and no seccomp here: the resource limits in the cage
//! specification are validated but not enforced. The Linux backend is where
//! the isolation lives.

use std::io;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, Command};
use std::thread;
use std::time::{Duration, Instant};

use crate::error::CageError;
use crate::spec::{BootTimings, CageSpec};

const POLL_INTERVAL: Duration = Duration::from_millis(1);

/// A running cage and the measurements captured while it booted.
///
/// On this platform a cage is a plain child process with no isolation.
#[derive(Debug)]
pub struct Cage {
    child: Option<Child>,
    timings: BootTimings,
}

impl Cage {
    /// Boot the target as an unisolated child process.
    pub fn boot(spec: CageSpec) -> Result<Self, CageError> {
        spec.validate()?;
        let boot_started = Instant::now();
        let deadline = boot_started
            .checked_add(spec.readiness_timeout)
            .ok_or_else(|| CageError::InvalidSpec("readiness_timeout is too large".into()))?;

        let mut command = Command::new(&spec.command);
        command.args(&spec.args).env_clear().envs(&spec.env);
        let exec_started = Instant::now();
        let mut child = command.spawn().map_err(|source| CageError::Spawn {
            operation: "spawn cage process",
            source,
        })?;
        let exec_runtime_init = exec_started.elapsed();

        let socket_ready = if let Some(path) = &spec.readiness_uds {
            let socket_started = Instant::now();
            let wait = wait_for_socket(path, &mut child, deadline, spec.readiness_timeout);
            if let Err(error) = wait {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
            socket_started.elapsed()
        } else {
            Duration::ZERO
        };

        Ok(Self {
            child: Some(child),
            timings: BootTimings {
                namespaces_cgroup: Duration::ZERO,
                network: Duration::ZERO,
                mounts: Duration::ZERO,
                seccomp: Duration::ZERO,
                exec_runtime_init,
                socket_ready,
                total: boot_started.elapsed(),
            },
        })
    }

    /// Return the completed cold-start phase timings.
    pub const fn timings(&self) -> BootTimings {
        self.timings
    }

    /// Return the target's PID as seen by the host.
    pub fn host_pid(&self) -> Option<i32> {
        self.child.as_ref().map(|child| child.id() as i32)
    }

    /// Return the cage's cgroup v2 path. Always `None` on this platform.
    pub fn cgroup_path(&self) -> Option<&Path> {
        None
    }

    /// Kill the target and reap it.
    pub fn teardown(mut self) -> Result<(), CageError> {
        self.cleanup()
    }

    fn cleanup(&mut self) -> Result<(), CageError> {
        let Some(mut child) = self.child.take() else {
            return Ok(());
        };
        if let Err(source) = child.kill()
            && source.kind() != io::ErrorKind::InvalidInput
        {
            let _ = child.wait();
            return Err(CageError::Spawn {
                operation: "kill cage process",
                source,
            });
        }
        child.wait().map(|_| ()).map_err(|source| CageError::Spawn {
            operation: "reap cage process",
            source,
        })
    }
}

impl Drop for Cage {
    fn drop(&mut self) {
        let _ = self.cleanup();
    }
}

fn wait_for_socket(
    path: &Path,
    child: &mut Child,
    deadline: Instant,
    timeout: Duration,
) -> Result<(), CageError> {
    loop {
        match UnixStream::connect(path) {
            Ok(stream) => {
                drop(stream);
                return Ok(());
            }
            Err(source) if retry_socket_error(&source) => {
                let status = child.try_wait().map_err(|source| CageError::Spawn {
                    operation: "poll cage process",
                    source,
                })?;
                if let Some(status) = status {
                    return Err(CageError::ChildExited(status.to_string()));
                }
                if Instant::now() >= deadline {
                    return Err(CageError::ReadinessTimeout {
                        phase: "readiness socket",
                        timeout,
                    });
                }
                thread::sleep(POLL_INTERVAL);
            }
            Err(source) => {
                return Err(CageError::ReadinessSocket {
                    path: path.to_path_buf(),
                    source,
                });
            }
        }
    }
}

fn retry_socket_error(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::NotFound
            | io::ErrorKind::ConnectionRefused
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::WouldBlock
            | io::ErrorKind::Interrupted
    )
}
