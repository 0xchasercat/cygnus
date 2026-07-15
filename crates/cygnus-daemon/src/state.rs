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
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use thiserror::Error;

/// Default on-disk database used by the daemon binary.
pub const DEFAULT_STATE_PATH: &str = "/var/lib/cygnus/state.db";
const SCHEMA_VERSION: i32 = 2;
const BUSY_TIMEOUT_MS: u64 = 5_000;
const SHA256_HEX_LEN: usize = 64;

/// Deployment lifecycle persisted by the daemon.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DeploymentStatus {
    Building,
    Failed,
    Sealed,
    Active,
}

/// A trusted Bun engine registered by the operator. `host_root` is the host
/// directory mounted for the engine; `cage_executable` is an absolute path as
/// seen inside that rootfs, never a tenant-selected host path.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct EngineRecord {
    pub version: String,
    pub host_root: PathBuf,
    pub cage_executable: PathBuf,
    pub sha256: String,
}

/// A deployment identity accepted from the caller.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DeploymentInput {
    pub id: String,
    pub app: String,
    pub source_hash: String,
    pub engine_version: String,
}

/// Build output submitted when sealing a deployment.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ArtifactInput {
    pub app: String,
    pub source_hash: String,
    pub artifact_hash: String,
    pub engine_version: String,
    pub host_path: PathBuf,
    pub metadata_json: String,
}

/// A sealed, content-addressed build output.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ArtifactRecord {
    pub app: String,
    pub source_hash: String,
    pub artifact_hash: String,
    pub engine_version: String,
    pub host_path: PathBuf,
    pub metadata_json: String,
}

/// A deployment and, after sealing, the artifact it produced.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct DeploymentRecord {
    pub id: String,
    pub app: String,
    pub source_hash: String,
    pub engine_version: String,
    pub artifact_hash: Option<String>,
    pub status: DeploymentStatus,
    pub error: Option<String>,
}

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
    #[error("invalid {kind}: {detail}")]
    InvalidRecord { kind: &'static str, detail: String },
    #[error("deployment {id:?} has illegal transition from {from:?} to {to:?}")]
    InvalidDeploymentTransition {
        id: String,
        from: DeploymentStatus,
        to: DeploymentStatus,
    },
    #[error("complete-config apply would destroy artifact/deployment state")]
    DestructiveApply,
    #[error("artifact {artifact:?} is not owned by deployment {deployment:?}")]
    ArtifactOwnership {
        artifact: String,
        deployment: String,
    },
    #[error("artifact metadata does not agree with its deployment")]
    MetadataMismatch,
}

/// A SQLite-backed node configuration store.
pub struct State {
    connection: Connection,
}

