//! GitHub App GitOps integration owned entirely by the daemon.
//!
//! This module deliberately exposes capability-specific operations rather than a
//! generic HTTP proxy. Private keys, webhook secrets, JWTs, and installation
//! tokens remain inside this module and are never part of admin data.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use flate2::read::GzDecoder;
use hmac::{Hmac, Mac};
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tar::Archive;
use thiserror::Error;

use crate::deploy::{DeployError, DeployRequest, DeployResult, deploy_with_audit_and_prepare};
use crate::state::{
    AuditContext, DeployJob, DeployJobSource, DeployJobSpec, DeploymentInput, DeploymentSource,
    GitHubAppRecord, GitHubAppSecrets, GitHubDelivery, GitHubDeployJob, GitHubDeployJobStatus,
    GitHubJobKind, GitHubRepositoryConfig, State, StateError, provisional_source_hash,
};

pub const GITHUB_API_VERSION: &str = "2026-03-10";
pub const GITHUB_USER_AGENT: &str = "cygnus-tenant-zero";
pub const MAX_WEBHOOK_CHUNK_FRAME_BYTES: usize = 64 * 1024;
pub const MAX_GITHUB_WEBHOOK_BYTES: u64 = 25 * 1024 * 1024;
pub const MAX_GITHUB_WEBHOOK_CHUNK_BYTES: usize = 48 * 1024;
pub const MAX_ARCHIVE_BODY_BYTES: usize = 256 * 1024 * 1024;
pub const MAX_NORMAL_BODY_BYTES: usize = 1024 * 1024;
pub const MAX_ARCHIVE_EXTRACTED_BYTES: u64 = 256 * 1024 * 1024;
const MAX_WEBHOOK_SESSIONS: usize = 8;
const MAX_WEBHOOK_AGGREGATE_BYTES: u64 = MAX_GITHUB_WEBHOOK_BYTES;
const WEBHOOK_READ_BUFFER: usize = 64 * 1024;
const WEBHOOK_SESSION_TIMEOUT: Duration = Duration::from_secs(15 * 60);

