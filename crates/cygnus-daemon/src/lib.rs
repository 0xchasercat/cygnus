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
pub mod domains;
pub mod edge;
pub mod github;
pub mod ingress;
pub mod metrics;
mod relay_framing;
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
use crate::ingress::{BodyGuard, BodyGuardError, IngressController, IngressLimits, RequestSpan};
use crate::metrics::MetricsHub;
use crate::relay_framing::ResponseFraming;
use crate::tls::{TlsServer, relay_tls};
use cygnus_cage::Cage;
use cygnus_router::{
    BodyFraming, HeadParse, RequestHead, Route, Router, normalize_host, parse_request_head,
};
use cygnus_supervisor::{AcquireError, Supervisor};

/// How long a client has to send a complete request head before the connection
/// is dropped, so a slow or stuck client cannot pin a worker thread.
const HEAD_READ_TIMEOUT: Duration = Duration::from_secs(15);
const RELAY_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// A minimal HTTP status the front returns on its own (never proxied).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
    /// 400 — the request head was unusable (no host, malformed, or too slow).
    BadRequest,
    /// 408 — the client did not complete the request head in time.
    RequestTimeout,
    /// 413 — the declared request body exceeds the edge policy.
    PayloadTooLarge,
    /// 404 — no route matched the host.
    NotFound,
    /// 421 — TLS SNI and HTTP Host disagree.
    MisdirectedRequest,
    /// 429 — ingress concurrency or rate admission failed.
    TooManyRequests,
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
        Status::RequestTimeout => {
            b"HTTP/1.1 408 Request Timeout\r\nconnection: close\r\ncontent-length: 0\r\n\r\n"
        }
        Status::PayloadTooLarge => {
            b"HTTP/1.1 413 Payload Too Large\r\nconnection: close\r\ncontent-length: 0\r\n\r\n"
        }
        Status::NotFound => {
            b"HTTP/1.1 404 Not Found\r\nconnection: close\r\ncontent-length: 0\r\n\r\n"
        }
        Status::MisdirectedRequest => {
            b"HTTP/1.1 421 Misdirected Request\r\nconnection: close\r\ncontent-length: 0\r\n\r\n"
        }
        Status::TooManyRequests => {
            b"HTTP/1.1 429 Too Many Requests\r\nconnection: close\r\nretry-after: 1\r\ncontent-length: 0\r\n\r\n"
        }
        Status::BadGateway => {
            b"HTTP/1.1 502 Bad Gateway\r\nconnection: close\r\ncontent-length: 0\r\n\r\n"
        }
        Status::Unavailable => {
            b"HTTP/1.1 503 Service Unavailable\r\nconnection: close\r\ncontent-length: 0\r\n\r\n"
        }
    }
}

