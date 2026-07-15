use std::collections::BTreeMap;
use std::ffi::OsString;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};
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
/// Fixed directory inside an artifact-rooted cage where the host exposes app ingress.
pub const INGRESS_CAGE_DIR: &str = "/cygnus/io";
/// Fixed read-only directory containing Tenant Zero's typed admin socket.
pub const ADMIN_CAGE_DIR: &str = "/cygnus/admin";
/// Fixed socket name inside [`ADMIN_CAGE_DIR`].
pub const ADMIN_SOCKET_FILENAME: &str = "admin.sock";
/// Fixed writable build-artifact directory exposed inside a rooted job.
pub const BUILD_OUTPUT_CAGE_DIR: &str = "/cygnus/output";

/// Action taken when a cage syscall matches the seccomp denylist.
///
/// Defined here, in the platform-neutral spec, so a `CageSpec` can carry the
/// choice on every host; the filter it drives is compiled and installed only
/// by the Linux backend.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FilterMode {
    /// Fail a blocked syscall with `EPERM`, like Docker's default profile.
    Enforce,
    /// Log a blocked syscall to the kernel audit log and allow it to continue,
    /// so a workload can be observed before enforcing.
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

/// One exact DNS-name allowance used by [`EgressMode::BuildDomains`].
///
/// Domain egress is intentionally limited to build jobs. A successful A
/// lookup populates a short-lived nftables set in that build cage, and only
/// TCP traffic to the listed destination ports may use the resolved address.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DomainEgressRule {
    /// Canonical lowercase DNS name without a trailing root dot or wildcard.
    pub domain: String,
    /// Non-empty, unique TCP destination ports.
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
    /// Build-only exact-domain policy populated dynamically by DNS answers.
    BuildDomains { allow: Vec<DomainEgressRule> },
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

/// Host-side directory mounted at [`INGRESS_CAGE_DIR`] inside a cage.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IngressSpec {
    /// Existing host directory containing this cage's readiness socket.
    pub host_dir: PathBuf,
}

impl IngressSpec {
    /// Describe the host directory to expose for this cage's ingress socket.
    pub fn new(host_dir: impl Into<PathBuf>) -> Self {
        Self {
            host_dir: host_dir.into(),
        }
    }
}

/// Daemon-owned directory mounted read-only at [`ADMIN_CAGE_DIR`].
///
/// This capability is purpose-specific to Tenant Zero. It is deliberately
/// separate from writable ingress and is not a generic host volume.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AdminSocketSpec {
    pub host_dir: PathBuf,
}

impl AdminSocketSpec {
    pub fn new(host_dir: impl Into<PathBuf>) -> Self {
        Self {
            host_dir: host_dir.into(),
        }
    }
}

/// Host-side directory mounted writable at [`BUILD_OUTPUT_CAGE_DIR`] for a
/// finite build job. The mount is intentionally purpose-specific rather than
/// a generic volume: it is the only host path a build can publish to.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BuildOutputSpec {
    pub host_dir: PathBuf,
}

