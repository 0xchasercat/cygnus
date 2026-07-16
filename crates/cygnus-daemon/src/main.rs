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
use std::time::{Duration, Instant};

use clap::{Parser, Subcommand};
use cygnus_cage::{ADMIN_CAGE_DIR, ADMIN_SOCKET_FILENAME, AdminSocketSpec, Cage, CageSpec};
use cygnus_daemon::Frontend;
use cygnus_daemon::admin::{
    ActiveDeploymentView, AdminBinding, AdminData, AdminErrorCode, AdminHandler, AdminMutation,
    AdminMutationError, AdminMutationHandler, AdminRole, AdminServer, DEFAULT_HOST_ADMIN_SOCKET,
    DEFAULT_TENANT_ADMIN_SOCKET, StateAdminHandler,
};
use cygnus_daemon::deploy::{DeployRequest, deploy, register_engine};
use cygnus_daemon::state::{
    AuditContext, DEFAULT_STATE_PATH, LoadedApp, NodeConfig, Snapshot, State, StateError,
};
use cygnus_daemon::tls::TlsServer;
use cygnus_router::{Route, RouteTable, Router};
use cygnus_supervisor::{LifecycleState, Supervisor};
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
    Serve,
    /// Validate and atomically apply a complete JSON node configuration.
    Apply {
        /// JSON configuration to import into the state database.
        config: PathBuf,
    },
    /// Register an operator-trusted Bun engine.
    Engine {
        #[command(subcommand)]
        command: EngineCommand,
    },
    /// Build source and activate the first app.
    Deploy {
        /// Source directory (canonicalized and copied before building).
        #[arg(long = "source-dir", alias = "source")]
        source_dir: PathBuf,
        /// App name.
        #[arg(long)]
        app: String,
        /// Hostname route.
        #[arg(long)]
        domain: String,
        /// Registered engine version.
        #[arg(long)]
        engine_version: String,
        /// Relative source entry point.
        #[arg(long, default_value = "index.ts")]
        entry: PathBuf,
        /// Content-addressed artifact root.
        #[arg(long)]
        artifact_root: PathBuf,
        /// Host upstream Unix socket.
        #[arg(long)]
        upstream: PathBuf,
    },
}

#[derive(Debug, Subcommand)]
enum EngineCommand {
    /// Register one Bun executable and its content hash.
    Register {
        #[arg(long)]
        version: String,
        #[arg(long)]
        host_root: PathBuf,
        #[arg(long)]
        cage_executable: PathBuf,
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
    match cli.command.unwrap_or(Command::Serve) {
        Command::Serve => serve(&cli.state, &cli.admin_socket, &cli.tenant_admin_socket),
        Command::Apply { config } => apply(&cli.state, &config),
        Command::Engine { command } => engine_command(&cli.state, command),
        Command::Deploy {
            source_dir,
            app,
            domain,
            engine_version,
            entry,
            artifact_root,
            upstream,
        } => deploy_command(
            &cli.state,
            DeployRequest::new(
                source_dir,
                app,
                domain,
                engine_version,
                entry,
                artifact_root,
                upstream,
            ),
        ),
    }
}

fn engine_command(state_path: &Path, command: EngineCommand) -> Result<(), Box<dyn Error>> {
    let EngineCommand::Register {
        version,
        host_root,
        cage_executable,
    } = command;
    let mut state = State::open(state_path)?;
    let engine = register_engine(&mut state, version, host_root, cage_executable)?;
    println!("registered engine {} ({})", engine.version, engine.sha256);
    Ok(())
}

fn deploy_command(state_path: &Path, request: DeployRequest) -> Result<(), Box<dyn Error>> {
    let mut state = State::open(state_path)?;
    let result = deploy(&mut state, request)?;
    println!(
        "activated deployment {} artifact {}",
        result.deployment_id, result.artifact_hash
    );
    Ok(())
}

fn apply(state_path: &Path, config_path: &Path) -> Result<(), Box<dyn Error>> {
    let bytes = fs::read(config_path)?;
    let config: NodeConfig = serde_json::from_slice(&bytes)?;
    let mut state = State::open(state_path)?;
    state.apply(&config)?;
    println!(
        "applied {} app(s) to {} (listen {})",
        config.apps.len(),
        state_path.display(),
        config.listen
    );
    Ok(())
}

struct LiveAdminMutations {
    state_path: PathBuf,
    supervisor: Arc<Supervisor<Cage>>,
    router: Arc<Router>,
    tenant_admin_socket: PathBuf,
}

impl AdminMutationHandler for LiveAdminMutations {
    fn execute(
        &self,
        mutation: AdminMutation,
        audit: &AuditContext,
    ) -> Result<AdminData, AdminMutationError> {
        match mutation {
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

fn map_admin_state_error(error: StateError) -> AdminMutationError {
    let code = match error {
        StateError::AppNotFound(_) => AdminErrorCode::NotFound,
        StateError::DomainConflict { .. } | StateError::ActivationConflict { .. } => {
            AdminErrorCode::Conflict
        }
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
    let tls = if let Some(https_listen) = snapshot.edge.https_listen {
        let certificates = state.certificates()?;
        Some((
            TcpListener::bind(https_listen)?,
            TlsServer::from_certificates(&certificates)?,
        ))
    } else {
        None
    };
    drop(state);
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
    let mutations: Arc<dyn AdminMutationHandler> = Arc::new(LiveAdminMutations {
        state_path: state_path.to_owned(),
        supervisor: Arc::clone(&supervisor),
        tenant_admin_socket: tenant_admin_socket.to_owned(),
        router: Arc::clone(&router),
    });
    let admin_handler: Arc<dyn AdminHandler> = Arc::new(StateAdminHandler::new(
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
    ));
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
        "cygnus-daemon: listening on {} (admin {})",
        local_addr(&listener),
        admin_socket.display()
    );
    let serve_result = frontend.serve_until(listener, &shutdown);
    shutdown.store(true, Ordering::Release);
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
    for (app, error) in supervisor.shutdown_all() {
        eprintln!("cygnus-daemon: app {app:?} did not shut down cleanly: {error}");
    }
    if let Some(result) = tls_result {
        result?;
    }
    serve_result?;
    admin_result?;
    Ok(())
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
