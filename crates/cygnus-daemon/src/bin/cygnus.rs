//! `cygnus` — developer-facing client for the local Cygnus daemon.
//!
//! A thin porcelain layer over the existing AdminClient + AdminCommand
//! protocol. Read commands render a quiet instrument-panel view by default and
//! emit the raw pretty JSON they always have when given `--json`.

use std::error::Error;
use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use clap::{Parser, Subcommand};
use cygnus_daemon::admin::{
    ADMIN_PROTOCOL_VERSION, ActiveDeploymentView, AdminClient, AdminCommand, AdminData,
    AdminRequest, AdminResponse, AppView, DEFAULT_HOST_ADMIN_SOCKET, DeploymentView, LogStream,
    MAX_LOG_CHUNK_BYTES, NodeView,
};
use cygnus_daemon::deploy::DeployRequest;
use cygnus_daemon::state::DeploymentSource;
use cygnus_daemon::state::NodeConfig;
use indicatif::{ProgressBar, ProgressStyle};
use owo_colors::OwoColorize;

/// Cygnus — self-hosted serverless for Bun/Node apps.
#[derive(Debug, Parser)]
#[command(
    name = "cygnus",
    about = "Cygnus — self-hosted serverless for Bun/Node apps",
    long_about = "Cygnus — self-hosted serverless for Bun/Node apps.\n\n\
        A thin client for the local Cygnus daemon. Read commands render a compact \
        instrument-panel view; pass --json for the raw pretty JSON payload (stable \
        for scripting).",
    version,
    styles = cygnus_styles(),
)]
struct Cli {
    /// Daemon administration socket (default: $CYGNUS_ADMIN_SOCKET, then
    /// ~/.cygnus/run/admin.sock when present, then /var/run/cygnus/admin.sock).
    #[arg(long, global = true)]
    admin_socket: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Apply a node configuration file to the running daemon.
    Apply { config: PathBuf },
    /// Register or inspect the Bun engine used to run apps.
    Engine {
        #[command(subcommand)]
        command: EngineCommand,
    },
    /// Build and activate a source directory as a deployment.
    ///
    /// When run with no flags, deploys the current directory and names the app
    /// after the folder (e.g. `cygnus deploy` inside `~/apps/draco` → app `draco`).
    Deploy {
        /// Source directory (default: current directory).
        /// Accepts a bare path as the first positional argument.
        #[arg(long = "source-dir", alias = "source", value_name = "SOURCE_DIR")]
        source_dir: Option<PathBuf>,
        /// Optional positional source directory (same as --source-dir).
        #[arg(value_name = "SOURCE", conflicts_with = "source_dir")]
        source: Option<PathBuf>,
        /// App name (default: basename of the source directory).
        #[arg(long)]
        app: Option<String>,
        /// Hostname to route (default: <app>.<apps-domain>).
        #[arg(long)]
        domain: Option<String>,
        /// Engine version (default: the node's default engine).
        #[arg(long)]
        engine_version: Option<String>,
        /// Entry file inside the source directory (default: index.ts).
        #[arg(long)]
        entry: Option<PathBuf>,
        /// Artifact store root (default: daemon-owned).
        #[arg(long)]
        artifact_root: Option<PathBuf>,
        /// Upstream socket path (default: daemon-owned).
        #[arg(long)]
        upstream: Option<PathBuf>,
        /// Environment variable to set, as KEY=VALUE. Repeatable.
        #[arg(long = "env", value_name = "KEY=VALUE")]
        env: Vec<String>,
        /// Deploy as an isolated preview under `<app>-<slug>` instead of
        /// touching the production app/domain.
        #[arg(long)]
        preview: Option<String>,
    },
    /// Check protocol and daemon availability.
    Health,
    /// Show node configuration status.
    Status {
        #[arg(long)]
        json: bool,
    },
    /// List one page of registered apps.
    Apps {
        #[arg(long)]
        cursor: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: u16,
        #[arg(long)]
        json: bool,
    },
    /// Show one registered app.
    App {
        app: String,
        #[arg(long)]
        json: bool,
    },
    /// List one page of deployments.
    Deployments {
        #[arg(long)]
        app: Option<String>,
        #[arg(long)]
        cursor: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: u16,
        #[arg(long)]
        json: bool,
    },
    /// Show one deployment.
    Deployment {
        deployment: String,
        #[arg(long)]
        json: bool,
    },
    /// Add a hostname route to an app.
    MapDomain { app: String, domain: String },
    /// Atomically activate a retained sealed deployment.
    Rollback {
        app: String,
        deployment: String,
        #[arg(long)]
        expected_active_artifact: String,
    },
    /// Write a deployment build log to stdout. With no <DEPLOYMENT>, shows the
    /// most recent deployment's log (run `cygnus deployments` for ids).
    Logs {
        deployment: Option<String>,
        #[arg(long, value_enum, default_value_t = StreamArg::Stdout)]
        stream: StreamArg,
        #[arg(long, default_value_t = 0)]
        offset: u64,
        #[arg(long)]
        follow: bool,
    },
    /// Write the daemon process log to stdout.
    ///
    /// Looks under ~/.cygnus/log (macOS user install) then /var/log/cygnus.
    DaemonLogs {
        /// Read stderr instead of stdout.
        #[arg(long)]
        error: bool,
        /// Follow the file like `tail -f`.
        #[arg(long, short = 'f')]
        follow: bool,
        /// Print only the last N lines (default 200; ignored with --follow).
        #[arg(long, default_value_t = 200)]
        lines: usize,
    },
}

#[derive(Debug, Subcommand)]
enum EngineCommand {
    /// Register a Bun engine version rooted at a host directory.
    Register {
        #[arg(long)]
        version: String,
        #[arg(long)]
        host_root: PathBuf,
        #[arg(long)]
        cage_executable: PathBuf,
        /// Make this the node's default engine for deploys.
        #[arg(long)]
        default: bool,
    },
}

#[derive(Clone, Copy, Debug, clap::ValueEnum)]
enum StreamArg {
    Stdout,
    Stderr,
}

impl From<StreamArg> for LogStream {
    fn from(value: StreamArg) -> Self {
        match value {
            StreamArg::Stdout => Self::Stdout,
            StreamArg::Stderr => Self::Stderr,
        }
    }
}