impl BuildOutputSpec {
    pub fn new(host_dir: impl Into<PathBuf>) -> Self {
        Self {
            host_dir: host_dir.into(),
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
    /// Optional host ingress directory mounted at [`INGRESS_CAGE_DIR`] after
    /// the cage pivots into its overlay root.
    pub ingress: Option<IngressSpec>,
    /// Optional daemon admin directory mounted read-only at [`ADMIN_CAGE_DIR`].
    pub admin_socket: Option<AdminSocketSpec>,
    /// Optional host build output directory mounted at the fixed
    /// [`BUILD_OUTPUT_CAGE_DIR`] path. This requires a rootfs.
    pub build_output: Option<BuildOutputSpec>,
    /// Optional absolute directory to use as the target's current directory
    /// after the rootfs is assembled. The path is interpreted inside the cage.
    pub working_dir: Option<PathBuf>,
    /// Seccomp filter to install in the cage child immediately before `execve`.
    /// Defaults to `Some(FilterMode::Enforce)` (see [`CageSpec::new`]), so a
    /// cage is sandboxed out of the box like a Docker container; `None` boots
    /// without a filter. The Linux backend honors this; the plain-process
    /// backend ignores it.
    pub seccomp: Option<FilterMode>,
    /// Egress network policy. The Linux backend wires the veth and nftables;
    /// the plain-process backend ignores it.
    pub egress: EgressMode,
    /// Optional static PID-1 init to exec as the cage's first process, with the
    /// command and its arguments passed through to it. The init reaps orphaned
    /// descendants and forwards signals — the correct behaviour for PID 1.
    /// `None` execs the command directly. The path is exec'd verbatim, so it
    /// must resolve inside the cage's filesystem view. Honored by the Linux
    /// backend; the plain-process backend ignores it.
    pub init: Option<PathBuf>,
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
            ingress: None,
            admin_socket: None,
            build_output: None,
            working_dir: None,
            seccomp: Some(FilterMode::Enforce),
            egress: EgressMode::None,
            init: None,
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
        if let Some(ingress) = &self.ingress {
            if self.rootfs.is_none() {
                return Err(CageError::InvalidSpec("ingress requires a rootfs".into()));
            }
            if !ingress.host_dir.is_absolute() {
                return Err(CageError::InvalidSpec(
                    "ingress host directory must be absolute".into(),
                ));
            }
            if ingress.host_dir == Path::new("/") {
                return Err(CageError::InvalidSpec(
                    "ingress host directory must not be the host root".into(),
                ));
            }
            if ingress.host_dir.as_os_str().as_bytes().contains(&0) {
                return Err(CageError::InvalidSpec(
                    "ingress host directory must not contain a NUL byte".into(),
                ));
            }
            if ingress
                .host_dir
                .components()
                .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
            {
                return Err(CageError::InvalidSpec(
                    "ingress host directory must not contain '.' or '..' components".into(),
                ));
            }
            let Some(readiness_uds) = &self.readiness_uds else {
                return Err(CageError::InvalidSpec(
                    "ingress requires a readiness UDS".into(),
                ));
            };
            if readiness_uds.parent() != Some(ingress.host_dir.as_path()) {
                return Err(CageError::InvalidSpec(
                    "readiness UDS parent must equal ingress host directory".into(),
                ));
            }
        }
        if let Some(admin) = &self.admin_socket {
            if self.rootfs.is_none() {
                return Err(CageError::InvalidSpec(
                    "Tenant admin socket requires a rootfs".into(),
                ));
            }
            validate_host_directory_shape(&admin.host_dir, "Tenant admin host directory")?;
        }
        if let Some(output) = &self.build_output {
            if self.rootfs.is_none() {
                return Err(CageError::InvalidSpec(
                    "build output requires a rootfs".into(),
                ));
            }
            validate_host_directory_shape(&output.host_dir, "build output host directory")?;
        }
        if let Some(working_dir) = &self.working_dir {
            if !working_dir.is_absolute() {
                return Err(CageError::InvalidSpec(
                    "working directory must be absolute and inside the cage".into(),
                ));
            }
            if working_dir
                .components()
                .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
            {
                return Err(CageError::InvalidSpec(
                    "working directory must not contain '.' or '..' components".into(),
                ));
            }
            if working_dir.as_os_str().as_bytes().contains(&0) {
                return Err(CageError::InvalidSpec(
                    "working directory must not contain a NUL byte".into(),
                ));
            }
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
        match &self.egress {
            EgressMode::Restricted { allow } => {
                for rule in allow {
                    validate_cidr(&rule.cidr)?;
                }
            }
            EgressMode::BuildDomains { allow } => {
                for rule in allow {
                    validate_domain(&rule.domain)?;
                    if rule.ports.is_empty() {
                        return Err(CageError::InvalidSpec(format!(
                            "domain egress rule for {:?} must list at least one port",
                            rule.domain
                        )));
                    }
                    let mut unique = std::collections::BTreeSet::new();
                    if rule
                        .ports
                        .iter()
                        .any(|port| *port == 0 || !unique.insert(*port))
                    {
                        return Err(CageError::InvalidSpec(format!(
                            "domain egress ports for {:?} must be nonzero and unique",
                            rule.domain
                        )));
                    }
                }
            }
            EgressMode::None | EgressMode::Public | EgressMode::Open => {}
        }
        if let Some(init) = &self.init
            && !init.is_absolute()
        {
            return Err(CageError::InvalidSpec(
                "init path must be absolute; it is exec'd inside the cage".into(),
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

fn validate_host_directory_shape(path: &Path, label: &str) -> Result<(), CageError> {
    if !path.is_absolute() {
        return Err(CageError::InvalidSpec(format!("{label} must be absolute")));
    }
    if path == Path::new("/") {
        return Err(CageError::InvalidSpec(format!(
            "{label} must not be the host root"
        )));
    }
    if path.as_os_str().as_bytes().contains(&0) {
        return Err(CageError::InvalidSpec(format!(
            "{label} must not contain a NUL byte"
        )));
    }
    if path
        .components()
        .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
    {
        return Err(CageError::InvalidSpec(format!(
            "{label} must not contain '.' or '..' components"
        )));
    }
    Ok(())
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

/// Validate a canonical exact host name as stored in a domain egress rule.
fn validate_domain(domain: &str) -> Result<(), CageError> {
    if domain.is_empty() || domain.len() > 253 {
        return Err(CageError::InvalidSpec(format!(
            "domain egress name {domain:?} must contain 1 to 253 bytes"
        )));
    }
    for label in domain.split('.') {
        if label.is_empty() || label.len() > 63 {
            return Err(CageError::InvalidSpec(format!(
                "domain egress name {domain:?} contains an empty or overlong label"
            )));
        }
        let bytes = label.as_bytes();
        if !bytes[0].is_ascii_lowercase() && !bytes[0].is_ascii_digit()
            || !bytes[label.len() - 1].is_ascii_lowercase()
                && !bytes[label.len() - 1].is_ascii_digit()
            || !bytes
                .iter()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
        {
            return Err(CageError::InvalidSpec(format!(
                "domain egress name {domain:?} is not a canonical lowercase host name"
            )));
        }
    }
    Ok(())
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
    use std::os::unix::ffi::OsStringExt;

    #[test]
    fn defaults_match_the_specification() {
        let spec = CageSpec::new("example", "/bin/true");

        assert_eq!(spec.limits.memory_max, 256 * 1024 * 1024);
        assert_eq!(spec.limits.memory_high, 224 * 1024 * 1024);
        assert_eq!(spec.limits.cpu_quota, 100_000);
        assert_eq!(spec.limits.cpu_period, 100_000);
        assert_eq!(spec.limits.pids_max, 128);
        // Sandboxed out of the box, like a Docker container.
        assert_eq!(spec.seccomp, Some(FilterMode::Enforce));
        assert_eq!(spec.egress, EgressMode::None);
        assert!(spec.ingress.is_none());
        assert!(spec.admin_socket.is_none());
        assert!(spec.init.is_none());
        assert!(spec.validate().is_ok());
    }

    #[test]
    fn validation_requires_an_absolute_init_path() {
        let mut spec = CageSpec::new("example", "/bin/true");
        spec.init = Some(PathBuf::from("cygnus-init"));
        assert!(spec.validate().is_err(), "accepted a relative init path");

        spec.init = Some(PathBuf::from("/usr/bin/cygnus-init"));
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
    fn validation_accepts_canonical_build_domains_with_unique_ports() {
        let mut spec = CageSpec::new("builder", "/bin/true");
        spec.egress = EgressMode::BuildDomains {
            allow: vec![
                DomainEgressRule {
                    domain: "registry.npmjs.org".into(),
                    ports: vec![443],
                },
                DomainEgressRule {
                    domain: "cache-1.example.com".into(),
                    ports: vec![80, 443],
                },
            ],
        };

        assert!(spec.validate().is_ok());
    }

    #[test]
    fn validation_rejects_noncanonical_or_wildcard_build_domains() {
        for domain in [
            "Registry.npmjs.org",
            "registry.npmjs.org.",
            "*.npmjs.org",
            ".npmjs.org",
            "npmjs..org",
            "-registry.npmjs.org",
            "registry-.npmjs.org",
            "registry_npmjs.org",
            "éxample.org",
        ] {
            let mut spec = CageSpec::new("builder", "/bin/true");
            spec.egress = EgressMode::BuildDomains {
                allow: vec![DomainEgressRule {
                    domain: domain.into(),
                    ports: vec![443],
                }],
            };
            assert!(
                spec.validate().is_err(),
                "accepted noncanonical build domain {domain:?}"
            );
        }
    }

    #[test]
    fn validation_rejects_empty_duplicate_or_zero_build_ports() {
        for ports in [vec![], vec![443, 443], vec![0, 443]] {
            let mut spec = CageSpec::new("builder", "/bin/true");
            spec.egress = EgressMode::BuildDomains {
                allow: vec![DomainEgressRule {
                    domain: "registry.npmjs.org".into(),
                    ports,
                }],
            };
            assert!(spec.validate().is_err());
        }
    }

    #[test]
    fn validation_rejects_overlong_domain_labels_and_names() {
        let label = "a".repeat(64);
        let name = [
            "a".repeat(63),
            "b".repeat(63),
            "c".repeat(63),
            "d".repeat(62),
        ]
        .join(".");
        for domain in [format!("{label}.example"), name] {
            let mut spec = CageSpec::new("builder", "/bin/true");
            spec.egress = EgressMode::BuildDomains {
                allow: vec![DomainEgressRule {
                    domain,
                    ports: vec![443],
                }],
            };
            assert!(spec.validate().is_err());
        }
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
    fn validation_enforces_ingress_shape() {
        let mut spec = CageSpec::new("example", "/bin/true");
        let host_dir = PathBuf::from("/tmp/cygnus-ingress");
        let readiness = host_dir.join("app.sock");

        spec.ingress = Some(IngressSpec::new(host_dir.clone()));
        assert!(
            spec.validate().is_err(),
            "accepted ingress without a rootfs"
        );

        spec.rootfs = Some(RootfsSpec::new(vec![PathBuf::from("/lower")]));
        assert!(
            spec.validate().is_err(),
            "accepted ingress without readiness UDS"
        );

        spec.readiness_uds = Some(PathBuf::from("app.sock"));
        assert!(spec.validate().is_err(), "accepted relative readiness UDS");

        spec.readiness_uds = Some(PathBuf::from("/tmp/other/app.sock"));
        assert!(
            spec.validate().is_err(),
            "accepted readiness UDS from another directory"
        );

        spec.readiness_uds = Some(readiness);
        assert!(spec.validate().is_ok());

        spec.ingress = Some(IngressSpec::new("relative"));
        assert!(
            spec.validate().is_err(),
            "accepted relative ingress host directory"
        );

        spec.ingress = Some(IngressSpec::new("/"));
        assert!(
            spec.validate().is_err(),
            "accepted host root as ingress directory"
        );

        let traversing = PathBuf::from("/tmp/../etc");
        spec.readiness_uds = Some(traversing.join("app.sock"));
        spec.ingress = Some(IngressSpec::new(traversing));
        assert!(spec.validate().is_err(), "accepted ingress path traversal");
    }

    #[test]
    fn validation_rejects_nul_in_ingress_host_directory() {
        let mut spec = CageSpec::new("example", "/bin/true");
        spec.rootfs = Some(RootfsSpec::new(vec![PathBuf::from("/lower")]));
        spec.readiness_uds = Some(PathBuf::from("/tmp/app.sock"));
        spec.ingress = Some(IngressSpec::new(PathBuf::from(OsString::from_vec(
            b"/tmp/with\0nul".to_vec(),
        ))));
        assert!(spec.validate().is_err());
    }

    #[test]
    fn validation_enforces_tenant_admin_mount_shape() {
        let mut spec = CageSpec::new("tenant-0", "/bin/true");
        spec.admin_socket = Some(AdminSocketSpec::new("/run/cygnus/tenant0-admin"));
        assert!(spec.validate().is_err(), "admin socket requires a rootfs");

        spec.rootfs = Some(RootfsSpec::new(vec![PathBuf::from("/lower")]));
        assert!(spec.validate().is_ok());

        spec.admin_socket = Some(AdminSocketSpec::new("relative"));
        assert!(spec.validate().is_err());
        spec.admin_socket = Some(AdminSocketSpec::new("/"));
        assert!(spec.validate().is_err());
        spec.admin_socket = Some(AdminSocketSpec::new("/tmp/../etc"));
        assert!(spec.validate().is_err());
    }

    #[test]
    fn validation_enforces_build_output_and_working_directory_shape() {
        let mut spec = CageSpec::new("example", "/bin/true");
        spec.build_output = Some(BuildOutputSpec::new("/tmp/output"));
        assert!(spec.validate().is_err(), "output requires a rootfs");

        spec.rootfs = Some(RootfsSpec::new(vec![PathBuf::from("/lower")]));
        assert!(spec.validate().is_ok());

        spec.build_output = Some(BuildOutputSpec::new("relative"));
        assert!(
            spec.validate().is_err(),
            "accepted relative output directory"
        );
        spec.build_output = Some(BuildOutputSpec::new("/"));
        assert!(
            spec.validate().is_err(),
            "accepted host root as output directory"
        );
        spec.build_output = Some(BuildOutputSpec::new("/tmp/../output"));
        assert!(spec.validate().is_err(), "accepted output path traversal");
        spec.build_output = Some(BuildOutputSpec::new("/tmp/output"));

        spec.working_dir = Some(PathBuf::from("relative"));
        assert!(
            spec.validate().is_err(),
            "accepted relative working directory"
        );
        spec.working_dir = Some(PathBuf::from("/tmp/../workspace"));
        assert!(
            spec.validate().is_err(),
            "accepted working-directory traversal"
        );
        spec.working_dir = Some(PathBuf::from("/workspace"));
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
