use std::collections::BTreeMap;
use std::ffi::OsString;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::error::CageError;

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
/// Default size cap for the writable tmpfs layer of an overlay root: 64 MiB.
pub const DEFAULT_ROOTFS_TMPFS_SIZE: u64 = 64 * 1024 * 1024;

/// Action taken when a cage syscall does not match the seccomp allowlist.
///
/// Defined here, in the platform-neutral spec, so a `CageSpec` can carry the
/// choice on every host; the filter it drives is compiled and installed only
/// by the Linux backend.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FilterMode {
    /// Terminate the process on the first non-matching syscall.
    Enforce,
    /// Log non-matching syscalls to the kernel audit log and allow them to
    /// continue, so a filter can be validated against a corpus before it is
    /// enforced.
    Audit,
}

/// A single allowance in `EgressMode::Restricted`.
///
/// Traffic to `cidr` is permitted; an empty `ports` list allows every
/// destination port, otherwise only the listed ports (TCP or UDP) are allowed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EgressRule {
    /// Destination network in CIDR form, e.g. `203.0.113.0/24`.
    pub cidr: String,
    /// Allowed destination ports; empty means all ports.
    pub ports: Vec<u16>,
}

/// Per-cage egress policy, enforced with nftables in the cage's network
/// namespace (spec §7).
///
/// Ingress is independent and always available over the UDS, so a cage with
/// `None` still serves traffic. `Public` is SSRF-contained by default:
/// RFC1918, link-local (cloud metadata), the node's own addresses, and the
/// bridge subnet (no cage-to-cage) are denied; the public internet and the
/// host DNS forwarder are allowed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EgressMode {
    /// No veth at all: pure compute, no outbound network.
    None,
    /// Deny by default; allow only the listed destinations, plus DNS.
    Restricted { allow: Vec<EgressRule> },
    /// The public internet, with private ranges and metadata denied.
    Public,
    /// Public plus RFC1918, for apps that reach LAN services. Explicit opt-in;
    /// metadata and the bridge subnet stay denied.
    Open,
}

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

/// Overlay root filesystem for a cage.
///
/// The cage assembles an overlayfs from read-only lower directories (base
/// files, engine, artifact) and a size-capped tmpfs upper layer, then pivots
/// into it: the host tree is gone from the cage's view, and everything
/// written at runtime lands in cage-private memory that vanishes at teardown.
/// The Linux backend applies this; the plain-process backend runs on the host
/// filesystem.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RootfsSpec {
    /// Read-only lower directories, top-most first (overlayfs order).
    pub lowerdirs: Vec<PathBuf>,
    /// Byte size cap for the writable tmpfs upper layer.
    pub tmpfs_size: u64,
    /// Host directory holding the per-cage staging mount point. Defaults to
    /// the system temporary directory.
    pub staging_dir: Option<PathBuf>,
}

