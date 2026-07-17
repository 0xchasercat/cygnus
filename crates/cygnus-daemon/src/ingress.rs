use std::collections::HashMap;
use std::io::Write as _;
use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;
use serde::Serialize;

use crate::metrics::{MetricsHub, RequestRecord};

#[derive(Clone, Debug)]
pub struct IngressLimits {
    pub max_body_bytes: u64,
    pub max_connections_per_ip: usize,
    pub max_connections_per_app: usize,
    pub requests_per_second_per_ip: u32,
    pub request_burst_per_ip: u32,
}

impl Default for IngressLimits {
    fn default() -> Self {
        Self {
            max_body_bytes: 16 * 1024 * 1024,
            max_connections_per_ip: 128,
            max_connections_per_app: 256,
            requests_per_second_per_ip: 100,
            request_burst_per_ip: 200,
        }
    }
}

impl IngressLimits {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.max_body_bytes == 0
            || self.max_connections_per_ip == 0
            || self.max_connections_per_app == 0
            || self.requests_per_second_per_ip == 0
            || self.request_burst_per_ip == 0
            || self.request_burst_per_ip < self.requests_per_second_per_ip
        {
            return Err("ingress limits must be non-zero and burst must cover one second");
        }
        Ok(())
    }
}

#[derive(Clone)]
pub(crate) struct IngressController {
    inner: Arc<ControllerInner>,
}

struct ControllerInner {
    limits: IngressLimits,
    state: Mutex<ControllerState>,
}

#[derive(Default)]
struct ControllerState {
    peers: HashMap<IpAddr, PeerState>,
    apps: HashMap<String, usize>,
}

struct PeerState {
    connections: usize,
    tokens: f64,
    refilled_at: Instant,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LimitRejection {
    PeerConcurrency,
    AppConcurrency,
    Rate,
}

impl IngressController {
    pub(crate) fn new(limits: IngressLimits) -> Result<Self, &'static str> {
        limits.validate()?;
        Ok(Self {
            inner: Arc::new(ControllerInner {
                limits,
                state: Mutex::new(ControllerState::default()),
            }),
        })
    }

    pub(crate) fn limits(&self) -> &IngressLimits {
        &self.inner.limits
    }

    pub(crate) fn enter_connection(
        &self,
        peer: IpAddr,
    ) -> Result<ConnectionPermit, LimitRejection> {
        let mut state = self.inner.state.lock();
        let burst = f64::from(self.inner.limits.request_burst_per_ip);
        let peer_state = state.peers.entry(peer).or_insert_with(|| PeerState {
            connections: 0,
            tokens: burst,
            refilled_at: Instant::now(),
        });
        if peer_state.connections >= self.inner.limits.max_connections_per_ip {
            return Err(LimitRejection::PeerConcurrency);
        }
        peer_state.connections += 1;
        Ok(ConnectionPermit {
            controller: self.clone(),
            peer,
        })
    }

    pub(crate) fn enter_request(
        &self,
        peer: IpAddr,
        app: &str,
    ) -> Result<RequestPermit, LimitRejection> {
        let mut state = self.inner.state.lock();
        let now = Instant::now();
        let rate = f64::from(self.inner.limits.requests_per_second_per_ip);
        let burst = f64::from(self.inner.limits.request_burst_per_ip);
        let app_connections = state.apps.get(app).copied().unwrap_or(0);
        if app_connections >= self.inner.limits.max_connections_per_app {
            return Err(LimitRejection::AppConcurrency);
        }
        let peer_state = state.peers.entry(peer).or_insert_with(|| PeerState {
            connections: 0,
            tokens: burst,
            refilled_at: now,
        });
        peer_state.tokens = (peer_state.tokens
            + now.duration_since(peer_state.refilled_at).as_secs_f64() * rate)
            .min(burst);
        peer_state.refilled_at = now;
        if peer_state.tokens < 1.0 {
            return Err(LimitRejection::Rate);
        }
        peer_state.tokens -= 1.0;
        state.apps.insert(app.to_owned(), app_connections + 1);
        Ok(RequestPermit {
            controller: self.clone(),
            app: app.to_owned(),
        })
    }
}

