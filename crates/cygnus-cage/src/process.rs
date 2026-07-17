//! Portable process backend: the cage API without kernel isolation.
//!
//! Non-Linux hosts boot the target as a plain child process, so the whole
//! platform builds, tests, and runs anywhere. There are no namespaces, no
//! cgroups, and no seccomp here: the resource limits in the cage
//! specification are validated but not enforced. The Linux backend is where
//! the isolation lives.

use std::fs::File;
use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::InstanceStatus;
use crate::error::CageError;
use crate::jobs::JobExitOutcome;
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
        Self::boot_inner(spec, None, None)
    }

    /// Boot the target with its standard output and standard error connected
    /// to caller-opened files.
    ///
    /// The cage takes ownership of both files. It does not open, create, or
    /// otherwise interpret paths for process output.
    pub fn boot_with_output(spec: CageSpec, stdout: File, stderr: File) -> Result<Self, CageError> {
        Self::boot_inner(spec, Some(stdout.into()), Some(stderr.into()))
    }

    /// Boot the target with its standard streams connected to the supplied
    /// pipe write ends. The descriptors are consumed by the spawned child.
    pub(crate) fn boot_with_capture(
        spec: CageSpec,
        stdout: OwnedFd,
        stderr: OwnedFd,
    ) -> Result<Self, CageError> {
        Self::boot_inner(spec, Some(stdout), Some(stderr))
    }

    fn boot_inner(
        spec: CageSpec,
        stdout: Option<OwnedFd>,
        stderr: Option<OwnedFd>,
    ) -> Result<Self, CageError> {
        spec.validate()?;
        let boot_started = Instant::now();
        let deadline = boot_started
            .checked_add(spec.readiness_timeout)
            .ok_or_else(|| CageError::InvalidSpec("readiness_timeout is too large".into()))?;

        let mut command = Command::new(&spec.command);
        command.args(&spec.args).env_clear().envs(&spec.env);
        if let Some(working_dir) = &spec.working_dir {
            command.current_dir(working_dir);
        }
        if let Some(stdout) = stdout {
            command.stdout(Stdio::from(stdout));
        }
        if let Some(stderr) = stderr {
            command.stderr(Stdio::from(stderr));
        }
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

    /// Poll the child without blocking; an exited child is reaped exactly once.
    pub fn try_status(&mut self) -> Result<InstanceStatus, CageError> {
        let Some(child) = self.child.as_mut() else {
            return Ok(InstanceStatus::Exited);
        };
        match child.try_wait().map_err(|source| CageError::Spawn {
            operation: "poll cage process",
            source,
        })? {
            Some(_) => {
                // std::process::Child::try_wait reaps the child when it reports
                // an exit. Drop the handle so teardown never waits or kills it
                // a second time.
                self.child = None;
                Ok(InstanceStatus::Exited)
            }
            None => Ok(InstanceStatus::Running),
        }
    }

    /// Poll and return the finite job outcome when the child has exited.
    pub(crate) fn try_job_status(&mut self) -> Result<Option<JobExitOutcome>, CageError> {
        let Some(child) = self.child.as_mut() else {
            return Ok(None);
        };
        let Some(status) = child.try_wait().map_err(|source| CageError::Spawn {
            operation: "poll cage process",
            source,
        })?
        else {
            return Ok(None);
        };
        self.child = None;
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            if let Some(signal) = status.signal() {
                return Ok(Some(JobExitOutcome::Signaled(signal)));
            }
        }
        Ok(Some(JobExitOutcome::Exited(status.code().unwrap_or(-1))))
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::fs::{self, OpenOptions};
    use std::thread;

    fn shell_spec(script: &str) -> CageSpec {
        let mut spec = CageSpec::new("portable-test", "/bin/sh");
        spec.args = vec![OsString::from("-c"), OsString::from(script)];
        spec
    }

    #[test]
    fn boot_with_output_routes_the_child_streams_to_owned_files() {
        let directory = std::env::temp_dir().join(format!(
            "cygnus-cage-output-{}-{:?}",
            std::process::id(),
            thread::current().id()
        ));
        fs::create_dir(&directory).expect("create output directory");
        let stdout_path = directory.join("stdout");
        let stderr_path = directory.join("stderr");
        let stdout = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&stdout_path)
            .expect("open stdout");
        let stderr = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&stderr_path)
            .expect("open stderr");

        let mut cage = Cage::boot_with_output(
            shell_spec("printf stdout; printf stderr >&2"),
            stdout,
            stderr,
        )
        .expect("boot cage with output");
        let deadline = Instant::now() + Duration::from_secs(1);
        while cage.try_status().expect("poll output cage") == InstanceStatus::Running {
            assert!(Instant::now() < deadline, "child did not exit");
            thread::sleep(POLL_INTERVAL);
        }
        cage.teardown().expect("teardown output cage");

        assert_eq!(fs::read(stdout_path).expect("read stdout"), b"stdout");
        assert_eq!(fs::read(stderr_path).expect("read stderr"), b"stderr");
        fs::remove_dir_all(directory).expect("remove output directory");
    }

    #[test]
    fn try_status_leaves_a_running_child_untouched() {
        let mut cage = Cage::boot(shell_spec("exec sleep 1")).expect("boot portable cage");
        assert!(matches!(cage.try_status(), Ok(InstanceStatus::Running)));
        assert!(cage.host_pid().is_some(), "running child remains owned");
        cage.teardown().expect("teardown running child");
    }

    #[test]
    fn try_status_reaps_an_exit_once_and_teardown_is_idempotent() {
        let mut cage = Cage::boot(shell_spec("exit 0")).expect("boot portable cage");
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            if cage.try_status().expect("poll portable cage") == InstanceStatus::Exited {
                break;
            }
            assert!(Instant::now() < deadline, "child did not exit");
            thread::sleep(POLL_INTERVAL);
        }
        assert!(matches!(cage.try_status(), Ok(InstanceStatus::Exited)));
        assert!(
            cage.host_pid().is_none(),
            "exit poll relinquishes the child"
        );
        cage.teardown().expect("teardown already-reaped child");
    }
}