impl State {
    /// Open or create a state database and apply every ordered migration.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StateError> {
        let path = path.as_ref();
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent)?;
        }
        let mut connection = Connection::open(path)?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "FULL")?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        connection.busy_timeout(Duration::from_millis(BUSY_TIMEOUT_MS))?;

        let mut version: i32 =
            connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if !(0..=SCHEMA_VERSION).contains(&version) {
            return Err(StateError::UnknownSchemaVersion {
                found: version,
                expected: SCHEMA_VERSION,
            });
        }
        if version == 0 {
            let transaction = connection.transaction()?;
            create_schema(&transaction)?;
            transaction.pragma_update(None, "user_version", 1_i32)?;
            transaction.commit()?;
            version = 1;
        }
        while version < SCHEMA_VERSION {
            let transaction = connection.transaction()?;
            match version {
                1 => migrate_v1_to_v2(&transaction)?,
                _ => unreachable!("validated schema version"),
            }
            let next = version + 1;
            transaction.pragma_update(None, "user_version", next)?;
            transaction.commit()?;
            version = next;
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

    /// Register an operator-trusted engine and return the validated record.
    pub fn register_engine(&mut self, engine: &EngineRecord) -> Result<EngineRecord, StateError> {
        validate_engine(engine)?;
        self.connection.execute(
            "INSERT INTO engines (version, host_root, cage_executable, sha256) VALUES (?1, ?2, ?3, ?4)",
            params![engine.version, engine.host_root.to_string_lossy(), engine.cage_executable.to_string_lossy(), engine.sha256],
        )?;
        Ok(engine.clone())
    }

    pub fn engine(&self, version: &str) -> Result<Option<EngineRecord>, StateError> {
        self.connection.query_row(
            "SELECT version, host_root, cage_executable, sha256 FROM engines WHERE version = ?1",
            [version],
            |row| Ok(EngineRecord {
                version: row.get(0)?,
                host_root: PathBuf::from(row.get::<_, String>(1)?),
                cage_executable: PathBuf::from(row.get::<_, String>(2)?),
                sha256: row.get(3)?,
            }),
        ).optional().map_err(StateError::from)
    }

    /// Start a caller-identified build against a registered engine.
    pub fn begin_deployment(
        &mut self,
        input: &DeploymentInput,
    ) -> Result<DeploymentRecord, StateError> {
        validate_deployment_input(input)?;
        if self.engine(&input.engine_version)?.is_none() {
            return Err(StateError::InvalidRecord {
                kind: "deployment",
                detail: format!("engine {:?} is not registered", input.engine_version),
            });
        }
        self.connection.execute(
            "INSERT INTO deployments (id, app, source_hash, engine_version, status, error) VALUES (?1, ?2, ?3, ?4, 'building', NULL)",
            params![input.id, input.app, input.source_hash, input.engine_version],
        )?;
        self.deployment(&input.id)?.ok_or_else(|| {
            StateError::IncompleteState(format!(
                "deployment {:?} disappeared after insert",
                input.id
            ))
        })
    }

    pub fn mark_deployment_failed(
        &mut self,
        id: &str,
        error: &str,
    ) -> Result<DeploymentRecord, StateError> {
        if id.trim().is_empty() || error.trim().is_empty() {
            return Err(StateError::InvalidRecord {
                kind: "deployment",
                detail: "id and failure error must be nonempty".into(),
            });
        }
        let current = self
            .deployment(id)?
            .ok_or_else(|| StateError::InvalidRecord {
                kind: "deployment",
                detail: format!("deployment {id:?} does not exist"),
            })?;
        ensure_transition(id, current.status, DeploymentStatus::Failed)?;
        self.connection.execute(
            "UPDATE deployments SET status = 'failed', error = ?2 WHERE id = ?1",
            params![id, error],
        )?;
        self.deployment(id)?.ok_or_else(|| {
            StateError::IncompleteState(format!("deployment {id:?} disappeared after failure"))
        })
    }

    /// Seal the server-computed artifact and advance its deployment.
    pub fn seal_deployment(
        &mut self,
        id: &str,
        artifact: &ArtifactInput,
    ) -> Result<ArtifactRecord, StateError> {
        validate_artifact_input(artifact)?;
        let transaction = self.connection.transaction()?;
        let deployment =
            query_deployment_tx(&transaction, id)?.ok_or_else(|| StateError::InvalidRecord {
                kind: "deployment",
                detail: format!("deployment {id:?} does not exist"),
            })?;
        ensure_transition(id, deployment.status, DeploymentStatus::Sealed)?;
        if deployment.app != artifact.app
            || deployment.source_hash != artifact.source_hash
            || deployment.engine_version != artifact.engine_version
        {
            return Err(StateError::ArtifactOwnership {
                artifact: artifact.artifact_hash.clone(),
                deployment: id.to_owned(),
            });
        }
        validate_metadata(artifact)?;
        if !query_engine_tx(&transaction, &artifact.engine_version)? {
            return Err(StateError::InvalidRecord {
                kind: "artifact",
                detail: format!("engine {:?} is not registered", artifact.engine_version),
            });
        }
        transaction.execute(
            "INSERT INTO artifacts (app, source_hash, artifact_hash, engine_version, host_path, metadata_json, status) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'sealed')",
            params![artifact.app, artifact.source_hash, artifact.artifact_hash, artifact.engine_version, artifact.host_path.to_string_lossy(), artifact.metadata_json],
        )?;
        transaction.execute("UPDATE deployments SET artifact_hash = ?2, status = 'sealed', error = NULL WHERE id = ?1", params![id, artifact.artifact_hash])?;
        transaction.commit()?;
        self.artifact(&artifact.artifact_hash)?.ok_or_else(|| {
            StateError::IncompleteState(format!(
                "artifact {:?} disappeared after sealing",
                artifact.artifact_hash
            ))
        })
    }

    pub fn deployment(&self, id: &str) -> Result<Option<DeploymentRecord>, StateError> {
        self.connection.query_row(
            "SELECT id, app, source_hash, engine_version, artifact_hash, status, error FROM deployments WHERE id = ?1",
            [id],
            |row| {
                let status: String = row.get(5)?;
                Ok(DeploymentRecord { id: row.get(0)?, app: row.get(1)?, source_hash: row.get(2)?, engine_version: row.get(3)?, artifact_hash: row.get(4)?, status: parse_status(&status).map_err(|error| rusqlite::Error::FromSqlConversionFailure(5, rusqlite::types::Type::Text, Box::new(std::io::Error::other(error))))?, error: row.get(6)? })
            },
        ).optional().map_err(StateError::from)
    }

    pub fn artifact(&self, hash: &str) -> Result<Option<ArtifactRecord>, StateError> {
        self.connection.query_row(
            "SELECT app, source_hash, artifact_hash, engine_version, host_path, metadata_json FROM artifacts WHERE artifact_hash = ?1 AND status = 'sealed'",
            [hash],
            |row| Ok(ArtifactRecord { app: row.get(0)?, source_hash: row.get(1)?, artifact_hash: row.get(2)?, engine_version: row.get(3)?, host_path: PathBuf::from(row.get::<_, String>(4)?), metadata_json: row.get(5)? }),
        ).optional().map_err(StateError::from)
    }

    /// Atomically register the first runtime app and activate its sealed artifact.
    pub fn activate_first(
        &mut self,
        app: &AppConfig,
        artifact_hash: &str,
    ) -> Result<DeploymentRecord, StateError> {
        let snapshot = snapshot_from_config(&NodeConfig {
            listen: SocketAddr::from(([127, 0, 0, 1], 3000)),
            apps: vec![app.clone()],
        })?;
        let stored = snapshot_to_stored(&snapshot)?;
        let stored_app = stored
            .apps
            .first()
            .ok_or_else(|| StateError::InvalidConfig("activation app is empty".into()))?;
        validate_absolute_path(&app.upstream, "app upstream")?;
        validate_hash(artifact_hash, "artifact hash")?;
        let transaction = self.connection.transaction()?;
        let artifact = query_artifact_tx(&transaction, artifact_hash)?.ok_or_else(|| {
            StateError::InvalidRecord {
                kind: "artifact",
                detail: format!("sealed artifact {artifact_hash:?} does not exist"),
            }
        })?;
        if artifact.app != app.name {
            return Err(StateError::ArtifactOwnership {
                artifact: artifact_hash.to_owned(),
                deployment: artifact.app,
            });
        }
        let existing_app = transaction
            .query_row("SELECT name FROM apps ORDER BY id LIMIT 1", [], |row| {
                row.get::<_, String>(0)
            })
            .optional()?;
        if let Some(existing) = existing_app {
            return Err(StateError::InvalidConfig(format!(
                "first activation requires an empty node (existing app {existing:?})"
            )));
        }
        let deployment =
            query_deployment_by_artifact_tx(&transaction, artifact_hash)?.ok_or_else(|| {
                StateError::IncompleteState("artifact deployment relation is missing".into())
            })?;
        ensure_transition(&deployment.id, deployment.status, DeploymentStatus::Active)?;
        let runtime_json = serde_json::to_string(&StoredApp {
            name: &stored_app.name,
            upstream: &stored_app.upstream,
            domains: &[],
            runtime: &stored_app.runtime,
        })
        .map_err(|error| {
            StateError::InvalidConfig(format!("serialize app {:?}: {error}", app.name))
        })?;
        let app_id = transaction.query_row(
            "INSERT INTO apps (name, upstream, runtime_json) VALUES (?1, ?2, ?3) RETURNING id",
            params![stored_app.name, stored_app.upstream, runtime_json],
            |row| row.get::<_, i64>(0),
        )?;
        for domain in &stored_app.domains {
            transaction.execute(
                "INSERT INTO domains (app_id, domain) VALUES (?1, ?2)",
                params![app_id, domain],
            )?;
        }
        transaction.execute(
            "INSERT INTO app_artifacts (app_id, artifact_id) VALUES (?1, ?2)",
            params![app_id, artifact.id],
        )?;
        transaction.execute(
            "UPDATE deployments SET status = 'active' WHERE id = ?1",
            [deployment.id.as_str()],
        )?;
        transaction.commit()?;
        self.deployment(&deployment.id)?.ok_or_else(|| {
            StateError::IncompleteState(format!(
                "deployment {:?} disappeared after activation",
                deployment.id
            ))
        })
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

fn migrate_v1_to_v2(connection: &Connection) -> Result<(), StateError> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS engines (
             id INTEGER PRIMARY KEY,
             version TEXT NOT NULL UNIQUE,
             host_root TEXT NOT NULL,
             cage_executable TEXT NOT NULL,
             sha256 TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS artifacts (
             id INTEGER PRIMARY KEY,
             app TEXT NOT NULL,
             source_hash TEXT NOT NULL,
             artifact_hash TEXT NOT NULL UNIQUE,
             engine_version TEXT NOT NULL REFERENCES engines(version),
             host_path TEXT NOT NULL UNIQUE,
             metadata_json TEXT NOT NULL,
             status TEXT NOT NULL CHECK (status = 'sealed')
         );
         CREATE TABLE IF NOT EXISTS deployments (
             id TEXT PRIMARY KEY,
             app TEXT NOT NULL,
             source_hash TEXT NOT NULL,
             engine_version TEXT NOT NULL REFERENCES engines(version),
             artifact_hash TEXT UNIQUE REFERENCES artifacts(artifact_hash),
             status TEXT NOT NULL CHECK (status IN ('building', 'failed', 'sealed', 'active')),
             error TEXT
         );
         CREATE TABLE IF NOT EXISTS app_artifacts (
             app_id INTEGER PRIMARY KEY REFERENCES apps(id) ON DELETE CASCADE,
             artifact_id INTEGER NOT NULL UNIQUE REFERENCES artifacts(id),
             activated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
         );
         CREATE INDEX IF NOT EXISTS artifacts_app ON artifacts(app);
         CREATE INDEX IF NOT EXISTS deployments_app ON deployments(app);",
    )?;
    Ok(())
}

#[derive(Clone, Debug)]
struct ArtifactRow {
    id: i64,
    app: String,
}

fn validate_engine(engine: &EngineRecord) -> Result<(), StateError> {
    if engine.version.trim().is_empty() || engine.version.chars().any(char::is_control) {
        return Err(StateError::InvalidRecord {
            kind: "engine",
            detail: "version must be nonempty and printable".into(),
        });
    }
    validate_absolute_path(&engine.host_root, "engine host root")?;
    if engine
        .host_root
        .as_os_str()
        .as_bytes()
        .iter()
        .any(|byte| matches!(byte, b':' | b',' | b'\\' | 0))
    {
        return Err(StateError::InvalidRecord {
            kind: "path",
            detail: "engine host root contains bytes unsupported by overlayfs options".into(),
        });
    }
    validate_cage_path(&engine.cage_executable, "engine cage executable")?;
    let relative =
        engine
            .cage_executable
            .strip_prefix("/")
            .map_err(|_| StateError::InvalidRecord {
                kind: "path",
                detail: "engine cage executable must be absolute".into(),
            })?;
    let host_executable = engine.host_root.join(relative);
    let canonical_executable =
        fs::canonicalize(&host_executable).map_err(|error| StateError::InvalidRecord {
            kind: "path",
            detail: format!("engine executable is unavailable: {error}"),
        })?;
    if canonical_executable != host_executable
        || !canonical_executable.starts_with(&engine.host_root)
    {
        return Err(StateError::InvalidRecord {
            kind: "path",
            detail: "engine executable must not traverse a symlink or escape host root".into(),
        });
    }
    let metadata =
        fs::symlink_metadata(&host_executable).map_err(|error| StateError::InvalidRecord {
            kind: "path",
            detail: format!("engine executable is unavailable: {error}"),
        })?;
    if !metadata.file_type().is_file() || metadata.permissions().mode() & 0o111 == 0 {
        return Err(StateError::InvalidRecord {
            kind: "path",
            detail: "engine executable must be a regular executable file".into(),
        });
    }
    validate_hash(&engine.sha256, "engine SHA-256")
}

fn validate_cage_path(path: &Path, kind: &str) -> Result<(), StateError> {
    if !path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                std::path::Component::CurDir
                    | std::path::Component::ParentDir
                    | std::path::Component::Prefix(_)
            )
        })
    {
        return Err(StateError::InvalidRecord {
            kind: "path",
            detail: format!("{kind} must be an absolute canonical path"),
        });
    }
    Ok(())
}

