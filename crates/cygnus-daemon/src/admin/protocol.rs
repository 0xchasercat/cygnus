use std::io::{self, Read, Write};

use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::deploy::DeployRequest;
pub use crate::github::{
    GitHubInstallationRepositoryView, GitHubManifestMetadata, GitHubRepositoryInput,
    GitHubRepositoryView,
};
use crate::state::NodeConfig;

pub const ADMIN_PROTOCOL_VERSION: u16 = 1;
pub const MAX_ADMIN_FRAME_BYTES: usize = 64 * 1024;
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
    Status,
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
    },
    Deploy {
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
}

const fn default_list_limit() -> u16 {
    50
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
    pub fn filename(self) -> &'static str {
        match self {
            Self::Stdout => "build.stdout.log",
            Self::Stderr => "build.stderr.log",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum AdminResponse {
    Ok {
        version: u16,
        request_id: String,
        data: AdminData,
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
            data,
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

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum AdminData {
    Health {
        service: String,
        isolation: String,
    },
    Status {
        node: NodeView,
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
    ConfigApplied {
        listen: String,
        app_count: usize,
    },
    EngineRegistered {
        version: String,
        sha256: String,
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
        job: GitHubJobView,
    },
    InstallationRepositories {
        repositories: Vec<GitHubInstallationRepositoryView>,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NodeView {
    pub listen: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub https_listen: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub apps_domain: Option<String>,
    pub app_count: usize,
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
    pub installation_id: i64,
    pub repository_id: i64,
    pub owner: String,
    pub name: String,
    pub environment: String,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pull_request: Option<i64>,
    pub sha: String,
    pub status: String,
    pub attempts: u32,
    pub next_attempt_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check_run_id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deployment_id: Option<i64>,
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
