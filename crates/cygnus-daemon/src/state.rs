use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::Read;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::edge::{
    AcmeConfig, CertificateInput, CertificateRecord, CertificateStore, CertificateStoreError,
    EdgeConfig, SslMode,
};
use argon2::Argon2;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use cygnus_cage::{
    CageError, CageSpec, CgroupLimits, DEFAULT_READINESS_TIMEOUT, EgressMode,
    EgressRule as CageEgressRule, FilterMode, IngressSpec, RootfsSpec,
};
use cygnus_router::normalize_host;
use cygnus_supervisor::{
    DEFAULT_BACKOFF_BASE, DEFAULT_BACKOFF_MAX, DEFAULT_CRASH_LOOP_THRESHOLD, DEFAULT_CRASH_WINDOW,
    DEFAULT_IDLE_TTL, LifecycleConfig,
};
use getrandom::fill as random_fill;
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use thiserror::Error;

/// Default on-disk database used by the daemon binary.
pub const DEFAULT_STATE_PATH: &str = "/var/lib/cygnus/state.db";
const SCHEMA_VERSION: i32 = 11;
const BUSY_TIMEOUT_MS: u64 = 5_000;
pub const MAX_ACCOUNT_EMAIL_BYTES: usize = 254;
pub const MIN_ACCOUNT_PASSWORD_BYTES: usize = 12;
pub const MAX_ACCOUNT_PASSWORD_BYTES: usize = 1024;
const ACCOUNT_SALT_BYTES: usize = 16;
const SHA256_HEX_LEN: usize = 64;
const NODE_KEY_LEN: usize = 32;
const SECRET_NONCE_LEN: usize = 24;
const SECRET_AAD: &[u8] = b"cygnus/github-secret/v5";
const MAX_GITHUB_ATTEMPTS: u32 = 8;
const RETRY_BASE_SECONDS: i64 = 5;
const RETRY_MAX_SECONDS: i64 = 3600;
const MAX_GITHUB_TEXT_LEN: usize = 16 * 1024;
const MAX_GITHUB_JOBS_PER_DELIVERY: usize = 256;
pub const MAX_GITHUB_WEBHOOK_BYTES: u64 = 25 * 1024 * 1024;
pub const MAX_ENV_VAR_VALUE_BYTES: usize = 32 * 1024;

/// Deployment lifecycle persisted by the daemon.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DeploymentStatus {
    Building,
    Failed,
    Sealed,
    Active,
}

/// Ownership class for an application domain.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DomainKind {
    Native,
    Custom,
}

/// Requested TLS policy for a domain.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DomainTls {
    Acme,
    SelfSigned,
}

/// Runtime certificate lifecycle for a domain.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DomainStatus {
    Active,
    FallbackActive,
    Issuing,
    Pending,
    Failed,
}

/// One persisted application-domain lifecycle record.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DomainRecord {
    pub host: String,
    pub app: String,
    pub kind: DomainKind,
    pub tls: DomainTls,
    pub status: DomainStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_unix: Option<i64>,
    /// Last ACME failure message, if any. Cleared on the next successful issuance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Earliest time (unix seconds) the reconciler may retry this domain.
    /// `None` means eligible immediately.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_retry_unix: Option<i64>,
    /// Consecutive ACME failures since the last success or manual retry.
    /// Drives the reconciler's exponential backoff.
    #[serde(default)]
    pub retry_count: i64,
    pub is_primary: bool,
}

/// One decrypted environment-variable key for an app. Values are encrypted
/// at rest with the node key (same primitive as GitHub app secrets) and
/// only ever decrypted server-side to build a cage's runtime environment or
/// to answer an authenticated admin read.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct EnvVarRecord {
    pub key: String,
    pub value: String,
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
    #[serde(default)]
    pub is_default: bool,
}

/// A registered engine together with the number of active apps using it.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct EngineStatus {
    pub engine: EngineRecord,
    pub app_count: u32,
}

/// The system that supplied a deployment.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DeploymentSourceKind {
    GitHub,
    Upload,
    #[default]
    Cli,
}

/// Typed deployment provenance persisted with every deployment.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DeploymentSource {
    pub kind: DeploymentSourceKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
}

impl DeploymentSource {
    pub fn cli() -> Self {
        Self::default()
    }

    pub fn upload() -> Self {
        Self {
            kind: DeploymentSourceKind::Upload,
            ..Self::default()
        }
    }

    pub fn github(branch: Option<String>, commit: Option<String>) -> Self {
        Self {
            kind: DeploymentSourceKind::GitHub,
            branch,
            commit,
        }
    }
}

/// A deployment identity accepted from the caller.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DeploymentInput {
    pub id: String,
    pub app: String,
    pub source_hash: String,
    pub engine_version: String,
    #[serde(default)]
    pub source: DeploymentSource,
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
    pub source: DeploymentSource,
    pub artifact_hash: Option<String>,
    pub status: DeploymentStatus,
    pub error: Option<String>,
    pub created_at: String,
    pub created_ms: i64,
    pub updated_at: String,
    /// Milliseconds since epoch of the last status transition. For a
    /// terminal deployment (active/failed/sealed) this is effectively the
    /// finish time; the console uses it instead of "now" to compute duration.
    pub updated_ms: i64,
    pub log_path: Option<PathBuf>,
}

/// The deployment currently selected by an app's active artifact pointer.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ActiveDeploymentRecord {
    pub deployment_id: String,
    pub artifact_hash: String,
    pub engine_version: String,
}

/// Whether password authentication has been configured for this node.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct AccountStatus {
    pub configured: bool,
}

/// Public identity returned after the first account is created.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct InitialAccount {
    pub subject: String,
}

/// Result of checking an email/password pair. Password hashes never leave state.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CredentialVerification {
    pub ok: bool,
    pub subject: Option<String>,
}

/// Administrative endpoint provenance persisted with an audit event.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditEndpointRole {
    Host,
    TenantZero,
}

/// Result recorded for an audited command.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AuditOutcome {
    Success,
    Failure,
}

/// Caller and request provenance supplied by an administrative command.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AuditContext {
    pub endpoint_role: AuditEndpointRole,
    pub peer_uid: Option<u32>,
    pub peer_gid: Option<u32>,
    pub peer_pid: Option<u32>,
    pub actor_subject: Option<String>,
    pub request_id: String,
    pub command_kind: String,
    pub request_digest: String,
}

/// One immutable command audit event.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AuditRecord {
    pub id: i64,
    pub recorded_at: String,
    pub endpoint_role: AuditEndpointRole,
    pub peer_uid: Option<u32>,
    pub peer_gid: Option<u32>,
    pub peer_pid: Option<u32>,
    pub actor_subject: Option<String>,
    pub request_id: String,
    pub command_kind: String,
    pub request_digest: String,
    pub outcome: AuditOutcome,
    pub error_code: Option<String>,
}

/// A read-only, deployment-specific candidate for activation or rollback.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActivationPlan {
    pub logical_app: String,
    pub candidate: LoadedApp,
    pub target_deployment_id: String,
    pub target_artifact_hash: String,
    pub expected_active_artifact: Option<String>,
    pub previous_runtime_key: Option<String>,
    pub previous_upstream: Option<PathBuf>,
    pub runtime_key: String,
}

/// Result of an atomic activation commit.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ActivationRecord {
    pub deployment_id: String,
    pub artifact_hash: String,
    pub runtime_key: String,
    pub previous_deployment_id: Option<String>,
    pub previous_artifact_hash: Option<String>,
    pub previous_runtime_key: Option<String>,
}

/// The JSON document accepted by the daemon's apply operation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct NodeConfig {
    pub listen: SocketAddr,
    #[serde(default)]
    pub edge: EdgeConfig,
    #[serde(default)]
    pub apps: Vec<AppConfig>,
}

impl Default for NodeConfig {
    fn default() -> Self {
        Self {
            listen: SocketAddr::from(([127, 0, 0, 1], 3000)),
            edge: EdgeConfig::default(),
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
    #[serde(default)]
    pub tenant_admin: bool,
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
            tenant_admin: false,
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
    pub edge: EdgeConfig,
    pub apps: Vec<LoadedApp>,
}

/// One app projected into the cage and supervisor APIs.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LoadedApp {
    pub name: String,
    pub domains: Vec<String>,
    pub tenant_admin: bool,
    pub upstream: PathBuf,
    pub spec: CageSpec,
    pub lifecycle: LifecycleConfig,
}

/// Public metadata for the configured GitHub App. Secret material is kept separate.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct GitHubAppRecord {
    pub app_id: String,
    pub client_id: String,
    pub name: String,
    pub html_url: Option<String>,
    pub owner: Option<String>,
    pub configured_at: String,
}

/// Secret material for the GitHub App. This type intentionally does not implement
/// `Debug`, `Serialize`, or `Deserialize`, preventing accidental wire/log exposure.
#[derive(Clone, Eq, PartialEq)]
pub struct GitHubAppSecrets {
    pub client_secret: String,
    pub pem: String,
    pub webhook_secret: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitHubRepositoryConfig {
    pub installation_id: i64,
    pub repository_id: i64,
    pub owner: String,
    pub name: String,
    pub branch: String,
    pub app: String,
    pub domain: String,
    pub engine_version: String,
    pub entry: PathBuf,
    pub artifact_root: PathBuf,
    pub upstream: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitHubDelivery {
    pub delivery_id: String,
    pub event: String,
    pub action: Option<String>,
    pub payload_path: PathBuf,
    pub accepted_at: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GitHubJobKind {
    Production,
    Preview,
}

/// Origin of a durable deployment queue item.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeployJobSource {
    #[serde(rename = "github")]
    GitHub,
    Upload,
    Cli,
}

/// Lifecycle state shared by every deployment source.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeployJobStatus {
    Queued,
    Running,
    Succeeded,
    Failed,
    Retry,
    Cancelled,
}

/// Input for the source-neutral deployment queue. GitHub identity and reporting
/// fields are optional so upload and CLI producers do not need synthetic values.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DeployJobSpec {
    pub id: String,
    pub key: String,
    pub source: DeployJobSource,
    pub source_path: PathBuf,
    pub source_ref: String,
    pub app: String,
    pub domain: String,
    pub engine_version: String,
    pub entry: PathBuf,
    pub artifact_root: PathBuf,
    pub upstream: PathBuf,
    pub branch: Option<String>,
    pub commit: Option<String>,
    pub installation_id: Option<i64>,
    pub repository_id: Option<i64>,
    pub owner: Option<String>,
    pub name: Option<String>,
    pub environment: Option<String>,
    pub kind: Option<GitHubJobKind>,
    pub pull_request: Option<i64>,
}

/// One persisted source-neutral deployment queue item.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DeployJob {
    pub id: String,
    pub key: String,
    pub source: DeployJobSource,
    pub source_path: PathBuf,
    pub source_ref: String,
    pub app: String,
    pub domain: String,
    pub engine_version: String,
    pub entry: PathBuf,
    pub artifact_root: PathBuf,
    pub upstream: PathBuf,
    pub branch: Option<String>,
    pub commit: Option<String>,
    pub installation_id: Option<i64>,
    pub repository_id: Option<i64>,
    pub owner: Option<String>,
    pub name: Option<String>,
    pub environment: Option<String>,
    pub kind: Option<GitHubJobKind>,
    pub pull_request: Option<i64>,
    pub status: DeployJobStatus,
    pub attempts: u32,
    pub next_attempt_at: String,
    pub error: Option<String>,
    pub check_run_id: Option<i64>,
    pub github_deployment_id: Option<i64>,
    pub deployment_id: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// Compatibility name retained for existing GitHub callers.