fn validate_deployment_input(input: &DeploymentInput) -> Result<(), StateError> {
    if input.id.trim().is_empty() || input.id.chars().any(char::is_control) {
        return Err(StateError::InvalidRecord {
            kind: "deployment",
            detail: "id must be nonempty and printable".into(),
        });
    }
    if input.app.trim().is_empty() || input.app.chars().any(char::is_control) {
        return Err(StateError::InvalidRecord {
            kind: "deployment",
            detail: "app must be nonempty and printable".into(),
        });
    }
    validate_hash(&input.source_hash, "source hash")?;
    if input.engine_version.trim().is_empty() {
        return Err(StateError::InvalidRecord {
            kind: "deployment",
            detail: "engine version must be nonempty".into(),
        });
    }
    Ok(())
}

fn validate_artifact_input(input: &ArtifactInput) -> Result<(), StateError> {
    if input.app.trim().is_empty() || input.app.chars().any(char::is_control) {
        return Err(StateError::InvalidRecord {
            kind: "artifact",
            detail: "app must be nonempty and printable".into(),
        });
    }
    validate_hash(&input.source_hash, "source hash")?;
    validate_hash(&input.artifact_hash, "artifact hash")?;
    if input.engine_version.trim().is_empty() {
        return Err(StateError::InvalidRecord {
            kind: "artifact",
            detail: "engine version must be nonempty".into(),
        });
    }
    validate_absolute_path(&input.host_path, "artifact host path")
}

