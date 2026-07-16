use std::fs::OpenOptions;
use std::io::{self, Read, Seek, SeekFrom};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::{
    ActiveDeploymentView, AdminCommand, AdminData, AdminErrorCode, AdminHandler,
    AdminPeerCredentials, AdminRequest, AdminResponse, AdminRole, AppView, DeploymentView,
    MAX_LOG_CHUNK_BYTES, NodeView,
};
use crate::deploy::DeployRequest;
use crate::state::{
    AuditContext, AuditEndpointRole, AuditOutcome, DeploymentRecord, DeploymentStatus, LoadedApp,
    NodeConfig, State,
};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use cygnus_cage::EgressMode;
use sha2::{Digest, Sha256};

const APP_PAGE_QUERY_LIMIT: usize = 50;

type LifecycleLookup = dyn Fn(&str) -> Option<String> + Send + Sync;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AdminMutation {
    ApplyConfig(NodeConfig),
    RegisterEngine {
        version: String,
        host_root: std::path::PathBuf,
        cage_executable: std::path::PathBuf,
    },
    Deploy(DeployRequest),
    MapDomain {
        app: String,
        domain: String,
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
}

impl StateAdminHandler {
    pub fn new(
        state_path: impl Into<PathBuf>,
        lifecycle: impl Fn(&str) -> Option<String> + Send + Sync + 'static,
        mutations: Arc<dyn AdminMutationHandler>,
    ) -> Self {
        Self {
            state_path: state_path.into(),
            lifecycle: Arc::new(lifecycle),
            mutations,
        }
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
            AdminCommand::Status => {
                let state = self.open_state()?;
                let snapshot = state.load().map_err(HandlerFault::internal)?;
                Ok(AdminData::Status {
                    node: NodeView {
                        listen: snapshot.listen.to_string(),
                        https_listen: snapshot.edge.https_listen.map(|value| value.to_string()),
                        apps_domain: snapshot.edge.apps_domain,
                        app_count: snapshot.apps.len(),
                    },
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
                let deployment = state
                    .deployment(&deployment)
                    .map_err(HandlerFault::internal)?
                    .ok_or_else(|| HandlerFault::not_found("deployment does not exist"))?;
                Ok(AdminData::Deployment {
                    deployment: deployment_view(deployment, role),
                })
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
            } => self.mutate(
                role,
                peer,
                request,
                AdminMutation::RegisterEngine {
                    version,
                    host_root,
                    cage_executable,
                },
                "register_engine",
            ),
            AdminCommand::Deploy {
                request: deployment,
            } => self.mutate(
                role,
                peer,
                request,
                AdminMutation::Deploy(deployment),
                "deploy",
            ),
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
                let path = directory.join(stream.filename());
                let (bytes, next_offset, eof) = read_log_chunk(&path, offset, limit)?;
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
                    message: error.message,
                })
            }
        }
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

fn deployment_view(deployment: DeploymentRecord, role: AdminRole) -> DeploymentView {
    DeploymentView {
        id: deployment.id,
        app: deployment.app,
        source_hash: deployment.source_hash,
        engine_version: deployment.engine_version,
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
            io::ErrorKind::NotFound => HandlerFault::not_found("build log does not exist"),
            _ => HandlerFault::internal(error),
        })?;
    let metadata = file.metadata().map_err(HandlerFault::internal)?;
    if !metadata.is_file() {
        return Err(HandlerFault::internal("build log is not a regular file"));
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

fn map_state_query_error(error: crate::state::StateError) -> HandlerFault {
    match &error {
        crate::state::StateError::InvalidRecord { kind, .. } if *kind == "deployment cursor" => {
            HandlerFault::not_found("deployment cursor does not exist")
        }
        _ => HandlerFault::internal(error),
    }
}

struct HandlerFault {
    code: AdminErrorCode,
    message: String,
}

impl HandlerFault {
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
    use std::fs;
    use std::net::SocketAddr;
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
        let AdminResponse::Ok {
            data: AdminData::Apps { apps, .. },
            ..
        } = response
        else {
            panic!("unexpected response");
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