fn main() -> ExitCode {
    let color = io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none();
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let prefix = if color {
                "cygnus: error:".red().to_string()
            } else {
                "cygnus: error:".to_owned()
            };
            eprintln!("{prefix} {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<(), Box<dyn Error>> {
    // Long-running commands hold the connection while the daemon works:
    // a deploy blocks through the whole server-side build, and engine
    // registration hashes the engine binary. Give them room to finish.
    let timeout = match &cli.command {
        Command::Deploy { .. } => Duration::from_secs(15 * 60),
        Command::Apply { .. } | Command::Engine { .. } => Duration::from_secs(120),
        _ => Duration::from_secs(10),
    };
    let admin_socket = resolve_admin_socket(cli.admin_socket);
    let client = AdminClient::new(&admin_socket).with_timeout(timeout)?;
    ADMIN_SOCKET_IN_USE
        .set(admin_socket)
        .expect("admin socket resolved once");
    let theme = Theme::detect();
    match cli.command {
        Command::Apply { config } => {
            let config: NodeConfig = serde_json::from_slice(&std::fs::read(config)?)?;
            let data = call(&client, AdminCommand::ApplyConfig(config))?;
            let AdminData::ConfigApplied { listen, app_count } = data else {
                return Err("daemon returned an unexpected response to ApplyConfig".into());
            };
            theme.line_kv("applied", &format!("listen {listen} · apps {app_count}"));
        }
        Command::Engine { command } => {
            let EngineCommand::Register {
                version,
                host_root,
                cage_executable,
                default,
            } = command;
            let data = call(
                &client,
                AdminCommand::RegisterEngine {
                    version,
                    host_root,
                    cage_executable,
                    is_default: default,
                },
            )?;
            let AdminData::EngineRegistered { version, sha256 } = data else {
                return Err("daemon returned an unexpected response to RegisterEngine".into());
            };
            theme.line_kv(
                "registered engine",
                &format!("{version} · sha256 {}", short_hash(&sha256)),
            );
        }
        Command::Deploy {
            source_dir,
            source,
            app,
            domain,
            engine_version,
            entry,
            artifact_root,
            upstream,
            env,
            preview,
        } => {
            let source_dir = resolve_deploy_source(source_dir.or(source))?;
            let app = resolve_deploy_app(app, &source_dir)?;
            let env = parse_env_flags(&env)?;
            let request = DeployRequest {
                source_dir,
                app,
                domain,
                engine_version,
                entry,
                artifact_root,
                upstream,
                env,
                preview,
                deployment_id: None,
                source: DeploymentSource::cli(),
            };
            render_deploy(&theme, &client, request)?;
        }
        Command::Health => {
            let data = call(&client, AdminCommand::Health)?;
            let AdminData::Health {
                service: _,
                isolation,
            } = data
            else {
                return Err("daemon returned an unexpected response to Health".into());
            };
            theme.health(&isolation);
        }
        Command::Status { json } => {
            let data = call(&client, AdminCommand::Status)?;
            if json {
                return print_json(data);
            }
            let AdminData::Status { node } = data else {
                return Err("daemon returned an unexpected response to Status".into());
            };
            theme.status(&node);
        }
        Command::Apps {
            cursor,
            limit,
            json,
        } => {
            let data = call(&client, AdminCommand::ListApps { cursor, limit })?;
            if json {
                return print_json(data);
            }
            let AdminData::Apps { apps, next_cursor } = data else {
                return Err("daemon returned an unexpected response to ListApps".into());
            };
            theme.apps(&apps, next_cursor.as_deref());
        }
        Command::App { app, json } => {
            let data = call(&client, AdminCommand::GetApp { app })?;
            if json {
                return print_json(data);
            }
            let AdminData::App { app } = data else {
                return Err("daemon returned an unexpected response to GetApp".into());
            };
            theme.app(&app);
        }
        Command::Deployments {
            app,
            cursor,
            limit,
            json,
        } => {
            let data = call(
                &client,
                AdminCommand::ListDeployments { app, cursor, limit },
            )?;
            if json {
                return print_json(data);
            }
            let AdminData::Deployments {
                deployments,
                next_cursor,
            } = data
            else {
                return Err("daemon returned an unexpected response to ListDeployments".into());
            };
            theme.deployments(&deployments, next_cursor.as_deref());
        }
        Command::Deployment { deployment, json } => {
            let data = call(&client, AdminCommand::GetDeployment { deployment })?;
            if json {
                return print_json(data);
            }
            let AdminData::Deployment { deployment } = data else {
                return Err("daemon returned an unexpected response to GetDeployment".into());
            };
            theme.deployment(&deployment);
        }
        Command::MapDomain { app, domain } => {
            let data = call(&client, AdminCommand::MapDomain { app, domain })?;
            let AdminData::DomainMapped { app, domain } = data else {
                return Err("daemon returned an unexpected response to MapDomain".into());
            };
            theme.mapped(&app, &domain);
        }
        Command::Rollback {
            app,
            deployment,
            expected_active_artifact,
        } => {
            let data = call(
                &client,
                AdminCommand::Rollback {
                    app,
                    deployment,
                    expected_active_artifact,
                },
            )?;
            let AdminData::Activated { app, active } = data else {
                return Err("daemon returned an unexpected response to Rollback".into());
            };
            theme.activated(&app, &active);
        }
        Command::Logs {
            deployment,
            stream,
            offset,
            follow,
        } => stream_log(&client, deployment, stream.into(), offset, follow)?,
        Command::DaemonLogs {
            error,
            follow,
            lines,
        } => stream_daemon_log(error, follow, lines)?,
    }
    Ok(())
}

static ADMIN_SOCKET_IN_USE: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// The socket to talk to, in order of intent: explicit flag, environment
/// (set for caged consoles and scripts), the per-user install location when
/// it exists (macOS), then the system-wide default.
fn resolve_admin_socket(flag: Option<PathBuf>) -> PathBuf {
    if let Some(path) = flag {
        return path;
    }
    if let Some(env_path) = std::env::var_os("CYGNUS_ADMIN_SOCKET") {
        let env_path = PathBuf::from(env_path);
        if env_path.is_absolute() {
            return env_path;
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        let user_socket = PathBuf::from(home).join(".cygnus/run/admin.sock");
        if user_socket.exists() {
            return user_socket;
        }
    }
    PathBuf::from(DEFAULT_HOST_ADMIN_SOCKET)
}

/// Resolve the deploy source directory: explicit path, or the process CWD.
fn resolve_deploy_source(source: Option<PathBuf>) -> Result<PathBuf, Box<dyn Error>> {
    let path = match source {
        Some(path) => path,
        None => std::env::current_dir()
            .map_err(|error| format!("cannot determine current directory for deploy: {error}"))?,
    };
    // Prefer absolute paths so the daemon and CLI agree on the root, and so a
    // relative `./` never collapses into a symlink trap under /tmp or volumes.
    let absolute = if path.is_absolute() {
        path
    } else {
        std::env::current_dir()
            .map_err(|error| format!("cannot resolve source directory: {error}"))?
            .join(path)
    };
    if !absolute.is_dir() {
        return Err(format!(
            "source directory does not exist or is not a directory: {}",
            absolute.display()
        )
        .into());
    }
    Ok(absolute)
}

/// Resolve the app name: explicit --app, else a sanitized folder basename.
fn resolve_deploy_app(
    app: Option<String>,
    source_dir: &std::path::Path,
) -> Result<String, Box<dyn Error>> {
    if let Some(app) = app {
        let trimmed = app.trim();
        if trimmed.is_empty() {
            return Err("app name must not be empty".into());
        }
        return Ok(trimmed.to_owned());
    }
    let name = source_dir
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or("cannot infer app name from source directory")?;
    let sanitized = sanitize_app_name(name);
    if sanitized.is_empty() {
        return Err(
            format!("cannot infer a valid app name from directory {name:?}; pass --app").into(),
        );
    }
    Ok(sanitized)
}

/// Parse repeated `--env KEY=VALUE` flags into a map. Later duplicates win.
fn parse_env_flags(
    flags: &[String],
) -> Result<std::collections::BTreeMap<String, String>, Box<dyn Error>> {
    let mut env = std::collections::BTreeMap::new();
    for flag in flags {
        let (key, value) = flag
            .split_once('=')
            .ok_or_else(|| format!("--env {flag:?} must be in KEY=VALUE form"))?;
        if key.is_empty() {
            return Err(format!("--env {flag:?} has an empty key").into());
        }
        env.insert(key.to_owned(), value.to_owned());
    }
    Ok(env)
}

/// Lowercase DNS-label-ish app names: a-z0-9 and hyphens, no leading/trailing hyphen.
fn sanitize_app_name(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut last_hyphen = false;
    for ch in raw.chars() {
        let mapped = match ch {
            'A'..='Z' => ch.to_ascii_lowercase(),
            'a'..='z' | '0'..='9' => ch,
            _ => '-',
        };
        if mapped == '-' {
            if out.is_empty() || last_hyphen {
                continue;
            }
            last_hyphen = true;
            out.push('-');
        } else {
            last_hyphen = false;
            out.push(mapped);
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    // Keep within the daemon's app-name budget.
    if out.len() > 63 {
        out.truncate(63);
        while out.ends_with('-') {
            out.pop();
        }
    }
    out
}

fn call(client: &AdminClient, command: AdminCommand) -> Result<AdminData, Box<dyn Error>> {
    let request = AdminRequest {
        version: ADMIN_PROTOCOL_VERSION,
        request_id: request_id(),
        actor: None,
        command,
    };
    let response = client.request(&request).map_err(|error| {
        let socket = ADMIN_SOCKET_IN_USE
            .get()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "the admin socket".to_owned());
        match error.kind() {
            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => {
                Box::<dyn Error>::from("timed out waiting for the daemon to answer")
            }
            io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused => Box::<dyn Error>::from(
                format!("cannot reach the Cygnus daemon at {socket} (is it running?)"),
            ),
            io::ErrorKind::PermissionDenied => Box::<dyn Error>::from(format!(
                "permission denied for {socket} (on Linux the admin socket is root-only — try sudo)"
            )),
            _ => Box::<dyn Error>::from(error),
        }
    })?;
    match response {
        AdminResponse::Ok { data, .. } => Ok(*data),
        AdminResponse::Error { error, .. } => {
            Err(format!("{:?}: {}", error.code, error.message).into())
        }
    }
}

fn print_json(data: AdminData) -> Result<(), Box<dyn Error>> {
    let stdout = io::stdout();
    let mut output = stdout.lock();
    serde_json::to_writer_pretty(&mut output, &data)?;
    output.write_all(b"\n")?;
    Ok(())
}
/// Pick the most recent deployment so `cygnus logs` (no id) does the obvious
/// thing instead of erroring. If nothing has shipped yet, print a hint.
fn latest_deployment_id(client: &AdminClient) -> Result<String, Box<dyn Error>> {
    let data = call(
        client,
        AdminCommand::ListDeployments {
            app: None,
            cursor: None,
            limit: 1,
        },
    )?;
    let AdminData::Deployments { deployments, .. } = data else {
        return Err("daemon returned an unexpected response to ListDeployments".into());
    };
    match deployments.into_iter().next() {
        Some(d) => Ok(d.id),
        None => Err(
            "no deployments yet — run `cygnus deploy` or pass an explicit <DEPLOYMENT> id".into(),
        ),
    }
}

fn stream_log(
    client: &AdminClient,
    deployment: Option<String>,
    stream: LogStream,
    mut offset: u64,
    follow: bool,
) -> Result<(), Box<dyn Error>> {
    let deployment = match deployment {
        Some(id) => id,
        None => latest_deployment_id(client)?,
    };
    let stdout = io::stdout();
    let mut output = stdout.lock();
    let mut quiet_polls: u32 = 0;
    loop {
        let data = call(
            client,
            AdminCommand::ReadLog {
                deployment: deployment.clone(),
                stream,
                offset,
                limit: MAX_LOG_CHUNK_BYTES,
            },
        )?;
        let AdminData::Log {
            next_offset,
            eof,
            data_base64,
            ..
        } = data
        else {
            return Err("daemon returned the wrong response to ReadLog".into());
        };
        let bytes = BASE64_STANDARD.decode(data_base64)?;
        output.write_all(&bytes)?;
        output.flush()?;
        if eof {
            if !follow {
                return Ok(());
            }
            // Drain phase: keep polling after eof until the stream is quiet for
            // a few consecutive polls, then stop. An eof with no new bytes
            // counts as one quiet poll.
            if bytes.is_empty() {
                quiet_polls += 1;
                if quiet_polls >= LOG_FOLLOW_QUIET_POLLS {
                    return Ok(());
                }
            } else {
                quiet_polls = 0;
            }
            thread::sleep(LOG_FOLLOW_POLL_INTERVAL);
            continue;
        }
        if next_offset <= offset {
            return Err("daemon returned a non-advancing log offset".into());
        }
        offset = next_offset;
    }
}

const LOG_FOLLOW_QUIET_POLLS: u32 = 3;
const LOG_FOLLOW_POLL_INTERVAL: Duration = Duration::from_millis(500);

fn stream_daemon_log(error: bool, follow: bool, lines: usize) -> Result<(), Box<dyn Error>> {
    // The CLI looks for a file log first (macOS user install, dev runs). On
    // a systemd-managed install the daemon writes to journald and there is
    // no file; fall back to `journalctl -u cygnus.service` so the same
    // command works in both worlds.
    let path = match resolve_daemon_log_path(error) {
        Ok(path) => path,
        Err(file_error) => return stream_daemon_journal(error, follow, lines, file_error),
    };
    let stdout = io::stdout();
    let mut output = stdout.lock();
    if follow {
        // Start at EOF so follow only streams new lines, then poll for appends.
        let mut file = std::fs::File::open(&path)
            .map_err(|err| format!("cannot open daemon log {}: {err}", path.display()))?;
        use std::io::Seek;
        file.seek(io::SeekFrom::End(0))?;
        let mut buf = [0_u8; 8192];
        loop {
            match std::io::Read::read(&mut file, &mut buf) {
                Ok(0) => thread::sleep(LOG_FOLLOW_POLL_INTERVAL),
                Ok(n) => {
                    output.write_all(&buf[..n])?;
                    output.flush()?;
                }
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(err) => return Err(err.into()),
            }
        }
    } else {
        let contents = std::fs::read_to_string(&path)
            .map_err(|err| format!("cannot read daemon log {}: {err}", path.display()))?;
        let selected = tail_lines(&contents, lines);
        output.write_all(selected.as_bytes())?;
        if !selected.is_empty() && !selected.ends_with('\n') {
            output.write_all(b"\n")?;
        }
        output.flush()?;
        Ok(())
    }
}

/// Fall back to `journalctl -u cygnus.service` for systemd installs. The
/// caller passes along the file-log error so the user gets a single
/// actionable message if journalctl is also unavailable.
fn stream_daemon_journal(
    error: bool,
    follow: bool,
    lines: usize,
    file_error: Box<dyn Error>,
) -> Result<(), Box<dyn Error>> {
    let unit = match std::env::var_os("CYGNUS_JOURNAL_UNIT") {
        Some(value) => value.to_string_lossy().into_owned(),
        None => "cygnus.service".to_owned(),
    };
    let journalctl = which_journalctl().ok_or_else(|| {
        format!(
            "{file_error}; journalctl is also unavailable — install systemd-journald or write a log file at ~/.cygnus/log/daemon.log"
        )
    })?;
    let mut command = std::process::Command::new(&journalctl);
    command.arg("-u").arg(&unit);
    if error {
        command.arg("-p").arg("err");
    }
    if !follow {
        command.arg("--no-pager");
        command.arg("-n").arg(format!("{lines}"));
    } else {
        command.arg("-f");
    }
    let status = command
        .status()
        .map_err(|err| format!("failed to spawn journalctl: {err}"))?;
    if !status.success() {
        return Err(format!(
            "journalctl -u {unit} exited with {status}; (file fallback: {file_error})"
        )
        .into());
    }
    Ok(())
}

fn which_journalctl() -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    for root in std::env::split_paths(&path) {
        let candidate = root.join("journalctl");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    // Common fallback when /usr/sbin (root-only) isn't on this user's PATH.
    for absolute in &[
        "/usr/bin/journalctl",
        "/bin/journalctl",
        "/usr/sbin/journalctl",
    ] {
        let path = std::path::PathBuf::from(absolute);
        if path.is_file() {
            return Some(path);
        }
    }
    None
}

fn resolve_daemon_log_path(error: bool) -> Result<PathBuf, Box<dyn Error>> {
    let names: &[&str] = if error {
        &["daemon.error.log"]
    } else {
        // launchd/user installs often write only to the error stream; prefer
        // stdout when it has content, otherwise fall back so `cygnus daemon-logs`
        // is useful without flags.
        &["daemon.log", "daemon.error.log"]
    };
    let mut roots = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        roots.push(PathBuf::from(home).join(".cygnus/log"));
    }
    roots.push(PathBuf::from("/var/log/cygnus"));
    roots.push(PathBuf::from("/var/log"));

    let mut seen_empty = None;
    for root in &roots {
        for name in names {
            let path = root.join(name);
            if !path.is_file() {
                continue;
            }
            let len = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            if len > 0 || error {
                return Ok(path);
            }
            if seen_empty.is_none() {
                seen_empty = Some(path);
            }
        }
    }
    if let Some(path) = seen_empty {
        return Ok(path);
    }
    Err("daemon log not found (looked under ~/.cygnus/log and /var/log/cygnus; try --error)".into())
}

fn tail_lines(contents: &str, lines: usize) -> &str {
    if lines == 0 || contents.is_empty() {
        return "";
    }
    let mut count = 0_usize;
    for (idx, ch) in contents.char_indices().rev() {
        if ch == '\n' {
            count += 1;
            if count > lines {
                return &contents[idx + ch.len_utf8()..];
            }
        }
    }
    contents
}

fn request_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{:032x}", nanos ^ u128::from(std::process::id()))
}

fn short_hash(hash: &str) -> String {
    if hash.len() <= 12 {
        hash.to_owned()
    } else {
        format!("{}…", &hash[..12])
    }
}

/// Color and presentation policy for the instrument panel. When stdout is not a
/// tty or NO_COLOR is set, every method renders plain text so the CLI stays
/// safe to pipe and script.
struct Theme {
    color: bool,
}

impl Theme {
    fn detect() -> Self {
        let color = io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none();
        Self { color }
    }

    fn paint<'a>(&self, value: &'a str) -> Painted<'a> {
        Painted {
            color: self.color,
            value,
        }
    }

    fn dot(&self, state: &str) -> String {
        let glyph = "●";
        if !self.color {
            return glyph.to_owned();
        }
        match state_color(state) {
            StateColor::Green => glyph.green().to_string(),
            StateColor::Yellow => glyph.yellow().to_string(),
            StateColor::Red => glyph.red().to_string(),
            StateColor::Dim => glyph.dimmed().to_string(),
            StateColor::Default => glyph.to_owned(),
        }
    }

    fn line_kv(&self, key: &str, value: &str) {
        let stdout = io::stdout();
        let mut out = stdout.lock();
        let _ = writeln!(
            out,
            "{}  {}",
            self.paint(key).dim(),
            self.paint(value).plain()
        );
    }

    fn health(&self, isolation: &str) {
        let stdout = io::stdout();
        let mut out = stdout.lock();
        let version = env!("CARGO_PKG_VERSION");
        let _ = writeln!(
            out,
            "{} cygnus {} · isolation: {}",
            self.dot("ready"),
            self.paint(version).blue(),
            self.paint(isolation).plain(),
        );
    }

    fn status(&self, node: &NodeView) {
        let stdout = io::stdout();
        let mut out = stdout.lock();
        let kv = |out: &mut std::io::StdoutLock<'_>, key: &str, value: &str| {
            let _ = writeln!(out, "  {:<12} {}", key, value);
        };
        let _ = writeln!(out, "{}", self.paint("node").dim());
        kv(
            &mut out,
            "version",
            &format!(
                "{} · up {}",
                node.version,
                format_uptime(node.uptime_seconds)
            ),
        );
        kv(&mut out, "isolation", &node.isolation);
        kv(&mut out, "edge", "0.0.0.0:80");
        kv(&mut out, "management", &node.listen);
        if let Some(https) = node.https_listen.as_deref() {
            kv(&mut out, "https", https);
        }
        if let Some(domain) = node.apps_domain.as_deref() {
            kv(&mut out, "apps domain", domain);
        }
        kv(
            &mut out,
            "apps",
            &format!("{} registered · {} warm", node.app_count, node.warm_count),
        );
        {
            let memory = &node.memory;
            kv(
                &mut out,
                "memory",
                &format!(
                    "{} available of {}",
                    format_bytes(memory.available_bytes),
                    format_bytes(memory.total_bytes)
                ),
            );
        }
        if !node.engines.is_empty() {
            let _ = writeln!(out);
            let _ = writeln!(out, "{}", self.paint("engines").dim());
            for engine in &node.engines {
                let mut facts = Vec::new();
                if engine.is_default {
                    facts.push("default".to_owned());
                }
                facts.push(format!(
                    "{} app{}",
                    engine.apps,
                    if engine.apps == 1 { "" } else { "s" }
                ));
                facts.push(format!("sha256 {}", short_hash(&engine.sha256)));
                let _ = writeln!(
                    out,
                    "  {} bun {}   {}",
                    self.dot("ready"),
                    self.paint(&engine.version).blue(),
                    self.paint(&facts.join(" · ")).dim()
                );
            }
        }
        if !node.certificates.is_empty() {
            let _ = writeln!(out);
            let _ = writeln!(out, "{}", self.paint("certificates").dim());
            for certificate in &node.certificates {
                let state = if certificate.ok { "ready" } else { "building" };
                let mut facts = vec![certificate.kind.clone()];
                if let Some(expires) = certificate.expires_unix {
                    facts.push(format!("expires {}", format_unix_date(expires)));
                }
                let _ = writeln!(
                    out,
                    "  {} {}   {}",
                    self.dot(state),
                    self.paint(&certificate.domain).plain(),
                    self.paint(&facts.join(" · ")).dim()
                );
            }
        }
    }

    fn apps(&self, apps: &[AppView], next_cursor: Option<&str>) {
        if apps.is_empty() {
            self.line_kv("apps", "none registered");
            return;
        }
        let rows: Vec<[String; 6]> = apps
            .iter()
            .map(|app| {
                [
                    app.name.clone(),
                    state_cell(self, &app.lifecycle_state),
                    app.domains.join(", "),
                    format!("egress {}", app.egress),
                    format_bytes(app.memory_max),
                    app.active
                        .as_ref()
                        .map(|active| short_hash(&active.artifact_hash))
                        .unwrap_or_else(|| "—".to_owned()),
                ]
            })
            .collect();
        let headers = ["NAME", "STATE", "DOMAINS", "POLICY", "MEMORY", "ARTIFACT"];
        print_table(self, &headers, &rows);
        if let Some(cursor) = next_cursor {
            let stdout = io::stdout();
            let mut out = stdout.lock();
            let _ = writeln!(
                out,
                "{}",
                self.paint(&format!("next: --cursor {cursor}")).dim()
            );
        }
    }

    fn app(&self, app: &AppView) {
        let stdout = io::stdout();
        let mut out = stdout.lock();
        let _ = writeln!(
            out,
            "{} {}",
            self.dot(&app.lifecycle_state),
            self.paint(&app.name).blue(),
        );
        write_kv(&mut out, self, "domains", &app.domains.join(", "));
        write_kv(&mut out, self, "state", &app.lifecycle_state);
        write_kv(&mut out, self, "egress", &app.egress);
        write_kv(&mut out, self, "memory", &format_bytes(app.memory_max));
        write_kv(
            &mut out,
            self,
            "pinned",
            if app.pinned { "yes" } else { "no" },
        );
        if let Some(active) = app.active.as_ref() {
            write_kv(&mut out, self, "deployment", &active.deployment_id);
            write_kv(
                &mut out,
                self,
                "artifact",
                &short_hash(&active.artifact_hash),
            );
            write_kv(&mut out, self, "engine", &active.engine_version);
        }
    }

    fn deployments(&self, deployments: &[DeploymentView], next_cursor: Option<&str>) {
        if deployments.is_empty() {
            self.line_kv("deployments", "none");
            return;
        }
        let rows: Vec<[String; 5]> = deployments
            .iter()
            .map(|deployment| {
                [
                    short_hash(&deployment.id),
                    deployment.app.clone(),
                    state_cell(self, &deployment.status),
                    deployment.engine_version.clone(),
                    deployment
                        .artifact_hash
                        .as_deref()
                        .map(short_hash)
                        .unwrap_or_else(|| "—".to_owned()),
                ]
            })
            .collect();
        let headers = ["ID", "APP", "STATUS", "ENGINE", "ARTIFACT"];
        print_table(self, &headers, &rows);
        if let Some(cursor) = next_cursor {
            let stdout = io::stdout();
            let mut out = stdout.lock();
            let _ = writeln!(
                out,
                "{}",
                self.paint(&format!("next: --cursor {cursor}")).dim()
            );
        }
    }

    fn deployment(&self, deployment: &DeploymentView) {
        let stdout = io::stdout();
        let mut out = stdout.lock();
        let _ = writeln!(
            out,
            "{} {}",
            self.dot(&deployment.status),
            self.paint(&deployment.id).blue(),
        );
        write_kv(&mut out, self, "app", &deployment.app);
        write_kv(&mut out, self, "status", &deployment.status);
        write_kv(&mut out, self, "engine", &deployment.engine_version);
        write_kv(
            &mut out,
            self,
            "source",
            &short_hash(&deployment.source_hash),
        );
        write_kv(
            &mut out,
            self,
            "artifact",
            &deployment
                .artifact_hash
                .as_deref()
                .map(short_hash)
                .unwrap_or_else(|| "—".to_owned()),
        );
        if let Some(error) = deployment.error.as_deref() {
            write_kv_red(&mut out, self, "error", error);
        }
    }

    fn mapped(&self, app: &str, domain: &str) {
        let stdout = io::stdout();
        let mut out = stdout.lock();
        let _ = writeln!(
            out,
            "mapped {} → {}",
            self.paint(domain).blue(),
            self.paint(app).plain(),
        );
    }

    fn activated(&self, app: &str, active: &ActiveDeploymentView) {
        let stdout = io::stdout();
        let mut out = stdout.lock();
        let _ = writeln!(out, "activated {}", self.paint(app).blue());
        write_kv(&mut out, self, "deployment", &active.deployment_id);
        write_kv(
            &mut out,
            self,
            "artifact",
            &short_hash(&active.artifact_hash),
        );
        write_kv(&mut out, self, "engine", &active.engine_version);
    }
}