fn validate_absolute_path(path: &Path, kind: &str) -> Result<(), StateError> {
    if !path.is_absolute() {
        return Err(StateError::InvalidRecord {
            kind: "path",
            detail: format!("{kind} must be absolute"),
        });
    }
    if path.components().any(|component| {
        matches!(
            component,
            std::path::Component::CurDir
                | std::path::Component::ParentDir
                | std::path::Component::Prefix(_)
        )
    }) {
        return Err(StateError::InvalidRecord {
            kind: "path",
            detail: format!("{kind} must be canonical"),
        });
    }
    if path.exists() && fs::canonicalize(path)? != path {
        return Err(StateError::InvalidRecord {
            kind: "path",
            detail: format!("{kind} is not canonical"),
        });
    }
    Ok(())
}

fn validate_hash(hash: &str, kind: &str) -> Result<(), StateError> {
    if hash.len() != SHA256_HEX_LEN
        || !hash
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(StateError::InvalidRecord {
            kind: "hash",
            detail: format!("{kind} must be lowercase 64-character SHA-256 hex"),
        });
    }
    Ok(())
}

fn metadata_value<'a>(
    object: &'a serde_json::Map<String, serde_json::Value>,
    a: &str,
    b: &str,
) -> Option<&'a str> {
    object
        .get(a)
        .or_else(|| object.get(b))
        .and_then(serde_json::Value::as_str)
}

