use std::collections::VecDeque;
use std::fs;
use std::io::{self, ErrorKind};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use super::protocol::{
    read_frame, write_frame, AdminCommand, AdminErrorCode, AdminRequest, AdminResponse,
    ADMIN_PROTOCOL_VERSION, MAX_LOG_CHUNK_BYTES,
};

/// The listener through which an admin request arrived.
///
/// A request's `actor` field is descriptive input only. Authorization is based on
/// this role, which is assigned by the listener and cannot be supplied by a client.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AdminRole {
    Host,
    TenantZero,
}

/// The state-backed implementation of the admin protocol.
pub trait AdminHandler: Send + Sync + 'static {
    fn handle(&self, role: AdminRole, request: AdminRequest) -> AdminResponse;
}

pub const ADMIN_IO_TIMEOUT: Duration = Duration::from_secs(5);
pub const DEFAULT_ADMIN_WORKERS: usize = 4;
pub const DEFAULT_ADMIN_QUEUE_CAPACITY: usize = 64;
pub const MAX_ADMIN_ACTOR_BYTES: usize = 256;
pub const MAX_ADMIN_APP_BYTES: usize = 128;
pub const MAX_ADMIN_DEPLOYMENT_BYTES: usize = 128;
pub const MAX_ADMIN_LIST_LIMIT: u16 = 50;

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
    fs::create_dir_all(parent)?;

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

/// One role-bound admin listener. The role is never read from request JSON.
pub struct AdminBinding {
    listener: UnixListener,
    role: AdminRole,
    path: PathBuf,
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
        Ok(Self {
            listener,
            role,
            path: path.into(),
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
        let _ = fs::remove_file(&self.path);
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
        let mut state = self.state.lock().expect("admin queue mutex poisoned");
        if state.closed || state.pending.len() >= self.capacity {
            return false;
        }
        state.pending.push_back((stream, role));
        self.wake.notify_one();
        true
    }

    fn pop(&self) -> Option<(UnixStream, AdminRole)> {
        let mut state = self.state.lock().expect("admin queue mutex poisoned");
        loop {
            if let Some(item) = state.pending.pop_front() {
                return Some(item);
            }
            if state.closed {
                return None;
            }
            state = self.wake.wait(state).expect("admin queue mutex poisoned");
        }
    }

    fn close(&self) {
        let mut state = self.state.lock().expect("admin queue mutex poisoned");
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

    if let Err(message) = validate_request(&request) {
        let _ = write_frame(
            stream,
            &AdminResponse::error(
                request.request_id,
                AdminErrorCode::Validation,
                message,
            ),
        );
        return;
    }

    let response = handler.handle(role, request);
    let _ = write_frame(stream, &response);
}

fn validate_request(request: &AdminRequest) -> Result<(), String> {
    if !super::protocol::valid_request_id(&request.request_id) {
        return Err("request_id must be exactly 32 lowercase hexadecimal characters".into());
    }
    if let Some(actor) = request.actor.as_deref() {
        validate_text(actor, MAX_ADMIN_ACTOR_BYTES, "actor")?;
    }
    match &request.command {
        AdminCommand::Health | AdminCommand::Status => {}
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
        AdminCommand::ReadLog {
            deployment,
            limit,
            ..
        } => {
            validate_text(deployment, MAX_ADMIN_DEPLOYMENT_BYTES, "deployment")?;
            if !(1..=MAX_LOG_CHUNK_BYTES).contains(limit) {
                return Err(format!("log limit must be between 1 and {MAX_LOG_CHUNK_BYTES}"));
            }
        }
    }
    Ok(())
}

fn validate_list_limit(limit: u16) -> Result<(), String> {
    if !(1..=MAX_ADMIN_LIST_LIMIT).contains(&limit) {
        return Err(format!("list limit must be between 1 and {MAX_ADMIN_LIST_LIMIT}"));
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

/// Check the peer effective UID where the host operating system exposes it.
/// Unsupported Unix platforms are permitted rather than guessing an identity.
fn peer_uid_matches(stream: &UnixStream) -> io::Result<bool> {
    let fd = stream.as_raw_fd();
    let daemon_uid = unsafe { libc::geteuid() };

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
        if result == 0 {
            return Ok(credentials.uid == daemon_uid);
        }
        return Err(io::Error::last_os_error());
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
        if result == 0 {
            return Ok(uid == daemon_uid);
        }
        return Err(io::Error::last_os_error());
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
        let _ = (fd, daemon_uid);
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{self, File};
    use std::io::Write;
    use std::sync::atomic::AtomicUsize;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TestHandler {
        calls: Arc<AtomicUsize>,
        response: AdminResponse,
    }

    impl AdminHandler for TestHandler {
        fn handle(&self, _role: AdminRole, _request: AdminRequest) -> AdminResponse {
            self.calls.fetch_add(1, Ordering::Relaxed);
            self.response.clone()
        }
    }

    fn temp_path(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("ca-{label}-{}-{nonce}", std::process::id()))
    }

    fn request() -> AdminRequest {
        AdminRequest {
            version: ADMIN_PROTOCOL_VERSION,
            request_id: "0123456789abcdef0123456789abcdef".into(),
            actor: Some("operator".into()),
            command: AdminCommand::Health,
        }
    }

    #[test]
    fn stale_socket_removed_and_non_socket_preserved() {
        let root = temp_path("prepare");
        fs::create_dir_all(&root).unwrap();
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
        assert!(matches!(response, AdminResponse::Error { error, .. } if error.code == AdminErrorCode::UnsupportedVersion));
        assert_eq!(calls.load(Ordering::Relaxed), 0);
        shutdown.store(true, Ordering::Release);
        join.join().unwrap();
        assert!(!socket.exists());
        fs::remove_dir_all(root).unwrap();
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
        assert!(matches!(response, AdminResponse::Error { error, .. } if error.code == AdminErrorCode::InvalidRequest));
        shutdown.store(true, Ordering::Release);
        join.join().unwrap();
        assert!(!socket.exists());
        fs::remove_dir_all(root).unwrap();
    }
}