fn state_cell(theme: &Theme, state: &str) -> String {
    format!("{} {}", theme.dot(state), state)
}

fn state_color(state: &str) -> StateColor {
    match state {
        "ready" | "active" => StateColor::Green,
        "cold" | "sealed" | "draining" | "booting" | "unknown" => StateColor::Dim,
        "building" | "pending" => StateColor::Yellow,
        "failed" | "crashloop" => StateColor::Red,
        _ => StateColor::Default,
    }
}

enum StateColor {
    Green,
    Yellow,
    Red,
    Dim,
    Default,
}

fn write_kv(out: &mut impl Write, theme: &Theme, key: &str, value: &str) {
    let _ = writeln!(
        out,
        "  {}  {}",
        theme.paint(key).dim(),
        theme.paint(value).plain()
    );
}

fn write_kv_red(out: &mut impl Write, theme: &Theme, key: &str, value: &str) {
    let _ = writeln!(
        out,
        "  {}  {}",
        theme.paint(key).dim(),
        theme.paint(value).red(),
    );
}

/// Hand-rolled column alignment: dim uppercase header row, two-space gutters,
/// no borders. ANSI escapes in cells are ignored when measuring widths so the
/// colored state dots stay aligned.
fn print_table<const N: usize>(theme: &Theme, headers: &[&str; N], rows: &[[String; N]]) {
    let mut widths = [0_usize; N];
    for (i, header) in headers.iter().enumerate() {
        widths[i] = header.len();
    }
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            let visible = visible_len(cell);
            if visible > widths[i] {
                widths[i] = visible;
            }
        }
    }

    let stdout = io::stdout();
    let mut out = stdout.lock();
    let mut header_line = String::new();
    for (i, header) in headers.iter().enumerate() {
        if i > 0 {
            header_line.push_str("  ");
        }
        let padded = pad_to(header, widths[i]);
        header_line.push_str(&theme.paint(&padded).dim());
    }
    let _ = writeln!(out, "{header_line}");

    for row in rows {
        let mut line = String::new();
        for (i, cell) in row.iter().enumerate() {
            if i > 0 {
                line.push_str("  ");
            }
            let visible = visible_len(cell);
            let padding = widths[i].saturating_sub(visible);
            line.push_str(cell);
            if padding > 0 {
                line.push_str(&" ".repeat(padding));
            }
        }
        let _ = writeln!(out, "{line}");
    }
}