#[derive(Debug, Error)]
pub enum GitHubError {
    #[error("GitHub state error: {0}")]
    State(#[from] StateError),
    #[error("GitHub filesystem error: {0}")]
    Io(#[from] io::Error),
    #[error("GitHub HTTP error: {0}")]
    Http(String),
    #[error("GitHub JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("GitHub input is invalid: {0}")]
    InvalidInput(String),
    #[error("GitHub webhook signature is invalid")]
    InvalidSignature,
    #[error("GitHub webhook delivery is incomplete")]
    IncompleteWebhook,
    #[error("GitHub archive is unsafe: {0}")]
    UnsafeArchive(String),
    #[error("GitHub deployment failed: {0}")]
    Deploy(String),
    #[error("GitHub operation should be retried: {0}")]
    Transient(String),
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GitHubManifest {
    pub name: String,
    pub url: String,
    pub redirect_url: String,
    pub setup_url: String,
    pub callback_urls: Vec<String>,
    pub public: bool,
    pub hook_attributes: GitHubHookAttributes,
    pub default_permissions: HashMap<String, String>,
    pub default_events: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GitHubHookAttributes {
    pub url: String,
    pub active: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GitHubManifestMetadata {
    pub app_id: String,
    pub client_id: String,
    pub name: String,
    pub html_url: Option<String>,
    pub owner: Option<String>,
    pub configured_at: String,
}

impl From<GitHubAppRecord> for GitHubManifestMetadata {
    fn from(value: GitHubAppRecord) -> Self {
        Self {
            app_id: value.app_id,
            client_id: value.client_id,
            name: value.name,
            html_url: value.html_url,
            owner: value.owner,
            configured_at: value.configured_at,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GitHubRepositoryInput {
    pub installation_id: i64,
    pub repository_id: i64,
    pub owner: String,
    pub name: String,
    pub branch: String,
    pub app: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry: Option<PathBuf>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GitHubRepositoryView {
    pub installation_id: i64,
    pub repository_id: i64,
    pub owner: String,
    pub name: String,
    pub branch: String,
    pub app: String,
    pub domain: String,
    pub engine_version: String,
    pub entry: String,
}

impl From<GitHubRepositoryConfig> for GitHubRepositoryView {
    fn from(value: GitHubRepositoryConfig) -> Self {
        Self {
            installation_id: value.installation_id,
            repository_id: value.repository_id,
            owner: value.owner,
            name: value.name,
            branch: value.branch,
            app: value.app,
            domain: value.domain,
            engine_version: value.engine_version,
            entry: value.entry.to_string_lossy().into_owned(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GitHubInstallationRepositoryView {
    pub installation_id: i64,
    pub repository_id: i64,
    pub owner: String,
    pub name: String,
    pub full_name: String,
    pub default_branch: String,
    pub private: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GitHubInstallationView {
    pub installation_id: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_login: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_type: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitHubHttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// The only HTTP boundary used by GitHubManager. Tests can provide a fake
/// transport and assert exact headers without exposing a generic admin proxy.
pub trait GitHubTransport: Send + Sync + 'static {
    fn request(
        &self,
        method: &str,
        path: &str,
        authorization: Option<&str>,
        body: Option<&[u8]>,
    ) -> Result<GitHubHttpResponse, GitHubError>;

    fn request_limited(
        &self,
        method: &str,
        path: &str,
        authorization: Option<&str>,
        body: Option<&[u8]>,
        max_body_bytes: usize,
    ) -> Result<GitHubHttpResponse, GitHubError> {
        let response = self.request(method, path, authorization, body)?;
        if response.body.len() > max_body_bytes {
            return Err(GitHubError::Transient(format!(
                "GitHub response exceeded {max_body_bytes} byte bound"
            )));
        }
        Ok(response)
    }
}

#[derive(Clone)]
pub struct UreqGitHubTransport {
    base_url: String,
    agent: Arc<ureq::Agent>,
}

impl UreqGitHubTransport {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            agent: Arc::new(ureq::Agent::new_with_defaults()),
        }
    }
}

impl GitHubTransport for UreqGitHubTransport {
    fn request(
        &self,
        method: &str,
        path: &str,
        authorization: Option<&str>,
        body: Option<&[u8]>,
    ) -> Result<GitHubHttpResponse, GitHubError> {
        let url = format!("{}{}", self.base_url, path);
        let mut request = ureq::http::Request::builder()
            .method(method)
            .uri(url)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", GITHUB_API_VERSION)
            .header("User-Agent", GITHUB_USER_AGENT);
        if body.is_some() {
            request = request.header("Content-Type", "application/json");
        }
        if let Some(auth) = authorization {
            request = request.header("Authorization", auth);
        }
        let request = request
            .body(body.unwrap_or_default().to_vec())
            .map_err(|error| GitHubError::Http(error.to_string()))?;
        let response = self
            .agent
            .run(request)
            .map_err(|error| GitHubError::Http(error.to_string()))?;
        let status = response.status().as_u16();
        let headers = response
            .headers()
            .iter()
            .map(|(name, value)| {
                (
                    name.to_string(),
                    value.to_str().unwrap_or_default().to_owned(),
                )
            })
            .collect();
        let body = response
            .into_body()
            .read_to_vec()
            .map_err(|error| GitHubError::Http(error.to_string()))?;
        Ok(GitHubHttpResponse {
            status,
            headers,
            body,
        })
    }
}

struct WebhookSession {
    delivery_id: String,
    event: String,
    signature: String,
    expected: u64,
    received: u64,
    next_offset: u64,
    path: PathBuf,
    started: SystemTime,
}

fn remove_webhook_session(sessions: &mut HashMap<String, WebhookSession>, delivery_id: &str) {
    if let Some(session) = sessions.remove(delivery_id) {
        let _ = fs::remove_file(session.path);
    }
}

#[derive(Clone)]
pub struct GitHubManager {
    state_path: PathBuf,
    transport: Arc<dyn GitHubTransport>,
    sessions: Arc<Mutex<HashMap<String, WebhookSession>>>,
}

impl GitHubManager {
    pub fn new(state_path: impl Into<PathBuf>) -> Self {
        Self::with_transport(
            state_path,
            Arc::new(UreqGitHubTransport::new("https://api.github.com")),
        )
    }

    pub fn with_transport(
        state_path: impl Into<PathBuf>,
        transport: Arc<dyn GitHubTransport>,
    ) -> Self {
        Self {
            state_path: state_path.into(),
            transport,
            sessions: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn manifest_conversion(
        &self,
        code: &str,
        owner: Option<&str>,
        audit: &AuditContext,
    ) -> Result<GitHubManifestMetadata, GitHubError> {
        if code.trim().is_empty() || code.len() > 512 || code.chars().any(char::is_control) {
            return Err(GitHubError::InvalidInput("manifest code is invalid".into()));
        }
        let response = self.transport.request_limited(
            "POST",
            &format!("/app-manifests/{code}/conversions"),
            None,
            Some(b"{}"),
            MAX_NORMAL_BODY_BYTES,
        )?;
        if response.status != 201 {
            return Err(http_status(
                "manifest conversion",
                response.status,
                &response.body,
            ));
        }
        let value: Value = serde_json::from_slice(&response.body)?;
        let app = GitHubAppRecord {
            app_id: value
                .get("id")
                .and_then(Value::as_i64)
                .map(|id| id.to_string())
                .or_else(|| {
                    value
                        .get("id")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned)
                })
                .ok_or_else(|| {
                    GitHubError::InvalidInput("conversion response omitted app id".into())
                })?,
            client_id: value
                .get("client_id")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    GitHubError::InvalidInput("conversion response omitted client id".into())
                })?
                .to_owned(),
            name: value
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("Cygnus Tenant Zero")
                .to_owned(),
            html_url: value
                .get("html_url")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            owner: owner.map(ToOwned::to_owned),
            configured_at: now_string(),
        };
        let secrets = GitHubAppSecrets {
            client_secret: value
                .get("client_secret")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    GitHubError::InvalidInput("conversion response omitted client secret".into())
                })?
                .to_owned(),
            pem: value
                .get("pem")
                .and_then(Value::as_str)
                .ok_or_else(|| GitHubError::InvalidInput("conversion response omitted PEM".into()))?
                .to_owned(),
            webhook_secret: value
                .get("webhook_secret")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    GitHubError::InvalidInput("conversion response omitted webhook secret".into())
                })?
                .to_owned(),
        };
        let mut state = State::open(&self.state_path)?;
        state.set_github_app_with_audit(&app, &secrets, audit)?;
        let stored = state.github_app()?.ok_or_else(|| {
            GitHubError::State(StateError::IncompleteState(
                "GitHub app disappeared after conversion".into(),
            ))
        })?;
        Ok(stored.into())
    }

    pub fn app_status(&self) -> Result<Option<GitHubManifestMetadata>, GitHubError> {
        Ok(State::open(&self.state_path)?.github_app()?.map(Into::into))
    }

    /// List every installation of this GitHub App via JWT auth.
    /// Used so the console never asks operators to paste installation IDs.
    pub fn list_installations(&self) -> Result<Vec<GitHubInstallationView>, GitHubError> {
        let jwt = self.app_jwt()?;
        let auth = format!("Bearer {jwt}");
        let mut page = 1_u32;
        let mut installations = Vec::new();
        loop {
            let path = format!("/app/installations?per_page=100&page={page}");
            let response = self.transport.request_limited(
                "GET",
                &path,
                Some(&auth),
                None,
                MAX_NORMAL_BODY_BYTES,
            )?;
            if response.status != 200 {
                return Err(http_status(
                    "installation listing",
                    response.status,
                    &response.body,
                ));
            }
            let value: Value = serde_json::from_slice(&response.body)?;
            let items = value.as_array().ok_or_else(|| {
                GitHubError::InvalidInput("installation listing response is not an array".into())
            })?;
            for item in items {
                let id = item
                    .get("id")
                    .and_then(Value::as_i64)
                    .ok_or_else(|| GitHubError::InvalidInput("installation omitted id".into()))?;
                if id <= 0 {
                    return Err(GitHubError::InvalidInput(
                        "installation id must be positive".into(),
                    ));
                }
                installations.push(GitHubInstallationView {
                    installation_id: id,
                    account_login: item
                        .pointer("/account/login")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned),
                    account_type: item
                        .pointer("/account/type")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned),
                });
            }
            let has_next = response
                .headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case("link"))
                .is_some_and(|(_, value)| value.contains("rel=\"next\""));
            if !has_next || items.is_empty() {
                break;
            }
            page = page.checked_add(1).ok_or_else(|| {
                GitHubError::InvalidInput("installation pagination overflow".into())
            })?;
            if page > 100 {
                return Err(GitHubError::InvalidInput(
                    "installation pagination exceeded bound".into(),
                ));
            }
        }
        Ok(installations)
    }

    /// Discover every repository the app can currently access by walking
    /// installations → installation tokens → /installation/repositories.
    pub fn discoverable_repositories(
        &self,
    ) -> Result<Vec<GitHubInstallationRepositoryView>, GitHubError> {
        let mut repositories = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for installation in self.list_installations()? {
            for repo in self.installation_repositories(installation.installation_id)? {
                if seen.insert(repo.repository_id) {
                    repositories.push(repo);
                }
            }
        }
        Ok(repositories)
    }

    pub fn configure_repository(
        &self,
        input: GitHubRepositoryInput,
        audit: &AuditContext,
    ) -> Result<GitHubRepositoryView, GitHubError> {
        if input.app.trim().is_empty() || !safe_component(&input.app) {
            return Err(GitHubError::InvalidInput(
                "app name is not a safe path component".into(),
            ));
        }
        let mut state = State::open(&self.state_path)?;
        // An empty stored entry means zero-config detection. Existing mappings
        // with index.ts remain explicit server deployments.
        let entry = input.entry.unwrap_or_default();
        if entry.is_absolute()
            || entry.components().any(|component| {
                matches!(
                    component,
                    Component::ParentDir | Component::RootDir | Component::Prefix(_)
                )
            })
        {
            return Err(GitHubError::InvalidInput(
                "entry must be a relative path inside the repository".into(),
            ));
        }
        let app = input.app.clone();
        let domain = match input.domain {
            Some(domain) if !domain.trim().is_empty() => domain,
            _ => {
                let edge = state.load()?.edge;
                let apex = edge.apex_domain.or(edge.apps_domain).ok_or_else(|| {
                    GitHubError::InvalidInput(
                        "domain was omitted and neither edge.apex_domain nor edge.apps_domain is configured"
                            .into(),
                    )
                })?;
                format!("{app}.{apex}")
            }
        };
        let engine_version = match input.engine_version {
            Some(version) if !version.trim().is_empty() => version,
            _ => state
                .default_engine()?
                .map(|engine| engine.version)
                .ok_or_else(|| {
                    GitHubError::InvalidInput(
                        "engine_version was omitted and no default engine is registered".into(),
                    )
                })?,
        };
        let config = GitHubRepositoryConfig {
            installation_id: input.installation_id,
            repository_id: input.repository_id,
            owner: input.owner,
            name: input.name,
            branch: input.branch,
            app: app.clone(),
            domain,
            engine_version,
            entry,
            artifact_root: state.deployment_artifact_root(&app),
            upstream: state.deployment_upstream(&app),
        };
        state.configure_github_repository_with_audit(&config, audit)?;
        Ok(config.into())
    }

    /// Trigger an initial build for a newly configured repository.
    /// Fetches the latest commit SHA from GitHub and enqueues a deploy job.
    pub fn trigger_initial_deploy(
        &self,
        installation_id: i64,
        repository_id: i64,
        _audit: &AuditContext,
    ) -> Result<DeployJob, GitHubError> {
        let state = State::open(&self.state_path)?;
        let config = state
            .github_repository(installation_id, repository_id)?
            .ok_or_else(|| GitHubError::InvalidInput("repository not configured".into()))?;

        // Fetch the latest commit SHA from GitHub.
        let token = self.installation_token(installation_id, None)?;
        let auth = format!("Bearer {token}");
        let path = format!(
            "/repos/{}/{}/commits/{}?per_page=1",
            config.owner, config.name, config.branch
        );
        let response = self.transport.request_limited(
            "GET",
            &path,
            Some(&auth),
            None,
            MAX_NORMAL_BODY_BYTES,
        )?;
        if response.status != 200 {
            return Err(http_status(
                "fetch latest commit",
                response.status,
                &response.body,
            ));
        }
        let value: Value = serde_json::from_slice(&response.body)?;
        let sha = value
            .get("sha")
            .and_then(Value::as_str)
            .ok_or_else(|| GitHubError::InvalidInput("commit response omitted sha".into()))?
            .to_owned();

        // Create and enqueue the deploy job.
        let job = DeployJobSpec {
            id: job_id("initial", installation_id, repository_id, None, &sha),
            key: format!("{installation_id}:{repository_id}:initial"),
            source: DeployJobSource::GitHub,
            source_path: PathBuf::from(format!("{}/{}", config.owner, config.name)),
            source_ref: sha.clone(),
            app: config.app,
            domain: config.domain,
            engine_version: config.engine_version,
            entry: config.entry,
            artifact_root: config.artifact_root,
            upstream: config.upstream,
            branch: Some(config.branch),
            commit: Some(sha),
            installation_id: Some(installation_id),
            repository_id: Some(repository_id),
            owner: Some(config.owner),
            name: Some(config.name),
            environment: Some("production".into()),
            kind: Some(GitHubJobKind::Production),
            pull_request: None,
        };

        let mut state = State::open(&self.state_path)?;
        state.enqueue_preassigned_deployment(
            &DeploymentInput {
                id: job.id.clone(),
                app: job.app.clone(),
                source_hash: provisional_source_hash(&job.source_ref),
                engine_version: job.engine_version.clone(),
                source: DeploymentSource::github(job.branch.clone(), job.commit.clone()),
            },
            &job,
        )?;
        state
            .deploy_job(&job.id)?
            .ok_or_else(|| GitHubError::InvalidInput("enqueued job not found".into()))
    }

    pub fn repositories(&self) -> Result<Vec<GitHubRepositoryView>, GitHubError> {
        Ok(State::open(&self.state_path)?
            .github_repositories()?
            .into_iter()
            .map(Into::into)
            .collect())
    }

    pub fn installation_repositories(
        &self,
        installation_id: i64,
    ) -> Result<Vec<GitHubInstallationRepositoryView>, GitHubError> {
        if installation_id <= 0 {
            return Err(GitHubError::InvalidInput(
                "installation id must be positive".into(),
            ));
        }
        let token = self.installation_token(installation_id, None)?;
        let auth = format!("Bearer {token}");
        let mut page = 1_u32;
        let mut repositories = Vec::new();
        loop {
            let path = format!("/installation/repositories?per_page=100&page={page}");
            let response = self.transport.request_limited(
                "GET",
                &path,
                Some(&auth),
                None,
                MAX_NORMAL_BODY_BYTES,
            )?;
            if response.status != 200 {
                return Err(http_status(
                    "installation repository discovery",
                    response.status,
                    &response.body,
                ));
            }
            let value: Value = serde_json::from_slice(&response.body)?;
            let Some(items) = value.get("repositories").and_then(Value::as_array) else {
                return Err(GitHubError::InvalidInput(
                    "installation repository response omitted repositories".into(),
                ));
            };
            for item in items {
                let id = item.get("id").and_then(Value::as_i64).ok_or_else(|| {
                    GitHubError::InvalidInput("installation repository omitted id".into())
                })?;
                let name = item.get("name").and_then(Value::as_str).unwrap_or_default();
                let owner = item
                    .pointer("/owner/login")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let default_branch = item
                    .get("default_branch")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if id <= 0 || name.is_empty() || owner.is_empty() || default_branch.is_empty() {
                    return Err(GitHubError::InvalidInput(
                        "installation repository metadata is incomplete".into(),
                    ));
                }
                repositories.push(GitHubInstallationRepositoryView {
                    installation_id,
                    repository_id: id,
                    owner: owner.to_owned(),
                    name: name.to_owned(),
                    full_name: item
                        .get("full_name")
                        .and_then(Value::as_str)
                        .unwrap_or(&format!("{owner}/{name}"))
                        .to_owned(),
                    default_branch: default_branch.to_owned(),
                    private: item
                        .get("private")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                });
            }
            let has_next = response
                .headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case("link"))
                .is_some_and(|(_, value)| value.contains("rel=\"next\""));
            if !has_next || items.is_empty() {
                break;
            }
            page = page.checked_add(1).ok_or_else(|| {
                GitHubError::InvalidInput("installation repository pagination overflow".into())
            })?;
            if page > 1000 {
                return Err(GitHubError::InvalidInput(
                    "installation repository pagination exceeded bound".into(),
                ));
            }
        }
        Ok(repositories)
    }

    pub fn app_jwt(&self) -> Result<String, GitHubError> {
        let state = State::open(&self.state_path)?;
        let app = state
            .github_app()?
            .ok_or_else(|| GitHubError::InvalidInput("GitHub App is not configured".into()))?;
        let secrets = state.github_app_secrets()?.ok_or_else(|| {
            GitHubError::InvalidInput("GitHub App secrets are unavailable".into())
        })?;
        #[derive(Serialize)]
        struct Claims<'a> {
            iat: u64,
            exp: u64,
            iss: &'a str,
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let claims = Claims {
            iat: now.saturating_sub(60),
            exp: now.saturating_add(9 * 60),
            iss: &app.client_id,
        };
        let key = EncodingKey::from_rsa_pem(secrets.pem.as_bytes()).map_err(|error| {
            GitHubError::InvalidInput(format!("invalid app private key: {error}"))
        })?;
        encode(&Header::new(Algorithm::RS256), &claims, &key)
            .map_err(|error| GitHubError::Http(format!("JWT signing failed: {error}")))
    }
    pub fn installation_token(
        &self,
        installation_id: i64,
        repository_id: Option<i64>,
    ) -> Result<String, GitHubError> {
        if installation_id <= 0 {
            return Err(GitHubError::InvalidInput(
                "installation id must be positive".into(),
            ));
        }
        let jwt = self.app_jwt()?;
        let body = repository_id
            .map(|id| json!({ "repository_ids": [id] }))
            .unwrap_or_else(|| json!({}));
        let encoded = serde_json::to_vec(&body)?;
        let auth = format!("Bearer {jwt}");
        let response = self.transport.request_limited(
            "POST",
            &format!("/app/installations/{installation_id}/access_tokens"),
            Some(&auth),
            Some(&encoded),
            MAX_NORMAL_BODY_BYTES,
        )?;
        if response.status != 201 {
            return Err(http_status(
                "installation token",
                response.status,
                &response.body,
            ));
        }
        let value: Value = serde_json::from_slice(&response.body)?;
        value
            .get("token")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
            .ok_or_else(|| {
                GitHubError::InvalidInput("installation token response omitted token".into())
            })
    }

    pub fn webhook_begin(
        &self,
        delivery_id: String,
        event: String,
        signature: String,
        expected: u64,
    ) -> Result<bool, GitHubError> {
        self.cleanup_webhooks();
        if delivery_id.trim().is_empty()
            || delivery_id.len() > 128
            || delivery_id.chars().any(char::is_control)
        {
            return Err(GitHubError::InvalidInput("delivery id is invalid".into()));
        }
        if event.trim().is_empty() || event.len() > 128 || event.chars().any(char::is_control) {
            return Err(GitHubError::InvalidInput("event is invalid".into()));
        }
        if expected == 0 || expected > MAX_GITHUB_WEBHOOK_BYTES {
            return Err(GitHubError::InvalidInput(
                "webhook exceeds 25 MiB bound".into(),
            ));
        }
        if !valid_signature_shape(&signature) {
            return Err(GitHubError::InvalidSignature);
        }
        let state = State::open(&self.state_path)?;
        if state.github_delivery_exists(&delivery_id)? {
            return Ok(false);
        }
        let mut sessions = self.sessions.lock();
        let promised = sessions
            .values()
            .try_fold(expected, |sum, item| sum.checked_add(item.expected))
            .ok_or_else(|| GitHubError::InvalidInput("webhook aggregate size overflow".into()))?;
        if sessions.len() >= MAX_WEBHOOK_SESSIONS || promised > MAX_WEBHOOK_AGGREGATE_BYTES {
            return Err(GitHubError::InvalidInput(
                "webhook session capacity exceeded".into(),
            ));
        }
        let directory = self
            .state_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("github-webhooks");
        fs::create_dir_all(&directory)?;
        let mut digest = Sha256::new();
        digest.update(delivery_id.as_bytes());
        let path = directory.join(format!("{}.part", hex::encode(digest.finalize())));
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path);
        if let Err(error) = file {
            if error.kind() == io::ErrorKind::AlreadyExists {
                return Err(GitHubError::InvalidInput(
                    "webhook delivery is already in progress".into(),
                ));
            }
            return Err(error.into());
        }
        sessions.insert(
            delivery_id.clone(),
            WebhookSession {
                delivery_id,
                event,
                signature,
                expected,
                received: 0,
                next_offset: 0,
                path,
                started: SystemTime::now(),
            },
        );
        Ok(true)
    }

    pub fn webhook_chunk(&self, delivery_id: &str, encoded: &str) -> Result<u64, GitHubError> {
        let offset = self
            .sessions
            .lock()
            .get(delivery_id)
            .map(|session| session.next_offset)
            .unwrap_or(0);
        self.webhook_chunk_at(delivery_id, offset, encoded)
    }

    pub fn webhook_chunk_at(
        &self,
        delivery_id: &str,
        offset: u64,
        encoded: &str,
    ) -> Result<u64, GitHubError> {
        let bytes = match BASE64.decode(encoded) {
            Ok(bytes) => bytes,
            Err(_) => {
                let mut sessions = self.sessions.lock();
                remove_webhook_session(&mut sessions, delivery_id);
                return Err(GitHubError::InvalidInput(
                    "webhook chunk is not valid base64".into(),
                ));
            }
        };
        if bytes.is_empty() || bytes.len() > MAX_GITHUB_WEBHOOK_CHUNK_BYTES {
            let mut sessions = self.sessions.lock();
            remove_webhook_session(&mut sessions, delivery_id);
            return Err(GitHubError::InvalidInput(
                "webhook chunk exceeds 48 KiB".into(),
            ));
        }
        let mut sessions = self.sessions.lock();
        let Some((path, received, expected, next_offset)) =
            sessions.get(delivery_id).map(|session| {
                (
                    session.path.clone(),
                    session.received,
                    session.expected,
                    session.next_offset,
                )
            })
        else {
            return Err(GitHubError::InvalidInput(
                "webhook session does not exist".into(),
            ));
        };
        if offset != next_offset {
            remove_webhook_session(&mut sessions, delivery_id);
            return Err(GitHubError::InvalidInput(
                "webhook chunk offset is out of order".into(),
            ));
        }
        let next = match received.checked_add(bytes.len() as u64) {
            Some(next) => next,
            None => {
                remove_webhook_session(&mut sessions, delivery_id);
                return Err(GitHubError::InvalidInput("webhook size overflow".into()));
            }
        };
        if next > expected {
            remove_webhook_session(&mut sessions, delivery_id);
            return Err(GitHubError::InvalidInput(
                "webhook has too many bytes".into(),
            ));
        }
        let result = (|| {
            let mut file = OpenOptions::new().write(true).open(&path)?;
            file.seek(SeekFrom::Start(offset))?;
            file.write_all(&bytes)?;
            file.sync_data()?;
            Ok::<(), io::Error>(())
        })();
        if let Err(error) = result {
            remove_webhook_session(&mut sessions, delivery_id);
            return Err(error.into());
        }
        if let Some(session) = sessions.get_mut(delivery_id) {
            session.received = next;
            session.next_offset = next;
        }
        Ok(next)
    }

    pub fn webhook_finish(
        &self,
        delivery_id: &str,
        audit: &AuditContext,
    ) -> Result<WebhookResult, GitHubError> {
        let session =
            self.sessions.lock().remove(delivery_id).ok_or_else(|| {
                GitHubError::InvalidInput("webhook session does not exist".into())
            })?;
        let path = session.path.clone();
        let result = (|| {
            if session.received != session.expected {
                return Err(GitHubError::IncompleteWebhook);
            }
            let secret = self.webhook_secret()?;
            verify_file_signature(&path, &session.signature, &secret)?;
            let payload = File::open(&path)?;
            let value: Value = serde_json::from_reader(payload)?;
            let action = value
                .get("action")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned);
            let mut state = State::open(&self.state_path)?;
            let installation_id = value
                .pointer("/installation/id")
                .and_then(Value::as_i64)
                .unwrap_or_default();
            let removed = value
                .get("repositories_removed")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| item.get("id").and_then(Value::as_i64))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            state.reconcile_github_event(
                &session.event,
                action.as_deref(),
                installation_id,
                &removed,
            )?;
            let jobs = derive_event_jobs(&state, &session.event, action.as_deref(), &value)?;
            if session.event == "pull_request"
                && action.as_deref() == Some("closed")
                && let Some(number) = value.get("number").and_then(Value::as_i64)
            {
                let key = format!(
                    "{}:{}:pr:{number}",
                    installation_id,
                    value
                        .pointer("/repository/id")
                        .and_then(Value::as_i64)
                        .unwrap_or_default()
                );
                for prior in state.github_jobs(200, None)? {
                    if prior.key == key
                        && matches!(
                            prior.status,
                            GitHubDeployJobStatus::Queued
                                | GitHubDeployJobStatus::Retry
                                | GitHubDeployJobStatus::Running
                        )
                    {
                        state.finish_github_job(
                            &prior.id,
                            GitHubDeployJobStatus::Cancelled,
                            Some("pull request closed"),
                        )?;
                    }
                }
            }
            let accepted = state.accept_github_delivery_jobs(
                &GitHubDelivery {
                    delivery_id: session.delivery_id.clone(),
                    event: session.event.clone(),
                    action,
                    payload_path: path.clone(),
                    accepted_at: now_string(),
                },
                &jobs,
            )?;
            let _ = audit;
            if !accepted {
                let _ = fs::remove_file(&path);
                return Ok(WebhookResult {
                    delivery_id: delivery_id.to_owned(),
                    duplicate: true,
                    jobs: 0,
                });
            }
            Ok(WebhookResult {
                delivery_id: delivery_id.to_owned(),
                duplicate: false,
                jobs: jobs.len(),
            })
        })();
        if result.is_err() {
            let _ = fs::remove_file(path);
        }
        result
    }

