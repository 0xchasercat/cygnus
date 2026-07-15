use std::error::Error;
use std::fs;
use std::net::{SocketAddr, TcpListener};
use std::os::unix::fs::FileTypeExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, Weak};
use std::thread;
use std::time::{Duration, Instant};

use clap::{Parser, Subcommand};
use cygnus_cage::{Cage, CageSpec};
use cygnus_daemon::Frontend;
use cygnus_daemon::state::{DEFAULT_STATE_PATH, LoadedApp, NodeConfig, State};
use cygnus_router::{Route, RouteTable, Router};
use cygnus_supervisor::Supervisor;

const REAPER_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Debug, Parser)]
#[command(name = "cygnus-daemon", about = "Run the Cygnus request plane")]
struct Cli {
    /// SQLite state database.
    #[arg(long, global = true, default_value = DEFAULT_STATE_PATH)]
    state: PathBuf,

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
        Command::Serve => serve(&cli.state),
        Command::Apply { config } => apply(&cli.state, &config),
    }
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

fn serve(state_path: &Path) -> Result<(), Box<dyn Error>> {
    let state = State::open(state_path)?;
    let snapshot = state.load()?;
    let listener = TcpListener::bind(snapshot.listen)?;

    let supervisor = Arc::new(Supervisor::<Cage>::new(boot_cage));
    let mut routes = RouteTable::new();
    let mut pinned = Vec::new();

    for app in snapshot.apps {
        install_app(&supervisor, &mut routes, &mut pinned, app);
    }

    for app in pinned {
        if let Err(error) = supervisor.acquire(&app) {
            eprintln!("cygnus-daemon: pinned app {app:?} did not boot: {error:?}");
        }
    }

    spawn_reaper(Arc::downgrade(&supervisor));
    let frontend = Arc::new(Frontend::new(Arc::new(Router::new(routes)), supervisor));
    eprintln!("cygnus-daemon: listening on {}", local_addr(&listener));
    frontend.serve(listener)?;
    Ok(())
}

fn install_app(
    supervisor: &Supervisor<Cage>,
    routes: &mut RouteTable,
    pinned: &mut Vec<String>,
    app: LoadedApp,
) {
    let LoadedApp {
        name,
        domains,
        upstream,
        spec,
        lifecycle,
    } = app;

    if lifecycle.min_instances >= 1 {
        pinned.push(name.clone());
    }
    supervisor.register(name.clone(), spec, lifecycle);
    for domain in domains {
        routes.insert(
            &domain,
            Route {
                app: name.clone(),
                upstream: upstream.clone(),
            },
        );
    }
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
