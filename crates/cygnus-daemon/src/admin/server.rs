use parking_lot::{Condvar, Mutex};
use std::collections::VecDeque;
use std::fs;
use std::io::{self, ErrorKind};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;

use super::protocol::{
    ADMIN_PROTOCOL_VERSION, AdminCommand, AdminErrorCode, AdminRequest, AdminResponse,
    MAX_ADMIN_FRAME_BYTES, MAX_LOG_CHUNK_BYTES, read_frame, write_frame,
};
use crate::deploy::upload::{
    MAX_UPLOAD_BYTES, MAX_UPLOAD_CHUNK_BASE64_CHARS, MAX_UPLOAD_CHUNK_BYTES,
};
use crate::state::{
    MAX_ACCOUNT_EMAIL_BYTES, MAX_ACCOUNT_PASSWORD_BYTES, MIN_ACCOUNT_PASSWORD_BYTES, NodeConfig,
};

/// The listener through which an admin request arrived.
///
/// The request actor is audit attribution only. Authorization is based on this
/// listener-assigned role, which a client cannot supply.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AdminRole {
    Host,
    TenantZero,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AdminPeerCredentials {
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub pid: Option<u32>,
}

/// The state-backed implementation of the admin protocol.
pub trait AdminHandler: Send + Sync + 'static {
    fn handle(
        &self,
        role: AdminRole,
        peer: AdminPeerCredentials,
        request: AdminRequest,
    ) -> AdminResponse;
}

pub const ADMIN_IO_TIMEOUT: Duration = Duration::from_secs(30);
pub const DEFAULT_ADMIN_WORKERS: usize = 4;
pub const DEFAULT_ADMIN_QUEUE_CAPACITY: usize = 64;
pub const MAX_ADMIN_ACTOR_BYTES: usize = 128;
pub const MAX_ADMIN_APP_BYTES: usize = 128;
pub const MAX_ADMIN_DOMAIN_BYTES: usize = 253;
pub const MAX_ADMIN_DEPLOYMENT_BYTES: usize = 128;
pub const MAX_ADMIN_LIST_LIMIT: u16 = 50;
pub const MAX_ADMIN_METRICS_LIST_LIMIT: u16 = 500;

/// Prepare a daemon-owned admin socket path.
///
/// Parent directories are created as needed. An existing socket is removed only
/// when it is stale (a connect attempt is refused); every other existing file,
/// including symlinks, is preserved and rejected.
pub fn prepare_listener(path: impl AsRef<Path>) -> io::Result<UnixListener> {
    let path = path.as_ref();
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            ErrorKind::InvalidInput,
            format!("admin socket path {} has no parent", path.display()),
        )
    })?;
    prepare_private_parent(parent)?;
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.file_type().is_socket() {
                return Err(io::Error::new(
                    ErrorKind::AlreadyExists,
                    format!("admin socket path {} is not a socket", path.display()),
                ));
            }
            match UnixStream::connect(path) {
                Ok(_) => {
                    return Err(io::Error::new(
                        ErrorKind::AddrInUse,
                        format!("admin socket path {} is already in use", path.display()),
                    ));
                }
                Err(error) if error.kind() == ErrorKind::ConnectionRefused => {
                    fs::remove_file(path)?;
                }
                Err(error) => return Err(error),
            }
        }
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }

    let listener = UnixListener::bind(path)?;
    if let Err(error) = fs::set_permissions(path, fs::Permissions::from_mode(0o600)) {
        let _ = fs::remove_file(path);
        return Err(error);
    }
    Ok(listener)
}

fn prepare_private_parent(parent: &Path) -> io::Result<()> {
    match fs::symlink_metadata(parent) {
        Ok(_) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => {
            fs::create_dir_all(parent)?;
            fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
        }
        Err(error) => return Err(error),
    }
    let metadata = fs::symlink_metadata(parent)?;
    if !metadata.file_type().is_dir() {
        return Err(io::Error::new(
            ErrorKind::PermissionDenied,
            format!(
                "admin socket parent {} is not a directory",
                parent.display()
            ),
        ));
    }
    let daemon_uid = unsafe { libc::geteuid() };
    if metadata.uid() != daemon_uid || metadata.mode() & 0o077 != 0 {
        return Err(io::Error::new(
            ErrorKind::PermissionDenied,
            format!(
                "admin socket parent {} must be owned by daemon uid {daemon_uid} with mode 0700",
                parent.display()
            ),
        ));
    }
    Ok(())
}

/// One role-bound admin listener. The role is never read from request JSON.
pub struct AdminBinding {
    listener: UnixListener,
    role: AdminRole,
    path: PathBuf,
    identity: Option<(u64, u64)>,
}