    fn webhook_secret(&self) -> Result<String, GitHubError> {
        State::open(&self.state_path)?
            .github_app_secrets()?
            .map(|secrets| secrets.webhook_secret)
            .ok_or_else(|| {
                GitHubError::InvalidInput("GitHub webhook secret is not configured".into())
            })
    }

    pub fn list_jobs(
        &self,
        limit: u16,
        cursor: Option<&str>,
    ) -> Result<Vec<GitHubDeployJob>, GitHubError> {
        Ok(State::open(&self.state_path)?.github_jobs(limit, cursor)?)
    }
    pub fn retry_job(
        &self,
        id: &str,
        audit: &AuditContext,
    ) -> Result<GitHubDeployJob, GitHubError> {
        let mut state = State::open(&self.state_path)?;
        Ok(state.retry_github_job_with_audit(id, audit)?)
    }

    pub fn cleanup_webhooks(&self) {
        let cutoff = SystemTime::now()
            .checked_sub(WEBHOOK_SESSION_TIMEOUT)
            .unwrap_or(UNIX_EPOCH);
        let expired = {
            let mut sessions = self.sessions.lock();
            let mut expired = Vec::new();
            sessions.retain(|_, session| {
                let keep = session.started > cutoff;
                if !keep {
                    expired.push(session.path.clone());
                }
                keep
            });
            expired
        };
        for path in expired {
            let _ = fs::remove_file(path);
        }
        let _ = fs::read_dir(
            self.state_path
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join("github-webhooks"),
        )
        .map(|entries| {
            for entry in entries.flatten() {
                if let Ok(metadata) = entry.metadata()
                    && metadata.modified().unwrap_or(UNIX_EPOCH) < cutoff
                {
                    let _ = fs::remove_file(entry.path());
                }
            }
        });
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct WebhookResult {
    pub delivery_id: String,
    pub duplicate: bool,
    pub jobs: usize,
}

fn valid_signature_shape(value: &str) -> bool {
    value.len() == 71
        && value.starts_with("sha256=")
        && value[7..].bytes().all(|b| b.is_ascii_hexdigit())
}

fn verify_file_signature(path: &Path, signature: &str, secret: &str) -> Result<(), GitHubError> {
    let expected = hex::decode(&signature[7..]).map_err(|_| GitHubError::InvalidSignature)?;
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes())
        .map_err(|_| GitHubError::InvalidSignature)?;
    let mut file = File::open(path)?;
    let mut buffer = [0_u8; WEBHOOK_READ_BUFFER];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        mac.update(&buffer[..read]);
    }
    mac.verify_slice(&expected)
        .map_err(|_| GitHubError::InvalidSignature)
}