pub type GitHubDeployJobStatus = DeployJobStatus;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitHubJobSpec {
    pub id: String,
    pub key: String,
    pub installation_id: i64,
    pub repository_id: i64,
    pub owner: String,
    pub name: String,
    pub environment: String,
    pub kind: GitHubJobKind,
    pub pull_request: Option<i64>,
    pub sha: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitHubDeployJob {
    pub id: String,
    pub key: String,
    pub installation_id: i64,
    pub repository_id: i64,
    pub owner: String,
    pub name: String,
    pub environment: String,
    pub kind: GitHubJobKind,
    pub pull_request: Option<i64>,
    pub sha: String,
    pub entry: PathBuf,
    pub status: GitHubDeployJobStatus,
    pub attempts: u32,
    pub next_attempt_at: String,
    pub error: Option<String>,
    pub check_run_id: Option<i64>,
    /// GitHub's deployment-report id (not the local Cygnus deployment id).
    pub deployment_id: Option<i64>,
    pub created_at: String,
    pub updated_at: String,
}

impl DeployJob {
    /// Require and materialize the GitHub-only identity/reporting view.
    pub fn into_github(self) -> Result<GitHubDeployJob, StateError> {
        self.try_into()
    }
}

impl TryFrom<DeployJob> for GitHubDeployJob {
    type Error = StateError;

    fn try_from(job: DeployJob) -> Result<Self, Self::Error> {
        if job.source != DeployJobSource::GitHub {
            return Err(StateError::InvalidRecord {
                kind: "github job",
                detail: "job source is not github".into(),
            });
        }
        fn required<T>(value: Option<T>, id: &str, field: &'static str) -> Result<T, StateError> {
            value.ok_or_else(|| {
                StateError::IncompleteState(format!("github job {id:?} is missing {field}"))
            })
        }
        let id = job.id.clone();
        Ok(Self {
            id: job.id,
            key: job.key,
            installation_id: required(job.installation_id, &id, "installation id")?,
            repository_id: required(job.repository_id, &id, "repository id")?,
            owner: required(job.owner, &id, "owner")?,
            name: required(job.name, &id, "repository name")?,
            environment: required(job.environment, &id, "environment")?,
            kind: required(job.kind, &id, "job kind")?,
            pull_request: job.pull_request,
            sha: job.commit.ok_or_else(|| {
                StateError::IncompleteState("github job is missing commit SHA".into())
            })?,
            entry: job.entry,
            status: job.status,
            attempts: job.attempts,
            next_attempt_at: job.next_attempt_at,
            error: job.error,
            check_run_id: job.check_run_id,
            deployment_id: job.github_deployment_id,
            created_at: job.created_at,
            updated_at: job.updated_at,
        })
    }
}

/// Durable state and configuration errors.
#[derive(Debug, Error)]
pub enum StateError {
    #[error("SQLite state error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("state filesystem error: {0}")]
    Io(#[from] std::io::Error),
    #[error("secret authentication failed")]
    SecretAuthentication,
    #[error("password hashing failed: {0}")]
    PasswordHash(#[from] argon2::password_hash::Error),
    #[error("invalid account input: {0}")]
    InvalidAccountInput(String),
    #[error("an account with email {0:?} already exists")]
    DuplicateAccountEmail(String),
    #[error("initial account setup has already been completed")]
    AccountAlreadyConfigured,
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
    #[error(
        "activation CAS conflict for app {app:?}: expected active artifact {expected:?}, found {actual:?}"
    )]
    ActivationConflict {
        app: String,
        expected: Option<String>,
        actual: Option<String>,
    },
    #[error("app {0:?} does not exist")]
    AppNotFound(String),
    #[error("domain {domain:?} is already mapped to app {owner:?}")]
    DomainConflict { domain: String, owner: String },
    #[error("domain {0:?} does not exist")]
    DomainNotFound(String),
    #[error("native domain {0:?} is managed by the configured apex and cannot be removed")]
    NativeDomainImmutable(String),
    #[error("certificate store error: {0}")]
    CertificateStore(#[from] CertificateStoreError),
    #[error("certificate domain {domain:?} is already owned by certificate {owner:?}")]
    CertificateDomainConflict { domain: String, owner: String },
}

/// A SQLite-backed node configuration store.
pub struct State {
    connection: Connection,
    certificate_store: CertificateStore,
    node_key: [u8; NODE_KEY_LEN],
    state_root: PathBuf,
    /// Absolute path to the SQLite database file. Used by long-running deploy
    /// workers to drop and re-open the connection around the build phase so
    /// dashboard admin polls are not blocked for the entire build duration.
    db_path: PathBuf,
}

impl State {
    /// Open or create a state database and apply every ordered migration.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StateError> {
        let path = path.as_ref();
        let certificate_store = CertificateStore::for_state_database(path);
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent)?;
        }
        let state_root = match path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            Some(parent) => fs::canonicalize(parent)?,
            None => fs::canonicalize(".")?,
        };
        let node_key = load_node_key(path)?;
        let mut connection = Connection::open(path)?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "FULL")?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        connection.pragma_update(None, "secure_delete", "ON")?;
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
                2 => migrate_v2_to_v3(&transaction)?,
                3 => migrate_v3_to_v4(&transaction)?,
                4 => migrate_v4_to_v5(&transaction, &node_key)?,
                5 => migrate_v5_to_v6(&transaction)?,
                6 => migrate_v6_to_v7(&transaction)?,
                7 => migrate_v7_to_v8(&transaction)?,
                8 => migrate_v8_to_v9(&transaction)?,
                9 => migrate_v9_to_v10(&transaction)?,
                10 => migrate_v10_to_v11(&transaction)?,
                _ => unreachable!("validated schema version"),
            }
            let next = version + 1;
            transaction.pragma_update(None, "user_version", next)?;
            transaction.commit()?;
            version = next;
        }
        connection.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
        let db_path = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        Ok(Self {
            connection,
            certificate_store,
            node_key,
            state_root,
            db_path,
        })
    }

    /// Canonical parent directory that owns daemon state and deployment data.
    pub fn state_root(&self) -> &Path {
        &self.state_root
    }

    /// Absolute path to the on-disk SQLite database.
    pub fn db_path(&self) -> &Path {
        &self.db_path
    }

    /// Release the on-disk SQLite connection (and its locks) while keeping the
    /// rest of `State` identity. Used by long-running deploy builds so admin
    /// polls can open the database without waiting on the build worker.
    ///
    /// Call [`State::unpark`] before any further state method.
    pub fn park(&mut self) -> Result<(), StateError> {
        // Best-effort checkpoint so readers see committed work; ignore errors
        // from concurrent checkpoints.
        let _ = self
            .connection
            .execute_batch("PRAGMA wal_checkpoint(PASSIVE)");
        // Opening an in-memory database replaces the file-backed connection
        // and drops its file descriptors / locks.
        self.connection = Connection::open_in_memory()?;
        Ok(())
    }

    /// Re-open the on-disk database after [`State::park`].
    pub fn unpark(&mut self) -> Result<(), StateError> {
        *self = Self::open(&self.db_path)?;
        Ok(())
    }

    /// Report whether the node has an account configured for password authentication.
    pub fn account_status(&self) -> Result<AccountStatus, StateError> {
        let configured =
            self.connection
                .query_row("SELECT EXISTS(SELECT 1 FROM accounts)", [], |row| {
                    row.get(0)
                })?;
        Ok(AccountStatus { configured })
    }

    /// Create the node's first account. Once any account exists, setup is permanently closed.
    pub fn create_initial_account(
        &mut self,
        email: &str,
        password: &str,
    ) -> Result<InitialAccount, StateError> {
        self.create_initial_account_inner(email, password, None)
    }

    /// Create the first account and append the successful creation audit atomically.
    pub fn create_initial_account_with_audit(
        &mut self,
        email: &str,
        password: &str,
        audit: &AuditContext,
    ) -> Result<InitialAccount, StateError> {
        validate_audit_context(audit)?;
        self.create_initial_account_inner(email, password, Some(audit))
    }

    fn create_initial_account_inner(
        &mut self,
        email: &str,
        password: &str,
        audit: Option<&AuditContext>,
    ) -> Result<InitialAccount, StateError> {
        let email = normalize_and_validate_account_email(email)?;
        validate_account_password(password)?;
        let password_hash = hash_account_password(password)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let duplicate: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM accounts WHERE email = ?1)",
            params![email],
            |row| row.get(0),
        )?;
        if duplicate {
            return Err(StateError::DuplicateAccountEmail(email));
        }
        let configured: bool =
            transaction.query_row("SELECT EXISTS(SELECT 1 FROM accounts)", [], |row| {
                row.get(0)
            })?;
        if configured {
            return Err(StateError::AccountAlreadyConfigured);
        }
        transaction.execute(
            "INSERT INTO accounts (email, password_hash) VALUES (?1, ?2)",
            params![email, password_hash],
        )?;
        let id = transaction.last_insert_rowid();
        if let Some(audit) = audit {
            append_audit_tx(&transaction, audit, AuditOutcome::Success, None)?;
        }
        transaction.commit()?;
        Ok(InitialAccount {
            subject: account_subject(id),
        })
    }

    /// Verify a bounded email/password pair without exposing the stored password hash.
    pub fn verify_credentials(
        &self,
        email: &str,
        password: &str,
    ) -> Result<CredentialVerification, StateError> {
        let email = normalize_and_validate_account_email(email)?;
        validate_account_password(password)?;
        let account = self
            .connection
            .query_row(
                "SELECT id, password_hash FROM accounts WHERE email = ?1",
                params![email],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?;
        let Some((id, password_hash)) = account else {
            return Ok(CredentialVerification {
                ok: false,
                subject: None,
            });
        };
        let parsed = PasswordHash::new(&password_hash)?;
        let ok = Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok();
        Ok(CredentialVerification {
            ok,
            subject: ok.then(|| account_subject(id)),
        })
    }

    /// Change the account's password after verifying the current one. The
    /// caller (admin handler) is responsible for identifying which account
    /// via `email`; this never trusts a bare subject string for authorization.
    pub fn update_account_password(
        &mut self,
        email: &str,
        current_password: &str,
        new_password: &str,
        audit: &AuditContext,
    ) -> Result<(), StateError> {
        validate_audit_context(audit)?;
        let verification = self.verify_credentials(email, current_password)?;
        if !verification.ok {
            return Err(StateError::InvalidAccountInput(
                "current password is incorrect".into(),
            ));
        }
        validate_account_password(new_password)?;
        let email = normalize_and_validate_account_email(email)?;
        let password_hash = hash_account_password(new_password)?;
        let transaction = self.connection.transaction()?;
        let changed = transaction.execute(
            "UPDATE accounts SET password_hash = ?2 WHERE email = ?1",
            params![email, password_hash],
        )?;
        if changed == 0 {
            return Err(StateError::InvalidAccountInput(format!(
                "no account exists for {email:?}"
            )));
        }
        append_audit_tx(&transaction, audit, AuditOutcome::Success, None)?;
        transaction.commit()?;
        Ok(())
    }

    /// Daemon-owned artifact root for an app deployment.
    pub fn deployment_artifact_root(&self, app: &str) -> PathBuf {
        self.state_root.join("artifacts").join(app)
    }

    /// Daemon-owned upstream base for an app deployment.
    pub fn deployment_upstream(&self, app: &str) -> PathBuf {
        self.state_root.join("upstreams").join(app)
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
    /// Validate and materialize a configuration without changing durable state.
    pub fn preview(&self, config: &NodeConfig) -> Result<Snapshot, StateError> {
        snapshot_from_config(config)
    }
    /// Apply a complete configuration and append its success audit in one transaction.
    pub fn apply_with_audit(
        &mut self,
        config: &NodeConfig,
        audit: &AuditContext,
    ) -> Result<(), StateError> {
        validate_audit_context(audit)?;
        let snapshot = snapshot_from_config(config)?;
        let stored = snapshot_to_stored(&snapshot)?;
        let transaction = self.connection.transaction()?;
        replace_database(&transaction, &stored)?;
        append_audit_tx(&transaction, audit, AuditOutcome::Success, None)?;
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

        let edge = self
            .connection
            .query_row(
                "SELECT https_listen, apps_domain, acme_email, acme_directory_url, dns_provider,
                        dashboard_domain, apex_domain, ssl_mode
                 FROM edge_config WHERE id = 1",
                [],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, Option<String>>(4)?,
                        row.get::<_, Option<String>>(5)?,
                        row.get::<_, Option<String>>(6)?,
                        row.get::<_, String>(7)?,
                    ))
                },
            )
            .optional()?
            .ok_or_else(|| {
                StateError::IncompleteState("singleton edge config is missing".into())
            })?;
        let https_listen = edge
            .0
            .map(|value| {
                value
                    .parse::<SocketAddr>()
                    .map_err(|error| StateError::InvalidPersisted {
                        app: "<edge>".into(),
                        detail: format!("invalid HTTPS listen address: {error}"),
                    })
            })
            .transpose()?;
        let acme = match (edge.2, edge.3) {
            (None, None) => None,
            (Some(email), Some(directory_url)) => Some(AcmeConfig {
                email,
                directory_url,
                dns_provider: edge.4,
            }),
            _ => {
                return Err(StateError::IncompleteState(
                    "ACME email and directory must be stored together".into(),
                ));
            }
        };
        let ssl_mode = match edge.7.as_str() {
            "acme" => SslMode::Acme,
            "self_signed" => SslMode::SelfSigned,
            value => {
                return Err(StateError::InvalidPersisted {
                    app: "<edge>".into(),
                    detail: format!("invalid SSL mode {value:?}"),
                });
            }
        };
        let edge = canonical_edge_config(
            listen,
            &EdgeConfig {
                https_listen,
                apps_domain: edge.1,
                dashboard_domain: edge.5,
                apex_domain: edge.6,
                ssl_mode,
                acme,
            },
        )?;

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
            let mut loaded = loaded_from_stored(&name, &upstream, domains, runtime)?;
            let active_artifact_hash = self
                .connection
                .query_row(
                    "SELECT d.artifact_hash
                     FROM deployments d
                     JOIN app_artifacts aa ON aa.app_id = ?1
                     JOIN artifacts ar ON ar.id = aa.artifact_id AND ar.artifact_hash = d.artifact_hash
                     WHERE d.app = ?2 AND d.status = 'active'
                     LIMIT 1",
                    params![id, name],
                    |row| row.get::<_, String>(0),
                )
                .optional()?;
            if let Some(artifact_hash) = active_artifact_hash {
                loaded.spec.name = format!("r-{artifact_hash}");
            }
            apps.push(loaded);
        }
        let snapshot = Snapshot { listen, edge, apps };
        validate_snapshot(&snapshot)?;
        Ok(snapshot)
    }

    /// Replace public-edge configuration and append its audit event atomically.
    pub fn update_edge_config(
        &mut self,
        edge: &EdgeConfig,
        audit: &AuditContext,
    ) -> Result<EdgeConfig, StateError> {
        validate_audit_context(audit)?;
        let listen = self.load()?.listen;
        let edge = canonical_edge_config(listen, edge)?;
        let transaction = self.connection.transaction()?;
        reconcile_native_domains_tx(&transaction, edge.apex_domain.as_deref(), edge.ssl_mode)?;
        store_edge_config_tx(&transaction, &edge)?;
        append_audit_tx(&transaction, audit, AuditOutcome::Success, None)?;
        transaction.commit()?;
        Ok(edge)
    }

    /// Publish immutable certificate files, then select that generation in SQLite.
    pub fn install_certificate(
        &mut self,
        input: &CertificateInput,
        audit: &AuditContext,
    ) -> Result<CertificateRecord, StateError> {
        validate_audit_context(audit)?;
        if input.not_after_unix <= 0 {
            return Err(StateError::InvalidRecord {
                kind: "certificate",
                detail: "not_after_unix must be positive".into(),
            });
        }
        let domains = canonical_certificate_domains(&input.domains)?;
        for domain in &domains {
            let owner = self
                .connection
                .query_row(
                    "SELECT certificate_id FROM certificate_domains WHERE domain = ?1",
                    [domain],
                    |row| row.get::<_, String>(0),
                )
                .optional()?;
            if let Some(owner) = owner
                && owner != input.id
                && !input.id.starts_with("domain-")
            {
                return Err(StateError::CertificateDomainConflict {
                    domain: domain.clone(),
                    owner,
                });
            }
        }
        let published = self.certificate_store.publish(
            &input.id,
            &input.certificate_pem,
            &input.private_key_pem,
        )?;
        let transaction = self.connection.transaction()?;
        for domain in &domains {
            let owner = transaction
                .query_row(
                    "SELECT certificate_id FROM certificate_domains WHERE domain = ?1",
                    [domain],
                    |row| row.get::<_, String>(0),
                )
                .optional()?;
            if let Some(owner) = owner
                && owner != input.id
            {
                if input.id.starts_with("domain-") {
                    transaction.execute(
                        "DELETE FROM certificate_domains WHERE domain = ?1",
                        [domain],
                    )?;
                } else {
                    return Err(StateError::CertificateDomainConflict {
                        domain: domain.clone(),
                        owner,
                    });
                }
            }
        }
        transaction.execute(
            "INSERT INTO certificates (id, generation, not_after_unix, installed_at)
             VALUES (?1, ?2, ?3, CURRENT_TIMESTAMP)
             ON CONFLICT(id) DO UPDATE SET generation = excluded.generation,
                 not_after_unix = excluded.not_after_unix, installed_at = CURRENT_TIMESTAMP",
            params![input.id, published.generation, input.not_after_unix],
        )?;
        transaction.execute(
            "DELETE FROM certificate_domains WHERE certificate_id = ?1",
            [&input.id],
        )?;
        for domain in &domains {
            transaction.execute(
                "INSERT INTO certificate_domains (certificate_id, domain) VALUES (?1, ?2)",
                params![input.id, domain],
            )?;
        }
        append_audit_tx(&transaction, audit, AuditOutcome::Success, None)?;
        transaction.commit()?;
        self.certificate(&input.id)?.ok_or_else(|| {
            StateError::IncompleteState(format!(
                "certificate {:?} disappeared after installation",
                input.id
            ))
        })
    }

    pub fn certificate(&self, id: &str) -> Result<Option<CertificateRecord>, StateError> {
        let stored = self
            .connection
            .query_row(
                "SELECT generation, not_after_unix, installed_at FROM certificates WHERE id = ?1",
                [id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()?;
        let Some((generation, not_after_unix, installed_at)) = stored else {
            return Ok(None);
        };
        let domains = self.load_certificate_domains(id)?;
        let published = self.certificate_store.resolve(id, &generation)?;
        Ok(Some(CertificateRecord {
            id: id.into(),
            domains,
            generation,
            certificate_path: published.certificate_path,
            private_key_path: published.private_key_path,
            not_after_unix,
            installed_at,
        }))
    }

    pub fn certificates(&self) -> Result<Vec<CertificateRecord>, StateError> {
        let mut statement = self
            .connection
            .prepare("SELECT id FROM certificates ORDER BY id COLLATE BINARY")?;
        let ids = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        ids.into_iter()
            .map(|id| {
                self.certificate(&id)?.ok_or_else(|| {
                    StateError::IncompleteState(format!(
                        "certificate {id:?} disappeared while listing"
                    ))
                })
            })
            .collect()
    }

    fn load_certificate_domains(&self, id: &str) -> Result<Vec<String>, StateError> {
        let mut statement = self.connection.prepare(
            "SELECT domain FROM certificate_domains
             WHERE certificate_id = ?1 ORDER BY domain COLLATE BINARY",
        )?;
        statement
            .query_map([id], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(StateError::from)
    }

    /// Register an operator-trusted engine and return the persisted record.
    ///
    /// The first registered engine becomes the default. After that, an input
    /// with `is_default` set switches the default atomically; otherwise the
    /// existing default is preserved.
    pub fn register_engine(&mut self, engine: &EngineRecord) -> Result<EngineRecord, StateError> {
        validate_engine(engine)?;
        let transaction = self.connection.transaction()?;
        let registered = register_engine_tx(&transaction, engine)?;
        transaction.commit()?;
        Ok(registered)
    }

    /// Register an engine and append its success audit in one transaction.
    pub fn register_engine_with_audit(
        &mut self,
        engine: &EngineRecord,
        audit: &AuditContext,
    ) -> Result<EngineRecord, StateError> {
        validate_audit_context(audit)?;
        validate_engine(engine)?;
        let transaction = self.connection.transaction()?;
        let registered = register_engine_tx(&transaction, engine)?;
        append_audit_tx(&transaction, audit, AuditOutcome::Success, None)?;
        transaction.commit()?;
        Ok(registered)
    }

    pub fn engine(&self, version: &str) -> Result<Option<EngineRecord>, StateError> {
        self.connection
            .query_row(
                "SELECT version, host_root, cage_executable, sha256, is_default
                 FROM engines WHERE version = ?1",
                [version],
                engine_from_row,
            )
            .optional()
            .map_err(StateError::from)
    }

    /// Return the operator-selected default engine, if one is registered.
    pub fn default_engine(&self) -> Result<Option<EngineRecord>, StateError> {
        self.connection
            .query_row(
                "SELECT version, host_root, cage_executable, sha256, is_default
                 FROM engines WHERE is_default = 1",
                [],
                engine_from_row,
            )
            .optional()
            .map_err(StateError::from)
    }

    /// List registered engines in stable version order with active app counts.
    pub fn engines(&self) -> Result<Vec<EngineStatus>, StateError> {
        let mut statement = self.connection.prepare(
            "SELECT e.version, e.host_root, e.cage_executable, e.sha256, e.is_default,
                    COUNT(DISTINCT d.app)
             FROM engines e
             LEFT JOIN deployments d
               ON d.engine_version = e.version AND d.status = 'active'
             GROUP BY e.id
             ORDER BY e.version COLLATE BINARY",
        )?;
        statement
            .query_map([], |row| {
                Ok(EngineStatus {
                    engine: engine_from_row(row)?,
                    app_count: row.get(5)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()
            .map_err(StateError::from)
    }

    /// Select an already-registered engine as the default.
    pub fn set_default_engine(&mut self, version: &str) -> Result<EngineRecord, StateError> {
        let transaction = self.connection.transaction()?;
        let engine = set_default_engine_tx(&transaction, version)?;
        transaction.commit()?;
        Ok(engine)
    }

    /// Select the default engine and append its success audit atomically.
    pub fn set_default_engine_with_audit(
        &mut self,
        version: &str,
        audit: &AuditContext,
    ) -> Result<EngineRecord, StateError> {
        validate_audit_context(audit)?;
        let transaction = self.connection.transaction()?;
        let engine = set_default_engine_tx(&transaction, version)?;
        append_audit_tx(&transaction, audit, AuditOutcome::Success, None)?;
        transaction.commit()?;
        Ok(engine)
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
            "INSERT INTO deployments
             (id, app, source_hash, engine_version, source_kind, source_branch, source_commit, status, error)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'building', NULL)",
            params![
                input.id,
                input.app,
                input.source_hash,
                input.engine_version,
                deployment_source_kind_name(input.source.kind),
                input.source.branch,
                input.source.commit,
            ],
        )?;
        self.deployment(&input.id)?.ok_or_else(|| {
            StateError::IncompleteState(format!(
                "deployment {:?} disappeared after insert",
                input.id
            ))
        })
    }

    /// Resume a preassigned building deployment and replace its provisional
    /// source hash with the hash computed during trusted source intake.
    pub fn resume_building_deployment(
        &mut self,
        input: &DeploymentInput,
    ) -> Result<DeploymentRecord, StateError> {
        validate_deployment_input(input)?;
        let current = self
            .deployment(&input.id)?
            .ok_or_else(|| StateError::InvalidRecord {
                kind: "deployment",
                detail: format!("preassigned deployment {:?} does not exist", input.id),
            })?;
        if current.status != DeploymentStatus::Building {
            return Err(StateError::InvalidRecord {
                kind: "deployment",
                detail: format!("preassigned deployment {:?} is not building", input.id),
            });
        }
        if current.app != input.app
            || current.engine_version != input.engine_version
            || current.source != input.source
        {
            return Err(StateError::InvalidRecord {
                kind: "deployment",
                detail: format!(
                    "preassigned deployment {:?} does not match app, engine, or source provenance",
                    input.id
                ),
            });
        }
        self.connection.execute(
            "UPDATE deployments SET source_hash = ?2, updated_at = CURRENT_TIMESTAMP
             WHERE id = ?1 AND status = 'building'",
            params![input.id, input.source_hash],
        )?;
        self.deployment(&input.id)?.ok_or_else(|| {
            StateError::IncompleteState(format!(
                "deployment {:?} disappeared after source intake",
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
            "UPDATE deployments SET status = 'failed', error = ?2, updated_at = CURRENT_TIMESTAMP WHERE id = ?1",
            params![id, error],
        )?;
        self.deployment(id)?.ok_or_else(|| {
            StateError::IncompleteState(format!("deployment {id:?} disappeared after failure"))
        })
    }

    /// Persist the daemon-owned build log directory for any deployment state.
    pub fn set_deployment_log_path(
        &mut self,
        id: &str,
        log_path: &Path,
    ) -> Result<DeploymentRecord, StateError> {
        validate_absolute_path(log_path, "deployment log path")?;
        let changed = self.connection.execute(
            "UPDATE deployments SET log_path = ?2, updated_at = CURRENT_TIMESTAMP WHERE id = ?1",
            params![id, log_path.to_string_lossy()],
        )?;
        if changed == 0 {
            return Err(StateError::InvalidRecord {
                kind: "deployment",
                detail: format!("deployment {id:?} does not exist"),
            });
        }
        self.deployment(id)?.ok_or_else(|| {
            StateError::IncompleteState(format!("deployment {id:?} disappeared after log update"))
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
        if let Some(existing) = query_artifact_tx(&transaction, &artifact.artifact_hash)? {
            if existing.record.app != artifact.app
                || existing.record.source_hash != artifact.source_hash
                || existing.record.engine_version != artifact.engine_version
                || existing.record.host_path != artifact.host_path
                || !metadata_json_equal(&existing.record.metadata_json, &artifact.metadata_json)
            {
                return Err(StateError::ArtifactOwnership {
                    artifact: artifact.artifact_hash.clone(),
                    deployment: id.to_owned(),
                });
            }
        } else {
            transaction.execute(
                "INSERT INTO artifacts (app, source_hash, artifact_hash, engine_version, host_path, metadata_json, status) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'sealed')",
                params![artifact.app, artifact.source_hash, artifact.artifact_hash, artifact.engine_version, artifact.host_path.to_string_lossy(), artifact.metadata_json],
            )?;
        }
        transaction.execute(
            "UPDATE deployments SET artifact_hash = ?2, status = 'sealed', error = NULL, updated_at = CURRENT_TIMESTAMP WHERE id = ?1",
            params![id, artifact.artifact_hash],
        )?;
        transaction.commit()?;
        self.deployment_artifact(id)?.ok_or_else(|| {
            StateError::IncompleteState(format!(
                "artifact for deployment {id:?} disappeared after sealing"
            ))
        })
    }

    pub fn deployment(&self, id: &str) -> Result<Option<DeploymentRecord>, StateError> {
        self.connection
            .query_row(
                "SELECT id, app, source_hash, engine_version, artifact_hash, status, error,
                    created_at, unixepoch(created_at) * 1000, updated_at, unixepoch(updated_at) * 1000, log_path,
                    source_kind, source_branch, source_commit
             FROM deployments WHERE id = ?1",
                [id],
                deployment_from_row,
            )
            .optional()
            .map_err(StateError::from)
    }

    /// List newest deployments, optionally scoped to one app.
    pub fn deployments(
        &self,
        app: Option<&str>,
        cursor: Option<&str>,
        limit: u16,
    ) -> Result<Vec<DeploymentRecord>, StateError> {
        if limit == 0 || limit > 51 {
            return Err(StateError::InvalidRecord {
                kind: "deployment query",
                detail: "limit must be between 1 and 51".into(),
            });
        }
        if app.is_some_and(|name| name.trim().is_empty()) {
            return Err(StateError::InvalidRecord {
                kind: "deployment query",
                detail: "app filter must be nonempty".into(),
            });
        }
        let before = if let Some(cursor) = cursor {
            let rowid = self
                .connection
                .query_row(
                    "SELECT rowid FROM deployments WHERE id = ?1",
                    [cursor],
                    |row| row.get::<_, i64>(0),
                )
                .optional()?
                .ok_or_else(|| StateError::InvalidRecord {
                    kind: "deployment cursor",
                    detail: format!("deployment cursor {cursor:?} does not exist"),
                })?;
            Some(rowid)
        } else {
            None
        };

        let columns = "id, app, source_hash, engine_version, artifact_hash, status, error, created_at, unixepoch(created_at) * 1000, updated_at, unixepoch(updated_at) * 1000, log_path, source_kind, source_branch, source_commit";
        let mut deployments = Vec::new();
        match (app, before) {
            (Some(app), Some(before)) => {
                let sql = format!(
                    "SELECT {columns} FROM deployments WHERE app = ?1 AND rowid < ?2 ORDER BY rowid DESC LIMIT ?3"
                );
                let mut statement = self.connection.prepare(&sql)?;
                let rows = statement
                    .query_map(params![app, before, i64::from(limit)], deployment_from_row)?;
                for row in rows {
                    deployments.push(row?);
                }
            }
            (Some(app), None) => {
                let sql = format!(
                    "SELECT {columns} FROM deployments WHERE app = ?1 ORDER BY rowid DESC LIMIT ?2"
                );
                let mut statement = self.connection.prepare(&sql)?;
                let rows =
                    statement.query_map(params![app, i64::from(limit)], deployment_from_row)?;
                for row in rows {
                    deployments.push(row?);
                }
            }
            (None, Some(before)) => {
                let sql = format!(
                    "SELECT {columns} FROM deployments WHERE rowid < ?1 ORDER BY rowid DESC LIMIT ?2"
                );
                let mut statement = self.connection.prepare(&sql)?;
                let rows =
                    statement.query_map(params![before, i64::from(limit)], deployment_from_row)?;
                for row in rows {
                    deployments.push(row?);
                }
            }
            (None, None) => {
                let sql = format!("SELECT {columns} FROM deployments ORDER BY rowid DESC LIMIT ?1");
                let mut statement = self.connection.prepare(&sql)?;
                let rows = statement.query_map([i64::from(limit)], deployment_from_row)?;
                for row in rows {
                    deployments.push(row?);
                }
            }
        }
        Ok(deployments)
    }

    /// Resolve the deployment selected by an app's active artifact pointer.
    pub fn active_deployment(
        &self,
        app: &str,
    ) -> Result<Option<ActiveDeploymentRecord>, StateError> {
        self.connection
            .query_row(
                "SELECT d.id, ar.artifact_hash, ar.engine_version
                 FROM apps a
                 JOIN app_artifacts aa ON aa.app_id = a.id
                 JOIN artifacts ar ON ar.id = aa.artifact_id
                 JOIN deployments d ON d.artifact_hash = ar.artifact_hash
                 WHERE a.name = ?1 AND d.status = 'active'",
                [app],
                |row| {
                    Ok(ActiveDeploymentRecord {
                        deployment_id: row.get(0)?,
                        artifact_hash: row.get(1)?,
                        engine_version: row.get(2)?,
                    })
                },
            )
            .optional()
            .map_err(StateError::from)
    }

    /// Return the daemon-owned log directory for a deployment in any state.
    pub fn deployment_logs_dir(&self, id: &str) -> Result<Option<PathBuf>, StateError> {
        self.connection
            .query_row(
                "SELECT log_path FROM deployments WHERE id = ?1",
                [id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()
            .map(|path| path.flatten().map(PathBuf::from))
            .map_err(StateError::from)
    }

    /// Resolve the sealed artifact linked to one deployment ID.
    pub fn deployment_artifact(
        &self,
        deployment_id: &str,
    ) -> Result<Option<ArtifactRecord>, StateError> {
        self.connection
            .query_row(
                "SELECT ar.app, ar.source_hash, ar.artifact_hash, ar.engine_version, ar.host_path, ar.metadata_json
                 FROM deployments d
                 JOIN artifacts ar ON ar.artifact_hash = d.artifact_hash
                 WHERE d.id = ?1 AND ar.status = 'sealed'",
                [deployment_id],
                artifact_record_from_row,
            )
            .optional()
            .map_err(StateError::from)
    }
    /// Validate and materialize a deployment-specific runtime without writes.
    pub fn plan_activation(
        &self,
        deployment_id: &str,
        candidate: &AppConfig,
        expected_active_artifact: Option<&str>,
    ) -> Result<ActivationPlan, StateError> {
        if let Some(expected) = expected_active_artifact {
            validate_hash(expected, "expected active artifact hash")?;
        }
        let snapshot = snapshot_from_config(&NodeConfig {
            listen: SocketAddr::from(([127, 0, 0, 1], 3000)),
            edge: EdgeConfig::default(),
            apps: vec![candidate.clone()],
        })?;
        let mut loaded = snapshot
            .apps
            .into_iter()
            .next()
            .ok_or_else(|| StateError::InvalidConfig("activation app is empty".into()))?;
        let deployment =
            self.deployment(deployment_id)?
                .ok_or_else(|| StateError::InvalidRecord {
                    kind: "deployment",
                    detail: format!("deployment {deployment_id:?} does not exist"),
                })?;
        if deployment.status != DeploymentStatus::Sealed {
            return Err(StateError::InvalidDeploymentTransition {
                id: deployment_id.to_owned(),
                from: deployment.status,
                to: DeploymentStatus::Active,
            });
        }
        let artifact = self.deployment_artifact(deployment_id)?.ok_or_else(|| {
            StateError::IncompleteState(format!(
                "deployment {deployment_id:?} has no retained artifact"
            ))
        })?;
        validate_artifact_record_metadata(&artifact)?;
        if deployment.app != candidate.name || artifact.app != candidate.name {
            return Err(StateError::ArtifactOwnership {
                artifact: artifact.artifact_hash,
                deployment: deployment_id.to_owned(),
            });
        }
        if artifact.source_hash != deployment.source_hash
            || artifact.engine_version != deployment.engine_version
        {
            return Err(StateError::ArtifactOwnership {
                artifact: artifact.artifact_hash,
                deployment: deployment_id.to_owned(),
            });
        }
        let active = self.active_deployment(&candidate.name)?;
        let actual = active.as_ref().map(|active| active.artifact_hash.clone());
        let expected = expected_active_artifact.map(str::to_owned);
        if actual != expected {
            return Err(StateError::ActivationConflict {
                app: candidate.name.clone(),
                expected,
                actual,
            });
        }
        let previous_runtime_key = active
            .as_ref()
            .map(|active| format!("r-{}", active.artifact_hash));
        let previous_upstream = self
            .load()?
            .apps
            .into_iter()
            .find(|app| app.name == candidate.name)
            .map(|app| app.upstream);
        let runtime_key = format!("r-{}", artifact.artifact_hash);
        loaded.spec.name = runtime_key.clone();
        let upstream = revision_upstream(&candidate.upstream, &artifact.artifact_hash)?;
        loaded.upstream = upstream.clone();
        loaded.spec.readiness_uds = Some(upstream.clone());
        for record in match self.app_env_vars(&candidate.name) {
            Ok(records) => records,
            Err(StateError::AppNotFound(_)) => Vec::new(),
            Err(error) => return Err(error),
        } {
            loaded
                .spec
                .env
                .insert(OsString::from(record.key), OsString::from(record.value));
        }
        loaded.spec.env.insert(
            OsString::from("CYGNUS_SOCKET"),
            // Linux cages see the socket through the ingress bind mount at
            // INGRESS_CAGE_DIR; plain-process backends (macOS) share the host
            // view and must bind the host path directly.
            if cfg!(target_os = "linux") {
                runtime_socket_path(&upstream).into_os_string()
            } else {
                upstream.as_os_str().to_owned()
            },
        );
        validate_snapshot(&Snapshot {
            listen: SocketAddr::from(([127, 0, 0, 1], 3000)),
            edge: EdgeConfig::default(),
            apps: vec![loaded.clone()],
        })?;
        Ok(ActivationPlan {
            logical_app: candidate.name.clone(),
            candidate: loaded,
            target_deployment_id: deployment_id.to_owned(),
            target_artifact_hash: artifact.artifact_hash,
            expected_active_artifact: expected_active_artifact.map(str::to_owned),
            previous_runtime_key,
            previous_upstream,
            runtime_key,
        })
    }

    pub fn commit_activation(
        &mut self,
        plan: &ActivationPlan,
        audit: &AuditContext,
    ) -> Result<ActivationRecord, StateError> {
        validate_audit_context(audit)?;
        let expected_runtime_key = format!("r-{}", plan.target_artifact_hash);
        let expected_upstream =
            revision_upstream(&plan.candidate.upstream, &plan.target_artifact_hash)?;
        let expected_runtime_socket = if cfg!(target_os = "linux") {
            runtime_socket_path(&expected_upstream).into_os_string()
        } else {
            expected_upstream.as_os_str().to_owned()
        };
        if plan.candidate.name != plan.logical_app
            || plan.runtime_key != expected_runtime_key
            || plan.candidate.spec.name != expected_runtime_key
            || plan.candidate.upstream != expected_upstream
            || plan
                .candidate
                .spec
                .env
                .get(std::ffi::OsStr::new("CYGNUS_SOCKET"))
                != Some(&expected_runtime_socket)
            || plan.candidate.spec.readiness_uds.as_ref() != Some(&expected_upstream)
        {
            return Err(StateError::InvalidRecord {
                kind: "activation plan",
                detail: "candidate identity or readiness path was modified".into(),
            });
        }
        validate_snapshot(&Snapshot {
            listen: SocketAddr::from(([127, 0, 0, 1], 3000)),
            edge: EdgeConfig::default(),
            apps: vec![plan.candidate.clone()],
        })?;
        let stored_runtime = StoredRuntime::from_app(&plan.candidate)?;
        let stored_upstream = plan.candidate.upstream.to_string_lossy().into_owned();
        let runtime_json = serde_json::to_string(&StoredApp {
            name: &plan.logical_app,
            upstream: &stored_upstream,
            domains: &[],
            runtime: &stored_runtime,
        })
        .map_err(|error| {
            StateError::InvalidConfig(format!("serialize app {:?}: {error}", plan.logical_app))
        })?;
        let transaction = self.connection.transaction()?;
        let target =
            query_deployment_tx(&transaction, &plan.target_deployment_id)?.ok_or_else(|| {
                StateError::InvalidRecord {
                    kind: "deployment",
                    detail: format!("deployment {:?} does not exist", plan.target_deployment_id),
                }
            })?;
        if target.status != DeploymentStatus::Sealed
            || target.app != plan.logical_app
            || target.artifact_hash.as_deref() != Some(plan.target_artifact_hash.as_str())
        {
            return Err(StateError::InvalidDeploymentTransition {
                id: plan.target_deployment_id.clone(),
                from: target.status,
                to: DeploymentStatus::Active,
            });
        }
        let artifact =
            query_artifact_tx(&transaction, &plan.target_artifact_hash)?.ok_or_else(|| {
                StateError::IncompleteState(format!(
                    "deployment {:?} references a missing artifact",
                    plan.target_deployment_id
                ))
            })?;
        if artifact.record.app != plan.logical_app
            || artifact.record.source_hash != target.source_hash
            || artifact.record.engine_version != target.engine_version
        {
            return Err(StateError::ArtifactOwnership {
                artifact: plan.target_artifact_hash.clone(),
                deployment: plan.target_deployment_id.clone(),
            });
        }
        validate_artifact_record_metadata(&artifact.record)?;
        let actual = transaction.query_row("SELECT ar.artifact_hash FROM apps a JOIN app_artifacts aa ON aa.app_id = a.id JOIN artifacts ar ON ar.id = aa.artifact_id WHERE a.name = ?1", [&plan.logical_app], |row| row.get::<_, String>(0)).optional()?;
        if actual != plan.expected_active_artifact {
            return Err(StateError::ActivationConflict {
                app: plan.logical_app.clone(),
                expected: plan.expected_active_artifact.clone(),
                actual,
            });
        }
        let previous = transaction
            .query_row(
                "SELECT id, artifact_hash FROM deployments WHERE app = ?1 AND status = 'active'",
                [&plan.logical_app],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
            )
            .optional()?;
        let app_id = transaction
            .query_row(
                "SELECT id FROM apps WHERE name = ?1",
                [&plan.logical_app],
                |row| row.get::<_, i64>(0),
            )
            .optional()?;
        let app_id = if let Some(app_id) = app_id {
            transaction.execute(
                "UPDATE apps SET upstream = ?2, runtime_json = ?3 WHERE id = ?1",
                params![
                    app_id,
                    plan.candidate.upstream.to_string_lossy(),
                    runtime_json
                ],
            )?;
            app_id
        } else {
            transaction.query_row(
                "INSERT INTO apps (name, upstream, runtime_json) VALUES (?1, ?2, ?3) RETURNING id",
                params![
                    plan.logical_app,
                    plan.candidate.upstream.to_string_lossy(),
                    runtime_json
                ],
                |row| row.get::<_, i64>(0),
            )?
        };
        let (apex, mode) = transaction.query_row(
            "SELECT apex_domain, ssl_mode FROM edge_config WHERE id = 1",
            [],
            |row| Ok((row.get::<_, Option<String>>(0)?, row.get::<_, String>(1)?)),
        )?;
        // Product apps get app.<apex>. The console (tenant-0) is reached only
        // via edge.dashboard_domain / the management listener — never a
        // synthetic tenant-0.<apex> hostname.
        let native = if plan.logical_app == "tenant-0" {
            None
        } else {
            apex.as_deref()
                .map(|apex| native_domain(&plan.logical_app, apex))
                .transpose()?
        };
        for domain in &plan.candidate.domains {
            match domain_owner_tx(&transaction, domain)? {
                Some((owner, _)) if owner != plan.logical_app => {
                    return Err(StateError::DomainConflict {
                        domain: domain.clone(),
                        owner,
                    });
                }
                Some(_) => {}
                None => {
                    let kind = if native.as_deref() == Some(domain.as_str()) {
                        "native"
                    } else {
                        "custom"
                    };
                    transaction.execute(
                        "INSERT INTO domains (app_id, domain, kind, tls, status)
                         VALUES (?1, ?2, ?3, ?4, 'pending')",
                        params![app_id, domain, kind, mode],
                    )?;
                }
            }
        }
        if let Some(native) = native {
            match domain_owner_tx(&transaction, &native)? {
                Some((owner, _)) if owner != plan.logical_app => {
                    return Err(StateError::DomainConflict {
                        domain: native,
                        owner,
                    });
                }
                Some(_) => {}
                None => {
                    transaction.execute(
                        "INSERT INTO domains (app_id, domain, kind, tls, status)
                         VALUES (?1, ?2, 'native', ?3, 'pending')",
                        params![app_id, native, mode],
                    )?;
                }
            }
        }
        transaction.execute("INSERT INTO app_artifacts (app_id, artifact_id, activated_at) VALUES (?1, ?2, CURRENT_TIMESTAMP) ON CONFLICT(app_id) DO UPDATE SET artifact_id = excluded.artifact_id, activated_at = CURRENT_TIMESTAMP", params![app_id, artifact.id])?;
        transaction.execute("UPDATE deployments SET status = 'sealed', updated_at = CURRENT_TIMESTAMP WHERE app = ?1 AND status = 'active'", [&plan.logical_app])?;
        transaction.execute("UPDATE deployments SET status = 'active', error = NULL, updated_at = CURRENT_TIMESTAMP WHERE id = ?1", [&plan.target_deployment_id])?;
        append_audit_tx(&transaction, audit, AuditOutcome::Success, None)?;
        transaction.commit()?;
        Ok(ActivationRecord {
            deployment_id: plan.target_deployment_id.clone(),
            artifact_hash: plan.target_artifact_hash.clone(),
            runtime_key: plan.runtime_key.clone(),
            previous_deployment_id: previous.as_ref().map(|item| item.0.clone()),
            previous_artifact_hash: previous.and_then(|item| item.1),
            previous_runtime_key: plan.previous_runtime_key.clone(),
        })
    }

    /// Plan and commit activation without a live runtime handoff.
    /// Main should use [`State::plan_activation`] and
    /// [`State::commit_activation`] separately when it boots a candidate.
    pub fn activate_deployment(
        &mut self,
        deployment_id: &str,
        candidate: &AppConfig,
        expected_active_artifact: Option<&str>,
        audit: &AuditContext,
    ) -> Result<DeploymentRecord, StateError> {
        let plan = self.plan_activation(deployment_id, candidate, expected_active_artifact)?;
        self.commit_activation(&plan, audit)?;
        self.deployment(deployment_id)?.ok_or_else(|| {
            StateError::IncompleteState(format!(
                "deployment {deployment_id:?} disappeared after activation"
            ))
        })
    }

    /// Build a rollback candidate from current logical settings and target metadata.
    pub fn plan_rollback(
        &self,
        app: &str,
        target_deployment_id: &str,
        expected_active_artifact: &str,
    ) -> Result<ActivationPlan, StateError> {
        if app.trim().is_empty() {
            return Err(StateError::InvalidRecord {
                kind: "rollback",
                detail: "app must be nonempty".into(),
            });
        }
        validate_hash(expected_active_artifact, "expected active artifact hash")?;
        let active = self
            .active_deployment(app)?
            .ok_or_else(|| StateError::InvalidRecord {
                kind: "rollback",
                detail: format!("logical app {app:?} has no active deployment"),
            })?;
        if active.artifact_hash != expected_active_artifact {
            return Err(StateError::ActivationConflict {
                app: app.to_owned(),
                expected: Some(expected_active_artifact.to_owned()),
                actual: Some(active.artifact_hash),
            });
        }
        let deployment =
            self.deployment(target_deployment_id)?
                .ok_or_else(|| StateError::InvalidRecord {
                    kind: "deployment",
                    detail: format!("deployment {target_deployment_id:?} does not exist"),
                })?;
        if deployment.app != app {
            return Err(StateError::ArtifactOwnership {
                artifact: deployment.artifact_hash.unwrap_or_default(),
                deployment: target_deployment_id.to_owned(),
            });
        }
        if deployment.status != DeploymentStatus::Sealed {
            return Err(StateError::InvalidDeploymentTransition {
                id: target_deployment_id.to_owned(),
                from: deployment.status,
                to: DeploymentStatus::Active,
            });
        }
        let artifact = self
            .deployment_artifact(target_deployment_id)?
            .ok_or_else(|| {
                StateError::IncompleteState(format!(
                    "deployment {target_deployment_id:?} has no retained artifact"
                ))
            })?;
        validate_artifact_record_metadata(&artifact)?;
        let engine = self.engine(&deployment.engine_version)?.ok_or_else(|| {
            StateError::IncompleteState(format!(
                "deployment {target_deployment_id:?} references an unregistered engine"
            ))
        })?;
        let current = self
            .load()?
            .apps
            .into_iter()
            .find(|item| item.name == app)
            .ok_or_else(|| StateError::InvalidRecord {
                kind: "rollback",
                detail: format!("logical app {app:?} does not exist"),
            })?;
        let mut candidate = app_config_from_loaded(current)?;
        candidate.command = engine_runtime_command(&engine)?;
        let runtime_entry = metadata_runtime_entry(&artifact.metadata_json)?;
        candidate.args = vec!["--preload".into(), "/cygnus/shim.js".into(), runtime_entry];
        let mut rootfs = candidate.rootfs.unwrap_or_default();
        // Preserve hostlib (if present) and rebuild lowerdirs around the
        // selected engine + artifact. Order: hostlib → engine → artifact.
        let hostlib = rootfs
            .lowerdirs
            .iter()
            .find(|path| {
                path.file_name()
                    .is_some_and(|name| name.as_bytes() == b"hostlib")
            })
            .cloned()
            .or_else(|| {
                engine
                    .host_root
                    .parent()
                    .and_then(|p| p.parent())
                    .map(|state_root| state_root.join("hostlib"))
            });
        let mut lowerdirs = Vec::with_capacity(3);
        if cfg!(target_os = "linux")
            && let Some(hostlib) = hostlib
        {
            lowerdirs.push(hostlib);
        }
        lowerdirs.push(engine.host_root);
        lowerdirs.push(artifact.host_path);
        rootfs.lowerdirs = lowerdirs;
        candidate.rootfs = Some(rootfs);
        self.plan_activation(
            target_deployment_id,
            &candidate,
            Some(expected_active_artifact),
        )
    }

    /// Add one canonical route and its success audit event atomically.
    pub fn map_domain(
        &mut self,
        app: &str,
        domain: &str,
        audit: &AuditContext,
    ) -> Result<String, StateError> {
        validate_audit_context(audit)?;
        let canonical = canonical_domain(domain).ok_or_else(|| {
            StateError::InvalidConfig(format!("invalid DNS host pattern {domain:?}"))
        })?;
        let transaction = self.connection.transaction()?;
        let app_id = transaction
            .query_row("SELECT id FROM apps WHERE name = ?1", [app], |row| {
                row.get::<_, i64>(0)
            })
            .optional()?
            .ok_or_else(|| StateError::AppNotFound(app.to_owned()))?;
        let owner = transaction
            .query_row(
                "SELECT a.name FROM domains d JOIN apps a ON a.id = d.app_id WHERE d.domain = ?1",
                [&canonical],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        match owner.as_deref() {
            Some(existing) if existing != app => {
                return Err(StateError::DomainConflict {
                    domain: canonical,
                    owner: existing.to_owned(),
                });
            }
            Some(_) => {}
            None => {
                let tls = edge_ssl_mode_tx(&transaction)?;
                // If this is exactly the host the apex would derive for this
                // app, it IS the native domain — never create a duplicate
                // "custom" row for what is really the same hostname.
                let apex = transaction.query_row(
                    "SELECT apex_domain FROM edge_config WHERE id = 1",
                    [],
                    |row| row.get::<_, Option<String>>(0),
                )?;
                let is_native = apex
                    .as_deref()
                    .map(|apex| native_domain(app, apex))
                    .transpose()?
                    .as_deref()
                    == Some(canonical.as_str());
                let kind = if is_native { "native" } else { "custom" };
                transaction.execute(
                    "INSERT INTO domains (app_id, domain, kind, tls, status)
                     VALUES (?1, ?2, ?3, ?4, 'pending')",
                    params![app_id, canonical, kind, tls],
                )?;
            }
        }
        append_audit_tx(&transaction, audit, AuditOutcome::Success, None)?;
        transaction.commit()?;
        Ok(canonical)
    }

    /// Return every environment-variable key/value pair set for an app,
    /// decrypted. Sorted by key for stable rendering.
    pub fn app_env_vars(&self, app: &str) -> Result<Vec<EnvVarRecord>, StateError> {
        let app_id: i64 = self
            .connection
            .query_row("SELECT id FROM apps WHERE name = ?1", [app], |row| {
                row.get(0)
            })
            .optional()?
            .ok_or_else(|| StateError::AppNotFound(app.to_owned()))?;
        let mut statement = self.connection.prepare(
            "SELECT key, value FROM env_vars WHERE app_id = ?1 ORDER BY key COLLATE BINARY",
        )?;
        let rows = statement.query_map([app_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
        })?;
        rows.collect::<Result<Vec<_>, rusqlite::Error>>()?
            .into_iter()
            .map(|(key, blob)| {
                Ok(EnvVarRecord {
                    key,
                    value: decrypt_secret(&self.node_key, &blob)?,
                })
            })
            .collect()
    }

    /// Set (insert or overwrite) one environment variable for an app.
    pub fn set_env_var(
        &mut self,
        app: &str,
        key: &str,
        value: &str,
        audit: &AuditContext,
    ) -> Result<(), StateError> {
        validate_audit_context(audit)?;
        validate_env_var_key(key)?;
        if value.len() > MAX_ENV_VAR_VALUE_BYTES {
            return Err(StateError::InvalidRecord {
                kind: "env var",
                detail: format!("value exceeds {MAX_ENV_VAR_VALUE_BYTES} bytes"),
            });
        }
        let encrypted = encrypt_secret(&self.node_key, value)?;
        let transaction = self.connection.transaction()?;
        let app_id = app_id_tx(&transaction, app)?;
        transaction.execute(
            "INSERT INTO env_vars (app_id, key, value, updated_at)
             VALUES (?1, ?2, ?3, CURRENT_TIMESTAMP)
             ON CONFLICT(app_id, key) DO UPDATE SET value = excluded.value, updated_at = CURRENT_TIMESTAMP",
            params![app_id, key, encrypted],
        )?;
        append_audit_tx(&transaction, audit, AuditOutcome::Success, None)?;
        transaction.commit()?;
        Ok(())
    }

    /// Remove one environment variable from an app. Idempotent: removing a
    /// key that does not exist is not an error.
    pub fn remove_env_var(
        &mut self,
        app: &str,
        key: &str,
        audit: &AuditContext,
    ) -> Result<(), StateError> {
        validate_audit_context(audit)?;
        validate_env_var_key(key)?;
        let transaction = self.connection.transaction()?;
        let app_id = app_id_tx(&transaction, app)?;
        transaction.execute(
            "DELETE FROM env_vars WHERE app_id = ?1 AND key = ?2",
            params![app_id, key],
        )?;
        append_audit_tx(&transaction, audit, AuditOutcome::Success, None)?;
        transaction.commit()?;
        Ok(())
    }

    /// Return all application domains, optionally restricted to one app.
    pub fn app_domains(&self, app: Option<&str>) -> Result<Vec<DomainRecord>, StateError> {
        if let Some(app) = app {
            let exists: bool = self.connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM apps WHERE name = ?1)",
                [app],
                |row| row.get(0),
            )?;
            if !exists {
                return Err(StateError::AppNotFound(app.to_owned()));
            }
        }
        let mut statement = self.connection.prepare(
            "SELECT d.domain, a.name, d.kind, d.tls, d.status, d.expires_unix, d.error, d.next_retry_unix, d.is_primary, d.retry_count
             FROM domains d JOIN apps a ON a.id = d.app_id
             WHERE (?1 IS NULL OR a.name = ?1)
             ORDER BY a.name COLLATE BINARY, d.is_primary DESC, d.domain COLLATE BINARY",
        )?;
        let rows = statement.query_map([app], domain_record_from_row)?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StateError::from)
    }

    /// Add one custom domain using the current node SSL baseline.
    pub fn add_custom_domain(
        &mut self,
        app: &str,
        host: &str,
        audit: &AuditContext,
    ) -> Result<DomainRecord, StateError> {
        validate_audit_context(audit)?;
        let host = canonical_exact_domain(host)?;
        let transaction = self.connection.transaction()?;
        let app_id = app_id_tx(&transaction, app)?;
        if let Some((owner, kind)) = domain_owner_tx(&transaction, &host)? {
            if owner != app {
                return Err(StateError::DomainConflict {
                    domain: host,
                    owner,
                });
            }
            if kind == "native" {
                return Err(StateError::NativeDomainImmutable(host));
            }
        } else {
            let tls = edge_ssl_mode_tx(&transaction)?;
            // Same de-dup as map_domain: a "custom" add of the app's own
            // derived native hostname is the native domain, not a second one.
            let apex = transaction.query_row(
                "SELECT apex_domain FROM edge_config WHERE id = 1",
                [],
                |row| row.get::<_, Option<String>>(0),
            )?;
            let is_native = apex
                .as_deref()
                .map(|apex| native_domain(app, apex))
                .transpose()?
                .as_deref()
                == Some(host.as_str());
            let kind = if is_native { "native" } else { "custom" };
            transaction.execute(
                "INSERT INTO domains (app_id, domain, kind, tls, status)
                 VALUES (?1, ?2, ?3, ?4, 'pending')",
                params![app_id, host, kind, tls],
            )?;
        }
        append_audit_tx(&transaction, audit, AuditOutcome::Success, None)?;
        transaction.commit()?;
        self.domain_for_app(app, &host)
    }

    /// Remove one custom domain. Native rows are managed only by apex reconciliation.
    pub fn remove_custom_domain(
        &mut self,
        app: &str,
        host: &str,
        audit: &AuditContext,
    ) -> Result<(), StateError> {
        validate_audit_context(audit)?;
        let host = canonical_exact_domain(host)?;
        let transaction = self.connection.transaction()?;
        app_id_tx(&transaction, app)?;
        let record = transaction
            .query_row(
                "SELECT d.kind FROM domains d JOIN apps a ON a.id = d.app_id
                 WHERE a.name = ?1 AND d.domain = ?2",
                params![app, host],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        match record.as_deref() {
            Some("native") => return Err(StateError::NativeDomainImmutable(host)),
            Some("custom") => {
                transaction.execute(
                    "DELETE FROM domains WHERE domain = ?1 AND kind = 'custom'",
                    [&host],
                )?;
            }
            _ => return Err(StateError::DomainNotFound(host)),
        }
        append_audit_tx(&transaction, audit, AuditOutcome::Success, None)?;
        transaction.commit()?;
        Ok(())
    }

    /// Change one domain's TLS policy and return it to pending reconciliation.
    pub fn set_app_domain_tls(
        &mut self,
        app: &str,
        host: &str,
        tls: DomainTls,
        audit: &AuditContext,
    ) -> Result<DomainRecord, StateError> {
        validate_audit_context(audit)?;
        let host = canonical_exact_domain(host)?;
        let transaction = self.connection.transaction()?;
        app_id_tx(&transaction, app)?;
        let changed = transaction.execute(
            "UPDATE domains SET tls = ?3, status = 'pending', expires_unix = NULL
             WHERE domain = ?2 AND app_id = (SELECT id FROM apps WHERE name = ?1)",
            params![app, host, domain_tls_name(tls)],
        )?;
        if changed == 0 {
            return Err(StateError::DomainNotFound(host));
        }
        append_audit_tx(&transaction, audit, AuditOutcome::Success, None)?;
        transaction.commit()?;
        self.domain_for_app(app, &host)
    }

    /// Atomically set dashboard/apex domains and replace all native app domains.
    ///
    /// The console "Apps domain" field is the public apex used for native app
    /// hostnames (`app.<apex>`). Keep `apps_domain` in lockstep so deploy defaults,
    /// status identity, and native domain reconciliation all see the same value.
    pub fn update_dashboard_domains(
        &mut self,
        dashboard_domain: Option<&str>,
        apex_domain: Option<&str>,
        audit: &AuditContext,
    ) -> Result<EdgeConfig, StateError> {
        validate_audit_context(audit)?;
        let mut edge = self.load()?.edge;
        edge.dashboard_domain = dashboard_domain.map(str::to_owned);
        edge.apex_domain = apex_domain.map(str::to_owned);
        // Operator-facing "apps domain" is the apex; mirror it into apps_domain
        // unless a distinct apps_domain was already configured to something else
        // that is not the install default / previous apex.
        if let Some(apex) = apex_domain {
            edge.apps_domain = Some(apex.to_owned());
        }
        let listen = self.load()?.listen;
        let edge = canonical_edge_config(listen, &edge)?;
        let transaction = self.connection.transaction()?;
        reconcile_native_domains_tx(&transaction, edge.apex_domain.as_deref(), edge.ssl_mode)?;
        store_edge_config_tx(&transaction, &edge)?;
        append_audit_tx(&transaction, audit, AuditOutcome::Success, None)?;
        transaction.commit()?;
        Ok(edge)
    }

    /// Update the node SSL baseline and reset native domains for reconciliation.
    ///
    /// When enabling ACME, `email` seeds or refreshes the Let's Encrypt contact
    /// address. Without a stored email, ACME issuance is a no-op (self-signed
    /// fallback remains). Self-signed mode keeps any existing ACME config so
    /// flipping back does not require re-entering the email.
    pub fn update_ssl_mode(
        &mut self,
        mode: SslMode,
        email: Option<&str>,
        audit: &AuditContext,
    ) -> Result<EdgeConfig, StateError> {
        validate_audit_context(audit)?;
        let snapshot = self.load()?;
        let mut edge = snapshot.edge;
        edge.ssl_mode = mode;
        if mode == SslMode::Acme {
            let contact = email
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
                .or_else(|| edge.acme.as_ref().map(|config| config.email.clone()));
            let Some(contact) = contact else {
                return Err(StateError::InvalidConfig(
                    "ACME requires a contact email (pass email when enabling automatic HTTPS)"
                        .into(),
                ));
            };
            if !contact.contains('@') || contact.contains(char::is_whitespace) {
                return Err(StateError::InvalidConfig(
                    "ACME contact email is invalid".into(),
                ));
            }
            // Runtime already defaults to :443 when https_listen is unset; persist
            // that so config validation and status agree with the live listener.
            if edge.https_listen.is_none() {
                edge.https_listen = Some(SocketAddr::from(([0, 0, 0, 0], 443)));
            }
            let dns_provider = edge.acme.as_ref().and_then(|c| c.dns_provider.clone());
            let directory_url = edge
                .acme
                .as_ref()
                .map(|c| c.directory_url.clone())
                .unwrap_or_else(|| crate::edge::DEFAULT_ACME_DIRECTORY.into());
            edge.acme = Some(AcmeConfig {
                email: contact,
                directory_url,
                dns_provider,
            });
        }
        let transaction = self.connection.transaction()?;
        store_edge_config_tx(&transaction, &edge)?;
        transaction.execute(
            "UPDATE domains SET tls = ?1, status = 'pending', expires_unix = NULL
             WHERE kind = 'native'",
            [ssl_mode_name(mode)],
        )?;
        append_audit_tx(&transaction, audit, AuditOutcome::Success, None)?;
        transaction.commit()?;
        Ok(edge)
    }

    /// Update a domain reconciliation status and expiry atomically with its audit event.
    pub fn update_domain_status(
        &mut self,
        host: &str,
        status: DomainStatus,
        expires_unix: Option<i64>,
        audit: &AuditContext,
    ) -> Result<DomainRecord, StateError> {
        validate_audit_context(audit)?;
        let host = canonical_exact_domain(host)?;
        if expires_unix.is_some_and(|expires| expires <= 0) {
            return Err(StateError::InvalidRecord {
                kind: "domain",
                detail: "expires_unix must be positive when present".into(),
            });
        }
        let transaction = self.connection.transaction()?;
        let changed = transaction.execute(
            "UPDATE domains SET status = ?2, expires_unix = ?3 WHERE domain = ?1",
            params![host, domain_status_name(status), expires_unix],
        )?;
        if changed == 0 {
            return Err(StateError::DomainNotFound(host));
        }
        append_audit_tx(&transaction, audit, AuditOutcome::Success, None)?;
        transaction.commit()?;
        self.domain(&host)
    }

    /// Update a domain's ACME reconciliation outcome: status, expiry, the
    /// last error text (cleared on success), and the earliest next retry
    /// time. Used by the reconciler and the manual "retry now" mutation so
    /// operators can see why issuance failed and when it will try again.
    pub fn update_domain_acme_outcome(
        &mut self,
        host: &str,
        status: DomainStatus,
        expires_unix: Option<i64>,
        error: Option<&str>,
        next_retry_unix: Option<i64>,
        audit: &AuditContext,
    ) -> Result<DomainRecord, StateError> {
        validate_audit_context(audit)?;
        let host = canonical_exact_domain(host)?;
        if expires_unix.is_some_and(|expires| expires <= 0) {
            return Err(StateError::InvalidRecord {
                kind: "domain",
                detail: "expires_unix must be positive when present".into(),
            });
        }
        let transaction = self.connection.transaction()?;
        let changed = transaction.execute(
            "UPDATE domains SET status = ?2, expires_unix = ?3, error = ?4, next_retry_unix = ?5,
                 retry_count = CASE WHEN ?4 IS NULL THEN 0 ELSE retry_count + 1 END
             WHERE domain = ?1",
            params![
                host,
                domain_status_name(status),
                expires_unix,
                error,
                next_retry_unix
            ],
        )?;
        if changed == 0 {
            return Err(StateError::DomainNotFound(host));
        }
        append_audit_tx(&transaction, audit, AuditOutcome::Success, None)?;
        transaction.commit()?;
        self.domain(&host)
    }

    /// Clear a domain's backoff so the reconciler retries on its next pass,
    /// regardless of `next_retry_unix`. Backs the operator-facing "retry now".
    pub fn clear_domain_retry_backoff(
        &mut self,
        host: &str,
        audit: &AuditContext,
    ) -> Result<DomainRecord, StateError> {
        validate_audit_context(audit)?;
        let host = canonical_exact_domain(host)?;
        let transaction = self.connection.transaction()?;
        let changed = transaction.execute(
            "UPDATE domains SET next_retry_unix = NULL, retry_count = 0, status = 'pending'
             WHERE domain = ?1 AND tls = 'acme'",
            [&host],
        )?;
        if changed == 0 {
            return Err(StateError::DomainNotFound(host));
        }
        append_audit_tx(&transaction, audit, AuditOutcome::Success, None)?;
        transaction.commit()?;
        self.domain(&host)
    }

    /// Mark exactly one domain as the app's primary — the hostname shown as
    /// "the" app URL. Clears any previous primary for the same app inside
    /// the same transaction so at most one row is ever primary per app.
    pub fn set_primary_domain(
        &mut self,
        app: &str,
        host: &str,
        audit: &AuditContext,
    ) -> Result<DomainRecord, StateError> {
        validate_audit_context(audit)?;
        let host = canonical_exact_domain(host)?;
        let transaction = self.connection.transaction()?;
        let app_id = app_id_tx(&transaction, app)?;
        let owner = domain_owner_tx(&transaction, &host)?;
        match owner {
            Some((owner, _)) if owner != app => {
                return Err(StateError::DomainConflict {
                    domain: host,
                    owner,
                });
            }
            Some(_) => {}
            None => return Err(StateError::DomainNotFound(host)),
        }
        transaction.execute(
            "UPDATE domains SET is_primary = 0 WHERE app_id = ?1",
            [app_id],
        )?;
        transaction.execute(
            "UPDATE domains SET is_primary = 1 WHERE domain = ?1",
            [&host],
        )?;
        append_audit_tx(&transaction, audit, AuditOutcome::Success, None)?;
        transaction.commit()?;
        self.domain(&host)
    }

    fn domain(&self, host: &str) -> Result<DomainRecord, StateError> {
        self.connection
            .query_row(
                "SELECT d.domain, a.name, d.kind, d.tls, d.status, d.expires_unix, d.error, d.next_retry_unix, d.is_primary, d.retry_count
                 FROM domains d JOIN apps a ON a.id = d.app_id WHERE d.domain = ?1",
                [host],
                domain_record_from_row,
            )
            .optional()?
            .ok_or_else(|| StateError::DomainNotFound(host.to_owned()))
    }

    fn domain_for_app(&self, app: &str, host: &str) -> Result<DomainRecord, StateError> {
        let record = self.domain(host)?;
        if record.app != app {
            return Err(StateError::DomainConflict {
                domain: host.to_owned(),
                owner: record.app,
            });
        }
        Ok(record)
    }

    /// Append one explicit success or failure event.
    pub fn append_audit(
        &mut self,
        context: &AuditContext,
        outcome: AuditOutcome,
        error_code: Option<&str>,
    ) -> Result<AuditRecord, StateError> {
        validate_audit_context(context)?;
        let transaction = self.connection.transaction()?;
        let id = append_audit_tx(&transaction, context, outcome, error_code)?;
        transaction.commit()?;
        self.audit_record(id)?.ok_or_else(|| {
            StateError::IncompleteState(format!("audit event {id} disappeared after insert"))
        })
    }

    /// Return audit events in insertion order.
    pub fn audit_records(&self) -> Result<Vec<AuditRecord>, StateError> {
        let mut statement = self.connection.prepare(
            "SELECT id, recorded_at, endpoint_role, peer_uid, peer_gid, peer_pid,
                    actor_subject, request_id, command_kind, request_digest, outcome, error_code
             FROM audit_log ORDER BY id",
        )?;
        let rows = statement.query_map([], audit_record_from_row)?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StateError::from)
    }

    fn audit_record(&self, id: i64) -> Result<Option<AuditRecord>, StateError> {
        self.connection
            .query_row(
                "SELECT id, recorded_at, endpoint_role, peer_uid, peer_gid, peer_pid,
                        actor_subject, request_id, command_kind, request_digest, outcome, error_code
                 FROM audit_log WHERE id = ?1",
                [id],
                audit_record_from_row,
            )
            .optional()
            .map_err(StateError::from)
    }

    pub fn github_app(&self) -> Result<Option<GitHubAppRecord>, StateError> {
        self.connection.query_row(
            "SELECT app_id, client_id, name, html_url, owner, configured_at FROM github_app WHERE id = 1",
            [], |row| Ok(GitHubAppRecord {
                app_id: row.get(0)?, client_id: row.get(1)?, name: row.get(2)?,
                html_url: row.get(3)?, owner: row.get(4)?, configured_at: row.get(5)?,
            }),
        ).optional().map_err(StateError::from)
    }

    pub fn github_app_secrets(&self) -> Result<Option<GitHubAppSecrets>, StateError> {
        self.connection.query_row(
            "SELECT client_secret, pem, webhook_secret FROM github_app_secrets WHERE app_id = 1",
            [], |row| {
                let client_secret: Vec<u8> = row.get(0)?;
                let pem: Vec<u8> = row.get(1)?;
                let webhook_secret: Vec<u8> = row.get(2)?;
                Ok((client_secret, pem, webhook_secret))
            },
        ).optional()?.map(|(client_secret, pem, webhook_secret)| Ok(GitHubAppSecrets {
            client_secret: decrypt_secret(&self.node_key, &client_secret)?,
            pem: decrypt_secret(&self.node_key, &pem)?,
            webhook_secret: decrypt_secret(&self.node_key, &webhook_secret)?,
        })).transpose()
    }

    pub fn set_github_app(
        &mut self,
        app: &GitHubAppRecord,
        secrets: &GitHubAppSecrets,
    ) -> Result<(), StateError> {
        validate_github_app(app, secrets)?;
        let transaction = self.connection.transaction()?;
        upsert_github_app_tx(&transaction, app, secrets, &self.node_key)?;
        transaction.commit()?;
        Ok(())
    }

    pub fn set_github_app_with_audit(
        &mut self,
        app: &GitHubAppRecord,
        secrets: &GitHubAppSecrets,
        audit: &AuditContext,
    ) -> Result<(), StateError> {
        validate_audit_context(audit)?;
        validate_github_app(app, secrets)?;
        let transaction = self.connection.transaction()?;
        upsert_github_app_tx(&transaction, app, secrets, &self.node_key)?;
        append_audit_tx(&transaction, audit, AuditOutcome::Success, None)?;
        transaction.commit()?;
        Ok(())
    }

    pub fn github_repositories(&self) -> Result<Vec<GitHubRepositoryConfig>, StateError> {
        let mut statement = self.connection.prepare("SELECT installation_id, repository_id, owner, name, branch, app, domain, engine_version, entry, artifact_root, upstream FROM github_repositories WHERE enabled = 1 ORDER BY owner, name")?;
        let rows = statement.query_map([], github_repository_from_row)?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StateError::from)
    }

    pub fn github_repository(
        &self,
        installation_id: i64,
        repository_id: i64,
    ) -> Result<Option<GitHubRepositoryConfig>, StateError> {
        self.connection.query_row(
            "SELECT installation_id, repository_id, owner, name, branch, app, domain, engine_version, entry, artifact_root, upstream FROM github_repositories WHERE installation_id = ?1 AND repository_id = ?2 AND enabled = 1",
            params![installation_id, repository_id], github_repository_from_row,
        ).optional().map_err(StateError::from)
    }

    pub fn configure_github_repository(
        &mut self,
        config: &GitHubRepositoryConfig,
    ) -> Result<(), StateError> {
        validate_github_repository(config)?;
        let transaction = self.connection.transaction()?;
        upsert_github_repository_tx(&transaction, config)?;
        transaction.commit()?;
        Ok(())
    }

    pub fn configure_github_repository_with_audit(
        &mut self,
        config: &GitHubRepositoryConfig,
        audit: &AuditContext,
    ) -> Result<(), StateError> {
        validate_audit_context(audit)?;
        validate_github_repository(config)?;
        let transaction = self.connection.transaction()?;
        upsert_github_repository_tx(&transaction, config)?;
        append_audit_tx(&transaction, audit, AuditOutcome::Success, None)?;
        transaction.commit()?;
        Ok(())
    }

    pub fn github_delivery_exists(&self, delivery_id: &str) -> Result<bool, StateError> {
        Ok(self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM github_deliveries WHERE delivery_id = ?1)",
            [delivery_id],
            |row| row.get(0),
        )?)
    }

    pub fn accept_github_delivery(
        &mut self,
        delivery: &GitHubDelivery,
        jobs: &[GitHubJobSpec],
    ) -> Result<bool, StateError> {
        let mut generic = Vec::with_capacity(jobs.len());
        for job in jobs {
            validate_github_job_spec(job)?;
            let config = self
                .github_repository(job.installation_id, job.repository_id)?
                .ok_or_else(|| StateError::InvalidRecord {
                    kind: "github job",
                    detail: "repository mapping does not exist".into(),
                })?;
            generic.push(DeployJobSpec {
                id: job.id.clone(),
                key: job.key.clone(),
                source: DeployJobSource::GitHub,
                source_path: PathBuf::from(format!("{}/{}", job.owner, job.name)),
                source_ref: job.sha.clone(),
                app: config.app,
                domain: config.domain,
                engine_version: config.engine_version,
                entry: config.entry,
                artifact_root: config.artifact_root,
                upstream: config.upstream,
                branch: Some(config.branch),
                commit: Some(job.sha.clone()),
                installation_id: Some(job.installation_id),
                repository_id: Some(job.repository_id),
                owner: Some(job.owner.clone()),
                name: Some(job.name.clone()),
                environment: Some(job.environment.clone()),
                kind: Some(job.kind.clone()),
                pull_request: job.pull_request,
            });
        }
        self.accept_github_delivery_jobs(delivery, &generic)
    }

    /// Atomically records a GitHub delivery and its already-generalized jobs.
    pub fn accept_github_delivery_jobs(
        &mut self,
        delivery: &GitHubDelivery,
        jobs: &[DeployJobSpec],
    ) -> Result<bool, StateError> {
        validate_github_delivery(delivery)?;
        if jobs.len() > MAX_GITHUB_JOBS_PER_DELIVERY {
            return Err(StateError::InvalidRecord {
                kind: "github delivery",
                detail: "too many jobs in one delivery".into(),
            });
        }
        for job in jobs {
            validate_deploy_job_spec(job)?;
            if job.source != DeployJobSource::GitHub {
                return Err(StateError::InvalidRecord {
                    kind: "github delivery",
                    detail: "delivery contains a non-github job".into(),
                });
            }
        }
        let transaction = self.connection.transaction()?;
        let inserted = transaction.execute(
            "INSERT OR IGNORE INTO github_deliveries (delivery_id, event, action, payload_path, accepted_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![delivery.delivery_id, delivery.event, delivery.action, delivery.payload_path.to_string_lossy(), delivery.accepted_at],
        )?;
        if inserted == 0 {
            transaction.rollback()?;
            return Ok(false);
        }
        for job in jobs {
            enqueue_deploy_job_tx(&transaction, job)?;
        }
        transaction.commit()?;
        Ok(true)
    }

    /// Atomically precreate a building deployment and enqueue the job that will
    /// resume it. The local deployment id is persisted while the job is queued.
    /// Repeating the same job id is idempotent and never creates another
    /// deployment row.
    pub fn enqueue_preassigned_deployment(
        &mut self,
        deployment: &DeploymentInput,
        job: &DeployJobSpec,
    ) -> Result<bool, StateError> {
        validate_deployment_input(deployment)?;
        validate_deploy_job_spec(job)?;
        let deployment_source = match deployment.source.kind {
            DeploymentSourceKind::GitHub => DeployJobSource::GitHub,
            DeploymentSourceKind::Upload => DeployJobSource::Upload,
            DeploymentSourceKind::Cli => DeployJobSource::Cli,
        };
        if deployment.app != job.app
            || deployment.engine_version != job.engine_version
            || deployment_source != job.source
        {
            return Err(StateError::InvalidRecord {
                kind: "deploy job",
                detail: "job target does not match its preassigned deployment".into(),
            });
        }
        if self.engine(&deployment.engine_version)?.is_none() {
            return Err(StateError::InvalidRecord {
                kind: "deployment",
                detail: format!("engine {:?} is not registered", deployment.engine_version),
            });
        }

        let transaction = self.connection.transaction()?;
        let existing: Option<String> = transaction
            .query_row(
                "SELECT id FROM deploy_jobs WHERE id = ?1",
                [&job.id],
                |row| row.get(0),
            )
            .optional()?;
        if existing.is_some() {
            transaction.commit()?;
            return Ok(false);
        }
        transaction.execute(
            "INSERT INTO deployments
             (id, app, source_hash, engine_version, source_kind, source_branch, source_commit, status, error)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'building', NULL)",
            params![
                deployment.id,
                deployment.app,
                deployment.source_hash,
                deployment.engine_version,
                deployment_source_kind_name(deployment.source.kind),
                deployment.source.branch,
                deployment.source.commit,
            ],
        )?;
        enqueue_deploy_job_tx(&transaction, job)?;
        transaction.execute(
            "UPDATE deploy_jobs SET deployment_id = ?2, updated_at = CURRENT_TIMESTAMP
             WHERE id = ?1 AND status = 'queued'",
            params![job.id, deployment.id],
        )?;
        transaction.commit()?;
        Ok(true)
    }

    /// Enqueue one source-neutral deployment. Duplicate ids are idempotent.
    pub fn enqueue_deploy_job(&mut self, job: &DeployJobSpec) -> Result<bool, StateError> {
        validate_deploy_job_spec(job)?;
        let transaction = self.connection.transaction()?;
        let inserted = enqueue_deploy_job_tx(&transaction, job)?;
        transaction.commit()?;
        Ok(inserted)
    }

    /// List source-neutral deployment jobs in stable creation/id order.
    pub fn deploy_jobs(
        &self,
        limit: u16,
        cursor: Option<&str>,
    ) -> Result<Vec<DeployJob>, StateError> {
        let limit = i64::from(limit.clamp(1, 200));
        let mut statement = self.connection.prepare(&format!(
            "{DEPLOY_JOB_SELECT} WHERE (?1 IS NULL OR id > ?1) ORDER BY created_at, id LIMIT ?2"
        ))?;
        let rows = statement.query_map(params![cursor, limit], deploy_job_from_row)?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StateError::from)
    }

    pub fn deploy_job(&self, id: &str) -> Result<Option<DeployJob>, StateError> {
        self.connection
            .query_row(
                &format!("{DEPLOY_JOB_SELECT} WHERE id = ?1"),
                [id],
                deploy_job_from_row,
            )
            .optional()
            .map_err(StateError::from)
    }

    pub fn current_deploy_job(
        &self,
        source: DeployJobSource,
        key: &str,
    ) -> Result<Option<DeployJob>, StateError> {
        self.connection
            .query_row(
                &format!("{DEPLOY_JOB_SELECT} WHERE source_kind = ?1 AND job_key = ?2 AND status <> 'cancelled' ORDER BY created_at DESC, rowid DESC LIMIT 1"),
                params![deploy_job_source_name(source), key],
                deploy_job_from_row,
            )
            .optional()
            .map_err(StateError::from)
    }

    pub fn recover_deploy_jobs(&mut self) -> Result<usize, StateError> {
        let changed = self.connection.execute(
            "UPDATE deploy_jobs SET status = CASE WHEN attempts >= ?1 THEN 'failed' ELSE 'retry' END, next_attempt_at = CURRENT_TIMESTAMP, updated_at = CURRENT_TIMESTAMP, error = COALESCE(error, 'daemon restarted while job was running') WHERE status = 'running'",
            [i64::from(MAX_GITHUB_ATTEMPTS)],
        )?;
        Ok(changed)
    }

    pub fn claim_deploy_job(&mut self) -> Result<Option<DeployJob>, StateError> {
        self.claim_deploy_job_inner(None)
    }

    pub fn claim_deploy_job_for_source(
        &mut self,
        source: DeployJobSource,
    ) -> Result<Option<DeployJob>, StateError> {
        self.claim_deploy_job_inner(Some(source))
    }

    fn claim_deploy_job_inner(
        &mut self,
        source: Option<DeployJobSource>,
    ) -> Result<Option<DeployJob>, StateError> {
        let transaction = self.connection.transaction()?;
        let source = source.map(deploy_job_source_name);
        let id = transaction.query_row(
            "SELECT j.id FROM deploy_jobs j WHERE (?1 IS NULL OR j.source_kind = ?1) AND j.status IN ('queued','retry') AND datetime(j.next_attempt_at) <= CURRENT_TIMESTAMP AND (j.source_kind <> 'github' OR EXISTS (SELECT 1 FROM github_repositories r WHERE r.installation_id = j.installation_id AND r.repository_id = j.repository_id AND r.enabled = 1)) AND NOT EXISTS (SELECT 1 FROM deploy_jobs newer WHERE newer.source_kind = j.source_kind AND newer.job_key = j.job_key AND (newer.created_at > j.created_at OR (newer.created_at = j.created_at AND newer.rowid > j.rowid)) AND newer.status <> 'cancelled') ORDER BY j.next_attempt_at, j.created_at, j.rowid LIMIT 1",
            [source],
            |row| row.get::<_, String>(0),
        ).optional()?;
        let Some(id) = id else {
            transaction.commit()?;
            return Ok(None);
        };
        let changed = transaction.execute(
            "UPDATE deploy_jobs SET status = 'running', attempts = attempts + 1, updated_at = CURRENT_TIMESTAMP WHERE id = ?1 AND status IN ('queued','retry') AND attempts < ?2",
            params![id, i64::from(MAX_GITHUB_ATTEMPTS)],
        )?;
        if changed == 0 {
            transaction.rollback()?;
            return Ok(None);
        }
        let job = transaction.query_row(
            &format!("{DEPLOY_JOB_SELECT} WHERE id = ?1"),
            [&id],
            deploy_job_from_row,
        )?;
        transaction.commit()?;
        Ok(Some(job))
    }

    /// Attach the local Cygnus deployment identity to a running job.
    pub fn attach_deployment_id(
        &mut self,
        id: &str,
        deployment_id: &str,
    ) -> Result<(), StateError> {
        github_text(deployment_id, "deployment id")?;
        self.connection.execute(
            "UPDATE deploy_jobs SET deployment_id = ?2, updated_at = CURRENT_TIMESTAMP WHERE id = ?1 AND status = 'running'",
            params![id, deployment_id],
        )?;
        Ok(())
    }

    pub fn finish_deploy_job(
        &mut self,
        id: &str,
        status: DeployJobStatus,
        error: Option<&str>,
    ) -> Result<(), StateError> {
        if !matches!(
            status,
            DeployJobStatus::Succeeded | DeployJobStatus::Failed | DeployJobStatus::Cancelled
        ) {
            return Err(StateError::InvalidRecord {
                kind: "deploy job",
                detail: "finish requires a terminal status".into(),
            });
        }
        self.connection.execute(
            "UPDATE deploy_jobs SET status = ?2, error = ?3, updated_at = CURRENT_TIMESTAMP WHERE id = ?1 AND status = 'running' AND NOT EXISTS (SELECT 1 FROM deploy_jobs newer WHERE newer.source_kind = deploy_jobs.source_kind AND newer.job_key = deploy_jobs.job_key AND (newer.created_at > deploy_jobs.created_at OR (newer.created_at = deploy_jobs.created_at AND newer.rowid > deploy_jobs.rowid)) AND newer.status <> 'cancelled')",
            params![id, deploy_job_status_name(status), error],
        )?;
        Ok(())
    }

    pub fn retry_deploy_job_with_error(&mut self, id: &str, error: &str) -> Result<(), StateError> {
        validate_job_error(error)?;
        let attempts: i64 = self
            .connection
            .query_row(
                "SELECT attempts FROM deploy_jobs WHERE id = ?1",
                [id],
                |row| row.get(0),
            )
            .optional()?
            .ok_or_else(|| StateError::InvalidRecord {
                kind: "deploy job",
                detail: format!("job {id:?} does not exist"),
            })?;
        let shift = attempts.saturating_sub(1).min(7) as u32;
        let delay = RETRY_BASE_SECONDS
            .saturating_mul(1_i64.checked_shl(shift).unwrap_or(i64::MAX))
            .min(RETRY_MAX_SECONDS);
        self.connection.execute(
            "UPDATE deploy_jobs SET status = CASE WHEN attempts >= ?2 THEN 'failed' ELSE 'retry' END, next_attempt_at = datetime(CURRENT_TIMESTAMP, '+' || ?3 || ' seconds'), error = ?4, updated_at = CURRENT_TIMESTAMP WHERE id = ?1 AND status = 'running' AND NOT EXISTS (SELECT 1 FROM deploy_jobs newer WHERE newer.source_kind = deploy_jobs.source_kind AND newer.job_key = deploy_jobs.job_key AND (newer.created_at > deploy_jobs.created_at OR (newer.created_at = deploy_jobs.created_at AND newer.rowid > deploy_jobs.rowid)) AND newer.status <> 'cancelled')",
            params![id, i64::from(MAX_GITHUB_ATTEMPTS), delay, error],
        )?;
        Ok(())
    }

    pub fn retry_deploy_job(&mut self, id: &str) -> Result<DeployJob, StateError> {
        self.retry_deploy_job_inner(id, None)
    }

    pub fn retry_deploy_job_with_audit(
        &mut self,
        id: &str,
        audit: &AuditContext,
    ) -> Result<DeployJob, StateError> {
        self.retry_deploy_job_inner(id, Some(audit))
    }

    fn retry_deploy_job_inner(
        &mut self,
        id: &str,
        audit: Option<&AuditContext>,
    ) -> Result<DeployJob, StateError> {
        if let Some(audit) = audit {
            validate_audit_context(audit)?;
        }
        let transaction = self.connection.transaction()?;
        let changed = transaction.execute(
            "UPDATE deploy_jobs SET status = 'queued', attempts = 0, next_attempt_at = CURRENT_TIMESTAMP, error = NULL, updated_at = CURRENT_TIMESTAMP WHERE id = ?1 AND status IN ('failed','retry','cancelled') AND NOT EXISTS (SELECT 1 FROM deploy_jobs newer WHERE newer.source_kind = deploy_jobs.source_kind AND newer.job_key = deploy_jobs.job_key AND (newer.created_at > deploy_jobs.created_at OR (newer.created_at = deploy_jobs.created_at AND newer.rowid > deploy_jobs.rowid)) AND newer.status <> 'cancelled')",
            [id],
        )?;
        if changed == 0 {
            transaction.rollback()?;
            return Err(StateError::InvalidRecord {
                kind: "deploy job",
                detail: format!("job {id:?} cannot be retried"),
            });
        }
        transaction.execute(
            "UPDATE deployments SET status = 'building', error = NULL, updated_at = CURRENT_TIMESTAMP
             WHERE id = (SELECT deployment_id FROM deploy_jobs WHERE id = ?1)
               AND status = 'failed' AND artifact_hash IS NULL",
            [id],
        )?;
        if let Some(audit) = audit {
            append_audit_tx(&transaction, audit, AuditOutcome::Success, None)?;
        }
        let job = transaction.query_row(
            &format!("{DEPLOY_JOB_SELECT} WHERE id = ?1"),
            [id],
            deploy_job_from_row,
        )?;
        transaction.commit()?;
        Ok(job)
    }

    // Thin GitHub adapters retained for the admin API and existing workers.
    pub fn github_jobs(
        &self,
        limit: u16,
        cursor: Option<&str>,
    ) -> Result<Vec<GitHubDeployJob>, StateError> {
        self.deploy_jobs(limit, cursor)?
            .into_iter()
            .filter(|job| job.source == DeployJobSource::GitHub)
            .map(GitHubDeployJob::try_from)
            .collect()
    }

    pub fn github_job(&self, id: &str) -> Result<Option<GitHubDeployJob>, StateError> {
        self.deploy_job(id)?
            .map(GitHubDeployJob::try_from)
            .transpose()
    }

    pub fn current_github_job(&self, key: &str) -> Result<Option<GitHubDeployJob>, StateError> {
        self.current_deploy_job(DeployJobSource::GitHub, key)?
            .map(GitHubDeployJob::try_from)
            .transpose()
    }

    pub fn recover_github_jobs(&mut self) -> Result<usize, StateError> {
        self.connection
            .execute(
                "UPDATE deploy_jobs SET status = CASE WHEN attempts >= ?1 THEN 'failed' ELSE 'retry' END, next_attempt_at = CURRENT_TIMESTAMP, updated_at = CURRENT_TIMESTAMP, error = COALESCE(error, 'daemon restarted while job was running') WHERE source_kind = 'github' AND status = 'running'",
                [i64::from(MAX_GITHUB_ATTEMPTS)],
            )
            .map_err(StateError::from)
    }

    pub fn claim_github_job(&mut self) -> Result<Option<GitHubDeployJob>, StateError> {
        self.claim_deploy_job_for_source(DeployJobSource::GitHub)?
            .map(GitHubDeployJob::try_from)
            .transpose()
    }

    pub fn update_github_job_report(
        &mut self,
        id: &str,
        check_run_id: Option<i64>,
        deployment_id: Option<i64>,
    ) -> Result<(), StateError> {
        self.connection.execute(
            "UPDATE deploy_jobs SET check_run_id = COALESCE(?2, check_run_id), github_deployment_id = COALESCE(?3, github_deployment_id), updated_at = CURRENT_TIMESTAMP WHERE id = ?1 AND source_kind = 'github' AND status = 'running'",
            params![id, check_run_id, deployment_id],
        )?;
        Ok(())
    }

    pub fn finish_github_job(
        &mut self,
        id: &str,
        status: GitHubDeployJobStatus,
        error: Option<&str>,
    ) -> Result<(), StateError> {
        self.finish_deploy_job(id, status, error)
    }

    pub fn retry_github_job_with_error(&mut self, id: &str, error: &str) -> Result<(), StateError> {
        self.retry_deploy_job_with_error(id, error)
    }

    pub fn retry_github_job(&mut self, id: &str) -> Result<GitHubDeployJob, StateError> {
        GitHubDeployJob::try_from(self.retry_deploy_job(id)?)
    }

    pub fn retry_github_job_with_audit(
        &mut self,
        id: &str,
        audit: &AuditContext,
    ) -> Result<GitHubDeployJob, StateError> {
        GitHubDeployJob::try_from(self.retry_deploy_job_inner(id, Some(audit))?)
    }

    pub fn reconcile_github_event(
        &mut self,
        event: &str,
        action: Option<&str>,
        installation_id: i64,
        removed_repository_ids: &[i64],
    ) -> Result<(), StateError> {
        let transaction = self.connection.transaction()?;
        if event == "installation" && matches!(action, Some("suspend") | Some("deleted")) {
            transaction.execute(
                "UPDATE github_repositories SET enabled = 0 WHERE installation_id = ?1",
                [installation_id],
            )?;
            transaction.execute("UPDATE deploy_jobs SET status = 'cancelled', updated_at = CURRENT_TIMESTAMP WHERE source_kind = 'github' AND installation_id = ?1 AND status IN ('queued','retry')", [installation_id])?;
        } else if event == "installation"
            && matches!(
                action,
                Some("unsuspend") | Some("created") | Some("new_permissions_accepted")
            )
        {
            transaction.execute(
                "UPDATE github_repositories SET enabled = 1 WHERE installation_id = ?1",
                [installation_id],
            )?;
        } else if event == "installation_repositories" {
            for repository_id in removed_repository_ids {
                transaction.execute("UPDATE github_repositories SET enabled = 0 WHERE installation_id = ?1 AND repository_id = ?2", params![installation_id, repository_id])?;
                transaction.execute("UPDATE deploy_jobs SET status = 'cancelled', updated_at = CURRENT_TIMESTAMP WHERE source_kind = 'github' AND installation_id = ?1 AND repository_id = ?2 AND status IN ('queued','retry')", params![installation_id, repository_id])?;
            }
        }
        transaction.commit()?;
        Ok(())
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

fn canonical_exact_domain(input: &str) -> Result<String, StateError> {
    let domain = canonical_domain(input)
        .ok_or_else(|| StateError::InvalidConfig(format!("invalid DNS hostname {input:?}")))?;
    if domain.starts_with("*.") {
        return Err(StateError::InvalidConfig(
            "application domains must be exact hostnames".into(),
        ));
    }
    Ok(domain)
}

fn native_domain(app: &str, apex: &str) -> Result<String, StateError> {
    canonical_exact_domain(&format!("{app}.{apex}"))
}

fn app_id_tx(transaction: &Transaction<'_>, app: &str) -> Result<i64, StateError> {
    transaction
        .query_row("SELECT id FROM apps WHERE name = ?1", [app], |row| {
            row.get(0)
        })
        .optional()?
        .ok_or_else(|| StateError::AppNotFound(app.to_owned()))
}

fn domain_owner_tx(
    transaction: &Transaction<'_>,
    host: &str,
) -> Result<Option<(String, String)>, StateError> {
    transaction
        .query_row(
            "SELECT a.name, d.kind FROM domains d JOIN apps a ON a.id = d.app_id
             WHERE d.domain = ?1",
            [host],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(StateError::from)
}

fn edge_ssl_mode_tx(transaction: &Transaction<'_>) -> Result<&'static str, StateError> {
    let mode =
        transaction.query_row("SELECT ssl_mode FROM edge_config WHERE id = 1", [], |row| {
            row.get::<_, String>(0)
        })?;
    match mode.as_str() {
        "acme" => Ok("acme"),
        "self_signed" => Ok("self_signed"),
        _ => Err(StateError::IncompleteState("invalid edge SSL mode".into())),
    }
}

fn ssl_mode_name(mode: SslMode) -> &'static str {
    match mode {
        SslMode::Acme => "acme",
        SslMode::SelfSigned => "self_signed",
    }
}

fn domain_tls_name(tls: DomainTls) -> &'static str {
    match tls {
        DomainTls::Acme => "acme",
        DomainTls::SelfSigned => "self_signed",
    }
}

fn domain_status_name(status: DomainStatus) -> &'static str {
    match status {
        DomainStatus::Active => "active",
        DomainStatus::FallbackActive => "fallback_active",
        DomainStatus::Issuing => "issuing",
        DomainStatus::Pending => "pending",
        DomainStatus::Failed => "failed",
    }
}

fn domain_record_from_row(row: &rusqlite::Row<'_>) -> Result<DomainRecord, rusqlite::Error> {
    let kind = match row.get::<_, String>(2)?.as_str() {
        "native" => DomainKind::Native,
        "custom" => DomainKind::Custom,
        value => {
            return Err(rusqlite::Error::InvalidParameterName(format!(
                "domain kind {value}"
            )));
        }
    };
    let tls = match row.get::<_, String>(3)?.as_str() {
        "acme" => DomainTls::Acme,
        "self_signed" => DomainTls::SelfSigned,
        value => {
            return Err(rusqlite::Error::InvalidParameterName(format!(
                "domain TLS {value}"
            )));
        }
    };
    let status = match row.get::<_, String>(4)?.as_str() {
        "active" => DomainStatus::Active,
        "fallback_active" => DomainStatus::FallbackActive,
        "issuing" => DomainStatus::Issuing,
        "pending" => DomainStatus::Pending,
        "failed" => DomainStatus::Failed,
        value => {
            return Err(rusqlite::Error::InvalidParameterName(format!(
                "domain status {value}"
            )));
        }
    };
    Ok(DomainRecord {
        host: row.get(0)?,
        app: row.get(1)?,
        kind,
        tls,
        status,
        expires_unix: row.get(5)?,
        error: row.get(6)?,
        next_retry_unix: row.get(7)?,
        is_primary: row.get::<_, i64>(8)? != 0,
        retry_count: row.get(9)?,
    })
}

fn reconcile_native_domains_tx(
    transaction: &Transaction<'_>,
    apex: Option<&str>,
    mode: SslMode,
) -> Result<(), StateError> {
    transaction.execute("DELETE FROM domains WHERE kind = 'native'", [])?;
    let Some(apex) = apex else {
        return Ok(());
    };
    let mut statement =
        transaction.prepare("SELECT id, name FROM apps ORDER BY name COLLATE BINARY")?;
    let apps = statement
        .query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    for (app_id, app) in apps {
        // tenant-0 is the console process, not a product app. Its public
        // hostname is edge.dashboard_domain only — never invent
        // tenant-0.<apex> alongside the operator's dashboard URL.
        if app == "tenant-0" {
            continue;
        }
        let host = native_domain(&app, apex)?;
        match domain_owner_tx(transaction, &host)? {
            Some((owner, _)) if owner != app => {
                return Err(StateError::DomainConflict {
                    domain: host,
                    owner,
                });
            }
            // A custom row already covers this exact hostname for this same
            // app — that IS the native domain, promote it in place instead
            // of leaving a duplicate/second entry.
            Some((_, kind)) if kind == "custom" => {
                transaction.execute(
                    "UPDATE domains SET kind = 'native' WHERE domain = ?1",
                    [&host],
                )?;
            }
            Some(_) => {}
            None => {
                transaction.execute(
                    "INSERT INTO domains (app_id, domain, kind, tls, status)
                     VALUES (?1, ?2, 'native', ?3, 'pending')",
                    params![app_id, host, ssl_mode_name(mode)],
                )?;
            }
        }
    }
    Ok(())
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

fn migrate_v2_to_v3(connection: &Connection) -> Result<(), StateError> {
    connection.execute_batch(
        "ALTER TABLE deployments RENAME TO deployments_v2;
         CREATE TABLE deployments (
             id TEXT PRIMARY KEY,
             app TEXT NOT NULL,
             source_hash TEXT NOT NULL,
             engine_version TEXT NOT NULL REFERENCES engines(version),
             artifact_hash TEXT REFERENCES artifacts(artifact_hash),
             status TEXT NOT NULL CHECK (status IN ('building', 'failed', 'sealed', 'active')),
             error TEXT,
             log_path TEXT,
             created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
             updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
         );
         INSERT INTO deployments
             (id, app, source_hash, engine_version, artifact_hash, status, error)
         SELECT id, app, source_hash, engine_version, artifact_hash, status, error
         FROM deployments_v2;
         DROP TABLE deployments_v2;
         UPDATE deployments AS d
         SET status = 'sealed', updated_at = CURRENT_TIMESTAMP
         WHERE d.status = 'active'
           AND d.rowid <> COALESCE(
               (SELECT d2.rowid
                FROM deployments d2
                JOIN artifacts ar ON ar.artifact_hash = d2.artifact_hash
                JOIN app_artifacts aa ON aa.artifact_id = ar.id
                JOIN apps a ON a.id = aa.app_id AND a.name = d2.app
                WHERE d2.app = d.app AND d2.status = 'active'
                ORDER BY d2.rowid DESC LIMIT 1),
               (SELECT MAX(d3.rowid)
                FROM deployments d3
                WHERE d3.app = d.app AND d3.status = 'active')
           );
         CREATE INDEX deployments_app ON deployments(app);
         CREATE INDEX deployments_artifact_hash ON deployments(artifact_hash);
         CREATE INDEX deployments_created_at ON deployments(created_at DESC, id DESC);
         CREATE INDEX deployments_status_updated_at
             ON deployments(status, updated_at DESC);
         CREATE UNIQUE INDEX deployments_one_active_per_app
             ON deployments(app) WHERE status = 'active';
         CREATE TABLE audit_log (
             id INTEGER PRIMARY KEY,
             recorded_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
             endpoint_role TEXT NOT NULL CHECK (endpoint_role IN ('host', 'tenant_zero')),
             peer_uid INTEGER,
             peer_gid INTEGER,
             peer_pid INTEGER,
             actor_subject TEXT,
             request_id TEXT NOT NULL,
             command_kind TEXT NOT NULL,
             request_digest TEXT NOT NULL,
             outcome TEXT NOT NULL CHECK (outcome IN ('success', 'failure')),
             error_code TEXT,
             CHECK ((outcome = 'success' AND error_code IS NULL)
                 OR (outcome = 'failure' AND error_code IS NOT NULL))
         );
         CREATE INDEX audit_log_recorded_at ON audit_log(recorded_at DESC, id DESC);
         CREATE TRIGGER audit_log_no_update
             BEFORE UPDATE ON audit_log
             BEGIN SELECT RAISE(ABORT, 'audit log is append-only'); END;
         CREATE TRIGGER audit_log_no_delete
             BEFORE DELETE ON audit_log
             BEGIN SELECT RAISE(ABORT, 'audit log is append-only'); END;",
    )?;
    Ok(())
}

fn migrate_v3_to_v4(connection: &Connection) -> Result<(), StateError> {
    connection.execute_batch(
        "CREATE TABLE edge_config (
             id INTEGER PRIMARY KEY CHECK (id = 1),
             https_listen TEXT,
             apps_domain TEXT,
             acme_email TEXT,
             acme_directory_url TEXT,
             dns_provider TEXT,
             CHECK ((acme_email IS NULL AND acme_directory_url IS NULL AND dns_provider IS NULL)
                 OR (acme_email IS NOT NULL AND acme_directory_url IS NOT NULL))
         );
         INSERT INTO edge_config (id) VALUES (1);
         CREATE TABLE certificates (
             id TEXT PRIMARY KEY,
             generation TEXT NOT NULL,
             not_after_unix INTEGER NOT NULL CHECK (not_after_unix > 0),
             installed_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
         );
         CREATE TABLE certificate_domains (
             certificate_id TEXT NOT NULL REFERENCES certificates(id) ON DELETE CASCADE,
             domain TEXT NOT NULL COLLATE BINARY UNIQUE,
             PRIMARY KEY (certificate_id, domain)
         );
         CREATE INDEX certificate_domains_certificate_id
             ON certificate_domains(certificate_id);",
    )?;
    Ok(())
}
fn github_text(value: &str, field: &'static str) -> Result<(), StateError> {
    if value.trim().is_empty()
        || value.len() > MAX_GITHUB_TEXT_LEN
        || value.chars().any(char::is_control)
    {
        return Err(StateError::InvalidRecord {
            kind: "github",
            detail: format!("{field} must be nonempty and printable"),
        });
    }
    Ok(())
}

fn validate_github_app(
    app: &GitHubAppRecord,
    secrets: &GitHubAppSecrets,
) -> Result<(), StateError> {
    for (value, field) in [
        (&app.app_id, "app id"),
        (&app.client_id, "client id"),
        (&app.name, "name"),
        (&secrets.client_secret, "client secret"),
        (&secrets.webhook_secret, "webhook secret"),
    ] {
        github_text(value, field)?;
    }
    if secrets.pem.trim().is_empty()
        || secrets.pem.len() > MAX_GITHUB_TEXT_LEN
        || secrets.pem.bytes().any(|byte| byte == 0)
    {
        return Err(StateError::InvalidRecord {
            kind: "github",
            detail: "private key must be bounded UTF-8 text".into(),
        });
    }
    if let Some(url) = &app.html_url {
        github_text(url, "html URL")?;
    }
    if let Some(owner) = &app.owner {
        github_text(owner, "owner")?;
    }
    github_text(&app.configured_at, "configured timestamp")
}

fn validate_github_repository(config: &GitHubRepositoryConfig) -> Result<(), StateError> {
    if config.installation_id <= 0 || config.repository_id <= 0 {
        return Err(StateError::InvalidRecord {
            kind: "github repository",
            detail: "installation and repository ids must be positive".into(),
        });
    }
    for (value, field) in [
        (&config.owner, "owner"),
        (&config.name, "name"),
        (&config.branch, "branch"),
        (&config.app, "app"),
        (&config.domain, "domain"),
        (&config.engine_version, "engine version"),
    ] {
        github_text(value, field)?;
    }
    if !config.entry.as_os_str().is_empty()
        && (config.entry.is_absolute()
            || config.entry.components().any(|component| {
                matches!(
                    component,
                    std::path::Component::CurDir
                        | std::path::Component::ParentDir
                        | std::path::Component::Prefix(_)
                )
            }))
    {
        return Err(StateError::InvalidRecord {
            kind: "github repository",
            detail: "entry must be automatic or a relative path without traversal".into(),
        });
    }
    for (path, field) in [
        (&config.artifact_root, "artifact root"),
        (&config.upstream, "upstream"),
    ] {
        validate_absolute_path(path, field)?;
    }
    Ok(())
}

fn validate_github_delivery(delivery: &GitHubDelivery) -> Result<(), StateError> {
    for (value, field) in [
        (&delivery.delivery_id, "delivery id"),
        (&delivery.event, "event"),
        (&delivery.accepted_at, "accepted timestamp"),
    ] {
        github_text(value, field)?;
    }
    if let Some(action) = &delivery.action {
        github_text(action, "action")?;
    }
    if delivery.payload_path.as_os_str().as_bytes().len() > 4096 {
        return Err(StateError::InvalidRecord {
            kind: "github delivery",
            detail: "payload path is too long".into(),
        });
    }
    Ok(())
}

fn validate_github_job_spec(job: &GitHubJobSpec) -> Result<(), StateError> {
    for (value, field) in [
        (&job.id, "job id"),
        (&job.key, "job key"),
        (&job.owner, "owner"),
        (&job.name, "name"),
        (&job.environment, "environment"),
    ] {
        github_text(value, field)?;
    }
    if job.installation_id <= 0 || job.repository_id <= 0 {
        return Err(StateError::InvalidRecord {
            kind: "github job",
            detail: "installation and repository ids must be positive".into(),
        });
    }
    validate_hash(&job.sha, "github SHA")
}

fn validate_deploy_job_spec(job: &DeployJobSpec) -> Result<(), StateError> {
    for (value, field) in [
        (&job.id, "job id"),
        (&job.key, "job key"),
        (&job.source_ref, "source ref"),
        (&job.app, "app"),
        (&job.domain, "domain"),
        (&job.engine_version, "engine version"),
    ] {
        github_text(value, field)?;
    }
    if job.source_path.as_os_str().is_empty()
        || job.source_path.as_os_str().as_bytes().len() > 4096
        || job.source_path.as_os_str().as_bytes().contains(&0)
    {
        return Err(StateError::InvalidRecord {
            kind: "deploy job",
            detail: "source path must be nonempty and bounded".into(),
        });
    }
    if !job.entry.as_os_str().is_empty()
        && (job.entry.is_absolute()
            || job.entry.components().any(|component| {
                matches!(
                    component,
                    std::path::Component::CurDir
                        | std::path::Component::ParentDir
                        | std::path::Component::Prefix(_)
                )
            }))
    {
        return Err(StateError::InvalidRecord {
            kind: "deploy job",
            detail: "entry must be automatic or a relative path without traversal".into(),
        });
    }
    for (path, field) in [
        (&job.artifact_root, "artifact root"),
        (&job.upstream, "upstream"),
    ] {
        validate_absolute_path(path, field)?;
    }
    for (value, field) in [
        (job.branch.as_deref(), "branch"),
        (job.commit.as_deref(), "commit"),
        (job.owner.as_deref(), "owner"),
        (job.name.as_deref(), "name"),
        (job.environment.as_deref(), "environment"),
    ] {
        if let Some(value) = value {
            github_text(value, field)?;
        }
    }
    if job.source == DeployJobSource::GitHub
        && (job.installation_id.is_none()
            || job.repository_id.is_none()
            || job.owner.is_none()
            || job.name.is_none()
            || job.environment.is_none()
            || job.kind.is_none()
            || job.commit.is_none())
    {
        return Err(StateError::InvalidRecord {
            kind: "github job",
            detail: "github identity fields are required for github sources".into(),
        });
    }
    Ok(())
}

fn validate_job_error(error: &str) -> Result<(), StateError> {
    if error.trim().is_empty()
        || error.len() > MAX_GITHUB_TEXT_LEN
        || error.chars().any(char::is_control)
    {
        return Err(StateError::InvalidRecord {
            kind: "deploy job",
            detail: "retry error must be printable".into(),
        });
    }
    Ok(())
}

fn enqueue_deploy_job_tx(
    transaction: &Transaction<'_>,
    job: &DeployJobSpec,
) -> Result<bool, StateError> {
    validate_deploy_job_spec(job)?;
    let inserted = transaction.execute(
        "INSERT OR IGNORE INTO deploy_jobs (id, job_key, source_kind, source_path, source_ref, app, domain, engine_version, entry, artifact_root, upstream, branch, commit_sha, installation_id, repository_id, owner, name, environment, github_kind, pull_request, status, attempts, next_attempt_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, 'queued', 0, CURRENT_TIMESTAMP)",
        params![
            job.id,
            job.key,
            deploy_job_source_name(job.source),
            job.source_path.to_string_lossy(),
            job.source_ref,
            job.app,
            job.domain,
            job.engine_version,
            job.entry.to_string_lossy(),
            job.artifact_root.to_string_lossy(),
            job.upstream.to_string_lossy(),
            job.branch,
            job.commit,
            job.installation_id,
            job.repository_id,
            job.owner,
            job.name,
            job.environment,
            job.kind.as_ref().map(github_job_kind_name),
            job.pull_request,
        ],
    )?;
    if inserted != 0 {
        transaction.execute(
            "UPDATE deploy_jobs SET status = 'cancelled', updated_at = CURRENT_TIMESTAMP WHERE source_kind = ?1 AND job_key = ?2 AND source_ref <> ?3 AND status IN ('queued','retry') AND rowid < (SELECT rowid FROM deploy_jobs WHERE id = ?4)",
            params![deploy_job_source_name(job.source), job.key, job.source_ref, job.id],
        )?;
    }
    Ok(inserted != 0)
}

fn upsert_github_app_tx(
    transaction: &Transaction<'_>,
    app: &GitHubAppRecord,
    secrets: &GitHubAppSecrets,
    key: &[u8; NODE_KEY_LEN],
) -> Result<(), StateError> {
    transaction.execute("INSERT INTO github_app (id, app_id, client_id, name, html_url, owner, configured_at) VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6) ON CONFLICT(id) DO UPDATE SET app_id=excluded.app_id, client_id=excluded.client_id, name=excluded.name, html_url=excluded.html_url, owner=excluded.owner, configured_at=excluded.configured_at", params![app.app_id, app.client_id, app.name, app.html_url, app.owner, app.configured_at])?;
    transaction.execute("INSERT INTO github_app_secrets (app_id, client_secret, pem, webhook_secret) VALUES (1, ?1, ?2, ?3) ON CONFLICT(app_id) DO UPDATE SET client_secret=excluded.client_secret, pem=excluded.pem, webhook_secret=excluded.webhook_secret", params![encrypt_secret(key, &secrets.client_secret)?, encrypt_secret(key, &secrets.pem)?, encrypt_secret(key, &secrets.webhook_secret)?])?;
    Ok(())
}

fn upsert_github_repository_tx(
    transaction: &Transaction<'_>,
    config: &GitHubRepositoryConfig,
) -> Result<(), StateError> {
    transaction.execute("INSERT INTO github_repositories (installation_id, repository_id, owner, name, branch, app, domain, engine_version, entry, artifact_root, upstream, enabled) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 1) ON CONFLICT(installation_id, repository_id) DO UPDATE SET owner=excluded.owner, name=excluded.name, branch=excluded.branch, app=excluded.app, domain=excluded.domain, engine_version=excluded.engine_version, entry=excluded.entry, artifact_root=excluded.artifact_root, upstream=excluded.upstream, enabled=1", params![config.installation_id, config.repository_id, config.owner, config.name, config.branch, config.app, config.domain, config.engine_version, config.entry.to_string_lossy(), config.artifact_root.to_string_lossy(), config.upstream.to_string_lossy()])?;
    Ok(())
}

fn github_repository_from_row(
    row: &rusqlite::Row<'_>,
) -> Result<GitHubRepositoryConfig, rusqlite::Error> {
    Ok(GitHubRepositoryConfig {
        installation_id: row.get(0)?,
        repository_id: row.get(1)?,
        owner: row.get(2)?,
        name: row.get(3)?,
        branch: row.get(4)?,
        app: row.get(5)?,
        domain: row.get(6)?,
        engine_version: row.get(7)?,
        entry: PathBuf::from(row.get::<_, String>(8)?),
        artifact_root: PathBuf::from(row.get::<_, String>(9)?),
        upstream: PathBuf::from(row.get::<_, String>(10)?),
    })
}

fn github_job_kind_name(kind: &GitHubJobKind) -> &'static str {
    match kind {
        GitHubJobKind::Production => "production",
        GitHubJobKind::Preview => "preview",
    }
}
const DEPLOY_JOB_SELECT: &str = "SELECT id, job_key, source_kind, source_path, source_ref, app, domain, engine_version, entry, artifact_root, upstream, branch, commit_sha, installation_id, repository_id, owner, name, environment, github_kind, pull_request, status, attempts, next_attempt_at, error, check_run_id, github_deployment_id, deployment_id, created_at, updated_at FROM deploy_jobs";

fn deploy_job_source_name(source: DeployJobSource) -> &'static str {
    match source {
        DeployJobSource::GitHub => "github",
        DeployJobSource::Upload => "upload",
        DeployJobSource::Cli => "cli",
    }
}

fn deploy_job_status_name(status: DeployJobStatus) -> &'static str {
    match status {
        DeployJobStatus::Queued => "queued",
        DeployJobStatus::Running => "running",
        DeployJobStatus::Succeeded => "succeeded",
        DeployJobStatus::Failed => "failed",
        DeployJobStatus::Retry => "retry",
        DeployJobStatus::Cancelled => "cancelled",
    }
}

fn deploy_job_from_row(row: &rusqlite::Row<'_>) -> Result<DeployJob, rusqlite::Error> {
    let invalid = |index, name: &str| {
        rusqlite::Error::InvalidColumnType(index, name.into(), rusqlite::types::Type::Text)
    };
    let source = match row.get::<_, String>(2)?.as_str() {
        "github" => DeployJobSource::GitHub,
        "upload" => DeployJobSource::Upload,
        "cli" => DeployJobSource::Cli,
        _ => return Err(invalid(2, "source_kind")),
    };
    let kind = match row.get::<_, Option<String>>(18)?.as_deref() {
        Some("production") => Some(GitHubJobKind::Production),
        Some("preview") => Some(GitHubJobKind::Preview),
        None => None,
        _ => return Err(invalid(18, "github_kind")),
    };
    let status = match row.get::<_, String>(20)?.as_str() {
        "queued" => DeployJobStatus::Queued,
        "running" => DeployJobStatus::Running,
        "succeeded" => DeployJobStatus::Succeeded,
        "failed" => DeployJobStatus::Failed,
        "retry" => DeployJobStatus::Retry,
        "cancelled" => DeployJobStatus::Cancelled,
        _ => return Err(invalid(20, "status")),
    };
    Ok(DeployJob {
        id: row.get(0)?,
        key: row.get(1)?,
        source,
        source_path: PathBuf::from(row.get::<_, String>(3)?),
        source_ref: row.get(4)?,
        app: row.get(5)?,
        domain: row.get(6)?,
        engine_version: row.get(7)?,
        entry: PathBuf::from(row.get::<_, String>(8)?),
        artifact_root: PathBuf::from(row.get::<_, String>(9)?),
        upstream: PathBuf::from(row.get::<_, String>(10)?),
        branch: row.get(11)?,
        commit: row.get(12)?,
        installation_id: row.get(13)?,
        repository_id: row.get(14)?,
        owner: row.get(15)?,
        name: row.get(16)?,
        environment: row.get(17)?,
        kind,
        pull_request: row.get(19)?,
        status,
        attempts: row.get::<_, i64>(21)?.try_into().unwrap_or(u32::MAX),
        next_attempt_at: row.get(22)?,
        error: row.get(23)?,
        check_run_id: row.get(24)?,
        github_deployment_id: row.get(25)?,
        deployment_id: row.get(26)?,
        created_at: row.get(27)?,
        updated_at: row.get(28)?,
    })
}

fn load_node_key(db_path: &Path) -> Result<[u8; NODE_KEY_LEN], StateError> {
    let parent = db_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let key_path = parent.join("node.key");
    let mut file = match fs::symlink_metadata(&key_path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
                return Err(StateError::InvalidConfig(
                    "node key must be a regular file".into(),
                ));
            }
            if metadata.permissions().mode() & 0o7777 != 0o600 {
                return Err(StateError::InvalidConfig(
                    "node key permissions must be 0600".into(),
                ));
            }
            OpenOptions::new()
                .read(true)
                .write(false)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&key_path)?
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut key = [0u8; NODE_KEY_LEN];
            random_fill(&mut key)
                .map_err(|error| StateError::Io(std::io::Error::other(error.to_string())))?;
            let mut options = OpenOptions::new();
            options
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW);
            match options.open(&key_path) {
                Ok(mut created) => {
                    use std::io::Write;
                    created.write_all(&key)?;
                    created.sync_all()?;
                    return Ok(key);
                }
                Err(create_error) if create_error.kind() == std::io::ErrorKind::AlreadyExists => {
                    let metadata = fs::symlink_metadata(&key_path)?;
                    if metadata.file_type().is_symlink()
                        || !metadata.file_type().is_file()
                        || metadata.permissions().mode() & 0o7777 != 0o600
                    {
                        return Err(StateError::InvalidConfig(
                            "node key must be a regular 0600 file".into(),
                        ));
                    }
                    OpenOptions::new()
                        .read(true)
                        .custom_flags(libc::O_NOFOLLOW)
                        .open(&key_path)?
                }
                Err(create_error) => return Err(StateError::Io(create_error)),
            }
        }
        Err(error) => return Err(StateError::Io(error)),
    };
    let metadata = file.metadata()?;
    if metadata.len() != NODE_KEY_LEN as u64 {
        return Err(StateError::InvalidConfig(
            "node key must contain exactly 32 bytes".into(),
        ));
    }
    let mut key = [0u8; NODE_KEY_LEN];
    file.read_exact(&mut key)?;
    Ok(key)
}