impl AdminBinding {
    /// Prepare and bind a role-bound listener.
    pub fn bind(path: impl AsRef<Path>, role: AdminRole) -> io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let listener = prepare_listener(&path)?;
        Self::from_listener(listener, role, path)
    }

    /// Adopt an already-bound listener, retaining its path for cleanup.
    pub fn from_listener(
        listener: UnixListener,
        role: AdminRole,
        path: impl Into<PathBuf>,
    ) -> io::Result<Self> {
        listener.set_nonblocking(true)?;
        let path = path.into();
        let identity = fs::symlink_metadata(&path)
            .ok()
            .filter(|metadata| metadata.file_type().is_socket())
            .map(|metadata| (metadata.dev(), metadata.ino()));
        Ok(Self {
            listener,
            role,
            path,
            identity,
        })
    }

    pub fn role(&self) -> AdminRole {
        self.role
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for AdminBinding {
    fn drop(&mut self) {
        let current = fs::symlink_metadata(&self.path)
            .ok()
            .filter(|metadata| metadata.file_type().is_socket())
            .map(|metadata| (metadata.dev(), metadata.ino()));
        if current.is_some() && current == self.identity {
            let _ = fs::remove_file(&self.path);
        }
    }
}

struct WorkQueue {
    state: Mutex<QueueState>,
    wake: Condvar,
    capacity: usize,
}

struct QueueState {
    pending: VecDeque<(UnixStream, AdminRole)>,
    closed: bool,
}

impl WorkQueue {
    fn new(capacity: usize) -> Self {
        Self {
            state: Mutex::new(QueueState {
                pending: VecDeque::with_capacity(capacity),
                closed: false,
            }),
            wake: Condvar::new(),
            capacity,
        }
    }

    fn push(&self, stream: UnixStream, role: AdminRole) -> bool {
        let mut state = self.state.lock();
        if state.closed || state.pending.len() >= self.capacity {
            return false;
        }
        state.pending.push_back((stream, role));
        self.wake.notify_one();
        true
    }

    fn pop(&self) -> Option<(UnixStream, AdminRole)> {
        let mut state = self.state.lock();
        loop {
            if let Some(item) = state.pending.pop_front() {
                return Some(item);
            }
            if state.closed {
                return None;
            }
            self.wake.wait(&mut state);
        }
    }

    fn close(&self) {
        let mut state = self.state.lock();
        state.closed = true;
        state.pending.clear();
        self.wake.notify_all();
    }
}

/// A bounded, fixed-worker admin UDS server.
pub struct AdminServer {
    bindings: Vec<AdminBinding>,
    handler: Arc<dyn AdminHandler>,
    worker_count: usize,
    queue_capacity: usize,
}

impl AdminServer {
    pub fn new(bindings: Vec<AdminBinding>, handler: Arc<dyn AdminHandler>) -> Self {
        Self {
            bindings,
            handler,
            worker_count: DEFAULT_ADMIN_WORKERS,
            queue_capacity: DEFAULT_ADMIN_QUEUE_CAPACITY,
        }
    }

    pub fn with_limits(
        bindings: Vec<AdminBinding>,
        handler: Arc<dyn AdminHandler>,
        worker_count: usize,
        queue_capacity: usize,
    ) -> io::Result<Self> {
        if bindings.is_empty() {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                "admin server has no listeners",
            ));
        }
        if worker_count == 0 || queue_capacity == 0 {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                "admin worker and queue counts must be non-zero",
            ));
        }
        Ok(Self {
            bindings,
            handler,
            worker_count,
            queue_capacity,
        })
    }

    /// Accept and serve until `shutdown` is set. All workers are joined and all
    /// socket paths are removed before this method returns.
    pub fn serve(self, shutdown: Arc<AtomicBool>) -> io::Result<()> {
        self.serve_until(&shutdown)
    }

    /// Borrowed-flag variant useful to callers that already own the flag.
    pub fn serve_until(mut self, shutdown: &AtomicBool) -> io::Result<()> {
        if self.bindings.is_empty() {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                "admin server has no listeners",
            ));
        }

        let queue = Arc::new(WorkQueue::new(self.queue_capacity));
        let mut workers = Vec::with_capacity(self.worker_count);
        for _ in 0..self.worker_count {
            let queue = Arc::clone(&queue);
            let handler = Arc::clone(&self.handler);
            workers.push(thread::spawn(move || worker_loop(queue, handler)));
        }

        let accept_result = self.accept_loop(&queue, shutdown);
        queue.close();
        join_workers(workers);
        self.cleanup_paths();
        accept_result
    }

    fn accept_loop(&mut self, queue: &WorkQueue, shutdown: &AtomicBool) -> io::Result<()> {
        while !shutdown.load(Ordering::Acquire) {
            let mut accepted = false;
            for binding in &self.bindings {
                match binding.listener.accept() {
                    Ok((stream, _)) => {
                        accepted = true;
                        if stream.set_nonblocking(false).is_err() {
                            continue;
                        }
                        let _ = stream.set_read_timeout(Some(ADMIN_IO_TIMEOUT));
                        let _ = stream.set_write_timeout(Some(ADMIN_IO_TIMEOUT));
                        if peer_uid_matches(&stream)? {
                            // A full queue deliberately closes this connection. There is
                            // no unbounded fallback that could exhaust daemon memory.
                            let _ = queue.push(stream, binding.role);
                        }
                    }
                    Err(error) if error.kind() == ErrorKind::WouldBlock => {}
                    Err(error) if error.kind() == ErrorKind::Interrupted => {}
                    Err(error) => return Err(error),
                }
            }
            if !accepted {
                thread::sleep(Duration::from_millis(5));
            }
        }
        Ok(())
    }

    fn cleanup_paths(&mut self) {
        for binding in self.bindings.drain(..) {
            drop(binding);
        }
    }
}

impl Drop for AdminServer {
    fn drop(&mut self) {
        for binding in self.bindings.drain(..) {
            drop(binding);
        }
    }
}

fn join_workers(workers: Vec<JoinHandle<()>>) {
    for worker in workers {
        let _ = worker.join();
    }
}

fn worker_loop(queue: Arc<WorkQueue>, handler: Arc<dyn AdminHandler>) {
    while let Some((mut stream, role)) = queue.pop() {
        let _ = stream.set_read_timeout(Some(ADMIN_IO_TIMEOUT));
        let _ = stream.set_write_timeout(Some(ADMIN_IO_TIMEOUT));
        serve_connection(&mut stream, role, handler.as_ref());
    }
}

