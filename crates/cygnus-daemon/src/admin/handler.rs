use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, Read, Seek, SeekFrom};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use super::{
    ActiveDeploymentView, AdminCommand, AdminData, AdminErrorCode, AdminHandler,
    AdminPeerCredentials, AdminRequest, AdminResponse, AdminRole, AppDomainView, AppView,
    CertificateView, DeploymentView, DomainDnsView, EngineView, GitHubJobView,
    MAX_ADMIN_LIST_LIMIT, MAX_LOG_CHUNK_BYTES, MemoryView, NodeView,
};
use crate::deploy::upload::{UploadError, UploadManager, UploadMetadata};
use crate::deploy::{
    DeployError, DeployRequest, canonical_source_root, new_deployment_id, resolve_deploy_request,
};
use crate::domains::{StdDnsResolver, dns_precheck, expected_public_ipv4};
use crate::edge::{CertificateRecord, SslMode};
use crate::github::{GitHubError, GitHubManager};
use crate::metrics::MetricsHub;
use crate::state::{
    AuditContext, AuditEndpointRole, AuditOutcome, DeployJob, DeployJobSource, DeployJobSpec,
    DeployJobStatus, DeploymentInput, DeploymentRecord, DeploymentSource, DeploymentStatus,
    DomainRecord, DomainStatus, DomainTls, GitHubJobKind, LoadedApp, NodeConfig, State, StateError,
};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use cygnus_cage::EgressMode;
use sha2::{Digest, Sha256};

const APP_PAGE_QUERY_LIMIT: usize = 50;
const DEFAULT_RUNTIME_LOGS_ROOT: &str = "/var/log/cygnus/apps";