fn validate_metadata(artifact: &ArtifactInput) -> Result<(), StateError> {
    let value: serde_json::Value =
        serde_json::from_str(&artifact.metadata_json).map_err(|_| StateError::MetadataMismatch)?;
    let object = value.as_object().ok_or(StateError::MetadataMismatch)?;
    let source =
        metadata_value(object, "sourceHash", "source_hash").ok_or(StateError::MetadataMismatch)?;
    let bun =
        metadata_value(object, "bunVersion", "bun_version").ok_or(StateError::MetadataMismatch)?;
    if source != artifact.source_hash || bun != artifact.engine_version {
        return Err(StateError::MetadataMismatch);
    }
    if let Some(hash) = metadata_value(object, "artifactHash", "artifact_hash")
        && hash != artifact.artifact_hash
    {
        return Err(StateError::MetadataMismatch);
    }
    Ok(())
}

fn parse_status(status: &str) -> Result<DeploymentStatus, String> {
    match status {
        "building" => Ok(DeploymentStatus::Building),
        "failed" => Ok(DeploymentStatus::Failed),
        "sealed" => Ok(DeploymentStatus::Sealed),
        "active" => Ok(DeploymentStatus::Active),
        other => Err(format!("unknown deployment status {other:?}")),
    }
}

fn ensure_transition(
    id: &str,
    from: DeploymentStatus,
    to: DeploymentStatus,
) -> Result<(), StateError> {
    let legal = matches!(
        (from, to),
        (
            DeploymentStatus::Building,
            DeploymentStatus::Failed | DeploymentStatus::Sealed
        ) | (DeploymentStatus::Sealed, DeploymentStatus::Active)
    );
    if legal {
        Ok(())
    } else {
        Err(StateError::InvalidDeploymentTransition {
            id: id.to_owned(),
            from,
            to,
        })
    }
}

