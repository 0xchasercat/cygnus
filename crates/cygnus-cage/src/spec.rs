use std::collections::BTreeMap;
use std::ffi::OsString;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::time::Duration;

use crate::linux::CageError;

/// Default hard memory limit: 256 MiB.
pub const DEFAULT_MEMORY_MAX: u64 = 256 * 1024 * 1024;
/// Default memory pressure threshold: 224 MiB.
pub const DEFAULT_MEMORY_HIGH: u64 = 224 * 1024 * 1024;
/// Default CPU quota: one CPU worth of time per 100 ms period.
pub const DEFAULT_CPU_QUOTA: u64 = 100_000;
/// Default CPU accounting period: 100 ms.
pub const DEFAULT_CPU_PERIOD: u64 = 100_000;
/// Default process limit.
pub const DEFAULT_PIDS_MAX: u32 = 128;
/// Default time allowed for the target to exec and become ready.
pub const DEFAULT_READINESS_TIMEOUT: Duration = Duration::from_secs(10);

/// Resource limits written to a cage's cgroup v2 controls.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CgroupLimits {
    pub memory_max: u64,
    pub memory_high: u64,
    pub cpu_quota: u64,
    pub cpu_period: u64,
    pub pids_max: u32,
}

impl Default for CgroupLimits {
    fn default() -> Self {
        Self {
            memory_max: DEFAULT_MEMORY_MAX,
            memory_high: DEFAULT_MEMORY_HIGH,
            cpu_quota: DEFAULT_CPU_QUOTA,
            cpu_period: DEFAULT_CPU_PERIOD,
            pids_max: DEFAULT_PIDS_MAX,
        }
    }
}

/// Complete description of one cage boot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CageSpec {
    pub name: String,
    pub command: OsString,
    pub args: Vec<OsString>,
    pub env: BTreeMap<OsString, OsString>,
    pub limits: CgroupLimits,
    pub readiness_uds: Option<PathBuf>,
    pub readiness_timeout: Duration,
}

impl CageSpec {
    /// Build a cage specification with the default resource limits.
    pub fn new(name: impl Into<String>, command: impl Into<OsString>) -> Self {
        Self {
            name: name.into(),
            command: command.into(),
            args: Vec::new(),
            env: BTreeMap::new(),
            limits: CgroupLimits::default(),
            readiness_uds: None,
            readiness_timeout: DEFAULT_READINESS_TIMEOUT,
        }
    }

    /// Validate fields that must be safe before any kernel state is changed.
    pub fn validate(&self) -> Result<(), CageError> {
        validate_name(&self.name)?;
        if self.command.is_empty() {
            return Err(CageError::InvalidSpec("command must not be empty".into()));
        }
        if self.limits.memory_max == 0 {
            return Err(CageError::InvalidSpec(
                "memory_max must be greater than zero".into(),
            ));
        }
        if self.limits.memory_high > self.limits.memory_max {
            return Err(CageError::InvalidSpec(
                "memory_high must not exceed memory_max".into(),
            ));
        }
        if self.limits.cpu_quota == 0 || self.limits.cpu_period == 0 {
            return Err(CageError::InvalidSpec(
                "cpu quota and period must be greater than zero".into(),
            ));
        }
        if self.limits.pids_max == 0 {
            return Err(CageError::InvalidSpec(
                "pids_max must be greater than zero".into(),
            ));
        }
        if self.readiness_timeout.is_zero() {
            return Err(CageError::InvalidSpec(
                "readiness_timeout must be greater than zero".into(),
            ));
        }
        if let Some(path) = &self.readiness_uds
            && !path.is_absolute()
        {
            return Err(CageError::InvalidSpec(
                "readiness UDS path must be absolute".into(),
            ));
        }
        for key in self.env.keys() {
            let bytes = key.as_os_str().as_bytes();
            if bytes.is_empty() || bytes.contains(&b'=') {
                return Err(CageError::InvalidSpec(
                    "environment keys must be non-empty and must not contain '='".into(),
                ));
            }
        }
        Ok(())
    }
}

/// Per-phase decomposition of a completed cold boot.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct BootTimings {
    pub namespaces_cgroup: Duration,
    /// Placeholder until the mount stack is implemented.
    pub mounts: Duration,
    /// Parent release through successful `execve` detection.
    pub exec_runtime_init: Duration,
    /// Successful `execve` through a readiness UDS accepting connections.
    pub socket_ready: Duration,
    pub total: Duration,
}

pub(crate) fn validate_name(name: &str) -> Result<(), CageError> {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return Err(CageError::InvalidSpec("name must not be empty".into()));
    };
    if name.len() > 128 {
        return Err(CageError::InvalidSpec(
            "name must be at most 128 bytes".into(),
        ));
    }
    if !first.is_ascii_alphanumeric() {
        return Err(CageError::InvalidSpec(
            "name must begin with an ASCII letter or digit".into(),
        ));
    }
    if !chars.all(|character| {
        character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
    }) {
        return Err(CageError::InvalidSpec(
            "name contains unsupported characters".into(),
        ));
    }
    Ok(())
}