fn serve_connection(stream: &mut UnixStream, role: AdminRole, handler: &dyn AdminHandler) {
    let request = match read_frame::<AdminRequest>(stream) {
        Ok(request) => request,
        Err(_) => {
            let _ = write_frame(
                stream,
                &AdminResponse::error(
                    super::client::invalid_request_id(),
                    AdminErrorCode::InvalidRequest,
                    "invalid admin request frame",
                ),
            );
            return;
        }
    };

    if request.version != ADMIN_PROTOCOL_VERSION {
        let _ = write_frame(
            stream,
            &AdminResponse::error(
                request.request_id,
                AdminErrorCode::UnsupportedVersion,
                format!("unsupported admin protocol version {}", request.version),
            ),
        );
        return;
    }

    if let Err(message) = authorize_actor(role, &request) {
        let _ = write_frame(
            stream,
            &AdminResponse::error(request.request_id, AdminErrorCode::Unauthorized, message),
        );
        return;
    }

    if let Err(message) = validate_request(&request) {
        let _ = write_frame(
            stream,
            &AdminResponse::error(request.request_id, AdminErrorCode::Validation, message),
        );
        return;
    }

    let peer = match peer_credentials(stream) {
        Ok(peer) => peer,
        Err(_) => {
            let _ = write_frame(
                stream,
                &AdminResponse::error(
                    request.request_id,
                    AdminErrorCode::Unauthorized,
                    "admin peer credentials unavailable",
                ),
            );
            return;
        }
    };

    let response = handler.handle(role, peer, request);
    let _ = write_frame(stream, &response);
}

