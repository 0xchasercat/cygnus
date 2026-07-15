//! The cage: Cygnus's per-app isolation stack.
//!
//! A cage is a warm, reusable sandbox built from kernel primitives. This slice
//! establishes the boot path with Linux namespaces and cgroups v2, a private
//! mount tree, an optional overlay root the cage pivots into, a `procfs` bound
//! to the cage's own PID namespace, and an optional seccomp filter installed
//! immediately before `execve`. Egress addressing and per-cage nftables policy
//! are modelled in [`net`]; wiring the veth and loading the policy, plus
//! long-lived supervision, are added by later slices.
//!
//! On non-Linux hosts the same API boots the target as a plain child
//! process with no isolation; the platform runs identically, minus the cage
//! walls.

mod error;
mod jobs;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
mod mount;
#[cfg(target_os = "linux")]
pub mod net;
#[cfg(not(target_os = "linux"))]
mod process;
#[cfg(target_os = "linux")]
pub mod seccomp;
mod spec;

pub use error::CageError;
pub use jobs::{JobConfig, JobExitOutcome, JobResult, run_job};
#[cfg(target_os = "linux")]
pub use linux::Cage;
#[cfg(not(target_os = "linux"))]
pub use process::Cage;
#[cfg(target_os = "linux")]
pub use seccomp::{SeccompPlan, denied_syscalls};
pub use spec::{
    BUILD_OUTPUT_CAGE_DIR, BootTimings, BuildOutputSpec, CageSpec, CgroupLimits,
    DEFAULT_CPU_PERIOD, DEFAULT_CPU_QUOTA, DEFAULT_MEMORY_HIGH, DEFAULT_MEMORY_MAX,
    DEFAULT_PIDS_MAX, DEFAULT_READINESS_TIMEOUT, DEFAULT_ROOTFS_TMPFS_SIZE, EgressMode, EgressRule,
    FilterMode, INGRESS_CAGE_DIR, IngressSpec, RootfsSpec,
};

/// Nonblocking status reported by a cage process.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InstanceStatus {
    /// The cage process has not exited.
    Running,
    /// The cage process exited and its status was reaped by the poll.
    Exited,
}

/// Isolation provided by the cage backend compiled for this platform.
#[cfg(target_os = "linux")]
pub const ISOLATION: &str = "kernel (namespaces + cgroups v2)";
/// Isolation provided by the cage backend compiled for this platform.
#[cfg(not(target_os = "linux"))]
pub const ISOLATION: &str = "none (plain process)";