fn encrypt_secret(key: &[u8; NODE_KEY_LEN], secret: &str) -> Result<Vec<u8>, StateError> {
    let cipher =
        XChaCha20Poly1305::new_from_slice(key).map_err(|_| StateError::SecretAuthentication)?;
    let mut nonce = [0u8; SECRET_NONCE_LEN];
    random_fill(&mut nonce)
        .map_err(|error| StateError::Io(std::io::Error::other(error.to_string())))?;
    let ciphertext = cipher
        .encrypt(
            XNonce::from_slice(&nonce),
            chacha20poly1305::aead::Payload {
                msg: secret.as_bytes(),
                aad: SECRET_AAD,
            },
        )
        .map_err(|_| StateError::SecretAuthentication)?;
    let mut result = Vec::with_capacity(SECRET_NONCE_LEN + ciphertext.len());
    result.extend_from_slice(&nonce);
    result.extend_from_slice(&ciphertext);
    Ok(result)
}

fn decrypt_secret(key: &[u8; NODE_KEY_LEN], blob: &[u8]) -> Result<String, StateError> {
    if blob.len() < SECRET_NONCE_LEN + 16 {
        return Err(StateError::SecretAuthentication);
    }
    let cipher =
        XChaCha20Poly1305::new_from_slice(key).map_err(|_| StateError::SecretAuthentication)?;
    let plaintext = cipher
        .decrypt(
            XNonce::from_slice(&blob[..SECRET_NONCE_LEN]),
            chacha20poly1305::aead::Payload {
                msg: &blob[SECRET_NONCE_LEN..],
                aad: SECRET_AAD,
            },
        )
        .map_err(|_| StateError::SecretAuthentication)?;
    String::from_utf8(plaintext).map_err(|_| StateError::SecretAuthentication)
}

