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
            supervisor.reap_idle(Instant::now());
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
}
