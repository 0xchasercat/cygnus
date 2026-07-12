//! The cage: Cygnus's per-app isolation stack.
//!
//! A cage is a warm, reusable sandbox built from kernel primitives. This slice
//! establishes the boot path with Linux namespaces and cgroups v2. Mounts,
//! seccomp, networking, and long-lived supervision are added by later slices.
//!
//! On non-Linux hosts the same API boots the target as a plain child process
//! with no isolation, so the platform runs identically on a development
//! workstation. Production nodes are Linux.

mod error;
#[cfg(target_os = "linux")]
mod linux;
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
pub const ISOLATION: &str = "none (plain process; development only)";