fn query_engine_tx(transaction: &Transaction<'_>, version: &str) -> Result<bool, rusqlite::Error> {
    transaction
        .query_row(
            "SELECT 1 FROM engines WHERE version = ?1",
            [version],
            |_| Ok(()),
        )
        .optional()
        .map(|value| value.is_some())
}

fn query_deployment_tx(
    transaction: &Transaction<'_>,
    id: &str,
) -> Result<Option<DeploymentRecord>, StateError> {
    transaction.query_row("SELECT id, app, source_hash, engine_version, artifact_hash, status, error FROM deployments WHERE id = ?1", [id], |row| {
        let status: String = row.get(5)?;
        Ok(DeploymentRecord { id: row.get(0)?, app: row.get(1)?, source_hash: row.get(2)?, engine_version: row.get(3)?, artifact_hash: row.get(4)?, status: parse_status(&status).map_err(|error| rusqlite::Error::FromSqlConversionFailure(5, rusqlite::types::Type::Text, Box::new(std::io::Error::other(error))))?, error: row.get(6)? })
    }).optional().map_err(StateError::from)
}

fn query_artifact_tx(
    transaction: &Transaction<'_>,
    hash: &str,
) -> Result<Option<ArtifactRow>, StateError> {
    transaction
        .query_row(
            "SELECT id, app FROM artifacts WHERE artifact_hash = ?1 AND status = 'sealed'",
            [hash],
            |row| {
                Ok(ArtifactRow {
                    id: row.get(0)?,
                    app: row.get(1)?,
                })
            },
        )
        .optional()
        .map_err(StateError::from)
}