fn safe_component(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn derive_event_jobs(
    state: &State,
    event: &str,
    action: Option<&str>,
    value: &Value,
) -> Result<Vec<DeployJobSpec>, GitHubError> {
    match event {
        "push" => derive_push_job(state, value),
        "pull_request" => derive_pull_request_job(state, action, value),
        "installation_repositories" => Ok(Vec::new()),
        "installation" => Ok(Vec::new()),
        _ => Ok(Vec::new()),
    }
}

fn derive_push_job(state: &State, value: &Value) -> Result<Vec<DeployJobSpec>, GitHubError> {
    if value
        .get("deleted")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return Ok(Vec::new());
    }
    let installation_id = value
        .pointer("/installation/id")
        .and_then(Value::as_i64)
        .unwrap_or_default();
    let repo = value
        .get("repository")
        .ok_or_else(|| GitHubError::InvalidInput("push omitted repository".into()))?;
    let repository_id = repo.get("id").and_then(Value::as_i64).unwrap_or_default();
    let ref_name = value.get("ref").and_then(Value::as_str).unwrap_or_default();
    let sha = value
        .get("after")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if installation_id <= 0
        || repository_id <= 0
        || sha.len() != 40
        || sha.chars().any(|c| !c.is_ascii_hexdigit())
    {
        return Ok(Vec::new());
    }
    let Some(config) = state.github_repository(installation_id, repository_id)? else {
        return Ok(Vec::new());
    };
    if ref_name != format!("refs/heads/{}", config.branch) {
        return Ok(Vec::new());
    }
    let owner = repo
        .get("owner")
        .and_then(|o| o.get("login"))
        .and_then(Value::as_str)
        .unwrap_or(&config.owner);
    Ok(vec![DeployJobSpec {
        id: job_id("production", installation_id, repository_id, None, sha),
        key: format!("{installation_id}:{repository_id}:production"),
        source: DeployJobSource::GitHub,
        source_path: PathBuf::from(format!("{owner}/{}", config.name)),
        source_ref: sha.to_owned(),
        app: config.app,
        domain: config.domain,
        engine_version: config.engine_version,
        entry: config.entry,
        artifact_root: config.artifact_root,
        upstream: config.upstream,
        branch: Some(config.branch),
        commit: Some(sha.to_owned()),
        installation_id: Some(installation_id),
        repository_id: Some(repository_id),
        owner: Some(owner.to_owned()),
        name: Some(config.name),
        environment: Some("production".into()),
        kind: Some(GitHubJobKind::Production),
        pull_request: None,
    }])
}

