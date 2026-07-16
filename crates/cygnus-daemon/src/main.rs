use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fs;
use std::io;
use std::net::{SocketAddr, TcpListener};
use std::os::unix::fs::FileTypeExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use clap::{Parser, Subcommand};
use cygnus_cage::{ADMIN_CAGE_DIR, ADMIN_SOCKET_FILENAME, AdminSocketSpec, Cage, CageSpec};
use cygnus_daemon::Frontend;
use cygnus_daemon::acme::{AcmeManager, CloudflareDnsProvider, Dns01Provider, Http01Challenges};
use cygnus_daemon::admin::{
    ActiveDeploymentView, AdminBinding, AdminData, AdminErrorCode, AdminHandler, AdminMutation,
    AdminMutationError, AdminMutationHandler, AdminRole, AdminServer, DEFAULT_HOST_ADMIN_SOCKET,
    DEFAULT_TENANT_ADMIN_SOCKET, StateAdminHandler,
};
use cygnus_daemon::deploy::{
    ActivationPreparation, DeployError, DeployRequest, DeployResult, deploy_with_audit_and_prepare,
    register_engine_with_audit,
};
use cygnus_daemon::edge::EdgeConfig;
use cygnus_daemon::github::{GitHubDeployExecutor, GitHubManager, GitHubWorker};
use cygnus_daemon::state::{
    AuditContext, AuditEndpointRole, DEFAULT_STATE_PATH, LoadedApp, NodeConfig, Snapshot, State,
    StateError,
};
use cygnus_daemon::tls::TlsServer;
use cygnus_router::{Route, RouteTable, Router};
use cygnus_supervisor::{Instance, LifecycleState, Supervisor};
use sha2::{Digest, Sha256};
use signal_hook::consts::{SIGINT, SIGTERM};

const REAPER_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Debug, Parser)]
#[command(name = "cygnus-daemon", about = "Run the Cygnus request plane")]
struct Cli {
    /// SQLite state database.
    #[arg(long, global = true, default_value = DEFAULT_STATE_PATH)]
    state: PathBuf,

    /// Daemon-owned endpoint mounted read-only into Tenant Zero.
    #[arg(long, global = true, default_value = DEFAULT_TENANT_ADMIN_SOCKET)]
    tenant_admin_socket: PathBuf,
    /// Root-only local administration socket.
    #[arg(long, global = true, default_value = DEFAULT_HOST_ADMIN_SOCKET)]
    admin_socket: PathBuf,

    #[command(subcommand)]
    command: Option<Command>,
}
#[derive(Debug, Subcommand)]
enum Command {
    /// Serve the apps and domains currently stored in the state database.
    Serve {
        #[arg(long)]
        initial_config: Option<PathBuf>,
    },
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("cygnus-daemon: {error}");
            ExitCode::FAILURE
        }
    }
}
fn run(cli: Cli) -> Result<(), Box<dyn Error>> {
    match cli.command.unwrap_or(Command::Serve {
        initial_config: None,
    }) {
        Command::Serve { initial_config } => {
            if let Some(path) = initial_config {
                apply_initial_config(&cli.state, &path)?;
            }
            serve(&cli.state, &cli.admin_socket, &cli.tenant_admin_socket)
        }
    }
}

fn apply_initial_config(state_path: &Path, config_path: &Path) -> Result<bool, Box<dyn Error>> {
    if state_path.exists() {
        return Ok(false);
    }
    let bytes = fs::read(config_path)?;
    let config: NodeConfig = serde_json::from_slice(&bytes)?;
    let digest = format!("{:x}", Sha256::digest(&bytes));
    let audit = AuditContext {
        endpoint_role: AuditEndpointRole::Host,
        peer_uid: Some(unsafe { libc::geteuid() }),
        peer_gid: Some(unsafe { libc::getegid() }),
        peer_pid: Some(std::process::id()),
        actor_subject: Some("system:initial-config".into()),
        request_id: digest[..32].to_owned(),
        command_kind: "initial_config".into(),
        request_digest: digest,
    };
    let result = (|| -> Result<(), StateError> {
        let mut state = State::open(state_path)?;
        state.apply_with_audit(&config, &audit)
    })();
    if let Err(error) = result {
        remove_sqlite_files(state_path);
        return Err(Box::new(error));
    }
    Ok(true)
}

fn remove_sqlite_files(path: &Path) {
    let _ = fs::remove_file(path);
    for suffix in ["-wal", "-shm"] {
        let mut sidecar = path.as_os_str().to_os_string();
        sidecar.push(suffix);
        let _ = fs::remove_file(PathBuf::from(sidecar));
    }
}

struct LiveDeployRuntime<I> {
    supervisor: Arc<Supervisor<I>>,
    router: Arc<Router>,
    tenant_admin_socket: PathBuf,
    gate: Arc<parking_lot::Mutex<()>>,
}

impl<I: Instance + 'static> LiveDeployRuntime<I> {
    fn new(
        supervisor: Arc<Supervisor<I>>,
        router: Arc<Router>,
        tenant_admin_socket: impl Into<PathBuf>,
    ) -> Self {
        Self {
            supervisor,
            router,
            tenant_admin_socket: tenant_admin_socket.into(),
            gate: Arc::new(parking_lot::Mutex::new(())),
        }
    }

    fn prepare_candidate(
        &self,
        candidate: &LoadedApp,
        previous_runtime_key: Option<&str>,
    ) -> Result<ActivationPreparation, DeployError> {
        let key = candidate.spec.name.clone();
        if previous_runtime_key == Some(key.as_str()) {
            return Ok(ActivationPreparation::new(|| {}));
        }
        self.supervisor.register(
            key.clone(),
            candidate.spec.clone(),
            candidate.lifecycle.clone(),
        );
        match self.supervisor.acquire(&key) {
            Ok(()) => {
                let cleanup = Arc::clone(&self.supervisor);
                Ok(ActivationPreparation::new(move || {
                    let _ = cleanup.remove(&key);
                }))
            }
            Err(error) => {
                let _ = self.supervisor.remove(&key);
                Err(DeployError::ActivationFailed {
                    id: key,
                    detail: format!("{error:?}"),
                })
            }
        }
    }

    fn install_after_commit(
        &self,
        snapshot: &Snapshot,
        previous_runtime_key: Option<String>,
        new_runtime_key: &str,
    ) {
        if previous_runtime_key.as_deref() == Some(new_runtime_key) {
            return;
        }
        let retired = self.router.install(route_table(snapshot));
        if let Some(previous) = previous_runtime_key {
            retire_runtime_after_quiescence(retired, Arc::clone(&self.supervisor), previous);
        }
    }

    fn deploy(
        &self,
        state: &mut State,
        request: DeployRequest,
        audit: &AuditContext,
    ) -> Result<DeployResult, DeployError> {
        let _deployment_gate = self.gate.lock();
        let previous_runtime_key = state
            .load()?
            .apps
            .into_iter()
            .find(|app| app.name == request.app)
            .map(|app| app.spec.name);
        let previous_for_prepare = previous_runtime_key.clone();
        let tenant_socket = self.tenant_admin_socket.clone();
        let result = deploy_with_audit_and_prepare(state, request, audit, |candidate| {
            let mut candidate = candidate.clone();
            configure_tenant_admin(&mut candidate, &tenant_socket)
                .map_err(|error| DeployError::InvalidInput(error.to_string()))?;
            self.prepare_candidate(&candidate, previous_for_prepare.as_deref())
        })?;

        let new_runtime_key = format!("r-{}", result.artifact_hash);
        self.install_after_commit(&state.load()?, previous_runtime_key, &new_runtime_key);
        Ok(result)
    }
}

