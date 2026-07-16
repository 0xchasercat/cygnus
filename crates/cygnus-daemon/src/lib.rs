//! The Cygnus front: the request path that ties the routing plane, the
//! supervisor, and the cages together (spec §6).
//!
//! Per connection: read the HTTP/1.1 request head, route it by host to an app,
//! ensure that app's cage is booted via the supervisor, then relay the
//! connection to the cage's Unix socket. Routing, TLS termination, and metering
//! live in this plane precisely because a cage cannot be trusted to self-report
//! — the front owns them.
//!
//! This slice relays with a portable thread-per-connection copy so the path is
//! correct and host-independent; swapping in the io_uring `splice` relay from
//! `cygnus-proxy` for the body phase is a later optimization behind the same
//! request path. The request-handling core (head read, routing, error
//! responses) is separated out and unit-tested; the socket plumbing is thin.
pub mod acme;
pub mod admin;
pub mod deploy;
pub mod edge;
pub mod state;
pub mod tls;

use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use crate::acme::Http01Challenges;
use crate::tls::{TlsServer, relay_tls};
use cygnus_cage::Cage;
use cygnus_router::{HeadParse, RequestHead, Route, Router, normalize_host, parse_request_head};
use cygnus_supervisor::{AcquireError, Supervisor};

/// How long a client has to send a complete request head before the connection
/// is dropped, so a slow or stuck client cannot pin a worker thread.
const HEAD_READ_TIMEOUT: Duration = Duration::from_secs(15);

/// A minimal HTTP status the front returns on its own (never proxied).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
    /// 400 — the request head was unusable (no host, malformed, or too slow).
    BadRequest,
    /// 404 — no route matched the host.
    NotFound,
    /// 421 — TLS SNI and HTTP Host disagree.
    MisdirectedRequest,
    /// 502 — the app booted but its socket could not be reached.
    BadGateway,
    /// 503 — the app is crash-looping or backing off; try again later.
    Unavailable,
}

/// The canned response bytes for a front-generated status. Each closes the
/// connection, since the front does not keep-alive its own error replies.
pub fn error_response(status: Status) -> &'static [u8] {
    match status {
        Status::BadRequest => {
            b"HTTP/1.1 400 Bad Request\r\nconnection: close\r\ncontent-length: 0\r\n\r\n"
        }
        Status::NotFound => {
            b"HTTP/1.1 404 Not Found\r\nconnection: close\r\ncontent-length: 0\r\n\r\n"
        }
        Status::MisdirectedRequest => {
            b"HTTP/1.1 421 Misdirected Request\r\nconnection: close\r\ncontent-length: 0\r\n\r\n"
        }
        Status::BadGateway => {
            b"HTTP/1.1 502 Bad Gateway\r\nconnection: close\r\ncontent-length: 0\r\n\r\n"
        }
        Status::Unavailable => {
            b"HTTP/1.1 503 Service Unavailable\r\nconnection: close\r\ncontent-length: 0\r\n\r\n"
        }
    }
}

/// Read from `client` until a full request head is parsed. Returns the head and
/// every byte read from the client so far — the head plus any pipelined bytes
/// past it — so the caller can forward them to the upstream unchanged.
pub fn read_head<R: Read>(client: &mut R) -> Result<(RequestHead, Vec<u8>), Status> {
    let mut buffer = Vec::with_capacity(1024);
    let mut chunk = [0_u8; 4096];
    loop {
        let read = client.read(&mut chunk).map_err(|_| Status::BadRequest)?;
        if read == 0 {
            return Err(Status::BadRequest);
        }
        buffer.extend_from_slice(&chunk[..read]);
        match parse_request_head(&buffer) {
            HeadParse::Complete(head) => return Ok((head, buffer)),
            HeadParse::Malformed => return Err(Status::BadRequest),
            HeadParse::Incomplete => {}
        }
    }
}

/// Route a parsed head to its app, or a status to return instead.
pub fn route_request(head: &RequestHead, router: &Router) -> Result<Arc<Route>, Status> {
    let host = head.host.as_deref().ok_or(Status::BadRequest)?;
    router.resolve(host).ok_or(Status::NotFound)
}

/// Route HTTPS by SNI and reject domain-fronted Host headers.
pub fn route_tls_request(
    head: &RequestHead,
    server_name: &str,
    router: &Router,
) -> Result<Arc<Route>, Status> {
    let host = head.host.as_deref().ok_or(Status::BadRequest)?;
    if normalize_host(host) != normalize_host(server_name) {
        return Err(Status::MisdirectedRequest);
    }
    router.resolve(server_name).ok_or(Status::NotFound)
}