fn validate_request(request: &AdminRequest) -> Result<(), String> {
    if !super::protocol::valid_request_id(&request.request_id) {
        return Err("request_id must be exactly 32 lowercase hexadecimal characters".into());
    }
    if let Some(actor) = request.actor.as_deref() {
        validate_actor(actor)?;
    }
    match &request.command {
        AdminCommand::Health
        | AdminCommand::AccountStatus
        | AdminCommand::Status
        | AdminCommand::GetMetrics => {}
        AdminCommand::SetDashboardDomain { domain, apex } => {
            if let Some(domain) = domain {
                validate_text(domain, MAX_ADMIN_DOMAIN_BYTES, "dashboard domain")?;
            }
            if let Some(apex) = apex {
                validate_text(apex, MAX_ADMIN_DOMAIN_BYTES, "apex domain")?;
            }
        }
        AdminCommand::SetDashboardTls { .. } => {}
        AdminCommand::ListAppDomains { app } => validate_app_name(app)?,
        AdminCommand::AddAppDomain { app, host }
        | AdminCommand::RemoveAppDomain { app, host }
        | AdminCommand::SetAppDomainTls { app, host, .. }
        | AdminCommand::SetPrimaryDomain { app, host }
        | AdminCommand::RetryDomainAcme { app, host } => {
            validate_app_name(app)?;
            validate_text(host, MAX_ADMIN_DOMAIN_BYTES, "host")?;
        }
        AdminCommand::ListEnvVars { app } => validate_app_name(app)?,
        AdminCommand::SetEnvVar { app, key, value } => {
            validate_app_name(app)?;
            let mut env = std::collections::BTreeMap::new();
            env.insert(key.clone(), value.clone());
            validate_env_vars(&env)?;
        }
        AdminCommand::RemoveEnvVar { app, key } => {
            validate_app_name(app)?;
            validate_text(key, MAX_ADMIN_APP_BYTES, "env var key")?;
        }
        AdminCommand::CreateInitialAccount { email, password }
        | AdminCommand::VerifyCredentials { email, password } => {
            validate_account_email(email)?;
            validate_account_password(password)?;
        }
        AdminCommand::ChangePassword {
            email,
            current_password,
            new_password,
        } => {
            validate_account_email(email)?;
            validate_account_password(current_password)?;
            validate_account_password(new_password)?;
        }
        AdminCommand::ListRequests { limit } | AdminCommand::ListEvents { limit } => {
            if !(1..=MAX_ADMIN_METRICS_LIST_LIMIT).contains(limit) {
                return Err(format!(
                    "metrics list limit must be between 1 and {MAX_ADMIN_METRICS_LIST_LIMIT}"
                ));
            }
        }
        AdminCommand::ReadAppLog { app, limit, .. } => {
            validate_app_name(app)?;
            if !(1..=MAX_LOG_CHUNK_BYTES).contains(limit) {
                return Err(format!(
                    "log limit must be between 1 and {MAX_LOG_CHUNK_BYTES}"
                ));
            }
        }
        AdminCommand::ListApps { cursor, limit } => {
            if let Some(cursor) = cursor.as_deref() {
                validate_text(cursor, MAX_ADMIN_DEPLOYMENT_BYTES, "cursor")?;
            }
            validate_list_limit(*limit)?;
        }
        AdminCommand::GetApp { app } => validate_text(app, MAX_ADMIN_APP_BYTES, "app")?,
        AdminCommand::ListDeployments { app, cursor, limit } => {
            if let Some(app) = app.as_deref() {
                validate_text(app, MAX_ADMIN_APP_BYTES, "app")?;
            }
            if let Some(cursor) = cursor.as_deref() {
                validate_text(cursor, MAX_ADMIN_DEPLOYMENT_BYTES, "cursor")?;
            }
            validate_list_limit(*limit)?;
        }
        AdminCommand::GetDeployment { deployment } => {
            validate_text(deployment, MAX_ADMIN_DEPLOYMENT_BYTES, "deployment")?;
        }
        AdminCommand::ApplyConfig(config) => validate_node_config(config)?,
        AdminCommand::RegisterEngine {
            version,
            host_root,
            cage_executable,
            ..
        } => {
            validate_text(version, MAX_ADMIN_APP_BYTES, "engine version")?;
            validate_path(host_root, MAX_ADMIN_DEPLOYMENT_BYTES, "engine host root")?;
            validate_path(
                cage_executable,
                MAX_ADMIN_DEPLOYMENT_BYTES,
                "engine cage executable",
            )?;
        }
        AdminCommand::Deploy { request } | AdminCommand::DeployStart { request } => {
            validate_app_name(&request.app)?;
            if let Some(domain) = request.domain.as_deref() {
                validate_text(domain, MAX_ADMIN_DOMAIN_BYTES, "domain")?;
            }
            if let Some(engine_version) = request.engine_version.as_deref() {
                validate_text(engine_version, MAX_ADMIN_APP_BYTES, "engine version")?;
            }
            validate_path(
                &request.source_dir,
                MAX_ADMIN_DEPLOYMENT_BYTES,
                "source directory",
            )?;
            if let Some(entry) = request.entry.as_deref() {
                validate_path(entry, MAX_ADMIN_DEPLOYMENT_BYTES, "entry")?;
            }
            if let Some(artifact_root) = request.artifact_root.as_deref() {
                validate_path(artifact_root, MAX_ADMIN_DEPLOYMENT_BYTES, "artifact root")?;
            }
            if let Some(upstream) = request.upstream.as_deref() {
                validate_path(upstream, MAX_ADMIN_DEPLOYMENT_BYTES, "upstream")?;
            }
            validate_env_vars(&request.env)?;
            if let Some(preview) = request.preview.as_deref() {
                validate_preview_slug(preview)?;
            }
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
            validate_app_name(app)?;
            if let Some(domain) = domain.as_deref() {
                validate_text(domain, MAX_ADMIN_DOMAIN_BYTES, "domain")?;
            }
            if let Some(engine_version) = engine_version.as_deref() {
                validate_text(engine_version, MAX_ADMIN_APP_BYTES, "engine version")?;
            }
            if let Some(entry) = entry.as_deref() {
                validate_relative_path(entry, MAX_ADMIN_DEPLOYMENT_BYTES, "entry")?;
            }
            if !(1..=MAX_UPLOAD_BYTES).contains(total_bytes) {
                return Err("total_bytes must be between 1 byte and 64 MiB".into());
            }
            validate_env_vars(env)?;
            if let Some(preview) = preview.as_deref() {
                validate_preview_slug(preview)?;
            }
        }
        AdminCommand::DeployUploadChunk {
            upload_id,
            chunk_base64,
        } => {
            validate_upload_id(upload_id)?;
            if chunk_base64.is_empty() || chunk_base64.len() > MAX_UPLOAD_CHUNK_BASE64_CHARS {
                return Err("upload chunk exceeds the frame-safe base64 limit".into());
            }
            let decoded = BASE64_STANDARD
                .decode(chunk_base64)
                .map_err(|_| "upload chunk is not valid base64".to_owned())?;
            if decoded.is_empty() || decoded.len() > MAX_UPLOAD_CHUNK_BYTES {
                return Err("upload chunk must decode to between 1 byte and 48 KiB".into());
            }
        }
        AdminCommand::DeployUploadFinish { upload_id } => validate_upload_id(upload_id)?,
        AdminCommand::MapDomain { app, domain } => {
            validate_text(app, MAX_ADMIN_APP_BYTES, "app")?;
            validate_text(domain, MAX_ADMIN_DOMAIN_BYTES, "domain")?;
        }
        AdminCommand::ConvertManifest { code, owner } => {
            validate_text(code, 512, "manifest code")?;
            if let Some(owner) = owner.as_deref() {
                validate_text(owner, MAX_ADMIN_APP_BYTES, "owner")?;
            }
        }
        AdminCommand::GitHubStatus => {}
        AdminCommand::ListRepositories { limit } => validate_list_limit(*limit)?,
        AdminCommand::ConfigureRepository { repository } => {
            validate_text(&repository.owner, MAX_ADMIN_APP_BYTES, "repository owner")?;
            validate_text(&repository.name, MAX_ADMIN_APP_BYTES, "repository name")?;
            validate_text(&repository.branch, MAX_ADMIN_APP_BYTES, "repository branch")?;
            validate_text(&repository.app, MAX_ADMIN_APP_BYTES, "app")?;
            if let Some(domain) = repository.domain.as_deref() {
                validate_text(domain, MAX_ADMIN_DOMAIN_BYTES, "domain")?;
            }
            if let Some(engine_version) = repository.engine_version.as_deref() {
                validate_text(engine_version, MAX_ADMIN_APP_BYTES, "engine version")?;
            }
            if let Some(entry) = repository.entry.as_deref() {
                validate_path(entry, MAX_ADMIN_DEPLOYMENT_BYTES, "entry")?;
            }
            if repository.installation_id <= 0 || repository.repository_id <= 0 {
                return Err("installation and repository ids must be positive".into());
            }
        }
        AdminCommand::ListInstallationRepositories { installation_id } => {
            if *installation_id <= 0 {
                return Err("installation id must be positive".into());
            }
        }
        AdminCommand::ListDiscoverableRepositories => {}
        AdminCommand::WebhookBegin {
            delivery_id,
            event,
            signature,
            total_bytes,
        } => {
            validate_text(delivery_id, MAX_ADMIN_DEPLOYMENT_BYTES, "delivery id")?;
            validate_text(event, MAX_ADMIN_APP_BYTES, "webhook event")?;
            validate_text(signature, 128, "webhook signature")?;
            if *total_bytes == 0 || *total_bytes > crate::state::MAX_GITHUB_WEBHOOK_BYTES {
                return Err("webhook body exceeds 25 MiB".into());
            }
        }
        AdminCommand::WebhookChunk {
            delivery_id,
            chunk_base64,
        } => {
            validate_text(delivery_id, MAX_ADMIN_DEPLOYMENT_BYTES, "delivery id")?;
            validate_text(
                chunk_base64,
                MAX_ADMIN_FRAME_BYTES.saturating_sub(256),
                "webhook chunk",
            )?;
        }
        AdminCommand::WebhookFinish { delivery_id } => {
            validate_text(delivery_id, MAX_ADMIN_DEPLOYMENT_BYTES, "delivery id")?
        }
        AdminCommand::ListDeployJobs { cursor, limit } => {
            if let Some(cursor) = cursor.as_deref() {
                validate_text(cursor, MAX_ADMIN_DEPLOYMENT_BYTES, "cursor")?;
            }
            validate_list_limit(*limit)?;
        }
        AdminCommand::RetryDeployJob { job_id } => {
            validate_text(job_id, MAX_ADMIN_DEPLOYMENT_BYTES, "job id")?
        }
        AdminCommand::Rollback {
            app,
            deployment,
            expected_active_artifact,
        } => {
            validate_text(app, MAX_ADMIN_APP_BYTES, "app")?;
            validate_text(deployment, MAX_ADMIN_DEPLOYMENT_BYTES, "deployment")?;
            if !valid_sha256(expected_active_artifact) {
                return Err(
                    "expected_active_artifact must be 64 lowercase hexadecimal characters".into(),
                );
            }
        }
        AdminCommand::ReadLog {
            deployment, limit, ..
        } => {
            validate_text(deployment, MAX_ADMIN_DEPLOYMENT_BYTES, "deployment")?;
            if !(1..=MAX_LOG_CHUNK_BYTES).contains(limit) {
                return Err(format!(
                    "log limit must be between 1 and {MAX_LOG_CHUNK_BYTES}"
                ));
            }
        }
    }
    Ok(())
}