fn create_github_schema_v4(connection: &Connection) -> Result<(), StateError> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS github_app (
             id INTEGER PRIMARY KEY CHECK (id = 1), app_id TEXT NOT NULL, client_id TEXT NOT NULL,
             name TEXT NOT NULL, html_url TEXT, owner TEXT, client_secret TEXT NOT NULL,
             pem TEXT NOT NULL, webhook_secret TEXT NOT NULL,
             configured_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
         );
         CREATE TABLE IF NOT EXISTS github_repositories (
             installation_id INTEGER NOT NULL, repository_id INTEGER NOT NULL,
             owner TEXT NOT NULL, name TEXT NOT NULL, branch TEXT NOT NULL, app TEXT NOT NULL,
             domain TEXT NOT NULL, engine_version TEXT NOT NULL, entry TEXT NOT NULL,
             artifact_root TEXT NOT NULL, upstream TEXT NOT NULL, enabled INTEGER NOT NULL DEFAULT 1,
             PRIMARY KEY (installation_id, repository_id)
         );
         CREATE TABLE IF NOT EXISTS github_deliveries (
             delivery_id TEXT PRIMARY KEY, event TEXT NOT NULL, action TEXT,
             payload_path TEXT NOT NULL, accepted_at TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS github_deploy_jobs (
             id TEXT PRIMARY KEY, job_key TEXT NOT NULL, installation_id INTEGER NOT NULL,
             repository_id INTEGER NOT NULL, owner TEXT NOT NULL, name TEXT NOT NULL,
             environment TEXT NOT NULL, kind TEXT NOT NULL CHECK (kind IN ('production','preview')),
             pull_request INTEGER, sha TEXT NOT NULL, status TEXT NOT NULL
                CHECK (status IN ('queued','running','succeeded','failed','retry','cancelled')),
             attempts INTEGER NOT NULL DEFAULT 0, next_attempt_at TEXT NOT NULL,
             error TEXT, check_run_id INTEGER, deployment_id INTEGER,
             created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
             updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
             UNIQUE(job_key, sha)
         );
         CREATE INDEX IF NOT EXISTS github_jobs_due ON github_deploy_jobs(status, next_attempt_at, created_at);
         CREATE INDEX IF NOT EXISTS github_jobs_key ON github_deploy_jobs(job_key, created_at DESC);
         CREATE INDEX IF NOT EXISTS github_repositories_enabled ON github_repositories(enabled);",
    )?;
    Ok(())
}

