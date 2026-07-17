//! Source intake, finite Bun builds, and deployment activation.
//!
//! Deployments intentionally copy source into a daemon-owned workspace before
//! beginning any durable deployment. The build cage never receives a caller
//! path, and the only host path it can publish is the bounded output mount.

mod publish;
pub mod upload;

use std::collections::BTreeMap;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::ffi::CString;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
#[cfg(target_os = "linux")]
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

use cygnus_cage::{
    BuildOutputSpec, CageError, DomainEgressRule, EgressMode, FilterMode, JobCompletion, JobConfig,
    JobExitOutcome, RootfsSpec, run_job_streaming,
};
#[cfg(target_os = "linux")]
use cygnus_cage::{DnsForwarder, run_job_streaming_with_dns};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::state::{
    AppConfig, ArtifactInput, AuditContext, AuditEndpointRole, DeploymentInput, DeploymentRecord,
    EngineRecord, LoadedApp, RootfsConfig, SeccompMode, State, StateError,
};
use publish::PublishDir;

const WORKSPACE_REL: &str = ".work";
const BUILD_OUTPUT_PREFIX: &str = ".building-";
const FAILED_REL: &str = "failed";
const SHIM_REL: &str = "cygnus/shim.js";
const BUILD_RUNNER_REL: &str = "cygnus/build-runner.js";
const BUILD_CONFIG_REL: &str = "cygnus/build.bunfig.toml";
const BUILD_RUNNER_CAGE_PATH: &str = "/cygnus/build-runner.js";
const BUILD_CONFIG_CAGE_PATH: &str = "/cygnus/build.bunfig.toml";
const BUILD_WORKDIR_CAGE_PATH: &str = "/cygnus";
const BUILD_CACHE_CAGE_PATH: &str = "/workspace/.cygnus-cache";
const BUILD_HOME_CAGE_PATH: &str = "/cygnus/home";
const BUILD_TMPDIR_CAGE_PATH: &str = "/cygnus/tmp";
const BUILD_PATH: &str = "/usr/local/bin:/usr/bin:/bin";
const INIT_CAGE_PATH: &str = "/usr/local/bin/cygnus-init";
const BUILD_REGISTRY: &str = "https://registry.npmjs.org";
const BUILD_REGISTRY_DOMAIN: &str = "registry.npmjs.org";
const BUILD_ROOTFS_TMPFS_SIZE: u64 = 512 * 1024 * 1024;
const BUILD_INSTALL_MEMORY_MAX: u64 = 512 * 1024 * 1024;
const BUILD_INSTALL_MEMORY_HIGH: u64 = 448 * 1024 * 1024;
const BUILD_INSTALL_PIDS_MAX: u32 = 512;
const MAX_PACKAGE_JSON_BYTES: u64 = 1024 * 1024;
const MAX_BUN_LOCK_BYTES: u64 = 16 * 1024 * 1024;
const MAX_BUILD_OUTPUT: usize = 4 * 1024 * 1024;
const LOG_REL: &str = "logs";
const MAX_ARTIFACT_BYTES: u64 = 256 * 1024 * 1024;
const MAX_ARTIFACT_INODES: u64 = 8_192;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct BuildPlan {
    install: bool,
}
/// Inputs to one source deployment.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DeployRequest {
    pub source_dir: PathBuf,
    pub app: String,
    pub domain: String,
    pub engine_version: String,
    pub entry: PathBuf,
    pub artifact_root: PathBuf,
    pub upstream: PathBuf,
}

impl DeployRequest {
    pub fn new(
        source_dir: impl Into<PathBuf>,
        app: impl Into<String>,
        domain: impl Into<String>,
        engine_version: impl Into<String>,
        entry: impl Into<PathBuf>,
        artifact_root: impl Into<PathBuf>,
        upstream: impl Into<PathBuf>,
    ) -> Self {
        Self {
            source_dir: source_dir.into(),
            app: app.into(),
            domain: domain.into(),
            engine_version: engine_version.into(),
            entry: entry.into(),
            artifact_root: artifact_root.into(),
            upstream: upstream.into(),
        }
    }
}

/// Result of a successful deployment.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeployResult {
    pub deployment_id: String,
    pub source_hash: String,
    pub artifact_hash: String,
    pub artifact_path: PathBuf,
    pub deployment: DeploymentRecord,
}
pub struct ActivationPreparation {
    cleanup: Option<Box<dyn FnOnce() + Send>>,
}

impl ActivationPreparation {
    pub fn new(cleanup: impl FnOnce() + Send + 'static) -> Self {
        Self {
            cleanup: Some(Box::new(cleanup)),
        }
    }

    fn committed(mut self) {
        self.cleanup = None;
    }
}

impl Drop for ActivationPreparation {
    fn drop(&mut self) {
        if let Some(cleanup) = self.cleanup.take() {
            cleanup();
        }
    }
}

