use std::io;
use std::path::PathBuf;
use std::time::Duration;

use nix::errno::Errno;
use nix::unistd::Pid;
use thiserror::Error;

/// Cage configuration or lifecycle failure.
#[derive(Debug, Error)]
pub enum CageError {
    #[error("invalid cage specification: {0}")]
    InvalidSpec(String),
    #[error(
        "cannot create cage namespaces; user namespaces may be disabled or privileges are insufficient: {source}"
    )]
    NamespaceUnavailable {
        #[source]
        source: Errno,
    },
    #[error("failed to {operation}: {source}")]
    Nix {
        operation: &'static str,
        #[source]
        source: Errno,
    },
    #[error("failed to {operation} at {path:?}: {source}")]
    Io {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to {operation}: {source}")]
    Spawn {
        operation: &'static str,
        #[source]
        source: io::Error,
    },
    #[error("cgroup v2 is unavailable: {0}")]
    CgroupUnavailable(String),
    #[error("required cgroup v2 controller '{0}' is unavailable")]
    CgroupControllerUnavailable(&'static str),
    #[error("cgroup already exists at {0:?}")]
    CgroupExists(PathBuf),
    #[error("failed to compile the seccomp filter: {0}")]
    SeccompFilter(String),
    #[error("network setup failed during {operation}: {detail}")]
    Network { operation: String, detail: String },
    #[error("cage child failed during {stage} with errno {errno}")]
    ChildSetup { stage: &'static str, errno: i32 },
    #[error("cage child sent a malformed setup status")]
    MalformedChildStatus,
    #[error("cage child exited before readiness: {0}")]
    ChildExited(String),
    #[error("timed out after {timeout:?} waiting for {phase}")]
    ReadinessTimeout {
        phase: &'static str,
        timeout: Duration,
    },
    #[error("readiness socket {path:?} failed: {source}")]
    ReadinessSocket {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to signal cage process {pid}: {source}")]
    Signal {
        pid: Pid,
        #[source]
        source: Errno,
    },
    #[error("failed to reap cage process {pid}: {source}")]
    Wait {
        pid: Pid,
        #[source]
        source: Errno,
    },
    #[error("internal cage state is incomplete: {0}")]
    Internal(&'static str),
}

#[cfg(target_os = "linux")]
impl CageError {
    pub(crate) fn nix(operation: &'static str, source: Errno) -> Self {
        Self::Nix { operation, source }
    }

    pub(crate) fn io(operation: &'static str, path: impl Into<PathBuf>, source: io::Error) -> Self {
        Self::Io {
            operation,
            path: path.into(),
            source,
        }
    }
}