/// Map a supervisor acquire failure to the status the client should see.
fn acquire_status(error: &AcquireError) -> Status {
    match error {
        AcquireError::Unknown => Status::NotFound,
        AcquireError::CrashLooping
        | AcquireError::BackingOff { .. }
        | AcquireError::ShuttingDown => Status::Unavailable,
        AcquireError::BootFailed(_) => Status::BadGateway,
    }
}

/// The front: shared routing table plus the cage supervisor.
pub struct Frontend {
    router: Arc<Router>,
    supervisor: Arc<Supervisor<Cage>>,
    http01: Http01Challenges,
}

impl Frontend {
    /// Compose a front from a routing table and a supervisor.
    pub fn new(router: Arc<Router>, supervisor: Arc<Supervisor<Cage>>) -> Self {
        Self {
            router,
            supervisor,
            http01: Http01Challenges::default(),
        }
    }

    /// Return the shared HTTP-01 challenge registry used by the ACME manager.
    pub fn http01_challenges(&self) -> Http01Challenges {
        self.http01.clone()
    }

    /// Accept connections forever, handling each on its own thread. Returns
    /// only if accepting itself fails.
    pub fn serve(self: Arc<Self>, listener: TcpListener) -> io::Result<()> {
        for incoming in listener.incoming() {
            let client = incoming?;
            let front = Arc::clone(&self);
            thread::spawn(move || front.serve_connection(client));
        }
        Ok(())
    }
    /// Accept connections until `shutdown` is set. The listener is polled in
    /// nonblocking mode so signal-driven daemon shutdown cannot strand it in
    /// `accept` while cages remain alive.
    pub fn serve_until(
        self: Arc<Self>,
        listener: TcpListener,
        shutdown: &AtomicBool,
    ) -> io::Result<()> {
        listener.set_nonblocking(true)?;
        while !shutdown.load(Ordering::Acquire) {
            match listener.accept() {
                Ok((client, _)) => {
                    let front = Arc::clone(&self);
                    thread::spawn(move || front.serve_connection(client));
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }
    /// Accept HTTPS connections until shutdown, selecting certificates by SNI.
    pub fn serve_tls_until(
        self: Arc<Self>,
        listener: TcpListener,
        tls: TlsServer,
        shutdown: &AtomicBool,
    ) -> io::Result<()> {
        listener.set_nonblocking(true)?;
        while !shutdown.load(Ordering::Acquire) {
            match listener.accept() {
                Ok((client, _)) => {
                    let front = Arc::clone(&self);
                    let tls = tls.clone();
                    thread::spawn(move || front.serve_tls_connection(client, &tls));
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    pub fn serve_tls_connection(&self, client: TcpStream, tls: &TlsServer) {
        let _ = client.set_read_timeout(Some(HEAD_READ_TIMEOUT));
        let connection = match tls.connection() {
            Ok(connection) => connection,
            Err(_) => return,
        };
        let mut client = rustls::StreamOwned::new(connection, client);
        let (head, buffered) = match read_head(&mut client) {
            Ok(routed) => routed,
            Err(status) => {
                let _ = client.write_all(error_response(status));
                let _ = client.flush();
                return;
            }
        };
        let server_name = match client.conn.server_name() {
            Some(server_name) => server_name.to_owned(),
            None => {
                let _ = client.write_all(error_response(Status::BadRequest));
                let _ = client.flush();
                return;
            }
        };
        let route = match route_tls_request(&head, &server_name, &self.router) {
            Ok(route) => route,
            Err(status) => {
                let _ = client.write_all(error_response(status));
                let _ = client.flush();
                return;
            }
        };
        if let Err(error) = self.supervisor.acquire(&route.app) {
            eprintln!(
                "cygnus-daemon: app {:?} acquire failed: {error:?}",
                route.app
            );
            let _ = client.write_all(error_response(acquire_status(&error)));
            let _ = client.flush();
            return;
        }
        let upstream = match UnixStream::connect(&route.upstream) {
            Ok(upstream) => upstream,
            Err(_) => {
                let _ = client.write_all(error_response(Status::BadGateway));
                let _ = client.flush();
                return;
            }
        };
        if (&upstream).write_all(&buffered).is_err() {
            let _ = client.write_all(error_response(Status::BadGateway));
            let _ = client.flush();
            return;
        }
        let (connection, client) = client.into_parts();
        let _ = client.set_read_timeout(None);
        let _ = relay_tls(connection, client, upstream);
    }

    /// Serve one accepted client connection end to end on the current thread.
    /// This is the same path [`Self::serve`] dispatches to its workers.
    pub fn serve_connection(&self, mut client: TcpStream) {
        // Bound the head read so a slow client cannot hold the worker forever.
        let _ = client.set_read_timeout(Some(HEAD_READ_TIMEOUT));
        let (head, buffered) = match read_head(&mut client) {
            Ok(parsed) => parsed,
            Err(status) => {
                let _ = client.write_all(error_response(status));
                return;
            }
        };
        if let Some(response) = self.http01.response(&head) {
            let _ = client.write_all(&response);
            return;
        }
        let route = match route_request(&head, &self.router) {
            Ok(route) => route,
            Err(status) => {
                let _ = client.write_all(error_response(status));
                return;
            }
        };

        if let Err(error) = self.supervisor.acquire(&route.app) {
            eprintln!(
                "cygnus-daemon: app {:?} acquire failed: {error:?}",
                route.app
            );
            let _ = client.write_all(error_response(acquire_status(&error)));
            return;
        }

        let upstream = match UnixStream::connect(&route.upstream) {
            Ok(upstream) => upstream,
            Err(_) => {
                let _ = client.write_all(error_response(Status::BadGateway));
                return;
            }
        };
        if (&upstream).write_all(&buffered).is_err() {
            let _ = client.write_all(error_response(Status::BadGateway));
            return;
        }

        // The connection is now long-lived; drop the head-read deadline so a
        // quiet keep-alive or streaming response is not torn down.
        let _ = client.set_read_timeout(None);
        let _ = relay(client, upstream);
    }
}

/// Relay bytes both ways between the client and the upstream until each side
/// closes, propagating half-close so a one-way finish (a drained response, a
/// client that stopped sending) is passed through rather than hung on.
fn relay(client: TcpStream, upstream: UnixStream) -> io::Result<()> {
    let mut client_reader = client.try_clone()?;
    let mut upstream_writer = upstream.try_clone()?;
    let mut upstream_reader = upstream;
    let mut client_writer = client;

    let client_to_upstream = thread::spawn(move || {
        let _ = io::copy(&mut client_reader, &mut upstream_writer);
        let _ = upstream_writer.shutdown(Shutdown::Write);
    });

    let _ = io::copy(&mut upstream_reader, &mut client_writer);
    let _ = client_writer.shutdown(Shutdown::Write);
    let _ = client_to_upstream.join();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use cygnus_router::RouteTable;
    use std::io::Cursor;
    use std::path::PathBuf;

    /// A reader that hands out its data in fixed-size chunks, to exercise the
    /// incomplete-then-complete path of `read_head`.
    struct ChunkedReader {
        data: Vec<u8>,
        position: usize,
        chunk: usize,
    }

    impl Read for ChunkedReader {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            let remaining = &self.data[self.position..];
            let take = remaining.len().min(self.chunk).min(out.len());
            out[..take].copy_from_slice(&remaining[..take]);
            self.position += take;
            Ok(take)
        }
    }

    fn router_with(host: &str, app: &str) -> Router {
        let mut table = RouteTable::new();
        table.insert(
            host,
            Route {
                app: app.to_owned(),
                upstream: PathBuf::from("/run/cygnus/app.sock"),
            },
        );
        Router::new(table)
    }

    #[test]
    fn read_head_returns_head_and_all_bytes() {
        let raw = b"GET /x HTTP/1.1\r\nHost: api.example.com\r\n\r\nBODY".to_vec();
        let mut cursor = Cursor::new(raw.clone());
        let (head, buffered) = read_head(&mut cursor).expect("a head");
        assert_eq!(head.host.as_deref(), Some("api.example.com"));
        // Everything read, including the pipelined body bytes, is returned.
        assert_eq!(buffered, raw);
    }

    #[test]
    fn read_head_reassembles_across_chunks() {
        let raw = b"GET / HTTP/1.1\r\nHost: api.example.com\r\n\r\n".to_vec();
        let mut reader = ChunkedReader {
            data: raw.clone(),
            position: 0,
            chunk: 7,
        };
        let (head, buffered) = read_head(&mut reader).expect("a head");
        assert_eq!(head.host.as_deref(), Some("api.example.com"));
        assert_eq!(buffered, raw);
    }

    #[test]
    fn read_head_rejects_a_closed_connection() {
        let mut cursor = Cursor::new(b"GET / HTTP/1.1\r\nHost: x\r\n".to_vec());
        assert_eq!(read_head(&mut cursor), Err(Status::BadRequest));
    }

    #[test]
    fn read_head_rejects_a_malformed_head() {
        let mut cursor = Cursor::new(b"not http\r\n\r\n".to_vec());
        assert_eq!(read_head(&mut cursor), Err(Status::BadRequest));
    }

    #[test]
    fn routing_maps_host_to_app_or_a_status() {
        let router = router_with("api.example.com", "api");

        let head = RequestHead {
            method: "GET".into(),
            target: "/".into(),
            host: Some("api.example.com".into()),
            head_len: 0,
        };
        assert_eq!(route_request(&head, &router).unwrap().app, "api");

        let miss = RequestHead {
            host: Some("nope.example.com".into()),
            ..head.clone()
        };
        assert_eq!(route_request(&miss, &router), Err(Status::NotFound));

        let no_host = RequestHead { host: None, ..head };
        assert_eq!(route_request(&no_host, &router), Err(Status::BadRequest));
    }

    #[test]
    fn tls_routing_rejects_domain_fronting() {
        let router = router_with("api.example.com", "api");
        let head = RequestHead {
            method: "GET".into(),
            target: "/".into(),
            host: Some("API.EXAMPLE.COM:443".into()),
            head_len: 0,
        };
        assert_eq!(
            route_tls_request(&head, "api.example.com", &router)
                .unwrap()
                .app,
            "api"
        );
        assert_eq!(
            route_tls_request(&head, "other.example.com", &router),
            Err(Status::MisdirectedRequest)
        );
    }

    #[test]
    fn acquire_failures_map_to_client_statuses() {
        assert_eq!(acquire_status(&AcquireError::Unknown), Status::NotFound);
        assert_eq!(
            acquire_status(&AcquireError::ShuttingDown),
            Status::Unavailable
        );
        assert_eq!(
            acquire_status(&AcquireError::CrashLooping),
            Status::Unavailable
        );
        assert_eq!(
            acquire_status(&AcquireError::BackingOff {
                retry_after: Duration::from_secs(1)
            }),
            Status::Unavailable
        );
        assert_eq!(
            acquire_status(&AcquireError::BootFailed("boom".into())),
            Status::BadGateway
        );
    }

    #[test]
    fn http01_challenge_bypasses_application_routing() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let router = Arc::new(Router::new(RouteTable::new()));
        let supervisor = Arc::new(Supervisor::new(|_| Err("must not boot".into())));
        let frontend = Arc::new(Frontend::new(router, supervisor));
        frontend
            .http01_challenges()
            .insert("acme.example.com", "token", "token.thumbprint")
            .unwrap();
        let worker = thread::spawn(move || {
            let (client, _) = listener.accept().unwrap();
            frontend.serve_connection(client);
        });

        let mut client = TcpStream::connect(address).unwrap();
        client
            .write_all(
                b"GET /.well-known/acme-challenge/token HTTP/1.1\r\nHost: acme.example.com\r\n\r\n",
            )
            .unwrap();
        client.shutdown(Shutdown::Write).unwrap();
        let mut response = Vec::new();
        client.read_to_end(&mut response).unwrap();
        worker.join().unwrap();
        assert!(response.starts_with(b"HTTP/1.1 200 OK"));
        assert!(response.ends_with(b"token.thumbprint"));
    }

    #[test]
    fn interruptible_serve_returns_without_accepting_after_shutdown() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let router = Arc::new(Router::new(RouteTable::new()));
        let supervisor = Arc::new(Supervisor::new(|_| Err("must not boot".into())));
        let frontend = Arc::new(Frontend::new(router, supervisor));
        let shutdown = AtomicBool::new(true);

        frontend.serve_until(listener, &shutdown).unwrap();
    }

    #[test]
    fn error_responses_carry_the_right_status_line() {
        assert!(error_response(Status::NotFound).starts_with(b"HTTP/1.1 404"));
        assert!(error_response(Status::Unavailable).starts_with(b"HTTP/1.1 503"));
        assert!(error_response(Status::BadGateway).starts_with(b"HTTP/1.1 502"));
        assert!(error_response(Status::BadRequest).starts_with(b"HTTP/1.1 400"));
        assert!(error_response(Status::MisdirectedRequest).starts_with(b"HTTP/1.1 421"));
    }
}