#[derive(Debug, Error)]
pub enum DeployError {
    #[error("deployment input is invalid: {0}")]
    InvalidInput(String),
    #[error("deployment filesystem error: {0}")]
    Io(#[from] io::Error),
    #[error("deployment state error: {0}")]
    State(#[from] StateError),
    #[error("deployment JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("deployment {id:?} failed: {detail} (captured logs: {})", logs.display())]
    BuildFailed {
        id: String,
        detail: String,
        logs: PathBuf,
    },
    #[error("deployment {id:?} reached sealed state but could not activate: {detail}")]
    ActivationFailed { id: String, detail: String },
}

/// Register an engine after hashing the executable visible under `host_root`.
/// The cage path remains the absolute path recorded in the engine registry.
pub fn register_engine(
    state: &mut State,
    version: impl Into<String>,
    host_root: impl AsRef<Path>,
    cage_executable: impl AsRef<Path>,
) -> Result<EngineRecord, DeployError> {
    let version = version.into();
    let host_root = fs::canonicalize(host_root.as_ref()).map_err(|error| {
        DeployError::InvalidInput(format!(
            "engine host root {} is unavailable: {error}",
            host_root.as_ref().display()
        ))
    })?;
    let cage_executable = cage_executable.as_ref().to_path_buf();
    if !cage_executable.is_absolute()
        || cage_executable.components().any(|component| {
            matches!(
                component,
                Component::CurDir | Component::ParentDir | Component::Prefix(_)
            )
        })
    {
        return Err(DeployError::InvalidInput(
            "engine cage executable must be an absolute canonical path".into(),
        ));
    }
    let executable = engine_executable_path(&host_root, &cage_executable)?;
    let record = EngineRecord {
        version,
        host_root,
        cage_executable,
        sha256: sha256_file(&executable)?,
        is_default: false,
    };
    Ok(state.register_engine(&record)?)
}

pub fn register_engine_with_audit(
    state: &mut State,
    version: impl Into<String>,
    host_root: impl AsRef<Path>,
    cage_executable: impl AsRef<Path>,
    is_default: bool,
    audit: &AuditContext,
) -> Result<EngineRecord, DeployError> {
    let version = version.into();
    let host_root = fs::canonicalize(host_root.as_ref()).map_err(|error| {
        DeployError::InvalidInput(format!(
            "engine host root {} is unavailable: {error}",
            host_root.as_ref().display()
        ))
    })?;
    let cage_executable = cage_executable.as_ref().to_path_buf();
    if !cage_executable.is_absolute()
        || cage_executable.components().any(|component| {
            matches!(
                component,
                Component::CurDir | Component::ParentDir | Component::Prefix(_)
            )
        })
    {
        return Err(DeployError::InvalidInput(
            "engine cage executable must be an absolute canonical path".into(),
        ));
    }
    let executable = engine_executable_path(&host_root, &cage_executable)?;
    let record = EngineRecord {
        version,
        host_root,
        cage_executable,
        sha256: sha256_file(&executable)?,
        is_default,
    };
    Ok(state.register_engine_with_audit(&record, audit)?)
}

fn engine_executable_path(
    host_root: &Path,
    cage_executable: &Path,
) -> Result<PathBuf, DeployError> {
    let relative = cage_executable
        .strip_prefix("/")
        .map_err(|_| DeployError::InvalidInput("engine cage executable must be absolute".into()))?;
    let executable = host_root.join(relative);
    let canonical = fs::canonicalize(&executable).map_err(|error| {
        DeployError::InvalidInput(format!(
            "engine executable {} is unavailable: {error}",
            executable.display()
        ))
    })?;
    if canonical != executable || !canonical.starts_with(host_root) {
        return Err(DeployError::InvalidInput(
            "engine executable must not traverse a symlink or escape host root".into(),
        ));
    }
    let metadata = fs::symlink_metadata(&executable)?;
    if !metadata.file_type().is_file() || metadata.permissions().mode() & 0o111 == 0 {
        return Err(DeployError::InvalidInput(format!(
            "engine executable {} must be a regular executable file",
            executable.display()
        )));
    }
    Ok(executable)
}

fn verify_engine(engine: &EngineRecord) -> Result<(), DeployError> {
    let executable = engine_executable_path(&engine.host_root, &engine.cage_executable)?;
    let actual = sha256_file(&executable)?;
    if actual != engine.sha256 {
        return Err(DeployError::InvalidInput(format!(
            "registered engine {:?} changed on disk (expected {}, found {actual})",
            engine.version, engine.sha256
        )));
    }
    Ok(())
}

/// Intake source, build it with the registered engine, seal the artifact, and
/// atomically activate a new or replacement app in the state database.
pub fn deploy(state: &mut State, request: DeployRequest) -> Result<DeployResult, DeployError> {
    let audit = AuditContext {
        endpoint_role: AuditEndpointRole::Host,
        peer_uid: None,
        peer_gid: None,
        peer_pid: None,
        actor_subject: None,
        request_id: new_deployment_id(),
        command_kind: "deploy".into(),
        request_digest: deploy_request_digest(&request),
    };
    deploy_with_audit(state, request, &audit)
}

/// Build and activate a source deployment with caller-supplied audit provenance.
pub fn deploy_with_audit(
    state: &mut State,
    request: DeployRequest,
    audit: &AuditContext,
) -> Result<DeployResult, DeployError> {
    deploy_with_audit_and_prepare(state, request, audit, |_| {
        Ok(ActivationPreparation::new(|| {}))
    })
}

pub fn deploy_with_audit_and_prepare<F>(
    state: &mut State,
    request: DeployRequest,
    audit: &AuditContext,
    mut prepare: F,
) -> Result<DeployResult, DeployError>
where
    F: FnMut(&LoadedApp) -> Result<ActivationPreparation, DeployError>,
{
    validate_entry(&request.entry)?;
    if request.app.trim().is_empty() || request.domain.trim().is_empty() {
        return Err(DeployError::InvalidInput(
            "app and domain must be nonempty".into(),
        ));
    }
    validate_upstream(&request.upstream)?;
    let expected_active_artifact = state
        .active_deployment(&request.app)?
        .map(|active| active.artifact_hash);
    let engine = state.engine(&request.engine_version)?.ok_or_else(|| {
        DeployError::InvalidInput(format!(
            "engine {:?} is not registered",
            request.engine_version
        ))
    })?;
    verify_engine(&engine)?;
    let source_root = canonical_source_root(&request.source_dir)?;
    let artifact_root = prepare_artifact_root(&request.artifact_root)?;
    let entry = request.entry.clone();
    let deployment_id = new_deployment_id();
    let workspace = artifact_root
        .join(WORKSPACE_REL)
        .join(&deployment_id)
        .join("rootfs/workspace");
    fs::create_dir_all(&workspace)?;
    let source_hash = match copy_source(&source_root, &workspace) {
        Ok(hash) => hash,
        Err(error) => {
            let _ = remove_work(&artifact_root, &deployment_id);
            return Err(error);
        }
    };
    let build_plan = match preflight_workspace(&workspace) {
        Ok(plan) => plan,
        Err(error) => {
            let _ = remove_work(&artifact_root, &deployment_id);
            return Err(error);
        }
    };
    let rootfs = workspace.parent().unwrap_or(&workspace);
    if let Err(error) = stage_build_controls(rootfs) {
        let _ = remove_work(&artifact_root, &deployment_id);
        return Err(error);
    };

    let input = DeploymentInput {
        id: deployment_id.clone(),
        app: request.app.clone(),
        source_hash: source_hash.clone(),
        engine_version: request.engine_version.clone(),
    };
    if let Err(error) = state.begin_deployment(&input) {
        let _ = remove_work(&artifact_root, &deployment_id);
        return Err(error.into());
    }

    let building = artifact_root.join(format!("{BUILD_OUTPUT_PREFIX}{deployment_id}"));
    let log_path = artifact_root.join(LOG_REL).join(&deployment_id);
    let (stdout_log, stderr_log) = match prepare_live_logs(&artifact_root, &deployment_id) {
        Ok(logs) => logs,
        Err(error) => {
            let detail = error.to_string();
            return Err(fail_build(
                state,
                &artifact_root,
                &building,
                &log_path,
                &deployment_id,
                detail,
            ));
        }
    };
    if let Err(error) = state.set_deployment_log_path(&deployment_id, &log_path) {
        return Err(fail_build(
            state,
            &artifact_root,
            &building,
            &log_path,
            &deployment_id,
            error.to_string(),
        ));
    }
    let result = (|| {
        let publish = PublishDir::create(
            &artifact_root,
            &deployment_id,
            MAX_ARTIFACT_BYTES,
            MAX_ARTIFACT_INODES,
        )?;
        let job = build_job(
            &engine,
            &workspace,
            publish.path(),
            &entry,
            &deployment_id,
            build_plan,
        );
        let job_result = match run_build_job(job, build_plan.install, stdout_log, stderr_log) {
            Ok(result) => result,
            Err(error) => {
                append_log(
                    &log_path.join("build.stderr.log"),
                    error.to_string().as_bytes(),
                )?;
                publish.close()?;
                return Err(fail_build(
                    state,
                    &artifact_root,
                    &building,
                    &log_path,
                    &deployment_id,
                    format!("Bun build pipeline cage could not start: {error}"),
                ));
            }
        };
        if !job_result.success() {
            let detail = match job_result.outcome {
                JobExitOutcome::Exited(code) => {
                    format!("Bun build pipeline exited with status {code}")
                }
                JobExitOutcome::Signaled(signal) => {
                    format!("Bun build pipeline was terminated by signal {signal}")
                }
                JobExitOutcome::TimedOut => "Bun build pipeline exceeded its deadline".into(),
                JobExitOutcome::OutputLimitExceeded => {
                    "Bun build pipeline exceeded its output limit".into()
                }
            };
            publish.close()?;
            return Err(fail_build(
                state,
                &artifact_root,
                &building,
                &log_path,
                &deployment_id,
                detail,
            ));
        }

        let staging_result = (|| {
            validate_build_payload(publish.path())?;
            copy_output_tree(publish.path(), &building)
        })();
        let close_result = publish.close();
        close_result?;
        staging_result?;
        let generated = expected_generated_entry(&building.join("app"), &entry)?;
        let sidecar = generated.with_extension("js.jsc");
        let sidecar_meta = fs::symlink_metadata(&sidecar).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!(
                    "expected Bun bytecode sidecar {}: {error}",
                    sidecar.display()
                ),
            )
        })?;
        if !sidecar_meta.file_type().is_file() {
            return Err(DeployError::Io(io::Error::other(format!(
                "expected Bun bytecode sidecar {} to be a regular file",
                sidecar.display()
            ))));
        }

        let shim = building.join(SHIM_REL);
        if let Some(parent) = shim.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&shim, include_bytes!("../../../assets/shim.js"))?;
        validate_tree(&building)?;

        let manifest = build_manifest(&building)?;
        let artifact_hash = hash_manifest(&manifest);
        let meta_dir = building.join("meta");
        fs::create_dir_all(&meta_dir)?;
        let files_json = serde_json::to_vec(&FilesManifest { files: &manifest })?;
        fs::write(meta_dir.join("files.json"), &files_json)?;
        let generated_relative = generated
            .strip_prefix(&building)
            .map_err(|_| io::Error::other("generated entry escaped build output"))?
            .to_string_lossy()
            .replace(std::path::MAIN_SEPARATOR, "/");
        let metadata = ArtifactMetadata {
            source_hash: &source_hash,
            artifact_hash: &artifact_hash,
            bun_version: &engine.version,
            entry: format!("/{generated_relative}"),
            runtime_entry: format!("/{generated_relative}"),
        };
        let metadata_json = serde_json::to_string(&metadata)?;
        fs::write(meta_dir.join("meta.json"), metadata_json.as_bytes())?;
        let final_path = artifact_root.join(&artifact_hash);
        let runtime = runtime_config(
            &request,
            &engine,
            &final_path,
            &generated_relative,
            &deployment_id,
        )?;
        publish_or_reuse(&building, &final_path, &artifact_hash, &metadata_json)?;
        let _ = remove_work(&artifact_root, &deployment_id);

