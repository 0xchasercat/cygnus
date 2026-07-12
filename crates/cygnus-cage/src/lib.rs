//! The cage: Cygnus's per-app isolation stack.
//!
//! A cage is a warm, reusable sandbox built from kernel primitives. This slice
//! establishes the boot path with Linux namespaces and cgroups v2, a private
//! mount tree, and a `procfs` bound to the cage's own PID namespace. The
//! overlay root, seccomp, networking, and long-lived supervision are added by
//! later slices.
//!
//! On non-Linux hosts the same API boots the target as a plain child
//! process with no isolation; the platform runs identically, minus the cage
//! walls.

mod error;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
mod mount;
#[cfg(not(target_os = "linux"))]
mod process;
mod spec;

pub use error::CageError;
#[cfg(target_os = "linux")]
pub use linux::Cage;
#[cfg(not(target_os = "linux"))]
pub use process::Cage;
pub use spec::{
    BootTimings, CageSpec, CgroupLimits, DEFAULT_CPU_PERIOD, DEFAULT_CPU_QUOTA,
    DEFAULT_MEMORY_HIGH, DEFAULT_MEMORY_MAX, DEFAULT_PIDS_MAX, DEFAULT_READINESS_TIMEOUT,
};

/// Isolation provided by the cage backend compiled for this platform.
#[cfg(target_os = "linux")]
pub const ISOLATION: &str = "kernel (namespaces + cgroups v2)";
/// Isolation provided by the cage backend compiled for this platform.
#[cfg(not(target_os = "linux"))]
pub const ISOLATION: &str = "none (plain process)";