fn migrate_v4_to_v5(connection: &Connection, key: &[u8; NODE_KEY_LEN]) -> Result<(), StateError> {
    create_github_schema_v4(connection)?;
    let old_exists: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'github_app')",
        [],
        |row| row.get(0),
    )?;
    let old = if old_exists {
        connection.execute_batch("ALTER TABLE github_app RENAME TO github_app_v4")?;
        connection.query_row(
            "SELECT app_id, client_id, name, html_url, owner, client_secret, pem, webhook_secret, configured_at FROM github_app_v4 WHERE id = 1",
            [], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?, row.get::<_, Option<String>>(3)?, row.get::<_, Option<String>>(4)?, row.get::<_, String>(5)?, row.get::<_, String>(6)?, row.get::<_, String>(7)?, row.get::<_, String>(8)?))).optional()?
    } else {
        None
    };
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS github_app (
             id INTEGER PRIMARY KEY CHECK (id = 1), app_id TEXT NOT NULL, client_id TEXT NOT NULL,
             name TEXT NOT NULL, html_url TEXT, owner TEXT,
             configured_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
         );
         CREATE TABLE IF NOT EXISTS github_app_secrets (
             app_id INTEGER PRIMARY KEY REFERENCES github_app(id) ON DELETE CASCADE,
             client_secret BLOB NOT NULL, pem BLOB NOT NULL, webhook_secret BLOB NOT NULL
         );
         CREATE INDEX IF NOT EXISTS github_repositories_enabled ON github_repositories(enabled);
         CREATE INDEX IF NOT EXISTS github_jobs_due ON github_deploy_jobs(status, next_attempt_at, created_at);
         CREATE INDEX IF NOT EXISTS github_jobs_key ON github_deploy_jobs(job_key, created_at DESC);",
    )?;
    if let Some((
        app_id,
        client_id,
        name,
        html_url,
        owner,
        client_secret,
        pem,
        webhook_secret,
        configured_at,
    )) = old
    {
        let encrypted_client = encrypt_secret(key, &client_secret)?;
        let encrypted_pem = encrypt_secret(key, &pem)?;
        let encrypted_webhook = encrypt_secret(key, &webhook_secret)?;
        connection.execute(
            "INSERT OR REPLACE INTO github_app (id, app_id, client_id, name, html_url, owner, configured_at) VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6)",
            params![app_id, client_id, name, html_url, owner, configured_at])?;
        connection.execute(
            "INSERT OR REPLACE INTO github_app_secrets (app_id, client_secret, pem, webhook_secret) VALUES (1, ?1, ?2, ?3)",
            params![encrypted_client, encrypted_pem, encrypted_webhook])?;
    }
    if old_exists {
        connection.execute_batch("DROP TABLE github_app_v4")?;
    }
    Ok(())
}

fn migrate_v5_to_v6(connection: &Connection) -> Result<(), StateError> {
    connection.execute_batch(
        "ALTER TABLE engines
             ADD COLUMN is_default INTEGER NOT NULL DEFAULT 0
             CHECK (is_default IN (0, 1));
         UPDATE engines
             SET is_default = 1
             WHERE id = (SELECT id FROM engines ORDER BY id ASC LIMIT 1);
         CREATE UNIQUE INDEX engines_one_default
             ON engines(is_default) WHERE is_default = 1;",
    )?;
    Ok(())
}

fn migrate_v6_to_v7(connection: &Connection) -> Result<(), StateError> {
    connection.execute_batch(
        "CREATE TABLE deploy_jobs (
             id TEXT PRIMARY KEY,
             job_key TEXT NOT NULL,
             source_kind TEXT NOT NULL CHECK (source_kind IN ('github','upload','cli')),
             source_path TEXT NOT NULL,
             source_ref TEXT NOT NULL,
             app TEXT NOT NULL,
             domain TEXT NOT NULL,
             engine_version TEXT NOT NULL,
             entry TEXT NOT NULL,
             artifact_root TEXT NOT NULL,
             upstream TEXT NOT NULL,
             branch TEXT,
             commit_sha TEXT,
             installation_id INTEGER,
             repository_id INTEGER,
             owner TEXT,
             name TEXT,
             environment TEXT,
             github_kind TEXT CHECK (github_kind IS NULL OR github_kind IN ('production','preview')),
             pull_request INTEGER,
             status TEXT NOT NULL CHECK (status IN ('queued','running','succeeded','failed','retry','cancelled')),
             attempts INTEGER NOT NULL DEFAULT 0,
             next_attempt_at TEXT NOT NULL,
             error TEXT,
             check_run_id INTEGER,
             github_deployment_id INTEGER,
             deployment_id TEXT,
             created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
             updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
         );
         INSERT INTO deploy_jobs (
             id, job_key, source_kind, source_path, source_ref,
             app, domain, engine_version, entry, artifact_root, upstream, branch, commit_sha,
             installation_id, repository_id, owner, name, environment, github_kind, pull_request,
             status, attempts, next_attempt_at, error, check_run_id, github_deployment_id,
             created_at, updated_at
         )
         SELECT j.id, j.job_key, 'github', j.owner || '/' || j.name, j.sha,
                COALESCE(r.app, ''), COALESCE(r.domain, ''), COALESCE(r.engine_version, ''),
                COALESCE(r.entry, ''), COALESCE(r.artifact_root, ''), COALESCE(r.upstream, ''),
                r.branch, j.sha,
                j.installation_id, j.repository_id, j.owner, j.name, j.environment, j.kind,
                j.pull_request, j.status, j.attempts, j.next_attempt_at, j.error,
                j.check_run_id, j.deployment_id, j.created_at, j.updated_at
           FROM github_deploy_jobs j
           LEFT JOIN github_repositories r
             ON r.installation_id = j.installation_id AND r.repository_id = j.repository_id;
         DROP TABLE github_deploy_jobs;
         CREATE INDEX deploy_jobs_due ON deploy_jobs(status, next_attempt_at, created_at);
         CREATE INDEX deploy_jobs_key ON deploy_jobs(source_kind, job_key, created_at DESC);
         CREATE INDEX deploy_jobs_github_repository
             ON deploy_jobs(installation_id, repository_id) WHERE source_kind = 'github';",
    )?;
    Ok(())
}

fn migrate_v7_to_v8(connection: &Connection) -> Result<(), StateError> {
    connection.execute_batch(
        "ALTER TABLE deployments
             ADD COLUMN source_kind TEXT NOT NULL DEFAULT 'cli'
             CHECK (source_kind IN ('github', 'upload', 'cli'));
         ALTER TABLE deployments ADD COLUMN source_branch TEXT;
         ALTER TABLE deployments ADD COLUMN source_commit TEXT;
         UPDATE deployments SET source_kind = 'cli';",
    )?;
    Ok(())
}

fn migrate_v8_to_v9(connection: &Connection) -> Result<(), StateError> {
    connection.execute_batch(
        "CREATE TABLE accounts (
             id INTEGER PRIMARY KEY,
             email TEXT NOT NULL COLLATE BINARY UNIQUE
                 CHECK (email = lower(email) AND email = trim(email)),
             password_hash TEXT NOT NULL,
             created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
         );",
    )?;
    Ok(())
}

fn migrate_v9_to_v10(connection: &Connection) -> Result<(), StateError> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS edge_config (
             id INTEGER PRIMARY KEY CHECK (id = 1),
             https_listen TEXT,
             apps_domain TEXT,
             acme_email TEXT,
             acme_directory_url TEXT,
             dns_provider TEXT
         );
         INSERT OR IGNORE INTO edge_config (id) VALUES (1);
         ALTER TABLE edge_config ADD COLUMN dashboard_domain TEXT;
         ALTER TABLE edge_config ADD COLUMN apex_domain TEXT;
         ALTER TABLE edge_config ADD COLUMN ssl_mode TEXT NOT NULL DEFAULT 'self_signed'
             CHECK (ssl_mode IN ('acme', 'self_signed'));
         ALTER TABLE domains RENAME TO domains_v9;
         CREATE TABLE domains (
             id INTEGER PRIMARY KEY,
             app_id INTEGER NOT NULL REFERENCES apps(id) ON DELETE CASCADE,
             domain TEXT NOT NULL COLLATE BINARY UNIQUE,
             kind TEXT NOT NULL CHECK (kind IN ('native', 'custom')),
             tls TEXT NOT NULL CHECK (tls IN ('acme', 'self_signed')),
             status TEXT NOT NULL CHECK (status IN ('active', 'fallback_active', 'issuing', 'pending', 'failed')),
             expires_unix INTEGER CHECK (expires_unix IS NULL OR expires_unix > 0)
         );
         INSERT INTO domains (id, app_id, domain, kind, tls, status, expires_unix)
             SELECT id, app_id, domain, 'custom', 'self_signed', 'pending', NULL
             FROM domains_v9;
         DROP TABLE domains_v9;
         CREATE INDEX domains_app_id ON domains(app_id);
         CREATE INDEX domains_tls_status ON domains(tls, status);",
    )?;
    Ok(())
}

fn migrate_v10_to_v11(connection: &Connection) -> Result<(), StateError> {
    connection.execute_batch(
        "ALTER TABLE domains ADD COLUMN error TEXT;
         ALTER TABLE domains ADD COLUMN next_retry_unix INTEGER;
         ALTER TABLE domains ADD COLUMN retry_count INTEGER NOT NULL DEFAULT 0;
         ALTER TABLE domains ADD COLUMN is_primary INTEGER NOT NULL DEFAULT 0;
         CREATE UNIQUE INDEX domains_primary_per_app ON domains(app_id) WHERE is_primary = 1;
         CREATE TABLE env_vars (
             id INTEGER PRIMARY KEY,
             app_id INTEGER NOT NULL REFERENCES apps(id) ON DELETE CASCADE,
             key TEXT NOT NULL CHECK (key GLOB '[A-Za-z_][A-Za-z0-9_]*'),
             value BLOB NOT NULL,
             created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
             updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
             UNIQUE (app_id, key)
         );
         CREATE INDEX env_vars_app_id ON env_vars(app_id);",
    )?;
    Ok(())
}

fn normalize_and_validate_account_email(email: &str) -> Result<String, StateError> {
    let normalized = email.trim().to_lowercase();
    if normalized.is_empty() || normalized.len() > MAX_ACCOUNT_EMAIL_BYTES {
        return Err(StateError::InvalidAccountInput(format!(
            "email must be between 1 and {MAX_ACCOUNT_EMAIL_BYTES} bytes"
        )));
    }
    if normalized
        .chars()
        .any(|character| character.is_control() || character.is_whitespace())
    {
        return Err(StateError::InvalidAccountInput(
            "email contains whitespace or control characters".into(),
        ));
    }
    let Some((local, domain)) = normalized.split_once('@') else {
        return Err(StateError::InvalidAccountInput(
            "email must contain a local part and domain".into(),
        ));
    };
    if local.is_empty() || domain.is_empty() || domain.contains('@') {
        return Err(StateError::InvalidAccountInput(
            "email must contain a local part and domain".into(),
        ));
    }
    Ok(normalized)
}

fn validate_account_password(password: &str) -> Result<(), StateError> {
    if !(MIN_ACCOUNT_PASSWORD_BYTES..=MAX_ACCOUNT_PASSWORD_BYTES).contains(&password.len()) {
        return Err(StateError::InvalidAccountInput(format!(
            "password must be between {MIN_ACCOUNT_PASSWORD_BYTES} and {MAX_ACCOUNT_PASSWORD_BYTES} bytes"
        )));
    }
    if password.chars().any(char::is_control) {
        return Err(StateError::InvalidAccountInput(
            "password contains control characters".into(),
        ));
    }
    Ok(())
}

/// Reject env var keys that collide with names the daemon injects itself
/// (`CYGNUS_SOCKET`) or that would be nonsensical/dangerous to override
/// (`PATH`, `HOME`). The SQLite CHECK constraint already enforces the
/// `[A-Za-z_][A-Za-z0-9_]*` shape; this adds the semantic reservations.
pub(crate) fn validate_env_var_key(key: &str) -> Result<(), StateError> {
    if key.is_empty()
        || !key
            .chars()
            .next()
            .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        || !key
            .chars()
            .all(|char| char.is_ascii_alphanumeric() || char == '_')
    {
        return Err(StateError::InvalidRecord {
            kind: "env var",
            detail: format!("key {key:?} must match [A-Za-z_][A-Za-z0-9_]*"),
        });
    }
    const RESERVED: &[&str] = &["CYGNUS_SOCKET", "PATH", "HOME"];
    if RESERVED.contains(&key) {
        return Err(StateError::InvalidRecord {
            kind: "env var",
            detail: format!("{key} is reserved by the daemon"),
        });
    }
    Ok(())
}

fn hash_account_password(password: &str) -> Result<String, StateError> {
    let mut salt = [0_u8; ACCOUNT_SALT_BYTES];
    random_fill(&mut salt).map_err(|error| {
        StateError::Io(std::io::Error::other(format!(
            "could not generate account password salt: {error}"
        )))
    })?;
    let salt = SaltString::encode_b64(&salt)?;
    Ok(Argon2::default()
        .hash_password(password.as_bytes(), &salt)?
        .to_string())
}

fn account_subject(id: i64) -> String {
    format!("account:{id}")
}

fn register_engine_tx(
    transaction: &Transaction<'_>,
    engine: &EngineRecord,
) -> Result<EngineRecord, StateError> {
    let has_default: bool = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM engines WHERE is_default = 1)",
        [],
        |row| row.get(0),
    )?;
    let is_default = engine.is_default || !has_default;
    if is_default {
        transaction.execute("UPDATE engines SET is_default = 0 WHERE is_default = 1", [])?;
    }
    // Re-registering a version updates its host root, executable, and hash in
    // place instead of failing. Every reinstall or upgrade ships a fresh bundled
    // engine binary (new sha256), so idempotent registration is what lets the
    // installer run repeatedly. The version is the stable key that artifacts and
    // deployments reference, so it is never rewritten here.
    transaction.execute(
        "INSERT INTO engines
             (version, host_root, cage_executable, sha256, is_default)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(version) DO UPDATE SET
             host_root = excluded.host_root,
             cage_executable = excluded.cage_executable,
             sha256 = excluded.sha256,
             is_default = excluded.is_default OR engines.is_default",
        params![
            engine.version,
            engine.host_root.to_string_lossy(),
            engine.cage_executable.to_string_lossy(),
            engine.sha256,
            is_default,
        ],
    )?;
    transaction
        .query_row(
            "SELECT version, host_root, cage_executable, sha256, is_default
             FROM engines WHERE version = ?1",
            [engine.version.as_str()],
            engine_from_row,
        )
        .map_err(StateError::from)
}

fn set_default_engine_tx(
    transaction: &Transaction<'_>,
    version: &str,
) -> Result<EngineRecord, StateError> {
    let exists: bool = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM engines WHERE version = ?1)",
        [version],
        |row| row.get(0),
    )?;
    if !exists {
        return Err(StateError::InvalidRecord {
            kind: "engine",
            detail: format!("engine {version:?} is not registered"),
        });
    }
    transaction.execute("UPDATE engines SET is_default = 0 WHERE is_default = 1", [])?;
    transaction.execute(
        "UPDATE engines SET is_default = 1 WHERE version = ?1",
        [version],
    )?;
    transaction
        .query_row(
            "SELECT version, host_root, cage_executable, sha256, is_default
             FROM engines WHERE version = ?1",
            [version],
            engine_from_row,
        )
        .map_err(StateError::from)
}

fn engine_from_row(row: &rusqlite::Row<'_>) -> Result<EngineRecord, rusqlite::Error> {
    Ok(EngineRecord {
        version: row.get(0)?,
        host_root: PathBuf::from(row.get::<_, String>(1)?),
        cage_executable: PathBuf::from(row.get::<_, String>(2)?),
        sha256: row.get(3)?,
        is_default: row.get(4)?,
    })
}