fn derive_pull_request_job(
    state: &State,
    action: Option<&str>,
    value: &Value,
) -> Result<Vec<DeployJobSpec>, GitHubError> {
    let Some(action) = action else {
        return Ok(Vec::new());
    };
    let installation_id = value
        .pointer("/installation/id")
        .and_then(Value::as_i64)
        .unwrap_or_default();
    let repo = value
        .get("repository")
        .ok_or_else(|| GitHubError::InvalidInput("pull request omitted repository".into()))?;
    let repository_id = repo.get("id").and_then(Value::as_i64).unwrap_or_default();
    let number = value
        .get("number")
        .and_then(Value::as_i64)
        .unwrap_or_default();
    let pull = value.get("pull_request").unwrap_or(&Value::Null);
    let sha = pull
        .pointer("/head/sha")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if installation_id <= 0
        || repository_id <= 0
        || number <= 0
        || sha.len() != 40
        || sha.chars().any(|c| !c.is_ascii_hexdigit())
    {
        return Ok(Vec::new());
    }
    let Some(config) = state.github_repository(installation_id, repository_id)? else {
        return Ok(Vec::new());
    };
    if action == "closed" || !matches!(action, "opened" | "reopened" | "synchronize") {
        return Ok(Vec::new());
    }
    let Some(head_repo_id) = pull.pointer("/head/repo/id").and_then(Value::as_i64) else {
        return Ok(Vec::new());
    };
    let Some(base_ref) = pull.pointer("/base/ref").and_then(Value::as_str) else {
        return Ok(Vec::new());
    };
    if head_repo_id != repository_id || base_ref != config.branch {
        return Ok(Vec::new());
    }
    Ok(vec![DeployJobSpec {
        id: job_id("preview", installation_id, repository_id, Some(number), sha),
        key: format!("{installation_id}:{repository_id}:pr:{number}"),
        source: DeployJobSource::GitHub,
        source_path: PathBuf::from(format!("{}/{}", config.owner, config.name)),
        source_ref: sha.to_owned(),
        app: config.app,
        domain: config.domain,
        engine_version: config.engine_version,
        entry: config.entry,
        artifact_root: config.artifact_root,
        upstream: config.upstream,
        branch: Some(config.branch),
        commit: Some(sha.to_owned()),
        installation_id: Some(installation_id),
        repository_id: Some(repository_id),
        owner: Some(config.owner),
        name: Some(config.name),
        environment: Some(format!("pr-{number}")),
        kind: Some(GitHubJobKind::Preview),
        pull_request: Some(number),
    }])
}

fn job_id(
    kind: &str,
    installation_id: i64,
    repository_id: i64,
    pr: Option<i64>,
    sha: &str,
) -> String {
    let mut digest = Sha256::new();
    digest.update(kind.as_bytes());
    digest.update(installation_id.to_be_bytes());
    digest.update(repository_id.to_be_bytes());
    digest.update(pr.unwrap_or_default().to_be_bytes());
    digest.update(sha.as_bytes());
    format!("gh-{:x}", digest.finalize())
}

fn now_string() -> String {
    format!("{}", chrono_like_now())
}
fn chrono_like_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn http_status(operation: &str, status: u16, _body: &[u8]) -> GitHubError {
    if status == 429 || status >= 500 {
        GitHubError::Transient(format!("{operation} returned HTTP {status}"))
    } else {
        GitHubError::Http(format!("{operation} returned HTTP {status}"))
    }
}

/// Extract a GitHub tarball without allowing traversal, links, or device nodes.
pub(crate) fn safe_extract_archive(bytes: &[u8], destination: &Path) -> Result<(), GitHubError> {
    safe_extract_archive_reader(bytes, destination)
}

