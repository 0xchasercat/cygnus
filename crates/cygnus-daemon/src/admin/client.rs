use std::io::{self, ErrorKind};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::protocol::{read_frame, write_frame, AdminRequest, AdminResponse};
use super::server::ADMIN_IO_TIMEOUT;

const ZERO_REQUEST_ID: &str = "00000000000000000000000000000000";

/// A reusable client for the one-frame-per-connection admin protocol.
///
/// Each call opens a fresh stream, so one `AdminClient` can safely be used for
/// any number of requests without depending on server connection reuse.
#[derive(Clone, Debug)]
pub struct AdminClient {
    path: PathBuf,
    timeout: Duration,
}

impl AdminClient {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            timeout: ADMIN_IO_TIMEOUT,
        }
    }

    /// Deferred-connect constructor, useful when a caller wants to build its
    /// client before the daemon socket is available.
    pub fn connect(path: impl Into<PathBuf>) -> io::Result<Self> {
        Ok(Self::new(path))
    }

    pub fn with_timeout(mut self, timeout: Duration) -> io::Result<Self> {
        if timeout.is_zero() {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                "admin client timeout must be non-zero",
            ));
        }
        self.timeout = timeout;
        Ok(self)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn request(&self, request: &AdminRequest) -> io::Result<AdminResponse> {
        validate_request_id(&request.request_id)?;
        let mut stream = UnixStream::connect(&self.path)?;
        stream.set_read_timeout(Some(self.timeout))?;
        stream.set_write_timeout(Some(self.timeout))?;
        write_frame(&mut stream, request)?;
        let response: AdminResponse = read_frame(&mut stream)?;
        validate_response_id(&response, &request.request_id)?;
        Ok(response)
    }

    pub fn call(&self, request: &AdminRequest) -> io::Result<AdminResponse> {
        self.request(request)
    }
}

/// Perform one request using a fresh Unix stream.
pub fn request(path: impl AsRef<Path>, request: &AdminRequest) -> io::Result<AdminResponse> {
    AdminClient::new(path.as_ref().to_path_buf()).request(request)
}

fn validate_response_id(response: &AdminResponse, expected: &str) -> io::Result<()> {
    let response_id = match response {
        AdminResponse::Ok { request_id, .. } | AdminResponse::Error { request_id, .. } => request_id,
    };
    validate_request_id(response_id)?;
    if response_id != expected {
        return Err(io::Error::new(
            ErrorKind::InvalidData,
            "admin response request_id does not match request",
        ));
    }
    Ok(())
}

pub(crate) fn validate_request_id(request_id: &str) -> io::Result<()> {
    if request_id.len() != 32
        || !request_id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "admin request_id must be exactly 32 lowercase hexadecimal characters",
        ));
    }
    Ok(())
}

pub fn invalid_request_id() -> String {
    ZERO_REQUEST_ID.to_owned()
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::admin::{
        AdminBinding, AdminData, AdminErrorCode, AdminHandler, AdminRole, AdminServer,
        ADMIN_PROTOCOL_VERSION,
    };
    use std::fs;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::thread;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct Handler {
        calls: AtomicUsize,
    }

    impl AdminHandler for Handler {
        fn handle(&self, _role: AdminRole, request: AdminRequest) -> AdminResponse {
            self.calls.fetch_add(1, Ordering::Relaxed);
            AdminResponse::ok(
                request.request_id,
                AdminData::Health {
                    service: "test".into(),
                    isolation: "test".into(),
                },
            )
        }
    }

    fn temp_path() -> std::path::PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("cygnus-admin-client-{nonce}"));
        fs::create_dir_all(&root).unwrap();
        root.join("admin.sock")
    }

    #[test]
    fn real_uds_round_trip_and_request_id_match() {
        let socket = temp_path();
        let root = socket.parent().unwrap().to_path_buf();
        let handler = Arc::new(Handler {
            calls: AtomicUsize::new(0),
        });
        let binding = AdminBinding::bind(&socket, AdminRole::Host).unwrap();
        let server = AdminServer::with_limits(
            vec![binding],
            Arc::clone(&handler) as Arc<dyn AdminHandler>,
            1,
            4,
        )
        .unwrap();
        let shutdown = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&shutdown);
        let join = thread::spawn(move || server.serve(flag).unwrap());

        let request = AdminRequest {
            request_id: "0123456789abcdef0123456789abcdef".into(),
            version: ADMIN_PROTOCOL_VERSION,
            actor: Some("operator".into()),
            command: crate::admin::AdminCommand::Health,
        };
        let response = AdminClient::new(&socket).request(&request).unwrap();
        assert!(matches!(response, AdminResponse::Ok { request_id, .. } if request_id == request.request_id));
        assert_eq!(handler.calls.load(Ordering::Relaxed), 1);

        shutdown.store(true, Ordering::Release);
        join.join().unwrap();
        assert!(!socket.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn client_rejects_invalid_request_id_before_connecting() {
        let request = AdminRequest {
            request_id: "not-an-id".into(),
            version: ADMIN_PROTOCOL_VERSION,
            actor: None,
            command: crate::admin::AdminCommand::Health,
        };
        let error = AdminClient::new("/does/not/exist").request(&request).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
    }

    #[test]
    fn error_code_is_available_for_typed_transport_errors() {
        let _ = AdminErrorCode::InvalidRequest;
    }
}