fn query_deployment_by_artifact_tx(
    transaction: &Transaction<'_>,
    hash: &str,
) -> Result<Option<DeploymentRecord>, StateError> {
    transaction.query_row("SELECT id, app, source_hash, engine_version, artifact_hash, status, error FROM deployments WHERE artifact_hash = ?1", [hash], |row| {
        let status: String = row.get(5)?;
        Ok(DeploymentRecord { id: row.get(0)?, app: row.get(1)?, source_hash: row.get(2)?, engine_version: row.get(3)?, artifact_hash: row.get(4)?, status: parse_status(&status).map_err(|error| rusqlite::Error::FromSqlConversionFailure(5, rusqlite::types::Type::Text, Box::new(std::io::Error::other(error))))?, error: row.get(6)? })
    }).optional().map_err(StateError::from)
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

impl TryFrom<&EgressMode> for StoredEgress {
    type Error = StateError;

    fn try_from(mode: &EgressMode) -> Result<Self, Self::Error> {
        match mode {
            EgressMode::None => Ok(Self::None),
            EgressMode::Public => Ok(Self::Public),
            EgressMode::Open => Ok(Self::Open),
            EgressMode::Restricted { allow } => Ok(Self::Restricted {
                allow: allow
                    .iter()
                    .map(|rule| StoredEgressRule {
                        cidr: rule.cidr.clone(),
                        ports: rule.ports.clone(),
                    })
                    .collect(),
            }),
            EgressMode::BuildDomains { .. } => Err(StateError::InvalidConfig(
                "build-only domain egress cannot be persisted as an app policy".into(),
            )),
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
            egress: StoredEgress::try_from(&app.spec.egress)?,
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
    let has_artifact_state: bool = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM artifacts) OR EXISTS(SELECT 1 FROM deployments) OR EXISTS(SELECT 1 FROM app_artifacts)",
        [],
        |row| row.get(0),
    )?;
    if has_artifact_state {
        return Err(StateError::DestructiveApply);
    }
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
    #[test]
    fn migrates_v1_fixture_without_losing_runtime_data() {
        let path = temp_db("v1-migrate");
        {
            let connection = Connection::open(&path).expect("fixture database");
            connection.execute_batch(
                "CREATE TABLE node_config (id INTEGER PRIMARY KEY CHECK (id = 1), listen TEXT NOT NULL);
                 CREATE TABLE apps (id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE, upstream TEXT NOT NULL UNIQUE, runtime_json TEXT NOT NULL);
                 CREATE TABLE domains (id INTEGER PRIMARY KEY, app_id INTEGER NOT NULL REFERENCES apps(id) ON DELETE CASCADE, domain TEXT NOT NULL COLLATE BINARY UNIQUE);
                 CREATE INDEX domains_app_id ON domains(app_id);
                 INSERT INTO node_config VALUES (1, '127.0.0.1:8181');
                 INSERT INTO apps VALUES (1, 'legacy', '/run/legacy.sock', '{\"name\":\"legacy\",\"upstream\":\"/run/legacy.sock\",\"domains\":[],\"runtime\":{\"command\":\"/bin/true\",\"args\":[],\"env\":{},\"limits\":{\"memory_max\":268435456,\"memory_high\":234881024,\"cpu_quota\":100000,\"cpu_period\":100000,\"pids_max\":128},\"rootfs\":null,\"seccomp\":\"enforce\",\"egress\":{\"mode\":\"none\"},\"init\":null,\"readiness_timeout_ms\":5000,\"idle_ttl_ms\":600000,\"min_instances\":0,\"backoff_base_ms\":100,\"backoff_max_ms\":30000,\"crash_window_ms\":60000,\"crash_loop_threshold\":5}}');
                 INSERT INTO domains VALUES (1, 1, 'legacy.example.com');
                 PRAGMA user_version = 1;",
            ).expect("write v1 fixture");
        }
        let state = State::open(&path).expect("migrate fixture");
        let loaded = state.load().expect("load migrated fixture");
        assert_eq!(loaded.listen, "127.0.0.1:8181".parse().unwrap());
        assert_eq!(loaded.apps[0].name, "legacy");
        assert_eq!(loaded.apps[0].domains, ["legacy.example.com"]);
        let version: i32 = state
            .connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, 2);
        drop(state);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn artifact_deployment_round_trip_and_first_activation_are_atomic() {
        let path = temp_db("activation");
        let mut state = State::open(&path).expect("open state");
        let engine = EngineRecord {
            version: "1.2.3".into(),
            host_root: "/".into(),
            cage_executable: "/usr/bin/true".into(),
            sha256: "a".repeat(64),
        };
        assert_eq!(state.register_engine(&engine).unwrap(), engine);
        let source_hash = "b".repeat(64);
        let artifact_hash = "c".repeat(64);
        let input = DeploymentInput {
            id: "dep-1".into(),
            app: "api".into(),
            source_hash: source_hash.clone(),
            engine_version: engine.version.clone(),
        };
        assert_eq!(
            state.begin_deployment(&input).unwrap().status,
            DeploymentStatus::Building
        );
        let artifact = ArtifactInput {
            app: "api".into(),
            source_hash: source_hash.clone(),
            artifact_hash: artifact_hash.clone(),
            engine_version: engine.version.clone(),
            host_path: "/var/lib/cygnus/apps/api/c".into(),
            metadata_json: format!(
                "{{\"bunVersion\":\"{}\",\"sourceHash\":\"{}\"}}",
                engine.version, source_hash
            ),
        };
        assert_eq!(
            state
                .seal_deployment("dep-1", &artifact)
                .unwrap()
                .artifact_hash,
            artifact_hash
        );
        let app = AppConfig {
            name: "api".into(),
            domains: vec!["API.Example.com".into()],
            upstream: "/run/api.sock".into(),
            command: "/bin/true".into(),
            ..AppConfig::default()
        };
        assert_eq!(
            state.activate_first(&app, &artifact_hash).unwrap().status,
            DeploymentStatus::Active
        );
        assert_eq!(state.load().unwrap().apps[0].name, "api");
        assert!(state.activate_first(&app, &artifact_hash).is_err());
        assert_eq!(
            state.deployment("dep-1").unwrap().unwrap().status,
            DeploymentStatus::Active
        );
        let second_source_hash = "d".repeat(64);
        let second_artifact_hash = "e".repeat(64);
        state
            .begin_deployment(&DeploymentInput {
                id: "dep-2".into(),
                app: "worker".into(),
                source_hash: second_source_hash.clone(),
                engine_version: engine.version.clone(),
            })
            .unwrap();
        state
            .seal_deployment(
                "dep-2",
                &ArtifactInput {
                    app: "worker".into(),
                    source_hash: second_source_hash.clone(),
                    artifact_hash: second_artifact_hash.clone(),
                    engine_version: engine.version.clone(),
                    host_path: "/var/lib/cygnus/apps/worker/e".into(),
                    metadata_json: format!(
                        "{{\"bunVersion\":\"{}\",\"sourceHash\":\"{}\"}}",
                        engine.version, second_source_hash
                    ),
                },
            )
            .unwrap();
        let worker = AppConfig {
            name: "worker".into(),
            domains: vec!["worker.example.com".into()],
            upstream: "/run/worker.sock".into(),
            command: "/bin/true".into(),
            ..AppConfig::default()
        };
        assert!(
            state
                .activate_first(&worker, &second_artifact_hash)
                .is_err()
        );
        assert_eq!(
            state.deployment("dep-2").unwrap().unwrap().status,
            DeploymentStatus::Sealed
        );
        assert!(matches!(
            state.apply(&NodeConfig::default()),
            Err(StateError::DestructiveApply)
        ));
        drop(state);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn invalid_hashes_paths_and_status_transitions_are_rejected() {
        let path = temp_db("validation");
        let mut state = State::open(&path).expect("open state");
        assert!(
            state
                .register_engine(&EngineRecord {
                    version: "bad".into(),
                    host_root: "relative".into(),
                    cage_executable: "/usr/bin/true".into(),
                    sha256: "A".repeat(64)
                })
                .is_err()
        );
        let engine = EngineRecord {
            version: "1".into(),
            host_root: "/".into(),
            cage_executable: "/usr/bin/true".into(),
            sha256: "d".repeat(64),
        };
        state.register_engine(&engine).unwrap();
        assert!(
            state
                .begin_deployment(&DeploymentInput {
                    id: "".into(),
                    app: "api".into(),
                    source_hash: "e".repeat(64),
                    engine_version: "1".into()
                })
                .is_err()
        );
        state
            .begin_deployment(&DeploymentInput {
                id: "dep".into(),
                app: "api".into(),
                source_hash: "e".repeat(64),
                engine_version: "1".into(),
            })
            .unwrap();
        state.mark_deployment_failed("dep", "build failed").unwrap();
        assert!(state.mark_deployment_failed("dep", "again").is_err());
        drop(state);
        let _ = fs::remove_file(path);
    }
}