fn authorize_actor(role: AdminRole, request: &AdminRequest) -> Result<(), String> {
    let host_only = matches!(
        request.command,
        AdminCommand::ApplyConfig(_)
            | AdminCommand::RegisterEngine { .. }
            | AdminCommand::Deploy { .. }
            | AdminCommand::DeployStart { .. }
    );
    if host_only && role != AdminRole::Host {
        return Err("command is restricted to the host admin listener".into());
    }
    let account_auth = matches!(
        request.command,
        AdminCommand::AccountStatus
            | AdminCommand::CreateInitialAccount { .. }
            | AdminCommand::VerifyCredentials { .. }
    );
    if account_auth {
        if role != AdminRole::TenantZero {
            return Err("command is restricted to the Tenant Zero admin listener".into());
        }
        return if request.actor.is_none() {
            Ok(())
        } else {
            Err("account authentication requests must not supply an actor".into())
        };
    }
    match (role, request.actor.as_deref()) {
        (AdminRole::Host, None) => Ok(()),
        (AdminRole::Host, Some(_)) => Err("host admin requests must not supply an actor".into()),
        (AdminRole::TenantZero, Some(actor)) => validate_actor(actor),
        (AdminRole::TenantZero, None) => {
            Err("Tenant Zero admin requests require an authenticated actor".into())
        }
    }
}

fn validate_account_email(email: &str) -> Result<(), String> {
    let email = email.trim();
    if email.is_empty() || email.len() > MAX_ACCOUNT_EMAIL_BYTES {
        return Err(format!(
            "email must be between 1 and {MAX_ACCOUNT_EMAIL_BYTES} bytes"
        ));
    }
    if email
        .chars()
        .any(|character| character.is_control() || character.is_whitespace())
    {
        return Err("email contains whitespace or control characters".into());
    }
    let Some((local, domain)) = email.split_once('@') else {
        return Err("email must contain a local part and domain".into());
    };
    if local.is_empty() || domain.is_empty() || domain.contains('@') {
        return Err("email must contain a local part and domain".into());
    }
    Ok(())
}

fn validate_account_password(password: &str) -> Result<(), String> {
    if !(MIN_ACCOUNT_PASSWORD_BYTES..=MAX_ACCOUNT_PASSWORD_BYTES).contains(&password.len()) {
        return Err(format!(
            "password must be between {MIN_ACCOUNT_PASSWORD_BYTES} and {MAX_ACCOUNT_PASSWORD_BYTES} bytes"
        ));
    }
    if password.chars().any(char::is_control) {
        return Err("password contains control characters".into());
    }
    Ok(())
}

fn validate_actor(actor: &str) -> Result<(), String> {
    validate_text(actor, MAX_ADMIN_ACTOR_BYTES, "actor")?;
    let mut bytes = actor.bytes();
    if !bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_alphanumeric())
        || !bytes.all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'@' | b':' | b'-')
        })
    {
        return Err("actor contains unsupported characters".into());
    }
    Ok(())
}

fn validate_app_name(app: &str) -> Result<(), String> {
    if app.is_empty() || app.len() > MAX_ADMIN_APP_BYTES {
        return Err(format!(
            "app name must be between 1 and {MAX_ADMIN_APP_BYTES} bytes"
        ));
    }
    let mut bytes = app.bytes();
    if !bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_alphanumeric())
        || !bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err("app name contains unsupported characters".into());
    }
    Ok(())
}

fn validate_list_limit(limit: u16) -> Result<(), String> {
    if !(1..=MAX_ADMIN_LIST_LIMIT).contains(&limit) {
        return Err(format!(
            "list limit must be between 1 and {MAX_ADMIN_LIST_LIMIT}"
        ));
    }
    Ok(())
}

fn validate_text(value: &str, max_bytes: usize, field: &str) -> Result<(), String> {
    if value.is_empty() {
        return Err(format!("{field} must not be empty"));
    }
    if value.len() > max_bytes {
        return Err(format!("{field} exceeds {max_bytes} bytes"));
    }
    if value.chars().any(|character| character.is_control()) {
        return Err(format!("{field} contains control characters"));
    }
    Ok(())
}
fn validate_path(path: &Path, max_bytes: usize, field: &str) -> Result<(), String> {
    validate_text(&path.to_string_lossy(), max_bytes, field)
}