fn pad_to(value: &str, width: usize) -> String {
    let len = visible_len(value);
    if len >= width {
        value.to_owned()
    } else {
        format!("{value}{}", " ".repeat(width - len))
    }
}

/// Visible width of a string: characters counted with ANSI CSI escape
/// sequences removed, so colored and multi-byte cells align identically.
fn visible_len(value: &str) -> usize {
    let mut len = 0;
    let mut chars = value.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' && chars.peek() == Some(&'[') {
            chars.next();
            for skip in chars.by_ref() {
                if ('\x40'..='\x7e').contains(&skip) {
                    break;
                }
            }
        } else {
            len += 1;
        }
    }
    len
}

fn format_uptime(seconds: u64) -> String {
    let days = seconds / 86_400;
    let hours = (seconds % 86_400) / 3_600;
    let minutes = (seconds % 3_600) / 60;
    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {minutes}m")
    } else if minutes > 0 {
        format!("{minutes}m")
    } else {
        format!("{seconds}s")
    }
}

fn format_unix_date(unix: i64) -> String {
    // Render as days-from-now: certificates care about lead time, not dates.
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let delta_days = (unix - now) / 86_400;
    if delta_days < 0 {
        "expired".to_owned()
    } else if delta_days == 0 {
        "today".to_owned()
    } else {
        format!("in {delta_days}d")
    }
}