pub(crate) struct ConnectionPermit {
    controller: IngressController,
    peer: IpAddr,
}

impl Drop for ConnectionPermit {
    fn drop(&mut self) {
        let mut state = self.controller.inner.state.lock();
        if let Some(peer) = state.peers.get_mut(&self.peer) {
            peer.connections = peer.connections.saturating_sub(1);
            if peer.connections == 0
                && peer.tokens >= f64::from(self.controller.inner.limits.request_burst_per_ip)
            {
                state.peers.remove(&self.peer);
            }
        }
    }
}

pub(crate) struct RequestPermit {
    controller: IngressController,
    app: String,
}

impl Drop for RequestPermit {
    fn drop(&mut self) {
        let mut state = self.controller.inner.state.lock();
        if let Some(count) = state.apps.get_mut(&self.app) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                state.apps.remove(&self.app);
            }
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BodyGuardError {
    Malformed,
    TooLarge,
}

pub(crate) struct BodyGuard {
    max: u64,
    decoded: u64,
    state: ChunkState,
}

enum ChunkState {
    Fixed(u64),
    Line { trailer: bool, bytes: Vec<u8> },
    Data(u64),
    DataCrlf(u8),
    Done,
}

impl BodyGuard {
    pub(crate) fn none() -> Self {
        Self {
            max: 0,
            decoded: 0,
            state: ChunkState::Done,
        }
    }

    pub(crate) fn fixed(length: u64) -> Self {
        Self {
            max: length,
            decoded: 0,
            state: if length == 0 {
                ChunkState::Done
            } else {
                ChunkState::Fixed(length)
            },
        }
    }

    pub(crate) fn chunked(max: u64) -> Self {
        Self {
            max,
            decoded: 0,
            state: ChunkState::Line {
                trailer: false,
                bytes: Vec::new(),
            },
        }
    }

    pub(crate) fn is_complete(&self) -> bool {
        matches!(self.state, ChunkState::Done)
    }