fn validate_env_vars(env: &std::collections::BTreeMap<String, String>) -> Result<(), String> {
    const MAX_ENV_VARS_PER_REQUEST: usize = 100;
    if env.len() > MAX_ENV_VARS_PER_REQUEST {
        return Err(format!(
            "env vars exceed {MAX_ENV_VARS_PER_REQUEST} entries per request"
        ));
    }
    for (key, value) in env {
        validate_text(key, MAX_ADMIN_APP_BYTES, "env var key")?;
        if value.len() > crate::state::MAX_ENV_VAR_VALUE_BYTES {
            return Err(format!(
                "env var {key:?} value exceeds {} bytes",
                crate::state::MAX_ENV_VAR_VALUE_BYTES
            ));
        }
        if value.bytes().any(|byte| byte == 0) {
            return Err(format!("env var {key:?} value contains a null byte"));
        }
    }
    Ok(())
}

fn validate_preview_slug(slug: &str) -> Result<(), String> {
    if slug.is_empty() || slug.len() > MAX_ADMIN_APP_BYTES {
        return Err(format!(
            "preview slug must be between 1 and {MAX_ADMIN_APP_BYTES} bytes"
        ));
    }
    if slug == "." || slug == ".." {
        return Err("preview slug must not be \".\" or \"..\"".into());
    }
    if !slug
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err("preview slug contains unsupported characters".into());
    }
    Ok(())
}

fn validate_relative_path(path: &Path, max_bytes: usize, field: &str) -> Result<(), String> {
    validate_path(path, max_bytes, field)?;
    if path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                std::path::Component::CurDir
                    | std::path::Component::ParentDir
                    | std::path::Component::RootDir
                    | std::path::Component::Prefix(_)
            )
        })
    {
        return Err(format!("{field} must be a relative path without traversal"));
    }
    Ok(())
}

fn validate_upload_id(upload_id: &str) -> Result<(), String> {
    if !valid_sha256(upload_id) {
        return Err("upload_id must be exactly 64 lowercase hexadecimal characters".into());
    }
    Ok(())
}

fn validate_node_config(config: &NodeConfig) -> Result<(), String> {
    let encoded = serde_json::to_vec(config).map_err(|_| "config cannot be encoded")?;
    if encoded.len() > MAX_ADMIN_FRAME_BYTES {
        return Err(format!("config exceeds {MAX_ADMIN_FRAME_BYTES} bytes"));
    }
    if config.apps.len() > 256 {
        return Err("config contains too many apps".into());
    }
    for app in &config.apps {
        validate_text(&app.name, MAX_ADMIN_APP_BYTES, "app")?;
        validate_path(&app.upstream, MAX_ADMIN_DEPLOYMENT_BYTES, "upstream")?;
        validate_text(&app.command, MAX_ADMIN_DEPLOYMENT_BYTES, "command")?;
        for domain in &app.domains {
            validate_text(domain, MAX_ADMIN_DOMAIN_BYTES, "domain")?;
        }
    }
    Ok(())
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Read peer credentials where the host exposes them.
fn peer_credentials(stream: &UnixStream) -> io::Result<AdminPeerCredentials> {
    let fd = stream.as_raw_fd();

    #[cfg(target_os = "linux")]
    {
        let mut credentials = libc::ucred {
            pid: 0,
            uid: 0,
            gid: 0,
        };
        let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        let result = unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&mut credentials as *mut libc::ucred).cast(),
                &mut length,
            )
        };
        if result != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(AdminPeerCredentials {
            uid: Some(credentials.uid),
            gid: Some(credentials.gid),
            pid: u32::try_from(credentials.pid).ok(),
        })
    }

    #[cfg(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd",
        target_os = "dragonfly"
    ))]
    {
        let mut uid = 0;
        let mut gid = 0;
        let result = unsafe { libc::getpeereid(fd, &mut uid, &mut gid) };
        if result != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(AdminPeerCredentials {
            uid: Some(uid),
            gid: Some(gid),
            pid: None,
        })
    }

    #[cfg(not(any(
        target_os = "linux",
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd",
        target_os = "dragonfly"
    )))]
    {
        let _ = fd;
        Ok(AdminPeerCredentials::default())
    }
}

