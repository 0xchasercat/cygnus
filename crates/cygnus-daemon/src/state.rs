use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use cygnus_cage::{
    CageError, CageSpec, CgroupLimits, DEFAULT_READINESS_TIMEOUT, EgressMode,
    EgressRule as CageEgressRule, FilterMode, IngressSpec, RootfsSpec,
};
use cygnus_router::normalize_host;
use cygnus_supervisor::{
    DEFAULT_BACKOFF_BASE, DEFAULT_BACKOFF_MAX, DEFAULT_CRASH_LOOP_THRESHOLD, DEFAULT_CRASH_WINDOW,
    DEFAULT_IDLE_TTL, LifecycleConfig,
};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Default on-disk database used by the daemon binary.
pub const DEFAULT_STATE_PATH: &str = "/var/lib/cygnus/state.db";
const SCHEMA_VERSION: i32 = 1;
const BUSY_TIMEOUT_MS: u64 = 5_000;

/// The JSON document accepted by the daemon's apply operation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct NodeConfig {
    pub listen: SocketAddr,
    #[serde(default)]
    pub apps: Vec<AppConfig>,
}

impl Default for NodeConfig {
    fn default() -> Self {
        Self {
            listen: SocketAddr::from(([127, 0, 0, 1], 3000)),
            apps: Vec::new(),
        }
    }
}

/// One app in a [`NodeConfig`]. Durations are represented as milliseconds in
/// JSON so that the input remains straightforward and unambiguous.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AppConfig {
    pub name: String,
    #[serde(default)]
    pub domains: Vec<String>,
    pub upstream: PathBuf,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub limits: LimitsConfig,
    #[serde(default)]
    pub rootfs: Option<RootfsConfig>,
    /// Omitted means the default enforcing filter; explicit JSON `null`
    /// disables seccomp for this app.
    #[serde(default = "default_seccomp")]
    pub seccomp: Option<SeccompMode>,
    #[serde(default)]
    pub egress: EgressConfig,
    #[serde(default)]
    pub init: Option<PathBuf>,
    #[serde(default = "default_readiness_timeout_ms")]
    pub readiness_timeout_ms: u64,
    #[serde(default)]
    pub lifecycle: LifecyclePolicy,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            domains: Vec::new(),
            upstream: PathBuf::new(),
            command: String::new(),
            args: Vec::new(),
            env: BTreeMap::new(),
            limits: LimitsConfig::default(),
            rootfs: None,
            seccomp: default_seccomp(),
            egress: EgressConfig::default(),
            init: None,
            readiness_timeout_ms: default_readiness_timeout_ms(),
            lifecycle: LifecyclePolicy::default(),
        }
    }
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LimitsConfig {
    #[serde(default = "default_memory_max")]
    pub memory_max: u64,
    #[serde(default = "default_memory_high")]
    pub memory_high: u64,
    #[serde(default = "default_cpu_quota")]
    pub cpu_quota: u64,
    #[serde(default = "default_cpu_period")]
    pub cpu_period: u64,
    #[serde(default = "default_pids_max")]
    pub pids_max: u32,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        let limits = CgroupLimits::default();
        Self {
            memory_max: limits.memory_max,
            memory_high: limits.memory_high,
            cpu_quota: limits.cpu_quota,
            cpu_period: limits.cpu_period,
            pids_max: limits.pids_max,
        }
    }
}