/// Safely extract a tar or gzip-compressed tar stream into `destination`.
///
/// This public reader-based entry point is shared by the library's GitHub
/// ingestion and the daemon binary's upload worker. It rejects absolute and
/// parent-traversing paths, links, special files, duplicate output files, and
/// archives whose compressed or extracted size exceeds the configured bounds.
/// Callers must provide a private, daemon-owned destination directory.
pub fn safe_extract_archive_reader<R: Read>(
    mut reader: R,
    destination: &Path,
) -> Result<(), GitHubError> {
    let mut bytes = Vec::new();
    reader
        .by_ref()
        .take(MAX_ARCHIVE_BODY_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_ARCHIVE_BODY_BYTES {
        return Err(GitHubError::UnsafeArchive(
            "archive body exceeds 256 MiB".into(),
        ));
    }
    fs::create_dir_all(destination)?;
    if bytes.starts_with(&[0x1f, 0x8b]) {
        extract_tar(GzDecoder::new(bytes.as_slice()), destination)
    } else {
        extract_tar(bytes.as_slice(), destination)
    }
}

fn extract_tar<R: Read>(reader: R, destination: &Path) -> Result<(), GitHubError> {
    let mut archive = Archive::new(reader);
    let mut extracted = 0_u64;
    for item in archive
        .entries()
        .map_err(|error| GitHubError::UnsafeArchive(error.to_string()))?
    {
        let mut entry = item.map_err(|error| GitHubError::UnsafeArchive(error.to_string()))?;
        let path = entry
            .path()
            .map_err(|error| GitHubError::UnsafeArchive(error.to_string()))?
            .into_owned();
        if path.as_os_str().is_empty()
            || path.components().any(|component| {
                matches!(
                    component,
                    Component::ParentDir | Component::RootDir | Component::Prefix(_)
                )
            })
        {
            return Err(GitHubError::UnsafeArchive(format!(
                "path escapes archive root: {}",
                path.display()
            )));
        }
        let entry_type = entry.header().entry_type();
        if entry_type.is_symlink()
            || entry_type.is_hard_link()
            || entry_type.is_character_special()
            || entry_type.is_block_special()
            || entry_type.is_fifo()
        {
            return Err(GitHubError::UnsafeArchive(format!(
                "links and special files are not allowed: {}",
                path.display()
            )));
        }
        // Extract paths verbatim first. GitHub tarballs normally wrap every
        // file in one repository directory, but that directory is not
        // guaranteed to have its own tar entry. Deciding while streaming made
        // archives beginning with `repo-sha/package.json` remain one level too
        // deep, so framework detection could not see package.json.
        let output = destination.join(&path);
        if !output.starts_with(destination) {
            return Err(GitHubError::UnsafeArchive(
                "archive path escaped destination".into(),
            ));
        }
        if entry_type.is_dir() {
            fs::create_dir_all(&output)?;
            continue;
        }
        let size = entry
            .header()
            .size()
            .map_err(|error| GitHubError::UnsafeArchive(error.to_string()))?;
        extracted = extracted
            .checked_add(size)
            .ok_or_else(|| GitHubError::UnsafeArchive("archive extraction size overflow".into()))?;
        if extracted > MAX_ARCHIVE_EXTRACTED_BYTES {
            return Err(GitHubError::UnsafeArchive(
                "archive extraction exceeds 256 MiB".into(),
            ));
        }
        if let Some(parent) = output.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&output)
            .map_err(|error| GitHubError::UnsafeArchive(error.to_string()))?;
        let copied = io::copy(&mut entry, &mut file)
            .map_err(|error| GitHubError::UnsafeArchive(error.to_string()))?;
        if copied != size {
            return Err(GitHubError::UnsafeArchive(
                "archive entry size mismatch".into(),
            ));
        }
        file.sync_data()?;
    }
    flatten_single_archive_wrapper(destination)?;
    Ok(())
}

fn flatten_single_archive_wrapper(destination: &Path) -> Result<(), GitHubError> {
    let mut entries = fs::read_dir(destination)?.collect::<Result<Vec<_>, _>>()?;
    if entries.len() != 1 || !entries[0].file_type()?.is_dir() {
        return Ok(());
    }
    let wrapper = entries.pop().expect("one archive root entry").path();
    let children = fs::read_dir(&wrapper)?.collect::<Result<Vec<_>, _>>()?;
    for child in children {
        fs::rename(child.path(), destination.join(child.file_name()))?;
    }
    fs::remove_dir(wrapper)?;
    Ok(())
}

pub trait GitHubDeployExecutor: Send + Sync + 'static {
    fn deploy(
        &self,
        state: &mut State,
        job: &GitHubDeployJob,
        config: &GitHubRepositoryConfig,
        source: &Path,
        audit: &AuditContext,
    ) -> Result<DeployResult, DeployError>;
}

/// Default executor used by the daemon runtime. All paths come from the
/// daemon-owned repository mapping; no tenant path crosses this boundary.
pub struct TrustedDeployExecutor;