struct ProductionGitHubDeployExecutor {
    runtime: Arc<LiveDeployRuntime<Cage>>,
}

impl ProductionGitHubDeployExecutor {
    fn new(runtime: Arc<LiveDeployRuntime<Cage>>) -> Self {
        Self { runtime }
    }
}

impl GitHubDeployExecutor for ProductionGitHubDeployExecutor {
    fn deploy(
        &self,
        state: &mut State,
        _job: &cygnus_daemon::state::GitHubDeployJob,
        config: &cygnus_daemon::state::GitHubRepositoryConfig,
        source: &Path,
        audit: &AuditContext,
    ) -> Result<DeployResult, DeployError> {
        self.runtime.deploy(
            state,
            DeployRequest::new(
                source,
                &config.app,
                &config.domain,
                &config.engine_version,
                &config.entry,
                &config.artifact_root,
                &config.upstream,
            ),
            audit,
        )
    }
}

fn retire_runtime_after_quiescence<I: Instance + 'static>(
    retired: Arc<RouteTable>,
    supervisor: Arc<Supervisor<I>>,
    previous: String,
) {
    thread::spawn(move || {
        while !retired.is_quiescent() {
            thread::sleep(Duration::from_millis(10));
        }
        if let Err(error) = supervisor.remove(&previous) {
            eprintln!("cygnus-daemon: retired runtime {previous:?} did not stop: {error}");
        }
    });
}

struct LiveAdminMutations {
    state_path: PathBuf,
    supervisor: Arc<Supervisor<Cage>>,
    router: Arc<Router>,
    tenant_admin_socket: PathBuf,
    runtime: Arc<LiveDeployRuntime<Cage>>,
}

impl AdminMutationHandler for LiveAdminMutations {
    fn execute(
        &self,
        mutation: AdminMutation,
        audit: &AuditContext,
    ) -> Result<AdminData, AdminMutationError> {
        match mutation {
            AdminMutation::ApplyConfig(config) => self.apply_config(&config, audit),
            AdminMutation::RegisterEngine {
                version,
                host_root,
                cage_executable,
            } => self.register_engine(version, host_root, cage_executable, audit),
            AdminMutation::Deploy(request) => self.deploy(request, audit),
            AdminMutation::MapDomain { app, domain } => self.map_domain(&app, &domain, audit),
            AdminMutation::Rollback {
                app,
                deployment,
                expected_active_artifact,
            } => self.rollback(&app, &deployment, &expected_active_artifact, audit),
        }
    }
}
impl LiveAdminMutations {
    fn apply_config(
        &self,
        config: &NodeConfig,
        audit: &AuditContext,
    ) -> Result<AdminData, AdminMutationError> {
        let mut state = State::open(&self.state_path).map_err(map_admin_state_error)?;
        let current = state.load().map_err(map_admin_state_error)?;
        let desired = state.preview(config).map_err(map_admin_state_error)?;
        if current.listen != desired.listen || current.edge != desired.edge {
            return Err(AdminMutationError::new(
                AdminErrorCode::Conflict,
                "listener and public-edge changes require daemon restart",
            ));
        }

        let current_apps = current
            .apps
            .iter()
            .map(|app| (app.name.as_str(), app))
            .collect::<BTreeMap<_, _>>();
        let desired_apps = desired
            .apps
            .iter()
            .map(|app| (app.name.as_str(), app))
            .collect::<BTreeMap<_, _>>();
        for (name, candidate) in &desired_apps {
            if let Some(existing) = current_apps.get(name)
                && (existing.tenant_admin != candidate.tenant_admin
                    || existing.upstream != candidate.upstream
                    || existing.spec != candidate.spec
                    || existing.lifecycle != candidate.lifecycle)
            {
                return Err(AdminMutationError::new(
                    AdminErrorCode::Conflict,
                    format!(
                        "runtime changes for app {name:?} require a deployment or daemon restart"
                    ),
                ));
            }
        }

        let mut added = Vec::new();
        for (name, candidate) in &desired_apps {
            if current_apps.contains_key(name) {
                continue;
            }
            let mut candidate = (*candidate).clone();
            configure_tenant_admin(&mut candidate, &self.tenant_admin_socket).map_err(|error| {
                AdminMutationError::new(AdminErrorCode::Conflict, error.to_string())
            })?;
            let key = candidate.spec.name.clone();
            self.supervisor.register(
                key.clone(),
                candidate.spec.clone(),
                candidate.lifecycle.clone(),
            );
            added.push(key.clone());
            if candidate.lifecycle.min_instances > 0
                && let Err(error) = self.supervisor.acquire(&key)
            {
                for added_key in added.drain(..) {
                    let _ = self.supervisor.remove(&added_key);
                }
                return Err(AdminMutationError::new(
                    AdminErrorCode::Conflict,
                    format!("pinned app {name:?} did not become ready: {error:?}"),
                ));
            }
        }

        if let Err(error) = state.apply_with_audit(config, audit) {
            for key in added {
                let _ = self.supervisor.remove(&key);
            }
            return Err(map_admin_state_error(error));
        }
        let retired = self.router.install(route_table(&desired));
        let removed = current_apps
            .keys()
            .filter(|name| !desired_apps.contains_key(**name))
            .map(|name| (*name).to_owned())
            .collect::<Vec<_>>();
        if !removed.is_empty() {
            let supervisor = Arc::clone(&self.supervisor);
            thread::spawn(move || {
                while !retired.is_quiescent() {
                    thread::sleep(Duration::from_millis(10));
                }
                for key in removed {
                    if let Err(error) = supervisor.remove(&key) {
                        eprintln!("cygnus-daemon: retired runtime {key:?} did not stop: {error}");
                    }
                }
            });
        }
        Ok(AdminData::ConfigApplied {
            listen: config.listen.to_string(),
            app_count: config.apps.len(),
        })
    }