        let artifact = ArtifactInput {
            app: request.app.clone(),
            source_hash: source_hash.clone(),
            artifact_hash: artifact_hash.clone(),
            engine_version: engine.version.clone(),
            host_path: final_path.clone(),
            metadata_json: metadata_json.clone(),
        };
        if let Err(error) = state.seal_deployment(&deployment_id, &artifact) {
            return Err(fail_build(
                state,
                &artifact_root,
                &building,
                &log_path,
                &deployment_id,
                format!("artifact could not be sealed: {error}"),
            ));
        }
        let plan = state
            .plan_activation(
                &deployment_id,
                &runtime,
                expected_active_artifact.as_deref(),
            )
            .map_err(|error| DeployError::ActivationFailed {
                id: deployment_id.clone(),
                detail: error.to_string(),
            })?;
        let preparation = prepare(&plan.candidate)?;
        if let Err(error) = state.commit_activation(&plan, audit) {
            drop(preparation);
            return Err(DeployError::ActivationFailed {
                id: deployment_id.clone(),
                detail: error.to_string(),
            });
        }
        preparation.committed();
        let deployment =
            state
                .deployment(&deployment_id)?
                .ok_or_else(|| DeployError::ActivationFailed {
                    id: deployment_id.clone(),
                    detail: "deployment disappeared after activation".into(),
                })?;
        Ok(DeployResult {
            deployment_id: deployment_id.clone(),
            source_hash,
            artifact_hash,
            artifact_path: final_path,
            deployment,
        })
    })();

    match result {
        Ok(result) => Ok(result),
        Err(error @ (DeployError::BuildFailed { .. } | DeployError::ActivationFailed { .. })) => {
            Err(error)
        }
        Err(error) => {
            let detail = error.to_string();
            Err(fail_build(
                state,
                &artifact_root,
                &building,
                &log_path,
                &deployment_id,
                detail,
            ))
        }
    }
}

fn run_build_job(
    job: JobConfig,
    needs_dns: bool,
    stdout: File,
    stderr: File,
) -> Result<JobCompletion, CageError> {
    #[cfg(target_os = "linux")]
    if needs_dns {
        let dns = DnsForwarder::start()?;
        return run_job_streaming_with_dns(job, stdout, stderr, &dns);
    }

    #[cfg(not(target_os = "linux"))]
    let _ = needs_dns;
    run_job_streaming(job, stdout, stderr)
}

fn build_job(
    engine: &EngineRecord,
    workspace: &Path,
    publish: &Path,
    entry: &Path,
    deployment_id: &str,
    plan: BuildPlan,
) -> JobConfig {
    let linux = cfg!(target_os = "linux");
    let relative = engine
        .cage_executable
        .strip_prefix("/")
        .unwrap_or(engine.cage_executable.as_path());
    let command = if linux {
        engine.cage_executable.clone()
    } else {
        engine.host_root.join(relative)
    };
    let mut job = JobConfig::new(format!("cygnus-build-{deployment_id}"), command);
    job.args = vec![
        OsString::from("--no-env-file"),
        OsString::from(format!("--config={BUILD_CONFIG_CAGE_PATH}")),
        OsString::from(BUILD_RUNNER_CAGE_PATH),
    ];
    if plan.install {
        job.args.push(OsString::from("--install"));
    }
    job.args.push(entry.as_os_str().to_owned());
    job.env.insert("HOME".into(), BUILD_HOME_CAGE_PATH.into());
    job.env
        .insert("TMPDIR".into(), BUILD_TMPDIR_CAGE_PATH.into());
    job.env.insert("PATH".into(), BUILD_PATH.into());
    job.env
        .insert("BUN_INSTALL_CACHE_DIR".into(), BUILD_CACHE_CAGE_PATH.into());
    job.env
        .insert("NPM_CONFIG_REGISTRY".into(), BUILD_REGISTRY.into());
    job.egress = if plan.install {
        EgressMode::BuildDomains {
            allow: vec![DomainEgressRule {
                domain: BUILD_REGISTRY_DOMAIN.into(),
                ports: vec![443],
            }],
        }
    } else {
        EgressMode::None
    };
    job.seccomp = Some(FilterMode::Enforce);
    if linux {
        job.init = Some(PathBuf::from(INIT_CAGE_PATH));
    }
    job.timeout = JobConfig::DEFAULT_TIMEOUT;
    job.stdout_limit = MAX_BUILD_OUTPUT;
    job.stderr_limit = MAX_BUILD_OUTPUT;
    job.total_output_limit = Some(MAX_BUILD_OUTPUT * 2);
    job.working_dir = Some(if linux {
        PathBuf::from(BUILD_WORKDIR_CAGE_PATH)
    } else {
        workspace.parent().unwrap_or(workspace).to_path_buf()
    });
    if plan.install {
        job.limits.memory_max = BUILD_INSTALL_MEMORY_MAX;
        job.limits.memory_high = BUILD_INSTALL_MEMORY_HIGH;
        job.limits.pids_max = BUILD_INSTALL_PIDS_MAX;
    }
    if linux {
        let mut rootfs = RootfsSpec::new(vec![
            workspace.parent().unwrap_or(workspace).to_path_buf(),
            engine.host_root.clone(),
        ]);
        rootfs.tmpfs_size = BUILD_ROOTFS_TMPFS_SIZE;
        job.rootfs = Some(rootfs);
        job.build_output = Some(BuildOutputSpec::new(publish));
    }
    job
}

fn runtime_config(
    request: &DeployRequest,
    engine: &EngineRecord,
    artifact_path: &Path,
    generated_relative: &str,
    deployment_id: &str,
) -> Result<AppConfig, DeployError> {
    let relative = engine
        .cage_executable
        .strip_prefix("/")
        .map_err(|_| DeployError::InvalidInput("engine executable must be absolute".into()))?;
    let linux = cfg!(target_os = "linux");
    let command = if linux {
        engine.cage_executable.to_string_lossy().into_owned()
    } else {
        engine
            .host_root
            .join(relative)
            .to_string_lossy()
            .into_owned()
    };
    let socket_dir = request
        .upstream
        .parent()
        .ok_or_else(|| DeployError::InvalidInput("upstream must have a parent directory".into()))?;
    fs::create_dir_all(socket_dir)?;
    let runtime_id = format!("{}--{deployment_id}", request.app);
    let host_upstream = socket_dir.join(format!("{runtime_id}.sock"));
    let socket = if linux {
        PathBuf::from(cygnus_cage::INGRESS_CAGE_DIR).join(
            host_upstream
                .file_name()
                .ok_or_else(|| DeployError::InvalidInput("upstream must have a filename".into()))?,
        )
    } else {
        host_upstream.clone()
    };
    let (shim, entry) = if linux {
        (
            PathBuf::from("/cygnus/shim.js"),
            PathBuf::from(format!("/{generated_relative}")),
        )
    } else {
        (
            artifact_path.join(SHIM_REL),
            artifact_path.join(generated_relative),
        )
    };
    let mut env = BTreeMap::new();
    env.insert(
        "CYGNUS_SOCKET".into(),
        socket.to_string_lossy().into_owned(),
    );
    Ok(AppConfig {
        name: request.app.clone(),
        domains: vec![request.domain.clone()],
        upstream: host_upstream,
        command,
        args: vec![
            "--preload".into(),
            shim.to_string_lossy().into_owned(),
            entry.to_string_lossy().into_owned(),
        ],
        env,
        rootfs: Some(RootfsConfig {
            lowerdirs: vec![engine.host_root.clone(), artifact_path.to_path_buf()],
            ..RootfsConfig::default()
        }),
        init: linux.then(|| PathBuf::from(INIT_CAGE_PATH)),
        seccomp: Some(SeccompMode::Enforce),
        egress: crate::state::EgressConfig::None,
        ..AppConfig::default()
    })
}

fn deploy_request_digest(request: &DeployRequest) -> String {
    let mut hasher = Sha256::new();
    for value in [
        request.source_dir.as_os_str(),
        OsStr::new(&request.app),
        OsStr::new(&request.domain),
        OsStr::new(&request.engine_version),
        request.entry.as_os_str(),
        request.artifact_root.as_os_str(),
        request.upstream.as_os_str(),
    ] {
        hasher.update(value.as_bytes());
        hasher.update([0]);
    }
    hex_digest(hasher)
}