type LifecycleLookup = dyn Fn(&str) -> Option<String> + Send + Sync;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AdminMutation {
    ApplyConfig(NodeConfig),
    RegisterEngine {
        version: String,
        host_root: std::path::PathBuf,
        cage_executable: std::path::PathBuf,
        is_default: bool,
    },
    Deploy(DeployRequest),
    SetDashboardDomain {
        domain: Option<String>,
        apex: Option<String>,
    },
    SetDashboardTls {
        mode: SslMode,
        email: Option<String>,
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
    MapDomain {
        app: String,
        domain: String,
    },
    SetPrimaryDomain {
        app: String,
        host: String,
    },
    RetryDomainAcme {
        app: String,
        host: String,
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
    Rollback {
        app: String,
        deployment: String,
        expected_active_artifact: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AdminMutationError {
    pub code: AdminErrorCode,
    pub message: String,
}

impl AdminMutationError {
    pub fn new(code: AdminErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

pub trait AdminMutationHandler: Send + Sync + 'static {
    fn execute(
        &self,
        mutation: AdminMutation,
        audit: &AuditContext,
    ) -> Result<AdminData, AdminMutationError>;
}

/// State-backed implementation of the read-only v1 admin command set.
pub struct StateAdminHandler {
    state_path: PathBuf,
    lifecycle: Arc<LifecycleLookup>,
    mutations: Arc<dyn AdminMutationHandler>,
    github: Option<Arc<GitHubManager>>,
    metrics: MetricsHub,
    started_at: Instant,
    runtime_logs_root: PathBuf,
    uploads: UploadManager,
}

impl StateAdminHandler {
    pub fn new(
        state_path: impl Into<PathBuf>,
        lifecycle: impl Fn(&str) -> Option<String> + Send + Sync + 'static,
        mutations: Arc<dyn AdminMutationHandler>,
    ) -> Self {
        let state_path = state_path.into();
        let state_root = state_path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let state_root = fs::canonicalize(state_root).unwrap_or_else(|_| state_root.to_path_buf());
        let uploads = UploadManager::new(&state_root)
            .unwrap_or_else(|error| panic!("could not initialize deployment uploads: {error}"));
        Self {
            state_path,
            lifecycle: Arc::new(lifecycle),
            mutations,
            github: None,
            metrics: MetricsHub::new(),
            started_at: Instant::now(),
            runtime_logs_root: PathBuf::from(DEFAULT_RUNTIME_LOGS_ROOT),
            uploads,
        }
    }

    pub fn with_github(mut self, github: Arc<GitHubManager>) -> Self {
        self.github = Some(github);
        self
    }

    /// Clone the handler-owned upload spool handle for the deployment worker.
    pub fn upload_manager(&self) -> UploadManager {
        self.uploads.clone()
    }

    pub fn with_metrics(mut self, metrics: MetricsHub) -> Self {
        self.metrics = metrics;
        self
    }

    pub fn with_runtime_logs(mut self, runtime_logs_root: impl Into<PathBuf>) -> Self {
        self.runtime_logs_root = runtime_logs_root.into();
        self
    }

    fn dispatch(
        &self,
        role: AdminRole,
        peer: AdminPeerCredentials,
        request: &AdminRequest,
    ) -> Result<AdminData, HandlerFault> {
        match request.command.clone() {
            AdminCommand::Health => Ok(AdminData::Health {
                service: "cygnus-daemon".into(),
                isolation: cygnus_cage::ISOLATION.into(),
            }),
            AdminCommand::AccountStatus => {
                let status = self
                    .open_state()?
                    .account_status()
                    .map_err(HandlerFault::internal)?;
                Ok(AdminData::AccountStatus {
                    configured: status.configured,
                })
            }
            AdminCommand::CreateInitialAccount { email, password } => {
                let audit = self.account_creation_audit(role, peer, request, &email)?;
                let mut state = self.open_state()?;
                match state.create_initial_account_with_audit(&email, &password, &audit) {
                    Ok(account) => Ok(AdminData::InitialAccountCreated {
                        subject: account.subject,
                    }),
                    Err(error) => {
                        let fault = map_account_state_error(error);
                        state
                            .append_audit(
                                &audit,
                                AuditOutcome::Failure,
                                Some(admin_error_name(fault.code)),
                            )
                            .map_err(HandlerFault::internal)?;
                        Err(fault)
                    }
                }
            }
            AdminCommand::ChangePassword {
                email,
                current_password,
                new_password,
            } => {
                let audit = self.request_audit(role, peer, request, "change_password")?;
                let mut state = self.open_state()?;
                match state.update_account_password(
                    &email,
                    &current_password,
                    &new_password,
                    &audit,
                ) {
                    Ok(()) => Ok(AdminData::PasswordChanged),
                    Err(error) => {
                        let fault = map_account_state_error(error);
                        state
                            .append_audit(
                                &audit,
                                AuditOutcome::Failure,
                                Some(admin_error_name(fault.code)),
                            )
                            .map_err(HandlerFault::internal)?;
                        Err(fault)
                    }
                }
            }
            AdminCommand::VerifyCredentials { email, password } => {
                let credentials = self
                    .open_state()?
                    .verify_credentials(&email, &password)
                    .map_err(map_account_state_error)?;
                Ok(AdminData::Credentials {
                    ok: credentials.ok,
                    subject: credentials.subject,
                })
            }
            AdminCommand::Status => self.status(),
            AdminCommand::SetDashboardDomain { domain, apex } => self.mutate(
                role,
                peer,
                request,
                AdminMutation::SetDashboardDomain { domain, apex },
                "set_dashboard_domain",
            ),
            AdminCommand::SetDashboardTls { mode, email } => self.mutate(
                role,
                peer,
                request,
                AdminMutation::SetDashboardTls { mode, email },
                "set_dashboard_tls",
            ),
            AdminCommand::ListAppDomains { app } => {
                let domains = self
                    .open_state()?
                    .app_domains(Some(&app))
                    .map_err(map_state_query_error)?;
                let public_ip = expected_public_ipv4();
                Ok(AdminData::AppDomains {
                    domains: domains
                        .into_iter()
                        .map(|domain| {
                            let expected = if crate::domains::is_local_host(&domain.host) {
                                Some(std::net::Ipv4Addr::new(127, 0, 0, 1))
                            } else {
                                public_ip
                            };
                            app_domain_view(domain, expected)
                        })
                        .collect(),
                })
            }
            AdminCommand::AddAppDomain { app, host } => self.mutate(
                role,
                peer,
                request,
                AdminMutation::AddAppDomain { app, host },
                "add_app_domain",
            ),
            AdminCommand::RemoveAppDomain { app, host } => self.mutate(
                role,
                peer,
                request,
                AdminMutation::RemoveAppDomain { app, host },
                "remove_app_domain",
            ),
            AdminCommand::SetAppDomainTls { app, host, mode } => self.mutate(
                role,
                peer,
                request,
                AdminMutation::SetAppDomainTls { app, host, mode },
                "set_app_domain_tls",
            ),
            AdminCommand::SetPrimaryDomain { app, host } => self.mutate(
                role,
                peer,
                request,
                AdminMutation::SetPrimaryDomain { app, host },
                "set_primary_domain",
            ),
            AdminCommand::RetryDomainAcme { app, host } => self.mutate(
                role,
                peer,
                request,
                AdminMutation::RetryDomainAcme { app, host },
                "retry_domain_acme",
            ),
            AdminCommand::ListEnvVars { app } => {
                let vars = self
                    .open_state()?
                    .app_env_vars(&app)
                    .map_err(map_state_query_error)?
                    .into_iter()
                    .map(|record| (record.key, record.value))
                    .collect();
                Ok(AdminData::EnvVars { vars })
            }
            AdminCommand::SetEnvVar { app, key, value } => self.mutate(
                role,
                peer,
                request,
                AdminMutation::SetEnvVar { app, key, value },
                "set_env_var",
            ),
            AdminCommand::RemoveEnvVar { app, key } => self.mutate(
                role,
                peer,
                request,
                AdminMutation::RemoveEnvVar { app, key },
                "remove_env_var",
            ),
            AdminCommand::GetMetrics => Ok(AdminData::Metrics {
                metrics: self.metrics.metrics(),
            }),
            AdminCommand::ListRequests { limit } => Ok(AdminData::Requests {
                requests: self.metrics.list_requests(usize::from(limit)),
            }),
            AdminCommand::ListEvents { limit } => Ok(AdminData::Events {
                events: self.metrics.list_events(usize::from(limit)),
            }),
            AdminCommand::ReadAppLog {
                app,
                stream,
                offset,
                limit,
            } => {
                let path = self
                    .runtime_logs_root
                    .join(&app)
                    .join(stream.app_filename());
                let (bytes, next_offset, eof) = read_log_chunk(&path, offset, limit, "app log")?;
                Ok(AdminData::AppLog {
                    app,
                    stream,
                    offset,
                    next_offset,
                    eof,
                    data_base64: BASE64_STANDARD.encode(bytes),
                })
            }
            AdminCommand::ListApps { cursor, limit } => {
                let state = self.open_state()?;
                let snapshot = state.load().map_err(HandlerFault::internal)?;
                let start = app_page_start(&snapshot.apps, cursor.as_deref())?;
                let limit = usize::from(limit).min(APP_PAGE_QUERY_LIMIT);
                let end = start.saturating_add(limit).min(snapshot.apps.len());
                let mut apps = Vec::with_capacity(end.saturating_sub(start));
                for app in &snapshot.apps[start..end] {
                    apps.push(self.app_view(&state, app)?);
                }
                let next_cursor =
                    (end < snapshot.apps.len()).then(|| snapshot.apps[end - 1].name.clone());
                Ok(AdminData::Apps { apps, next_cursor })
            }
            AdminCommand::GetApp { app } => {
                let state = self.open_state()?;
                let snapshot = state.load().map_err(HandlerFault::internal)?;
                let app = snapshot
                    .apps
                    .iter()
                    .find(|candidate| candidate.name == app)
                    .ok_or_else(|| HandlerFault::not_found("app does not exist"))?;
                Ok(AdminData::App {
                    app: self.app_view(&state, app)?,
                })
            }
            AdminCommand::ListDeployments { app, cursor, limit } => {
                let state = self.open_state()?;
                let query_limit = limit.saturating_add(1);
                let mut deployments = state
                    .deployments(app.as_deref(), cursor.as_deref(), query_limit)
                    .map_err(map_state_query_error)?;
                let has_more = deployments.len() > usize::from(limit);
                deployments.truncate(usize::from(limit));
                let next_cursor = has_more
                    .then(|| deployments.last().map(|deployment| deployment.id.clone()))
                    .flatten();
                Ok(AdminData::Deployments {
                    deployments: deployments
                        .into_iter()
                        .map(|deployment| deployment_view(deployment, role))
                        .collect(),
                    next_cursor,
                })
            }
            AdminCommand::GetDeployment { deployment } => {
                let state = self.open_state()?;
                if let Some(deployment) = state
                    .deployment(&deployment)
                    .map_err(HandlerFault::internal)?
                {
                    return Ok(AdminData::Deployment {
                        deployment: deployment_view(deployment, role),
                    });
                }
                // Fall back to a prefix match against recent deployments. The
                // `cygnus deployments` table only renders the first 12 chars of
                // the id, so the obvious copy/paste comes back here as a
                // short prefix rather than the full hex string. Exact match
                // above handles the 23-char case unambiguously.
                let candidates = state
                    .deployments(None, None, 50)
                    .map_err(HandlerFault::internal)?
                    .into_iter()
                    .filter(|record| record.id.starts_with(&deployment))
                    .map(|record| record.id)
                    .collect::<Vec<_>>();
                match candidates.len() {
                    0 => Err(HandlerFault::not_found("deployment does not exist")),
                    1 => {
                        let only = candidates.into_iter().next().unwrap();
                        let record = state
                            .deployment(&only)
                            .map_err(HandlerFault::internal)?
                            .ok_or_else(|| HandlerFault::not_found("deployment does not exist"))?;
                        Ok(AdminData::Deployment {
                            deployment: deployment_view(record, role),
                        })
                    }
                    n => Err(HandlerFault::validation(format!(
                        "deployment {deployment:?} is ambiguous ({n} matches); pass a longer prefix or the full id"
                    ))),
                }
            }
            AdminCommand::ApplyConfig(config) => self.mutate(
                role,
                peer,
                request,
                AdminMutation::ApplyConfig(config),
                "apply_config",
            ),
            AdminCommand::RegisterEngine {
                version,
                host_root,
                cage_executable,
                is_default,
            } => self.mutate(
                role,
                peer,
                request,
                AdminMutation::RegisterEngine {
                    version,
                    host_root,
                    cage_executable,
                    is_default,
                },
                "register_engine",
            ),
            AdminCommand::Deploy {
                request: mut deployment,
            } => {
                deployment.source = DeploymentSource::cli();
                deployment.deployment_id = None;
                self.mutate(
                    role,
                    peer,
                    request,
                    AdminMutation::Deploy(deployment),
                    "deploy",
                )
            }
            AdminCommand::DeployUploadBegin {
                app,
                domain,
                engine_version,
                entry,
                env,
                preview,
                total_bytes,
            } => {
                let upload_id = self
                    .uploads
                    .begin(
                        UploadMetadata {
                            app,
                            domain,
                            engine_version,
                            entry,
                            env,
                            preview,
                        },
                        total_bytes,
                    )
                    .map_err(upload_fault)?;
                Ok(AdminData::DeployUploadBegun { upload_id })
            }
            AdminCommand::DeployUploadChunk {
                upload_id,
                chunk_base64,
            } => {
                let received_bytes = self
                    .uploads
                    .append_next(&upload_id, &chunk_base64)
                    .map_err(upload_fault)?;
                Ok(AdminData::DeployUploadChunked {
                    upload_id,
                    received_bytes,
                })
            }
            AdminCommand::DeployUploadFinish { upload_id } => self.finish_upload(&upload_id),
            AdminCommand::DeployStart {
                request: mut deployment,
            } => {
                deployment.source = DeploymentSource::cli();
                deployment.deployment_id = None;
                self.start_deploy(deployment)
            }
            AdminCommand::MapDomain { app, domain } => self.mutate(
                role,
                peer,
                request,
                AdminMutation::MapDomain { app, domain },
                "map_domain",
            ),
            AdminCommand::Rollback {
                app,
                deployment,
                expected_active_artifact,
            } => self.mutate(
                role,
                peer,
                request,
                AdminMutation::Rollback {
                    app,
                    deployment,
                    expected_active_artifact,
                },
                "rollback",
            ),
            AdminCommand::ConvertManifest { code, owner } => {
                let github = self.github_manager()?;
                let audit = self.request_audit(role, peer, request, "convert_manifest")?;
                let app = github
                    .manifest_conversion(&code, owner.as_deref(), &audit)
                    .map_err(github_fault)?;
                Ok(AdminData::ManifestConverted { app })
            }
            AdminCommand::GitHubStatus => {
                let github = self.github_manager()?;
                let app = github.app_status().map_err(github_fault)?;
                Ok(AdminData::GitHubStatus {
                    configured: app.is_some(),
                    app,
                })
            }
            AdminCommand::ListRepositories { limit } => {
                let github = self.github_manager()?;
                let mut repositories = github.repositories().map_err(github_fault)?;
                repositories.truncate(usize::from(limit));
                Ok(AdminData::Repositories { repositories })
            }
            AdminCommand::ListInstallationRepositories { installation_id } => {
                let github = self.github_manager()?;
                let mut repositories = github
                    .installation_repositories(installation_id)
                    .map_err(github_fault)?;
                repositories.truncate(usize::from(MAX_ADMIN_LIST_LIMIT));
                Ok(AdminData::InstallationRepositories { repositories })
            }
            AdminCommand::ConfigureRepository { repository } => {
                let github = self.github_manager()?;
                let audit = self.request_audit(role, peer, request, "configure_repository")?;
                let repository = github
                    .configure_repository(repository, &audit)
                    .map_err(github_fault)?;
                Ok(AdminData::RepositoryConfigured { repository })
            }
            AdminCommand::WebhookBegin {
                delivery_id,
                event,
                signature,
                total_bytes,
            } => {
                let github = self.github_manager()?;
                let duplicate = !github
                    .webhook_begin(delivery_id.clone(), event, signature, total_bytes)
                    .map_err(github_fault)?;
                Ok(AdminData::WebhookBegun {
                    delivery_id,
                    duplicate,
                })
            }
            AdminCommand::WebhookChunk {
                delivery_id,
                chunk_base64,
            } => {
                let github = self.github_manager()?;
                let received_bytes = github
                    .webhook_chunk(&delivery_id, &chunk_base64)
                    .map_err(github_fault)?;
                Ok(AdminData::WebhookChunked {
                    delivery_id,
                    received_bytes,
                })
            }
            AdminCommand::WebhookFinish { delivery_id } => {
                let github = self.github_manager()?;
                let audit = self.request_audit(role, peer, request, "webhook_finish")?;
                let result = github
                    .webhook_finish(&delivery_id, &audit)
                    .map_err(github_fault)?;
                Ok(AdminData::WebhookAccepted {
                    delivery_id: result.delivery_id,
                    duplicate: result.duplicate,
                    jobs: result.jobs,
                })
            }
            AdminCommand::ListDeployJobs { cursor, limit } => {
                let state = self.open_state()?;
                let jobs = state
                    .deploy_jobs(limit, cursor.as_deref())
                    .map_err(map_state_query_error)?;
                let next_cursor = (jobs.len() == usize::from(limit))
                    .then(|| jobs.last().map(|job| job.id.clone()))
                    .flatten();
                Ok(AdminData::DeployJobs {
                    jobs: jobs.into_iter().map(job_view).collect(),
                    next_cursor,
                })
            }
            AdminCommand::RetryDeployJob { job_id } => {
                let audit = self.request_audit(role, peer, request, "retry_deploy_job")?;
                let mut state = self.open_state()?;
                let job = state
                    .retry_deploy_job_with_audit(&job_id, &audit)
                    .map_err(map_state_query_error)?;
                Ok(AdminData::DeployJobRetried {
                    job: Box::new(job_view(job)),
                })
            }
            AdminCommand::ReadLog {
                deployment,
                stream,
                offset,
                limit,
            } => {
                let state = self.open_state()?;
                let directory = state
                    .deployment_logs_dir(&deployment)
                    .map_err(HandlerFault::internal)?
                    .ok_or_else(|| HandlerFault::not_found("deployment logs are unavailable"))?;
                let path = directory.join(stream.build_filename());
                let (bytes, next_offset, eof) = read_log_chunk(&path, offset, limit, "build log")?;
                Ok(AdminData::Log {
                    deployment,
                    stream,
                    offset,
                    next_offset,
                    eof,
                    data_base64: BASE64_STANDARD.encode(bytes),
                })
            }
        }
    }

    fn finish_upload(&self, upload_id: &str) -> Result<AdminData, HandlerFault> {
        if !valid_upload_id(upload_id) {
            return Err(HandlerFault::validation("upload id is invalid"));
        }
        let job_id = format!("upload-{upload_id}");
        let mut state = self.open_state()?;
        if let Some(job) = state.deploy_job(&job_id).map_err(map_state_query_error)? {
            return finished_upload_data(job);
        }
        let upload = self.uploads.finish(upload_id).map_err(upload_fault)?;

        let request = DeployRequest {
            source_dir: upload.archive_path.clone(),
            app: upload.metadata.app,
            domain: upload.metadata.domain,
            engine_version: upload.metadata.engine_version,
            entry: upload.metadata.entry,
            artifact_root: None,
            upstream: None,
            env: upload.metadata.env,
            preview: upload.metadata.preview.clone(),
            deployment_id: None,
            source: DeploymentSource::upload(),
        };
        let target = resolve_deploy_request(&state, request).map_err(deploy_fault)?;
        let queued_entry = if target.entry_explicit {
            target.entry.clone()
        } else {
            PathBuf::new()
        };
        let deployment_id = new_deployment_id();
        let deployment = DeploymentInput {
            id: deployment_id.clone(),
            app: target.app.clone(),
            source_hash: upload.digest.clone(),
            engine_version: target.engine_version.clone(),
            source: DeploymentSource::upload(),
        };
        let job = DeployJobSpec {
            id: job_id.clone(),
            key: upload_id.to_owned(),
            source: DeployJobSource::Upload,
            source_path: upload.archive_path,
            source_ref: upload.digest,
            app: target.app,
            domain: target.domain,
            engine_version: target.engine_version,
            entry: queued_entry,
            artifact_root: target.artifact_root,
            upstream: target.upstream,
            branch: None,
            commit: None,
            installation_id: None,
            repository_id: None,
            owner: None,
            name: None,
            environment: None,
            kind: None,
            pull_request: None,
        };
        state
            .enqueue_preassigned_deployment(&deployment, &job)
            .map_err(map_state_query_error)?;
        let job = state
            .deploy_job(&job_id)
            .map_err(map_state_query_error)?
            .ok_or_else(|| HandlerFault::internal("queued upload job disappeared"))?;
        finished_upload_data(job)
    }

    fn start_deploy(&self, request: DeployRequest) -> Result<AdminData, HandlerFault> {
        let mut state = self.open_state()?;
        let mut target = resolve_deploy_request(&state, request).map_err(deploy_fault)?;
        target.source_dir = canonical_source_root(&target.source_dir).map_err(deploy_fault)?;
        let queued_entry = if target.entry_explicit {
            target.entry.clone()
        } else {
            PathBuf::new()
        };
        let deployment_id = new_deployment_id();
        let job_id = new_deployment_id();
        let mut hasher = Sha256::new();
        hasher.update(target.source_dir.as_os_str().as_bytes());
        hasher.update([0]);
        hasher.update(job_id.as_bytes());
        let source_ref = format!("{:x}", hasher.finalize());
        let deployment = DeploymentInput {
            id: deployment_id.clone(),
            app: target.app.clone(),
            source_hash: source_ref.clone(),
            engine_version: target.engine_version.clone(),
            source: DeploymentSource::cli(),
        };
        let job = DeployJobSpec {
            id: job_id.clone(),
            key: target.app.clone(),
            source: DeployJobSource::Cli,
            source_path: target.source_dir,
            source_ref,
            app: target.app,
            domain: target.domain,
            engine_version: target.engine_version,
            entry: queued_entry,
            artifact_root: target.artifact_root,
            upstream: target.upstream,
            branch: None,
            commit: None,
            installation_id: None,
            repository_id: None,
            owner: None,
            name: None,
            environment: None,
            kind: None,
            pull_request: None,
        };
        state
            .enqueue_preassigned_deployment(&deployment, &job)
            .map_err(map_state_query_error)?;
        Ok(AdminData::DeployStarted {
            deployment_id,
            job_id,
        })
    }

    fn status(&self) -> Result<AdminData, HandlerFault> {
        let state = self.open_state()?;
        let snapshot = state.load().map_err(HandlerFault::internal)?;
        let engines = state
            .engines()
            .map_err(HandlerFault::internal)?
            .into_iter()
            .map(|status| EngineView {
                version: status.engine.version,
                sha256: status.engine.sha256,
                is_default: status.engine.is_default,
                apps: status.app_count,
            })
            .collect();
        let now = unix_seconds();
        let acme = snapshot.edge.acme.as_ref();
        let mut certificates = Vec::new();
        for certificate in state.certificates().map_err(HandlerFault::internal)? {
            let ok = certificate.not_after_unix > now
                && certificate.certificate_path.is_file()
                && certificate.private_key_path.is_file();
            let is_fallback = certificate_is_self_signed_fallback(&certificate);
            for domain in certificate.domains {
                let kind = if is_fallback {
                    "self_signed".into()
                } else if domain.starts_with("*.") {
                    "wildcard".into()
                } else if acme.is_some_and(|config| config.dns_provider.is_some()) {
                    "acme_dns01".into()
                } else if acme.is_some() {
                    "acme_http01".into()
                } else {
                    "manual".into()
                };
                certificates.push(CertificateView {
                    domain,
                    kind,
                    ok,
                    expires_unix: Some(certificate.not_after_unix),
                });
            }
        }
        let warm_count = snapshot
            .apps
            .iter()
            .filter(|app| (self.lifecycle)(&app.name).as_deref() == Some("ready"))
            .count();
        Ok(AdminData::Status {
            node: NodeView {
                listen: snapshot.listen.to_string(),
                https_listen: snapshot.edge.https_listen.map(|value| value.to_string()),
                apps_domain: snapshot.edge.apps_domain,
                dashboard_domain: snapshot.edge.dashboard_domain,
                apex_domain: snapshot.edge.apex_domain,
                ssl_mode: snapshot.edge.ssl_mode,
                acme_email: acme.map(|config| config.email.clone()),
                app_count: snapshot.apps.len(),
                version: env!("CARGO_PKG_VERSION").into(),
                uptime_seconds: self.started_at.elapsed().as_secs(),
                isolation: cygnus_cage::ISOLATION.into(),
                warm_count,
                engines,
                certificates,
                memory: memory_view(),
            },
        })
    }

    fn mutate(
        &self,
        role: AdminRole,
        peer: AdminPeerCredentials,
        request: &AdminRequest,
        mutation: AdminMutation,
        command_kind: &str,
    ) -> Result<AdminData, HandlerFault> {
        let encoded = serde_json::to_vec(request).map_err(HandlerFault::internal)?;
        let audit = AuditContext {
            endpoint_role: match role {
                AdminRole::Host => AuditEndpointRole::Host,
                AdminRole::TenantZero => AuditEndpointRole::TenantZero,
            },
            peer_uid: peer.uid,
            peer_gid: peer.gid,
            peer_pid: peer.pid,
            actor_subject: request.actor.clone(),
            request_id: request.request_id.clone(),
            command_kind: command_kind.into(),
            request_digest: format!("{:x}", Sha256::digest(encoded)),
        };
        match self.mutations.execute(mutation, &audit) {
            Ok(data) => Ok(data),
            Err(error) => {
                let mut state = self.open_state()?;
                state
                    .append_audit(
                        &audit,
                        AuditOutcome::Failure,
                        Some(admin_error_name(error.code)),
                    )
                    .map_err(HandlerFault::internal)?;
                Err(HandlerFault {
                    code: error.code,
                    message: redact_public_error(&error.message),
                })
            }
        }
    }

    fn github_manager(&self) -> Result<&GitHubManager, HandlerFault> {
        self.github
            .as_deref()
            .ok_or_else(|| HandlerFault::internal("GitHub integration is unavailable"))
    }

    fn account_creation_audit(
        &self,
        role: AdminRole,
        peer: AdminPeerCredentials,
        request: &AdminRequest,
        email: &str,
    ) -> Result<AuditContext, HandlerFault> {
        let digest_input = format!("create_initial_account\0{}", email.trim().to_lowercase());
        Ok(AuditContext {
            endpoint_role: match role {
                AdminRole::Host => AuditEndpointRole::Host,
                AdminRole::TenantZero => AuditEndpointRole::TenantZero,
            },
            peer_uid: peer.uid,
            peer_gid: peer.gid,
            peer_pid: peer.pid,
            actor_subject: None,
            request_id: request.request_id.clone(),
            command_kind: "create_initial_account".into(),
            request_digest: format!("{:x}", Sha256::digest(digest_input)),
        })
    }

    fn request_audit(
        &self,
        role: AdminRole,
        peer: AdminPeerCredentials,
        request: &AdminRequest,
        command_kind: &str,
    ) -> Result<AuditContext, HandlerFault> {
        let encoded = serde_json::to_vec(request).map_err(HandlerFault::internal)?;
        Ok(AuditContext {
            endpoint_role: match role {
                AdminRole::Host => AuditEndpointRole::Host,
                AdminRole::TenantZero => AuditEndpointRole::TenantZero,
            },
            peer_uid: peer.uid,
            peer_gid: peer.gid,
            peer_pid: peer.pid,
            actor_subject: request.actor.clone(),
            request_id: request.request_id.clone(),
            command_kind: command_kind.into(),
            request_digest: format!("{:x}", Sha256::digest(encoded)),
        })
    }

    fn open_state(&self) -> Result<State, HandlerFault> {
        State::open(&self.state_path).map_err(HandlerFault::internal)
    }

    fn app_view(&self, state: &State, app: &LoadedApp) -> Result<AppView, HandlerFault> {
        let idle_ttl_ms = u64::try_from(app.lifecycle.idle_ttl.as_millis())
            .map_err(|_| HandlerFault::internal("idle TTL does not fit protocol"))?;
        let mut env_keys = app
            .spec
            .env
            .keys()
            .map(|key| key.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        env_keys.sort();
        let active = state
            .active_deployment(&app.name)
            .map_err(HandlerFault::internal)?
            .map(|active| ActiveDeploymentView {
                deployment_id: active.deployment_id,
                artifact_hash: active.artifact_hash,
                engine_version: active.engine_version,
            });
        Ok(AppView {
            name: app.name.clone(),
            domains: app.domains.clone(),
            lifecycle_state: (self.lifecycle)(&app.name).unwrap_or_else(|| "unknown".into()),
            pinned: app.lifecycle.min_instances >= 1,
            idle_ttl_ms,
            egress: egress_name(&app.spec.egress).into(),
            memory_max: app.spec.limits.memory_max,
            env_keys,
            active,
        })
    }
}

fn app_domain_view(domain: DomainRecord, expected_ip: Option<std::net::Ipv4Addr>) -> AppDomainView {
    let dns = dns_precheck(&StdDnsResolver, &domain.host, expected_ip);
    // Local / already-resolving hosts should not look "pending" forever when
    // the cert path is self-signed or a fallback is already serving traffic.
    // Surface a usable status so operators aren't told to edit public DNS for
    // apps.localhost.
    let status = match domain.status {
        DomainStatus::Pending | DomainStatus::Issuing
            if dns.ok
                && (domain.tls == DomainTls::SelfSigned
                    || crate::domains::is_local_host(&domain.host)) =>
        {
            DomainStatus::FallbackActive
        }
        other => other,
    };
    AppDomainView {
        host: domain.host,
        kind: domain.kind,
        tls: domain.tls,
        status,
        dns: DomainDnsView {
            expected_ip: dns.expected_ip.map(|ip| ip.to_string()),
            resolves_to: dns
                .resolves_to
                .into_iter()
                .map(|ip| ip.to_string())
                .collect(),
            ok: dns.ok,
        },
        expires_unix: domain.expires_unix,
        error: domain.error,
        next_retry_unix: domain.next_retry_unix,
        is_primary: domain.is_primary,
    }
}

fn valid_upload_id(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn finished_upload_data(job: DeployJob) -> Result<AdminData, HandlerFault> {
    let deployment_id = job
        .deployment_id
        .ok_or_else(|| HandlerFault::internal("upload job has no local deployment id"))?;
    Ok(AdminData::DeployUploadFinished {
        deployment_id,
        job_id: job.id,
    })
}

fn deploy_fault(error: DeployError) -> HandlerFault {
    match error {
        DeployError::InvalidInput(message) => HandlerFault::validation(message),
        DeployError::State(StateError::InvalidRecord { detail, .. }) => {
            HandlerFault::validation(detail)
        }
        error => HandlerFault::internal(error),
    }
}

fn upload_fault(error: UploadError) -> HandlerFault {
    match error {
        UploadError::InvalidInput(message) => HandlerFault::validation(message),
        UploadError::NotFound => HandlerFault::not_found("upload session does not exist"),
        error @ (UploadError::Capacity | UploadError::OutOfOrder { .. }) => HandlerFault {
            code: AdminErrorCode::Conflict,
            message: error.to_string(),
        },
        error @ (UploadError::Overflow | UploadError::Incomplete { .. }) => {
            HandlerFault::validation(error.to_string())
        }
        error @ UploadError::Io(_) => HandlerFault::internal(error),
    }
}

fn admin_error_name(code: AdminErrorCode) -> &'static str {
    match code {
        AdminErrorCode::InvalidRequest => "invalid_request",
        AdminErrorCode::UnsupportedVersion => "unsupported_version",
        AdminErrorCode::Unauthorized => "unauthorized",
        AdminErrorCode::NotFound => "not_found",
        AdminErrorCode::Conflict => "conflict",
        AdminErrorCode::Validation => "validation",
        AdminErrorCode::Internal => "internal",
    }
}

impl AdminHandler for StateAdminHandler {
    fn handle(
        &self,
        role: AdminRole,
        peer: AdminPeerCredentials,
        request: AdminRequest,
    ) -> AdminResponse {
        let request_id = request.request_id.clone();
        match self.dispatch(role, peer, &request) {
            Ok(data) => AdminResponse::ok(request_id, data),
            Err(error) => AdminResponse::error(request_id, error.code, error.message),
        }
    }
}

fn app_page_start(apps: &[LoadedApp], cursor: Option<&str>) -> Result<usize, HandlerFault> {
    let Some(cursor) = cursor else {
        return Ok(0);
    };
    apps.iter()
        .position(|app| app.name == cursor)
        .map(|position| position + 1)
        .ok_or_else(|| HandlerFault::not_found("app cursor does not exist"))
}
fn job_view(job: DeployJob) -> GitHubJobView {
    GitHubJobView {
        id: bounded_text(job.id, 128),
        key: bounded_text(job.key, 128),
        source: job.source,
        source_ref: bounded_text(job.source_ref, 128),
        app: bounded_text(job.app, 128),
        installation_id: job.installation_id,
        repository_id: job.repository_id,
        owner: job.owner.map(|value| bounded_text(value, 128)),
        name: job.name.map(|value| bounded_text(value, 128)),
        environment: job.environment.map(|value| bounded_text(value, 128)),
        kind: job.kind.map(|kind| {
            match kind {
                GitHubJobKind::Production => "production",
                GitHubJobKind::Preview => "preview",
            }
            .into()
        }),
        pull_request: job.pull_request,
        sha: job.commit.map(|value| bounded_text(value, 128)),
        status: match job.status {
            DeployJobStatus::Queued => "queued",
            DeployJobStatus::Running => "running",
            DeployJobStatus::Succeeded => "succeeded",
            DeployJobStatus::Failed => "failed",
            DeployJobStatus::Retry => "retry",
            DeployJobStatus::Cancelled => "cancelled",
        }
        .into(),
        attempts: job.attempts,
        next_attempt_at: bounded_text(job.next_attempt_at, 64),
        error: job.error.as_deref().map(redact_public_error),
        check_run_id: job.check_run_id,
        github_deployment_id: job.github_deployment_id,
        deployment_id: job.deployment_id,
        created_at: bounded_text(job.created_at, 64),
        updated_at: bounded_text(job.updated_at, 64),
    }
}

fn bounded_text(value: String, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value;
    }
    let mut end = max_bytes;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

fn github_fault(error: GitHubError) -> HandlerFault {
    let code = match &error {
        GitHubError::InvalidInput(_)
        | GitHubError::Json(_)
        | GitHubError::UnsafeArchive(_)
        | GitHubError::IncompleteWebhook
        | GitHubError::InvalidSignature => AdminErrorCode::Validation,
        GitHubError::Transient(_) => AdminErrorCode::Conflict,
        GitHubError::State(StateError::InvalidRecord { .. }) => AdminErrorCode::Validation,
        GitHubError::State(StateError::AppNotFound(_)) => AdminErrorCode::NotFound,
        _ => AdminErrorCode::Internal,
    };
    HandlerFault {
        code,
        message: redact_public_error(&error.to_string()),
    }
}

fn redact_public_error(value: &str) -> String {
    let lower = value.to_ascii_lowercase();
    let sensitive = lower.contains("secret")
        || lower.contains("private key")
        || lower.contains("begin ")
        || lower.contains("artifact_root")
        || lower.contains("upstream")
        || value.contains('/')
        || value.contains('\\')
        || value.chars().any(char::is_control);
    if sensitive {
        return "GitHub operation failed".into();
    }
    bounded_text(value.to_owned(), 512)
}

fn deployment_view(deployment: DeploymentRecord, role: AdminRole) -> DeploymentView {
    DeploymentView {
        id: deployment.id,
        app: deployment.app,
        source_hash: deployment.source_hash,
        engine_version: deployment.engine_version,
        created_ms: deployment.created_ms,
        updated_ms: deployment.updated_ms,
        source: deployment.source,
        artifact_hash: deployment.artifact_hash,
        status: deployment_status_name(deployment.status).into(),
        error: matches!(role, AdminRole::Host)
            .then_some(deployment.error)
            .flatten(),
    }
}

fn deployment_status_name(status: DeploymentStatus) -> &'static str {
    match status {
        DeploymentStatus::Building => "building",
        DeploymentStatus::Failed => "failed",
        DeploymentStatus::Sealed => "sealed",
        DeploymentStatus::Active => "active",
    }
}

fn egress_name(egress: &EgressMode) -> &'static str {
    match egress {
        EgressMode::None => "none",
        EgressMode::Restricted { .. } => "restricted",
        EgressMode::BuildDomains { .. } => "build_domains",
        EgressMode::Public => "public",
        EgressMode::Open => "open",
    }
}

fn read_log_chunk(
    path: &Path,
    offset: u64,
    limit: u32,
    label: &str,
) -> Result<(Vec<u8>, u64, bool), HandlerFault> {
    if limit == 0 || limit > MAX_LOG_CHUNK_BYTES {
        return Err(HandlerFault::validation(
            "log limit is outside the supported range",
        ));
    }
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .map_err(|error| match error.kind() {
            io::ErrorKind::NotFound => HandlerFault::not_found(format!("{label} does not exist")),
            _ => HandlerFault::internal(error),
        })?;
    let metadata = file.metadata().map_err(HandlerFault::internal)?;
    if !metadata.is_file() {
        return Err(HandlerFault::internal(format!(
            "{label} is not a regular file"
        )));
    }
    if offset > metadata.len() {
        return Err(HandlerFault::validation(
            "log offset exceeds the file length",
        ));
    }
    file.seek(SeekFrom::Start(offset))
        .map_err(HandlerFault::internal)?;
    let available = metadata.len() - offset;
    let count = usize::try_from(available.min(u64::from(limit)))
        .map_err(|_| HandlerFault::internal("log chunk size does not fit memory"))?;
    let mut bytes = vec![0_u8; count];
    file.read_exact(&mut bytes)
        .map_err(HandlerFault::internal)?;
    let next_offset = offset + count as u64;
    Ok((bytes, next_offset, next_offset == metadata.len()))
}

fn certificate_is_self_signed_fallback(certificate: &CertificateRecord) -> bool {
    let Ok(file) = File::open(&certificate.certificate_path) else {
        return false;
    };
    let mut reader = BufReader::new(file);
    let Some(Ok(der)) = rustls_pemfile::certs(&mut reader).next() else {
        return false;
    };
    let Ok((_, parsed)) = x509_parser::parse_x509_certificate(der.as_ref()) else {
        return false;
    };
    parsed.subject().iter_common_name().any(|name| {
        name.as_str()
            .is_ok_and(|value| value == "Cygnus self-signed fallback")
    })
}

fn unix_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_secs()).ok())
        .unwrap_or(0)
}

#[cfg(target_os = "linux")]
fn memory_view() -> MemoryView {
    let Ok(contents) = std::fs::read_to_string("/proc/meminfo") else {
        return MemoryView::default();
    };
    let mut total_bytes = 0;
    let mut available_bytes = 0;
    for line in contents.lines() {
        let mut fields = line.split_ascii_whitespace();
        let Some(name) = fields.next() else {
            continue;
        };
        let Some(value) = fields.next().and_then(|value| value.parse::<u64>().ok()) else {
            continue;
        };
        match name {
            "MemTotal:" => total_bytes = value.saturating_mul(1024),
            "MemAvailable:" => available_bytes = value.saturating_mul(1024),
            _ => {}
        }
    }
    MemoryView {
        total_bytes,
        available_bytes,
    }
}

#[cfg(not(target_os = "linux"))]
fn memory_view() -> MemoryView {
    MemoryView::default()
}

fn map_state_query_error(error: crate::state::StateError) -> HandlerFault {
    match &error {
        crate::state::StateError::InvalidRecord { kind, .. } if *kind == "deployment cursor" => {
            HandlerFault::not_found("deployment cursor does not exist")
        }
        crate::state::StateError::AppNotFound(_) => HandlerFault::not_found("app does not exist"),
        crate::state::StateError::DomainNotFound(_) => {
            HandlerFault::not_found("domain does not exist")
        }
        _ => HandlerFault::internal(error),
    }
}

fn map_account_state_error(error: StateError) -> HandlerFault {
    match error {
        StateError::InvalidAccountInput(message) => HandlerFault::validation(message),
        error @ (StateError::DuplicateAccountEmail(_) | StateError::AccountAlreadyConfigured) => {
            HandlerFault::conflict(error.to_string())
        }
        error => HandlerFault::internal(error),
    }
}

struct HandlerFault {
    code: AdminErrorCode,
    message: String,
}

impl HandlerFault {
    fn conflict(message: impl Into<String>) -> Self {
        Self {
            code: AdminErrorCode::Conflict,
            message: message.into(),
        }
    }

    fn not_found(message: impl Into<String>) -> Self {
        Self {
            code: AdminErrorCode::NotFound,
            message: message.into(),
        }
    }

    fn validation(message: impl Into<String>) -> Self {
        Self {
            code: AdminErrorCode::Validation,
            message: message.into(),
        }
    }

    fn internal(_error: impl std::fmt::Display) -> Self {
        Self {
            code: AdminErrorCode::Internal,
            message: "admin operation failed".into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{AppConfig, NodeConfig};
    use std::net::SocketAddr;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct UnusedMutations;

    impl AdminMutationHandler for UnusedMutations {
        fn execute(
            &self,
            _mutation: AdminMutation,
            _audit: &AuditContext,
        ) -> Result<AdminData, AdminMutationError> {
            panic!("read-only handler test invoked a mutation")
        }
    }

    fn unused_mutations() -> Arc<dyn AdminMutationHandler> {
        Arc::new(UnusedMutations)
    }

    struct FailingMutations;

    impl AdminMutationHandler for FailingMutations {
        fn execute(
            &self,
            _mutation: AdminMutation,
            _audit: &AuditContext,
        ) -> Result<AdminData, AdminMutationError> {
            Err(AdminMutationError::new(
                AdminErrorCode::Conflict,
                "state changed",
            ))
        }
    }

    static NEXT_STATE_PATH: AtomicU64 = AtomicU64::new(1);

    fn state_path() -> PathBuf {
        let nonce = NEXT_STATE_PATH.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "cygnus-admin-handler-{}-{nonce}.db",
            std::process::id()
        ))
    }

    fn request(command: AdminCommand) -> AdminRequest {
        AdminRequest {
            version: super::super::ADMIN_PROTOCOL_VERSION,
            request_id: "0123456789abcdef0123456789abcdef".into(),
            actor: None,
            command,
        }
    }

    fn state_with_engine(label: &str) -> (PathBuf, PathBuf) {
        let root = state_path().with_extension(label);
        fs::create_dir_all(&root).unwrap();
        let path = root.join("state.db");
        let engine_root = root.join("engine");
        fs::create_dir_all(engine_root.join("bin")).unwrap();
        let executable = engine_root.join("bin/bun");
        fs::write(&executable, b"bun").unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let mut state = State::open(&path).unwrap();
        state
            .register_engine(&crate::state::EngineRecord {
                version: "bun".into(),
                host_root: fs::canonicalize(&engine_root).unwrap(),
                cage_executable: "/bin/bun".into(),
                sha256: format!("{:x}", Sha256::digest(b"bun")),
                is_default: true,
            })
            .unwrap();
        (root, path)
    }

    #[test]
    fn account_handlers_create_verify_and_audit_without_exposing_hashes() {
        let root = state_path().with_extension("account-auth");
        fs::create_dir_all(&root).unwrap();
        let path = root.join("state.db");
        State::open(&path).unwrap();
        let handler = StateAdminHandler::new(&path, |_| None, unused_mutations());

        let status = handler.handle(
            AdminRole::TenantZero,
            AdminPeerCredentials::default(),
            request(AdminCommand::AccountStatus),
        );
        let AdminResponse::Ok { data, .. } = &status else {
            panic!("unexpected account status response");
        };
        assert_eq!(
            data.as_ref(),
            &AdminData::AccountStatus { configured: false }
        );

        let created = handler.handle(
            AdminRole::TenantZero,
            AdminPeerCredentials {
                uid: Some(1000),
                gid: Some(1000),
                pid: Some(42),
            },
            request(AdminCommand::CreateInitialAccount {
                email: " Admin@Example.COM ".into(),
                password: "correct horse battery staple".into(),
            }),
        );
        let AdminResponse::Ok { data, .. } = &created else {
            panic!("unexpected account creation response");
        };
        assert_eq!(
            data.as_ref(),
            &AdminData::InitialAccountCreated {
                subject: "account:1".into(),
            }
        );

        for (password, expected_ok) in [
            ("correct horse battery staple", true),
            ("incorrect password value", false),
        ] {
            let verified = handler.handle(
                AdminRole::TenantZero,
                AdminPeerCredentials::default(),
                request(AdminCommand::VerifyCredentials {
                    email: "ADMIN@example.com".into(),
                    password: password.into(),
                }),
            );
            let AdminResponse::Ok { data, .. } = verified else {
                panic!("unexpected credential verification response");
            };
            assert_eq!(
                *data,
                AdminData::Credentials {
                    ok: expected_ok,
                    subject: expected_ok.then(|| "account:1".into()),
                }
            );
        }

        let duplicate = handler.handle(
            AdminRole::TenantZero,
            AdminPeerCredentials::default(),
            request(AdminCommand::CreateInitialAccount {
                email: "admin@example.com".into(),
                password: "another strong password".into(),
            }),
        );
        assert!(matches!(
            duplicate,
            AdminResponse::Error { error, .. } if error.code == AdminErrorCode::Conflict
        ));

        let state = State::open(&path).unwrap();
        let audits = state.audit_records().unwrap();
        assert_eq!(audits.len(), 2);
        assert_eq!(audits[0].command_kind, "create_initial_account");
        assert_eq!(audits[0].outcome, AuditOutcome::Success);
        assert_eq!(audits[0].endpoint_role, AuditEndpointRole::TenantZero);
        assert_eq!(audits[0].peer_uid, Some(1000));
        assert_eq!(audits[0].actor_subject, None);
        assert_eq!(audits[1].outcome, AuditOutcome::Failure);
        assert_eq!(audits[1].error_code.as_deref(), Some("conflict"));
        let serialized = serde_json::to_string(&created).unwrap();
        assert!(!serialized.contains("password_hash"));
        assert!(!serialized.contains("$argon2"));
        drop(state);
        drop(handler);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn account_handler_maps_invalid_inputs_to_validation() {
        let root = state_path().with_extension("account-validation");
        fs::create_dir_all(&root).unwrap();
        let path = root.join("state.db");
        State::open(&path).unwrap();
        let handler = StateAdminHandler::new(&path, |_| None, unused_mutations());
        let response = handler.handle(
            AdminRole::TenantZero,
            AdminPeerCredentials::default(),
            request(AdminCommand::CreateInitialAccount {
                email: "bad".into(),
                password: "correct horse battery staple".into(),
            }),
        );
        assert!(matches!(
            response,
            AdminResponse::Error { error, .. } if error.code == AdminErrorCode::Validation
        ));
        drop(handler);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn upload_handler_appends_implicitly_and_finish_is_deduplicated() {
        let (root, path) = state_with_engine("upload-dedupe");
        let handler = StateAdminHandler::new(&path, |_| None, unused_mutations());
        let response = handler.handle(
            AdminRole::TenantZero,
            AdminPeerCredentials::default(),
            request(AdminCommand::DeployUploadBegin {
                app: "hello".into(),
                domain: Some("hello.example".into()),
                engine_version: None,
                entry: None,
                env: Default::default(),
                preview: None,
                total_bytes: 4,
            }),
        );
        let AdminResponse::Ok { data, .. } = response else {
            panic!("unexpected begin response");
        };
        let AdminData::DeployUploadBegun { upload_id } = *data else {
            panic!("unexpected begin data");
        };

        let chunk = |bytes: &[u8]| {
            let response = handler.handle(
                AdminRole::TenantZero,
                AdminPeerCredentials::default(),
                request(AdminCommand::DeployUploadChunk {
                    upload_id: upload_id.clone(),
                    chunk_base64: BASE64_STANDARD.encode(bytes),
                }),
            );
            let AdminResponse::Ok { data, .. } = response else {
                panic!("unexpected chunk response");
            };
            let AdminData::DeployUploadChunked { received_bytes, .. } = *data else {
                panic!("unexpected chunk data");
            };
            received_bytes
        };
        assert_eq!(chunk(b"ab"), 2);
        assert_eq!(chunk(b"cd"), 4);

        let finish = || {
            let response = handler.handle(
                AdminRole::TenantZero,
                AdminPeerCredentials::default(),
                request(AdminCommand::DeployUploadFinish {
                    upload_id: upload_id.clone(),
                }),
            );
            let AdminResponse::Ok { data, .. } = response else {
                panic!("unexpected finish response");
            };
            let AdminData::DeployUploadFinished {
                deployment_id,
                job_id,
            } = *data
            else {
                panic!("unexpected finish data");
            };
            (deployment_id, job_id)
        };
        let first = finish();
        let second = finish();
        assert_eq!(first, second);

        let state = State::open(&path).unwrap();
        assert_eq!(state.deploy_jobs(10, None).unwrap().len(), 1);
        assert_eq!(state.deployments(None, None, 10).unwrap().len(), 1);
        let job = state.deploy_job(&first.1).unwrap().unwrap();
        assert_eq!(job.deployment_id.as_deref(), Some(first.0.as_str()));
        assert_eq!(job.status, DeployJobStatus::Queued);
        assert_eq!(job.source, DeployJobSource::Upload);
        drop(state);
        let listed = handler.handle(
            AdminRole::TenantZero,
            AdminPeerCredentials::default(),
            request(AdminCommand::ListDeployJobs {
                cursor: None,
                limit: 10,
            }),
        );
        let AdminResponse::Ok { data, .. } = listed else {
            panic!("unexpected list response");
        };
        let AdminData::DeployJobs { jobs, .. } = *data else {
            panic!("unexpected list data");
        };
        assert_eq!(jobs[0].source, DeployJobSource::Upload);
        assert_eq!(jobs[0].deployment_id.as_deref(), Some(first.0.as_str()));
        assert!(jobs[0].installation_id.is_none());
        assert!(jobs[0].github_deployment_id.is_none());
        drop(handler);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn deploy_start_precreates_cli_deployment_and_queued_job() {
        let (root, path) = state_with_engine("deploy-start");
        let source = root.join("source");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("index.ts"), b"export default 1").unwrap();
        let handler = StateAdminHandler::new(&path, |_| None, unused_mutations());
        let response = handler.handle(
            AdminRole::Host,
            AdminPeerCredentials::default(),
            request(AdminCommand::DeployStart {
                request: DeployRequest {
                    source_dir: source.clone(),
                    app: "hello".into(),
                    domain: Some("hello.example".into()),
                    engine_version: None,
                    entry: None,
                    artifact_root: None,
                    upstream: None,
                    env: Default::default(),
                    preview: None,
                    deployment_id: None,
                    source: DeploymentSource::upload(),
                },
            }),
        );
        let AdminResponse::Ok { data, .. } = response else {
            panic!("unexpected start response");
        };
        let AdminData::DeployStarted {
            deployment_id,
            job_id,
        } = *data
        else {
            panic!("unexpected start data");
        };
        let state = State::open(&path).unwrap();
        let deployment = state.deployment(&deployment_id).unwrap().unwrap();
        let job = state.deploy_job(&job_id).unwrap().unwrap();
        assert_eq!(deployment.source, DeploymentSource::cli());
        assert_eq!(deployment.status, DeploymentStatus::Building);
        assert_eq!(job.source, DeployJobSource::Cli);
        assert_eq!(job.status, DeployJobStatus::Queued);
        assert_eq!(job.deployment_id.as_deref(), Some(deployment_id.as_str()));
        assert_eq!(job.source_path, fs::canonicalize(source).unwrap());
        assert!(job.entry.as_os_str().is_empty());
        drop(state);
        drop(handler);
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn get_deployment_resolves_a_unique_prefix() {
        let (root, path) = state_with_engine("get-deploy-prefix");
        let source = root.join("source");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("index.ts"), b"export default 1").unwrap();
        let handler = StateAdminHandler::new(&path, |_| None, unused_mutations());
        let started = handler.handle(
            AdminRole::Host,
            AdminPeerCredentials::default(),
            request(AdminCommand::DeployStart {
                request: DeployRequest {
                    source_dir: source,
                    app: "hello".into(),
                    domain: Some("hello.example".into()),
                    engine_version: None,
                    entry: None,
                    artifact_root: None,
                    upstream: None,
                    env: Default::default(),
                    preview: None,
                    deployment_id: None,
                    source: DeploymentSource::cli(),
                },
            }),
        );
        let started_data = match started {
            AdminResponse::Ok { data, .. } => *data,
            other => panic!("unexpected start: {other:?}"),
        };
        let deployment_id = match started_data {
            AdminData::DeployStarted { deployment_id, .. } => deployment_id,
            other => panic!("unexpected start data: {other:?}"),
        };
        let short = deployment_id[..12].to_owned();
        let resolved = handler.handle(
            AdminRole::Host,
            AdminPeerCredentials::default(),
            request(AdminCommand::GetDeployment { deployment: short }),
        );
        let resolved_data = match resolved {
            AdminResponse::Ok { data, .. } => *data,
            other => panic!("unexpected get: {other:?}"),
        };
        let deployment = match resolved_data {
            AdminData::Deployment { deployment } => deployment,
            other => panic!("unexpected get data: {other:?}"),
        };
        assert_eq!(deployment.id, deployment_id);
        let missing = handler.handle(
            AdminRole::Host,
            AdminPeerCredentials::default(),
            request(AdminCommand::GetDeployment {
                deployment: "ffffffff".into(),
            }),
        );
        assert!(matches!(
            missing,
            AdminResponse::Error { error, .. } if error.code == AdminErrorCode::NotFound
        ));
        drop(handler);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn status_includes_runtime_and_host_basics() {
        let path = state_path();
        let mut state = State::open(&path).unwrap();
        state
            .apply(&NodeConfig {
                listen: SocketAddr::from(([127, 0, 0, 1], 3000)),
                edge: Default::default(),
                apps: vec![AppConfig {
                    name: "api".into(),
                    upstream: "/run/cygnus/api.sock".into(),
                    command: "/bin/true".into(),
                    ..AppConfig::default()
                }],
            })
            .unwrap();
        drop(state);

        let handler = StateAdminHandler::new(
            &path,
            |app| (app == "api").then(|| "ready".into()),
            unused_mutations(),
        );
        let response = handler.handle(
            AdminRole::Host,
            AdminPeerCredentials::default(),
            request(AdminCommand::Status),
        );
        let AdminResponse::Ok { data, .. } = response else {
            panic!("unexpected response");
        };
        let AdminData::Status { node } = *data else {
            panic!("unexpected data kind");
        };
        assert_eq!(node.version, env!("CARGO_PKG_VERSION"));
        assert_eq!(node.isolation, cygnus_cage::ISOLATION);
        assert_eq!(node.app_count, 1);
        assert_eq!(node.warm_count, 1);
        assert!(node.engines.is_empty());
        assert!(node.certificates.is_empty());
        #[cfg(target_os = "linux")]
        assert!(node.memory.total_bytes > 0);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn read_app_log_honors_offset_limit_and_eof() {
        let path = state_path();
        State::open(&path).unwrap();
        let logs = path.with_extension("runtime-logs");
        fs::create_dir_all(logs.join("api")).unwrap();
        fs::write(logs.join("api/stdout.log"), b"abcdef").unwrap();
        let handler =
            StateAdminHandler::new(&path, |_| None, unused_mutations()).with_runtime_logs(&logs);

        let read = |offset, limit| {
            let response = handler.handle(
                AdminRole::Host,
                AdminPeerCredentials::default(),
                request(AdminCommand::ReadAppLog {
                    app: "api".into(),
                    stream: super::super::LogStream::Stdout,
                    offset,
                    limit,
                }),
            );
            let AdminResponse::Ok { data, .. } = response else {
                panic!("unexpected response");
            };
            let AdminData::AppLog {
                offset,
                next_offset,
                eof,
                data_base64,
                ..
            } = *data
            else {
                panic!("unexpected data kind");
            };
            (
                offset,
                next_offset,
                eof,
                BASE64_STANDARD.decode(data_base64).unwrap(),
            )
        };
        assert_eq!(read(2, 3), (2, 5, false, b"cde".to_vec()));
        assert_eq!(read(5, 3), (5, 6, true, b"f".to_vec()));

        fs::remove_dir_all(logs).unwrap();
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn state_snapshot_exposes_configuration_without_environment_values() {
        let path = state_path();
        let mut state = State::open(&path).unwrap();
        let mut app = AppConfig {
            name: "api".into(),
            domains: vec!["api.example.test".into()],
            upstream: "/run/cygnus/api.sock".into(),
            command: "/bin/true".into(),
            ..AppConfig::default()
        };
        app.env.insert("SECRET_NAME".into(), "must-not-leak".into());
        state
            .apply(&NodeConfig {
                listen: SocketAddr::from(([127, 0, 0, 1], 3000)),
                edge: Default::default(),
                apps: vec![app],
            })
            .unwrap();
        drop(state);

        let handler = StateAdminHandler::new(
            &path,
            |name| (name == "api").then(|| "cold".into()),
            unused_mutations(),
        );
        let response = handler.handle(
            AdminRole::TenantZero,
            AdminPeerCredentials::default(),
            request(AdminCommand::ListApps {
                cursor: None,
                limit: 50,
            }),
        );
        let AdminResponse::Ok { data, .. } = response else {
            panic!("unexpected response");
        };
        let AdminData::Apps { apps, .. } = *data else {
            panic!("unexpected data kind");
        };
        assert_eq!(apps.len(), 1);
        assert_eq!(apps[0].env_keys, ["SECRET_NAME"]);
        let encoded = serde_json::to_string(&apps[0]).unwrap();
        assert!(!encoded.contains("must-not-leak"));
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn app_cursor_and_missing_objects_return_typed_faults() {
        let path = state_path();
        State::open(&path).unwrap();
        let handler = StateAdminHandler::new(&path, |_| None, unused_mutations());
        let response = handler.handle(
            AdminRole::Host,
            AdminPeerCredentials::default(),
            request(AdminCommand::GetApp {
                app: "missing".into(),
            }),
        );
        assert!(matches!(
            response,
            AdminResponse::Error {
                error: super::super::AdminFault {
                    code: AdminErrorCode::NotFound,
                    ..
                },
                ..
            }
        ));
        fs::remove_file(path).unwrap();
    }
    #[test]
    fn github_error_and_job_paths_are_bounded_and_redacted() {
        let fault = github_fault(GitHubError::InvalidInput(
            "private key at /srv/daemon/key.pem".into(),
        ));
        assert_eq!(fault.message, "GitHub operation failed");
        let long = "x".repeat(2048);
        assert_eq!(redact_public_error(&long).len(), 512);
        assert!(redact_public_error(&"é".repeat(1024)).len() <= 512);
        assert!(!redact_public_error("upstream /run/secret.sock").contains("secret.sock"));
    }

    #[test]
    fn mutation_failure_records_endpoint_peer_actor_and_digest() {
        let path = state_path();
        State::open(&path).unwrap();
        let handler = StateAdminHandler::new(&path, |_| None, Arc::new(FailingMutations));
        let mut mutation = request(AdminCommand::MapDomain {
            app: "api".into(),
            domain: "api.example".into(),
        });
        mutation.actor = Some("github:alice".into());
        let response = handler.handle(
            AdminRole::TenantZero,
            AdminPeerCredentials {
                uid: Some(1000),
                gid: Some(1001),
                pid: Some(42),
            },
            mutation,
        );
        assert!(matches!(
            response,
            AdminResponse::Error {
                error: super::super::AdminFault {
                    code: AdminErrorCode::Conflict,
                    ..
                },
                ..
            }
        ));
        let state = State::open(&path).unwrap();
        let records = state.audit_records().unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].endpoint_role, AuditEndpointRole::TenantZero);
        assert_eq!(records[0].peer_uid, Some(1000));
        assert_eq!(records[0].peer_gid, Some(1001));
        assert_eq!(records[0].peer_pid, Some(42));
        assert_eq!(records[0].actor_subject.as_deref(), Some("github:alice"));
        assert_eq!(records[0].outcome, AuditOutcome::Failure);
        assert_eq!(records[0].error_code.as_deref(), Some("conflict"));
        assert_eq!(records[0].request_digest.len(), 64);
        drop(state);
        fs::remove_file(path).unwrap();
    }
}
