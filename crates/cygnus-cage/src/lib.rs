//! The cage: Cygnus's per-app isolation stack.
//!
//! A cage is a warm, reusable sandbox built from kernel primitives. This slice
//! establishes the boot path with Linux namespaces and cgroups v2. Mounts,
//! seccomp, networking, and long-lived supervision are added by later slices.

mod linux;
mod spec;

pub use linux::{Cage, CageError};
pub use spec::{
    BootTimings, CageSpec, CgroupLimits, DEFAULT_CPU_PERIOD, DEFAULT_CPU_QUOTA,
    DEFAULT_MEMORY_HIGH, DEFAULT_MEMORY_MAX, DEFAULT_PIDS_MAX, DEFAULT_READINESS_TIMEOUT,
};