fn preflight_workspace(workspace: &Path) -> Result<BuildPlan, DeployError> {
    reject_workspace_path(workspace, "node_modules", true)?;
    reject_workspace_path(workspace, ".npmrc", false)?;
    reject_workspace_path(workspace, "bunfig.toml", false)?;
    reject_workspace_path(workspace, "bun.lockb", false)?;

    let package_path = workspace.join("package.json");
    let has_dependencies = match fs::symlink_metadata(&package_path) {
        Ok(metadata) => {
            if !metadata.file_type().is_file() {
                return Err(DeployError::InvalidInput(
                    "package.json must be a regular file".into(),
                ));
            }
            let bytes = read_control_file(&package_path, MAX_PACKAGE_JSON_BYTES, "package.json")?;
            let value: serde_json::Value = serde_json::from_slice(&bytes).map_err(|error| {
                DeployError::InvalidInput(format!("package.json is malformed: {error}"))
            })?;
            let package = value.as_object().ok_or_else(|| {
                DeployError::InvalidInput("package.json must contain a JSON object".into())
            })?;
            dependency_section_nonempty(package, "dependencies")?
                || dependency_section_nonempty(package, "devDependencies")?
                || dependency_section_nonempty(package, "optionalDependencies")?
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return preflight_lock_only(workspace);
        }
        Err(error) => return Err(error.into()),
    };

    let lock_path = workspace.join("bun.lock");
    match fs::symlink_metadata(&lock_path) {
        Ok(metadata) => {
            if !metadata.file_type().is_file() {
                return Err(DeployError::InvalidInput(
                    "bun.lock must be a regular text file".into(),
                ));
            }
            let bytes = read_control_file(&lock_path, MAX_BUN_LOCK_BYTES, "bun.lock")?;
            validate_bun_lock(&bytes)?;
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound && has_dependencies => {
            return Err(DeployError::InvalidInput(
                "package dependencies require a regular text bun.lock".into(),
            ));
        }
        Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error.into()),
        Err(_) => {}
    }

    if has_dependencies {
        for name in [
            "package-lock.json",
            "npm-shrinkwrap.json",
            "yarn.lock",
            "pnpm-lock.yaml",
            "pnpm-lock.yml",
        ] {
            reject_workspace_path(workspace, name, false)?;
        }
    }
    Ok(BuildPlan {
        install: has_dependencies,
    })
}

fn preflight_lock_only(workspace: &Path) -> Result<BuildPlan, DeployError> {
    let lock_path = workspace.join("bun.lock");
    match fs::symlink_metadata(&lock_path) {
        Ok(metadata) => {
            if !metadata.file_type().is_file() {
                return Err(DeployError::InvalidInput(
                    "bun.lock must be a regular text file".into(),
                ));
            }
            let bytes = read_control_file(&lock_path, MAX_BUN_LOCK_BYTES, "bun.lock")?;
            validate_bun_lock(&bytes)?;
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(BuildPlan { install: false })
}

fn reject_workspace_path(workspace: &Path, name: &str, directory: bool) -> Result<(), DeployError> {
    let path = workspace.join(name);
    match fs::symlink_metadata(&path) {
        Ok(_) if directory => Err(DeployError::InvalidInput(format!(
            "workspace contains forbidden root directory {name}"
        ))),
        Ok(_) => Err(DeployError::InvalidInput(format!(
            "workspace contains forbidden control file {name}"
        ))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn dependency_section_nonempty(
    package: &serde_json::Map<String, serde_json::Value>,
    name: &str,
) -> Result<bool, DeployError> {
    let Some(value) = package.get(name) else {
        return Ok(false);
    };
    let Some(dependencies) = value.as_object() else {
        return Err(DeployError::InvalidInput(format!(
            "package.json {name} must be a JSON object"
        )));
    };
    if dependencies
        .iter()
        .any(|(dependency, version)| dependency.is_empty() || !version.is_string())
    {
        return Err(DeployError::InvalidInput(format!(
            "package.json {name} must map names to version strings"
        )));
    }
    Ok(!dependencies.is_empty())
}

fn read_control_file(path: &Path, max_bytes: u64, name: &str) -> Result<Vec<u8>, DeployError> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.len() > max_bytes {
        return Err(DeployError::InvalidInput(format!(
            "{name} exceeds the {} byte preflight limit",
            max_bytes
        )));
    }
    let bytes = fs::read(path)?;
    if bytes.len() as u64 > max_bytes {
        return Err(DeployError::InvalidInput(format!(
            "{name} exceeds the {} byte preflight limit",
            max_bytes
        )));
    }
    Ok(bytes)
}

fn validate_bun_lock(bytes: &[u8]) -> Result<(), DeployError> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| DeployError::InvalidInput("bun.lock must be regular UTF-8 text".into()))?;
    if text.is_empty() || text.as_bytes().contains(&0) {
        return Err(DeployError::InvalidInput(
            "bun.lock must be regular UTF-8 text".into(),
        ));
    }
    let normalized = strip_trailing_json_commas(text)?;
    let value: serde_json::Value = serde_json::from_str(&normalized)
        .map_err(|error| DeployError::InvalidInput(format!("bun.lock is malformed: {error}")))?;
    if !value.is_object() || value.get("lockfileVersion").is_none() {
        return Err(DeployError::InvalidInput(
            "bun.lock is not a Bun text lockfile".into(),
        ));
    }
    Ok(())
}

fn strip_trailing_json_commas(text: &str) -> Result<String, DeployError> {
    let chars: Vec<char> = text.chars().collect();
    let mut output = String::with_capacity(text.len());
    let mut in_string = false;
    let mut escaped = false;
    for (index, ch) in chars.iter().copied().enumerate() {
        if in_string {
            output.push(ch);
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        if ch == '"' {
            in_string = true;
            output.push(ch);
        } else if ch == ',' {
            let mut next = index + 1;
            while next < chars.len() && chars[next].is_whitespace() {
                next += 1;
            }
            if next == chars.len() || matches!(chars[next], '}' | ']') {
                continue;
            }
            output.push(ch);
        } else {
            output.push(ch);
        }
    }
    if in_string || escaped {
        return Err(DeployError::InvalidInput(
            "bun.lock is malformed: unterminated string".into(),
        ));
    }
    Ok(output)
}

fn stage_build_controls(rootfs: &Path) -> Result<(), DeployError> {
    let control_dir = rootfs.join("cygnus");
    fs::create_dir_all(&control_dir)?;
    let metadata = fs::symlink_metadata(&control_dir)?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() {
        return Err(DeployError::InvalidInput(
            "build control directory must be daemon-owned".into(),
        ));
    }
    write_control_asset(
        &control_dir.join(BUILD_RUNNER_REL.strip_prefix("cygnus/").unwrap()),
        include_bytes!("../../../assets/build-runner.js"),
    )?;
    write_control_asset(
        &control_dir.join(BUILD_CONFIG_REL.strip_prefix("cygnus/").unwrap()),
        include_bytes!("../../../assets/build.bunfig.toml"),
    )?;
    Ok(())
}

fn write_control_asset(path: &Path, bytes: &[u8]) -> Result<(), DeployError> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(bytes)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o444))?;
    Ok(())
}

fn canonical_source_root(path: &Path) -> Result<PathBuf, DeployError> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        DeployError::InvalidInput(format!(
            "source root {} is unavailable: {error}",
            path.display()
        ))
    })?;
    if metadata.file_type().is_symlink() {
        return Err(DeployError::InvalidInput(
            "source root must not be a symlink".into(),
        ));
    }
    if !metadata.file_type().is_dir() {
        return Err(DeployError::InvalidInput(
            "source root must be a directory".into(),
        ));
    }
    Ok(fs::canonicalize(path)?)
}

fn prepare_artifact_root(path: &Path) -> Result<PathBuf, DeployError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(DeployError::InvalidInput(
                "artifact root must not be a symlink".into(),
            ));
        }
        Ok(metadata) if !metadata.file_type().is_dir() => {
            return Err(DeployError::InvalidInput(
                "artifact root must be a directory".into(),
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => fs::create_dir_all(path)?,
        Err(error) => return Err(error.into()),
    }
    Ok(fs::canonicalize(path)?)
}

fn validate_entry(entry: &Path) -> Result<(), DeployError> {
    if entry.as_os_str().is_empty()
        || entry.is_absolute()
        || entry.components().any(|component| {
            matches!(
                component,
                Component::CurDir | Component::ParentDir | Component::Prefix(_)
            )
        })
    {
        return Err(DeployError::InvalidInput(
            "entry must be a nonempty relative path without '.' or '..' components".into(),
        ));
    }
    Ok(())
}
fn validate_upstream(path: &Path) -> Result<(), DeployError> {
    if !path.is_absolute()
        || path.as_os_str().as_bytes().contains(&0)
        || path.file_name().is_none()
        || path.components().any(|component| {
            matches!(
                component,
                Component::CurDir | Component::ParentDir | Component::Prefix(_)
            )
        })
    {
        return Err(DeployError::InvalidInput(
            "upstream must be an absolute canonical socket path".into(),
        ));
    }
    Ok(())
}

fn copy_source(source: &Path, destination: &Path) -> Result<String, DeployError> {
    let source = fs::canonicalize(source)?;
    let mut files = Vec::new();
    collect_source_files(&source, Path::new(""), &mut files)?;
    files.sort_by(|left, right| left.0.cmp(&right.0));
    let mut hasher = Sha256::new();
    for (relative, path) in files {
        let relative = slash_path(&relative);
        let bytes = read_source_file(&source, &path)?;
        hash_path_length_bytes(&mut hasher, &relative, bytes.len() as u64, &bytes);
        let target = destination.join(&relative);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&target)?;
        output.write_all(&bytes)?;
    }
    Ok(hex_digest(hasher))
}