    fn register_engine(
        &self,
        version: String,
        host_root: PathBuf,
        cage_executable: PathBuf,
        audit: &AuditContext,
    ) -> Result<AdminData, AdminMutationError> {
        let mut state = State::open(&self.state_path).map_err(map_admin_state_error)?;
        let engine =
            register_engine_with_audit(&mut state, version, host_root, cage_executable, audit)
                .map_err(map_deploy_error)?;
        Ok(AdminData::EngineRegistered {
            version: engine.version,
            sha256: engine.sha256,
        })
    }

    fn deploy(
        &self,
        request: DeployRequest,
        audit: &AuditContext,
    ) -> Result<AdminData, AdminMutationError> {
        let mut state = State::open(&self.state_path).map_err(map_admin_state_error)?;
        let result = self
            .runtime
            .deploy(&mut state, request, audit)
            .map_err(map_deploy_error)?;
        Ok(AdminData::DeploymentActivated {
            app: result.deployment.app,
            deployment_id: result.deployment_id,
            artifact_hash: result.artifact_hash,
            engine_version: result.deployment.engine_version,
        })
    }
}

impl LiveAdminMutations {
    fn map_domain(
        &self,
        app: &str,
        domain: &str,
        audit: &AuditContext,
    ) -> Result<AdminData, AdminMutationError> {
        let mut state = State::open(&self.state_path).map_err(map_admin_state_error)?;
        let snapshot = state.load().map_err(map_admin_state_error)?;
        let loaded = snapshot
            .apps
            .iter()
            .find(|candidate| candidate.name == app)
            .ok_or_else(|| {
                AdminMutationError::new(AdminErrorCode::NotFound, "app does not exist")
            })?;
        let mut routes = route_table(&snapshot);
        let canonical = state
            .map_domain(app, domain, audit)
            .map_err(map_admin_state_error)?;
        routes.insert(
            &canonical,
            Route {
                app: loaded.spec.name.clone(),
                upstream: loaded.upstream.clone(),
            },
        );
        drop(self.router.install(routes));
        Ok(AdminData::DomainMapped {
            app: app.to_owned(),
            domain: canonical,
        })
    }

    fn rollback(
        &self,
        app: &str,
        deployment: &str,
        expected_active_artifact: &str,
        audit: &AuditContext,
    ) -> Result<AdminData, AdminMutationError> {
        let mut state = State::open(&self.state_path).map_err(map_admin_state_error)?;
        let mut plan = state
            .plan_rollback(app, deployment, expected_active_artifact)
            .map_err(map_admin_state_error)?;
        configure_tenant_admin(&mut plan.candidate, &self.tenant_admin_socket).map_err(|_| {
            AdminMutationError::new(
                AdminErrorCode::Internal,
                "Tenant admin mount is unavailable",
            )
        })?;
        let target = state
            .deployment(deployment)
            .map_err(map_admin_state_error)?
            .ok_or_else(|| {
                AdminMutationError::new(AdminErrorCode::NotFound, "deployment does not exist")
            })?;
        let mut snapshot = state.load().map_err(map_admin_state_error)?;
        let current = snapshot
            .apps
            .iter_mut()
            .find(|candidate| candidate.name == app)
            .ok_or_else(|| {
                AdminMutationError::new(AdminErrorCode::NotFound, "app does not exist")
            })?;
        *current = plan.candidate.clone();
        let routes = route_table(&snapshot);
        let runtime_changed = plan.previous_runtime_key.as_deref() != Some(&plan.runtime_key);
        if runtime_changed {
            self.supervisor.register(
                plan.runtime_key.clone(),
                plan.candidate.spec.clone(),
                plan.candidate.lifecycle.clone(),
            );
            if self.supervisor.acquire(&plan.runtime_key).is_err() {
                let _ = self.supervisor.remove(&plan.runtime_key);
                return Err(AdminMutationError::new(
                    AdminErrorCode::Internal,
                    "rollback candidate did not become ready",
                ));
            }
        }
        if let Err(error) = state.commit_activation(&plan, audit) {
            if runtime_changed {
                let _ = self.supervisor.remove(&plan.runtime_key);
            }
            return Err(map_admin_state_error(error));
        }
        let retired = self.router.install(routes);
        if runtime_changed && let Some(previous) = plan.previous_runtime_key.clone() {
            let supervisor = Arc::clone(&self.supervisor);
            thread::spawn(move || {
                while !retired.is_quiescent() {
                    thread::sleep(Duration::from_millis(10));
                }
                if let Err(error) = supervisor.remove(&previous) {
                    eprintln!("cygnus-daemon: retired runtime {previous:?} did not stop: {error}");
                }
            });
        }
        Ok(AdminData::Activated {
            app: app.to_owned(),
            active: ActiveDeploymentView {
                deployment_id: deployment.to_owned(),
                artifact_hash: plan.target_artifact_hash,
                engine_version: target.engine_version,
            },
        })
    }
}

fn map_deploy_error(error: DeployError) -> AdminMutationError {
    match error {
        DeployError::State(error) => map_admin_state_error(error),
        DeployError::InvalidInput(message) => {
            AdminMutationError::new(AdminErrorCode::Validation, message)
        }
        DeployError::ActivationFailed { detail, .. } => {
            AdminMutationError::new(AdminErrorCode::Conflict, detail)
        }
        DeployError::BuildFailed { detail, .. } => {
            AdminMutationError::new(AdminErrorCode::Validation, detail)
        }
        DeployError::Io(error) => {
            AdminMutationError::new(AdminErrorCode::Internal, error.to_string())
        }
        DeployError::Json(error) => {
            AdminMutationError::new(AdminErrorCode::Validation, error.to_string())
        }
    }
}

fn map_admin_state_error(error: StateError) -> AdminMutationError {
    let code = match error {
        StateError::AppNotFound(_) => AdminErrorCode::NotFound,
        StateError::DomainConflict { .. }
        | StateError::ActivationConflict { .. }
        | StateError::DestructiveApply => AdminErrorCode::Conflict,
        StateError::InvalidConfig(_)
        | StateError::InvalidRecord { .. }
        | StateError::InvalidDeploymentTransition { .. }
        | StateError::ArtifactOwnership { .. }
        | StateError::MetadataMismatch => AdminErrorCode::Validation,
        _ => AdminErrorCode::Internal,
    };
    let message = match code {
        AdminErrorCode::NotFound => "requested object does not exist",
        AdminErrorCode::Conflict => "state changed; refresh and try again",
        AdminErrorCode::Validation => "mutation was rejected",
        _ => "admin mutation failed",
    };
    AdminMutationError::new(code, message)
}