impl RootfsSpec {
    /// Describe an overlay root with the default tmpfs size cap.
    pub fn new(lowerdirs: Vec<PathBuf>) -> Self {
        Self {
            lowerdirs,
            tmpfs_size: DEFAULT_ROOTFS_TMPFS_SIZE,
            staging_dir: None,
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
    pub rootfs: Option<RootfsSpec>,
    /// Seccomp filter to install in the cage child immediately before `execve`.
    /// `None` boots without a filter. The Linux backend honors this; the
    /// plain-process backend ignores it.
    pub seccomp: Option<FilterMode>,
    /// Egress network policy. The Linux backend wires the veth and nftables;
    /// the plain-process backend ignores it.
    pub egress: EgressMode,
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
            rootfs: None,
            seccomp: None,
            egress: EgressMode::None,
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
        if let Some(rootfs) = &self.rootfs {
            if rootfs.lowerdirs.is_empty() {
                return Err(CageError::InvalidSpec(
                    "rootfs must list at least one lower directory".into(),
                ));
            }
            for lower in &rootfs.lowerdirs {
                validate_overlay_path(lower, "rootfs lower directory")?;
            }
            if rootfs.tmpfs_size == 0 {
                return Err(CageError::InvalidSpec(
                    "rootfs tmpfs_size must be greater than zero".into(),
                ));
            }
            if let Some(staging) = &rootfs.staging_dir {
                validate_overlay_path(staging, "rootfs staging directory")?;
            }
            if !Path::new(&self.command).is_absolute() {
                return Err(CageError::InvalidSpec(
                    "command must be an absolute path when a rootfs is set; it resolves inside \
                     the overlay root"
                        .into(),
                ));
            }
        }
        if let EgressMode::Restricted { allow } = &self.egress {
            for rule in allow {
                validate_cidr(&rule.cidr)?;
            }
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
    /// Veth creation, bridge attach, addressing, and loading the egress
    /// policy. Zero when the cage has no egress network.
    pub network: Duration,
    /// Private propagation, the optional overlay root pivot, and `procfs`.
    pub mounts: Duration,
    /// Seccomp filter compilation is done in the parent; this covers the
    /// child-side install. Zero when no filter is requested.
    pub seccomp: Duration,
    /// Parent release through successful `execve` detection.
    pub exec_runtime_init: Duration,
    /// Successful `execve` through a readiness UDS accepting connections.
    pub socket_ready: Duration,
    pub total: Duration,
}

fn validate_overlay_path(path: &Path, label: &str) -> Result<(), CageError> {
    if !path.is_absolute() {
        return Err(CageError::InvalidSpec(format!("{label} must be absolute")));
    }
    let bytes = path.as_os_str().as_bytes();
    if bytes
        .iter()
        .any(|&byte| matches!(byte, b':' | b',' | b'\\' | 0))
    {
        return Err(CageError::InvalidSpec(format!(
            "{label} must not contain ':', ',', '\\', or NUL bytes; they cannot be expressed in \
             overlay mount options"
        )));
    }
    Ok(())
}

/// Validate an IPv4 CIDR of the form `A.B.C.D/P` with `0 <= P <= 32`.
fn validate_cidr(cidr: &str) -> Result<(), CageError> {
    let Some((address, prefix)) = cidr.split_once('/') else {
        return Err(CageError::InvalidSpec(format!(
            "egress CIDR {cidr:?} must be in A.B.C.D/prefix form"
        )));
    };
    if address.parse::<std::net::Ipv4Addr>().is_err() {
        return Err(CageError::InvalidSpec(format!(
            "egress CIDR {cidr:?} has an invalid IPv4 address"
        )));
    }
    match prefix.parse::<u8>() {
        Ok(bits) if bits <= 32 => Ok(()),
        _ => Err(CageError::InvalidSpec(format!(
            "egress CIDR {cidr:?} prefix must be between 0 and 32"
        ))),
    }
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
    if !chars
        .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.'))
    {
        return Err(CageError::InvalidSpec(
            "name contains unsupported characters".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_specification() {
        let spec = CageSpec::new("example", "/bin/true");

        assert_eq!(spec.limits.memory_max, 256 * 1024 * 1024);
        assert_eq!(spec.limits.memory_high, 224 * 1024 * 1024);
        assert_eq!(spec.limits.cpu_quota, 100_000);
        assert_eq!(spec.limits.cpu_period, 100_000);
        assert_eq!(spec.limits.pids_max, 128);
        assert!(spec.seccomp.is_none());
        assert_eq!(spec.egress, EgressMode::None);
        assert!(spec.validate().is_ok());
    }

    #[test]
    fn validation_checks_restricted_egress_cidrs() {
        let mut spec = CageSpec::new("example", "/bin/true");

        spec.egress = EgressMode::Restricted {
            allow: vec![EgressRule {
                cidr: "203.0.113.0/24".into(),
                ports: vec![443],
            }],
        };
        assert!(spec.validate().is_ok());

        spec.egress = EgressMode::Restricted {
            allow: vec![EgressRule {
                cidr: "203.0.113.0".into(),
                ports: Vec::new(),
            }],
        };
        assert!(spec.validate().is_err(), "accepted a CIDR without a prefix");

        spec.egress = EgressMode::Restricted {
            allow: vec![EgressRule {
                cidr: "203.0.113.0/33".into(),
                ports: Vec::new(),
            }],
        };
        assert!(spec.validate().is_err(), "accepted an out-of-range prefix");

        // Public and Open carry no rules to validate.
        spec.egress = EgressMode::Open;
        assert!(spec.validate().is_ok());
    }

    #[test]
    fn validation_rejects_unsafe_cgroup_names() {
        for name in ["", "../escape", "a/b", "-leading", "app space", "äpp"] {
            let spec = CageSpec::new(name, "/bin/true");
            assert!(spec.validate().is_err(), "accepted cage name {name:?}");
        }
    }

    #[test]
    fn validation_accepts_safe_cgroup_names() {
        for name in ["app", "app-1", "App_2.blue"] {
            let spec = CageSpec::new(name, "/bin/true");
            assert!(spec.validate().is_ok(), "rejected cage name {name:?}");
        }
    }

    #[test]
    fn validation_rejects_inconsistent_limits() {
        let mut spec = CageSpec::new("example", "/bin/true");
        spec.limits.memory_high = spec.limits.memory_max + 1;
        assert!(spec.validate().is_err());

        spec.limits.memory_high = spec.limits.memory_max;
        spec.limits.cpu_quota = 0;
        assert!(spec.validate().is_err());
    }

    #[test]
    fn validation_requires_an_absolute_readiness_path() {
        let mut spec = CageSpec::new("example", "/bin/true");
        spec.readiness_uds = Some(PathBuf::from("app.sock"));
        assert!(spec.validate().is_err());

        spec.readiness_uds = Some(PathBuf::from("/tmp/app.sock"));
        assert!(spec.validate().is_ok());
    }

    #[test]
    fn validation_enforces_overlay_rootfs_constraints() {
        let mut spec = CageSpec::new("example", "/bin/true");

        spec.rootfs = Some(RootfsSpec::new(Vec::new()));
        assert!(spec.validate().is_err(), "accepted an empty lowerdir list");

        spec.rootfs = Some(RootfsSpec::new(vec![PathBuf::from("relative")]));
        assert!(spec.validate().is_err(), "accepted a relative lowerdir");

        spec.rootfs = Some(RootfsSpec::new(vec![PathBuf::from("/lower:dir")]));
        assert!(spec.validate().is_err(), "accepted a lowerdir with ':'");

        let mut rootfs = RootfsSpec::new(vec![PathBuf::from("/lower")]);
        rootfs.tmpfs_size = 0;
        spec.rootfs = Some(rootfs);
        assert!(spec.validate().is_err(), "accepted a zero-size tmpfs");

        let mut rootfs = RootfsSpec::new(vec![PathBuf::from("/lower")]);
        rootfs.staging_dir = Some(PathBuf::from("staging"));
        spec.rootfs = Some(rootfs);
        assert!(spec.validate().is_err(), "accepted a relative staging dir");

        spec.rootfs = Some(RootfsSpec::new(vec![PathBuf::from("/lower")]));
        assert!(spec.validate().is_ok());
    }

    #[test]
    fn validation_requires_an_absolute_command_with_a_rootfs() {
        let mut spec = CageSpec::new("example", "true");
        assert!(
            spec.validate().is_ok(),
            "PATH lookup is fine without a rootfs"
        );

        spec.rootfs = Some(RootfsSpec::new(vec![PathBuf::from("/lower")]));
        assert!(
            spec.validate().is_err(),
            "PATH lookup cannot resolve inside the overlay root"
        );
    }

    #[test]
    fn validation_rejects_invalid_environment_keys() {
        let mut spec = CageSpec::new("example", "/bin/true");
        spec.env
            .insert(OsString::from("BAD=KEY"), OsString::from("value"));
        assert!(spec.validate().is_err());
    }
}