    pub(crate) fn observe(&mut self, input: &[u8]) -> Result<(), BodyGuardError> {
        let mut offset = 0;
        while offset < input.len() {
            match &mut self.state {
                ChunkState::Done => return Err(BodyGuardError::Malformed),
                ChunkState::Fixed(remaining) => {
                    let available = (input.len() - offset) as u64;
                    if available > *remaining {
                        return Err(BodyGuardError::Malformed);
                    }
                    *remaining -= available;
                    self.decoded += available;
                    offset = input.len();
                    if *remaining == 0 {
                        self.state = ChunkState::Done;
                    }
                }
                ChunkState::Data(remaining) => {
                    let take = (*remaining).min((input.len() - offset) as u64);
                    self.decoded = self
                        .decoded
                        .checked_add(take)
                        .ok_or(BodyGuardError::TooLarge)?;
                    if self.decoded > self.max {
                        return Err(BodyGuardError::TooLarge);
                    }
                    *remaining -= take;
                    offset += take as usize;
                    if *remaining == 0 {
                        self.state = ChunkState::DataCrlf(0);
                    }
                }
                ChunkState::DataCrlf(position) => {
                    let expected = if *position == 0 { b'\r' } else { b'\n' };
                    if input[offset] != expected {
                        return Err(BodyGuardError::Malformed);
                    }
                    offset += 1;
                    if *position == 0 {
                        *position = 1;
                    } else {
                        self.state = ChunkState::Line {
                            trailer: false,
                            bytes: Vec::new(),
                        };
                    }
                }
                ChunkState::Line { trailer, bytes } => {
                    let byte = input[offset];
                    offset += 1;
                    bytes.push(byte);
                    if bytes.len() > 64 * 1024 {
                        return Err(BodyGuardError::Malformed);
                    }
                    if byte != b'\n' {
                        continue;
                    }
                    if bytes.len() < 2 || bytes[bytes.len() - 2] != b'\r' {
                        return Err(BodyGuardError::Malformed);
                    }
                    bytes.truncate(bytes.len() - 2);
                    if *trailer {
                        if bytes.is_empty() {
                            self.state = ChunkState::Done;
                        } else {
                            bytes.clear();
                        }
                        continue;
                    }
                    let size_field = bytes.split(|byte| *byte == b';').next().unwrap_or_default();
                    let size_text =
                        std::str::from_utf8(size_field).map_err(|_| BodyGuardError::Malformed)?;
                    let size = u64::from_str_radix(size_text.trim(), 16)
                        .map_err(|_| BodyGuardError::Malformed)?;
                    if size > self.max.saturating_sub(self.decoded) {
                        return Err(BodyGuardError::TooLarge);
                    }
                    if size == 0 {
                        self.state = ChunkState::Line {
                            trailer: true,
                            bytes: Vec::new(),
                        };
                    } else {
                        self.state = ChunkState::Data(size);
                    }
                }
            }
        }
        Ok(())
    }
}

const MAX_RESPONSE_STATUS_LINE_BYTES: usize = 1_024;

#[derive(Debug)]
pub(crate) struct ResponseStatus {
    line: [u8; MAX_RESPONSE_STATUS_LINE_BYTES],
    len: usize,
    complete: bool,
    status: Option<u16>,
}

impl Default for ResponseStatus {
    fn default() -> Self {
        Self {
            line: [0; MAX_RESPONSE_STATUS_LINE_BYTES],
            len: 0,
            complete: false,
            status: None,
        }
    }
}

impl ResponseStatus {
    pub(crate) fn observe(&mut self, bytes: &[u8]) {
        if self.complete {
            return;
        }
        for &byte in bytes {
            if self.len == self.line.len() {
                self.complete = true;
                return;
            }
            self.line[self.len] = byte;
            self.len += 1;
            if byte.is_ascii_whitespace()
                && let Some(status) = parse_response_status(&self.line[..self.len])
            {
                self.status = Some(status);
                self.complete = true;
                return;
            }
            if byte == b'\n' {
                self.complete = true;
                return;
            }
        }
    }

    pub(crate) fn status(&self) -> Option<u16> {
        self.status
    }
}

fn parse_response_status(line: &[u8]) -> Option<u16> {
    let line = std::str::from_utf8(line)
        .ok()?
        .trim_end_matches(|character| matches!(character, '\r' | '\n'));
    let mut fields = line.split_ascii_whitespace();
    let version = fields.next()?;
    let status = fields.next()?;
    if !version.starts_with("HTTP/")
        || status.len() != 3
        || !status.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    let status = status.parse().ok()?;
    (100..=599).contains(&status).then_some(status)
}

static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Serialize)]
struct RequestEvent<'a> {
    event: &'static str,
    timestamp_unix_ms: u128,
    request_id: &'a str,
    protocol: &'a str,
    peer_ip: IpAddr,
    method: Option<&'a str>,
    host: Option<&'a str>,
    app: Option<&'a str>,
    outcome: &'a str,
    edge_status: Option<u16>,
    duration_ms: u128,
    bytes_from_client: u64,
    bytes_to_client: u64,
}

pub(crate) struct RequestSpan {
    metrics: MetricsHub,
    started: Instant,
    started_unix_ms: u64,
    request_id: String,
    protocol: &'static str,
    peer_ip: IpAddr,
    method: Option<String>,
    host: Option<String>,
    app: Option<String>,
    path: Option<String>,
    outcome: &'static str,
    edge_status: Option<u16>,
    upstream_status: Option<u16>,
    cold: bool,
    bytes_from_client: u64,
    bytes_to_client: u64,
}

impl RequestSpan {
    pub(crate) fn new(metrics: MetricsHub, protocol: &'static str, peer_ip: IpAddr) -> Self {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let sequence = NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed);
        Self {
            metrics,
            started: Instant::now(),
            started_unix_ms: u64::try_from(now.as_millis()).unwrap_or(u64::MAX),
            request_id: format!("{:016x}{sequence:016x}", now.as_nanos()),
            protocol,
            peer_ip,
            method: None,
            host: None,
            app: None,
            path: None,
            outcome: "connection_closed",
            edge_status: None,
            upstream_status: None,
            cold: false,
            bytes_from_client: 0,
            bytes_to_client: 0,
        }
    }