#[derive(Clone, Debug)]
struct ArtifactRow {
    id: i64,
    record: ArtifactRecord,
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
    for (value, field) in [
        (input.source.branch.as_deref(), "source branch"),
        (input.source.commit.as_deref(), "source commit"),
    ] {
        if value.is_some_and(|value| {
            value.trim().is_empty()
                || value.len() > MAX_GITHUB_TEXT_LEN
                || value.chars().any(char::is_control)
        }) {
            return Err(StateError::InvalidRecord {
                kind: "deployment",
                detail: format!("{field} must be nonempty, printable, and bounded"),
            });
        }
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
    let hash = metadata_value(object, "artifactHash", "artifact_hash")
        .ok_or(StateError::MetadataMismatch)?;
    let runtime_entry = metadata_value(object, "runtimeEntry", "runtime_entry")
        .ok_or(StateError::MetadataMismatch)?;
    if source != artifact.source_hash
        || bun != artifact.engine_version
        || hash != artifact.artifact_hash
        || validate_cage_path(Path::new(runtime_entry), "artifact runtime entry").is_err()
    {
        return Err(StateError::MetadataMismatch);
    }
    Ok(())
}

fn validate_artifact_record_metadata(artifact: &ArtifactRecord) -> Result<(), StateError> {
    validate_metadata(&ArtifactInput {
        app: artifact.app.clone(),
        source_hash: artifact.source_hash.clone(),
        artifact_hash: artifact.artifact_hash.clone(),
        engine_version: artifact.engine_version.clone(),
        host_path: artifact.host_path.clone(),
        metadata_json: artifact.metadata_json.clone(),
    })
}

fn metadata_runtime_entry(metadata_json: &str) -> Result<String, StateError> {
    let value: serde_json::Value =
        serde_json::from_str(metadata_json).map_err(|_| StateError::MetadataMismatch)?;
    let object = value.as_object().ok_or(StateError::MetadataMismatch)?;
    let entry = metadata_value(object, "runtimeEntry", "runtime_entry")
        .ok_or(StateError::MetadataMismatch)?;
    validate_cage_path(Path::new(entry), "artifact runtime entry")
        .map_err(|_| StateError::MetadataMismatch)?;
    Ok(entry.to_owned())
}

/// Bytes of the artifact hash used in the on-disk socket filename.
///
/// The full 64-hex hash is retained as the runtime key, but Unix domain
/// sockets have a short `sun_path` limit (~104 on macOS, 108 on Linux). A
/// macOS home-path state root plus `upstreams/` plus the full hash exceeds
/// that limit and leaves deployments stuck in `sealed`. Sixteen hex chars
/// (64 bits) keeps paths well under the limit while remaining unique for
/// content-addressed artifacts in practice.
const REVISION_SOCKET_HASH_CHARS: usize = 16;

fn revision_upstream(base: &Path, artifact_hash: &str) -> Result<PathBuf, StateError> {
    validate_hash(artifact_hash, "runtime artifact hash")?;
    let parent = base.parent().ok_or_else(|| {
        StateError::InvalidConfig("activation upstream has no parent directory".into())
    })?;
    let short = &artifact_hash[..REVISION_SOCKET_HASH_CHARS];
    let upstream = parent.join(format!("r-{short}.sock"));
    validate_absolute_path(&upstream, "activation upstream")?;
    // Leave headroom under the platform sun_path limit (104 macOS / 108 Linux).
    if upstream.as_os_str().as_bytes().len() > 100 {
        return Err(StateError::InvalidConfig(
            "activation upstream exceeds Unix socket path limit".into(),
        ));
    }
    Ok(upstream)
}

fn runtime_socket_path(upstream: &Path) -> PathBuf {
    PathBuf::from(cygnus_cage::INGRESS_CAGE_DIR).join(
        upstream
            .file_name()
            .expect("validated activation upstream has a filename"),
    )
}

fn engine_runtime_command(engine: &EngineRecord) -> Result<String, StateError> {
    if cfg!(target_os = "linux") {
        return Ok(engine.cage_executable.to_string_lossy().into_owned());
    }
    let relative =
        engine
            .cage_executable
            .strip_prefix("/")
            .map_err(|_| StateError::InvalidRecord {
                kind: "engine",
                detail: "engine cage executable must be absolute".into(),
            })?;
    Ok(engine
        .host_root
        .join(relative)
        .to_string_lossy()
        .into_owned())
}

fn app_config_from_loaded(app: LoadedApp) -> Result<AppConfig, StateError> {
    let egress = match app.spec.egress {
        EgressMode::None => EgressConfig::None,
        EgressMode::Public => EgressConfig::Public,
        EgressMode::Open => EgressConfig::Open,
        EgressMode::Restricted { allow } => EgressConfig::Restricted {
            allow: allow
                .into_iter()
                .map(|rule| EgressRuleConfig {
                    cidr: rule.cidr,
                    ports: rule.ports,
                })
                .collect(),
        },
        EgressMode::BuildDomains { .. } => {
            return Err(StateError::InvalidPersisted {
                app: app.name,
                detail: "build-only egress cannot be a persisted runtime policy".into(),
            });
        }
    };
    Ok(AppConfig {
        name: app.name,
        domains: app.domains,
        tenant_admin: app.tenant_admin,
        upstream: app.upstream,
        command: app.spec.command.to_string_lossy().into_owned(),
        args: app
            .spec
            .args
            .into_iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect(),
        env: app
            .spec
            .env
            .into_iter()
            .map(|(key, value)| {
                (
                    key.to_string_lossy().into_owned(),
                    value.to_string_lossy().into_owned(),
                )
            })
            .collect(),
        limits: LimitsConfig {
            memory_max: app.spec.limits.memory_max,
            memory_high: app.spec.limits.memory_high,
            cpu_quota: app.spec.limits.cpu_quota,
            cpu_period: app.spec.limits.cpu_period,
            pids_max: app.spec.limits.pids_max,
        },
        rootfs: app.spec.rootfs.map(|rootfs| RootfsConfig {
            lowerdirs: rootfs.lowerdirs,
            tmpfs_size: rootfs.tmpfs_size,
            staging_dir: rootfs.staging_dir,
        }),
        seccomp: app.spec.seccomp.map(|mode| match mode {
            FilterMode::Enforce => SeccompMode::Enforce,
            FilterMode::Audit => SeccompMode::Audit,
        }),
        egress,
        init: app.spec.init,
        readiness_timeout_ms: duration_millis(app.spec.readiness_timeout),
        lifecycle: LifecyclePolicy {
            idle_ttl_ms: duration_millis(app.lifecycle.idle_ttl),
            min_instances: app.lifecycle.min_instances,
            backoff_base_ms: duration_millis(app.lifecycle.backoff_base),
            backoff_max_ms: duration_millis(app.lifecycle.backoff_max),
            crash_window_ms: duration_millis(app.lifecycle.crash_window),
            crash_loop_threshold: app.lifecycle.crash_loop_threshold,
        },
    })
}

fn validate_audit_context(context: &AuditContext) -> Result<(), StateError> {
    for (kind, value) in [
        ("request id", context.request_id.as_str()),
        ("command kind", context.command_kind.as_str()),
    ] {
        if value.trim().is_empty() || value.chars().any(char::is_control) {
            return Err(StateError::InvalidRecord {
                kind: "audit",
                detail: format!("{kind} must be nonempty and printable"),
            });
        }
    }
    if context
        .actor_subject
        .as_deref()
        .is_some_and(|value| value.trim().is_empty() || value.chars().any(char::is_control))
    {
        return Err(StateError::InvalidRecord {
            kind: "audit",
            detail: "actor subject must be nonempty and printable when supplied".into(),
        });
    }
    validate_hash(&context.request_digest, "audit request digest")
}

fn append_audit_tx(
    transaction: &Transaction<'_>,
    context: &AuditContext,
    outcome: AuditOutcome,
    error_code: Option<&str>,
) -> Result<i64, StateError> {
    validate_audit_context(context)?;
    if matches!(outcome, AuditOutcome::Success) && error_code.is_some()
        || matches!(outcome, AuditOutcome::Failure)
            && error_code
                .is_none_or(|code| code.trim().is_empty() || code.chars().any(char::is_control))
    {
        return Err(StateError::InvalidRecord {
            kind: "audit",
            detail: "success must omit error_code and failure must supply a printable error_code"
                .into(),
        });
    }
    transaction.execute(
        "INSERT INTO audit_log
         (endpoint_role, peer_uid, peer_gid, peer_pid, actor_subject, request_id,
          command_kind, request_digest, outcome, error_code)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            audit_endpoint_role_name(context.endpoint_role),
            context.peer_uid,
            context.peer_gid,
            context.peer_pid,
            context.actor_subject,
            context.request_id,
            context.command_kind,
            context.request_digest,
            audit_outcome_name(outcome),
            error_code,
        ],
    )?;
    Ok(transaction.last_insert_rowid())
}

fn audit_endpoint_role_name(role: AuditEndpointRole) -> &'static str {
    match role {
        AuditEndpointRole::Host => "host",
        AuditEndpointRole::TenantZero => "tenant_zero",
    }
}

fn audit_outcome_name(outcome: AuditOutcome) -> &'static str {
    match outcome {
        AuditOutcome::Success => "success",
        AuditOutcome::Failure => "failure",
    }
}

fn audit_record_from_row(row: &rusqlite::Row<'_>) -> Result<AuditRecord, rusqlite::Error> {
    let endpoint_role: String = row.get(2)?;
    let endpoint_role = match endpoint_role.as_str() {
        "host" => AuditEndpointRole::Host,
        "tenant_zero" => AuditEndpointRole::TenantZero,
        other => {
            return Err(invalid_text_column(
                2,
                format!("unknown audit endpoint role {other:?}"),
            ));
        }
    };
    let outcome: String = row.get(10)?;
    let outcome = match outcome.as_str() {
        "success" => AuditOutcome::Success,
        "failure" => AuditOutcome::Failure,
        other => {
            return Err(invalid_text_column(
                10,
                format!("unknown audit outcome {other:?}"),
            ));
        }
    };
    Ok(AuditRecord {
        id: row.get(0)?,
        recorded_at: row.get(1)?,
        endpoint_role,
        peer_uid: row.get(3)?,
        peer_gid: row.get(4)?,
        peer_pid: row.get(5)?,
        actor_subject: row.get(6)?,
        request_id: row.get(7)?,
        command_kind: row.get(8)?,
        request_digest: row.get(9)?,
        outcome,
        error_code: row.get(11)?,
    })
}

fn invalid_text_column(column: usize, detail: String) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        column,
        rusqlite::types::Type::Text,
        Box::new(std::io::Error::other(detail)),
    )
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

fn deployment_source_kind_name(kind: DeploymentSourceKind) -> &'static str {
    match kind {
        DeploymentSourceKind::GitHub => "github",
        DeploymentSourceKind::Upload => "upload",
        DeploymentSourceKind::Cli => "cli",
    }
}

fn parse_deployment_source_kind(value: &str) -> Result<DeploymentSourceKind, String> {
    match value {
        "github" => Ok(DeploymentSourceKind::GitHub),
        "upload" => Ok(DeploymentSourceKind::Upload),
        "cli" => Ok(DeploymentSourceKind::Cli),
        other => Err(format!("unknown deployment source kind {other:?}")),
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
        ) | (
            // Activation can fail after the artifact is sealed (socket path
            // limits, engine mismatch, boot failure). Surface that as failed
            // so the console and CLI do not leave a permanent "pending
            // activation" corpse.
            DeploymentStatus::Sealed,
            DeploymentStatus::Active | DeploymentStatus::Failed
        )
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
    transaction
        .query_row(
            "SELECT id, app, source_hash, engine_version, artifact_hash, status, error,
                    created_at, unixepoch(created_at) * 1000, updated_at, unixepoch(updated_at) * 1000, log_path,
                    source_kind, source_branch, source_commit
             FROM deployments WHERE id = ?1",
            [id],
            deployment_from_row,
        )
        .optional()
        .map_err(StateError::from)
}

fn deployment_from_row(row: &rusqlite::Row<'_>) -> Result<DeploymentRecord, rusqlite::Error> {
    let status: String = row.get(5)?;
    let status = parse_status(&status).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            5,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::other(error)),
        )
    })?;
    let source_kind: String = row.get(12)?;
    let source_kind = parse_deployment_source_kind(&source_kind).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            12,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::other(error)),
        )
    })?;
    Ok(DeploymentRecord {
        id: row.get(0)?,
        app: row.get(1)?,
        source_hash: row.get(2)?,
        engine_version: row.get(3)?,
        source: DeploymentSource {
            kind: source_kind,
            branch: row.get(13)?,
            commit: row.get(14)?,
        },
        artifact_hash: row.get(4)?,
        status,
        error: row.get(6)?,
        created_at: row.get(7)?,
        created_ms: row.get(8)?,
        updated_at: row.get(9)?,
        updated_ms: row.get(10)?,
        log_path: row.get::<_, Option<String>>(11)?.map(PathBuf::from),
    })
}

fn query_artifact_tx(
    transaction: &Transaction<'_>,
    hash: &str,
) -> Result<Option<ArtifactRow>, StateError> {
    transaction
        .query_row(
            "SELECT id, app, source_hash, artifact_hash, engine_version, host_path, metadata_json
             FROM artifacts WHERE artifact_hash = ?1 AND status = 'sealed'",
            [hash],
            |row| {
                Ok(ArtifactRow {
                    id: row.get(0)?,
                    record: ArtifactRecord {
                        app: row.get(1)?,
                        source_hash: row.get(2)?,
                        artifact_hash: row.get(3)?,
                        engine_version: row.get(4)?,
                        host_path: PathBuf::from(row.get::<_, String>(5)?),
                        metadata_json: row.get(6)?,
                    },
                })
            },
        )
        .optional()
        .map_err(StateError::from)
}

fn artifact_record_from_row(row: &rusqlite::Row<'_>) -> Result<ArtifactRecord, rusqlite::Error> {
    Ok(ArtifactRecord {
        app: row.get(0)?,
        source_hash: row.get(1)?,
        artifact_hash: row.get(2)?,
        engine_version: row.get(3)?,
        host_path: PathBuf::from(row.get::<_, String>(4)?),
        metadata_json: row.get(5)?,
    })
}

fn metadata_json_equal(left: &str, right: &str) -> bool {
    match (
        serde_json::from_str::<serde_json::Value>(left),
        serde_json::from_str::<serde_json::Value>(right),
    ) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
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
    edge: EdgeConfig,
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
    #[serde(default)]
    tenant_admin: bool,
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
        edge: snapshot.edge.clone(),
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
            tenant_admin: app.tenant_admin,
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
        tenant_admin: runtime.tenant_admin,
        upstream: upstream_path,
        spec,
        lifecycle,
    })
}

fn store_edge_config_tx(
    transaction: &Transaction<'_>,
    edge: &EdgeConfig,
) -> Result<(), StateError> {
    let https_listen = edge.https_listen.map(|address| address.to_string());
    let (acme_email, acme_directory_url, dns_provider) = edge
        .acme
        .as_ref()
        .map(|acme| {
            (
                Some(acme.email.as_str()),
                Some(acme.directory_url.as_str()),
                acme.dns_provider.as_deref(),
            )
        })
        .unwrap_or((None, None, None));
    transaction.execute(
        "INSERT INTO edge_config
         (id, https_listen, apps_domain, acme_email, acme_directory_url, dns_provider,
          dashboard_domain, apex_domain, ssl_mode)
         VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
         ON CONFLICT(id) DO UPDATE SET https_listen = excluded.https_listen,
             apps_domain = excluded.apps_domain, acme_email = excluded.acme_email,
             acme_directory_url = excluded.acme_directory_url,
             dns_provider = excluded.dns_provider,
             dashboard_domain = excluded.dashboard_domain,
             apex_domain = excluded.apex_domain,
             ssl_mode = excluded.ssl_mode",
        params![
            https_listen,
            edge.apps_domain,
            acme_email,
            acme_directory_url,
            dns_provider,
            edge.dashboard_domain,
            edge.apex_domain,
            match edge.ssl_mode {
                SslMode::Acme => "acme",
                SslMode::SelfSigned => "self_signed",
            },
        ],
    )?;
    Ok(())
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
    store_edge_config_tx(transaction, &snapshot.edge)?;
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
        let native = snapshot
            .edge
            .apex_domain
            .as_deref()
            .map(|apex| native_domain(&app.name, apex))
            .transpose()?;
        for domain in &app.domains {
            if native.as_deref() == Some(domain.as_str()) {
                continue;
            }
            transaction.execute(
                "INSERT INTO domains (app_id, domain, kind, tls, status)
                 VALUES (?1, ?2, 'custom', ?3, 'pending')",
                params![app_id, domain, ssl_mode_name(snapshot.edge.ssl_mode)],
            )?;
        }
    }
    reconcile_native_domains_tx(
        transaction,
        snapshot.edge.apex_domain.as_deref(),
        snapshot.edge.ssl_mode,
    )?;
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
            tenant_admin: input.tenant_admin,
            upstream: input.upstream.clone(),
            spec,
            lifecycle,
        });
    }
    let edge = canonical_edge_config(config.listen, &config.edge)?;
    if let Some(apex) = edge.apex_domain.as_deref() {
        for app in &mut apps {
            // Console process is not a product app — no tenant-0.<apex>.
            if app.name == "tenant-0" || app.tenant_admin {
                continue;
            }
            let native = native_domain(&app.name, apex)?;
            app.domains.retain(|domain| domain != &native);
            app.domains.push(native);
        }
    }
    let snapshot = Snapshot {
        listen: config.listen,
        edge,
        apps,
    };
    validate_snapshot(&snapshot)?;
    Ok(sort_snapshot(snapshot))
}