fn route_table(snapshot: &Snapshot) -> RouteTable {
    let mut routes = RouteTable::new();
    for app in &snapshot.apps {
        for domain in &app.domains {
            routes.insert(
                domain,
                Route {
                    app: app.spec.name.clone(),
                    upstream: app.upstream.clone(),
                },
            );
        }
    }
    routes
}

fn serve(
    state_path: &Path,
    admin_socket: &Path,
    tenant_admin_socket: &Path,
) -> Result<(), Box<dyn Error>> {
    let shutdown = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(SIGINT, Arc::clone(&shutdown))?;
    signal_hook::flag::register(SIGTERM, Arc::clone(&shutdown))?;
    let state = State::open(state_path)?;
    let mut snapshot = state.load()?;
    drop(state);
    let https_listener = snapshot
        .edge
        .https_listen
        .map(TcpListener::bind)
        .transpose()?;
    let listener = TcpListener::bind(snapshot.listen)?;
    if admin_socket == tenant_admin_socket {
        return Err("host and Tenant Zero admin sockets must be distinct".into());
    }
    let mut admin_bindings = vec![AdminBinding::bind(admin_socket, AdminRole::Host)?];
    if snapshot.apps.iter().any(|app| app.tenant_admin) {
        admin_bindings.push(AdminBinding::bind(
            tenant_admin_socket,
            AdminRole::TenantZero,
        )?);
    }

    let router = Arc::new(Router::new(RouteTable::new()));
    let supervisor = Arc::new(Supervisor::<Cage>::new(boot_cage));
    let mut routes = RouteTable::new();
    let mut pinned = Vec::new();
    let edge = snapshot.edge.clone();
    let certificate_domains = snapshot
        .apps
        .iter()
        .flat_map(|app| app.domains.iter().cloned())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();

    for app in &mut snapshot.apps {
        configure_tenant_admin(app, tenant_admin_socket)?;
    }
    for app in snapshot.apps {
        install_app(&supervisor, &mut routes, &mut pinned, app);
    }
    drop(router.install(routes));

    for app in pinned {
        if let Err(error) = supervisor.acquire(&app) {
            eprintln!("cygnus-daemon: pinned app {app:?} did not boot: {error:?}");
        }
    }

    spawn_reaper(Arc::downgrade(&supervisor));
    let frontend = Arc::new(Frontend::new(Arc::clone(&router), Arc::clone(&supervisor)));
    let http_address = local_addr(&listener);
    let http_frontend = Arc::clone(&frontend);
    let http_shutdown = Arc::clone(&shutdown);
    let http_failure = Arc::clone(&shutdown);
    eprintln!("cygnus-daemon: HTTP listening on {http_address}");
    let http_thread = thread::spawn(move || {
        let result = http_frontend.serve_until(listener, &http_shutdown);
        if result.is_err() {
            http_failure.store(true, Ordering::Release);
        }
        result
    });
    if https_listener.is_some() && edge.acme.is_some() {
        ensure_acme_certificates(
            state_path,
            &edge,
            &certificate_domains,
            frontend.http01_challenges(),
        )?;
    }
    let tls = https_listener
        .map(|listener| {
            let state = State::open(state_path)?;
            let certificates = state.certificates()?;
            Ok::<_, Box<dyn Error>>((listener, TlsServer::from_certificates(&certificates)?))
        })
        .transpose()?;
    let renewal_thread = tls.as_ref().and_then(|(_, tls)| {
        edge.acme.as_ref()?;
        Some(spawn_certificate_renewer(
            state_path.to_owned(),
            edge.clone(),
            certificate_domains.clone(),
            frontend.http01_challenges(),
            tls.clone(),
            Arc::clone(&shutdown),
        ))
    });
    let tls_thread = tls.map(|(tls_listener, tls)| {
        let tls_address = local_addr(&tls_listener);
        let tls_frontend = Arc::clone(&frontend);
        let tls_shutdown = Arc::clone(&shutdown);
        let tls_failure = Arc::clone(&shutdown);
        eprintln!("cygnus-daemon: HTTPS listening on {tls_address}");
        thread::spawn(move || {
            let result = tls_frontend.serve_tls_until(tls_listener, tls, &tls_shutdown);
            if result.is_err() {
                tls_failure.store(true, Ordering::Release);
            }
            result
        })
    });
    let lifecycle_supervisor = Arc::clone(&supervisor);
    let lifecycle_state_path = state_path.to_owned();
    let live_runtime = Arc::new(LiveDeployRuntime::new(
        Arc::clone(&supervisor),
        Arc::clone(&router),
        tenant_admin_socket.to_owned(),
    ));
    let mutations: Arc<dyn AdminMutationHandler> = Arc::new(LiveAdminMutations {
        state_path: state_path.to_owned(),
        supervisor: Arc::clone(&supervisor),
        tenant_admin_socket: tenant_admin_socket.to_owned(),
        router: Arc::clone(&router),
        runtime: Arc::clone(&live_runtime),
    });
    let github = Arc::new(GitHubManager::new(state_path));
    let github_worker = GitHubWorker::new(
        (*github).clone(),
        Arc::new(ProductionGitHubDeployExecutor::new(Arc::clone(
            &live_runtime,
        ))),
    );
    let github_shutdown = Arc::clone(&shutdown);
    let github_thread = thread::spawn(move || {
        while !github_shutdown.load(Ordering::Acquire) {
            if let Err(error) = github_worker.run_once() {
                eprintln!("cygnus-daemon: GitHub worker error: {error}");
            }
            thread::sleep(Duration::from_millis(250));
        }
    });
    let admin_handler: Arc<dyn AdminHandler> = Arc::new(
        StateAdminHandler::new(
            state_path,
            move |app| {
                let state = State::open(&lifecycle_state_path).ok()?;
                let snapshot = state.load().ok()?;
                let runtime = snapshot
                    .apps
                    .into_iter()
                    .find(|candidate| candidate.name == app)?;
                lifecycle_supervisor
                    .state(&runtime.spec.name)
                    .map(lifecycle_state_name)
            },
            mutations,
        )
        .with_github(Arc::clone(&github)),
    );
    let admin_server = AdminServer::new(admin_bindings, admin_handler);
    let admin_shutdown = Arc::clone(&shutdown);
    let admin_failure = Arc::clone(&shutdown);
    let admin_thread = thread::spawn(move || {
        let result = admin_server.serve(admin_shutdown);
        if result.is_err() {
            admin_failure.store(true, Ordering::Release);
        }
        result
    });
    eprintln!(
        "cygnus-daemon: admin listening on {}",
        admin_socket.display()
    );
    while !shutdown.load(Ordering::Acquire) {
        thread::sleep(Duration::from_millis(100));
    }
    shutdown.store(true, Ordering::Release);
    let http_result = http_thread
        .join()
        .map_err(|_| io::Error::other("HTTP server thread panicked"))?;
    let admin_result = admin_thread
        .join()
        .map_err(|_| io::Error::other("admin server thread panicked"))?;
    let tls_result = tls_thread
        .map(|thread| {
            thread
                .join()
                .map_err(|_| io::Error::other("TLS server thread panicked"))
        })
        .transpose()?;
    if let Some(thread) = renewal_thread {
        thread
            .join()
            .map_err(|_| io::Error::other("ACME renewal thread panicked"))?;
    }
    github_thread
        .join()
        .map_err(|_| io::Error::other("GitHub worker thread panicked"))?;
    for (app, error) in supervisor.shutdown_all() {
        eprintln!("cygnus-daemon: app {app:?} did not shut down cleanly: {error}");
    }
    if let Some(result) = tls_result {
        result?;
    }
    http_result?;
    admin_result?;
    Ok(())
}
fn ensure_acme_certificates(
    state_path: &Path,
    edge: &EdgeConfig,
    domains: &[String],
    challenges: Http01Challenges,
) -> Result<(), Box<dyn Error>> {
    let Some(config) = edge.acme.clone() else {
        return Ok(());
    };
    if domains.is_empty() {
        return Err("ACME is configured but no routed domains exist".into());
    }
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;
    let renew_before = now + 30 * 24 * 60 * 60;
    let mut state = State::open(state_path)?;
    let installed = state.certificates()?;
    let pending = domains
        .chunks(100)
        .filter(|chunk| {
            !installed.iter().any(|certificate| {
                certificate.not_after_unix > renew_before
                    && chunk
                        .iter()
                        .all(|domain| certificate.domains.iter().any(|covered| covered == domain))
            })
        })
        .map(<[String]>::to_vec)
        .collect::<Vec<_>>();
    if pending.is_empty() {
        return Ok(());
    }
    let dns: Option<Arc<dyn Dns01Provider>> = match config.dns_provider.as_deref() {
        None => None,
        Some("cloudflare") => Some(Arc::new(CloudflareDnsProvider::from_environment()?)),
        Some(provider) => return Err(format!("unsupported DNS-01 provider {provider:?}").into()),
    };
    let manager = AcmeManager::new(config, state_path, challenges, dns)?;
    for chunk in pending {
        let input = manager.issue(&chunk)?;
        let mut digest = Sha256::new();
        for domain in &chunk {
            digest.update(domain.as_bytes());
        }
        let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        state.install_certificate(
            &input,
            &AuditContext {
                endpoint_role: AuditEndpointRole::Host,
                peer_uid: Some(unsafe { libc::geteuid() }),
                peer_gid: Some(unsafe { libc::getegid() }),
                peer_pid: Some(std::process::id()),
                actor_subject: Some("cygnus-acme".into()),
                request_id: format!("acme-{timestamp}"),
                command_kind: "certificate_install".into(),
                request_digest: format!("{:x}", digest.finalize()),
            },
        )?;
    }
    Ok(())
}