fn format_bytes(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = KIB * 1024;
    const GIB: u64 = MIB * 1024;
    if bytes >= GIB {
        format!("{:.1} GiB", bytes as f64 / GIB as f64)
    } else if bytes >= MIB {
        format!("{:.1} MiB", bytes as f64 / MIB as f64)
    } else if bytes >= KIB {
        format!("{:.1} KiB", bytes as f64 / KIB as f64)
    } else {
        format!("{} B", bytes)
    }
}

/// Render the deploy command. Isolated so a follow-up branch can swap the
/// single spinner for live streamed build logs without touching the command.
/// The deploy experience: start the build asynchronously, stream the server
/// side build log to the terminal as it happens, then print the outcome.
fn render_deploy(
    theme: &Theme,
    client: &AdminClient,
    request: DeployRequest,
) -> Result<(), Box<dyn Error>> {
    let app = request.app.clone();
    let requested_domain = request.domain.clone();
    let started = Instant::now();

    let data = call(client, AdminCommand::DeployStart { request })?;
    let AdminData::DeployStarted { deployment_id, .. } = data else {
        return Err("daemon returned an unexpected response to DeployStart".into());
    };

    {
        let stdout = io::stdout();
        let mut out = stdout.lock();
        let _ = writeln!(
            out,
            "{} {} {}",
            self_paint(theme, "building", true),
            theme.paint(&app).blue(),
            theme.paint(&format!("· deployment {deployment_id}")).dim()
        );
    }

    let spinner = deploy_spinner(theme, &app);
    let mut offset = 0_u64;
    let mut printed_any = false;
    let deployment = loop {
        // Pull whatever new build output exists and print it raw.
        let log = call(
            client,
            AdminCommand::ReadLog {
                deployment: deployment_id.clone(),
                stream: LogStream::Stdout,
                offset,
                limit: MAX_LOG_CHUNK_BYTES,
            },
        );
        if let Ok(AdminData::Log {
            next_offset,
            data_base64,
            ..
        }) = log
        {
            let bytes = BASE64_STANDARD.decode(data_base64)?;
            if !bytes.is_empty() {
                if !printed_any {
                    spinner.finish_and_clear();
                    printed_any = true;
                }
                spinner.suspend(|| {
                    let stdout = io::stdout();
                    let mut out = stdout.lock();
                    let _ = out.write_all(&bytes);
                    let _ = out.flush();
                });
            }
            offset = next_offset;
        }

        let data = call(
            client,
            AdminCommand::GetDeployment {
                deployment: deployment_id.clone(),
            },
        )?;
        let AdminData::Deployment { deployment } = data else {
            return Err("daemon returned an unexpected response to GetDeployment".into());
        };
        // Sealed is intermediate: the build finished but activation may still
        // be running (or may fail). Keep polling until active or failed.
        match deployment.status.as_str() {
            "building" | "sealed" => {}
            _ => break deployment,
        }
        thread::sleep(Duration::from_millis(400));
    };
    spinner.finish_and_clear();
    let elapsed = started.elapsed();

    match deployment.status.as_str() {
        "active" => {
            let stdout = io::stdout();
            let mut out = stdout.lock();
            let _ = writeln!(
                out,
                "{}{}",
                self_paint(theme, "deployed ", false),
                theme.paint(&app).blue()
            );
            write_kv(&mut out, theme, "deployment", &deployment.id);
            if let Some(artifact) = deployment.artifact_hash.as_deref() {
                write_kv(&mut out, theme, "artifact", &short_hash(artifact));
            }
            write_kv(&mut out, theme, "engine", &deployment.engine_version);
            let status = call(client, AdminCommand::Status).ok();
            let domain = requested_domain.or_else(|| match &status {
                Some(AdminData::Status { node }) => node
                    .apps_domain
                    .as_ref()
                    .map(|apps_domain| format!("{app}.{apps_domain}")),
                _ => None,
            });
            if let Some(domain) = domain {
                // Prefer https when the node is listening for it; otherwise
                // local/dev installs only have plain HTTP (apps.localhost).
                let scheme = match &status {
                    Some(AdminData::Status { node }) if node.https_listen.is_some() => "https",
                    _ => "http",
                };
                write_kv(&mut out, theme, "url", &format!("{scheme}://{domain}"));
            }
            write_kv(
                &mut out,
                theme,
                "time",
                &format!("{:.1} s", elapsed.as_secs_f64()),
            );
            Ok(())
        }
        _ => {
            // Show the build's stderr tail before the final error: that is
            // where compilers and installers explain themselves.
            if let Ok(AdminData::Log { data_base64, .. }) = call(
                client,
                AdminCommand::ReadLog {
                    deployment: deployment_id.clone(),
                    stream: LogStream::Stderr,
                    offset: 0,
                    limit: MAX_LOG_CHUNK_BYTES,
                },
            ) {
                let bytes = BASE64_STANDARD.decode(data_base64)?;
                if !bytes.is_empty() {
                    let stdout = io::stdout();
                    let mut out = stdout.lock();
                    let _ = writeln!(out, "{}", theme.paint("build stderr:").dim());
                    let _ = out.write_all(&bytes);
                    if !bytes.ends_with(b"\n") {
                        let _ = writeln!(out);
                    }
                    let _ = out.flush();
                }
            }
            let status = deployment.status.clone();
            let message = deployment
                .error
                .unwrap_or_else(|| format!("deploy ended in {status}"));
            Err(format!("deploy failed: {message}").into())
        }
    }
}