fn read_source_file(source_root: &Path, path: &Path) -> Result<Vec<u8>, DeployError> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)?;
    if !file.metadata()?.file_type().is_file() {
        return Err(DeployError::InvalidInput(format!(
            "source path {} changed type during intake",
            path.display()
        )));
    }
    #[cfg(target_os = "linux")]
    let opened_path = fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd()))?;
    #[cfg(not(target_os = "linux"))]
    let opened_path = fs::canonicalize(path)?;
    if !opened_path.starts_with(source_root) {
        return Err(DeployError::InvalidInput(format!(
            "source path {} escaped its source root during intake",
            path.display()
        )));
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn collect_source_files(
    root: &Path,
    relative: &Path,
    files: &mut Vec<(PathBuf, PathBuf)>,
) -> Result<(), DeployError> {
    let mut entries = fs::read_dir(root)?.collect::<Result<Vec<_>, io::Error>>()?;
    entries.sort_by(|left, right| {
        left.file_name()
            .as_bytes()
            .cmp(right.file_name().as_bytes())
    });
    for entry in entries {
        let name = entry.file_name();
        let child_relative = relative.join(&name);
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        let kind = metadata.file_type();
        if kind.is_symlink() {
            return Err(DeployError::InvalidInput(format!(
                "source contains symlink {}",
                child_relative.display()
            )));
        }
        if kind.is_dir() {
            collect_source_files(&path, &child_relative, files)?;
        } else if kind.is_file() {
            if child_relative.to_str().is_none() {
                return Err(DeployError::InvalidInput(
                    "source path is not valid UTF-8".into(),
                ));
            }

            files.push((child_relative, path));
        } else {
            return Err(DeployError::InvalidInput(format!(
                "source contains non-regular file {}",
                child_relative.display()
            )));
        }
    }
    Ok(())
}

fn validate_build_payload(root: &Path) -> Result<(), DeployError> {
    validate_tree(root)?;
    let entries = fs::read_dir(root)?.collect::<Result<Vec<_>, io::Error>>()?;
    if entries.len() != 1 || entries[0].file_name() != OsStr::new("app") {
        return Err(DeployError::InvalidInput(
            "build output may publish only the reserved app directory".into(),
        ));
    }
    if !fs::symlink_metadata(entries[0].path())?
        .file_type()
        .is_dir()
    {
        return Err(DeployError::InvalidInput(
            "build output app path must be a directory".into(),
        ));
    }
    Ok(())
}

fn copy_output_tree(source: &Path, destination: &Path) -> Result<(), DeployError> {
    fs::create_dir(destination)?;
    let mut entries = fs::read_dir(source)?.collect::<Result<Vec<_>, io::Error>>()?;
    entries.sort_by(|left, right| {
        left.file_name()
            .as_bytes()
            .cmp(right.file_name().as_bytes())
    });
    for entry in entries {
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        let metadata = fs::symlink_metadata(&source_path)?;
        if metadata.file_type().is_dir() {
            copy_output_tree(&source_path, &destination_path)?;
        } else if metadata.file_type().is_file() {
            let mut input = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
                .open(&source_path)?;
            if !input.metadata()?.file_type().is_file() {
                return Err(DeployError::InvalidInput(format!(
                    "build output changed type at {}",
                    source_path.display()
                )));
            }
            let mut output = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&destination_path)?;
            io::copy(&mut input, &mut output)?;
        } else {
            return Err(DeployError::InvalidInput(format!(
                "build output contains unsupported file {}",
                source_path.display()
            )));
        }
    }
    Ok(())
}

fn expected_generated_entry(app: &Path, entry: &Path) -> Result<PathBuf, DeployError> {
    let expected = app.join(entry.with_extension("js"));
    if fs::symlink_metadata(&expected).is_ok() {
        return Ok(expected);
    }
    let mut candidates = Vec::new();
    collect_files_with_suffix(app, ".js", &mut candidates)?;
    if candidates.len() == 1 {
        Ok(candidates.remove(0))
    } else {
        Err(DeployError::InvalidInput(format!(
            "Bun build did not produce expected entry {}",
            expected.display()
        )))
    }
}

fn collect_files_with_suffix(
    root: &Path,
    suffix: &str,
    output: &mut Vec<PathBuf>,
) -> Result<(), DeployError> {
    let mut entries = fs::read_dir(root)?.collect::<Result<Vec<_>, io::Error>>()?;
    entries.sort_by(|left, right| {
        left.file_name()
            .as_bytes()
            .cmp(right.file_name().as_bytes())
    });
    for entry in entries {
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_dir() {
            collect_files_with_suffix(&path, suffix, output)?;
        } else if metadata.file_type().is_file() && path.to_string_lossy().ends_with(suffix) {
            output.push(path);
        }
    }
    Ok(())
}

fn validate_tree(root: &Path) -> Result<(), DeployError> {
    let metadata = fs::symlink_metadata(root)?;
    if metadata.file_type().is_symlink() {
        return Err(DeployError::InvalidInput(format!(
            "build output contains symlink {}",
            root.display()
        )));
    }
    if metadata.file_type().is_file() {
        return Ok(());
    }
    if !metadata.file_type().is_dir() {
        return Err(DeployError::InvalidInput(format!(
            "build output contains special file {}",
            root.display()
        )));
    }
    for entry in fs::read_dir(root)? {
        validate_tree(&entry?.path())?;
    }
    Ok(())
}

fn prepare_live_logs(artifact_root: &Path, id: &str) -> Result<(File, File), DeployError> {
    let parent = artifact_root.join(LOG_REL);
    fs::create_dir_all(&parent)?;
    let metadata = fs::symlink_metadata(&parent)?;
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        return Err(DeployError::InvalidInput(
            "deployment log path must be a directory".into(),
        ));
    }
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o700))?;
    let logs = parent.join(id);
    fs::create_dir(&logs)?;
    fs::set_permissions(&logs, fs::Permissions::from_mode(0o700))?;
    let stdout = create_log_file(&logs.join("build.stdout.log"))?;
    let stderr = create_log_file(&logs.join("build.stderr.log"))?;
    Ok((stdout, stderr))
}

fn create_log_file(path: &Path) -> Result<File, io::Error> {
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    Ok(file)
}

fn append_log(path: &Path, bytes: &[u8]) -> Result<(), io::Error> {
    let mut file = OpenOptions::new().append(true).open(path)?;
    file.write_all(bytes)?;
    file.flush()?;
    file.sync_all()
}

fn sync_logs(logs: &Path) {
    for name in ["build.stdout.log", "build.stderr.log"] {
        if let Ok(file) = OpenOptions::new().write(true).open(logs.join(name)) {
            let _ = file.sync_all();
        }
    }
}

fn fail_build(
    state: &mut State,
    artifact_root: &Path,
    output: &Path,
    logs: &Path,
    id: &str,
    detail: String,
) -> DeployError {
    let failed = artifact_root.join(FAILED_REL).join(id);
    let failed_output = failed.join("output");
    let _ = fs::create_dir_all(artifact_root.join(FAILED_REL));
    let _ = fs::remove_dir_all(&failed);
    let _ = fs::create_dir(&failed);
    if output.exists() && !is_content_addressed_publication(artifact_root, output) {
        let _ = fs::rename(output, &failed_output);
    }
    let _ = fs::create_dir_all(logs);
    let _ = fs::set_permissions(logs, fs::Permissions::from_mode(0o700));
    for name in ["build.stdout.log", "build.stderr.log"] {
        let path = logs.join(name);
        if !path.exists() {
            let _ = create_log_file(&path);
        }
    }
    let _ = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(logs.join("pipeline.error.log"))
        .and_then(|mut file| {
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
            file.write_all(detail.as_bytes())?;
            file.flush()?;
            file.sync_all()
        });
    sync_logs(logs);
    let terminal = match state
        .set_deployment_log_path(id, logs)
        .and_then(|_| state.mark_deployment_failed(id, &detail))
    {
        Ok(_) => detail,
        Err(error) => format!("{detail}; unable to persist failed state: {error}"),
    };
    let _ = remove_work(artifact_root, id);
    DeployError::BuildFailed {
        id: id.to_owned(),
        detail: terminal,
        logs: logs.to_path_buf(),
    }
}

fn is_content_addressed_publication(artifact_root: &Path, path: &Path) -> bool {
    path.parent() == Some(artifact_root)
        && path.file_name().is_some_and(|name| {
            let bytes = name.as_bytes();
            bytes.len() == 64
                && bytes
                    .iter()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        })
}

fn remove_work(artifact_root: &Path, id: &str) -> Result<(), io::Error> {
    let work = artifact_root.join(WORKSPACE_REL).join(id);
    if work.exists() {
        fs::remove_dir_all(work)?;
    }
    let _ = fs::remove_dir(artifact_root.join(WORKSPACE_REL));
    Ok(())
}