impl Status {
    fn code(self) -> u16 {
        match self {
            Self::BadRequest => 400,
            Self::RequestTimeout => 408,
            Self::PayloadTooLarge => 413,
            Self::NotFound => 404,
            Self::MisdirectedRequest => 421,
            Self::TooManyRequests => 429,
            Self::BadGateway => 502,
            Self::Unavailable => 503,
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
        let read = client.read(&mut chunk).map_err(|error| {
            if matches!(
                error.kind(),
                io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
            ) {
                Status::RequestTimeout
            } else {
                Status::BadRequest
            }
        })?;
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

const MAX_GITHUB_WEBHOOK_BODY_BYTES: u64 = 25 * 1024 * 1024;
const MAX_DEPLOY_CHUNK_BODY_BYTES: u64 = 2 * 1024 * 1024;

fn request_body_limits(head: &RequestHead, defaults: &IngressLimits) -> IngressLimits {
    let mut limits = defaults.clone();
    limits.max_body_bytes = match (head.method.as_str(), head.target.as_str()) {
        ("POST", "/github/webhook") => MAX_GITHUB_WEBHOOK_BODY_BYTES,
        ("POST", "/api/v1/deploy/chunk") => MAX_DEPLOY_CHUNK_BODY_BYTES,
        _ => defaults.max_body_bytes,
    };
    limits
}

fn body_guard(
    head: &RequestHead,
    buffered: &[u8],
    limits: &IngressLimits,
) -> Result<BodyGuard, Status> {
    let mut guard = match head.body {
        BodyFraming::None => BodyGuard::none(),
        BodyFraming::ContentLength(length) if length <= limits.max_body_bytes => {
            BodyGuard::fixed(length)
        }
        BodyFraming::ContentLength(_) => return Err(Status::PayloadTooLarge),
        BodyFraming::Chunked => BodyGuard::chunked(limits.max_body_bytes),
    };
    guard
        .observe(&buffered[head.head_len..])
        .map_err(|error| match error {
            BodyGuardError::Malformed => Status::BadRequest,
            BodyGuardError::TooLarge => Status::PayloadTooLarge,
        })?;
    Ok(guard)
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

fn reject<W: Write>(writer: &mut W, span: &mut RequestSpan, status: Status, outcome: &'static str) {
    let response = error_response(status);
    span.responded(status.code(), outcome, response.len());
    let _ = writer.write_all(response);
    let _ = writer.flush();
}

/// The front: shared routing table plus the cage supervisor.
pub struct Frontend {
    router: Arc<Router>,
    supervisor: Arc<Supervisor<Cage>>,
    http01: Http01Challenges,
    ingress: IngressController,
    metrics: MetricsHub,
}

impl Frontend {
    /// Compose a front from a routing table and a supervisor.
    pub fn new(router: Arc<Router>, supervisor: Arc<Supervisor<Cage>>) -> Self {
        Self::with_limits(router, supervisor, IngressLimits::default())
            .expect("default ingress limits are valid")
    }

    pub fn with_limits(
        router: Arc<Router>,
        supervisor: Arc<Supervisor<Cage>>,
        limits: IngressLimits,
    ) -> Result<Self, &'static str> {
        Ok(Self {
            router,
            supervisor,
            http01: Http01Challenges::default(),
            ingress: IngressController::new(limits)?,
            metrics: MetricsHub::new(),
        })
    }

    /// Use a caller-provided metrics hub for request telemetry.
    #[must_use]
    pub fn with_metrics(mut self, metrics: MetricsHub) -> Self {
        self.metrics = metrics;
        self
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
                    // Accept on a nonblocking listener inherits O_NONBLOCK on
                    // macOS/BSD. The request/response relay is a blocking
                    // thread-per-connection copy; leave the client nonblocking
                    // and a body-complete GET immediately WouldBlock on the
                    // client read, SHUT_WR the upstream, and truncate large
                    // Bun responses to one UDS buffer (~8-16 KiB).
                    if let Err(error) = client.set_nonblocking(false) {
                        eprintln!("cygnus-daemon: set client blocking: {error}");
                        continue;
                    }
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
                    if let Err(error) = client.set_nonblocking(false) {
                        eprintln!("cygnus-daemon: set tls client blocking: {error}");
                        continue;
                    }
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
        let Ok(peer) = client.peer_addr().map(|address| address.ip()) else {
            return;
        };
        let mut span = RequestSpan::new(self.metrics.clone(), "https", peer);
        let _connection_permit = match self.ingress.enter_connection(peer) {
            Ok(permit) => permit,
            Err(_) => {
                let mut client = client;
                reject(
                    &mut client,
                    &mut span,
                    Status::TooManyRequests,
                    "peer_concurrency",
                );
                return;
            }
        };
        let _ = client.set_read_timeout(Some(HEAD_READ_TIMEOUT));
        let connection = match tls.connection() {
            Ok(connection) => connection,
            Err(_) => {
                span.relay_error();
                return;
            }
        };
        let mut client = rustls::StreamOwned::new(connection, client);
        let (head, buffered) = match read_head(&mut client) {
            Ok(parsed) => parsed,
            Err(status) => {
                reject(&mut client, &mut span, status, "invalid_head");
                return;
            }
        };
        span.set_head(
            &head.method,
            head.host.as_deref(),
            &head.target,
            buffered.len(),
        );
        let limits = request_body_limits(&head, self.ingress.limits());
        let body_guard = match body_guard(&head, &buffered, &limits) {
            Ok(guard) => guard,
            Err(status) => {
                reject(&mut client, &mut span, status, "body_rejected");
                return;
            }
        };
        let server_name = match client.conn.server_name() {
            Some(server_name) => server_name.to_owned(),
            None => {
                reject(&mut client, &mut span, Status::BadRequest, "missing_sni");
                return;
            }
        };
        let route = match route_tls_request(&head, &server_name, &self.router) {
            Ok(route) => route,
            Err(status) => {
                reject(&mut client, &mut span, status, "route_rejected");
                return;
            }
        };
        span.set_app(&route.app);
        let _request_permit = match self.ingress.enter_request(peer, &route.app) {
            Ok(permit) => permit,
            Err(_) => {
                reject(
                    &mut client,
                    &mut span,
                    Status::TooManyRequests,
                    "request_admission",
                );
                return;
            }
        };
        match self.supervisor.acquire_with_outcome(&route.app) {
            Ok(outcome) => span.set_cold(outcome.cold),
            Err(error) => {
                eprintln!(
                    "cygnus-daemon: app {app:?} is unavailable: {error:?}",
                    app = route.app
                );
                reject(
                    &mut client,
                    &mut span,
                    acquire_status(&error),
                    "app_unavailable",
                );
                return;
            }
        }
        let upstream = match UnixStream::connect(&route.upstream) {
            Ok(upstream) => upstream,
            Err(_) => {
                reject(
                    &mut client,
                    &mut span,
                    Status::BadGateway,
                    "upstream_connect",
                );
                return;
            }
        };
        let buffered = upstream_request_bytes(&head, &buffered);
        if (&upstream).write_all(&buffered).is_err() {
            reject(&mut client, &mut span, Status::BadGateway, "upstream_write");
            return;
        }
        let (connection, client) = client.into_parts();
        let _ = client.set_read_timeout(None);
        let request_is_head = head.method.eq_ignore_ascii_case("HEAD");
        match relay_tls(connection, client, upstream, body_guard, request_is_head) {
            Ok(stats) => span.proxied(stats.status, stats.to_upstream, stats.to_client),
            Err(_) => span.relay_error(),
        }
    }

    /// Serve one accepted client connection end to end on the current thread.
    /// This is the same path [`Self::serve`] dispatches to its workers.
    pub fn serve_connection(&self, mut client: TcpStream) {
        let Ok(peer) = client.peer_addr().map(|address| address.ip()) else {
            return;
        };
        let mut span = RequestSpan::new(self.metrics.clone(), "http", peer);
        let _connection_permit = match self.ingress.enter_connection(peer) {
            Ok(permit) => permit,
            Err(_) => {
                reject(
                    &mut client,
                    &mut span,
                    Status::TooManyRequests,
                    "peer_concurrency",
                );
                return;
            }
        };
        let _ = client.set_read_timeout(Some(HEAD_READ_TIMEOUT));
        let (head, buffered) = match read_head(&mut client) {
            Ok(parsed) => parsed,
            Err(status) => {
                reject(&mut client, &mut span, status, "invalid_head");
                return;
            }
        };
        span.set_head(
            &head.method,
            head.host.as_deref(),
            &head.target,
            buffered.len(),
        );
        let limits = request_body_limits(&head, self.ingress.limits());
        let body_guard = match body_guard(&head, &buffered, &limits) {
            Ok(guard) => guard,
            Err(status) => {
                reject(&mut client, &mut span, status, "body_rejected");
                return;
            }
        };
        if let Some(response) = self.http01.response(&head) {
            if client.write_all(&response).is_ok() {
                span.responded(200, "acme_http01", response.len());
            } else {
                span.relay_error();
            }
            return;
        }
        let route = match route_request(&head, &self.router) {
            Ok(route) => route,
            Err(status) => {
                reject(&mut client, &mut span, status, "route_rejected");
                return;
            }
        };
        span.set_app(&route.app);
        let _request_permit = match self.ingress.enter_request(peer, &route.app) {
            Ok(permit) => permit,
            Err(_) => {
                reject(
                    &mut client,
                    &mut span,
                    Status::TooManyRequests,
                    "request_admission",
                );
                return;
            }
        };
        match self.supervisor.acquire_with_outcome(&route.app) {
            Ok(outcome) => span.set_cold(outcome.cold),
            Err(error) => {
                eprintln!(
                    "cygnus-daemon: app {app:?} is unavailable: {error:?}",
                    app = route.app
                );
                reject(
                    &mut client,
                    &mut span,
                    acquire_status(&error),
                    "app_unavailable",
                );
                return;
            }
        }
        let upstream = match UnixStream::connect(&route.upstream) {
            Ok(upstream) => upstream,
            Err(_) => {
                reject(
                    &mut client,
                    &mut span,
                    Status::BadGateway,
                    "upstream_connect",
                );
                return;
            }
        };
        let buffered = upstream_request_bytes(&head, &buffered);
        if (&upstream).write_all(&buffered).is_err() {
            reject(&mut client, &mut span, Status::BadGateway, "upstream_write");
            return;
        }
        let _ = client.set_read_timeout(Some(RELAY_IDLE_TIMEOUT));
        let _ = client.set_write_timeout(Some(RELAY_IDLE_TIMEOUT));
        let _ = upstream.set_read_timeout(Some(RELAY_IDLE_TIMEOUT));
        let _ = upstream.set_write_timeout(Some(RELAY_IDLE_TIMEOUT));
        let request_is_head = head.method.eq_ignore_ascii_case("HEAD");
        match relay(client, upstream, body_guard, request_is_head) {
            Ok(stats) => span.proxied(stats.status, stats.to_upstream, stats.to_client),
            Err(_) => span.relay_error(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct RelayStats {
    to_upstream: u64,
    to_client: u64,
    status: u16,
}

/// Relay bytes both ways between the client and the upstream until each side
/// closes, propagating half-close so a one-way finish (a drained response, a
/// client that stopped sending) is passed through rather than hung on.
/// After the request body: forward client bytes when the exchange upgraded
/// to a tunnel (websockets), discard them otherwise. Returns when the client
/// closes, times out, or errors, counting only forwarded bytes.
fn forward_or_discard<R: Read, W: Write>(
    reader: &mut R,
    writer: &mut W,
    tunnel: &AtomicBool,
) -> io::Result<u64> {
    let mut forwarded = 0_u64;
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => return Ok(forwarded),
            Ok(read) => {
                if tunnel.load(Ordering::Acquire) {
                    writer.write_all(&buffer[..read])?;
                    forwarded += read as u64;
                }
            }
            // Keep waiting: a body-complete GET is silent while the response
            // streams. TimedOut is the relay idle timeout firing, not a client
            // hangup. WouldBlock should not happen on a blocking socket; if it
            // does (accept inherited O_NONBLOCK), park briefly instead of
            // returning — returning SHUT_WRs the upstream and truncates large
            // macOS/Bun responses to one UDS buffer.
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) if error.kind() == io::ErrorKind::TimedOut => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(1));
            }
            Err(_) => return Ok(forwarded),
        }
    }
}

/// The bytes to hand the upstream: the parsed head with hop-by-hop
/// connection headers stripped, followed by any payload bytes already
/// buffered. The exchange is ended by the relay itself once the response is
/// fully framed (see [`relay_framing`]); the app is never asked to close,
/// because event loops differ in how faithfully they flush large bodies
/// while closing.
fn upstream_request_bytes(head: &RequestHead, buffered: &[u8]) -> Vec<u8> {
    let head_bytes = &buffered[..head.head_len];
    let mut rewritten = Vec::with_capacity(buffered.len() + 32);
    let mut lines = head_bytes.split_inclusive(|byte| *byte == b'\n');
    if let Some(request_line) = lines.next() {
        rewritten.extend_from_slice(request_line);
    }
    for line in lines {
        if line == b"\r\n" || line == b"\n" {
            break;
        }
        let name = line.split(|byte| *byte == b':').next().unwrap_or_default();
        let name = name.trim_ascii();
        if name.eq_ignore_ascii_case(b"connection")
            || name.eq_ignore_ascii_case(b"proxy-connection")
            || name.eq_ignore_ascii_case(b"keep-alive")
        {
            continue;
        }
        rewritten.extend_from_slice(line);
    }
    rewritten.extend_from_slice(b"\r\n");
    rewritten.extend_from_slice(&buffered[head.head_len..]);
    rewritten
}

fn relay(
    client: TcpStream,
    upstream: UnixStream,
    body_guard: BodyGuard,
    request_is_head: bool,
) -> io::Result<RelayStats> {
    let mut client_reader = client.try_clone()?;
    let mut upstream_writer = upstream.try_clone()?;
    let mut upstream_reader = upstream.try_clone()?;
    let mut client_writer = client.try_clone()?;
    let tunnel = Arc::new(AtomicBool::new(false));

    let request_tunnel = Arc::clone(&tunnel);
    let client_to_upstream = thread::spawn(move || {
        let copied = copy_to_upstream(
            &mut client_reader,
            &mut upstream_writer,
            body_guard,
            &request_tunnel,
        );
        let _ = upstream_writer.shutdown(Shutdown::Write);
        copied
    });
    let to_client = copy_response_to_client(
        &mut upstream_reader,
        &mut client_writer,
        request_is_head,
        &tunnel,
    );
    let _ = client_writer.shutdown(Shutdown::Write);
    // End the exchange decisively: closing the read sides unblocks the
    // request-direction thread immediately instead of holding both sockets
    // until a peer close or idle timeout.
    let _ = client.shutdown(Shutdown::Read);
    let _ = upstream.shutdown(Shutdown::Both);
    let to_upstream = client_to_upstream
        .join()
        .map_err(|_| io::Error::other("client relay thread panicked"))?;
    let (to_client, status) = to_client?;
    Ok(RelayStats {
        to_upstream: to_upstream?,
        to_client,
        status,
    })
}

fn copy_response_to_client<R: Read, W: Write>(
    reader: &mut R,
    writer: &mut W,
    request_is_head: bool,
    tunnel: &AtomicBool,
) -> io::Result<(u64, u16)> {
    let mut copied = 0_u64;
    let mut framing = ResponseFraming::new(request_is_head);
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        let read = match reader.read(&mut buffer) {
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        if read == 0 {
            return Ok((copied, framing.status().unwrap_or_default()));
        }
        framing.observe(&buffer[..read]);
        if framing.tunnel() {
            tunnel.store(true, Ordering::Release);
        }
        writer.write_all(&buffer[..read])?;
        copied += read as u64;
        if framing.complete() {
            // One full response delivered; the exchange is over.
            return Ok((copied, framing.status().unwrap_or_default()));
        }
    }
}

fn copy_to_upstream<R: Read, W: Write>(
    reader: &mut R,
    writer: &mut W,
    mut body_guard: BodyGuard,
    tunnel: &AtomicBool,
) -> io::Result<u64> {
    let mut copied = 0_u64;
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        if body_guard.is_complete() {
            // The request is fully forwarded, but the upstream write side
            // must stay open: apps treat an early half-close as a client
            // abort and cancel in-flight handlers before responding. On an
            // upgraded exchange the client keeps talking (websocket frames);
            // otherwise anything else it sends has nowhere to go.
            return forward_or_discard(reader, writer, tunnel).map(|extra| copied + extra);
        }
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            return Ok(copied);
        }
        body_guard.observe(&buffer[..read]).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("request body rejected: {error:?}"),
            )
        })?;
        writer.write_all(&buffer[..read])?;
        copied += read as u64;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cygnus_router::{BodyFraming, RouteTable};
    use std::io::Cursor;
    use std::path::PathBuf;

    #[test]
    fn upstream_head_rewrite_strips_hop_by_hop_headers() {
        let raw = b"GET /api HTTP/1.1\r\nHost: a.example\r\nConnection: keep-alive\r\nKeep-Alive: timeout=5\r\nX-Custom: yes\r\n\r\nBODY";
        let (head, buffered) = match parse_request_head(raw) {
            HeadParse::Complete(head) => (head, raw.to_vec()),
            other => panic!("head should parse: {other:?}"),
        };
        let rewritten = upstream_request_bytes(&head, &buffered);
        let text = String::from_utf8(rewritten).expect("ascii head");
        assert!(text.starts_with("GET /api HTTP/1.1\r\n"));
        assert!(text.contains("Host: a.example\r\n"));
        assert!(text.contains("X-Custom: yes\r\n"));
        assert!(text.ends_with("\r\n\r\nBODY"));
        assert!(!text.contains("keep-alive"));
        assert!(!text.to_ascii_lowercase().contains("connection"));
    }

    /// Large responses must relay completely on every platform. The unix
    /// socket buffer on macOS is 8 KiB — a fraction of the Linux default —
    /// so backpressure and teardown ordering bugs only show up there.
    #[test]
    fn relay_delivers_large_response_single_write() {
        relay_large_response(600 * 1024, 600 * 1024, 0);
    }

    #[test]
    fn relay_delivers_large_response_chunked_slow_writer() {
        relay_large_response(600 * 1024, 8 * 1024, 2);
    }

    fn relay_large_response(total: usize, chunk: usize, delay_ms: u64) {
        use std::net::TcpListener;
        use std::os::unix::net::UnixListener;

        let dir = std::env::temp_dir().join(format!(
            "cyg-relay-large-{}-{total}-{chunk}",
            std::process::id()
        ));
        let _ = std::fs::create_dir_all(&dir);
        let socket_path = dir.join("upstream.sock");
        let _ = std::fs::remove_file(&socket_path);
        let upstream_listener = UnixListener::bind(&socket_path).expect("bind upstream");

        let upstream_thread = thread::spawn(move || {
            let (mut conn, _) = upstream_listener.accept().expect("accept upstream");
            let mut buffer = [0_u8; 4096];
            let mut request = Vec::new();
            loop {
                let read = conn.read(&mut buffer).expect("read request");
                assert_ne!(read, 0, "upstream saw EOF before responding");
                request.extend_from_slice(&buffer[..read]);
                if request.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            let head =
                format!("HTTP/1.1 200 OK\r\nconnection: close\r\ncontent-length: {total}\r\n\r\n");
            conn.write_all(head.as_bytes()).expect("write head");
            let body = vec![b'z'; total];
            for piece in body.chunks(chunk) {
                conn.write_all(piece).expect("write body chunk");
                if delay_ms > 0 {
                    thread::sleep(Duration::from_millis(delay_ms));
                }
            }
            // The app closes after one exchange, exactly like a
            // connection: close upstream does.
        });

        let tcp_listener = TcpListener::bind("127.0.0.1:0").expect("bind tcp");
        let address = tcp_listener.local_addr().expect("tcp addr");
        let server_side = thread::spawn(move || tcp_listener.accept().expect("accept client").0);
        let mut client = TcpStream::connect(address).expect("connect client");
        let server_client = server_side.join().expect("join accept");

        client
            .write_all(b"GET / HTTP/1.1\r\nhost: x\r\nconnection: close\r\n\r\n")
            .expect("send request");

        let upstream = UnixStream::connect(&socket_path).expect("connect upstream");
        // Mirror serve_connection's socket configuration exactly.
        let _ = server_client.set_read_timeout(Some(RELAY_IDLE_TIMEOUT));
        let _ = server_client.set_write_timeout(Some(RELAY_IDLE_TIMEOUT));
        let _ = upstream.set_read_timeout(Some(RELAY_IDLE_TIMEOUT));
        let _ = upstream.set_write_timeout(Some(RELAY_IDLE_TIMEOUT));
        (&upstream)
            .write_all(b"GET / HTTP/1.1\r\nhost: x\r\nconnection: close\r\n\r\n")
            .expect("forward head");

        // Mirror serve_connection's epilogue: relay, then drop the streams.
        let relay_thread = thread::spawn(move || {
            let stats = relay(server_client, upstream, BodyGuard::none(), false).expect("relay");
            stats.to_client
        });

        let mut response = Vec::new();
        client.read_to_end(&mut response).expect("read response");
        let relayed = relay_thread.join().expect("join relay");
        let header_end = response
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .expect("response head complete")
            + 4;
        assert_eq!(
            response.len() - header_end,
            total,
            "client received a truncated body (relay reported {relayed} bytes)"
        );
        upstream_thread.join().expect("upstream thread");
        let _ = std::fs::remove_file(&socket_path);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Regression: sockets accepted from a nonblocking listener inherit
    /// O_NONBLOCK on macOS. Leaving them nonblocking makes body-complete GETs
    /// WouldBlock in forward_or_discard, SHUT_WR the upstream immediately, and
    /// truncate large responses (Bun ships ~8-16 KiB then stops).
    #[test]
    fn relay_delivers_large_response_from_nonblocking_accept() {
        use std::net::TcpListener;
        use std::os::unix::net::UnixListener;

        let total = 600 * 1024;
        let dir = std::env::temp_dir().join(format!("cyg-relay-nonblock-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let socket_path = dir.join("upstream.sock");
        let _ = std::fs::remove_file(&socket_path);
        let upstream_listener = UnixListener::bind(&socket_path).expect("bind upstream");

        let upstream_thread = thread::spawn(move || {
            let (mut conn, _) = upstream_listener.accept().expect("accept upstream");
            let mut buffer = [0_u8; 4096];
            let mut request = Vec::new();
            loop {
                let read = conn.read(&mut buffer).expect("read request");
                assert_ne!(read, 0, "upstream saw EOF before responding");
                request.extend_from_slice(&buffer[..read]);
                if request.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            let head = format!("HTTP/1.1 200 OK\r\ncontent-length: {total}\r\n\r\n");
            conn.write_all(head.as_bytes()).expect("write head");
            // Single large write like Bun after buffering the console document.
            conn.write_all(&vec![b'z'; total]).expect("write body");
            // Hold the socket open briefly so a premature peer SHUT_WR is the
            // only way to truncate — matching Bun's keep-open-until-done behavior.
            thread::sleep(Duration::from_millis(50));
        });

        let tcp_listener = TcpListener::bind("127.0.0.1:0").expect("bind tcp");
        // Production serve_until path: nonblocking accept.
        tcp_listener
            .set_nonblocking(true)
            .expect("listener nonblocking");
        let address = tcp_listener.local_addr().expect("tcp addr");

        let server_side = thread::spawn(move || {
            loop {
                match tcp_listener.accept() {
                    Ok((client, _)) => {
                        // The fix under test: clear O_NONBLOCK on the accepted socket.
                        client.set_nonblocking(false).expect("client blocking");
                        return client;
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(1));
                    }
                    Err(error) => panic!("accept: {error}"),
                }
            }
        });

        let mut client = TcpStream::connect(address).expect("connect client");
        let server_client = server_side.join().expect("join accept");

        client
            .write_all(b"GET / HTTP/1.1\r\nhost: x\r\n\r\n")
            .expect("send request");

        let upstream = UnixStream::connect(&socket_path).expect("connect upstream");
        let _ = server_client.set_read_timeout(Some(RELAY_IDLE_TIMEOUT));
        let _ = server_client.set_write_timeout(Some(RELAY_IDLE_TIMEOUT));
        let _ = upstream.set_read_timeout(Some(RELAY_IDLE_TIMEOUT));
        let _ = upstream.set_write_timeout(Some(RELAY_IDLE_TIMEOUT));
        (&upstream)
            .write_all(b"GET / HTTP/1.1\r\nhost: x\r\n\r\n")
            .expect("forward head");

        let relay_thread = thread::spawn(move || {
            let stats = relay(server_client, upstream, BodyGuard::none(), false).expect("relay");
            stats.to_client
        });

        let mut response = Vec::new();
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("client read timeout");
        client.read_to_end(&mut response).expect("read response");
        let relayed = relay_thread.join().expect("join relay");
        let header_end = response
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .expect("response head complete")
            + 4;
        assert_eq!(
            response.len() - header_end,
            total,
            "client received a truncated body (relay reported {relayed} bytes)"
        );
        upstream_thread.join().expect("upstream thread");
        let _ = std::fs::remove_file(&socket_path);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// macOS + Bun: early upstream SHUT_WR truncates large responses to ~8-16 KiB.
    /// Covers the real console index over a UDS Bun server.
    #[test]
    fn relay_delivers_large_bun_unix_response() {
        use std::net::TcpListener;
        use std::process::{Command, Stdio};

        let bun = std::env::var("CYGNUS_TEST_BUN")
            .unwrap_or_else(|_| std::env::var("HOME").unwrap() + "/.cygnus/bin/bun");
        let index = std::env::var("CYGNUS_TEST_INDEX").unwrap_or_else(|_| {
            std::env::var("HOME").unwrap() + "/.cygnus/console/opt/cygnus-console/dist/index.html"
        });
        if !std::path::Path::new(&bun).exists() || !std::path::Path::new(&index).exists() {
            eprintln!("skip bun relay test: bun or index missing");
            return;
        }
        let body = std::fs::read(&index).expect("read index");
        let total = body.len();
        assert!(total > 64 * 1024, "index should be large, got {total}");

        let dir = std::env::temp_dir().join(format!("cyg-bun-relay-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let socket_path = dir.join("console.sock");
        let _ = std::fs::remove_file(&socket_path);
        let server_js = dir.join("server.js");
        std::fs::write(
            &server_js,
            format!(
                r#"const bytes = new Uint8Array(await Bun.file({index:?}).arrayBuffer());
const server = Bun.serve({{
  unix: {sock:?},
  fetch() {{
    return new Response(bytes, {{ headers: {{ "content-type": "text/html", "cache-control": "no-store" }} }});
  }},
}});
console.log("ready", server.hostname);"#,
                index = index,
                sock = socket_path,
            ),
        )
        .unwrap();

        let mut child = Command::new(&bun)
            .arg(&server_js)
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn bun");
        // wait for socket
        for _ in 0..200 {
            if socket_path.exists()
                && let Ok(s) = UnixStream::connect(&socket_path)
            {
                drop(s);
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }

        let tcp_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = tcp_listener.local_addr().unwrap();
        let server_side = thread::spawn(move || tcp_listener.accept().unwrap().0);
        let mut client = TcpStream::connect(address).unwrap();
        let server_client = server_side.join().unwrap();

        client
            .write_all(b"GET / HTTP/1.1\r\nhost: x\r\n\r\n")
            .unwrap();

        let upstream = UnixStream::connect(&socket_path).expect("connect bun");
        let _ = server_client.set_read_timeout(Some(RELAY_IDLE_TIMEOUT));
        let _ = server_client.set_write_timeout(Some(RELAY_IDLE_TIMEOUT));
        let _ = upstream.set_read_timeout(Some(RELAY_IDLE_TIMEOUT));
        let _ = upstream.set_write_timeout(Some(RELAY_IDLE_TIMEOUT));
        // Production path: write request BEFORE relay
        (&upstream)
            .write_all(b"GET / HTTP/1.1\r\nhost: x\r\n\r\n")
            .unwrap();

        let relay_thread = thread::spawn(move || {
            let stats = relay(server_client, upstream, BodyGuard::none(), false).expect("relay");
            stats.to_client
        });

        let mut response = Vec::new();
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        // read until EOF or enough
        let mut buf = [0u8; 16 * 1024];
        loop {
            match client.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => response.extend_from_slice(&buf[..n]),
                Err(e)
                    if e.kind() == io::ErrorKind::WouldBlock
                        || e.kind() == io::ErrorKind::TimedOut =>
                {
                    break;
                }
                Err(e) => panic!("client read: {e}"),
            }
        }
        let relayed = relay_thread.join().expect("join relay");
        let header_end = response
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .map(|p| p + 4)
            .unwrap_or(0);
        let body_len = response.len().saturating_sub(header_end);
        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(
            body_len, total,
            "truncated bun body: got {body_len}, want {total}, relay reported {relayed}"
        );
    }

    /// Upgraded exchanges must tunnel both directions: client bytes after the
    /// request head are websocket frames, not garbage to discard.
    #[test]
    fn relay_tunnels_upgraded_exchanges_bidirectionally() {
        use std::net::TcpListener;
        use std::os::unix::net::UnixListener;

        let dir = std::env::temp_dir().join(format!("cyg-relay-ws-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let socket_path = dir.join("upstream.sock");
        let _ = std::fs::remove_file(&socket_path);
        let upstream_listener = UnixListener::bind(&socket_path).expect("bind upstream");

        let upstream_thread = thread::spawn(move || {
            let (mut conn, _) = upstream_listener.accept().expect("accept upstream");
            let mut buffer = [0_u8; 4096];
            let mut request = Vec::new();
            loop {
                let read = conn.read(&mut buffer).expect("read request");
                assert_ne!(read, 0, "upstream saw EOF before responding");
                request.extend_from_slice(&buffer[..read]);
                if request.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            conn.write_all(b"HTTP/1.1 101 Switching Protocols\r\nupgrade: test\r\n\r\n")
                .expect("write upgrade");
            // Echo one client frame back, prefixed, then close.
            let mut frame = [0_u8; 5];
            conn.read_exact(&mut frame).expect("read client frame");
            conn.write_all(b"echo:").expect("write echo prefix");
            conn.write_all(&frame).expect("write echo frame");
        });

        let tcp_listener = TcpListener::bind("127.0.0.1:0").expect("bind tcp");
        let address = tcp_listener.local_addr().expect("tcp addr");
        let server_side = thread::spawn(move || tcp_listener.accept().expect("accept client").0);
        let mut client = TcpStream::connect(address).expect("connect client");
        let mut server_client = server_side.join().expect("join accept");

        client
            .write_all(b"GET /ws HTTP/1.1\r\nhost: x\r\nupgrade: test\r\n\r\n")
            .expect("send handshake");
        // serve_connection consumes the request head before relaying; the
        // relay must never see handshake bytes on the client leg.
        let (_, _) = read_head(&mut server_client).expect("consume handshake");

        let upstream = UnixStream::connect(&socket_path).expect("connect upstream");
        let _ = server_client.set_read_timeout(Some(RELAY_IDLE_TIMEOUT));
        let _ = upstream.set_read_timeout(Some(RELAY_IDLE_TIMEOUT));
        (&upstream)
            .write_all(b"GET /ws HTTP/1.1\r\nhost: x\r\nupgrade: test\r\n\r\n")
            .expect("forward head");

        let relay_thread =
            thread::spawn(move || relay(server_client, upstream, BodyGuard::none(), false));

        // Read the 101 head, then speak through the tunnel.
        let mut head = Vec::new();
        let mut byte = [0_u8; 1];
        while !head.windows(4).any(|w| w == b"\r\n\r\n") {
            client.read_exact(&mut byte).expect("read upgrade head");
            head.push(byte[0]);
        }
        client.write_all(b"hello").expect("send frame");
        let mut echo = Vec::new();
        client.read_to_end(&mut echo).expect("read echo");
        assert_eq!(echo, b"echo:hello");

        let stats = relay_thread
            .join()
            .expect("join relay")
            .expect("relay result");
        assert_eq!(stats.status, 101);
        assert_eq!(stats.to_upstream, 5);
        upstream_thread.join().expect("upstream thread");
        let _ = std::fs::remove_file(&socket_path);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The regression that broke every async app: the relay used to half-close
    /// the upstream write side as soon as the request body was complete, which
    /// upstream servers treat as a client abort. The response must arrive even
    /// when the upstream thinks before answering.
    #[test]
    fn relay_delivers_response_written_after_request_completes() {
        use std::net::TcpListener;
        use std::os::unix::net::UnixListener;

        let dir = std::env::temp_dir().join(format!("cygnus-relay-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let socket_path = dir.join("upstream.sock");
        let _ = std::fs::remove_file(&socket_path);
        let upstream_listener = UnixListener::bind(&socket_path).expect("bind upstream");

        let upstream_thread = thread::spawn(move || {
            let (mut conn, _) = upstream_listener.accept().expect("accept upstream");
            let mut buffer = [0_u8; 4096];
            let mut request = Vec::new();
            loop {
                let read = conn.read(&mut buffer).expect("read request");
                assert_ne!(read, 0, "upstream saw EOF before responding");
                request.extend_from_slice(&buffer[..read]);
                if request.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            // Simulate an app doing async work before it responds. With the
            // old eager half-close the client-side copy has already finished
            // and shut the socket down by now.
            thread::sleep(Duration::from_millis(150));
            conn.write_all(b"HTTP/1.1 200 OK\r\nconnection: close\r\ncontent-length: 2\r\n\r\nhi")
                .expect("write response");
        });

        let tcp_listener = TcpListener::bind("127.0.0.1:0").expect("bind tcp");
        let address = tcp_listener.local_addr().expect("tcp addr");
        let server_side = thread::spawn(move || tcp_listener.accept().expect("accept client").0);
        let mut client = TcpStream::connect(address).expect("connect client");
        let server_client = server_side.join().expect("join accept");

        client
            .write_all(b"GET / HTTP/1.1\r\nhost: x\r\nconnection: close\r\n\r\n")
            .expect("send request");

        let upstream = UnixStream::connect(&socket_path).expect("connect upstream");
        upstream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("upstream timeout");
        server_client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("client timeout");
        (&upstream)
            .write_all(b"GET / HTTP/1.1\r\nhost: x\r\nconnection: close\r\n\r\n")
            .expect("forward head");
        // Half-close our sending side so the drain observes EOF promptly.
        client
            .shutdown(Shutdown::Write)
            .expect("client done sending");

        let stats = relay(server_client, upstream, BodyGuard::none(), false).expect("relay");
        assert_eq!(stats.status, 200);

        let mut response = Vec::new();
        client.read_to_end(&mut response).expect("read response");
        let text = String::from_utf8_lossy(&response);
        assert!(text.starts_with("HTTP/1.1 200 OK"), "got: {text}");
        assert!(text.ends_with("hi"), "got: {text}");
        upstream_thread.join().expect("upstream thread");
        let _ = std::fs::remove_file(&socket_path);
    }

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

    fn request_head(target: &str, method: &str) -> RequestHead {
        let raw = format!(
            "{method} {target} HTTP/1.1\r\nHost: cygnus.apps.test\r\nContent-Length: 0\r\n\r\n"
        );
        match parse_request_head(raw.as_bytes()) {
            HeadParse::Complete(head) => head,
            _ => panic!("request head did not parse"),
        }
    }

    fn serve_once(frontend: Arc<Frontend>, request: &[u8]) -> Vec<u8> {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        // Bind the listener before the client connects so the kernel completes
        // the handshake immediately. On busy CI runners a connection can race
        // ahead of the spawned worker and surface as NotConnected/Aborted at
        // accept time; retry until the worker has caught up.
        listener.set_nonblocking(false).unwrap();
        let worker = thread::spawn(move || {
            loop {
                match listener.accept() {
                    Ok((client, _)) => {
                        frontend.serve_connection(client);
                        return;
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(1));
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error)
                        if matches!(
                            error.raw_os_error(),
                            Some(libc::ENOTCONN) | Some(libc::ECONNABORTED)
                        ) =>
                    {
                        thread::sleep(Duration::from_millis(1));
                    }
                    Err(error) => panic!("accept failed: {error}"),
                }
            }
        });
        let mut client = TcpStream::connect(address).unwrap();
        client.write_all(request).unwrap();
        // The server may already have answered and closed (e.g. a 429 that
        // never reads the body). On some platforms that leaves the client
        // half-closed with ENOTCONN/ECONNRESET/EPIPE at shutdown; treat those
        // the same as a successful half-close so the subsequent read can still
        // drain whatever response bytes arrived.
        if let Err(error) = client.shutdown(Shutdown::Write) {
            assert!(
                matches!(
                    error.kind(),
                    io::ErrorKind::NotConnected
                        | io::ErrorKind::ConnectionReset
                        | io::ErrorKind::BrokenPipe
                        | io::ErrorKind::ConnectionAborted
                ),
                "unexpected shutdown error: {error}"
            );
        }
        let mut response = Vec::new();
        if let Err(error) = client.read_to_end(&mut response) {
            assert!(
                matches!(
                    error.kind(),
                    io::ErrorKind::ConnectionReset
                        | io::ErrorKind::ConnectionAborted
                        | io::ErrorKind::BrokenPipe
                        | io::ErrorKind::NotConnected
                ),
                "unexpected read error: {error}"
            );
        }
        worker.join().unwrap();
        response
    }

    #[test]
    fn only_the_exact_github_webhook_post_gets_the_github_body_bound() {
        let defaults = IngressLimits::default();

        assert_eq!(
            request_body_limits(&request_head("/github/webhook", "POST"), &defaults).max_body_bytes,
            MAX_GITHUB_WEBHOOK_BODY_BYTES
        );
        for head in [
            request_head("/github/webhook", "GET"),
            request_head("/github/webhook/extra", "POST"),
            request_head("/github/webhook?delivery=1", "POST"),
        ] {
            assert_eq!(
                request_body_limits(&head, &defaults).max_body_bytes,
                defaults.max_body_bytes
            );
        }
    }

    #[test]
    fn only_the_exact_deploy_chunk_post_gets_the_deploy_chunk_body_bound() {
        let defaults = IngressLimits::default();

        assert_eq!(
            request_body_limits(&request_head("/api/v1/deploy/chunk", "POST"), &defaults)
                .max_body_bytes,
            MAX_DEPLOY_CHUNK_BODY_BYTES
        );
        for head in [
            request_head("/api/v1/deploy/chunk", "GET"),
            request_head("/api/v1/deploy/chunk/", "POST"),
            request_head("/api/v1/deploy/chunk?part=1", "POST"),
            request_head("/api/v1/deploy", "POST"),
        ] {
            assert_eq!(
                request_body_limits(&head, &defaults).max_body_bytes,
                defaults.max_body_bytes
            );
        }
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
    fn streamed_chunked_body_is_cut_off_before_oversized_bytes_reach_upstream() {
        let mut reader = Cursor::new(b"4\r\nWiki\r\n0\r\n\r\n".to_vec());
        let mut upstream = Vec::new();
        let error = copy_to_upstream(
            &mut reader,
            &mut upstream,
            BodyGuard::chunked(3),
            &AtomicBool::new(false),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(upstream.is_empty());
    }

    #[test]
    fn response_copy_preserves_bytes_and_captures_status() {
        let response = b"HTTP/1.1 206 Partial Content\r\ncontent-length: 4\r\n\r\ndata";
        let mut reader = ChunkedReader {
            data: response.to_vec(),
            position: 0,
            chunk: 5,
        };
        let mut output = Vec::new();
        let (copied, status) =
            copy_response_to_client(&mut reader, &mut output, false, &AtomicBool::new(false))
                .unwrap();
        assert_eq!(copied, response.len() as u64);
        assert_eq!(status, 206);
        assert_eq!(output, response);

        let mut no_status = Cursor::new(b"opaque response".to_vec());
        let (_, status) = copy_response_to_client(
            &mut no_status,
            &mut Vec::new(),
            false,
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(status, 0);
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
            version: "HTTP/1.1".into(),
            host: Some("api.example.com".into()),
            head_len: 0,
            body: BodyFraming::None,
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
            version: "HTTP/1.1".into(),
            host: Some("API.EXAMPLE.COM:443".into()),
            head_len: 0,
            body: BodyFraming::None,
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
    fn oversized_declared_body_is_rejected_before_routing() {
        let router = Arc::new(Router::new(RouteTable::new()));
        let supervisor = Arc::new(Supervisor::new(|_| Err("must not boot".into())));
        let limits = IngressLimits {
            max_body_bytes: 10,
            ..IngressLimits::default()
        };
        let frontend = Arc::new(Frontend::with_limits(router, supervisor, limits).unwrap());
        let response = serve_once(
            frontend,
            b"POST / HTTP/1.1\r\nHost: api.example.com\r\nContent-Length: 11\r\n\r\n",
        );
        assert!(response.starts_with(b"HTTP/1.1 413 Payload Too Large"));
    }

    #[test]
    fn peer_concurrency_limit_returns_retryable_429() {
        let router = Arc::new(Router::new(RouteTable::new()));
        let supervisor = Arc::new(Supervisor::new(|_| Err("must not boot".into())));
        let limits = IngressLimits {
            max_connections_per_ip: 1,
            ..IngressLimits::default()
        };
        let frontend = Arc::new(Frontend::with_limits(router, supervisor, limits).unwrap());
        let _held = frontend
            .ingress
            .enter_connection("127.0.0.1".parse().unwrap())
            .unwrap();
        let response = serve_once(frontend, b"GET / HTTP/1.1\r\nHost: api.example.com\r\n\r\n");
        assert!(response.starts_with(b"HTTP/1.1 429 Too Many Requests"));
        assert!(
            response
                .windows(b"retry-after: 1".len())
                .any(|window| window == b"retry-after: 1")
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
        assert!(error_response(Status::RequestTimeout).starts_with(b"HTTP/1.1 408"));
        assert!(error_response(Status::PayloadTooLarge).starts_with(b"HTTP/1.1 413"));
        assert!(error_response(Status::TooManyRequests).starts_with(b"HTTP/1.1 429"));
    }
}