impl GitHubDeployExecutor for TrustedDeployExecutor {
    fn deploy(
        &self,
        state: &mut State,
        job: &GitHubDeployJob,
        config: &GitHubRepositoryConfig,
        source: &Path,
        audit: &AuditContext,
    ) -> Result<DeployResult, DeployError> {
        let preassigned = state.deployment(&job.id)?.map(|_| job.id.clone());
        deploy_with_audit_and_prepare(
            state,
            DeployRequest {
                source_dir: source.to_path_buf(),
                app: config.app.clone(),
                domain: Some(config.domain.clone()),
                engine_version: Some(config.engine_version.clone()),
                entry: (!job.entry.as_os_str().is_empty()).then(|| job.entry.clone()),
                artifact_root: Some(config.artifact_root.clone()),
                upstream: Some(config.upstream.clone()),
                env: std::collections::BTreeMap::new(),
                preview: None,
                deployment_id: preassigned,
                source: DeploymentSource::github(
                    Some(config.branch.clone()),
                    Some(job.sha.clone()),
                ),
            },
            audit,
            |_| Ok(crate::deploy::ActivationPreparation::new(|| {})),
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GitHubWorkerResult {
    Idle,
    Succeeded {
        job_id: String,
        deployment_id: String,
    },
    Failed {
        job_id: String,
        transient: bool,
    },
}

pub struct GitHubWorker {
    manager: GitHubManager,
    executor: Arc<dyn GitHubDeployExecutor>,
}
impl GitHubWorker {
    pub fn new(manager: GitHubManager, executor: Arc<dyn GitHubDeployExecutor>) -> Self {
        Self { manager, executor }
    }

    pub fn run_once(&self) -> Result<GitHubWorkerResult, GitHubError> {
        let mut state = State::open(&self.manager.state_path)?;
        let _ = state.recover_github_jobs()?;
        let Some(claimed) = state.claim_github_job()? else {
            return Ok(GitHubWorkerResult::Idle);
        };
        let mut job = claimed.clone();
        if let Some(current) = state.current_github_job(&job.key)?
            && current.id != job.id
            && current.sha != job.sha
        {
            state.finish_github_job(
                &job.id,
                GitHubDeployJobStatus::Cancelled,
                Some("superseded by newer revision"),
            )?;
            return Ok(GitHubWorkerResult::Failed {
                job_id: job.id,
                transient: false,
            });
        }
        if let Err(error) = self.start_reports(&mut state, &mut job) {
            // Check-run/deployment reporting is best-effort — don't block the
            // actual deploy if GitHub reporting fails (e.g. missing permissions,
            // 404 on a repo the app can read but not write checks to).
            let _ = error;
        }
        let result = self.run_claimed(&mut state, &job);
        match result {
            Ok(deployment_id) => {
                if let Err(report_error) = self.finish_reports(&job, true, None) {
                    let transient = matches!(report_error, GitHubError::Transient(_));
                    if transient {
                        state.retry_github_job_with_error(&job.id, &report_error.to_string())?;
                    } else {
                        state.finish_github_job(
                            &job.id,
                            GitHubDeployJobStatus::Failed,
                            Some(&report_error.to_string()),
                        )?;
                    }
                    return Ok(GitHubWorkerResult::Failed {
                        job_id: job.id,
                        transient,
                    });
                }
                state.finish_github_job(&job.id, GitHubDeployJobStatus::Succeeded, None)?;
                Ok(GitHubWorkerResult::Succeeded {
                    job_id: job.id,
                    deployment_id,
                })
            }
            Err(error) => {
                let transient = matches!(error, GitHubError::Transient(_));
                if transient {
                    state.retry_github_job_with_error(&job.id, &error.to_string())?;
                } else {
                    // Archive/config/report failures can happen before the
                    // deploy pipeline has a chance to create build logs. Keep
                    // the pre-created deployment visible with the durable
                    // error so the console never loses the failure in Events.
                    let _ = state.mark_deployment_failed(&job.id, &error.to_string());
                    state.finish_github_job(
                        &job.id,
                        GitHubDeployJobStatus::Failed,
                        Some(&error.to_string()),
                    )?;
                }
                let _ = self.finish_reports(&job, false, Some(&error.to_string()));
                Ok(GitHubWorkerResult::Failed {
                    job_id: job.id,
                    transient,
                })
            }
        }
    }

    fn report_request(
        &self,
        job: &GitHubDeployJob,
        method: &str,
        path: &str,
        body: Option<&[u8]>,
    ) -> Result<GitHubHttpResponse, GitHubError> {
        let mut token = self
            .manager
            .installation_token(job.installation_id, Some(job.repository_id))?;
        let mut response = self.manager.transport.request_limited(
            method,
            path,
            Some(&format!("Bearer {token}")),
            body,
            MAX_NORMAL_BODY_BYTES,
        )?;
        if response.status == 401 {
            token = self
                .manager
                .installation_token(job.installation_id, Some(job.repository_id))?;
            response = self.manager.transport.request_limited(
                method,
                path,
                Some(&format!("Bearer {token}")),
                body,
                MAX_NORMAL_BODY_BYTES,
            )?;
        }
        if response.status == 429 || response.status >= 500 {
            return Err(http_status(
                "GitHub report",
                response.status,
                &response.body,
            ));
        }
        if response.status == 403 || response.status == 409 || response.status == 422 {
            return Err(GitHubError::Http(format!(
                "GitHub report returned terminal HTTP {}",
                response.status
            )));
        }
        Ok(response)
    }

    fn start_reports(
        &self,
        state: &mut State,
        job: &mut GitHubDeployJob,
    ) -> Result<(), GitHubError> {
        if job.check_run_id.is_none() {
            let lookup = self.report_request(
                job,
                "GET",
                &format!(
                    "/repos/{}/{}/check-runs?external_id={}",
                    job.owner, job.name, job.id
                ),
                None,
            )?;
            let known = if lookup.status == 200 {
                serde_json::from_slice::<Value>(&lookup.body)
                    .ok()
                    .and_then(|v| {
                        v.get("check_runs")
                            .and_then(Value::as_array)
                            .and_then(|runs| {
                                runs.iter()
                                    .find(|run| {
                                        run.get("external_id").and_then(Value::as_str)
                                            == Some(job.id.as_str())
                                    })
                                    .and_then(|run| run.get("id").and_then(Value::as_i64))
                            })
                    })
            } else {
                None
            };
            let check_id = if known.is_some() {
                known
            } else {
                let body = serde_json::to_vec(
                    &json!({ "name": "Cygnus deploy", "head_sha": job.sha, "status": "in_progress", "external_id": job.id, "started_at": now_string() }),
                )?;
                let response = self.report_request(
                    job,
                    "POST",
                    &format!("/repos/{}/{}/check-runs", job.owner, job.name),
                    Some(&body),
                )?;
                if !(200..300).contains(&response.status) {
                    return Err(http_status("check start", response.status, &response.body));
                }
                serde_json::from_slice::<Value>(&response.body)
                    .ok()
                    .and_then(|value| value.get("id").and_then(Value::as_i64))
            };
            if let Some(id) = check_id {
                state.update_github_job_report(&job.id, Some(id), None)?;
                job.check_run_id = Some(id);
            }
        }
        if job.deployment_id.is_none() {
            let lookup = self.report_request(
                job,
                "GET",
                &format!(
                    "/repos/{}/{}/deployments?sha={}&environment={}",
                    job.owner, job.name, job.sha, job.environment
                ),
                None,
            )?;
            let known = if lookup.status == 200 {
                serde_json::from_slice::<Value>(&lookup.body)
                    .ok()
                    .and_then(|v| {
                        v.as_array().and_then(|items| {
                            items
                                .iter()
                                .find(|item| {
                                    item.pointer("/payload/cygnus_job_id")
                                        .and_then(Value::as_str)
                                        == Some(job.id.as_str())
                                })
                                .and_then(|item| item.get("id").and_then(Value::as_i64))
                        })
                    })
            } else {
                None
            };
            let deployment_id = if known.is_some() {
                known
            } else {
                let body = serde_json::to_vec(
                    &json!({ "ref": job.sha, "environment": job.environment, "production_environment": matches!(job.kind, GitHubJobKind::Production), "transient_environment": matches!(job.kind, GitHubJobKind::Preview), "auto_merge": false, "required_contexts": [], "payload": { "cygnus_job_id": job.id } }),
                )?;
                let response = self.report_request(
                    job,
                    "POST",
                    &format!("/repos/{}/{}/deployments", job.owner, job.name),
                    Some(&body),
                )?;
                if !(200..300).contains(&response.status) {
                    return Err(http_status(
                        "deployment start",
                        response.status,
                        &response.body,
                    ));
                }
                serde_json::from_slice::<Value>(&response.body)
                    .ok()
                    .and_then(|value| value.get("id").and_then(Value::as_i64))
            };
            if let Some(id) = deployment_id {
                state.update_github_job_report(&job.id, None, Some(id))?;
                job.deployment_id = Some(id);
            }
        }
        Ok(())
    }
    fn run_claimed(&self, state: &mut State, job: &GitHubDeployJob) -> Result<String, GitHubError> {
        let config = state
            .github_repository(job.installation_id, job.repository_id)?
            .ok_or_else(|| GitHubError::InvalidInput("repository mapping was removed".into()))?;
        let token = self
            .manager
            .installation_token(job.installation_id, Some(job.repository_id))?;
        let response = self.manager.transport.request_limited(
            "GET",
            &format!(
                "/repos/{}/{}/tarball/{}",
                config.owner, config.name, job.sha
            ),
            Some(&format!("Bearer {token}")),
            None,
            MAX_ARCHIVE_BODY_BYTES,
        )?;
        if !(200..300).contains(&response.status) {
            return Err(http_status(
                "archive fetch",
                response.status,
                &response.body,
            ));
        }
        let workspace = state.state_root().join("github-work").join(&job.id);
        if workspace.exists() {
            fs::remove_dir_all(&workspace)?;
        }
        fs::create_dir_all(&workspace)?;
        let result = (|| {
            safe_extract_archive(&response.body, &workspace)?;
            let audit = worker_audit(job);
            let deployment_id = self
                .executor
                .deploy(state, job, &config, &workspace, &audit)
                .map(|result| result.deployment_id)
                .map_err(|error| GitHubError::Deploy(error.to_string()))?;
            state.attach_deployment_id(&job.id, &deployment_id)?;
            Ok(deployment_id)
        })();
        let _ = fs::remove_dir_all(&workspace);
        result
    }

    fn finish_reports(
        &self,
        job: &GitHubDeployJob,
        success: bool,
        error: Option<&str>,
    ) -> Result<(), GitHubError> {
        let conclusion = if success { "success" } else { "failure" };
        if let Some(check_run_id) = job.check_run_id {
            let body = serde_json::to_vec(
                &json!({ "status": "completed", "conclusion": conclusion, "completed_at": now_string(), "output": { "title": "Cygnus deploy", "summary": error.unwrap_or("Deployment completed") } }),
            )?;
            let response = self.report_request(
                job,
                "PATCH",
                &format!(
                    "/repos/{}/{}/check-runs/{check_run_id}",
                    job.owner, job.name
                ),
                Some(&body),
            )?;
            if !(200..300).contains(&response.status) {
                return Err(http_status(
                    "check completion",
                    response.status,
                    &response.body,
                ));
            }
        }
        if let Some(deployment_id) = job.deployment_id {
            let body = serde_json::to_vec(
                &json!({ "state": if success { "success" } else { "failure" }, "description": error.unwrap_or("Cygnus deploy completed"), "auto_inactive": false }),
            )?;
            let response = self.report_request(
                job,
                "POST",
                &format!(
                    "/repos/{}/{}/deployments/{deployment_id}/statuses",
                    job.owner, job.name
                ),
                Some(&body),
            )?;
            if !(200..300).contains(&response.status) {
                return Err(http_status(
                    "deployment status",
                    response.status,
                    &response.body,
                ));
            }
        }
        Ok(())
    }
}

fn worker_audit(job: &GitHubDeployJob) -> AuditContext {
    let mut digest = Sha256::new();
    digest.update(job.id.as_bytes());
    let digest = format!("{:x}", digest.finalize());
    AuditContext {
        endpoint_role: crate::state::AuditEndpointRole::Host,
        peer_uid: None,
        peer_gid: None,
        peer_pid: Some(std::process::id()),
        actor_subject: Some("github:worker".into()),
        request_id: digest[..32].into(),
        command_kind: "github_deploy".into(),
        request_digest: digest,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    type FakeRequest = (String, String, Option<String>, Vec<u8>);

    struct FakeTransport {
        statuses: Mutex<Vec<u16>>,
        seen: Mutex<Vec<FakeRequest>>,
    }
    impl GitHubTransport for FakeTransport {
        fn request(
            &self,
            method: &str,
            path: &str,
            authorization: Option<&str>,
            body: Option<&[u8]>,
        ) -> Result<GitHubHttpResponse, GitHubError> {
            self.seen.lock().push((
                method.into(),
                path.into(),
                authorization.map(ToOwned::to_owned),
                body.unwrap_or_default().to_vec(),
            ));
            let status = self.statuses.lock().pop().unwrap_or(201);
            Ok(GitHubHttpResponse { status, headers: vec![], body: br#"{"id":1,"client_id":"Iv1.x","name":"Cygnus","client_secret":"secret","pem":"-----BEGIN RSA PRIVATE KEY-----\ninvalid\n-----END RSA PRIVATE KEY-----","webhook_secret":"hook"}"#.to_vec() })
        }
    }
    fn tmp() -> PathBuf {
        static NEXT_ID: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "cygnus-github-test-{}-{}",
            std::process::id(),
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn signature_is_verified_before_json_parse() {
        let path = tmp().join("state.db");
        let manager = GitHubManager::with_transport(
            &path,
            Arc::new(FakeTransport {
                statuses: Mutex::new(vec![]),
                seen: Mutex::new(vec![]),
            }),
        );
        let mut state = State::open(&path).unwrap();
        state
            .set_github_app(
                &GitHubAppRecord {
                    app_id: "1".into(),
                    client_id: "1".into(),
                    name: "x".into(),
                    html_url: None,
                    owner: None,
                    configured_at: "1".into(),
                },
                &GitHubAppSecrets {
                    client_secret: "c".into(),
                    pem: "p".into(),
                    webhook_secret: "s".into(),
                },
            )
            .unwrap();
        drop(state);
        let body = br#"not-json"#;
        let mut mac = Hmac::<Sha256>::new_from_slice(b"s").unwrap();
        mac.update(body);
        let signature = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));
        manager
            .webhook_begin("d".into(), "push".into(), signature, body.len() as u64)
            .unwrap();
        manager.webhook_chunk("d", &BASE64.encode(body)).unwrap();
        assert!(matches!(
            manager.webhook_finish(
                "d",
                &AuditContext {
                    endpoint_role: crate::state::AuditEndpointRole::Host,
                    peer_uid: None,
                    peer_gid: None,
                    peer_pid: Some(std::process::id()),
                    actor_subject: Some("test".into()),
                    request_id: "test-request".into(),
                    command_kind: "test".into(),
                    request_digest:
                        "0000000000000000000000000000000000000000000000000000000000000000".into()
                }
            ),
            Err(GitHubError::Json(_))
        ));
    }
    fn write_entry(builder: &mut tar::Builder<&mut Vec<u8>>, path: &str, body: &[u8], dir: bool) {
        let mut header = tar::Header::new_gnu();
        header.set_path(path).unwrap();
        header.set_entry_type(if dir {
            tar::EntryType::Directory
        } else {
            tar::EntryType::Regular
        });
        header.set_size(if dir { 0 } else { body.len() as u64 });
        header.set_mode(0o644);
        header.set_cksum();
        builder.append(&header, body).unwrap();
    }

    fn build_tar(entries: &[(&str, &[u8], bool)]) -> Vec<u8> {
        let mut bytes = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut bytes);
            for (path, body, dir) in entries {
                write_entry(&mut builder, path, body, *dir);
            }
            builder.finish().unwrap();
        }
        bytes
    }

    #[test]
    fn archive_strips_github_style_wrapper() {
        let dir = tmp();
        let bytes = build_tar(&[
            ("repo-sha", b"", true),
            ("repo-sha/README.md", b"hello", false),
            ("repo-sha/src/index.js", b"x", false),
        ]);
        safe_extract_archive(&bytes, &dir).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("README.md")).unwrap(),
            "hello"
        );
        assert_eq!(std::fs::read(dir.join("src/index.js")).unwrap(), b"x");
        assert!(!dir.join("repo-sha").exists());
    }

    #[test]
    fn archive_strips_github_wrapper_without_directory_entry() {
        let dir = tmp();
        let bytes = build_tar(&[
            (
                "owner-repo-9f0c6a7e1ea393fab43108a9ea8480a5800508d3/package.json",
                br#"{"scripts":{"build":"next build","start":"next start"}}"#,
                false,
            ),
            (
                "owner-repo-9f0c6a7e1ea393fab43108a9ea8480a5800508d3/app/page.tsx",
                b"export default function Page() {}",
                false,
            ),
        ]);
        safe_extract_archive(&bytes, &dir).unwrap();
        assert!(dir.join("package.json").is_file());
        assert!(dir.join("app/page.tsx").is_file());
        assert!(
            !dir.join("owner-repo-9f0c6a7e1ea393fab43108a9ea8480a5800508d3")
                .exists()
        );
    }

    #[test]
    fn archive_preserves_paths_when_no_wrapper() {
        let dir = tmp();
        let bytes = build_tar(&[("README.md", b"top", false), ("package.json", b"{}", false)]);
        safe_extract_archive(&bytes, &dir).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("README.md")).unwrap(),
            "top"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("package.json")).unwrap(),
            "{}"
        );
    }

    #[test]
    fn archive_preserves_paths_when_wrapper_does_not_cover_every_entry() {
        let dir = tmp();
        let bytes = build_tar(&[
            ("src", b"", true),
            ("README.md", b"top", false),
            ("package.json", b"{}", false),
            ("src/index.js", b"x", false),
            ("src/README.md", b"inner", false),
        ]);
        safe_extract_archive(&bytes, &dir).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("README.md")).unwrap(),
            "top"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("src/README.md")).unwrap(),
            "inner"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("package.json")).unwrap(),
            "{}"
        );
    }

    #[test]
    fn archive_rejects_traversal_and_symlinks() {
        let path = tmp().join("archive");
        let mut bytes = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut bytes);
            let mut header = tar::Header::new_gnu();
            header.set_path("link").unwrap();
            header.set_entry_type(tar::EntryType::symlink());
            header.set_link_name("../../escape").unwrap();
            header.set_size(0);
            header.set_cksum();
            builder.append(&header, io::empty()).unwrap();
            builder.finish().unwrap();
        }
        assert!(matches!(
            safe_extract_archive(&bytes, &path),
            Err(GitHubError::UnsafeArchive(_))
        ));
    }
}