/// Check the peer effective UID where the host operating system exposes it.
fn peer_uid_matches(stream: &UnixStream) -> io::Result<bool> {
    let daemon_uid = unsafe { libc::geteuid() };
    Ok(peer_credentials(stream)?
        .uid
        .is_none_or(|uid| uid == daemon_uid))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{self, File};
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, AtomicUsize};

    struct TestHandler {
        calls: Arc<AtomicUsize>,
        response: AdminResponse,
    }

    impl AdminHandler for TestHandler {
        fn handle(
            &self,
            _role: AdminRole,
            _peer: AdminPeerCredentials,
            _request: AdminRequest,
        ) -> AdminResponse {
            self.calls.fetch_add(1, Ordering::Relaxed);
            self.response.clone()
        }
    }

    static NEXT_TEMP_PATH: AtomicU64 = AtomicU64::new(1);

    fn temp_path(label: &str) -> PathBuf {
        let nonce = NEXT_TEMP_PATH.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("ca-{label}-{}-{nonce}", std::process::id()))
    }

    fn request() -> AdminRequest {
        AdminRequest {
            version: ADMIN_PROTOCOL_VERSION,
            request_id: "0123456789abcdef0123456789abcdef".into(),
            actor: None,
            command: AdminCommand::Health,
        }
    }

    #[test]
    fn stale_socket_removed_and_non_socket_preserved() {
        let root = temp_path("prepare");
        fs::create_dir_all(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let stale = root.join("stale.sock");
        let listener = UnixListener::bind(&stale).unwrap();
        drop(listener);
        let listener = prepare_listener(&stale).unwrap();
        drop(listener);
        assert!(stale.exists());
        fs::remove_file(&stale).unwrap();

        let collision = root.join("collision");
        File::create(&collision).unwrap();
        let error = prepare_listener(&collision).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::AlreadyExists);
        assert!(collision.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn insecure_parent_is_rejected_without_changing_permissions() {
        let root = temp_path("insecure-parent");
        fs::create_dir_all(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
        let error = prepare_listener(root.join("admin.sock")).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::PermissionDenied);
        assert_eq!(
            fs::metadata(&root).unwrap().permissions().mode() & 0o777,
            0o755
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn binding_drop_preserves_a_replacement_socket() {
        let root = temp_path("replacement");
        let socket = root.join("admin.sock");
        let binding = AdminBinding::bind(&socket, AdminRole::Host).unwrap();
        fs::remove_file(&socket).unwrap();
        let replacement = UnixListener::bind(&socket).unwrap();
        drop(binding);
        assert!(socket.exists());
        drop(replacement);
        fs::remove_file(&socket).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unsupported_version_does_not_reach_handler() {
        let root = temp_path("version");
        let socket = root.join("admin.sock");
        let calls = Arc::new(AtomicUsize::new(0));
        let handler: Arc<dyn AdminHandler> = Arc::new(TestHandler {
            calls: Arc::clone(&calls),
            response: AdminResponse::error(
                "0123456789abcdef0123456789abcdef",
                AdminErrorCode::Internal,
                "unused",
            ),
        });
        let binding = AdminBinding::bind(&socket, AdminRole::Host).unwrap();
        let server = AdminServer::with_limits(vec![binding], handler, 1, 1).unwrap();
        let shutdown = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&shutdown);
        let join = thread::spawn(move || server.serve(flag).unwrap());
        let mut stream = UnixStream::connect(&socket).unwrap();
        let mut bad = request();

        bad.version = ADMIN_PROTOCOL_VERSION + 1;
        write_frame(&mut stream, &bad).unwrap();
        let response: AdminResponse = read_frame(&mut stream).unwrap();
        assert!(
            matches!(response, AdminResponse::Error { error, .. } if error.code == AdminErrorCode::UnsupportedVersion)
        );
        assert_eq!(calls.load(Ordering::Relaxed), 0);
        shutdown.store(true, Ordering::Release);
        join.join().unwrap();
        assert!(!socket.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn endpoint_roles_enforce_actor_provenance() {
        let mut host = request();
        assert!(authorize_actor(AdminRole::Host, &host).is_ok());
        host.actor = Some("spoofed".into());
        assert!(authorize_actor(AdminRole::Host, &host).is_err());

        let mut tenant = request();
        assert!(authorize_actor(AdminRole::TenantZero, &tenant).is_err());
        tenant.actor = Some("github:user-1".into());
        assert!(authorize_actor(AdminRole::TenantZero, &tenant).is_ok());
        tenant.command = AdminCommand::Deploy {
            request: crate::deploy::DeployRequest::new(
                "/srv/source",
                "hello",
                "hello.apps.test",
                "1.3.14",
                "src/index.ts",
                "/var/lib/cygnus/artifacts",
                "/run/cygnus/hello.sock",
            ),
        };
        assert!(authorize_actor(AdminRole::TenantZero, &tenant).is_err());
        tenant.command = AdminCommand::ApplyConfig(NodeConfig::default());
        assert!(authorize_actor(AdminRole::TenantZero, &tenant).is_err());
        tenant.actor = Some("bad actor".into());
        assert!(authorize_actor(AdminRole::TenantZero, &tenant).is_err());
    }

    #[test]
    fn account_auth_commands_are_unauthenticated_only_on_tenant_zero() {
        for command in [
            AdminCommand::AccountStatus,
            AdminCommand::CreateInitialAccount {
                email: " Admin@Example.COM ".into(),
                password: "correct horse battery staple".into(),
            },
            AdminCommand::VerifyCredentials {
                email: "admin@example.com".into(),
                password: "correct horse battery staple".into(),
            },
        ] {
            let mut auth = request();
            auth.command = command;
            assert!(authorize_actor(AdminRole::TenantZero, &auth).is_ok());
            assert!(authorize_actor(AdminRole::Host, &auth).is_err());
            assert!(validate_request(&auth).is_ok());
            auth.actor = Some("account:1".into());
            assert!(authorize_actor(AdminRole::TenantZero, &auth).is_err());
        }
    }

    #[test]
    fn account_auth_validation_rejects_malformed_and_unbounded_inputs() {
        let mut auth = request();
        auth.command = AdminCommand::CreateInitialAccount {
            email: "not-an-email".into(),
            password: "correct horse battery staple".into(),
        };
        assert!(validate_request(&auth).is_err());

        auth.command = AdminCommand::CreateInitialAccount {
            email: "admin@example.com".into(),
            password: "short".into(),
        };
        assert!(validate_request(&auth).is_err());

        auth.command = AdminCommand::VerifyCredentials {
            email: format!("{}@example.com", "a".repeat(MAX_ACCOUNT_EMAIL_BYTES)),
            password: "correct horse battery staple".into(),
        };
        assert!(validate_request(&auth).is_err());

        auth.command = AdminCommand::VerifyCredentials {
            email: "admin@example.com".into(),
            password: "x".repeat(MAX_ACCOUNT_PASSWORD_BYTES + 1),
        };
        assert!(validate_request(&auth).is_err());
    }

    #[test]
    fn upload_commands_allow_both_roles_but_deploy_start_is_host_only() {
        let mut upload = request();
        upload.command = AdminCommand::DeployUploadBegin {
            app: "hello".into(),
            domain: None,
            engine_version: None,
            entry: Some("src/index.ts".into()),
            env: Default::default(),
            preview: None,
            total_bytes: MAX_UPLOAD_BYTES,
        };
        assert!(authorize_actor(AdminRole::Host, &upload).is_ok());
        assert!(validate_request(&upload).is_ok());
        upload.actor = Some("tenant:operator".into());
        assert!(authorize_actor(AdminRole::TenantZero, &upload).is_ok());

        upload.command = AdminCommand::DeployUploadChunk {
            upload_id: "a".repeat(64),
            chunk_base64: BASE64_STANDARD.encode([7_u8; 32]),
        };
        assert!(authorize_actor(AdminRole::TenantZero, &upload).is_ok());
        assert!(validate_request(&upload).is_ok());
        if let AdminCommand::DeployUploadChunk { chunk_base64, .. } = &mut upload.command {
            *chunk_base64 = "A".repeat(MAX_UPLOAD_CHUNK_BASE64_CHARS + 1);
        }
        assert!(validate_request(&upload).is_err());

        upload.command = AdminCommand::DeployUploadFinish {
            upload_id: "not-an-id".into(),
        };
        assert!(validate_request(&upload).is_err());

        upload.command = AdminCommand::DeployStart {
            request: crate::deploy::DeployRequest {
                source_dir: "/host/source".into(),
                app: "hello".into(),
                domain: None,
                engine_version: None,
                entry: None,
                artifact_root: None,
                upstream: None,
                env: Default::default(),
                preview: None,
                deployment_id: None,
                source: crate::state::DeploymentSource::cli(),
            },
        };
        assert!(authorize_actor(AdminRole::TenantZero, &upload).is_err());
        upload.actor = None;
        assert!(authorize_actor(AdminRole::Host, &upload).is_ok());
    }

    #[test]
    fn observability_commands_validate_bounds_and_allow_both_roles() {
        for command in [
            AdminCommand::GetMetrics,
            AdminCommand::ListRequests { limit: 500 },
            AdminCommand::ListEvents { limit: 500 },
            AdminCommand::ReadAppLog {
                app: "api-1_test.example".into(),
                stream: super::super::protocol::LogStream::Stdout,
                offset: 0,
                limit: MAX_LOG_CHUNK_BYTES,
            },
        ] {
            let mut request = request();
            request.command = command;
            assert!(authorize_actor(AdminRole::Host, &request).is_ok());
            assert!(validate_request(&request).is_ok());
            request.actor = Some("tenant:operator".into());
            assert!(authorize_actor(AdminRole::TenantZero, &request).is_ok());
        }

        let mut invalid = request();
        invalid.command = AdminCommand::ListRequests { limit: 501 };
        assert!(validate_request(&invalid).is_err());
        invalid.command = AdminCommand::ListEvents { limit: 0 };
        assert!(validate_request(&invalid).is_err());
        invalid.command = AdminCommand::ReadAppLog {
            app: "../escape".into(),
            stream: super::super::protocol::LogStream::Stderr,
            offset: 0,
            limit: 1,
        };
        assert!(validate_request(&invalid).is_err());
        invalid.command = AdminCommand::ReadAppLog {
            app: "api".into(),
            stream: super::super::protocol::LogStream::Stderr,
            offset: 0,
            limit: MAX_LOG_CHUNK_BYTES + 1,
        };
        assert!(validate_request(&invalid).is_err());
    }

    #[test]
    fn bounded_fields_are_rejected_before_handler() {
        let oversized = AdminRequest {
            version: ADMIN_PROTOCOL_VERSION,
            request_id: "0123456789abcdef0123456789abcdef".into(),
            actor: Some("x".repeat(MAX_ADMIN_ACTOR_BYTES + 1)),
            command: AdminCommand::Health,
        };
        let error = validate_request(&oversized).unwrap_err();
        assert!(error.contains("actor"));
        let oversized_log = AdminRequest {
            version: ADMIN_PROTOCOL_VERSION,
            request_id: "0123456789abcdef0123456789abcdef".into(),
            actor: None,
            command: AdminCommand::ReadLog {
                deployment: "deploy".into(),
                stream: super::super::protocol::LogStream::Stdout,
                offset: 0,
                limit: MAX_LOG_CHUNK_BYTES + 1,
            },
        };
        assert!(validate_request(&oversized_log).is_err());
    }

    #[test]
    fn github_bounds_and_tenant_authorization_are_enforced() {
        let mut request = request();
        request.actor = Some("github:user-1".into());
        request.command = AdminCommand::ListInstallationRepositories { installation_id: 0 };
        assert!(authorize_actor(AdminRole::TenantZero, &request).is_ok());
        assert!(validate_request(&request).unwrap_err().contains("positive"));

        request.command = AdminCommand::WebhookChunk {
            delivery_id: "delivery".into(),
            chunk_base64: "A".repeat(MAX_ADMIN_FRAME_BYTES),
        };
        assert!(validate_request(&request).is_err());

        request.command = AdminCommand::Deploy {
            request: crate::deploy::DeployRequest::new(
                "/host/source",
                "app",
                "app.example",
                "bun",
                "index.ts",
                "/host/artifacts",
                "/host/upstream",
            ),
        };
        assert!(authorize_actor(AdminRole::TenantZero, &request).is_err());
    }

    #[test]
    fn malformed_frame_gets_typed_error_when_writable() {
        let root = temp_path("malformed");
        let socket = root.join("admin.sock");
        let handler: Arc<dyn AdminHandler> = Arc::new(TestHandler {
            calls: Arc::new(AtomicUsize::new(0)),
            response: AdminResponse::error(
                "0123456789abcdef0123456789abcdef",
                AdminErrorCode::Internal,
                "unused",
            ),
        });
        let binding = AdminBinding::bind(&socket, AdminRole::Host).unwrap();
        let server = AdminServer::with_limits(vec![binding], handler, 1, 1).unwrap();
        let shutdown = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&shutdown);
        let join = thread::spawn(move || server.serve(flag).unwrap());
        let mut stream = UnixStream::connect(&socket).unwrap();
        stream.write_all(&[0, 0, 0, 3, b'{', b'}', b'!']).unwrap();
        let response: AdminResponse = read_frame(&mut stream).unwrap();
        assert!(
            matches!(response, AdminResponse::Error { error, .. } if error.code == AdminErrorCode::InvalidRequest)
        );
        shutdown.store(true, Ordering::Release);
        join.join().unwrap();
        assert!(!socket.exists());
        fs::remove_dir_all(root).unwrap();
    }
}