fn spawn_certificate_renewer(
    state_path: PathBuf,
    edge: EdgeConfig,
    domains: Vec<String>,
    challenges: Http01Challenges,
    tls: TlsServer,
    shutdown: Arc<AtomicBool>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut next_check = Instant::now() + Duration::from_secs(12 * 60 * 60);
        while !shutdown.load(Ordering::Acquire) {
            if Instant::now() < next_check {
                thread::sleep(Duration::from_millis(250));
                continue;
            }
            match ensure_acme_certificates(&state_path, &edge, &domains, challenges.clone()) {
                Ok(()) => match State::open(&state_path)
                    .and_then(|state| state.certificates())
                    .map_err(|error| error.to_string())
                    .and_then(|certificates| {
                        tls.reload(&certificates).map_err(|error| error.to_string())
                    }) {
                    Ok(()) => next_check = Instant::now() + Duration::from_secs(12 * 60 * 60),
                    Err(error) => {
                        eprintln!("cygnus-daemon: TLS certificate reload failed: {error}");
                        next_check = Instant::now() + Duration::from_secs(60 * 60);
                    }
                },
                Err(error) => {
                    eprintln!("cygnus-daemon: ACME renewal failed: {error}");
                    next_check = Instant::now() + Duration::from_secs(60 * 60);
                }
            }
        }
    })
}

fn configure_tenant_admin(app: &mut LoadedApp, socket: &Path) -> Result<(), Box<dyn Error>> {
    if !app.tenant_admin {
        return Ok(());
    }
    let parent = socket
        .parent()
        .ok_or("Tenant Zero admin socket has no parent directory")?;
    app.spec.admin_socket = Some(AdminSocketSpec::new(parent));
    app.spec.env.insert(
        "CYGNUS_ADMIN_SOCKET".into(),
        PathBuf::from(ADMIN_CAGE_DIR)
            .join(ADMIN_SOCKET_FILENAME)
            .into_os_string(),
    );
    app.spec.validate()?;
    Ok(())
}

fn install_app(
    supervisor: &Supervisor<Cage>,
    routes: &mut RouteTable,
    pinned: &mut Vec<String>,
    app: LoadedApp,
) {
    let LoadedApp {
        name: _,
        domains,
        upstream,
        spec,
        lifecycle,
        tenant_admin: _,
    } = app;

    let runtime_key = spec.name.clone();
    if lifecycle.min_instances >= 1 {
        pinned.push(runtime_key.clone());
    }
    supervisor.register(runtime_key.clone(), spec, lifecycle);
    for domain in domains {
        routes.insert(
            &domain,
            Route {
                app: runtime_key.clone(),
                upstream: upstream.clone(),
            },
        );
    }
}

fn lifecycle_state_name(state: LifecycleState) -> String {
    match state {
        LifecycleState::Cold => "cold",
        LifecycleState::Booting => "booting",
        LifecycleState::Ready => "ready",
        LifecycleState::Draining => "draining",
        LifecycleState::Failed => "failed",
    }
    .into()
}