    pub(crate) fn set_head(
        &mut self,
        method: &str,
        host: Option<&str>,
        target: &str,
        bytes: usize,
    ) {
        self.method = Some(method.into());
        self.host = host.map(str::to_owned);
        self.path = Some(target.to_owned());
        self.bytes_from_client = bytes as u64;
    }

    pub(crate) fn set_app(&mut self, app: &str) {
        self.app = Some(app.into());
    }

    pub(crate) fn set_cold(&mut self, cold: bool) {
        self.cold = cold;
    }

    pub(crate) fn responded(&mut self, status: u16, outcome: &'static str, bytes: usize) {
        self.edge_status = Some(status);
        self.outcome = outcome;
        self.bytes_to_client = bytes as u64;
    }

    pub(crate) fn proxied(&mut self, status: u16, to_upstream: u64, to_client: u64) {
        self.outcome = "proxied";
        self.upstream_status = Some(status);
        self.bytes_from_client += to_upstream;
        self.bytes_to_client = to_client;
    }

    pub(crate) fn relay_error(&mut self) {
        self.outcome = "relay_error";
    }

    fn event(&self) -> RequestEvent<'_> {
        RequestEvent {
            event: "request",
            timestamp_unix_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis(),
            request_id: &self.request_id,
            protocol: self.protocol,
            peer_ip: self.peer_ip,
            method: self.method.as_deref(),
            host: self.host.as_deref(),
            app: self.app.as_deref(),
            outcome: self.outcome,
            edge_status: self.edge_status,
            duration_ms: self.started.elapsed().as_millis(),
            bytes_from_client: self.bytes_from_client,
            bytes_to_client: self.bytes_to_client,
        }
    }
}