fn validate_snapshot(snapshot: &Snapshot) -> Result<(), StateError> {
    let mut names = BTreeSet::new();
    let mut upstreams = BTreeSet::new();
    let mut domains = BTreeSet::new();
    if canonical_edge_config(snapshot.listen, &snapshot.edge)? != snapshot.edge {
        return Err(StateError::InvalidConfig(
            "edge configuration is not canonical".into(),
        ));
    }
    let mut tenant_admin_count = 0;
    for app in &snapshot.apps {
        if app.tenant_admin {
            tenant_admin_count += 1;
            if tenant_admin_count > 1 {
                return Err(StateError::InvalidConfig(
                    "only one app may be designated as Tenant Zero".into(),
                ));
            }
        }
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

fn canonical_edge_config(listen: SocketAddr, edge: &EdgeConfig) -> Result<EdgeConfig, StateError> {
    if edge.https_listen == Some(listen) {
        return Err(StateError::InvalidConfig(
            "HTTP and HTTPS listeners must be distinct".into(),
        ));
    }
    let apps_domain = edge
        .apps_domain
        .as_deref()
        .map(|domain| {
            if domain.trim_start().starts_with("*.") {
                return Err(StateError::InvalidConfig(
                    "apps_domain must be a hostname, not a wildcard pattern".into(),
                ));
            }
            canonical_domain(domain)
                .ok_or_else(|| StateError::InvalidConfig(format!("invalid apps_domain {domain:?}")))
        })
        .transpose()?;
    let canonical_edge_host = |value: Option<&str>, field: &str| {
        value
            .map(|domain| {
                if domain.trim_start().starts_with("*.") {
                    return Err(StateError::InvalidConfig(format!(
                        "{field} must be a hostname, not a wildcard pattern"
                    )));
                }
                canonical_domain(domain)
                    .ok_or_else(|| StateError::InvalidConfig(format!("invalid {field} {domain:?}")))
            })
            .transpose()
    };
    let dashboard_domain =
        canonical_edge_host(edge.dashboard_domain.as_deref(), "dashboard_domain")?;
    let apex_domain = canonical_edge_host(edge.apex_domain.as_deref(), "apex_domain")?;
    let mut https_listen = edge.https_listen;
    let acme = edge
        .acme
        .as_ref()
        .map(|acme| {
            // ACME needs a public HTTPS listener. If the operator never set one,
            // default to 0.0.0.0:443 — matching the daemon's runtime fallback —
            // instead of rejecting dashboard/domain saves with a dead-end error.
            if https_listen.is_none() {
                https_listen = Some(SocketAddr::from(([0, 0, 0, 0], 443)));
            }
            let email = acme.email.trim();
            if email != acme.email
                || email.len() > 254
                || !email.contains('@')
                || email.chars().any(char::is_control)
            {
                return Err(StateError::InvalidConfig(
                    "ACME email must be a canonical printable address".into(),
                ));
            }
            let directory_url = acme.directory_url.trim();
            if directory_url != acme.directory_url
                || directory_url.len() > 2048
                || !directory_url.starts_with("https://")
                || directory_url.trim_start_matches("https://").is_empty()
                || directory_url.chars().any(char::is_whitespace)
            {
                return Err(StateError::InvalidConfig(
                    "ACME directory_url must be a canonical HTTPS URL".into(),
                ));
            }
            if let Some(provider) = acme.dns_provider.as_deref()
                && (provider.is_empty()
                    || provider.len() > 64
                    || !provider.bytes().all(|byte| {
                        byte.is_ascii_lowercase()
                            || byte.is_ascii_digit()
                            || matches!(byte, b'-' | b'_')
                    }))
            {
                return Err(StateError::InvalidConfig(
                    "ACME dns_provider must be a short lowercase identifier".into(),
                ));
            }
            Ok(AcmeConfig {
                email: email.to_owned(),
                directory_url: directory_url.to_owned(),
                dns_provider: acme.dns_provider.clone(),
            })
        })
        .transpose()?;
    Ok(EdgeConfig {
        https_listen,
        apps_domain,
        dashboard_domain,
        apex_domain,
        ssl_mode: edge.ssl_mode,
        acme,
    })
}

fn canonical_certificate_domains(domains: &[String]) -> Result<Vec<String>, StateError> {
    if domains.is_empty() {
        return Err(StateError::InvalidRecord {
            kind: "certificate",
            detail: "at least one certificate domain is required".into(),
        });
    }
    let mut canonical = canonical_domains(domains)?;
    canonical.sort();
    if canonical.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(StateError::InvalidRecord {
            kind: "certificate",
            detail: "certificate domains must be unique after normalization".into(),
        });
    }
    Ok(canonical)
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
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static TEMP_DB_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    fn temp_db(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "cygnus-state-{label}-{}-{nonce}-{}",
            std::process::id(),
            TEMP_DB_SEQUENCE.fetch_add(1, Ordering::Relaxed),
        ));
        fs::create_dir_all(&directory).expect("create temporary state directory");
        directory.join("state.db")
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
            edge: EdgeConfig::default(),
            apps: vec![AppConfig {
                name: "api".into(),
                domains: vec!["API.Example.com.".into(), "*.Apps.Example.com".into()],
                upstream: "/run/cygnus/api.sock".into(),
                command: "/bin/true".into(),
                ..AppConfig::default()
            }],
        }
    }

    fn audit_context(request_id: &str) -> AuditContext {
        AuditContext {
            endpoint_role: AuditEndpointRole::Host,
            peer_uid: Some(0),
            peer_gid: Some(0),
            peer_pid: Some(42),
            actor_subject: Some("root".into()),
            request_id: request_id.into(),
            command_kind: "activate_deployment".into(),
            request_digest: "f".repeat(64),
        }
    }

    fn artifact_input(
        app: &str,
        source_hash: &str,
        artifact_hash: &str,
        engine_version: &str,
        host_path: &str,
        runtime_entry: &str,
    ) -> ArtifactInput {
        ArtifactInput {
            app: app.into(),
            source_hash: source_hash.into(),
            artifact_hash: artifact_hash.into(),
            engine_version: engine_version.into(),
            host_path: host_path.into(),
            metadata_json: format!(
                "{{\"bunVersion\":\"{engine_version}\",\"sourceHash\":\"{source_hash}\",\"artifactHash\":\"{artifact_hash}\",\"runtimeEntry\":\"{runtime_entry}\"}}"
            ),
        }
    }

    fn test_engine_record(version: &str, is_default: bool) -> EngineRecord {
        EngineRecord {
            version: version.into(),
            host_root: "/".into(),
            cage_executable: "/usr/bin/true".into(),
            sha256: "a".repeat(64),
            is_default,
        }
    }

    fn register_test_engine(state: &mut State, version: &str) -> EngineRecord {
        state
            .register_engine(&test_engine_record(version, false))
            .unwrap()
    }

    fn test_artifact(
        app: &str,
        source_hash: &str,
        artifact_hash: &str,
        engine_version: &str,
    ) -> ArtifactInput {
        ArtifactInput {
            app: app.into(),
            source_hash: source_hash.into(),
            artifact_hash: artifact_hash.into(),
            engine_version: engine_version.into(),
            host_path: PathBuf::from(format!("/var/lib/cygnus/apps/{app}/{artifact_hash}")),
            metadata_json: format!(
                "{{\"bunVersion\":\"{engine_version}\",\"sourceHash\":\"{source_hash}\",\"artifactHash\":\"{artifact_hash}\",\"runtimeEntry\":\"/app/index.js\"}}"
            ),
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
    fn migrates_v8_through_v10_with_constrained_accounts_table() {
        let path = temp_db("v8-accounts");
        {
            let connection = Connection::open(&path).expect("fixture database");
            create_schema(&connection).unwrap();
            migrate_v1_to_v2(&connection).unwrap();
            migrate_v2_to_v3(&connection).unwrap();
            migrate_v3_to_v4(&connection).unwrap();
            migrate_v4_to_v5(&connection, &[0; NODE_KEY_LEN]).unwrap();
            migrate_v5_to_v6(&connection).unwrap();
            migrate_v6_to_v7(&connection).unwrap();
            migrate_v7_to_v8(&connection).unwrap();
            connection
                .pragma_update(None, "user_version", 8_i32)
                .unwrap();
        }

        let state = State::open(&path).expect("migrate v8 fixture");
        assert_eq!(
            state.account_status().unwrap(),
            AccountStatus { configured: false }
        );
        let version: i32 = state
            .connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        let columns = state
            .connection
            .prepare("PRAGMA table_info(accounts)")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(columns, ["id", "email", "password_hash", "created_at"]);
        assert!(
            state
                .connection
                .execute(
                    "INSERT INTO accounts (email, password_hash) VALUES ('Admin@Example.com', 'hash')",
                    [],
                )
                .is_err()
        );
        state
            .connection
            .execute(
                "INSERT INTO accounts (email, password_hash) VALUES ('admin@example.com', 'hash')",
                [],
            )
            .unwrap();
        assert!(
            state
                .connection
                .execute(
                    "INSERT INTO accounts (email, password_hash) VALUES ('admin@example.com', 'other')",
                    [],
                )
                .is_err()
        );
        drop(state);
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn account_password_auth_round_trips_and_wrong_password_is_false() {
        let path = temp_db("account-roundtrip");
        let mut state = State::open(&path).unwrap();
        let account = state
            .create_initial_account("  Admin@Example.COM  ", "correct horse battery staple")
            .unwrap();
        assert_eq!(account.subject, "account:1");
        assert_eq!(
            state.account_status().unwrap(),
            AccountStatus { configured: true }
        );
        let (email, password_hash): (String, String) = state
            .connection
            .query_row("SELECT email, password_hash FROM accounts", [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .unwrap();
        assert_eq!(email, "admin@example.com");
        assert!(password_hash.starts_with("$argon2id$"));
        assert!(!password_hash.contains("correct horse battery staple"));

        assert_eq!(
            state
                .verify_credentials("ADMIN@example.com", "correct horse battery staple")
                .unwrap(),
            CredentialVerification {
                ok: true,
                subject: Some(account.subject),
            }
        );
        assert_eq!(
            state
                .verify_credentials("admin@example.com", "wrong password value")
                .unwrap(),
            CredentialVerification {
                ok: false,
                subject: None,
            }
        );
        drop(state);
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn initial_account_normalization_rejects_duplicates_and_closes_tofu() {
        let path = temp_db("account-tofu");
        let mut state = State::open(&path).unwrap();
        state
            .create_initial_account("Admin@Example.com", "correct horse battery staple")
            .unwrap();
        assert!(matches!(
            state.create_initial_account(" admin@example.COM ", "another strong password"),
            Err(StateError::DuplicateAccountEmail(email)) if email == "admin@example.com"
        ));
        assert!(matches!(
            state.create_initial_account("other@example.com", "another strong password"),
            Err(StateError::AccountAlreadyConfigured)
        ));
        let count: i64 = state
            .connection
            .query_row("SELECT COUNT(*) FROM accounts", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
        drop(state);
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn account_inputs_are_bounded() {
        let path = temp_db("account-bounds");
        let mut state = State::open(&path).unwrap();
        assert!(matches!(
            state.create_initial_account("not-an-email", "correct horse battery staple"),
            Err(StateError::InvalidAccountInput(_))
        ));
        assert!(matches!(
            state.create_initial_account("admin@example.com", "short"),
            Err(StateError::InvalidAccountInput(_))
        ));
        assert!(matches!(
            state.create_initial_account(
                "admin@example.com",
                &"x".repeat(MAX_ACCOUNT_PASSWORD_BYTES + 1)
            ),
            Err(StateError::InvalidAccountInput(_))
        ));
        drop(state);
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
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
    fn migrates_v5_engines_and_selects_oldest_as_default() {
        let path = temp_db("v5-engine-default");
        {
            let connection = Connection::open(&path).expect("fixture database");
            create_schema(&connection).unwrap();
            migrate_v1_to_v2(&connection).unwrap();
            migrate_v2_to_v3(&connection).unwrap();
            migrate_v3_to_v4(&connection).unwrap();
            migrate_v4_to_v5(&connection, &[0; NODE_KEY_LEN]).unwrap();
            connection
                .execute_batch(
                    "INSERT INTO engines
                         (id, version, host_root, cage_executable, sha256)
                     VALUES
                         (10, 'bun-old', '/', '/usr/bin/true', 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'),
                         (20, 'bun-new', '/', '/usr/bin/true', 'bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb');
                     PRAGMA user_version = 5;",
                )
                .unwrap();
        }

        let state = State::open(&path).expect("migrate v5 fixture");
        assert_eq!(
            state
                .engines()
                .unwrap()
                .into_iter()
                .map(|status| (status.engine.version, status.engine.is_default))
                .collect::<Vec<_>>(),
            [("bun-new".into(), false), ("bun-old".into(), true)]
        );
        let version: i32 = state
            .connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        drop(state);
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn first_registered_engine_becomes_default_automatically() {
        let path = temp_db("first-engine-default");
        let mut state = State::open(&path).unwrap();

        let registered = state
            .register_engine(&test_engine_record("bun-1", false))
            .unwrap();

        assert!(registered.is_default);
        assert!(state.engine("bun-1").unwrap().unwrap().is_default);
        drop(state);
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn reregistering_a_version_updates_it_in_place_and_keeps_default() {
        let path = temp_db("reregister-engine");
        let mut state = State::open(&path).unwrap();
        state
            .register_engine(&test_engine_record("bundled", false))
            .unwrap();
        assert!(state.engine("bundled").unwrap().unwrap().is_default);

        // A reinstall ships a fresh engine binary: same version, new hash and
        // root. Registering again must succeed (idempotent upgrade) and keep
        // the default marker rather than raising a UNIQUE conflict.
        let upgraded = EngineRecord {
            version: "bundled".into(),
            host_root: "/".into(),
            cage_executable: "/usr/bin/true".into(),
            sha256: "b".repeat(64),
            is_default: false,
        };
        let registered = state.register_engine(&upgraded).unwrap();
        assert!(registered.is_default);
        let stored = state.engine("bundled").unwrap().unwrap();
        assert_eq!(stored.sha256, "b".repeat(64));
        assert_eq!(state.engines().unwrap().len(), 1);
        drop(state);
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn later_registration_preserves_the_existing_default() {
        let path = temp_db("later-engine-default");
        let mut state = State::open(&path).unwrap();
        state
            .register_engine(&test_engine_record("bun-1", false))
            .unwrap();

        let registered = state
            .register_engine(&test_engine_record("bun-2", false))
            .unwrap();

        assert!(!registered.is_default);
        assert!(state.engine("bun-1").unwrap().unwrap().is_default);
        assert_eq!(
            state
                .engines()
                .unwrap()
                .into_iter()
                .map(|status| (status.engine.version, status.app_count))
                .collect::<Vec<_>>(),
            [("bun-1".into(), 0), ("bun-2".into(), 0)]
        );
        drop(state);
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn explicit_default_switching_survives_idempotent_registration() {
        let path = temp_db("explicit-engine-default");
        let mut state = State::open(&path).unwrap();
        let first = state
            .register_engine(&test_engine_record("bun-1", false))
            .unwrap();
        let second = state
            .register_engine(&test_engine_record("bun-2", true))
            .unwrap();

        assert!(!state.engine("bun-1").unwrap().unwrap().is_default);
        assert!(second.is_default);
        assert_eq!(second.host_root, PathBuf::from("/"));
        assert_eq!(second.sha256, "a".repeat(64));

        let selected = state
            .set_default_engine_with_audit("bun-1", &audit_context("default-bun-1"))
            .unwrap();
        assert!(selected.is_default);
        assert!(!state.engine("bun-2").unwrap().unwrap().is_default);
        assert_eq!(selected.host_root, first.host_root);
        assert_eq!(selected.sha256, first.sha256);
        assert_eq!(state.audit_records().unwrap().len(), 1);

        let duplicate = test_engine_record("bun-1", true);
        let updated = state.register_engine(&duplicate).unwrap();
        assert!(updated.is_default);
        assert_eq!(updated.host_root, duplicate.host_root);
        assert_eq!(updated.sha256, duplicate.sha256);
        drop(state);
        let _ = fs::remove_dir_all(path.parent().unwrap());
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
        assert_eq!(version, SCHEMA_VERSION);
        drop(state);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn migrates_v2_deployments_without_losing_the_active_row() {
        let path = temp_db("v2-migrate");
        let source_hash = "b".repeat(64);
        let artifact_hash = "c".repeat(64);
        {
            let connection = Connection::open(&path).expect("fixture database");
            connection
                .execute_batch(&format!(
                    "CREATE TABLE node_config (id INTEGER PRIMARY KEY CHECK (id = 1), listen TEXT NOT NULL);
                     CREATE TABLE apps (id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE, upstream TEXT NOT NULL UNIQUE, runtime_json TEXT NOT NULL);
                     CREATE TABLE domains (id INTEGER PRIMARY KEY, app_id INTEGER NOT NULL REFERENCES apps(id) ON DELETE CASCADE, domain TEXT NOT NULL COLLATE BINARY UNIQUE);
                     CREATE TABLE engines (id INTEGER PRIMARY KEY, version TEXT NOT NULL UNIQUE, host_root TEXT NOT NULL, cage_executable TEXT NOT NULL, sha256 TEXT NOT NULL);
                     CREATE TABLE artifacts (id INTEGER PRIMARY KEY, app TEXT NOT NULL, source_hash TEXT NOT NULL, artifact_hash TEXT NOT NULL UNIQUE, engine_version TEXT NOT NULL REFERENCES engines(version), host_path TEXT NOT NULL UNIQUE, metadata_json TEXT NOT NULL, status TEXT NOT NULL CHECK (status = 'sealed'));
                     CREATE TABLE deployments (id TEXT PRIMARY KEY, app TEXT NOT NULL, source_hash TEXT NOT NULL, engine_version TEXT NOT NULL REFERENCES engines(version), artifact_hash TEXT UNIQUE REFERENCES artifacts(artifact_hash), status TEXT NOT NULL CHECK (status IN ('building', 'failed', 'sealed', 'active')), error TEXT);
                     CREATE TABLE app_artifacts (app_id INTEGER PRIMARY KEY REFERENCES apps(id) ON DELETE CASCADE, artifact_id INTEGER NOT NULL UNIQUE REFERENCES artifacts(id), activated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP);
                     INSERT INTO node_config VALUES (1, '127.0.0.1:3000');
                     INSERT INTO apps VALUES (1, 'api', '/run/api.sock', '{{\"name\":\"api\",\"upstream\":\"/run/api.sock\",\"domains\":[],\"runtime\":{{\"command\":\"/bin/true\",\"args\":[],\"env\":{{}},\"limits\":{{\"memory_max\":268435456,\"memory_high\":234881024,\"cpu_quota\":100000,\"cpu_period\":100000,\"pids_max\":128}},\"rootfs\":null,\"seccomp\":\"enforce\",\"egress\":{{\"mode\":\"none\"}},\"init\":null,\"readiness_timeout_ms\":5000,\"idle_ttl_ms\":600000,\"min_instances\":0,\"backoff_base_ms\":100,\"backoff_max_ms\":30000,\"crash_window_ms\":60000,\"crash_loop_threshold\":5}}}}');
                     INSERT INTO engines VALUES (1, '1.2.3', '/', '/usr/bin/true', '{}');
                     INSERT INTO artifacts VALUES (1, 'api', '{source_hash}', '{artifact_hash}', '1.2.3', '/artifacts/{artifact_hash}', '{{\"sourceHash\":\"{source_hash}\",\"artifactHash\":\"{artifact_hash}\",\"bunVersion\":\"1.2.3\",\"runtimeEntry\":\"/app/index.js\"}}', 'sealed');
                     INSERT INTO deployments VALUES ('dep-active', 'api', '{source_hash}', '1.2.3', '{artifact_hash}', 'active', NULL);
                     INSERT INTO app_artifacts VALUES (1, 1, CURRENT_TIMESTAMP);
                     PRAGMA user_version = 2;",
                    "a".repeat(64)
                ))
                .expect("write v2 fixture");
        }

        let state = State::open(&path).expect("migrate fixture");
        let deployment = state
            .deployment("dep-active")
            .unwrap()
            .expect("active deployment preserved");
        assert_eq!(deployment.status, DeploymentStatus::Active);
        assert_eq!(
            deployment.artifact_hash.as_deref(),
            Some(artifact_hash.as_str())
        );
        assert_eq!(deployment.log_path, None);
        assert!(!deployment.created_at.is_empty());
        assert!(!deployment.updated_at.is_empty());
        assert_eq!(
            state
                .active_deployment("api")
                .unwrap()
                .unwrap()
                .deployment_id,
            "dep-active"
        );
        let version: i32 = state
            .connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        drop(state);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn migrates_v2_fixture_without_losing_active_deployment() {
        let path = temp_db("v2-migrate");
        let source_hash = "b".repeat(64);
        let artifact_hash = "c".repeat(64);
        {
            let connection = Connection::open(&path).expect("fixture database");
            connection
                .execute_batch(&format!(
                    r#"CREATE TABLE node_config (id INTEGER PRIMARY KEY CHECK (id = 1), listen TEXT NOT NULL);
                     CREATE TABLE apps (id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE, upstream TEXT NOT NULL UNIQUE, runtime_json TEXT NOT NULL);
                     CREATE TABLE domains (id INTEGER PRIMARY KEY, app_id INTEGER NOT NULL REFERENCES apps(id) ON DELETE CASCADE, domain TEXT NOT NULL COLLATE BINARY UNIQUE);
                     CREATE INDEX domains_app_id ON domains(app_id);
                     CREATE TABLE engines (id INTEGER PRIMARY KEY, version TEXT NOT NULL UNIQUE, host_root TEXT NOT NULL, cage_executable TEXT NOT NULL, sha256 TEXT NOT NULL);
                     CREATE TABLE artifacts (id INTEGER PRIMARY KEY, app TEXT NOT NULL, source_hash TEXT NOT NULL, artifact_hash TEXT NOT NULL UNIQUE, engine_version TEXT NOT NULL REFERENCES engines(version), host_path TEXT NOT NULL UNIQUE, metadata_json TEXT NOT NULL, status TEXT NOT NULL CHECK (status = 'sealed'));
                     CREATE TABLE deployments (id TEXT PRIMARY KEY, app TEXT NOT NULL, source_hash TEXT NOT NULL, engine_version TEXT NOT NULL REFERENCES engines(version), artifact_hash TEXT UNIQUE REFERENCES artifacts(artifact_hash), status TEXT NOT NULL CHECK (status IN ('building', 'failed', 'sealed', 'active')), error TEXT);
                     CREATE TABLE app_artifacts (app_id INTEGER PRIMARY KEY REFERENCES apps(id) ON DELETE CASCADE, artifact_id INTEGER NOT NULL UNIQUE REFERENCES artifacts(id), activated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP);
                     INSERT INTO node_config VALUES (1, '127.0.0.1:3000');
                     INSERT INTO apps VALUES (1, 'api', '/run/api.sock', '{{"name":"api","upstream":"/run/api.sock","domains":[],"runtime":{{"command":"/bin/true","args":[],"env":{{}},"limits":{{"memory_max":268435456,"memory_high":234881024,"cpu_quota":100000,"cpu_period":100000,"pids_max":128}},"rootfs":null,"seccomp":"enforce","egress":{{"mode":"none"}},"init":null,"readiness_timeout_ms":5000,"idle_ttl_ms":600000,"min_instances":0,"backoff_base_ms":100,"backoff_max_ms":30000,"crash_window_ms":60000,"crash_loop_threshold":5}}}}');
                     INSERT INTO engines VALUES (1, '1', '/', '/usr/bin/true', '{}');
                     INSERT INTO artifacts VALUES (1, 'api', '{}', '{}', '1', '/artifacts/c', '{{"bunVersion":"1","sourceHash":"{}","artifactHash":"{}","runtimeEntry":"/app/index.js"}}', 'sealed');
                     INSERT INTO deployments VALUES ('dep-v2', 'api', '{}', '1', '{}', 'active', NULL);
                     INSERT INTO app_artifacts (app_id, artifact_id) VALUES (1, 1);
                     PRAGMA user_version = 2;"#,
                    "a".repeat(64),
                    source_hash,
                    artifact_hash,
                    source_hash,
                    artifact_hash,
                    source_hash,
                    artifact_hash,
                ))
                .expect("write v2 fixture");
        }

        let state = State::open(&path).expect("migrate fixture");
        let version: i32 = state
            .connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        let deployment = state.deployment("dep-v2").unwrap().unwrap();
        assert_eq!(deployment.status, DeploymentStatus::Active);
        assert_eq!(
            deployment.artifact_hash.as_deref(),
            Some(artifact_hash.as_str())
        );
        assert_eq!(
            state
                .active_deployment("api")
                .unwrap()
                .unwrap()
                .deployment_id,
            "dep-v2"
        );
        drop(state);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn identical_artifact_is_reused_by_same_app_deployments_only() {
        let path = temp_db("artifact-reuse");
        let mut state = State::open(&path).unwrap();
        let engine = register_test_engine(&mut state, "1");
        let source_hash = "b".repeat(64);
        let artifact_hash = "c".repeat(64);
        for (id, app) in [("dep-1", "api"), ("dep-2", "api"), ("dep-foreign", "web")] {
            state
                .begin_deployment(&DeploymentInput {
                    id: id.into(),
                    app: app.into(),
                    source_hash: source_hash.clone(),
                    engine_version: engine.version.clone(),
                    source: DeploymentSource::cli(),
                })
                .unwrap();
        }
        let artifact = test_artifact("api", &source_hash, &artifact_hash, &engine.version);
        state.seal_deployment("dep-1", &artifact).unwrap();
        state.seal_deployment("dep-2", &artifact).unwrap();
        assert_eq!(
            state
                .deployment("dep-2")
                .unwrap()
                .unwrap()
                .artifact_hash
                .as_deref(),
            Some(artifact_hash.as_str())
        );

        let foreign = test_artifact("web", &source_hash, &artifact_hash, &engine.version);
        assert!(matches!(
            state.seal_deployment("dep-foreign", &foreign),
            Err(StateError::ArtifactOwnership { .. })
        ));
        assert_eq!(
            state.deployment("dep-foreign").unwrap().unwrap().status,
            DeploymentStatus::Building
        );
        drop(state);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn reused_artifact_keeps_content_addressed_runtime_identity_after_load() {
        let path = temp_db("artifact-runtime-identity");
        let mut state = State::open(&path).unwrap();
        let engine = register_test_engine(&mut state, "1");
        let source_hash = "b".repeat(64);
        let artifact_hash = "c".repeat(64);
        let artifact = test_artifact("api", &source_hash, &artifact_hash, &engine.version);
        for id in ["dep-1", "dep-2"] {
            state
                .begin_deployment(&DeploymentInput {
                    id: id.into(),
                    app: "api".into(),
                    source_hash: source_hash.clone(),
                    engine_version: engine.version.clone(),
                    source: DeploymentSource::cli(),
                })
                .unwrap();
            state.seal_deployment(id, &artifact).unwrap();
        }
        let app = AppConfig {
            name: "api".into(),
            domains: vec!["api.example".into()],
            upstream: "/run/api.sock".into(),
            command: "/usr/bin/true".into(),
            ..AppConfig::default()
        };
        state
            .activate_deployment("dep-1", &app, None, &audit_context("identity-1"))
            .unwrap();
        state
            .activate_deployment(
                "dep-2",
                &app,
                Some(&artifact_hash),
                &audit_context("identity-2"),
            )
            .unwrap();

        let loaded = state.load().unwrap();
        assert_eq!(loaded.apps[0].spec.name, format!("r-{artifact_hash}"));
        assert_eq!(
            loaded.apps[0].upstream,
            PathBuf::from(format!(
                "/run/r-{}.sock",
                &artifact_hash[..REVISION_SOCKET_HASH_CHARS]
            ))
        );
        assert_eq!(
            state
                .active_deployment("api")
                .unwrap()
                .unwrap()
                .deployment_id,
            "dep-2"
        );
        assert_eq!(
            state.deployment("dep-1").unwrap().unwrap().status,
            DeploymentStatus::Sealed
        );
        drop(state);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn deployment_provenance_round_trips_and_preassigned_builds_resume() {
        let path = temp_db("deployment-provenance");
        let mut state = State::open(&path).unwrap();
        let engine = register_test_engine(&mut state, "bun");
        let source =
            DeploymentSource::github(Some("main".into()), Some("a".repeat(SHA256_HEX_LEN)));
        state
            .begin_deployment(&DeploymentInput {
                id: "preassigned".into(),
                app: "api".into(),
                source_hash: "b".repeat(SHA256_HEX_LEN),
                engine_version: engine.version.clone(),
                source: source.clone(),
            })
            .unwrap();

        let resumed = state
            .resume_building_deployment(&DeploymentInput {
                id: "preassigned".into(),
                app: "api".into(),
                source_hash: "c".repeat(SHA256_HEX_LEN),
                engine_version: engine.version,
                source: source.clone(),
            })
            .unwrap();

        assert_eq!(resumed.source_hash, "c".repeat(SHA256_HEX_LEN));
        assert_eq!(resumed.source, source);
        assert!(resumed.created_ms > 0);
        assert_eq!(state.deployments(None, None, 1).unwrap(), [resumed]);
        drop(state);
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn artifact_deployment_round_trip_and_activation_are_atomic() {
        let path = temp_db("activation");
        let mut state = State::open(&path).expect("open state");
        let engine = EngineRecord {
            version: "1.2.3".into(),
            host_root: "/".into(),
            cage_executable: "/usr/bin/true".into(),
            sha256: "a".repeat(64),
            is_default: false,
        };
        let engine = state.register_engine(&engine).unwrap();
        let source_hash = "b".repeat(64);
        let artifact_hash = "c".repeat(64);
        let input = DeploymentInput {
            id: "dep-1".into(),
            app: "api".into(),
            source_hash: source_hash.clone(),
            engine_version: engine.version.clone(),
            source: DeploymentSource::cli(),
        };
        assert_eq!(
            state.begin_deployment(&input).unwrap().status,
            DeploymentStatus::Building
        );
        let success_logs = PathBuf::from("/var/log/cygnus/deployments/dep-1");
        state
            .set_deployment_log_path("dep-1", &success_logs)
            .unwrap();
        let artifact = artifact_input(
            "api",
            &source_hash,
            &artifact_hash,
            &engine.version,
            "/var/lib/cygnus/apps/api/c",
            "/app/index.js",
        );
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
            state
                .activate_deployment("dep-1", &app, None, &audit_context("activate-1"))
                .unwrap()
                .status,
            DeploymentStatus::Active
        );
        assert_eq!(state.load().unwrap().apps[0].name, "api");
        assert!(
            state
                .activate_deployment("dep-1", &app, None, &audit_context("activate-again"))
                .is_err()
        );
        assert_eq!(
            state.deployment("dep-1").unwrap().unwrap().status,
            DeploymentStatus::Active
        );
        assert_eq!(
            state.active_deployment("api").unwrap(),
            Some(ActiveDeploymentRecord {
                deployment_id: "dep-1".into(),
                artifact_hash: artifact_hash.clone(),
                engine_version: engine.version.clone(),
            })
        );
        assert_eq!(
            state.deployment_logs_dir("dep-1").unwrap(),
            Some(success_logs)
        );
        assert_eq!(
            state.deployments(Some("api"), None, 10).unwrap(),
            vec![state.deployment("dep-1").unwrap().unwrap()]
        );
        assert!(state.deployments(None, None, 0).is_err());
        let second_source_hash = "d".repeat(64);
        let second_artifact_hash = "e".repeat(64);
        state
            .begin_deployment(&DeploymentInput {
                id: "dep-2".into(),
                app: "worker".into(),
                source_hash: second_source_hash.clone(),
                engine_version: engine.version.clone(),
                source: DeploymentSource::cli(),
            })
            .unwrap();
        state
            .seal_deployment(
                "dep-2",
                &artifact_input(
                    "worker",
                    &second_source_hash,
                    &second_artifact_hash,
                    &engine.version,
                    "/var/lib/cygnus/apps/worker/e",
                    "/app/worker.js",
                ),
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
                .activate_deployment("dep-2", &worker, None, &audit_context("activate-2"))
                .is_ok()
        );
        assert_eq!(
            state.deployment("dep-2").unwrap().unwrap().status,
            DeploymentStatus::Active
        );
        assert_eq!(
            state
                .deployments(None, None, 10)
                .unwrap()
                .into_iter()
                .map(|deployment| deployment.id)
                .collect::<Vec<_>>(),
            ["dep-2", "dep-1"]
        );
        assert_eq!(
            state
                .deployments(None, Some("dep-2"), 10)
                .unwrap()
                .into_iter()
                .map(|deployment| deployment.id)
                .collect::<Vec<_>>(),
            ["dep-1"]
        );
        assert!(state.deployments(None, Some("missing"), 10).is_err());
        assert!(matches!(
            state.apply(&NodeConfig::default()),
            Err(StateError::DestructiveApply)
        ));
        drop(state);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn migrates_v2_deployments_preserving_active_row_and_adds_v3_shape() {
        let path = temp_db("v2-migrate");
        {
            let connection = Connection::open(&path).unwrap();
            create_schema(&connection).unwrap();
            migrate_v1_to_v2(&connection).unwrap();
            connection.execute_batch(
                "INSERT INTO engines VALUES (1, 'bun', '/', '/usr/bin/true', 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa');
                 INSERT INTO artifacts VALUES (1, 'api', 'bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb', 'cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc', 'bun', '/artifacts/c', '{\"bunVersion\":\"bun\",\"sourceHash\":\"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\",\"artifactHash\":\"cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc\",\"runtimeEntry\":\"/app/index.js\"}', 'sealed');
                 INSERT INTO deployments VALUES ('legacy-active', 'api', 'bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb', 'bun', 'cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc', 'active', NULL);
                 PRAGMA user_version = 2;",
            ).unwrap();
        }
        let state = State::open(&path).unwrap();
        let deployment = state.deployment("legacy-active").unwrap().unwrap();
        assert_eq!(deployment.status, DeploymentStatus::Active);
        assert!(!deployment.created_at.is_empty());
        assert!(!deployment.updated_at.is_empty());
        assert_eq!(deployment.log_path, None);
        let version: i32 = state
            .connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        drop(state);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn identical_artifact_can_seal_two_same_app_deployments_but_not_foreign_or_mismatched() {
        let path = temp_db("artifact-reuse");
        let mut state = State::open(&path).unwrap();
        let engine = EngineRecord {
            version: "bun".into(),
            host_root: "/".into(),
            cage_executable: "/usr/bin/true".into(),
            sha256: "a".repeat(64),
            is_default: false,
        };
        state.register_engine(&engine).unwrap();
        let source = "b".repeat(64);
        let hash = "c".repeat(64);
        let artifact = artifact_input(
            "api",
            &source,
            &hash,
            "bun",
            "/artifacts/c",
            "/app/index.js",
        );
        for id in ["dep-1", "dep-2"] {
            state
                .begin_deployment(&DeploymentInput {
                    id: id.into(),
                    app: "api".into(),
                    source_hash: source.clone(),
                    engine_version: "bun".into(),
                    source: DeploymentSource::cli(),
                })
                .unwrap();
            state.seal_deployment(id, &artifact).unwrap();
        }
        assert_eq!(
            state.deployment_artifact("dep-2").unwrap(),
            Some(state.deployment_artifact("dep-1").unwrap().unwrap())
        );
        state
            .begin_deployment(&DeploymentInput {
                id: "foreign".into(),
                app: "other".into(),
                source_hash: source.clone(),
                engine_version: "bun".into(),
                source: DeploymentSource::cli(),
            })
            .unwrap();
        let mut foreign = artifact.clone();
        foreign.app = "other".into();
        assert!(matches!(
            state.seal_deployment("foreign", &foreign),
            Err(StateError::ArtifactOwnership { .. })
        ));
        state
            .begin_deployment(&DeploymentInput {
                id: "mismatch".into(),
                app: "api".into(),
                source_hash: "d".repeat(64),
                engine_version: "bun".into(),
                source: DeploymentSource::cli(),
            })
            .unwrap();
        let mut mismatch = artifact;
        mismatch.source_hash = "d".repeat(64);
        mismatch.metadata_json = mismatch.metadata_json.replace(&source, &"d".repeat(64));
        assert!(matches!(
            state.seal_deployment("mismatch", &mismatch),
            Err(StateError::ArtifactOwnership { .. })
        ));
        drop(state);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn activation_cas_conflict_rolls_back_without_config_status_or_audit_writes() {
        let path = temp_db("activation-cas");
        let mut state = State::open(&path).unwrap();
        let engine = EngineRecord {
            version: "bun".into(),
            host_root: "/".into(),
            cage_executable: "/usr/bin/true".into(),
            sha256: "a".repeat(64),
            is_default: false,
        };
        state.register_engine(&engine).unwrap();
        let source = "b".repeat(64);
        let first_hash = "c".repeat(64);
        let second_hash = "d".repeat(64);
        for (id, hash, path) in [
            ("dep-1", &first_hash, "/artifacts/c"),
            ("dep-2", &second_hash, "/artifacts/d"),
        ] {
            state
                .begin_deployment(&DeploymentInput {
                    id: id.into(),
                    app: "api".into(),
                    source_hash: source.clone(),
                    engine_version: "bun".into(),
                    source: DeploymentSource::cli(),
                })
                .unwrap();
            state
                .seal_deployment(
                    id,
                    &artifact_input("api", &source, hash, "bun", path, "/app/index.js"),
                )
                .unwrap();
        }
        let first = AppConfig {
            name: "api".into(),
            domains: vec!["one.example".into()],
            upstream: "/run/api-dep-1.sock".into(),
            command: "/usr/bin/true".into(),
            ..AppConfig::default()
        };
        state
            .activate_deployment("dep-1", &first, None, &audit_context("first"))
            .unwrap();
        let replacement = AppConfig {
            domains: vec!["two.example".into()],
            upstream: "/run/api-dep-2.sock".into(),
            ..first.clone()
        };
        let error = state
            .activate_deployment(
                "dep-2",
                &replacement,
                Some(&"e".repeat(64)),
                &audit_context("conflict"),
            )
            .unwrap_err();
        assert!(matches!(error, StateError::ActivationConflict { .. }));
        assert_eq!(
            state
                .active_deployment("api")
                .unwrap()
                .unwrap()
                .deployment_id,
            "dep-1"
        );
        assert_eq!(
            state.deployment("dep-2").unwrap().unwrap().status,
            DeploymentStatus::Sealed
        );
        assert_eq!(state.load().unwrap().apps[0].domains, ["one.example"]);
        assert_eq!(state.audit_records().unwrap().len(), 1);
        drop(state);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn rollback_plan_is_read_only_and_active_sealed_active_transition_is_atomic() {
        let path = temp_db("rollback-plan");
        let mut state = State::open(&path).unwrap();
        let engine = EngineRecord {
            version: "bun".into(),
            host_root: "/".into(),
            cage_executable: "/usr/bin/true".into(),
            sha256: "a".repeat(64),
            is_default: false,
        };
        state.register_engine(&engine).unwrap();
        let source = "b".repeat(64);
        for (id, hash, artifact_path, entry) in [
            ("dep-1", "c".repeat(64), "/artifacts/c", "/app/one.js"),
            ("dep-2", "d".repeat(64), "/artifacts/d", "/app/two.js"),
        ] {
            state
                .begin_deployment(&DeploymentInput {
                    id: id.into(),
                    app: "api".into(),
                    source_hash: source.clone(),
                    engine_version: "bun".into(),
                    source: DeploymentSource::cli(),
                })
                .unwrap();
            state
                .seal_deployment(
                    id,
                    &artifact_input("api", &source, &hash, "bun", artifact_path, entry),
                )
                .unwrap();
        }
        let app = AppConfig {
            name: "api".into(),
            domains: vec!["api.example".into()],
            upstream: "/run/api-initial.sock".into(),
            command: "/usr/bin/true".into(),
            ..AppConfig::default()
        };
        state
            .activate_deployment("dep-1", &app, None, &audit_context("one"))
            .unwrap();
        let first_hash = "c".repeat(64);
        let plan = state.plan_rollback("api", "dep-2", &first_hash).unwrap();
        assert_eq!(
            plan.expected_active_artifact.as_deref(),
            Some(first_hash.as_str())
        );
        let second_hash = "d".repeat(64);
        assert_eq!(plan.runtime_key, format!("r-{second_hash}"));
        assert_eq!(plan.candidate.spec.name, format!("r-{second_hash}"));
        assert_eq!(
            plan.candidate.upstream,
            PathBuf::from(format!(
                "/run/r-{}.sock",
                &second_hash[..REVISION_SOCKET_HASH_CHARS]
            ))
        );
        assert_eq!(
            plan.candidate.spec.args.last().map(OsString::as_os_str),
            Some(std::ffi::OsStr::new("/app/two.js"))
        );
        assert_eq!(
            state.deployment("dep-2").unwrap().unwrap().status,
            DeploymentStatus::Sealed
        );
        state
            .commit_activation(&plan, &audit_context("two"))
            .unwrap();
        assert_eq!(
            state.deployment("dep-1").unwrap().unwrap().status,
            DeploymentStatus::Sealed
        );
        let reverse = state
            .plan_rollback("api", "dep-1", &"d".repeat(64))
            .unwrap();
        state
            .commit_activation(&reverse, &audit_context("rollback"))
            .unwrap();
        assert_eq!(
            state.deployment("dep-1").unwrap().unwrap().status,
            DeploymentStatus::Active
        );
        assert_eq!(
            state.deployment("dep-2").unwrap().unwrap().status,
            DeploymentStatus::Sealed
        );
        drop(state);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn audit_rows_are_typed_and_append_only() {
        let path = temp_db("audit-append-only");
        let mut state = State::open(&path).unwrap();
        state
            .append_audit(
                &audit_context("request-1"),
                AuditOutcome::Failure,
                Some("conflict"),
            )
            .unwrap();
        let records = state.audit_records().unwrap();
        assert_eq!(records[0].endpoint_role, AuditEndpointRole::Host);
        assert_eq!(records[0].outcome, AuditOutcome::Failure);
        assert_eq!(records[0].error_code.as_deref(), Some("conflict"));
        assert!(
            state
                .connection
                .execute(
                    "UPDATE audit_log SET error_code = NULL WHERE id = ?1",
                    [records[0].id]
                )
                .is_err()
        );
        assert!(
            state
                .connection
                .execute("DELETE FROM audit_log WHERE id = ?1", [records[0].id])
                .is_err()
        );
        drop(state);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn deployment_log_path_is_persisted_for_terminal_queries() {
        let path = temp_db("log-path");
        let mut state = State::open(&path).unwrap();
        state
            .register_engine(&EngineRecord {
                version: "bun".into(),
                host_root: "/".into(),
                cage_executable: "/usr/bin/true".into(),
                sha256: "a".repeat(64),
                is_default: false,
            })
            .unwrap();
        state
            .begin_deployment(&DeploymentInput {
                id: "dep".into(),
                app: "api".into(),
                source_hash: "b".repeat(64),
                engine_version: "bun".into(),
                source: DeploymentSource::cli(),
            })
            .unwrap();
        state
            .set_deployment_log_path("dep", Path::new("/var/log/cygnus/dep"))
            .unwrap();
        state.mark_deployment_failed("dep", "failed").unwrap();
        assert_eq!(
            state.deployment("dep").unwrap().unwrap().log_path,
            Some(PathBuf::from("/var/log/cygnus/dep"))
        );
        assert_eq!(
            state.deployment_logs_dir("dep").unwrap(),
            Some(PathBuf::from("/var/log/cygnus/dep"))
        );
        drop(state);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn sealing_requires_artifact_hash_and_runtime_entry_metadata() {
        let path = temp_db("metadata-switch-fields");
        let mut state = State::open(&path).unwrap();
        state
            .register_engine(&EngineRecord {
                version: "bun".into(),
                host_root: "/".into(),
                cage_executable: "/usr/bin/true".into(),
                sha256: "a".repeat(64),
                is_default: false,
            })
            .unwrap();
        let source = "b".repeat(64);
        let hash = "c".repeat(64);
        state
            .begin_deployment(&DeploymentInput {
                id: "dep".into(),
                app: "api".into(),
                source_hash: source.clone(),
                engine_version: "bun".into(),
                source: DeploymentSource::cli(),
            })
            .unwrap();
        let mut artifact = artifact_input(
            "api",
            &source,
            &hash,
            "bun",
            "/artifacts/c",
            "/app/index.js",
        );
        artifact.metadata_json = format!(
            "{{\"bunVersion\":\"bun\",\"sourceHash\":\"{source}\",\"runtimeEntry\":\"/app/index.js\"}}"
        );
        assert!(matches!(
            state.seal_deployment("dep", &artifact),
            Err(StateError::MetadataMismatch)
        ));
        artifact.metadata_json = format!(
            "{{\"bunVersion\":\"bun\",\"sourceHash\":\"{source}\",\"artifactHash\":\"{hash}\"}}"
        );
        assert!(matches!(
            state.seal_deployment("dep", &artifact),
            Err(StateError::MetadataMismatch)
        ));
        assert_eq!(
            state.deployment("dep").unwrap().unwrap().status,
            DeploymentStatus::Building
        );
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
                    sha256: "A".repeat(64),
                    is_default: false,
                })
                .is_err()
        );
        let engine = EngineRecord {
            version: "1".into(),
            host_root: "/".into(),
            cage_executable: "/usr/bin/true".into(),
            sha256: "d".repeat(64),
            is_default: false,
        };
        state.register_engine(&engine).unwrap();
        assert!(
            state
                .begin_deployment(&DeploymentInput {
                    id: "".into(),
                    app: "api".into(),
                    source_hash: "e".repeat(64),
                    engine_version: "1".into(),
                    source: DeploymentSource::cli(),
                })
                .is_err()
        );
        state
            .begin_deployment(&DeploymentInput {
                id: "dep".into(),
                app: "api".into(),
                source_hash: "e".repeat(64),
                engine_version: "1".into(),
                source: DeploymentSource::cli(),
            })
            .unwrap();
        state.mark_deployment_failed("dep", "build failed").unwrap();
        assert!(state.mark_deployment_failed("dep", "again").is_err());
        drop(state);
        let _ = fs::remove_file(path);
    }
    #[test]
    fn tenant_admin_allows_one_app_rooted_or_not_and_round_trips() {
        let path = temp_db("tenant-admin");
        let mut state = State::open(&path).unwrap();
        let mut input = config();
        // A rootless Tenant Zero is a legal configuration: plain-process
        // cages (macOS development, operator choice on Linux) have no
        // private root, and the daemon hands them the host socket path.
        input.apps[0].tenant_admin = true;
        state.apply(&input).unwrap();
        assert!(state.load().unwrap().apps[0].tenant_admin);

        input.apps[0].rootfs = Some(RootfsConfig {
            lowerdirs: vec![PathBuf::from("/lower")],
            ..RootfsConfig::default()
        });
        state.apply(&input).unwrap();
        assert!(state.load().unwrap().apps[0].tenant_admin);

        input.apps.push(AppConfig {
            name: "other".into(),
            upstream: "/run/other.sock".into(),
            command: "/bin/true".into(),
            rootfs: Some(RootfsConfig {
                lowerdirs: vec![PathBuf::from("/other")],
                ..RootfsConfig::default()
            }),
            tenant_admin: true,
            ..AppConfig::default()
        });
        assert!(matches!(
            state.apply(&input),
            Err(StateError::InvalidConfig(message)) if message.contains("only one app")
        ));
        drop(state);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn domain_mapping_is_canonical_atomic_and_conflict_safe() {
        let path = temp_db("map-domain");
        let mut state = State::open(&path).unwrap();
        let mut input = config();
        input.apps.push(AppConfig {
            name: "other".into(),
            upstream: "/run/other.sock".into(),
            command: "/bin/true".into(),
            ..AppConfig::default()
        });
        state.apply(&input).unwrap();
        let audit = audit_context("map-domain");

        let canonical = state.map_domain("api", "New.Example.COM.", &audit).unwrap();
        assert_eq!(canonical, "new.example.com");
        assert!(
            state
                .load()
                .unwrap()
                .apps
                .iter()
                .find(|app| app.name == "api")
                .unwrap()
                .domains
                .contains(&canonical)
        );
        assert_eq!(state.audit_records().unwrap().len(), 1);

        assert!(matches!(
            state.map_domain("other", &canonical, &audit),
            Err(StateError::DomainConflict { owner, .. }) if owner == "api"
        ));
        assert_eq!(state.audit_records().unwrap().len(), 1);
        drop(state);
        let _ = fs::remove_file(path);
    }
    #[test]
    fn edge_configuration_is_canonical_persisted_and_audited() {
        let path = temp_db("edge-config");
        let mut state = State::open(&path).unwrap();
        let mut input = config();
        input.edge = EdgeConfig {
            https_listen: Some("0.0.0.0:443".parse().unwrap()),
            apps_domain: Some("Apps.Example.COM.".into()),
            dashboard_domain: None,
            apex_domain: None,
            ssl_mode: SslMode::SelfSigned,
            acme: Some(AcmeConfig {
                email: "ops@example.com".into(),
                directory_url: crate::edge::DEFAULT_ACME_DIRECTORY.into(),
                dns_provider: Some("cloudflare".into()),
            }),
        };
        state.apply(&input).unwrap();
        assert_eq!(
            state.load().unwrap().edge.apps_domain.as_deref(),
            Some("apps.example.com")
        );

        let updated = EdgeConfig {
            https_listen: Some("127.0.0.1:8443".parse().unwrap()),
            apps_domain: Some("preview.example.com".into()),
            dashboard_domain: Some("console.example.com".into()),
            apex_domain: Some("native.example.com".into()),
            ssl_mode: SslMode::Acme,
            acme: Some(AcmeConfig {
                email: "admin@example.com".into(),
                directory_url: "https://acme.test/directory".into(),
                dns_provider: None,
            }),
        };
        state
            .update_edge_config(&updated, &audit_context("edge-update"))
            .unwrap();
        assert_eq!(state.load().unwrap().edge, updated);
        assert_eq!(state.audit_records().unwrap().len(), 1);

        let mut invalid = updated;
        invalid.https_listen = Some(input.listen);
        assert!(
            state
                .update_edge_config(&invalid, &audit_context("edge-invalid"))
                .is_err()
        );
        assert_eq!(state.audit_records().unwrap().len(), 1);
        drop(state);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn v9_domain_rows_migrate_as_custom_with_self_signed_pending_state() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "PRAGMA foreign_keys = ON;
                 CREATE TABLE apps (id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE);
                 CREATE TABLE domains (
                     id INTEGER PRIMARY KEY,
                     app_id INTEGER NOT NULL REFERENCES apps(id) ON DELETE CASCADE,
                     domain TEXT NOT NULL COLLATE BINARY UNIQUE
                 );
                 CREATE INDEX domains_app_id ON domains(app_id);
                 CREATE TABLE edge_config (
                     id INTEGER PRIMARY KEY CHECK (id = 1),
                     https_listen TEXT, apps_domain TEXT, acme_email TEXT,
                     acme_directory_url TEXT, dns_provider TEXT
                 );
                 INSERT INTO apps VALUES (1, 'api');
                 INSERT INTO domains VALUES (1, 1, 'api.example.com');
                 INSERT INTO edge_config (id) VALUES (1);",
            )
            .unwrap();
        migrate_v9_to_v10(&connection).unwrap();
        let row = connection
            .query_row(
                "SELECT kind, tls, status, expires_unix FROM domains WHERE id = 1",
                [],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, Option<i64>>(3)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(
            row,
            (
                "custom".into(),
                "self_signed".into(),
                "pending".into(),
                None
            )
        );
        let edge = connection
            .query_row(
                "SELECT dashboard_domain, apex_domain, ssl_mode FROM edge_config WHERE id = 1",
                [],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(edge, (None, None, "self_signed".into()));
    }

    #[test]
    fn apex_reconciliation_and_custom_lifecycle_are_atomic_and_audited() {
        let path = temp_db("domain-lifecycle");
        let mut state = State::open(&path).unwrap();
        let mut input = config();
        input.apps[0].domains = vec!["custom.example.net".into()];
        input.edge.apex_domain = Some("Example.COM.".into());
        state.apply(&input).unwrap();
        assert_eq!(
            native_domain("api", "example.com").unwrap(),
            "api.example.com"
        );
        let domains = state.app_domains(Some("api")).unwrap();
        assert_eq!(domains.len(), 2);
        assert!(domains.iter().any(|domain| domain.host == "api.example.com" && domain.kind == DomainKind::Native));
        assert!(
            domains
                .iter()
                .any(|domain| domain.host == "custom.example.net"
                    && domain.kind == DomainKind::Custom)
        );

        let custom = state
            .add_custom_domain("api", "WWW.Example.NET.", &audit_context("domain-add"))
            .unwrap();
        assert_eq!(custom.host, "www.example.net");
        let toggled = state
            .set_app_domain_tls(
                "api",
                "www.example.net",
                DomainTls::Acme,
                &audit_context("domain-tls"),
            )
            .unwrap();
        assert_eq!(toggled.tls, DomainTls::Acme);
        assert_eq!(toggled.status, DomainStatus::Pending);
        let active = state
            .update_domain_status(
                "www.example.net",
                DomainStatus::Active,
                Some(4_102_444_800),
                &audit_context("domain-status"),
            )
            .unwrap();
        assert_eq!(active.status, DomainStatus::Active);
        assert!(matches!(
            state.remove_custom_domain(
                "api",
                "api.example.com",
                &audit_context("domain-native-remove")
            ),
            Err(StateError::NativeDomainImmutable(_))
        ));
        state
            .remove_custom_domain("api", "www.example.net", &audit_context("domain-remove"))
            .unwrap();
        state
            .update_dashboard_domains(
                Some("Console.New.Example"),
                Some("new.example"),
                &audit_context("domain-apex"),
            )
            .unwrap();
        let domains = state.app_domains(Some("api")).unwrap();
        assert!(domains.iter().any(|domain| domain.host == "api.new.example" && domain.kind == DomainKind::Native));
        assert!(
            !domains
                .iter()
                .any(|domain| domain.host == "api.example.com")
        );
        assert!(
            domains
                .iter()
                .any(|domain| domain.host == "custom.example.net"
                    && domain.kind == DomainKind::Custom)
        );
        assert_eq!(
            state.load().unwrap().edge.dashboard_domain.as_deref(),
            Some("console.new.example")
        );
        assert_eq!(state.audit_records().unwrap().len(), 5);
        drop(state);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn certificate_generations_are_immutable_private_and_transactional() {
        let root = temp_db("certificate-store").with_extension("state");
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let path = root.join("state.db");
        let mut state = State::open(&path).unwrap();
        let certificate = b"-----BEGIN CERTIFICATE-----\nZmFrZQ==\n-----END CERTIFICATE-----\n";
        let private_key = b"-----BEGIN PRIVATE KEY-----\nZmFrZQ==\n-----END PRIVATE KEY-----\n";
        let first = state
            .install_certificate(
                &CertificateInput {
                    id: "apps-wildcard".into(),
                    domains: vec!["*.Apps.Example.COM.".into(), "api.example.com".into()],
                    certificate_pem: certificate.to_vec(),
                    private_key_pem: private_key.to_vec(),
                    not_after_unix: 4_102_444_800,
                },
                &audit_context("certificate-one"),
            )
            .unwrap();
        assert_eq!(first.domains, ["*.apps.example.com", "api.example.com"]);
        assert_eq!(
            fs::symlink_metadata(&first.private_key_path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(fs::read(&first.certificate_path).unwrap(), certificate);

        let second_certificate =
            b"-----BEGIN CERTIFICATE-----\nZGlmZmVyZW50\n-----END CERTIFICATE-----\n";
        let second = state
            .install_certificate(
                &CertificateInput {
                    id: "apps-wildcard".into(),
                    domains: vec!["*.apps.example.com".into()],
                    certificate_pem: second_certificate.to_vec(),
                    private_key_pem: private_key.to_vec(),
                    not_after_unix: 4_102_444_900,
                },
                &audit_context("certificate-two"),
            )
            .unwrap();
        assert_ne!(first.generation, second.generation);
        assert!(first.certificate_path.exists());
        assert_eq!(state.certificates().unwrap(), [second]);
        assert_eq!(state.audit_records().unwrap().len(), 2);

        assert!(matches!(
            state.install_certificate(
                &CertificateInput {
                    id: "conflict".into(),
                    domains: vec!["*.apps.example.com".into()],
                    certificate_pem: certificate.to_vec(),
                    private_key_pem: private_key.to_vec(),
                    not_after_unix: 4_102_445_000,
                },
                &audit_context("certificate-conflict"),
            ),
            Err(StateError::CertificateDomainConflict { owner, .. }) if owner == "apps-wildcard"
        ));
        assert_eq!(state.audit_records().unwrap().len(), 2);
        drop(state);
        fs::remove_dir_all(root).unwrap();
    }
    fn github_app_fixture() -> (GitHubAppRecord, GitHubAppSecrets) {
        (
            GitHubAppRecord {
                app_id: "123".into(),
                client_id: "client".into(),
                name: "Cygnus".into(),
                html_url: Some("https://github.com/apps/cygnus".into()),
                owner: Some("acme".into()),
                configured_at: "2026-01-01T00:00:00Z".into(),
            },
            GitHubAppSecrets {
                client_secret: "client-secret".into(),
                pem: "-----BEGIN PRIVATE KEY-----\nsecret\n-----END PRIVATE KEY-----".into(),
                webhook_secret: "webhook-secret".into(),
            },
        )
    }

    fn github_repo_fixture() -> GitHubRepositoryConfig {
        GitHubRepositoryConfig {
            installation_id: 7,
            repository_id: 8,
            owner: "acme".into(),
            name: "site".into(),
            branch: "main".into(),
            app: "site".into(),
            domain: "site.example.com".into(),
            engine_version: "bun".into(),
            entry: "index.ts".into(),
            artifact_root: "/var/lib/cygnus/artifacts/site".into(),
            upstream: "/run/cygnus/site.sock".into(),
        }
    }

    fn github_job_fixture(id: &str, sha: &str) -> GitHubJobSpec {
        GitHubJobSpec {
            id: id.into(),
            key: "installation:7/repository:8/production".into(),
            installation_id: 7,
            repository_id: 8,
            owner: "acme".into(),
            name: "site".into(),
            environment: "production".into(),
            kind: GitHubJobKind::Production,
            pull_request: None,
            sha: sha.into(),
        }
    }

    #[test]
    fn github_secrets_are_encrypted_and_reopenable_with_audited_write() {
        let path = temp_db("github-secrets");
        let (app, secrets) = github_app_fixture();
        let audit = audit_context("github-app");
        let state = {
            let mut state = State::open(&path).unwrap();
            state
                .set_github_app_with_audit(&app, &secrets, &audit)
                .unwrap();
            let blobs: (Vec<u8>, Vec<u8>, Vec<u8>) = state.connection.query_row(
                "SELECT client_secret, pem, webhook_secret FROM github_app_secrets WHERE app_id = 1",
                [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            ).unwrap();
            let joined = blobs
                .0
                .iter()
                .chain(blobs.1.iter())
                .chain(blobs.2.iter())
                .copied()
                .collect::<Vec<_>>();
            assert!(
                !joined
                    .windows(b"PRIVATE KEY".len())
                    .any(|window| window == b"PRIVATE KEY")
            );
            assert!(
                !joined
                    .windows(b"client-secret".len())
                    .any(|window| window == b"client-secret")
            );
            let loaded = state.github_app_secrets().unwrap().unwrap();
            assert_eq!(loaded.client_secret, secrets.client_secret);
            assert_eq!(loaded.pem, secrets.pem);
            assert_eq!(loaded.webhook_secret, secrets.webhook_secret);
            state
        };
        assert_eq!(state.audit_records().unwrap().len(), 1);
        drop(state);
        let key_path = path.parent().unwrap().join("node.key");
        let metadata = fs::metadata(&key_path).unwrap();
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
        let reopened = State::open(&path).unwrap();
        assert_eq!(reopened.github_app().unwrap().unwrap(), app);
        let loaded = reopened.github_app_secrets().unwrap().unwrap();
        assert_eq!(loaded.client_secret, secrets.client_secret);
        assert_eq!(loaded.pem, secrets.pem);
        assert_eq!(loaded.webhook_secret, secrets.webhook_secret);
        let _ = fs::remove_file(path);
        let _ = fs::remove_file(key_path);
    }

    #[test]
    fn github_secret_tampering_and_wrong_node_key_are_generic_auth_failures() {
        let path = temp_db("github-secret-tamper");
        let (_, secrets) = github_app_fixture();
        {
            let mut state = State::open(&path).unwrap();
            let (app, _) = github_app_fixture();
            state.set_github_app(&app, &secrets).unwrap();
            state
                .connection
                .execute(
                    "UPDATE github_app_secrets SET pem = zeroblob(length(pem)) WHERE app_id = 1",
                    [],
                )
                .unwrap();
            assert!(matches!(
                state.github_app_secrets(),
                Err(StateError::SecretAuthentication)
            ));
        }
        let key_path = path.parent().unwrap().join("node.key");
        let mut wrong = [0u8; NODE_KEY_LEN];
        wrong[0] = 1;
        fs::write(&key_path, wrong).unwrap();
        assert!(matches!(
            State::open(&path).unwrap().github_app_secrets(),
            Err(StateError::SecretAuthentication)
        ));
        let _ = fs::remove_file(path);
        let _ = fs::remove_file(key_path);
    }

    #[test]
    fn github_delivery_jobs_are_idempotent_revision_aware_and_recoverable() {
        let path = temp_db("github-jobs");
        let mut state = State::open(&path).unwrap();
        state
            .configure_github_repository(&github_repo_fixture())
            .unwrap();
        let first = "a".repeat(64);
        let second = "b".repeat(64);
        let delivery = |id: &str| GitHubDelivery {
            delivery_id: id.into(),
            event: "push".into(),
            action: None,
            payload_path: format!("/tmp/{id}.json").into(),
            accepted_at: "2026-01-01T00:00:00Z".into(),
        };
        assert!(
            state
                .accept_github_delivery(&delivery("d1"), &[github_job_fixture("j1", &first)])
                .unwrap()
        );
        assert!(
            !state
                .accept_github_delivery(&delivery("d1"), &[github_job_fixture("j1", &first)])
                .unwrap()
        );
        let running = state.claim_github_job().unwrap().unwrap();
        assert_eq!(running.id, "j1");
        assert_eq!(running.entry, PathBuf::from("index.ts"));
        assert!(
            state
                .accept_github_delivery(&delivery("d2"), &[github_job_fixture("j2", &second)])
                .unwrap()
        );
        state
            .finish_github_job("j1", GitHubDeployJobStatus::Succeeded, None)
            .unwrap();
        assert_eq!(
            state.github_job("j1").unwrap().unwrap().status,
            GitHubDeployJobStatus::Running
        );
        assert_eq!(
            state.current_github_job(&running.key).unwrap().unwrap().id,
            "j2"
        );
        let recovered = state.recover_github_jobs().unwrap();
        assert_eq!(recovered, 1);
        assert_eq!(
            state.github_job("j1").unwrap().unwrap().status,
            GitHubDeployJobStatus::Retry
        );
        state.connection.execute("UPDATE deploy_jobs SET next_attempt_at = datetime(CURRENT_TIMESTAMP, '-1 second') WHERE id = 'j1'", []).unwrap();
        assert_eq!(state.claim_github_job().unwrap().unwrap().id, "j2");
        let _ = fs::remove_file(path.clone());
        let _ = fs::remove_file(path.parent().unwrap().join("node.key"));
    }

    #[test]
    fn github_installation_suspend_disables_repositories_and_cancels_queued_work() {
        let path = temp_db("github-suspend");
        let mut state = State::open(&path).unwrap();
        state
            .configure_github_repository(&github_repo_fixture())
            .unwrap();
        let spec = github_job_fixture("suspend-job", &"c".repeat(64));
        let delivery = GitHubDelivery {
            delivery_id: "suspend-delivery".into(),
            event: "push".into(),
            action: None,
            payload_path: "/tmp/suspend.json".into(),
            accepted_at: "2026-01-01T00:00:00Z".into(),
        };
        state.accept_github_delivery(&delivery, &[spec]).unwrap();
        state
            .reconcile_github_event("installation", Some("suspend"), 7, &[])
            .unwrap();
        assert!(state.github_repository(7, 8).unwrap().is_none());
        assert_eq!(
            state.github_job("suspend-job").unwrap().unwrap().status,
            GitHubDeployJobStatus::Cancelled
        );
        let _ = fs::remove_file(path.clone());
        let _ = fs::remove_file(path.parent().unwrap().join("node.key"));
    }
    #[test]
    fn migrates_v4_plaintext_github_app_to_encrypted_columns() {
        let path = temp_db("github-v4-migrate");
        let (app, secrets) = github_app_fixture();
        {
            let connection = Connection::open(&path).unwrap();
            create_schema(&connection).unwrap();
            migrate_v1_to_v2(&connection).unwrap();
            migrate_v2_to_v3(&connection).unwrap();
            create_github_schema_v4(&connection).unwrap();
            connection.execute("INSERT INTO github_app (id, app_id, client_id, name, html_url, owner, client_secret, pem, webhook_secret, configured_at) VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)", params![app.app_id, app.client_id, app.name, app.html_url, app.owner, secrets.client_secret, secrets.pem, secrets.webhook_secret, app.configured_at]).unwrap();
            connection
                .pragma_update(None, "user_version", 4_i32)
                .unwrap();
        }
        let state = State::open(&path).unwrap();
        assert_eq!(state.github_app().unwrap().unwrap(), app);
        let loaded = state.github_app_secrets().unwrap().unwrap();
        assert_eq!(loaded.client_secret, secrets.client_secret);
        assert_eq!(loaded.pem, secrets.pem);
        assert_eq!(loaded.webhook_secret, secrets.webhook_secret);
        let raw = fs::read(&path).unwrap();
        assert!(
            !raw.windows(b"client-secret".len())
                .any(|window| window == b"client-secret")
        );
        assert!(
            !raw.windows(b"PRIVATE KEY".len())
                .any(|window| window == b"PRIVATE KEY")
        );
        let key_path = path.parent().unwrap().join("node.key");
        let _ = fs::remove_file(path.clone());
        let _ = fs::remove_file(key_path);
    }

    fn deploy_job_fixture(
        id: &str,
        key: &str,
        source: DeployJobSource,
        source_ref: &str,
    ) -> DeployJobSpec {
        DeployJobSpec {
            id: id.into(),
            key: key.into(),
            source,
            source_path: format!("/tmp/{id}.tar").into(),
            source_ref: source_ref.into(),
            app: "site".into(),
            domain: "site.example.com".into(),
            engine_version: "bun".into(),
            entry: "index.ts".into(),
            artifact_root: "/var/lib/cygnus/artifacts/site".into(),
            upstream: "/run/cygnus/site.sock".into(),
            branch: None,
            commit: None,
            installation_id: None,
            repository_id: None,
            owner: None,
            name: None,
            environment: None,
            kind: None,
            pull_request: None,
        }
    }

    #[test]
    fn generic_deploy_queue_supports_all_source_neutral_transitions() {
        let path = temp_db("generic-deploy-jobs");
        let mut state = State::open(&path).unwrap();
        let first = deploy_job_fixture("upload-1", "site", DeployJobSource::Upload, "one");
        let second = deploy_job_fixture("upload-2", "site", DeployJobSource::Upload, "two");
        let cli = deploy_job_fixture("cli-1", "other", DeployJobSource::Cli, "working-tree");
        assert!(state.enqueue_deploy_job(&first).unwrap());
        assert!(!state.enqueue_deploy_job(&first).unwrap());
        assert!(state.enqueue_deploy_job(&second).unwrap());
        assert!(state.enqueue_deploy_job(&cli).unwrap());
        assert_eq!(
            state.deploy_job("upload-1").unwrap().unwrap().status,
            DeployJobStatus::Cancelled
        );
        let listed = state.deploy_jobs(20, None).unwrap();
        assert_eq!(listed.len(), 3);
        assert!(listed.iter().all(|job| job.installation_id.is_none()));

        let running = state.claim_deploy_job().unwrap().unwrap();
        assert_eq!(running.id, "upload-2");
        assert_eq!(running.status, DeployJobStatus::Running);
        state
            .attach_deployment_id(&running.id, "local-deployment-1")
            .unwrap();
        state
            .finish_deploy_job(&running.id, DeployJobStatus::Succeeded, None)
            .unwrap();
        let finished = state.deploy_job(&running.id).unwrap().unwrap();
        assert_eq!(finished.status, DeployJobStatus::Succeeded);
        assert_eq!(
            finished.deployment_id.as_deref(),
            Some("local-deployment-1")
        );

        let running = state.claim_deploy_job().unwrap().unwrap();
        assert_eq!(running.id, "cli-1");
        assert_eq!(state.recover_deploy_jobs().unwrap(), 1);
        assert_eq!(
            state.deploy_job("cli-1").unwrap().unwrap().status,
            DeployJobStatus::Retry
        );
        let retried = state.retry_deploy_job("cli-1").unwrap();
        assert_eq!(retried.status, DeployJobStatus::Queued);
        let _ = fs::remove_file(path.clone());
        let _ = fs::remove_file(path.parent().unwrap().join("node.key"));
    }

    #[test]
    fn migrates_v6_github_jobs_losslessly_into_generic_queue() {
        let path = temp_db("github-v6-jobs-migrate");
        let node_key = load_node_key(&path).unwrap();
        {
            let connection = Connection::open(&path).unwrap();
            create_schema(&connection).unwrap();
            migrate_v1_to_v2(&connection).unwrap();
            migrate_v2_to_v3(&connection).unwrap();
            create_github_schema_v4(&connection).unwrap();
            migrate_v4_to_v5(&connection, &node_key).unwrap();
            migrate_v5_to_v6(&connection).unwrap();
            let repo = github_repo_fixture();
            connection.execute(
                "INSERT INTO github_repositories (installation_id, repository_id, owner, name, branch, app, domain, engine_version, entry, artifact_root, upstream, enabled) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 1)",
                params![repo.installation_id, repo.repository_id, repo.owner, repo.name, repo.branch, repo.app, repo.domain, repo.engine_version, repo.entry.to_string_lossy(), repo.artifact_root.to_string_lossy(), repo.upstream.to_string_lossy()],
            ).unwrap();
            connection.execute(
                "INSERT INTO github_deploy_jobs (id, job_key, installation_id, repository_id, owner, name, environment, kind, pull_request, sha, status, attempts, next_attempt_at, error, check_run_id, deployment_id, created_at, updated_at) VALUES ('legacy', 'legacy-key', 7, 8, 'acme', 'site', 'production', 'production', NULL, ?1, 'retry', 3, '2026-01-02 03:04:05', 'temporary', 91, 92, '2026-01-01 01:02:03', '2026-01-01 02:03:04')",
                [&"d".repeat(64)],
            ).unwrap();
            connection
                .pragma_update(None, "user_version", 6_i32)
                .unwrap();
        }
        let state = State::open(&path).unwrap();
        let job = state.deploy_job("legacy").unwrap().unwrap();
        assert_eq!(job.source, DeployJobSource::GitHub);
        assert_eq!(job.source_path, PathBuf::from("acme/site"));
        assert_eq!(job.source_ref, "d".repeat(64));
        assert_eq!(job.app, "site");
        assert_eq!(job.branch.as_deref(), Some("main"));
        assert_eq!(job.commit.as_deref(), Some("d".repeat(64).as_str()));
        assert_eq!(job.installation_id, Some(7));
        assert_eq!(job.repository_id, Some(8));
        assert_eq!(job.status, DeployJobStatus::Retry);
        assert_eq!(job.attempts, 3);
        assert_eq!(job.error.as_deref(), Some("temporary"));
        assert_eq!(job.check_run_id, Some(91));
        assert_eq!(job.github_deployment_id, Some(92));
        assert_eq!(job.created_at, "2026-01-01 01:02:03");
        assert_eq!(job.updated_at, "2026-01-01 02:03:04");
        let github = state.github_job("legacy").unwrap().unwrap();
        assert_eq!(github.sha, "d".repeat(64));
        assert_eq!(github.deployment_id, Some(92));
        let _ = fs::remove_file(path.clone());
        let _ = fs::remove_file(path.parent().unwrap().join("node.key"));
    }

    #[test]
    fn rejects_node_key_wrong_mode_and_symlink() {
        let path = temp_db("github-node-key");
        let state = State::open(&path).unwrap();
        drop(state);
        let key_path = path.parent().unwrap().join("node.key");
        let mut permissions = fs::metadata(&key_path).unwrap().permissions();
        permissions.set_mode(0o644);
        fs::set_permissions(&key_path, permissions).unwrap();
        assert!(
            matches!(State::open(&path), Err(StateError::InvalidConfig(message)) if message.contains("permissions"))
        );
        fs::remove_file(&key_path).unwrap();
        let target = path.parent().unwrap().join("key-target");
        fs::write(&target, [0u8; NODE_KEY_LEN]).unwrap();
        let mut target_permissions = fs::metadata(&target).unwrap().permissions();
        target_permissions.set_mode(0o600);
        fs::set_permissions(&target, target_permissions).unwrap();
        std::os::unix::fs::symlink(&target, &key_path).unwrap();
        assert!(
            matches!(State::open(&path), Err(StateError::InvalidConfig(message)) if message.contains("regular"))
        );
        let _ = fs::remove_file(path.clone());
        let _ = fs::remove_file(key_path);
        let _ = fs::remove_file(target);
    }
}