fn spawn_reaper(supervisor: Weak<Supervisor<Cage>>) {
    thread::spawn(move || {
        loop {
            thread::sleep(REAPER_INTERVAL);
            let Some(supervisor) = supervisor.upgrade() else {
                return;
            };
            let now = Instant::now();
            supervisor.reconcile(now);
            supervisor.reap_idle(now);
        }
    });
}

fn boot_cage(spec: &CageSpec) -> Result<Cage, String> {
    prepare_upstream(spec)?;
    Cage::boot(spec.clone()).map_err(|error| error.to_string())
}

fn prepare_upstream(spec: &CageSpec) -> Result<(), String> {
    let Some(path) = &spec.readiness_uds else {
        return Err(format!("app {:?} has no readiness UDS", spec.name));
    };
    let parent = path
        .parent()
        .ok_or_else(|| format!("upstream {} has no parent directory", path.display()))?;
    fs::create_dir_all(parent).map_err(|error| {
        format!(
            "create upstream directory {} for app {:?}: {error}",
            parent.display(),
            spec.name
        )
    })?;

    match UnixStream::connect(path) {
        Ok(_) => {
            return Err(format!(
                "upstream {} for app {:?} is already accepting connections",
                path.display(),
                spec.name
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(_) => {}
    }

    let metadata = fs::symlink_metadata(path).map_err(|error| {
        format!(
            "inspect stale upstream {} for app {:?}: {error}",
            path.display(),
            spec.name
        )
    })?;
    if !metadata.file_type().is_socket() {
        return Err(format!(
            "refusing to remove non-socket upstream {} for app {:?}",
            path.display(),
            spec.name
        ));
    }
    fs::remove_file(path).map_err(|error| {
        format!(
            "remove stale upstream {} for app {:?}: {error}",
            path.display(),
            spec.name
        )
    })
}

fn local_addr(listener: &TcpListener) -> SocketAddr {
    listener
        .local_addr()
        .unwrap_or_else(|_| SocketAddr::from(([0, 0, 0, 0], 0)))
}

#[cfg(test)]
mod tests {
    use std::os::unix::net::UnixListener;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    fn unique_dir(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "cygnus-daemon-{label}-{}-{nonce}",
            std::process::id()
        ))
    }

    #[derive(Clone)]
    struct FakeInstance {
        events: Arc<parking_lot::Mutex<Vec<&'static str>>>,
        shutdowns: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl Instance for FakeInstance {
        fn try_status(&mut self) -> Result<cygnus_supervisor::InstanceStatus, String> {
            Ok(cygnus_supervisor::InstanceStatus::Running)
        }

        fn shutdown(self) -> Result<(), String> {
            self.events.lock().push("shutdown");
            self.shutdowns.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    fn runtime_candidate(runtime_key: &str) -> LoadedApp {
        let upstream = PathBuf::from(format!("/tmp/{runtime_key}.sock"));
        let spec = CageSpec::new(runtime_key, "/bin/true");
        LoadedApp {
            name: "app".into(),
            domains: vec!["app.example".into()],
            tenant_admin: false,
            upstream,
            spec,
            lifecycle: Default::default(),
        }
    }

    fn fake_runtime() -> (
        LiveDeployRuntime<FakeInstance>,
        Arc<Supervisor<FakeInstance>>,
        Arc<Router>,
        Arc<parking_lot::Mutex<Vec<&'static str>>>,
        Arc<std::sync::atomic::AtomicUsize>,
    ) {
        let events = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let shutdowns = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let boot_events = Arc::clone(&events);
        let boot_shutdowns = Arc::clone(&shutdowns);
        let supervisor = Arc::new(Supervisor::new(move |_spec| {
            boot_events.lock().push("boot");
            Ok(FakeInstance {
                events: Arc::clone(&boot_events),
                shutdowns: Arc::clone(&boot_shutdowns),
            })
        }));
        let router = Arc::new(Router::new(RouteTable::new()));
        let runtime = LiveDeployRuntime::new(
            Arc::clone(&supervisor),
            Arc::clone(&router),
            "/tmp/cygnus-test-admin.sock",
        );
        (runtime, supervisor, router, events, shutdowns)
    }

    #[test]
    fn live_deploy_candidate_boot_precedes_commit() {
        let (runtime, supervisor, _router, events, _shutdowns) = fake_runtime();
        let candidate = runtime_candidate("r-new");
        let preparation = runtime.prepare_candidate(&candidate, None).unwrap();
        events.lock().push("commit");
        assert_eq!(&*events.lock(), &["boot", "commit"]);
        drop(preparation);
        assert_eq!(supervisor.state("r-new"), None);
    }

    #[test]
    fn live_deploy_commit_failure_preserves_old_route_and_runtime() {
        let (runtime, supervisor, router, _events, _shutdowns) = fake_runtime();
        let old = runtime_candidate("r-old");
        supervisor.register("r-old", old.spec.clone(), old.lifecycle.clone());
        supervisor.acquire("r-old").unwrap();
        let mut routes = RouteTable::new();
        routes.insert(
            "app.example",
            Route {
                app: "r-old".into(),
                upstream: old.upstream.clone(),
            },
        );
        drop(router.install(routes));

        let candidate = runtime_candidate("r-new");
        let preparation = runtime
            .prepare_candidate(&candidate, Some("r-old"))
            .unwrap();
        drop(preparation);

        assert_eq!(router.resolve("app.example").unwrap().app, "r-old");
        assert_eq!(supervisor.state("r-old"), Some(LifecycleState::Ready));
        assert_eq!(supervisor.state("r-new"), None);
        supervisor.shutdown_all();
    }

    #[test]
    fn live_deploy_success_routes_new_runtime_and_retires_after_quiescence() {
        let (runtime, supervisor, router, _events, shutdowns) = fake_runtime();
        let old = runtime_candidate("r-old");
        supervisor.register("r-old", old.spec.clone(), old.lifecycle.clone());
        supervisor.acquire("r-old").unwrap();
        let mut old_routes = RouteTable::new();
        old_routes.insert(
            "app.example",
            Route {
                app: "r-old".into(),
                upstream: old.upstream.clone(),
            },
        );
        drop(router.install(old_routes));
        let old_route = router.resolve("app.example").unwrap();

        let new = runtime_candidate("r-new");
        let snapshot = Snapshot {
            listen: "127.0.0.1:3000".parse().unwrap(),
            edge: Default::default(),
            apps: vec![new.clone()],
        };
        runtime.install_after_commit(&snapshot, Some("r-old".into()), "r-new");
        assert_eq!(router.resolve("app.example").unwrap().app, "r-new");
        assert_eq!(supervisor.state("r-old"), Some(LifecycleState::Ready));

        drop(old_route);
        let deadline = Instant::now() + Duration::from_secs(1);
        while supervisor.state("r-old").is_some() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(supervisor.state("r-old"), None);
        assert_eq!(shutdowns.load(Ordering::SeqCst), 1);
        supervisor.shutdown_all();
    }

    #[test]
    fn live_deploy_same_artifact_is_a_noop() {
        let (runtime, supervisor, router, _events, _shutdowns) = fake_runtime();
        let current = runtime_candidate("r-current");
        supervisor.register("r-current", current.spec.clone(), current.lifecycle.clone());
        supervisor.acquire("r-current").unwrap();
        let mut routes = RouteTable::new();
        routes.insert(
            "app.example",
            Route {
                app: "r-current".into(),
                upstream: current.upstream.clone(),
            },
        );
        drop(router.install(routes));
        let before = router.resolve("app.example").unwrap();
        let snapshot = Snapshot {
            listen: "127.0.0.1:3000".parse().unwrap(),
            edge: Default::default(),
            apps: vec![current],
        };
        runtime.install_after_commit(&snapshot, Some("r-current".into()), "r-current");
        let after = router.resolve("app.example").unwrap();
        assert!(Arc::ptr_eq(&before, &after));
        assert_eq!(supervisor.state("r-current"), Some(LifecycleState::Ready));
        supervisor.shutdown_all();
    }

    #[test]
    fn initial_config_applies_once_with_real_audit_provenance() {
        let directory = unique_dir("initial-config");
        fs::create_dir_all(&directory).unwrap();
        let state_path = directory.join("state.db");
        let config_path = directory.join("node.json");
        let mut config = NodeConfig::default();
        config.listen = "127.0.0.1:3300".parse().unwrap();
        let bytes = serde_json::to_vec(&config).unwrap();
        fs::write(&config_path, &bytes).unwrap();

        assert!(apply_initial_config(&state_path, &config_path).unwrap());
        let state = State::open(&state_path).unwrap();
        assert_eq!(state.load().unwrap().listen, config.listen);
        let audit = state.audit_records().unwrap();
        assert_eq!(audit.len(), 1);
        assert_eq!(
            audit[0].actor_subject.as_deref(),
            Some("system:initial-config")
        );
        assert_eq!(
            audit[0].request_digest,
            format!("{:x}", Sha256::digest(&bytes))
        );
        drop(state);

        config.listen = "127.0.0.1:4400".parse().unwrap();
        fs::write(&config_path, serde_json::to_vec(&config).unwrap()).unwrap();
        assert!(!apply_initial_config(&state_path, &config_path).unwrap());
        assert_eq!(
            State::open(&state_path).unwrap().load().unwrap().listen,
            "127.0.0.1:3300".parse().unwrap()
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn rejected_initial_config_leaves_no_database() {
        let directory = unique_dir("invalid-initial-config");
        fs::create_dir_all(&directory).unwrap();
        let state_path = directory.join("state.db");
        let config_path = directory.join("node.json");
        let mut config = NodeConfig::default();
        config.apps.push(cygnus_daemon::state::AppConfig {
            name: String::new(),
            upstream: directory.join("app.sock"),
            command: "/bin/true".into(),
            ..Default::default()
        });
        fs::write(&config_path, serde_json::to_vec(&config).unwrap()).unwrap();

        assert!(apply_initial_config(&state_path, &config_path).is_err());
        assert!(!state_path.exists());
        assert!(!state_path.with_extension("db-wal").exists());
        assert!(!state_path.with_extension("db-shm").exists());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn boot_preparation_removes_a_stale_socket() {
        let directory = unique_dir("stale");
        fs::create_dir_all(&directory).expect("create test directory");
        let socket = directory.join("app.sock");
        let listener = UnixListener::bind(&socket).expect("bind stale socket");
        drop(listener);

        let mut spec = CageSpec::new("stale", "/bin/true");
        spec.readiness_uds = Some(socket.clone());
        prepare_upstream(&spec).expect("prepare stale socket");

        assert!(!socket.exists());
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn boot_preparation_preserves_a_non_socket_path() {
        let directory = unique_dir("file");
        fs::create_dir_all(&directory).expect("create test directory");
        let socket = directory.join("app.sock");
        fs::write(&socket, b"owned by someone else").expect("write sentinel");

        let mut spec = CageSpec::new("file", "/bin/true");
        spec.readiness_uds = Some(socket.clone());
        let error = prepare_upstream(&spec).expect_err("regular file must be preserved");

        assert!(error.contains("refusing to remove non-socket"));
        assert_eq!(
            fs::read(&socket).expect("read sentinel"),
            b"owned by someone else"
        );
        fs::remove_dir_all(directory).expect("remove test directory");
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn overlay_rooted_request_reaches_the_cage() {
        use std::io::{Read, Write};
        use std::net::{Shutdown, TcpStream};

        use cygnus_daemon::state::{AppConfig, RootfsConfig};

        let directory = unique_dir("overlay-request");
        let host_io = directory.join("io");
        fs::create_dir_all(&host_io).expect("create host ingress directory");
        let lower = build_fixture_root(&directory).expect("build fixture root");
        let upstream = host_io.join("app.sock");
        let state_path = directory.join("state.db");
        let app_name = format!("daemon-ingress-{}", std::process::id());

        let mut app = AppConfig {
            name: app_name.clone(),
            domains: vec!["overlay.localhost".into()],
            upstream: upstream.clone(),
            command: "/fixture".into(),
            args: vec![
                "--exact".into(),
                "tests::cage_fixture_process".into(),
                "--nocapture".into(),
            ],
            rootfs: Some(RootfsConfig {
                lowerdirs: vec![lower],
                staging_dir: Some(directory.join("staging")),
                ..RootfsConfig::default()
            }),
            seccomp: None,
            ..AppConfig::default()
        };
        app.env.insert("CYGNUS_FIXTURE_MODE".into(), "uds".into());
        let config = NodeConfig {
            listen: "127.0.0.1:0".parse().expect("listen address"),
            edge: Default::default(),
            apps: vec![app],
        };
        let mut state = State::open(&state_path).expect("open state");
        state.apply(&config).expect("apply state");
        let snapshot = state.load().expect("load state");
        drop(state);

        // Probe the privileged environment with the exact projected spec so
        // an unavailable namespace/cgroup host skips, while a mount failure is
        // still a real test failure. The request path below cold-boots again.
        match Cage::boot(snapshot.apps[0].spec.clone()) {
            Ok(cage) => cage.teardown().expect("tear down environment probe"),
            Err(error)
                if cage_environment_unavailable(&error)
                    && std::env::var_os("CYGNUS_REQUIRE_PRIVILEGED").is_none() =>
            {
                eprintln!("skipping overlay request test: {error}");
                let _ = fs::remove_dir_all(directory);
                return;
            }
            Err(error) => panic!("overlay ingress probe failed: {error}"),
        }

        let supervisor = Arc::new(Supervisor::<Cage>::new(boot_cage));
        let mut routes = RouteTable::new();
        let mut pinned = Vec::new();
        for app in snapshot.apps {
            install_app(&supervisor, &mut routes, &mut pinned, app);
        }
        let frontend = Arc::new(Frontend::new(
            Arc::new(Router::new(routes)),
            Arc::clone(&supervisor),
        ));
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test front");
        let address = listener.local_addr().expect("front address");
        let serving = Arc::clone(&frontend);
        let worker = thread::spawn(move || {
            let (client, _) = listener.accept().expect("accept test client");
            serving.serve_connection(client);
        });

        let mut client = TcpStream::connect(address).expect("connect test front");
        client
            .write_all(b"GET / HTTP/1.1\r\nHost: overlay.localhost\r\nConnection: close\r\n\r\n")
            .expect("write request");
        client.shutdown(Shutdown::Write).expect("finish request");
        let mut response = Vec::new();
        client.read_to_end(&mut response).expect("read response");
        worker.join().expect("join front worker");

        assert!(response.starts_with(b"HTTP/1.1 200"));
        assert!(
            response
                .windows(b"overlay request reached the cage".len())
                .any(|window| window == b"overlay request reached the cage")
        );

        use cygnus_daemon::edge::CertificateRecord;
        use rcgen::{CertifiedKey as GeneratedKey, generate_simple_self_signed};
        use rustls::pki_types::ServerName;
        use rustls::{ClientConfig, ClientConnection, RootCertStore, StreamOwned};
        use std::os::unix::fs::PermissionsExt;

        let certificate_directory = directory.join("tls");
        fs::create_dir(&certificate_directory).unwrap();
        fs::set_permissions(&certificate_directory, fs::Permissions::from_mode(0o700)).unwrap();
        let GeneratedKey { cert, signing_key } =
            generate_simple_self_signed(["overlay.localhost".to_owned()]).unwrap();
        let certificate_path = certificate_directory.join("fullchain.pem");
        let private_key_path = certificate_directory.join("key.pem");
        fs::write(&certificate_path, cert.pem()).unwrap();
        fs::write(&private_key_path, signing_key.serialize_pem()).unwrap();
        fs::set_permissions(&certificate_path, fs::Permissions::from_mode(0o600)).unwrap();
        fs::set_permissions(&private_key_path, fs::Permissions::from_mode(0o600)).unwrap();
        let tls = TlsServer::from_certificates(&[CertificateRecord {
            id: "overlay-test".into(),
            domains: vec!["overlay.localhost".into()],
            generation: "a".repeat(64),
            certificate_path,
            private_key_path,
            not_after_unix: 4_102_444_800,
            installed_at: "test".into(),
        }])
        .unwrap();
        let tls_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let tls_address = tls_listener.local_addr().unwrap();
        let tls_frontend = Arc::clone(&frontend);
        let tls_worker = thread::spawn(move || {
            let (client, _) = tls_listener.accept().unwrap();
            tls_frontend.serve_tls_connection(client, &tls);
        });
        let mut roots = RootCertStore::empty();
        roots.add(cert.der().clone()).unwrap();
        let client_config = ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let connection = ClientConnection::new(
            Arc::new(client_config),
            ServerName::try_from("overlay.localhost".to_owned()).unwrap(),
        )
        .unwrap();
        let mut tls_client = StreamOwned::new(
            connection,
            TcpStream::connect(tls_address).expect("connect TLS front"),
        );
        tls_client
            .write_all(b"GET / HTTP/1.1\r\nHost: overlay.localhost\r\nConnection: close\r\n\r\n")
            .unwrap();
        tls_client.flush().unwrap();
        let mut tls_response = Vec::new();
        tls_client.read_to_end(&mut tls_response).unwrap();
        tls_worker.join().unwrap();
        assert!(tls_response.starts_with(b"HTTP/1.1 200"));
        assert!(
            tls_response
                .windows(b"overlay request reached the cage".len())
                .any(|window| window == b"overlay request reached the cage")
        );

        drop(frontend);
        drop(supervisor);
        fs::remove_dir_all(directory).expect("remove integration fixture");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn cage_fixture_process() {
        use std::io::{Read, Write};
        use std::os::unix::net::UnixListener;

        if std::env::var("CYGNUS_FIXTURE_MODE").as_deref() != Ok("uds") {
            return;
        }
        let listener = UnixListener::bind("/cygnus/io/app.sock").expect("bind fixture UDS");
        for stream in listener.incoming() {
            let mut stream = stream.expect("accept fixture UDS");
            let mut request = [0_u8; 4096];
            let read = stream.read(&mut request).unwrap_or(0);
            if read > 0 {
                let body = b"overlay request reached the cage\n";
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nconnection: close\r\ncontent-length: {}\r\n\r\n",
                    body.len()
                )
                .expect("write fixture response head");
                stream.write_all(body).expect("write fixture response body");
            }
        }
    }

    #[cfg(target_os = "linux")]
    fn build_fixture_root(base: &Path) -> std::io::Result<PathBuf> {
        let root = base.join("root");
        fs::create_dir_all(&root)?;
        let executable = std::env::current_exe()?;
        fs::copy(&executable, root.join("fixture"))?;

        let output = std::process::Command::new("ldd")
            .arg(&executable)
            .output()?;
        if !output.status.success() {
            return Err(std::io::Error::other("ldd failed for cage fixture"));
        }
        let dependencies = String::from_utf8(output.stdout)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        for line in dependencies.lines() {
            let Some(source) = line
                .split_whitespace()
                .find(|field| field.starts_with('/'))
                .map(PathBuf::from)
            else {
                continue;
            };
            let relative = source.strip_prefix("/").expect("absolute ldd dependency");
            let destination = root.join(relative);
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(source, destination)?;
        }
        Ok(root)
    }

    #[cfg(target_os = "linux")]
    fn cage_environment_unavailable(error: &cygnus_cage::CageError) -> bool {
        matches!(
            error,
            cygnus_cage::CageError::NamespaceUnavailable { .. }
                | cygnus_cage::CageError::CgroupUnavailable(_)
                | cygnus_cage::CageError::CgroupControllerUnavailable(_)
                | cygnus_cage::CageError::Io { .. }
        )
    }
}