/// Atomically publish a completed content-addressed directory, or recover an
/// earlier publication that reached the filesystem before state was sealed.
/// Existing content is only validated and reused; it is never replaced.
fn publish_or_reuse(
    building: &Path,
    final_path: &Path,
    artifact_hash: &str,
    metadata_json: &str,
) -> Result<bool, DeployError> {
    validate_tree(building)?;
    if hash_manifest(&build_manifest(building)?) != artifact_hash {
        return Err(DeployError::InvalidInput(
            "staged artifact content does not match its computed hash".into(),
        ));
    }

    if final_path.exists() {
        validate_reusable_artifact(final_path, artifact_hash, metadata_json)?;
        fs::remove_dir_all(building)?;
        return Ok(true);
    }

    sync_tree(building)?;
    make_read_only(building)?;
    sync_tree(building)?;
    match rename_noreplace(building, final_path) {
        Ok(()) => {
            if let Some(parent) = final_path.parent() {
                File::open(parent)?.sync_all()?;
            }
            Ok(false)
        }
        Err(_error) if final_path.exists() => {
            validate_reusable_artifact(final_path, artifact_hash, metadata_json)?;
            remove_read_only_tree(building)?;
            Ok(true)
        }
        Err(error) => Err(error.into()),
    }
}

fn validate_reusable_artifact(
    path: &Path,
    artifact_hash: &str,
    metadata_json: &str,
) -> Result<(), DeployError> {
    validate_tree(path)?;
    validate_read_only_tree(path)?;
    let actual_hash = hash_manifest(&build_manifest(path)?);
    if actual_hash != artifact_hash {
        return Err(DeployError::InvalidInput(format!(
            "existing artifact {} hashes to {actual_hash}, not {artifact_hash}",
            path.display()
        )));
    }
    let existing: serde_json::Value =
        serde_json::from_slice(&fs::read(path.join("meta/meta.json"))?)?;
    let expected: serde_json::Value = serde_json::from_str(metadata_json)?;
    if existing != expected {
        return Err(DeployError::InvalidInput(format!(
            "existing artifact {} metadata does not match this deployment",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn rename_noreplace(from: &Path, to: &Path) -> Result<(), io::Error> {
    let from = CString::new(from.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "source path contains NUL"))?;
    let to = CString::new(to.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "target path contains NUL"))?;
    let result = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            from.as_ptr(),
            libc::AT_FDCWD,
            to.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(target_os = "macos")]
fn rename_noreplace(from: &Path, to: &Path) -> Result<(), io::Error> {
    let from = CString::new(from.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "source path contains NUL"))?;
    let to = CString::new(to.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "target path contains NUL"))?;
    let result = unsafe { libc::renamex_np(from.as_ptr(), to.as_ptr(), libc::RENAME_EXCL) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn rename_noreplace(from: &Path, to: &Path) -> Result<(), io::Error> {
    if to.exists() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "artifact target already exists",
        ));
    }
    fs::rename(from, to)
}

fn sync_tree(path: &Path) -> Result<(), io::Error> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_file() {
        return File::open(path)?.sync_all();
    }
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        return Err(io::Error::other("artifact contains unsupported file type"));
    }
    for entry in fs::read_dir(path)? {
        sync_tree(&entry?.path())?;
    }
    File::open(path)?.sync_all()
}

fn validate_read_only_tree(path: &Path) -> Result<(), DeployError> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.permissions().mode() & 0o222 != 0 {
        return Err(DeployError::InvalidInput(format!(
            "existing artifact {} is not sealed read-only content",
            path.display()
        )));
    }
    if metadata.file_type().is_dir() {
        for entry in fs::read_dir(path)? {
            validate_read_only_tree(&entry?.path())?;
        }
    }
    Ok(())
}

fn remove_read_only_tree(path: &Path) -> Result<(), io::Error> {
    let metadata = fs::symlink_metadata(path)?;
    let mut permissions = metadata.permissions();
    if metadata.file_type().is_dir() {
        permissions.set_mode(0o700);
        fs::set_permissions(path, permissions)?;
        for entry in fs::read_dir(path)? {
            remove_read_only_tree(&entry?.path())?;
        }
    } else {
        permissions.set_mode(0o600);
        fs::set_permissions(path, permissions)?;
    }
    if path.is_dir() {
        fs::remove_dir(path)
    } else {
        fs::remove_file(path)
    }
}

fn make_read_only(root: &Path) -> Result<(), io::Error> {
    let metadata = fs::symlink_metadata(root)?;
    if metadata.file_type().is_symlink()
        || (!metadata.file_type().is_dir() && !metadata.file_type().is_file())
    {
        return Err(io::Error::other("artifact contains unsupported file type"));
    }
    let mut permissions = metadata.permissions();
    permissions.set_mode(permissions.mode() & !0o222);
    fs::set_permissions(root, permissions)?;
    if metadata.file_type().is_dir() {
        for entry in fs::read_dir(root)? {
            make_read_only(&entry?.path())?;
        }
    }
    Ok(())
}

#[derive(Clone, Debug, Serialize)]
struct ManifestEntry {
    path: String,
    length: u64,
    sha256: String,
}

#[derive(Serialize)]
struct FilesManifest<'a> {
    files: &'a [ManifestEntry],
}

#[derive(Serialize)]
struct ArtifactMetadata<'a> {
    #[serde(rename = "sourceHash")]
    source_hash: &'a str,
    #[serde(rename = "artifactHash")]
    artifact_hash: &'a str,
    #[serde(rename = "bunVersion")]
    bun_version: &'a str,
    entry: String,
    #[serde(rename = "runtimeEntry")]
    runtime_entry: String,
}

fn build_manifest(root: &Path) -> Result<Vec<ManifestEntry>, DeployError> {
    let mut paths = Vec::new();
    collect_payload_files(root, Path::new(""), &mut paths)?;
    paths.sort();
    let mut manifest = Vec::with_capacity(paths.len());
    for (relative, path) in paths {
        let bytes = fs::read(&path)?;
        manifest.push(ManifestEntry {
            path: slash_path(&relative),
            length: bytes.len() as u64,
            sha256: sha256_bytes(&bytes),
        });
    }
    Ok(manifest)
}

fn collect_payload_files(
    root: &Path,
    relative: &Path,
    output: &mut Vec<(PathBuf, PathBuf)>,
) -> Result<(), DeployError> {
    let mut entries = fs::read_dir(root)?.collect::<Result<Vec<_>, io::Error>>()?;
    entries.sort_by(|left, right| {
        left.file_name()
            .as_bytes()
            .cmp(right.file_name().as_bytes())
    });
    for entry in entries {
        let name = entry.file_name();
        let child_relative = relative.join(&name);
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_dir() {
            let excluded_metadata = relative.as_os_str().is_empty()
                && (name == OsStr::new("meta") || name == OsStr::new("logs"));
            if !excluded_metadata {
                collect_payload_files(&path, &child_relative, output)?;
            }
        } else if metadata.file_type().is_file() {
            output.push((child_relative, path));
        } else {
            return Err(DeployError::InvalidInput(format!(
                "artifact contains special file {}",
                child_relative.display()
            )));
        }
    }
    Ok(())
}

fn hash_manifest(manifest: &[ManifestEntry]) -> String {
    let mut hasher = Sha256::new();
    for entry in manifest {
        hash_path_length_bytes(
            &mut hasher,
            &entry.path,
            entry.length,
            entry.sha256.as_bytes(),
        );
    }
    hex_digest(hasher)
}

fn hash_path_length_bytes(hasher: &mut Sha256, path: &str, length: u64, bytes: &[u8]) {
    hasher.update(path.as_bytes());
    hasher.update([0]);
    hasher.update(length.to_be_bytes());
    hasher.update(bytes);
}

fn slash_path(path: &Path) -> String {
    path.to_string_lossy()
        .replace(std::path::MAIN_SEPARATOR, "/")
}

fn new_deployment_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{:x}-{}", nanos, std::process::id())
}

fn sha256_file(path: &Path) -> Result<String, io::Error> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(hex_digest(hasher))
}

fn sha256_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex_digest(hasher)
}

