use std::error::Error;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use clap::{Parser, Subcommand};
use cygnus_daemon::admin::{
    ADMIN_PROTOCOL_VERSION, AdminClient, AdminCommand, AdminData, AdminRequest, AdminResponse,
    DEFAULT_HOST_ADMIN_SOCKET, LogStream, MAX_LOG_CHUNK_BYTES,
};
use cygnus_daemon::deploy::DeployRequest;
use cygnus_daemon::state::NodeConfig;

#[derive(Debug, Parser)]
#[command(name = "cygnusctl", about = "Operate the local Cygnus daemon")]
struct Cli {
    /// Root-only daemon administration socket.
    #[arg(long, global = true, default_value = DEFAULT_HOST_ADMIN_SOCKET)]
    admin_socket: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Apply {
        config: PathBuf,
    },
    Engine {
        #[command(subcommand)]
        command: EngineCommand,
    },
    Deploy {
        #[arg(long = "source-dir", alias = "source")]
        source_dir: PathBuf,
        #[arg(long)]
        app: String,
        #[arg(long)]
        domain: String,
        #[arg(long)]
        engine_version: String,
        #[arg(long, default_value = "index.ts")]
        entry: PathBuf,
        #[arg(long)]
        artifact_root: PathBuf,
        #[arg(long)]
        upstream: PathBuf,
    },
    /// Check protocol and daemon availability.
    Health,
    /// Show node configuration status.
    Status,
    /// List one page of registered apps.
    Apps {
        #[arg(long)]
        cursor: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: u16,
    },
    /// Show one registered app.
    App {
        app: String,
    },
    /// List one page of deployments.
    Deployments {
        #[arg(long)]
        app: Option<String>,
        #[arg(long)]
        cursor: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: u16,
    },
    /// Show one deployment.
    Deployment {
        deployment: String,
    },
    /// Add a hostname route to an app.
    MapDomain {
        app: String,
        domain: String,
    },
    /// Atomically activate a retained sealed deployment.
    Rollback {
        app: String,
        deployment: String,
        #[arg(long)]
        expected_active_artifact: String,
    },
    /// Write a deployment build log to stdout.
    Logs {
        deployment: String,
        #[arg(long, value_enum, default_value_t = StreamArg::Stdout)]
        stream: StreamArg,
        #[arg(long, default_value_t = 0)]
        offset: u64,
    },
}
#[derive(Debug, Subcommand)]
enum EngineCommand {
    Register {
        #[arg(long)]
        version: String,
        #[arg(long)]
        host_root: PathBuf,
        #[arg(long)]
        cage_executable: PathBuf,
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
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("cygnusctl: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<(), Box<dyn Error>> {
    let client = AdminClient::new(cli.admin_socket);
    match cli.command {
        Command::Apply { config } => {
            let config: NodeConfig = serde_json::from_slice(&std::fs::read(config)?)?;
            print_data(call(&client, AdminCommand::ApplyConfig(config))?)
        }
        Command::Engine { command } => {
            let EngineCommand::Register {
                version,
                host_root,
                cage_executable,
            } = command;
            print_data(call(
                &client,
                AdminCommand::RegisterEngine {
                    version,
                    host_root,
                    cage_executable,
                    is_default: false,
                },
            )?)
        }
        Command::Deploy {
            source_dir,
            app,
            domain,
            engine_version,
            entry,
            artifact_root,
            upstream,
        } => print_data(call(
            &client,
            AdminCommand::Deploy {
                request: DeployRequest::new(
                    source_dir,
                    app,
                    domain,
                    engine_version,
                    entry,
                    artifact_root,
                    upstream,
                ),
            },
        )?),
        Command::Health => print_data(call(&client, AdminCommand::Health)?),
        Command::Status => print_data(call(&client, AdminCommand::Status)?),
        Command::Apps { cursor, limit } => {
            print_data(call(&client, AdminCommand::ListApps { cursor, limit })?)
        }
        Command::App { app } => print_data(call(&client, AdminCommand::GetApp { app })?),
        Command::Deployments { app, cursor, limit } => print_data(call(
            &client,
            AdminCommand::ListDeployments { app, cursor, limit },
        )?),
        Command::Deployment { deployment } => {
            print_data(call(&client, AdminCommand::GetDeployment { deployment })?)
        }
        Command::MapDomain { app, domain } => {
            print_data(call(&client, AdminCommand::MapDomain { app, domain })?)
        }
        Command::Rollback {
            app,
            deployment,
            expected_active_artifact,
        } => print_data(call(
            &client,
            AdminCommand::Rollback {
                app,
                deployment,
                expected_active_artifact,
            },
        )?),
        Command::Logs {
            deployment,
            stream,
            offset,
        } => stream_log(&client, deployment, stream.into(), offset),
    }
}

fn call(client: &AdminClient, command: AdminCommand) -> Result<AdminData, Box<dyn Error>> {
    let request = AdminRequest {
        version: ADMIN_PROTOCOL_VERSION,
        request_id: request_id(),
        actor: None,
        command,
    };
    match client.request(&request)? {
        AdminResponse::Ok { data, .. } => Ok(*data),
        AdminResponse::Error { error, .. } => {
            Err(format!("{:?}: {}", error.code, error.message).into())
        }
    }
}

fn print_data(data: AdminData) -> Result<(), Box<dyn Error>> {
    let stdout = io::stdout();
    let mut output = stdout.lock();
    serde_json::to_writer_pretty(&mut output, &data)?;
    output.write_all(b"\n")?;
    Ok(())
}

fn stream_log(
    client: &AdminClient,
    deployment: String,
    stream: LogStream,
    mut offset: u64,
) -> Result<(), Box<dyn Error>> {
    let stdout = io::stdout();
    let mut output = stdout.lock();
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
        if eof {
            return Ok(());
        }
        if next_offset <= offset {
            return Err("daemon returned a non-advancing log offset".into());
        }
        offset = next_offset;
    }
}

fn request_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{:032x}", nanos ^ u128::from(std::process::id()))
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
            Cli::try_parse_from(["cygnusctl", "status"])
                .unwrap()
                .command,
            Command::Status
        ));
        assert!(matches!(
            Cli::try_parse_from(["cygnusctl", "logs", "dep-1", "--stream", "stderr"])
                .unwrap()
                .command,
            Command::Logs {
                stream: StreamArg::Stderr,
                ..
            }
        ));
        assert!(matches!(
            Cli::try_parse_from([
                "cygnusctl",
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
}