impl From<LimitsConfig> for CgroupLimits {
    fn from(limits: LimitsConfig) -> Self {
        Self {
            memory_max: limits.memory_max,
            memory_high: limits.memory_high,
            cpu_quota: limits.cpu_quota,
            cpu_period: limits.cpu_period,
            pids_max: limits.pids_max,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RootfsConfig {
    #[serde(default)]
    pub lowerdirs: Vec<PathBuf>,
    #[serde(default = "default_rootfs_tmpfs_size")]
    pub tmpfs_size: u64,
    #[serde(default)]
    pub staging_dir: Option<PathBuf>,
}

impl Default for RootfsConfig {
    fn default() -> Self {
        Self {
            lowerdirs: Vec::new(),
            tmpfs_size: cygnus_cage::DEFAULT_ROOTFS_TMPFS_SIZE,
            staging_dir: None,
        }
    }
}

impl From<RootfsConfig> for RootfsSpec {
    fn from(rootfs: RootfsConfig) -> Self {
        Self {
            lowerdirs: rootfs.lowerdirs,
            tmpfs_size: rootfs.tmpfs_size,
            staging_dir: rootfs.staging_dir,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SeccompMode {
    Enforce,
    Audit,
}

impl From<SeccompMode> for FilterMode {
    fn from(mode: SeccompMode) -> Self {
        match mode {
            SeccompMode::Enforce => Self::Enforce,
            SeccompMode::Audit => Self::Audit,
        }
    }
}

fn default_seccomp() -> Option<SeccompMode> {
    Some(SeccompMode::Enforce)
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct EgressRuleConfig {
    pub cidr: String,
    #[serde(default)]
    pub ports: Vec<u16>,
}

/// A restricted egress allowance in the JSON DTO.
pub type EgressRule = EgressRuleConfig;

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "mode", rename_all = "lowercase")]
pub enum EgressConfig {
    #[default]
    None,
    Restricted {
        #[serde(default)]
        allow: Vec<EgressRuleConfig>,
    },
    Public,
    Open,
}

impl From<EgressConfig> for EgressMode {
    fn from(mode: EgressConfig) -> Self {
        match mode {
            EgressConfig::None => Self::None,
            EgressConfig::Public => Self::Public,
            EgressConfig::Open => Self::Open,
            EgressConfig::Restricted { allow } => Self::Restricted {
                allow: allow
                    .into_iter()
                    .map(|rule| CageEgressRule {
                        cidr: rule.cidr,
                        ports: rule.ports,
                    })
                    .collect(),
            },
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LifecyclePolicy {
    #[serde(default = "default_idle_ttl_ms")]
    pub idle_ttl_ms: u64,
    #[serde(default)]
    pub min_instances: u32,
    #[serde(default = "default_backoff_base_ms")]
    pub backoff_base_ms: u64,
    #[serde(default = "default_backoff_max_ms")]
    pub backoff_max_ms: u64,
    #[serde(default = "default_crash_window_ms")]
    pub crash_window_ms: u64,
    #[serde(default = "default_crash_loop_threshold")]
    pub crash_loop_threshold: u32,
}

impl Default for LifecyclePolicy {
    fn default() -> Self {
        Self {
            idle_ttl_ms: duration_millis(DEFAULT_IDLE_TTL),
            min_instances: 0,
            backoff_base_ms: duration_millis(DEFAULT_BACKOFF_BASE),
            backoff_max_ms: duration_millis(DEFAULT_BACKOFF_MAX),
            crash_window_ms: duration_millis(DEFAULT_CRASH_WINDOW),
            crash_loop_threshold: DEFAULT_CRASH_LOOP_THRESHOLD,
        }
    }
}

impl From<LifecyclePolicy> for LifecycleConfig {
    fn from(policy: LifecyclePolicy) -> Self {
        Self {
            idle_ttl: Duration::from_millis(policy.idle_ttl_ms),
            min_instances: policy.min_instances,
            backoff_base: Duration::from_millis(policy.backoff_base_ms),
            backoff_max: Duration::from_millis(policy.backoff_max_ms),
            crash_window: Duration::from_millis(policy.crash_window_ms),
            crash_loop_threshold: policy.crash_loop_threshold,
        }
    }
}

fn duration_millis(duration: Duration) -> u64 {
    duration.as_millis().try_into().unwrap_or(u64::MAX)
}
fn default_memory_max() -> u64 {
    CgroupLimits::default().memory_max
}
fn default_memory_high() -> u64 {
    CgroupLimits::default().memory_high
}
fn default_cpu_quota() -> u64 {
    CgroupLimits::default().cpu_quota
}
fn default_cpu_period() -> u64 {
    CgroupLimits::default().cpu_period
}
fn default_pids_max() -> u32 {
    CgroupLimits::default().pids_max
}
fn default_rootfs_tmpfs_size() -> u64 {
    cygnus_cage::DEFAULT_ROOTFS_TMPFS_SIZE
}
fn default_readiness_timeout_ms() -> u64 {
    duration_millis(DEFAULT_READINESS_TIMEOUT)
}
fn default_idle_ttl_ms() -> u64 {
    duration_millis(DEFAULT_IDLE_TTL)
}
fn default_backoff_base_ms() -> u64 {
    duration_millis(DEFAULT_BACKOFF_BASE)
}
fn default_backoff_max_ms() -> u64 {
    duration_millis(DEFAULT_BACKOFF_MAX)
}
fn default_crash_window_ms() -> u64 {
    duration_millis(DEFAULT_CRASH_WINDOW)
}
fn default_crash_loop_threshold() -> u32 {
    DEFAULT_CRASH_LOOP_THRESHOLD
}

/// A validated, deterministic view of the complete node configuration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Snapshot {
    pub listen: SocketAddr,
    pub apps: Vec<LoadedApp>,
}

/// One app projected into the cage and supervisor APIs.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LoadedApp {
    pub name: String,
    pub domains: Vec<String>,
    pub upstream: PathBuf,
    pub spec: CageSpec,
    pub lifecycle: LifecycleConfig,
}

/// Durable state and configuration errors.
#[derive(Debug, Error)]
pub enum StateError {
    #[error("SQLite state error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("state filesystem error: {0}")]
    Io(#[from] std::io::Error),
    #[error("state schema version {found} is unsupported (expected {expected})")]
    UnknownSchemaVersion { found: i32, expected: i32 },
    #[error("configuration for app {app:?} has an invalid cage specification: {source}")]
    InvalidSpec {
        app: String,
        #[source]
        source: CageError,
    },
    #[error("invalid configuration: {0}")]
    InvalidConfig(String),
    #[error("persisted app {app:?} contains invalid configuration: {detail}")]
    InvalidPersisted { app: String, detail: String },
    #[error("persisted state is incomplete: {0}")]
    IncompleteState(String),
    #[error("invalid persisted JSON for app {app:?}: {source}")]
    PersistedJson {
        app: String,
        #[source]
        source: serde_json::Error,
    },
}

/// A SQLite-backed node configuration store.
pub struct State {
    connection: Connection,
}

impl State {
    /// Open or create a state database, applying the v1 connection invariants.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StateError> {
        let path = path.as_ref();
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent)?;
        }
        let connection = Connection::open(path)?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "FULL")?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        connection.busy_timeout(Duration::from_millis(BUSY_TIMEOUT_MS))?;

        let version: i32 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if !(0..=SCHEMA_VERSION).contains(&version) {
            return Err(StateError::UnknownSchemaVersion {
                found: version,
                expected: SCHEMA_VERSION,
            });
        }
        if version == 0 {
            create_schema(&connection)?;
            connection.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        }
        Ok(Self { connection })
    }

    /// Validate and atomically replace the complete persisted configuration.
    pub fn apply(&mut self, config: &NodeConfig) -> Result<(), StateError> {
        let snapshot = snapshot_from_config(config)?;
        let stored = snapshot_to_stored(&snapshot)?;
        let transaction = self.connection.transaction()?;
        replace_database(&transaction, &stored)?;
        transaction.commit()?;
        Ok(())
    }

    /// Load the current configuration from SQLite and revalidate it.
    pub fn load(&self) -> Result<Snapshot, StateError> {
        let version: i32 = self
            .connection
            .pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version != SCHEMA_VERSION {
            return Err(StateError::UnknownSchemaVersion {
                found: version,
                expected: SCHEMA_VERSION,
            });
        }

        let listen = self
            .connection
            .query_row("SELECT listen FROM node_config WHERE id = 1", [], |row| {
                row.get::<_, String>(0)
            })
            .optional()?
            .ok_or_else(|| StateError::IncompleteState("singleton node config is missing".into()))?
            .parse::<SocketAddr>()
            .map_err(|error| StateError::InvalidPersisted {
                app: "<node>".into(),
                detail: format!("invalid listen address: {error}"),
            })?;

        let mut statement = self.connection.prepare(
            "SELECT id, name, upstream, runtime_json FROM apps ORDER BY name COLLATE BINARY ASC",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?;
        let mut apps = Vec::new();
        for row in rows {
            let (id, name, upstream, runtime_json) = row?;
            let stored: StoredAppJson = serde_json::from_str(&runtime_json).map_err(|source| {
                StateError::PersistedJson {
                    app: name.clone(),
                    source,
                }
            })?;
            let runtime = stored.runtime;
            let domains = self.load_domains(id, &name)?;
            apps.push(loaded_from_stored(&name, &upstream, domains, runtime)?);
        }
        let snapshot = Snapshot { listen, apps };
        validate_snapshot(&snapshot)?;
        Ok(snapshot)
    }

    fn load_domains(&self, app_id: i64, app: &str) -> Result<Vec<String>, StateError> {
        let mut statement = self.connection.prepare(
            "SELECT domain FROM domains WHERE app_id = ?1 ORDER BY domain COLLATE BINARY ASC",
        )?;
        let rows = statement.query_map([app_id], |row| row.get::<_, String>(0))?;
        let mut domains = Vec::new();
        for row in rows {
            let domain = row?;
            if canonical_domain(&domain).as_deref() != Some(domain.as_str()) {
                return Err(StateError::InvalidPersisted {
                    app: app.to_owned(),
                    detail: format!("domain {domain:?} is not canonical"),
                });
            }
            domains.push(domain);
        }
        Ok(domains)
    }
}

fn create_schema(connection: &Connection) -> Result<(), rusqlite::Error> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS node_config (
             id INTEGER PRIMARY KEY CHECK (id = 1),
             listen TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS apps (
             id INTEGER PRIMARY KEY,
             name TEXT NOT NULL UNIQUE,
             upstream TEXT NOT NULL UNIQUE,
             runtime_json TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS domains (
             id INTEGER PRIMARY KEY,
             app_id INTEGER NOT NULL REFERENCES apps(id) ON DELETE CASCADE,
             domain TEXT NOT NULL COLLATE BINARY UNIQUE
         );
         CREATE INDEX IF NOT EXISTS domains_app_id ON domains(app_id);
         INSERT OR IGNORE INTO node_config (id, listen) VALUES (1, '127.0.0.1:3000');",
    )
}

#[derive(Clone, Debug, Serialize)]
struct StoredApp<'a> {
    name: &'a str,
    upstream: &'a str,
    /// Domains are deliberately empty here; SQL owns and reattaches them.
    domains: &'static [String],
    runtime: &'a StoredRuntime,
}

#[derive(Clone, Debug, Deserialize)]
struct StoredAppJson {
    #[allow(dead_code)]
    name: String,
    #[allow(dead_code)]
    upstream: String,
    #[allow(dead_code)]
    domains: Vec<String>,
    runtime: StoredRuntime,
}

#[derive(Clone, Debug)]
struct StoredSnapshot {
    listen: SocketAddr,
    apps: Vec<StoredAppOwned>,
}

#[derive(Clone, Debug)]
struct StoredAppOwned {
    name: String,
    upstream: String,
    domains: Vec<String>,
    runtime: StoredRuntime,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct StoredRuntime {
    command: String,
    args: Vec<String>,
    env: BTreeMap<String, String>,
    limits: StoredLimits,
    rootfs: Option<StoredRootfs>,
    seccomp: Option<StoredSeccomp>,
    egress: StoredEgress,
    init: Option<String>,
    readiness_timeout_ms: u64,
    idle_ttl_ms: u64,
    min_instances: u32,
    backoff_base_ms: u64,
    backoff_max_ms: u64,
    crash_window_ms: u64,
    crash_loop_threshold: u32,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct StoredLimits {
    memory_max: u64,
    memory_high: u64,
    cpu_quota: u64,
    cpu_period: u64,
    pids_max: u32,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct StoredRootfs {
    lowerdirs: Vec<String>,
    tmpfs_size: u64,
    staging_dir: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
enum StoredSeccomp {
    Enforce,
    Audit,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct StoredEgressRule {
    cidr: String,
    ports: Vec<u16>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "mode", rename_all = "lowercase")]
enum StoredEgress {
    None,
    Public,
    Open,
    Restricted { allow: Vec<StoredEgressRule> },
}

impl From<FilterMode> for StoredSeccomp {
    fn from(mode: FilterMode) -> Self {
        match mode {
            FilterMode::Enforce => Self::Enforce,
            FilterMode::Audit => Self::Audit,
        }
    }
}

impl From<StoredSeccomp> for FilterMode {
    fn from(mode: StoredSeccomp) -> Self {
        match mode {
            StoredSeccomp::Enforce => Self::Enforce,
            StoredSeccomp::Audit => Self::Audit,
        }
    }
}

impl From<&EgressMode> for StoredEgress {
    fn from(mode: &EgressMode) -> Self {
        match mode {
            EgressMode::None => Self::None,
            EgressMode::Public => Self::Public,
            EgressMode::Open => Self::Open,
            EgressMode::Restricted { allow } => Self::Restricted {
                allow: allow
                    .iter()
                    .map(|rule| StoredEgressRule {
                        cidr: rule.cidr.clone(),
                        ports: rule.ports.clone(),
                    })
                    .collect(),
            },
        }
    }
}

impl From<StoredEgress> for EgressMode {
    fn from(mode: StoredEgress) -> Self {
        match mode {
            StoredEgress::None => Self::None,
            StoredEgress::Public => Self::Public,
            StoredEgress::Open => Self::Open,
            StoredEgress::Restricted { allow } => Self::Restricted {
                allow: allow
                    .into_iter()
                    .map(|rule| CageEgressRule {
                        cidr: rule.cidr,
                        ports: rule.ports,
                    })
                    .collect(),
            },
        }
    }
}

fn snapshot_to_stored(snapshot: &Snapshot) -> Result<StoredSnapshot, StateError> {
    let apps = snapshot
        .apps
        .iter()
        .map(|app| {
            let runtime = StoredRuntime::from_app(app)?;
            Ok(StoredAppOwned {
                name: app.name.clone(),
                upstream: app.upstream.to_string_lossy().into_owned(),
                domains: app.domains.clone(),
                runtime,
            })
        })
        .collect::<Result<Vec<_>, StateError>>()?;
    Ok(StoredSnapshot {
        listen: snapshot.listen,
        apps,
    })
}

impl StoredRuntime {
    fn from_app(app: &LoadedApp) -> Result<Self, StateError> {
        let command = app.spec.command.to_string_lossy().into_owned();
        let args = app
            .spec
            .args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        let env = app
            .spec
            .env
            .iter()
            .map(|(key, value)| {
                (
                    key.to_string_lossy().into_owned(),
                    value.to_string_lossy().into_owned(),
                )
            })
            .collect();
        let rootfs = app.spec.rootfs.as_ref().map(|rootfs| StoredRootfs {
            lowerdirs: rootfs
                .lowerdirs
                .iter()
                .map(|path| path.to_string_lossy().into_owned())
                .collect(),
            tmpfs_size: rootfs.tmpfs_size,
            staging_dir: rootfs
                .staging_dir
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned()),
        });
        Ok(Self {
            command,
            args,
            env,
            limits: StoredLimits {
                memory_max: app.spec.limits.memory_max,
                memory_high: app.spec.limits.memory_high,
                cpu_quota: app.spec.limits.cpu_quota,
                cpu_period: app.spec.limits.cpu_period,
                pids_max: app.spec.limits.pids_max,
            },
            rootfs,
            seccomp: app.spec.seccomp.map(StoredSeccomp::from),
            egress: StoredEgress::from(&app.spec.egress),
            init: app
                .spec
                .init
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned()),
            readiness_timeout_ms: duration_millis(app.spec.readiness_timeout),
            idle_ttl_ms: duration_millis(app.lifecycle.idle_ttl),
            min_instances: app.lifecycle.min_instances,
            backoff_base_ms: duration_millis(app.lifecycle.backoff_base),
            backoff_max_ms: duration_millis(app.lifecycle.backoff_max),
            crash_window_ms: duration_millis(app.lifecycle.crash_window),
            crash_loop_threshold: app.lifecycle.crash_loop_threshold,
        })
    }
}

fn ingress_for(
    rootfs: Option<&RootfsSpec>,
    upstream: &Path,
) -> Result<Option<IngressSpec>, StateError> {
    if rootfs.is_none() {
        return Ok(None);
    }
    let host_dir = upstream.parent().ok_or_else(|| {
        StateError::InvalidConfig(format!(
            "overlay-rooted upstream {} has no parent directory",
            upstream.display()
        ))
    })?;
    Ok(Some(IngressSpec::new(host_dir)))
}

fn loaded_from_stored(
    name: &str,
    upstream: &str,
    domains: Vec<String>,
    runtime: StoredRuntime,
) -> Result<LoadedApp, StateError> {
    let upstream_path = PathBuf::from(upstream);
    let mut spec = CageSpec::new(name, runtime.command);
    spec.args = runtime.args.into_iter().map(OsString::from).collect();
    spec.env = runtime
        .env
        .into_iter()
        .map(|(key, value)| (OsString::from(key), OsString::from(value)))
        .collect();
    spec.limits = CgroupLimits {
        memory_max: runtime.limits.memory_max,
        memory_high: runtime.limits.memory_high,
        cpu_quota: runtime.limits.cpu_quota,
        cpu_period: runtime.limits.cpu_period,
        pids_max: runtime.limits.pids_max,
    };
    spec.rootfs = runtime.rootfs.map(|rootfs| RootfsSpec {
        lowerdirs: rootfs.lowerdirs.into_iter().map(PathBuf::from).collect(),
        tmpfs_size: rootfs.tmpfs_size,
        staging_dir: rootfs.staging_dir.map(PathBuf::from),
    });
    spec.ingress = ingress_for(spec.rootfs.as_ref(), &upstream_path)?;
    spec.seccomp = runtime.seccomp.map(FilterMode::from);
    spec.egress = runtime.egress.into();
    spec.init = runtime.init.map(PathBuf::from);
    spec.readiness_uds = Some(upstream_path.clone());
    spec.readiness_timeout = Duration::from_millis(runtime.readiness_timeout_ms);
    let lifecycle = LifecycleConfig {
        idle_ttl: Duration::from_millis(runtime.idle_ttl_ms),
        min_instances: runtime.min_instances,
        backoff_base: Duration::from_millis(runtime.backoff_base_ms),
        backoff_max: Duration::from_millis(runtime.backoff_max_ms),
        crash_window: Duration::from_millis(runtime.crash_window_ms),
        crash_loop_threshold: runtime.crash_loop_threshold,
    };
    Ok(LoadedApp {
        name: name.to_owned(),
        domains,
        upstream: upstream_path,
        spec,
        lifecycle,
    })
}
fn replace_database(
    transaction: &Transaction<'_>,
    snapshot: &StoredSnapshot,
) -> Result<(), StateError> {
    transaction.execute("DELETE FROM node_config", [])?;
    transaction.execute("DELETE FROM apps", [])?;
    transaction.execute(
        "INSERT INTO node_config (id, listen) VALUES (1, ?1)",
        [snapshot.listen.to_string()],
    )?;
    for app in &snapshot.apps {
        let runtime_json = serde_json::to_string(&StoredApp {
            name: &app.name,
            upstream: &app.upstream,
            domains: &[],
            runtime: &app.runtime,
        })
        .map_err(|error| {
            StateError::InvalidConfig(format!("serialize app {:?}: {error}", app.name))
        })?;
        let app_id = transaction.query_row(
            "INSERT INTO apps (name, upstream, runtime_json) VALUES (?1, ?2, ?3) RETURNING id",
            params![app.name, app.upstream, runtime_json],
            |row| row.get::<_, i64>(0),
        )?;
        for domain in &app.domains {
            transaction.execute(
                "INSERT INTO domains (app_id, domain) VALUES (?1, ?2)",
                params![app_id, domain],
            )?;
        }
    }
    Ok(())
}

fn snapshot_from_config(config: &NodeConfig) -> Result<Snapshot, StateError> {
    let mut apps = Vec::with_capacity(config.apps.len());
    for input in &config.apps {
        let mut spec = CageSpec::new(&input.name, &input.command);
        spec.args = input.args.iter().cloned().map(OsString::from).collect();
        spec.env = input
            .env
            .iter()
            .map(|(key, value)| (OsString::from(key), OsString::from(value)))
            .collect();
        spec.limits = input.limits.clone().into();
        spec.rootfs = input.rootfs.clone().map(Into::into);
        spec.ingress = ingress_for(spec.rootfs.as_ref(), &input.upstream)?;
        spec.seccomp = input.seccomp.map(Into::into);
        spec.egress = input.egress.clone().into();
        spec.init = input.init.clone();
        spec.readiness_uds = Some(input.upstream.clone());
        spec.readiness_timeout = Duration::from_millis(input.readiness_timeout_ms);
        spec.validate().map_err(|source| StateError::InvalidSpec {
            app: input.name.clone(),
            source,
        })?;

        let lifecycle = input.lifecycle.clone().into();
        apps.push(LoadedApp {
            name: input.name.clone(),
            domains: canonical_domains(&input.domains)?,
            upstream: input.upstream.clone(),
            spec,
            lifecycle,
        });
    }
    let snapshot = Snapshot {
        listen: config.listen,
        apps,
    };
    validate_snapshot(&snapshot)?;
    Ok(sort_snapshot(snapshot))
}

fn validate_snapshot(snapshot: &Snapshot) -> Result<(), StateError> {
    let mut names = BTreeSet::new();
    let mut upstreams = BTreeSet::new();
    let mut domains = BTreeSet::new();
    for app in &snapshot.apps {
        if !names.insert(app.name.clone()) {
            return Err(StateError::InvalidConfig(format!(
                "duplicate app name {:?}",
                app.name
            )));
        }
        let upstream = app.upstream.to_string_lossy().into_owned();
        if !upstreams.insert(upstream) {
            return Err(StateError::InvalidConfig(format!(
                "duplicate upstream for app {:?}",
                app.name
            )));
        }
        app.spec
            .validate()
            .map_err(|source| StateError::InvalidSpec {
                app: app.name.clone(),
                source,
            })?;
        let lifecycle = &app.lifecycle;
        if lifecycle.idle_ttl.is_zero()
            || lifecycle.backoff_base.is_zero()
            || lifecycle.backoff_max.is_zero()
            || lifecycle.crash_window.is_zero()
        {
            return Err(StateError::InvalidConfig(format!(
                "app {:?} lifecycle durations must be positive",
                app.name
            )));
        }
        if lifecycle.crash_loop_threshold == 0 {
            return Err(StateError::InvalidConfig(format!(
                "app {:?} crash_loop_threshold must be positive",
                app.name
            )));
        }
        if lifecycle.min_instances > 1 {
            return Err(StateError::InvalidConfig(format!(
                "app {:?} min_instances must be 0 or 1",
                app.name
            )));
        }
        for domain in &app.domains {
            if canonical_domain(domain).as_deref() != Some(domain.as_str()) {
                return Err(StateError::InvalidConfig(format!(
                    "app {:?} domain {:?} is not canonical",
                    app.name, domain
                )));
            }
            if !domains.insert(normalize_host(domain)) {
                return Err(StateError::InvalidConfig(format!(
                    "duplicate domain {:?}",
                    domain
                )));
            }
        }
    }
    Ok(())
}

fn sort_snapshot(mut snapshot: Snapshot) -> Snapshot {
    snapshot
        .apps
        .sort_by(|left, right| left.name.cmp(&right.name));
    for app in &mut snapshot.apps {
        app.domains.sort();
    }
    snapshot
}

fn canonical_domains(domains: &[String]) -> Result<Vec<String>, StateError> {
    let mut canonical = Vec::with_capacity(domains.len());
    for domain in domains {
        let normalized = canonical_domain(domain).ok_or_else(|| {
            StateError::InvalidConfig(format!("invalid DNS host pattern {domain:?}"))
        })?;
        canonical.push(normalized);
    }
    Ok(canonical)
}

fn canonical_domain(input: &str) -> Option<String> {
    let trimmed = input.trim();
    let wildcard = trimmed.strip_prefix("*.");
    let host = wildcard.unwrap_or(trimmed);
    if host.is_empty() || host.contains('*') || host.contains(':') || host.contains('/') {
        return None;
    }
    let host = host.trim_end_matches('.');
    if host.is_empty() || host.len() > 253 {
        return None;
    }
    for label in host.split('.') {
        if label.is_empty()
            || label.len() > 63
            || !label
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            || !label.as_bytes()[0].is_ascii_alphanumeric()
            || !label.as_bytes()[label.len() - 1].is_ascii_alphanumeric()
        {
            return None;
        }
    }
    let normalized = normalize_host(host);
    if wildcard.is_some() {
        Some(format!("*.{normalized}"))
    } else {
        Some(normalized)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_db(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "cygnus-state-{label}-{}-{nonce}.db",
            std::process::id()
        ))
    }

    #[test]
    fn json_defaults_nested_policies_and_null_seccomp() {
        let input: NodeConfig = serde_json::from_str(
            r#"{
                "listen": "127.0.0.1:8080",
                "apps": [{
                    "name": "api",
                    "domains": ["api.example.com"],
                    "upstream": "/run/cygnus/api.sock",
                    "command": "/bin/true"
                }]
            }"#,
        )
        .expect("minimal config parses");
        assert_eq!(input.apps[0].seccomp, Some(SeccompMode::Enforce));
        assert_eq!(input.apps[0].limits, LimitsConfig::default());
        assert_eq!(input.apps[0].lifecycle, LifecyclePolicy::default());

        let disabled: NodeConfig = serde_json::from_str(
            r#"{
                "listen": "127.0.0.1:8080",
                "apps": [{
                    "name": "api",
                    "domains": [],
                    "upstream": "/run/cygnus/api.sock",
                    "command": "/bin/true",
                    "seccomp": null
                }]
            }"#,
        )
        .expect("explicit null parses");
        assert_eq!(disabled.apps[0].seccomp, None);
    }

    fn config() -> NodeConfig {
        NodeConfig {
            listen: "127.0.0.1:8080".parse().expect("address"),
            apps: vec![AppConfig {
                name: "api".into(),
                domains: vec!["API.Example.com.".into(), "*.Apps.Example.com".into()],
                upstream: "/run/cygnus/api.sock".into(),
                command: "/bin/true".into(),
                ..AppConfig::default()
            }],
        }
    }

    #[test]
    fn open_default_state_is_empty() {
        let path = temp_db("open");
        let state = State::open(&path).expect("open state");
        let snapshot = state.load().expect("load empty state");
        assert_eq!(snapshot.listen, "127.0.0.1:3000".parse().unwrap());
        assert!(snapshot.apps.is_empty());
        drop(state);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn apply_and_load_projects_everything() {
        let path = temp_db("projection");
        let mut state = State::open(&path).expect("open state");
        let mut input = config();
        input.apps[0].args = vec!["--serve".into()];
        input.apps[0].env.insert("MODE".into(), "test".into());
        input.apps[0].seccomp = None;
        input.apps[0].rootfs = Some(RootfsConfig {
            lowerdirs: vec![PathBuf::from("/lower")],
            ..RootfsConfig::default()
        });
        input.apps[0].egress = EgressConfig::Restricted {
            allow: vec![EgressRuleConfig {
                cidr: "203.0.113.0/24".into(),
                ports: vec![443],
            }],
        };
        input.apps[0].lifecycle.min_instances = 1;
        state.apply(&input).expect("apply config");
        let loaded = state.load().expect("load config");
        assert_eq!(loaded.listen, input.listen);
        assert_eq!(loaded.apps[0].name, "api");
        assert_eq!(
            loaded.apps[0].domains,
            ["*.apps.example.com", "api.example.com"]
        );
        assert_eq!(loaded.apps[0].spec.args, [OsString::from("--serve")]);
        assert_eq!(loaded.apps[0].spec.seccomp, None);
        assert_eq!(
            loaded.apps[0]
                .spec
                .ingress
                .as_ref()
                .map(|ingress| ingress.host_dir.as_path()),
            Some(Path::new("/run/cygnus"))
        );
        assert_eq!(loaded.apps[0].lifecycle.min_instances, 1);
        drop(state);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn duplicate_domains_are_rejected_after_router_normalization() {
        let path = temp_db("domains");
        let mut state = State::open(&path).expect("open state");
        let mut input = config();
        input.apps.push(AppConfig {
            name: "web".into(),
            domains: vec!["api.example.com".into()],
            upstream: "/run/cygnus/web.sock".into(),
            command: "/bin/true".into(),
            ..AppConfig::default()
        });
        assert!(state.apply(&input).is_err());
        drop(state);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn invalid_replacement_rolls_back() {
        let path = temp_db("rollback");
        let mut state = State::open(&path).expect("open state");
        let input = config();
        state.apply(&input).expect("initial apply");
        let mut invalid = input.clone();
        invalid.apps[0].limits.memory_max = 0;
        assert!(state.apply(&invalid).is_err());
        let loaded = state.load().expect("old state remains");
        assert_eq!(loaded.apps[0].name, "api");
        assert_eq!(
            loaded.apps[0].spec.limits.memory_max,
            CgroupLimits::default().memory_max
        );
        drop(state);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn unknown_schema_version_is_rejected() {
        let path = temp_db("version");
        {
            let connection = Connection::open(&path).expect("create database");
            connection
                .pragma_update(None, "user_version", 99_i32)
                .expect("set version");
        }
        assert!(matches!(
            State::open(&path),
            Err(StateError::UnknownSchemaVersion { found: 99, .. })
        ));
        let _ = fs::remove_file(path);
    }
}