fn hex_digest(hasher: Sha256) -> String {
    format!("{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::DeploymentStatus;
    use std::ffi::OsStr;
    use std::os::unix::fs::{MetadataExt, symlink};
    use std::process::{Command, Stdio};
    use std::sync::mpsc;
    use std::thread;
    use std::time::{Duration, Instant};

    fn temp_dir(label: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("cygnus-deploy-{label}-{}", new_deployment_id()));
        fs::create_dir_all(&path).expect("temporary directory");
        path
    }

    #[test]
    fn sha256_matches_known_vectors() {
        assert_eq!(
            sha256_bytes(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_bytes(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn intake_hash_and_copy_are_deterministic() {
        let parent = temp_dir("copy");
        let root = parent.join("source");
        fs::create_dir_all(root.join("z")).unwrap();
        fs::write(root.join("z/b.txt"), b"b").unwrap();
        fs::write(root.join("a.txt"), b"a").unwrap();
        let one = parent.join("one");
        let two = parent.join("two");
        fs::create_dir_all(&one).unwrap();
        fs::create_dir_all(&two).unwrap();
        let first = copy_source(&root, &one).unwrap();
        let second = copy_source(&root, &two).unwrap();
        assert_eq!(first, second);
        assert_eq!(fs::read(one.join("a.txt")).unwrap(), b"a");
        fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn intake_rejects_symlink_and_traversal_entry() {
        let root = temp_dir("reject");
        fs::write(root.join("file"), b"x").unwrap();
        symlink(root.join("file"), root.join("link")).unwrap();
        let destination = root.join("out");
        fs::create_dir_all(&destination).unwrap();
        assert!(copy_source(&root, &destination).is_err());
        assert!(validate_entry(Path::new("../file")).is_err());
        assert!(validate_entry(Path::new("/tmp/file")).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn dependency_preflight_matrix_is_strict_and_offline_without_deps() {
        let root = temp_dir("preflight");
        let workspace = root.join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        assert_eq!(
            preflight_workspace(&workspace).unwrap(),
            BuildPlan { install: false }
        );

        fs::write(
            workspace.join("package.json"),
            br#"{"name":"app","dependencies":{}}"#,
        )
        .unwrap();
        assert_eq!(
            preflight_workspace(&workspace).unwrap(),
            BuildPlan { install: false }
        );
        fs::write(
            workspace.join("package.json"),
            br#"{"name":"app","dependencies":{"left-pad":"1.3.0"}}"#,
        )
        .unwrap();
        assert!(preflight_workspace(&workspace).is_err());
        fs::write(
            workspace.join("bun.lock"),
            br#"{"lockfileVersion":1,"workspaces":{},}"#,
        )
        .unwrap();
        assert_eq!(
            preflight_workspace(&workspace).unwrap(),
            BuildPlan { install: true }
        );
        fs::write(workspace.join("package-lock.json"), b"{}\n").unwrap();
        assert!(preflight_workspace(&workspace).is_err());
        fs::remove_file(workspace.join("package-lock.json")).unwrap();
        fs::write(workspace.join("bun.lock"), b"not a lock\n").unwrap();
        assert!(preflight_workspace(&workspace).is_err());
        fs::remove_file(workspace.join("bun.lock")).unwrap();
        fs::create_dir(workspace.join("node_modules")).unwrap();
        assert!(preflight_workspace(&workspace).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn dependency_preflight_rejects_oversized_and_control_files() {
        let root = temp_dir("preflight-limits");
        let workspace = root.join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        fs::write(
            workspace.join("package.json"),
            vec![b'a'; (MAX_PACKAGE_JSON_BYTES + 1) as usize],
        )
        .unwrap();
        assert!(preflight_workspace(&workspace).is_err());
        fs::remove_file(workspace.join("package.json")).unwrap();
        fs::write(workspace.join(".npmrc"), b"registry=https://evil.invalid\n").unwrap();
        assert!(preflight_workspace(&workspace).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn daemon_stages_runner_controls_outside_workspace() {
        let root = temp_dir("control-assets");
        let rootfs = root.join("rootfs");
        let workspace = rootfs.join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        stage_build_controls(&rootfs).unwrap();
        assert!(rootfs.join(BUILD_RUNNER_REL).is_file());
        assert!(rootfs.join(BUILD_CONFIG_REL).is_file());
        assert!(!workspace.join("build-runner.js").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn install_job_has_closed_runner_argv_and_transient_domain_egress() {
        let workspace = temp_dir("job").join("rootfs/workspace");
        fs::create_dir_all(&workspace).unwrap();
        let publish = workspace.parent().unwrap().join("publish");
        fs::create_dir_all(&publish).unwrap();
        let engine = EngineRecord {
            version: "bun".into(),
            host_root: PathBuf::from("/engine"),
            cage_executable: PathBuf::from("/usr/local/bin/bun"),
            sha256: "0".repeat(64),
            is_default: false,
        };
        let job = build_job(
            &engine,
            &workspace,
            &publish,
            Path::new("index.ts"),
            "id",
            BuildPlan { install: true },
        );
        assert_eq!(
            job.args,
            vec![
                OsString::from("--no-env-file"),
                OsString::from("--config=/cygnus/build.bunfig.toml"),
                OsString::from("/cygnus/build-runner.js"),
                OsString::from("--install"),
                OsString::from("index.ts"),
            ]
        );
        assert_eq!(
            job.working_dir,
            Some(if cfg!(target_os = "linux") {
                PathBuf::from("/cygnus")
            } else {
                workspace.parent().unwrap().to_path_buf()
            })
        );
        assert_eq!(job.limits.memory_max, BUILD_INSTALL_MEMORY_MAX);
        assert_eq!(job.limits.pids_max, BUILD_INSTALL_PIDS_MAX);
        assert!(!job.env.contains_key(OsStr::new("BUN_OPTIONS")));
        assert_eq!(
            job.env.get(OsStr::new("NPM_CONFIG_REGISTRY")),
            Some(&OsString::from(BUILD_REGISTRY)),
        );
        assert!(matches!(job.egress, EgressMode::BuildDomains { .. }));
        assert_eq!(
            job.init,
            cfg!(target_os = "linux").then(|| PathBuf::from(INIT_CAGE_PATH))
        );
        if cfg!(target_os = "linux") {
            assert_eq!(
                job.rootfs.as_ref().unwrap().tmpfs_size,
                BUILD_ROOTFS_TMPFS_SIZE
            );
        }
        fs::remove_dir_all(workspace.ancestors().nth(2).unwrap()).unwrap();
    }

    #[test]
    fn published_payload_rejects_build_controlled_namespaces() {
        let root = temp_dir("published-namespace");
        fs::create_dir(root.join("app")).unwrap();
        let sentinel = root.join("sentinel");
        fs::write(&sentinel, b"preserved").unwrap();
        symlink(&sentinel, root.join("logs")).unwrap();
        assert!(validate_build_payload(&root).is_err());
        assert_eq!(fs::read(&sentinel).unwrap(), b"preserved");
        fs::remove_file(root.join("logs")).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn manifest_hash_changes_with_payload_and_is_sorted() {
        let root = temp_dir("manifest");
        fs::create_dir_all(root.join("app")).unwrap();
        fs::write(root.join("app/z.js"), b"z").unwrap();
        fs::write(root.join("app/a.js"), b"a").unwrap();
        fs::create_dir_all(root.join("app/logs")).unwrap();
        fs::write(root.join("app/logs/runtime.js"), b"runtime").unwrap();
        fs::create_dir_all(root.join("logs")).unwrap();
        fs::write(root.join("logs/build.stdout.log"), b"nondeterministic").unwrap();
        let manifest = build_manifest(&root).unwrap();
        assert_eq!(manifest[0].path, "app/a.js");
        assert!(
            manifest
                .iter()
                .any(|entry| entry.path == "app/logs/runtime.js")
        );
        assert!(
            !manifest
                .iter()
                .any(|entry| entry.path == "logs/build.stdout.log")
        );
        let first = hash_manifest(&manifest);
        fs::write(root.join("app/a.js"), b"changed").unwrap();
        assert_ne!(first, hash_manifest(&build_manifest(&root).unwrap()));
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn engine_registration_hashes_actual_executable() {
        let root = temp_dir("engine");
        let db = root.join("state.db");
        let mut state = State::open(&db).unwrap();
        let executable = fs::canonicalize("/bin/sh").unwrap();
        let record = register_engine(&mut state, "test", "/", &executable).unwrap();
        assert_eq!(record.sha256, sha256_file(&executable).unwrap());
        assert_eq!(state.engine("test").unwrap().unwrap(), record);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn engine_registration_rejects_symlinked_executable() {
        let root = temp_dir("engine-symlink");
        let engine_root = root.join("engine");
        fs::create_dir_all(engine_root.join("bin")).unwrap();
        symlink("/bin/sh", engine_root.join("bin/bun")).unwrap();
        let mut state = State::open(root.join("state.db")).unwrap();
        assert!(register_engine(&mut state, "test", &engine_root, "/bin/bun").is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn deployment_rejects_engine_changed_after_registration() {
        let root = temp_dir("engine-mutation");
        let engine_root = root.join("engine");
        let executable = engine_root.join("bin/bun");
        fs::create_dir_all(executable.parent().unwrap()).unwrap();
        fs::write(&executable, b"#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
        let mut state = State::open(root.join("state.db")).unwrap();
        register_engine(&mut state, "test", &engine_root, "/bin/bun").unwrap();
        fs::write(&executable, b"#!/bin/sh\nexit 1\n").unwrap();
        let source = root.join("source");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("index.ts"), b"export default {};").unwrap();
        let error = deploy(
            &mut state,
            DeployRequest::new(
                source,
                "changed-engine",
                "changed.localhost",
                "test",
                "index.ts",
                root.join("artifacts"),
                root.join("run/app.sock"),
            ),
        )
        .expect_err("mutated engine must not execute");
        assert!(error.to_string().contains("changed on disk"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn failed_build_records_logs_and_failed_state() {
        let root = temp_dir("failed");
        let source = root.join("source");
        let artifacts = root.join("artifacts");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("index.ts"), b"export default {};").unwrap();
        let db = root.join("state.db");
        let mut state = State::open(&db).unwrap();
        let shell = fs::canonicalize("/bin/sh").unwrap();
        register_engine(&mut state, "shell", "/", shell).unwrap();
        let error = deploy(
            &mut state,
            DeployRequest::new(
                &source,
                "failed-app",
                "failed.localhost",
                "shell",
                "index.ts",
                &artifacts,
                root.join("run/app.sock"),
            ),
        )
        .expect_err("shell is not Bun and must fail the build");
        let DeployError::BuildFailed { id, logs, .. } = error else {
            panic!("expected build failure, got {error:?}");
        };
        assert!(logs.join("build.stdout.log").is_file());
        assert!(logs.join("build.stderr.log").is_file());
        assert_eq!(
            state.deployment(&id).unwrap().unwrap().status,
            DeploymentStatus::Failed
        );
        assert_eq!(
            state.deployment(&id).unwrap().unwrap().log_path,
            Some(logs.clone())
        );
        assert_eq!(state.deployment_logs_dir(&id).unwrap(), Some(logs));
        assert!(artifacts.join("failed").join(id).is_dir());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn successful_build_logs_are_persisted_per_deployment_outside_artifacts() {
        let root = temp_dir("successful-logs");
        let artifacts = fs::canonicalize(&root).unwrap();
        let mut state = State::open(root.join("state.db")).unwrap();
        register_engine(
            &mut state,
            "shell",
            "/",
            fs::canonicalize("/bin/sh").unwrap(),
        )
        .unwrap();
        state
            .begin_deployment(&DeploymentInput {
                id: "dep-success".into(),
                app: "api".into(),
                source_hash: "b".repeat(64),
                engine_version: "shell".into(),
            })
            .unwrap();
        let logs = artifacts.join("logs/dep-success");
        let (mut stdout, mut stderr) = prepare_live_logs(&artifacts, "dep-success").unwrap();
        state.set_deployment_log_path("dep-success", &logs).unwrap();
        stdout.write_all(b"stdout").unwrap();
        stderr.write_all(b"stderr").unwrap();
        stdout.sync_all().unwrap();
        stderr.sync_all().unwrap();

        assert_eq!(logs, artifacts.join("logs/dep-success"));
        assert_eq!(
            state.deployment_logs_dir("dep-success").unwrap(),
            Some(logs.clone())
        );
        assert_eq!(fs::read(logs.join("build.stdout.log")).unwrap(), b"stdout");
        assert_eq!(
            fs::metadata(logs.join("build.stdout.log")).unwrap().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(logs.join("build.stderr.log")).unwrap().mode() & 0o777,
            0o600
        );
        assert!(!artifacts.join("c".repeat(64)).join("logs").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn build_logs_are_readable_incrementally_while_rootless_job_runs() {
        let root = temp_dir("live-build-logs");
        let artifacts = fs::canonicalize(&root).unwrap();
        let id = format!("live-{}", new_deployment_id());
        let mut state = State::open(root.join("state.db")).unwrap();
        register_engine(
            &mut state,
            "fixture",
            "/",
            fs::canonicalize("/bin/sh").unwrap(),
        )
        .unwrap();
        state
            .begin_deployment(&DeploymentInput {
                id: id.clone(),
                app: "live-app".into(),
                source_hash: "b".repeat(64),
                engine_version: "fixture".into(),
            })
            .unwrap();
        let logs = artifacts.join(LOG_REL).join(&id);
        let (stdout, stderr) = prepare_live_logs(&artifacts, &id).unwrap();
        state.set_deployment_log_path(&id, &logs).unwrap();

        let (finished_tx, finished_rx) = mpsc::channel();
        let handle = thread::spawn(move || {
            let result = Command::new("/bin/sh")
                .args(["-c", "printf first; sleep 0.4; printf second"])
                .stdout(Stdio::from(stdout))
                .stderr(Stdio::from(stderr))
                .spawn()
                .and_then(|mut child| child.wait());
            finished_tx.send(result).unwrap();
        });

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if fs::read(logs.join("build.stdout.log"))
                .is_ok_and(|bytes| bytes.starts_with(b"first"))
            {
                break;
            }
            match finished_rx.try_recv() {
                Ok(result) => panic!("job completed before first chunk was observed: {result:?}"),
                Err(mpsc::TryRecvError::Disconnected) => panic!("fixture job disconnected"),
                Err(mpsc::TryRecvError::Empty) => {}
            }
            assert!(Instant::now() < deadline, "first log chunk was not visible");
            thread::sleep(Duration::from_millis(5));
        }

        let deployment = state.deployment(&id).unwrap().unwrap();
        assert_eq!(deployment.status, DeploymentStatus::Building);
        assert_eq!(deployment.log_path, Some(logs.clone()));
        assert!(matches!(
            finished_rx.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));

        let completion = finished_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("fixture job completion")
            .expect("fixture job succeeds");
        assert!(completion.success());
        handle.join().unwrap();
        assert_eq!(
            fs::read(logs.join("build.stdout.log")).unwrap(),
            b"firstsecond"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn publication_reuses_a_valid_interrupted_artifact_without_rewriting_it() {
        let root = temp_dir("publication-recovery");
        let first = root.join("first");
        fs::create_dir_all(first.join("app")).unwrap();
        fs::write(first.join("app/index.js"), b"compiled").unwrap();
        let hash = hash_manifest(&build_manifest(&first).unwrap());
        let metadata_json = format!(
            "{{\"sourceHash\":\"{}\",\"artifactHash\":\"{hash}\",\"bunVersion\":\"bun\",\"runtimeEntry\":\"/app/index.js\"}}",
            "b".repeat(64)
        );
        fs::create_dir_all(first.join("meta")).unwrap();
        fs::write(first.join("meta/meta.json"), &metadata_json).unwrap();
        let final_path = root.join(&hash);
        assert!(!publish_or_reuse(&first, &final_path, &hash, &metadata_json).unwrap());
        let inode = fs::metadata(&final_path).unwrap().ino();

        let recovered = root.join("recovered");
        write_publish_fixture(&recovered, &metadata_json);
        assert!(publish_or_reuse(&recovered, &final_path, &hash, &metadata_json).unwrap());
        assert_eq!(fs::metadata(&final_path).unwrap().ino(), inode);
        assert!(!recovered.exists());
        remove_read_only_tree(&final_path).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn failure_cleanup_never_moves_a_content_addressed_publication() {
        let root = temp_dir("sealed-failure-cleanup");
        let artifacts = root.join("artifacts");
        fs::create_dir_all(&artifacts).unwrap();
        let final_path = artifacts.join("c".repeat(64));
        fs::create_dir_all(&final_path).unwrap();
        fs::write(final_path.join("sentinel"), b"sealed").unwrap();

        let mut state = State::open(root.join("state.db")).unwrap();
        register_engine(
            &mut state,
            "shell",
            "/",
            fs::canonicalize("/bin/sh").unwrap(),
        )
        .unwrap();
        state
            .begin_deployment(&DeploymentInput {
                id: "dep-cleanup".into(),
                app: "api".into(),
                source_hash: "b".repeat(64),
                engine_version: "shell".into(),
            })
            .unwrap();
        let logs = artifacts.join("logs/dep-cleanup");
        let (mut stdout, mut stderr) = prepare_live_logs(&artifacts, "dep-cleanup").unwrap();
        stdout.write_all(b"out").unwrap();
        stderr.write_all(b"err").unwrap();

        let _ = fail_build(
            &mut state,
            &artifacts,
            &final_path,
            &logs,
            "dep-cleanup",
            "state seal failed".into(),
        );
        assert_eq!(fs::read(final_path.join("sentinel")).unwrap(), b"sealed");
        fs::remove_dir_all(root).unwrap();
    }

    fn write_publish_fixture(path: &Path, metadata_json: &str) {
        fs::create_dir_all(path.join("app")).unwrap();
        fs::create_dir_all(path.join("meta")).unwrap();
        fs::write(path.join("app/index.js"), b"compiled").unwrap();
        fs::write(path.join("meta/meta.json"), metadata_json).unwrap();
    }
}