fn self_paint(theme: &Theme, text: &str, dim: bool) -> String {
    if !theme.color {
        return text.to_owned();
    }
    if dim {
        text.dimmed().to_string()
    } else {
        text.bold().to_string()
    }
}

fn deploy_spinner(theme: &Theme, app: &str) -> ProgressBar {
    let bar = ProgressBar::new_spinner();
    let template = "{spinner} {msg} {elapsed}";
    if theme.color {
        bar.set_style(
            ProgressStyle::with_template(template)
                .unwrap()
                .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏"),
        );
    } else {
        bar.set_style(
            ProgressStyle::with_template(template)
                .unwrap()
                .tick_chars("-\\|/"),
        );
    }
    bar.set_message(format!("building {app}"));
    bar.enable_steady_tick(Duration::from_millis(80));
    bar
}

/// clap 4 style sheet: a single blue accent for headers/identifiers.
fn cygnus_styles() -> clap::builder::Styles {
    use clap::builder::styling::AnsiColor;
    clap::builder::Styles::styled()
        .header(AnsiColor::Blue.on_default())
        .literal(AnsiColor::Blue.on_default())
}

/// A tiny wrapper for conditional coloring. Methods return the value (colored
/// or plain) so callers can inline without branching on `Theme::color`.
struct Painted<'a> {
    color: bool,
    value: &'a str,
}

