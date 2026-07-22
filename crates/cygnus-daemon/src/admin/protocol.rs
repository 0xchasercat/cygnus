use std::io::{self, Read, Write};

use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::deploy::DeployRequest;
use crate::edge::SslMode;
pub use crate::github::{
    GitHubInstallationRepositoryView, GitHubInstallationView, GitHubManifestMetadata,
    GitHubRepositoryInput, GitHubRepositoryView,
};
use crate::metrics::{EventRecord, MetricsSnapshot, RequestRecord};
use crate::state::{
    DeployJobSource, DeploymentSource, DomainKind, DomainStatus, DomainTls, NodeConfig,
};

pub const ADMIN_PROTOCOL_VERSION: u16 = 1;
// Discovery of large GitHub installations can serialize hundreds of repo
// records; 1 MiB keeps that under one frame without unbounded growth.
pub const MAX_ADMIN_FRAME_BYTES: usize = 1024 * 1024;
pub const MAX_LOG_CHUNK_BYTES: u32 = 48 * 1024;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AdminRequest {
    pub version: u16,
    pub request_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<String>,
    pub command: AdminCommand,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum AdminCommand {
    Health,
    AccountStatus,
    CreateInitialAccount {
        email: String,
        password: String,
    },
    VerifyCredentials {
        email: String,
        password: String,
    },
    ChangePassword {
        email: String,
        current_password: String,
        new_password: String,
    },
    Status,
    SetDashboardDomain {
        domain: Option<String>,
        apex: Option<String>,
    },
    SetDashboardTls {
        mode: SslMode,
        /// Let's Encrypt contact email. Required when enabling ACME if none is
        /// stored yet; ignored when switching to self_signed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        email: Option<String>,
    },
    ListAppDomains {
        app: String,
    },
    AddAppDomain {
        app: String,
        host: String,
    },
    RemoveAppDomain {
        app: String,
        host: String,
    },
    SetAppDomainTls {
        app: String,
        host: String,
        mode: DomainTls,
    },
    SetPrimaryDomain {
        app: String,
        host: String,
    },
    RetryDomainAcme {
        app: String,
        host: String,
    },
    ListEnvVars {
        app: String,
    },
    SetEnvVar {
        app: String,
        key: String,
        value: String,
    },
    RemoveEnvVar {
        app: String,
        key: String,
    },
    GetMetrics,
    ListRequests {
        #[serde(default = "default_metrics_list_limit")]
        limit: u16,
    },
    ListEvents {
        #[serde(default = "default_metrics_list_limit")]
        limit: u16,
    },
    ReadAppLog {
        app: String,
        stream: LogStream,
        #[serde(default)]
        offset: u64,
        #[serde(default = "default_log_limit")]
        limit: u32,
    },
    ListApps {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cursor: Option<String>,
        #[serde(default = "default_list_limit")]
        limit: u16,
    },
    GetApp {
        app: String,
    },
    ListDeployments {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        app: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cursor: Option<String>,
        #[serde(default = "default_list_limit")]
        limit: u16,
    },
    GetDeployment {
        deployment: String,
    },
    ApplyConfig(NodeConfig),
    RegisterEngine {
        version: String,
        host_root: std::path::PathBuf,
        cage_executable: std::path::PathBuf,
        #[serde(default, rename = "default")]
        is_default: bool,
    },
    Deploy {
        request: DeployRequest,
    },
    DeployUploadBegin {
        app: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        domain: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        engine_version: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        entry: Option<std::path::PathBuf>,
        #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
        env: std::collections::BTreeMap<String, String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        preview: Option<String>,
        total_bytes: u64,
    },
    DeployUploadChunk {
        upload_id: String,
        chunk_base64: String,
    },
    DeployUploadFinish {
        upload_id: String,
    },
    DeployStart {
        request: DeployRequest,
    },
    MapDomain {
        app: String,
        domain: String,
    },
    Rollback {
        app: String,
        deployment: String,
        expected_active_artifact: String,
    },
    #[serde(alias = "github_convert_manifest")]
    ConvertManifest {
        code: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        owner: Option<String>,
    },
    #[serde(alias = "github_status")]
    GitHubStatus,
    #[serde(alias = "list_github_repositories")]
    ListRepositories {
        #[serde(default = "default_list_limit")]
        limit: u16,
    },
    #[serde(alias = "configure_github_repository")]
    ConfigureRepository {
        repository: GitHubRepositoryInput,
    },
    WebhookBegin {
        delivery_id: String,
        event: String,
        signature: String,
        total_bytes: u64,
    },
    WebhookChunk {
        delivery_id: String,
        chunk_base64: String,
    },
    WebhookFinish {
        delivery_id: String,
    },
    ListDeployJobs {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cursor: Option<String>,
        #[serde(default = "default_list_limit")]
        limit: u16,
    },
    RetryDeployJob {
        job_id: String,
    },
    ReadLog {
        deployment: String,
        stream: LogStream,
        #[serde(default)]
        offset: u64,
        #[serde(default = "default_log_limit")]
        limit: u32,
    },
    ListInstallationRepositories {
        installation_id: i64,
    },
    /// Walk every installation of the configured GitHub App and return the
    /// union of accessible repositories. Preferred by the console so operators
    /// never have to paste an installation ID.
    ListDiscoverableRepositories,
}

const fn default_list_limit() -> u16 {
    50
}

const fn default_metrics_list_limit() -> u16 {
    100
}

const fn default_log_limit() -> u32 {
    16 * 1024
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LogStream {
    Stdout,
    Stderr,
}

impl LogStream {
    pub fn build_filename(self) -> &'static str {
        match self {
            Self::Stdout => "build.stdout.log",
            Self::Stderr => "build.stderr.log",
        }
    }

    pub fn app_filename(self) -> &'static str {
        match self {
            Self::Stdout => "stdout.log",
            Self::Stderr => "stderr.log",
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum AdminResponse {
    Ok {
        version: u16,
        request_id: String,
        data: Box<AdminData>,
    },
    Error {
        version: u16,
        request_id: String,
        error: AdminFault,
    },
}

impl AdminResponse {
    pub fn ok(request_id: impl Into<String>, data: AdminData) -> Self {
        Self::Ok {
            version: ADMIN_PROTOCOL_VERSION,
            request_id: request_id.into(),
            data: Box::new(data),
        }
    }

    pub fn error(
        request_id: impl Into<String>,
        code: AdminErrorCode,
        message: impl Into<String>,
    ) -> Self {
        Self::Error {
            version: ADMIN_PROTOCOL_VERSION,
            request_id: request_id.into(),
            error: AdminFault {
                code,
                message: message.into(),
            },
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum AdminData {
    Health {
        service: String,
        isolation: String,
    },
    AccountStatus {
        configured: bool,
    },
    InitialAccountCreated {
        subject: String,
    },
    Credentials {
        ok: bool,
        subject: Option<String>,
    },
    Status {
        node: NodeView,
    },
    Metrics {
        #[serde(flatten)]
        metrics: MetricsSnapshot,
    },
    Requests {
        requests: Vec<RequestRecord>,
    },
    Events {
        events: Vec<EventRecord>,
    },
    AppLog {
        app: String,
        stream: LogStream,
        offset: u64,
        next_offset: u64,
        eof: bool,
        data_base64: String,
    },
    Apps {
        apps: Vec<AppView>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        next_cursor: Option<String>,
    },
    App {
        app: AppView,
    },
    DomainMapped {
        app: String,
        domain: String,
    },
    DashboardDomainSet {
        domain: Option<String>,
        apex: Option<String>,
    },
    DashboardTlsSet {
        mode: SslMode,
    },
    AppDomains {
        domains: Vec<AppDomainView>,
    },
    AppDomainAdded {
        domain: AppDomainView,
    },
    AppDomainRemoved {
        app: String,
        host: String,
    },
    AppDomainTlsSet {
        domain: AppDomainView,
    },
    AppDomainPrimarySet {
        domain: AppDomainView,
    },
    AppDomainAcmeRetried {
        domain: AppDomainView,
    },
    PasswordChanged,
    EnvVars {
        vars: std::collections::BTreeMap<String, String>,
    },
    EnvVarSet {
        key: String,
    },
    EnvVarRemoved {
        key: String,
    },
    ConfigApplied {
        listen: String,
        app_count: usize,
    },
    EngineRegistered {
        version: String,
        sha256: String,
    },
    DeployUploadBegun {
        upload_id: String,
    },
    DeployUploadChunked {
        upload_id: String,
        received_bytes: u64,
    },
    DeployUploadFinished {
        deployment_id: String,
        job_id: String,
    },
    DeployStarted {
        deployment_id: String,
        job_id: String,
    },
    DeploymentActivated {
        app: String,
        deployment_id: String,
        artifact_hash: String,
        engine_version: String,
    },
    Activated {
        app: String,
        active: ActiveDeploymentView,
    },
    Deployments {
        deployments: Vec<DeploymentView>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        next_cursor: Option<String>,
    },
    Deployment {
        deployment: DeploymentView,
    },
    Log {
        deployment: String,
        stream: LogStream,
        offset: u64,
        next_offset: u64,
        eof: bool,
        data_base64: String,
    },
    ManifestConverted {
        app: GitHubManifestMetadata,
    },
    GitHubStatus {
        configured: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        app: Option<GitHubManifestMetadata>,
    },
    Repositories {
        repositories: Vec<GitHubRepositoryView>,
    },
    RepositoryConfigured {
        repository: GitHubRepositoryView,
    },
    WebhookBegun {
        delivery_id: String,
        duplicate: bool,
    },
    WebhookChunked {
        delivery_id: String,
        received_bytes: u64,
    },
    WebhookAccepted {
        delivery_id: String,
        duplicate: bool,
        jobs: usize,
    },
    DeployJobs {
        jobs: Vec<GitHubJobView>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        next_cursor: Option<String>,
    },
    DeployJobRetried {
        job: Box<GitHubJobView>,
    },
    InstallationRepositories {
        repositories: Vec<GitHubInstallationRepositoryView>,
    },
    DiscoverableRepositories {
        repositories: Vec<GitHubInstallationRepositoryView>,
        installations: Vec<GitHubInstallationView>,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DomainDnsView {
    pub expected_ip: Option<String>,
    pub resolves_to: Vec<String>,
    pub ok: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AppDomainView {
    pub host: String,
    pub kind: DomainKind,
    pub tls: DomainTls,
    pub status: DomainStatus,
    pub dns: DomainDnsView,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_unix: Option<i64>,
    /// Last ACME failure message, if any. `None` once issuance succeeds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Earliest time (unix seconds) the reconciler will retry a failed
    /// domain. `None` means eligible immediately (or never failed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_retry_unix: Option<i64>,
    pub is_primary: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NodeView {
    pub listen: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub https_listen: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub apps_domain: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dashboard_domain: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub apex_domain: Option<String>,
    #[serde(default)]
    pub ssl_mode: SslMode,
    /// Let's Encrypt contact email when ACME is configured (no secrets).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acme_email: Option<String>,
    pub app_count: usize,
    pub version: String,
    pub uptime_seconds: u64,
    pub isolation: String,
    pub warm_count: usize,
    pub engines: Vec<EngineView>,
    pub certificates: Vec<CertificateView>,
    pub memory: MemoryView,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EngineView {
    pub version: String,
    pub sha256: String,
    #[serde(rename = "default")]
    pub is_default: bool,
    pub apps: u32,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CertificateView {
    pub domain: String,
    pub kind: String,
    pub ok: bool,
    pub expires_unix: Option<i64>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryView {
    pub total_bytes: u64,
    pub available_bytes: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AppView {
    pub name: String,
    pub domains: Vec<String>,
    pub lifecycle_state: String,
    pub pinned: bool,
    pub idle_ttl_ms: u64,
    pub egress: String,
    pub memory_max: u64,
    pub env_keys: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active: Option<ActiveDeploymentView>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ActiveDeploymentView {
    pub deployment_id: String,
    pub artifact_hash: String,
    pub engine_version: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DeploymentView {
    pub id: String,
    pub app: String,
    pub source_hash: String,
    pub engine_version: String,
    pub created_ms: i64,
    pub updated_ms: i64,
    pub source: DeploymentSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_hash: Option<String>,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GitHubJobView {
    pub id: String,
    pub key: String,
    pub source: DeployJobSource,
    pub source_ref: String,
    pub app: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installation_id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository_id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pull_request: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha: Option<String>,
    pub status: String,
    pub attempts: u32,
    pub next_attempt_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check_run_id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub github_deployment_id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deployment_id: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AdminFault {
    pub code: AdminErrorCode,
    pub message: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AdminErrorCode {
    InvalidRequest,
    UnsupportedVersion,
    Unauthorized,
    NotFound,
    Conflict,
    Validation,
    Internal,
}

pub fn valid_request_id(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

pub fn read_frame<T: DeserializeOwned>(reader: &mut impl Read) -> io::Result<T> {
    let mut length = [0_u8; 4];
    reader.read_exact(&mut length)?;
    let length = u32::from_be_bytes(length) as usize;
    if length == 0 || length > MAX_ADMIN_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("admin frame length {length} is outside 1..={MAX_ADMIN_FRAME_BYTES}"),
        ));
    }
    let mut payload = vec![0_u8; length];
    reader.read_exact(&mut payload)?;
    serde_json::from_slice(&payload)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

pub fn write_frame<T: Serialize>(writer: &mut impl Write, value: &T) -> io::Result<()> {
    let payload = serde_json::to_vec(value)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if payload.is_empty() || payload.len() > MAX_ADMIN_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "encoded admin frame length {} is outside 1..={MAX_ADMIN_FRAME_BYTES}",
                payload.len()
            ),
        ));
    }
    writer.write_all(&(payload.len() as u32).to_be_bytes())?;
    writer.write_all(&payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_round_trip_preserves_tagged_command() {
        let request = AdminRequest {
            version: ADMIN_PROTOCOL_VERSION,
            actor: Some("operator@example.test".into()),
            request_id: "0123456789abcdef0123456789abcdef".into(),
            command: AdminCommand::ReadLog {
                deployment: "deploy-1".into(),
                stream: LogStream::Stderr,
                offset: 7,
                limit: 4096,
            },
        };
        let mut frame = Vec::new();
        write_frame(&mut frame, &request).unwrap();
        assert_eq!(
            read_frame::<AdminRequest>(&mut frame.as_slice()).unwrap(),
            request
        );
    }

    #[test]
    fn domain_lifecycle_protocol_shapes_are_frozen() {
        assert_eq!(
            serde_json::to_value(AdminCommand::SetDashboardDomain {
                domain: Some("console.example.com".into()),
                apex: Some("example.com".into()),
            })
            .unwrap(),
            serde_json::json!({
                "type": "set_dashboard_domain",
                "domain": "console.example.com",
                "apex": "example.com"
            })
        );
        assert_eq!(
            serde_json::to_value(AdminCommand::SetDashboardTls {
                mode: SslMode::SelfSigned,
                email: None,
            })
            .unwrap(),
            serde_json::json!({"type":"set_dashboard_tls","mode":"self_signed"})
        );
        assert_eq!(
            serde_json::to_value(AdminCommand::SetDashboardTls {
                mode: SslMode::Acme,
                email: Some("ops@example.com".into()),
            })
            .unwrap(),
            serde_json::json!({
                "type":"set_dashboard_tls",
                "mode":"acme",
                "email":"ops@example.com"
            })
        );
        assert_eq!(
            serde_json::to_value(AdminCommand::SetAppDomainTls {
                app: "api".into(),
                host: "api.example.com".into(),
                mode: DomainTls::Acme,
            })
            .unwrap(),
            serde_json::json!({
                "type":"set_app_domain_tls",
                "app":"api",
                "host":"api.example.com",
                "mode":"acme"
            })
        );
        let data = AdminData::AppDomains {
            domains: vec![AppDomainView {
                host: "api.example.com".into(),
                kind: DomainKind::Native,
                tls: DomainTls::Acme,
                status: DomainStatus::FallbackActive,
                dns: DomainDnsView {
                    expected_ip: Some("203.0.113.8".into()),
                    resolves_to: vec!["203.0.113.8".into()],
                    ok: true,
                },
                expires_unix: None,
                error: None,
                next_retry_unix: None,
                is_primary: false,
            }],
        };
        assert_eq!(
            serde_json::to_value(data).unwrap(),
            serde_json::json!({
                "kind":"app_domains",
                "domains":[{
                    "host":"api.example.com",
                    "kind":"native",
                    "tls":"acme",
                    "status":"fallback_active",
                    "dns":{
                        "expected_ip":"203.0.113.8",
                        "resolves_to":["203.0.113.8"],
                        "ok":true
                    },
                    "is_primary":false
                }]
            })
        );
    }

    #[test]
    fn account_auth_protocol_shapes_are_frozen() {
        let request_id = "0123456789abcdef0123456789abcdef";
        let account_status = AdminRequest {
            version: ADMIN_PROTOCOL_VERSION,
            request_id: request_id.into(),
            actor: None,
            command: AdminCommand::AccountStatus,
        };
        assert_eq!(
            serde_json::to_value(account_status).unwrap(),
            serde_json::json!({
                "version": 1,
                "request_id": request_id,
                "command": {"type": "account_status"}
            })
        );

        let create = AdminRequest {
            version: ADMIN_PROTOCOL_VERSION,
            request_id: request_id.into(),
            actor: None,
            command: AdminCommand::CreateInitialAccount {
                email: "admin@example.com".into(),
                password: "correct horse battery staple".into(),
            },
        };
        assert_eq!(
            serde_json::to_value(create).unwrap()["command"],
            serde_json::json!({
                "type": "create_initial_account",
                "email": "admin@example.com",
                "password": "correct horse battery staple"
            })
        );

        let verify = AdminRequest {
            version: ADMIN_PROTOCOL_VERSION,
            request_id: request_id.into(),
            actor: None,
            command: AdminCommand::VerifyCredentials {
                email: "admin@example.com".into(),
                password: "correct horse battery staple".into(),
            },
        };
        assert_eq!(
            serde_json::to_value(verify).unwrap()["command"],
            serde_json::json!({
                "type": "verify_credentials",
                "email": "admin@example.com",
                "password": "correct horse battery staple"
            })
        );

        for (data, expected) in [
            (
                AdminData::AccountStatus { configured: true },
                serde_json::json!({"kind": "account_status", "configured": true}),
            ),
            (
                AdminData::InitialAccountCreated {
                    subject: "account:1".into(),
                },
                serde_json::json!({
                    "kind": "initial_account_created",
                    "subject": "account:1"
                }),
            ),
            (
                AdminData::Credentials {
                    ok: false,
                    subject: None,
                },
                serde_json::json!({
                    "kind": "credentials",
                    "ok": false,
                    "subject": null
                }),
            ),
        ] {
            assert_eq!(serde_json::to_value(data).unwrap(), expected);
        }
    }

    #[test]
    fn deploy_command_uses_the_nested_request_contract() {
        let request = AdminRequest {
            version: ADMIN_PROTOCOL_VERSION,
            actor: Some("local:operator".into()),
            request_id: "0123456789abcdef0123456789abcdef".into(),
            command: AdminCommand::Deploy {
                request: DeployRequest::new(
                    "/srv/source",
                    "hello",
                    "hello.apps.test",
                    "1.3.14",
                    "src/index.ts",
                    "/var/lib/cygnus/artifacts",
                    "/run/cygnus/hello.sock",
                ),
            },
        };
        let encoded = serde_json::to_value(&request).unwrap();
        assert_eq!(encoded["command"]["type"], "deploy");
        assert_eq!(encoded["command"]["request"]["app"], "hello");
        assert!(encoded["command"].get("app").is_none());
        assert_eq!(
            serde_json::from_value::<AdminRequest>(encoded).unwrap(),
            request
        );
    }

    #[test]
    fn upload_and_async_deploy_commands_have_exact_frozen_json_shapes() {
        let begin = AdminCommand::DeployUploadBegin {
            app: "hello".into(),
            domain: Some("hello.example".into()),
            engine_version: None,
            entry: Some("src/index.ts".into()),
            env: Default::default(),
            preview: None,
            total_bytes: 123,
        };
        assert_eq!(
            serde_json::to_value(&begin).unwrap(),
            serde_json::json!({
                "type": "deploy_upload_begin",
                "app": "hello",
                "domain": "hello.example",
                "entry": "src/index.ts",
                "total_bytes": 123
            })
        );
        let a_id = "a".repeat(64);
        let b_id = "b".repeat(64);
        assert_eq!(
            serde_json::to_value(AdminCommand::DeployUploadChunk {
                upload_id: a_id.clone(),
                chunk_base64: "YQ==".into(),
            })
            .unwrap(),
            serde_json::json!({
                "type": "deploy_upload_chunk",
                "upload_id": a_id.clone(),
                "chunk_base64": "YQ=="
            })
        );
        assert_eq!(
            serde_json::to_value(AdminCommand::DeployUploadFinish {
                upload_id: b_id.clone(),
            })
            .unwrap(),
            serde_json::json!({
                "type": "deploy_upload_finish",
                "upload_id": b_id
            })
        );
        let start = AdminCommand::DeployStart {
            request: DeployRequest {
                source_dir: "/srv/source".into(),
                app: "hello".into(),
                domain: None,
                engine_version: None,
                entry: None,
                artifact_root: None,
                upstream: None,
                env: Default::default(),
                preview: None,
                deployment_id: None,
                source: DeploymentSource::cli(),
            },
        };
        assert_eq!(
            serde_json::to_value(start).unwrap(),
            serde_json::json!({
                "type": "deploy_start",
                "request": { "source_dir": "/srv/source", "app": "hello" }
            })
        );

        for (data, expected) in [
            (
                AdminData::DeployUploadBegun {
                    upload_id: a_id.clone(),
                },
                serde_json::json!({"kind":"deploy_upload_begun","upload_id":a_id.clone()}),
            ),
            (
                AdminData::DeployUploadChunked {
                    upload_id: a_id.clone(),
                    received_bytes: 12,
                },
                serde_json::json!({"kind":"deploy_upload_chunked","upload_id":a_id,"received_bytes":12}),
            ),
            (
                AdminData::DeployUploadFinished {
                    deployment_id: "deployment".into(),
                    job_id: "job".into(),
                },
                serde_json::json!({"kind":"deploy_upload_finished","deployment_id":"deployment","job_id":"job"}),
            ),
            (
                AdminData::DeployStarted {
                    deployment_id: "deployment".into(),
                    job_id: "job".into(),
                },
                serde_json::json!({"kind":"deploy_started","deployment_id":"deployment","job_id":"job"}),
            ),
        ] {
            assert_eq!(serde_json::to_value(data).unwrap(), expected);
        }
    }

    #[test]
    fn installation_repository_command_and_data_have_stable_nested_shapes() {
        let request: AdminRequest = serde_json::from_value(serde_json::json!({
            "version": 1,
            "request_id": "0123456789abcdef0123456789abcdef",
            "actor": "tenant:operator",
            "command": { "type": "list_installation_repositories", "installation_id": 42 }
        }))
        .unwrap();
        assert_eq!(
            request.command,
            AdminCommand::ListInstallationRepositories {
                installation_id: 42
            }
        );
        let encoded = serde_json::to_value(AdminData::InstallationRepositories {
            repositories: vec![GitHubInstallationRepositoryView {
                installation_id: 42,
                repository_id: 7,
                owner: "acme".into(),
                name: "web".into(),
                full_name: "acme/web".into(),
                default_branch: "main".into(),
                private: true,
            }],
        })
        .unwrap();
        assert_eq!(encoded["kind"], "installation_repositories");
        assert!(encoded["repositories"][0].get("artifact_root").is_none());
        assert!(encoded["repositories"][0].get("upstream").is_none());

        let configured = serde_json::to_value(AdminData::RepositoryConfigured {
            repository: GitHubRepositoryView {
                installation_id: 42,
                repository_id: 7,
                owner: "acme".into(),
                name: "web".into(),
                branch: "main".into(),
                app: "web".into(),
                domain: "web.example".into(),
                engine_version: "bun".into(),
                entry: "src/index.ts".into(),
            },
        })
        .unwrap();
        assert!(configured["repository"].get("artifact_root").is_none());
        assert!(configured["repository"].get("upstream").is_none());
    }

    #[test]
    fn observability_data_uses_exact_flat_field_names() {
        let metrics = serde_json::to_value(AdminData::Metrics {
            metrics: MetricsSnapshot::default(),
        })
        .unwrap();
        assert_eq!(metrics["kind"], "metrics");
        assert!(metrics.get("metrics").is_none());
        assert!(metrics.get("window_seconds").is_some());
        assert!(metrics.get("totals").is_some());
        assert!(metrics.get("series").is_some());
        assert!(metrics.get("boot_phases").is_some());
        assert!(metrics.get("apps").is_some());
        assert_eq!(
            serde_json::from_value::<AdminData>(metrics.clone()).unwrap(),
            AdminData::Metrics {
                metrics: MetricsSnapshot::default()
            }
        );

        let requests = serde_json::to_value(AdminData::Requests {
            requests: vec![RequestRecord {
                time_ms: 1,
                request_id: "req".into(),
                method: "GET".into(),
                host: "app.example".into(),
                app: "app".into(),
                path: "/".into(),
                status: 200,
                duration_ms: 1.5,
                cold: false,
                protocol: "http/1.1".into(),
                bytes_in: 2,
                bytes_out: 3,
                outcome: "proxied".into(),
            }],
        })
        .unwrap();
        assert_eq!(requests["kind"], "requests");
        assert_eq!(
            requests["requests"][0]
                .as_object()
                .unwrap()
                .keys()
                .cloned()
                .collect::<std::collections::BTreeSet<_>>(),
            [
                "app",
                "bytes_in",
                "bytes_out",
                "cold",
                "duration_ms",
                "host",
                "method",
                "path",
                "protocol",
                "request_id",
                "status",
                "time_ms",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect()
        );

        let events = serde_json::to_value(AdminData::Events {
            events: vec![EventRecord {
                time_ms: 1,
                r#type: "boot".into(),
                app: Some("app".into()),
                message: "ready".into(),
            }],
        })
        .unwrap();
        assert_eq!(events["kind"], "events");
        assert!(events["events"][0].get("type").is_some());
        assert!(events["events"][0].get("r#type").is_none());

        let app_log = serde_json::to_value(AdminData::AppLog {
            app: "api".into(),
            stream: LogStream::Stdout,
            offset: 2,
            next_offset: 5,
            eof: false,
            data_base64: "Y2Rl".into(),
        })
        .unwrap();
        assert_eq!(
            app_log
                .as_object()
                .unwrap()
                .keys()
                .cloned()
                .collect::<std::collections::BTreeSet<_>>(),
            [
                "app",
                "data_base64",
                "eof",
                "kind",
                "next_offset",
                "offset",
                "stream",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect()
        );
    }

    #[test]
    fn observability_command_defaults_and_engine_default_wire_name_are_stable() {
        let parse = |command| {
            serde_json::from_value::<AdminRequest>(serde_json::json!({
                "version": 1,
                "request_id": "0123456789abcdef0123456789abcdef",
                "command": command,
            }))
            .unwrap()
            .command
        };
        assert_eq!(
            parse(serde_json::json!({"type": "list_requests"})),
            AdminCommand::ListRequests { limit: 100 }
        );
        assert_eq!(
            parse(serde_json::json!({"type": "list_events"})),
            AdminCommand::ListEvents { limit: 100 }
        );
        assert_eq!(
            parse(serde_json::json!({
                "type": "read_app_log",
                "app": "api",
                "stream": "stdout"
            })),
            AdminCommand::ReadAppLog {
                app: "api".into(),
                stream: LogStream::Stdout,
                offset: 0,
                limit: 16 * 1024,
            }
        );
        let engine = parse(serde_json::json!({
            "type": "register_engine",
            "version": "1",
            "host_root": "/engine",
            "cage_executable": "/bin/bun"
        }));
        assert!(matches!(
            engine,
            AdminCommand::RegisterEngine {
                is_default: false,
                ..
            }
        ));
        let encoded = serde_json::to_value(AdminCommand::RegisterEngine {
            version: "1".into(),
            host_root: "/engine".into(),
            cage_executable: "/bin/bun".into(),
            is_default: true,
        })
        .unwrap();
        assert_eq!(encoded["default"], true);
        assert!(encoded.get("is_default").is_none());
    }

    #[test]
    fn frame_reader_rejects_zero_and_oversized_lengths_before_allocating() {
        assert_eq!(
            read_frame::<AdminRequest>(&mut [0_u8; 4].as_slice())
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
        let oversized = ((MAX_ADMIN_FRAME_BYTES + 1) as u32).to_be_bytes();
        assert_eq!(
            read_frame::<AdminRequest>(&mut oversized.as_slice())
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn deployment_view_uses_exact_nested_source_shape() {
        let view = DeploymentView {
            id: "deploy-1".into(),
            app: "hello".into(),
            source_hash: "a".repeat(64),
            engine_version: "bun".into(),
            created_ms: 1_700_000_000_000,
            updated_ms: 1_700_000_000_500,
            source: DeploymentSource::github(Some("main".into()), Some("b".repeat(64))),
            artifact_hash: None,
            status: "building".into(),
            error: None,
        };

        let encoded = serde_json::to_value(view).unwrap();

        assert_eq!(encoded["created_ms"], 1_700_000_000_000_i64);
        assert_eq!(
            encoded["source"],
            serde_json::json!({
                "kind": "github",
                "branch": "main",
                "commit": "b".repeat(64)
            })
        );
    }

    #[test]
    fn defaults_bound_lists_and_log_chunks() {
        let request: AdminRequest = serde_json::from_value(serde_json::json!({
            "version": 1,
            "request_id": "0123456789abcdef0123456789abcdef",
            "command": { "type": "list_deployments" }
        }))
        .unwrap();
        assert_eq!(
            request.command,
            AdminCommand::ListDeployments {
                app: None,
                cursor: None,
                limit: 50
            }
        );
        assert!(default_log_limit() <= MAX_LOG_CHUNK_BYTES);
        assert!(valid_request_id(&request.request_id));
        assert!(!valid_request_id("ABCDEF0123456789ABCDEF0123456789"));
    }
}