impl Drop for RequestSpan {
    fn drop(&mut self) {
        self.metrics.record_request(RequestRecord {
            time_ms: self.started_unix_ms,
            request_id: self.request_id.clone(),
            method: self.method.clone().unwrap_or_default(),
            host: self.host.clone().unwrap_or_default(),
            app: self.app.clone().unwrap_or_default(),
            path: self.path.clone().unwrap_or_default(),
            status: self
                .edge_status
                .or(self.upstream_status)
                .unwrap_or_default(),
            duration_ms: self.started.elapsed().as_secs_f64() * 1_000.0,
            cold: self.cold,
            protocol: self.protocol.to_owned(),
            bytes_in: self.bytes_from_client,
            bytes_out: self.bytes_to_client,
        });

        let event = self.event();
        let stderr = std::io::stderr();
        let mut writer = stderr.lock();
        if serde_json::to_writer(&mut writer, &event).is_ok() {
            let _ = writer.write_all(b"\n");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits() -> IngressLimits {
        IngressLimits {
            max_body_bytes: 10,
            max_connections_per_ip: 1,
            max_connections_per_app: 1,
            requests_per_second_per_ip: 1,
            request_burst_per_ip: 1,
        }
    }

    #[test]
    fn connection_and_app_permits_release_on_drop() {
        let controller = IngressController::new(limits()).unwrap();
        let peer = "127.0.0.1".parse().unwrap();
        let connection = controller.enter_connection(peer).unwrap();
        assert!(matches!(
            controller.enter_connection(peer),
            Err(LimitRejection::PeerConcurrency)
        ));
        let request = controller.enter_request(peer, "api").unwrap();
        assert!(matches!(
            controller.enter_request("127.0.0.2".parse().unwrap(), "api"),
            Err(LimitRejection::AppConcurrency)
        ));
        drop(request);
        drop(connection);
        assert!(controller.enter_connection(peer).is_ok());
    }

    #[test]
    fn token_bucket_rejects_a_burst_above_capacity() {
        let controller = IngressController::new(limits()).unwrap();
        let peer = "127.0.0.1".parse().unwrap();
        let _connection = controller.enter_connection(peer).unwrap();
        let first = controller.enter_request(peer, "one").unwrap();
        drop(first);
        assert!(matches!(
            controller.enter_request(peer, "two"),
            Err(LimitRejection::Rate)
        ));
    }
    #[test]
    fn chunked_body_limit_survives_arbitrary_read_boundaries() {
        let mut guard = BodyGuard::chunked(9);
        for part in [
            b"4\r".as_slice(),
            b"\nWiki\r\n5\r\nped".as_slice(),
            b"ia\r\n0\r\nX: y\r\n\r\n".as_slice(),
        ] {
            guard.observe(part).unwrap();
        }
        let mut too_large = BodyGuard::chunked(3);
        assert_eq!(
            too_large.observe(b"4\r\nWiki"),
            Err(BodyGuardError::TooLarge)
        );
        let mut malformed = BodyGuard::chunked(10);
        assert_eq!(malformed.observe(b"1\nX"), Err(BodyGuardError::Malformed));
    }

    #[test]
    fn fixed_and_empty_bodies_reject_pipelined_bytes() {
        let mut fixed = BodyGuard::fixed(2);
        fixed.observe(b"a").unwrap();
        fixed.observe(b"b").unwrap();
        assert!(fixed.is_complete());
        assert_eq!(fixed.observe(b"next"), Err(BodyGuardError::Malformed));
        assert_eq!(
            BodyGuard::none().observe(b"next"),
            Err(BodyGuardError::Malformed)
        );
    }

    #[test]
    fn request_event_is_structured_and_excludes_the_target() {
        let mut span = std::mem::ManuallyDrop::new(RequestSpan::new(
            MetricsHub::new(),
            "https",
            "192.0.2.1".parse().unwrap(),
        ));
        span.set_head("POST", Some("api.example.com"), "/secret", 512);
        span.set_app("api");
        span.proxied(201, 1024, 2048);
        let event = serde_json::to_value(span.event()).unwrap();
        assert_eq!(event["event"], "request");
        assert_eq!(event["protocol"], "https");
        assert_eq!(event["app"], "api");
        assert_eq!(event["bytes_from_client"], 1536);
        assert_eq!(event["bytes_to_client"], 2048);
        assert_eq!(event["request_id"].as_str().unwrap().len(), 32);
        assert!(event.get("target").is_none());
    }

    #[test]
    fn request_span_records_truncated_path_status_and_cold_flag() {
        let metrics = MetricsHub::new();
        {
            let mut span = RequestSpan::new(
                metrics.clone(),
                "http",
                "192.0.2.1".parse().unwrap(),
            );
            let target = format!("{}é", "a".repeat(199));
            span.set_head("GET", Some("api.example.com"), &target, 64);
            span.set_app("api");
            span.set_cold(true);
            span.proxied(202, 10, 20);
        }

        let requests = metrics.list_requests(1);
        let request = &requests[0];
        assert_eq!(request.path, "a".repeat(199));
        assert_eq!(request.status, 202);
        assert!(request.cold);
        assert_eq!(request.bytes_in, 74);
        assert_eq!(request.bytes_out, 20);
    }

    #[test]
    fn response_status_observer_handles_split_and_unobservable_lines() {
        let mut status = ResponseStatus::default();
        status.observe(b"HTTP/1.1 4");
        assert_eq!(status.status(), None);
        status.observe(b"29 Too Many Requests\r\ncontent-length: 0\r\n\r\n");
        assert_eq!(status.status(), Some(429));

        let mut invalid = ResponseStatus::default();
        invalid.observe(b"not-http\r\n");
        assert_eq!(invalid.status(), None);

        let mut bounded = ResponseStatus::default();
        bounded.observe(&[b'x'; MAX_RESPONSE_STATUS_LINE_BYTES + 1]);
        bounded.observe(b"HTTP/1.1 200 OK\r\n");
        assert_eq!(bounded.status(), None);
    }
}