impl<'a> Painted<'a> {
    fn dim(&self) -> String {
        if self.color {
            self.value.dimmed().to_string()
        } else {
            self.value.to_owned()
        }
    }

    fn plain(&self) -> String {
        self.value.to_owned()
    }

    fn blue(&self) -> String {
        if self.color {
            self.value.blue().to_string()
        } else {
            self.value.to_owned()
        }
    }

    fn red(&self) -> String {
        if self.color {
            self.value.red().to_string()
        } else {
            self.value.to_owned()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_ids_match_the_wire_contract() {
        let id = request_id();
        assert_eq!(id.len(), 32);
        assert!(
            id.bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        );
    }

    #[test]
    fn documented_read_commands_parse() {
        assert!(matches!(
            Cli::try_parse_from(["cygnus", "status"]).unwrap().command,
            Command::Status { .. }
        ));
        assert!(matches!(
            Cli::try_parse_from(["cygnus", "logs", "dep-1", "--stream", "stderr"])
                .unwrap()
                .command,
            Command::Logs {
                stream: StreamArg::Stderr,
                ..
            }
        ));
        assert!(matches!(
            Cli::try_parse_from([
                "cygnus",
                "rollback",
                "api",
                "dep-1",
                "--expected-active-artifact",
                &"a".repeat(64),
            ])
            .unwrap()
            .command,
            Command::Rollback { .. }
        ));
    }

    #[test]
    fn read_commands_accept_json_flag() {
        assert!(matches!(
            Cli::try_parse_from(["cygnus", "apps", "--json"])
                .unwrap()
                .command,
            Command::Apps { json: true, .. }
        ));
        assert!(matches!(
            Cli::try_parse_from(["cygnus", "status", "--json"])
                .unwrap()
                .command,
            Command::Status { json: true }
        ));
        assert!(matches!(
            Cli::try_parse_from(["cygnus", "deployment", "dep-1", "--json"])
                .unwrap()
                .command,
            Command::Deployment { json: true, .. }
        ));
    }

    #[test]
    fn logs_follow_flag_parses() {
        assert!(matches!(
            Cli::try_parse_from(["cygnus", "logs", "dep-1", "--follow"])
                .unwrap()
                .command,
            Command::Logs { follow: true, .. }
        ));
    }

    #[test]
    fn logs_without_deployment_arg_parses() {
        let parsed = Cli::try_parse_from(["cygnus", "logs"]).unwrap();
        match parsed.command {
            Command::Logs {
                deployment, follow, ..
            } => {
                assert!(deployment.is_none());
                assert!(!follow);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn deploy_defaults_source_and_app_from_cwd_basename() {
        let parsed = Cli::try_parse_from(["cygnus", "deploy"]).unwrap();
        let Command::Deploy {
            source_dir,
            source,
            app,
            ..
        } = parsed.command
        else {
            panic!("expected deploy");
        };
        assert!(source_dir.is_none());
        assert!(source.is_none());
        assert!(app.is_none());

        let dir = std::env::temp_dir().join(format!("cygnus-deploy-cli-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let resolved = resolve_deploy_source(Some(dir.clone())).unwrap();
        assert_eq!(resolved, dir);
        let inferred = resolve_deploy_app(None, &resolved).unwrap();
        assert_eq!(
            inferred,
            sanitize_app_name(dir.file_name().unwrap().to_str().unwrap())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn deploy_accepts_positional_source() {
        let parsed = Cli::try_parse_from(["cygnus", "deploy", ".", "--app", "demo"]).unwrap();
        match parsed.command {
            Command::Deploy {
                source: Some(path),
                app: Some(name),
                ..
            } => {
                assert_eq!(path, PathBuf::from("."));
                assert_eq!(name, "demo");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn sanitize_app_name_is_dns_label_ish() {
        assert_eq!(sanitize_app_name("Draco UI Design"), "draco-ui-design");
        assert_eq!(sanitize_app_name("---Hello---"), "hello");
        assert_eq!(sanitize_app_name("a".repeat(80).as_str()).len(), 63);
    }

    #[test]
    fn daemon_logs_command_parses() {
        assert!(matches!(
            Cli::try_parse_from(["cygnus", "daemon-logs", "--error", "-f", "--lines", "50"])
                .unwrap()
                .command,
            Command::DaemonLogs {
                error: true,
                follow: true,
                lines: 50,
            }
        ));
    }

    #[test]
    fn tail_lines_keeps_last_n() {
        let text = "a\nb\nc\nd\n";
        assert_eq!(tail_lines(text, 2), "c\nd\n");
        assert_eq!(tail_lines(text, 10), text);
        assert_eq!(tail_lines(text, 0), "");
    }

    #[test]
    fn short_hash_truncates_long_values() {
        assert_eq!(short_hash("abcdef1234567890"), "abcdef123456…");
        assert_eq!(short_hash("short"), "short");
    }

    #[test]
    fn visible_len_ignores_ansi_escapes() {
        let plain = "ready";
        let colored = plain.green().to_string();
        assert_eq!(visible_len(&colored), plain.len());
        // The dot is one visible column but three bytes: visible width is
        // what tables align on.
        assert_eq!(visible_len("● ready"), 7);
    }

    #[test]
    fn format_bytes_humanizes() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(2048), "2.0 KiB");
        assert_eq!(format_bytes(1024 * 1024 * 5), "5.0 MiB");
    }

    #[test]
    fn plain_theme_dot_has_no_escape() {
        let theme = Theme { color: false };
        assert_eq!(theme.dot("ready"), "●");
    }

    #[test]
    fn state_color_maps_known_states() {
        assert!(matches!(state_color("ready"), StateColor::Green));
        assert!(matches!(state_color("active"), StateColor::Green));
        assert!(matches!(state_color("failed"), StateColor::Red));
        assert!(matches!(state_color("crashloop"), StateColor::Red));
        assert!(matches!(state_color("building"), StateColor::Yellow));
        assert!(matches!(state_color("cold"), StateColor::Dim));
        assert!(matches!(state_color("sealed"), StateColor::Dim));
        assert!(matches!(state_color("custom"), StateColor::Default));
    }
}
