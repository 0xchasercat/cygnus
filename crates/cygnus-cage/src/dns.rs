//! Host DNS forwarding for exact-domain build egress.
//!
//! This is deliberately a small DNS wire proxy rather than a resolver. It
//! admits one exact A question, forwards the original bytes to the host's
//! upstream resolver, validates the response, installs public answer
//! addresses into the requesting cage's timed nftables set, and only then
//! returns the untouched upstream response.

use std::collections::{HashMap, HashSet};
use std::io::{self, ErrorKind, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4, TcpListener, TcpStream, UdpSocket};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use hickory_proto::op::{Message, MessageType, Metadata, OpCode, ResponseCode};
use hickory_proto::rr::{DNSClass, RData, RecordType};
use hickory_proto::serialize::binary::{BinDecodable, BinDecoder};

use crate::error::CageError;
use crate::net::{self, GATEWAY, cage_ipv4};
use crate::spec::DomainEgressRule;

const DNS_PORT: u16 = 53;
const WORKER_COUNT: usize = 4;
const WORK_QUEUE_CAPACITY: usize = 128;
const POLL_INTERVAL: Duration = Duration::from_millis(20);
const CLIENT_TIMEOUT: Duration = Duration::from_secs(2);
const UPSTREAM_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_DNS_MESSAGE: usize = u16::MAX as usize;
/// Lower TTL bound keeps an address present until the current request's
/// connection can be established.
pub const MIN_TTL_SECS: u32 = 1;
/// Upper TTL bound keeps this transient build policy from becoming permanent.
pub const MAX_TTL_SECS: u32 = 3_600;

/// Host-side DNS proxy shared by all domain-restricted build cages.
///
/// [`start`](Self::start) binds both UDP and TCP specifically to
/// `100.64.0.1:53`. A fixed worker pool serves a bounded queue; overload drops
/// new work rather than creating an unbounded thread per packet or connection.
#[derive(Debug)]
pub struct DnsForwarder {
    registry: Arc<Registry>,
    stop: Arc<AtomicBool>,
    sender: Option<SyncSender<Work>>,
    threads: Vec<JoinHandle<()>>,
}

impl DnsForwarder {
    /// Ensure the cage bridge exists and start UDP and TCP DNS listeners.
    pub fn start() -> Result<Self, CageError> {
        net::ensure_bridge()?;
        let resolv_conf = std::fs::read_to_string("/etc/resolv.conf")
            .map_err(|source| network_error("read host resolv.conf", source))?;
        let upstream_ip =
            nameserver_from_resolv_conf(&resolv_conf).ok_or_else(|| CageError::Network {
                operation: "select host DNS upstream".into(),
                detail: "no valid non-gateway nameserver in /etc/resolv.conf".into(),
            })?;
        let upstream = SocketAddr::new(upstream_ip, DNS_PORT);
        let listen = SocketAddrV4::new(GATEWAY, DNS_PORT);
        let udp = Arc::new(
            UdpSocket::bind(listen)
                .map_err(|source| network_error("bind host DNS UDP listener", source))?,
        );
        udp.set_nonblocking(true)
            .map_err(|source| network_error("configure host DNS UDP listener", source))?;
        let tcp = TcpListener::bind(listen)
            .map_err(|source| network_error("bind host DNS TCP listener", source))?;
        tcp.set_nonblocking(true)
            .map_err(|source| network_error("configure host DNS TCP listener", source))?;

        Self::start_bound(udp, tcp, upstream)
    }

    fn start_bound(
        udp: Arc<UdpSocket>,
        tcp: TcpListener,
        upstream: SocketAddr,
    ) -> Result<Self, CageError> {
        let registry = Arc::new(Registry::default());
        let stop = Arc::new(AtomicBool::new(false));
        let (sender, receiver) = mpsc::sync_channel(WORK_QUEUE_CAPACITY);
        let receiver = Arc::new(Mutex::new(receiver));
        let runtime = Arc::new(Runtime {
            registry: Arc::clone(&registry),
            udp: Arc::clone(&udp),
            upstream,
        });
        let mut threads = Vec::with_capacity(WORKER_COUNT + 2);

        {
            let stop = Arc::clone(&stop);
            let sender = sender.clone();
            threads.push(thread::spawn(move || udp_listener(udp, sender, stop)));
        }
        {
            let stop = Arc::clone(&stop);
            let sender = sender.clone();
            threads.push(thread::spawn(move || tcp_listener(tcp, sender, stop)));
        }
        for _ in 0..WORKER_COUNT {
            let stop = Arc::clone(&stop);
            let receiver = Arc::clone(&receiver);
            let runtime = Arc::clone(&runtime);
            threads.push(thread::spawn(move || worker(receiver, runtime, stop)));
        }

        Ok(Self {
            registry,
            stop,
            sender: Some(sender),
            threads,
        })
    }

    /// Register one cage after its `inet cygnus` nftables policy is loaded.
    ///
    /// The source address is derived deterministically from `cage_name`.
    /// Re-registering the same address advances its generation; dropping an
    /// older lease cannot remove or mutate the newer PID's registration.
    pub fn register(
        &self,
        cage_name: &str,
        host_pid: i32,
        rules: &[DomainEgressRule],
    ) -> Result<DnsLease, CageError> {
        if host_pid <= 0 {
            return Err(CageError::InvalidSpec(
                "DNS registration host PID must be greater than zero".into(),
            ));
        }
        for rule in rules {
            validate_domain_rule(rule)?;
        }
        if rules.is_empty() {
            return Err(CageError::InvalidSpec(
                "DNS registration requires at least one domain rule".into(),
            ));
        }
        Ok(self.registry.register(cage_name, host_pid, rules))
    }
}

impl Drop for DnsForwarder {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        // Dropping the last sender disconnects workers after listeners observe
        // stop. Their short receive timeout also makes shutdown deterministic.
        self.sender.take();
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

/// Registration lifetime for a cage's domain policy.
///
/// Dropping the lease waits for any in-flight nft mutation holding the same
/// registry lock, then removes only its own generation.
#[derive(Debug)]
pub struct DnsLease {
    registry: Arc<Registry>,
    cage_ip: Ipv4Addr,
    generation: u64,
}

impl DnsLease {
    /// Deterministic cage address used as this registration's lookup key.
    pub fn cage_ip(&self) -> Ipv4Addr {
        self.cage_ip
    }

    /// Alias for callers that use the generic cage-address terminology.
    pub fn address(&self) -> Ipv4Addr {
        self.cage_ip
    }

    /// Generation token for diagnostics and race tests.
    pub fn generation(&self) -> u64 {
        self.generation
    }
}

impl Drop for DnsLease {
    fn drop(&mut self) {
        let mut state = lock(&self.registry.state);
        if state
            .registrations
            .get(&self.cage_ip)
            .is_some_and(|registration| registration.generation == self.generation)
        {
            state.registrations.remove(&self.cage_ip);
        }
    }
}

#[derive(Debug, Default)]
pub(crate) struct Registry {
    state: Mutex<RegistryState>,
}

#[derive(Debug, Default)]
struct RegistryState {
    next_generation: u64,
    registrations: HashMap<Ipv4Addr, Registration>,
}

#[derive(Debug)]
struct Registration {
    _cage_name: String,
    pid: i32,
    generation: u64,
    domains: HashSet<String>,
}

#[derive(Clone, Copy, Debug)]
struct RegistrationToken {
    cage_ip: Ipv4Addr,
    pid: i32,
    generation: u64,
}

impl Registry {
    fn register(
        self: &Arc<Self>,
        cage_name: &str,
        pid: i32,
        rules: &[DomainEgressRule],
    ) -> DnsLease {
        let cage_ip = cage_ipv4(cage_name);
        let domains = rules.iter().map(|rule| rule.domain.clone()).collect();
        let mut state = lock(&self.state);
        state.next_generation = state.next_generation.wrapping_add(1).max(1);
        let generation = state.next_generation;
        state.registrations.insert(
            cage_ip,
            Registration {
                _cage_name: cage_name.to_owned(),
                pid,
                generation,
                domains,
            },
        );
        DnsLease {
            registry: Arc::clone(self),
            cage_ip,
            generation,
        }
    }

    fn authorize(&self, cage_ip: Ipv4Addr, domain: &str) -> Option<RegistrationToken> {
        let state = lock(&self.state);
        let registration = state.registrations.get(&cage_ip)?;
        registration
            .domains
            .contains(domain)
            .then_some(RegistrationToken {
                cage_ip,
                pid: registration.pid,
                generation: registration.generation,
            })
    }

    fn install_answers(
        &self,
        token: RegistrationToken,
        answers: &[(Ipv4Addr, u32)],
        update: &impl Fn(i32, Ipv4Addr, u32) -> io::Result<()>,
    ) -> io::Result<()> {
        // The lock covers generation validation and every synchronous nft
        // mutation. Lease Drop and replacement registration therefore cannot
        // race an old PID into an update.
        let state = lock(&self.state);
        let current = state.registrations.get(&token.cage_ip);
        if !current.is_some_and(|registration| {
            registration.generation == token.generation && registration.pid == token.pid
        }) {
            return Err(io::Error::new(
                ErrorKind::NotFound,
                "DNS cage registration changed before nft update",
            ));
        }
        for &(address, ttl) in answers {
            update(token.pid, address, ttl.clamp(MIN_TTL_SECS, MAX_TTL_SECS))?;
        }
        Ok(())
    }
}

/// Process one complete DNS datagram or TCP payload.
///
/// The forwarding and nft callbacks are deliberately injected at this narrow
/// seam so rootless tests can prove update-before-reply, answer rejection, and
/// lease unregister without binding privileged port 53 or invoking `nsenter`.
pub(crate) fn process_request(
    registry: &Registry,
    cage_ip: Ipv4Addr,
    request_bytes: &[u8],
    forward: impl FnOnce(&[u8]) -> io::Result<Vec<u8>>,
    update: &impl Fn(i32, Ipv4Addr, u32) -> io::Result<()>,
) -> Vec<u8> {
    let request = match parse_exact(request_bytes) {
        Ok(request) => request,
        Err(()) => {
            return error_response(
                request_bytes,
                Message::from_vec(request_bytes).ok().as_ref(),
                ResponseCode::ServFail,
            );
        }
    };
    let [question] = request.queries.as_slice() else {
        return error_response(request_bytes, Some(&request), ResponseCode::Refused);
    };
    if request.metadata.message_type != MessageType::Query
        || request.metadata.op_code != OpCode::Query
        || question.query_class() != DNSClass::IN
        || question.query_type() != RecordType::A
    {
        return error_response(request_bytes, Some(&request), ResponseCode::Refused);
    }
    let canonical = question.name().to_lowercase().to_ascii();
    let canonical = canonical.strip_suffix('.').unwrap_or(&canonical);
    let Some(token) = registry.authorize(cage_ip, canonical) else {
        return error_response(request_bytes, Some(&request), ResponseCode::Refused);
    };

    let upstream_bytes = match forward(request_bytes) {
        Ok(response) => response,
        Err(_) => return error_response(request_bytes, Some(&request), ResponseCode::ServFail),
    };
    let response = match parse_exact(&upstream_bytes) {
        Ok(response) => response,
        Err(()) => return error_response(request_bytes, Some(&request), ResponseCode::ServFail),
    };
    if response.metadata.message_type != MessageType::Response
        || response.metadata.id != request.metadata.id
        || response.queries != request.queries
    {
        return error_response(request_bytes, Some(&request), ResponseCode::ServFail);
    }

    let mut answers = Vec::new();
    for record in &response.answers {
        if let RData::A(address) = &record.data {
            let address = address.0;
            if !is_public_egress_address(address) {
                return error_response(request_bytes, Some(&request), ResponseCode::ServFail);
            }
            answers.push((address, record.ttl));
        }
    }
    if registry.install_answers(token, &answers, update).is_err() {
        return error_response(request_bytes, Some(&request), ResponseCode::ServFail);
    }
    upstream_bytes
}

fn parse_exact(payload: &[u8]) -> Result<Message, ()> {
    let mut decoder = BinDecoder::new(payload);
    let message = Message::read(&mut decoder).map_err(|_| ())?;
    decoder.is_empty().then_some(message).ok_or(())
}

fn error_response(raw: &[u8], request: Option<&Message>, code: ResponseCode) -> Vec<u8> {
    let id = request
        .map(|message| message.metadata.id)
        .or_else(|| raw.get(..2).map(|id| u16::from_be_bytes([id[0], id[1]])))
        .unwrap_or(0);
    let op_code = request
        .map(|message| message.metadata.op_code)
        .unwrap_or(OpCode::Query);
    let mut response = Message::response(id, op_code);
    if let Some(request) = request {
        response.metadata = Metadata::response_from_request(&request.metadata);
        if let [question] = request.queries.as_slice() {
            response.add_query(question.clone());
        }
    }
    response.metadata.response_code = code;
    response
        .to_vec()
        .unwrap_or_else(|_| minimal_error(id, code))
}

fn minimal_error(id: u16, code: ResponseCode) -> Vec<u8> {
    let code = u16::from(code) & 0x000f;
    let flags = 0x8000 | code;
    let mut response = Vec::with_capacity(12);
    response.extend_from_slice(&id.to_be_bytes());
    response.extend_from_slice(&flags.to_be_bytes());
    response.extend_from_slice(&[0; 8]);
    response
}

fn is_public_egress_address(address: Ipv4Addr) -> bool {
    let octets = address.octets();
    !address.is_private()
        && !address.is_link_local()
        && !address.is_loopback()
        && !address.is_multicast()
        && !address.is_unspecified()
        && octets[0] != 0
        && octets[0] < 240
        && !(octets[0] == 100 && octets[1] == 64)
}

fn nameserver_from_resolv_conf(contents: &str) -> Option<IpAddr> {
    contents.lines().find_map(|line| {
        let line = line.split('#').next().unwrap_or_default();
        let mut fields = line.split_whitespace();
        if fields.next()? != "nameserver" {
            return None;
        }
        let address = fields.next()?.parse::<IpAddr>().ok()?;
        (address != IpAddr::V4(GATEWAY)).then_some(address)
    })
}

fn nft_add(pid: i32, address: Ipv4Addr, ttl: u32) -> io::Result<()> {
    let pid = pid.to_string();
    let address = address.to_string();
    let ttl = format!("{}s", ttl.clamp(MIN_TTL_SECS, MAX_TTL_SECS));
    let output = Command::new("nsenter")
        .args([
            "-t", &pid, "-n", "--", "nft", "add", "element", "inet", "cygnus", "dns_v4", "{",
            &address, "timeout", &ttl, "}",
        ])
        .output()?;
    if output.status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "nsenter nft add element failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )))
    }
}

#[derive(Debug)]
struct Runtime {
    registry: Arc<Registry>,
    udp: Arc<UdpSocket>,
    upstream: SocketAddr,
}

#[derive(Debug)]
enum Work {
    Udp { request: Vec<u8>, peer: SocketAddr },
    Tcp { stream: TcpStream, peer: SocketAddr },
}

fn udp_listener(socket: Arc<UdpSocket>, sender: SyncSender<Work>, stop: Arc<AtomicBool>) {
    let mut buffer = [0_u8; u16::MAX as usize];
    while !stop.load(Ordering::Acquire) {
        match socket.recv_from(&mut buffer) {
            Ok((length, peer)) => {
                let work = Work::Udp {
                    request: buffer[..length].to_vec(),
                    peer,
                };
                let _ = sender.try_send(work);
            }
            Err(error) if error.kind() == ErrorKind::WouldBlock => thread::sleep(POLL_INTERVAL),
            Err(_) => thread::sleep(POLL_INTERVAL),
        }
    }
}

fn tcp_listener(listener: TcpListener, sender: SyncSender<Work>, stop: Arc<AtomicBool>) {
    while !stop.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((stream, peer)) => {
                let _ = stream.set_read_timeout(Some(CLIENT_TIMEOUT));
                let _ = stream.set_write_timeout(Some(CLIENT_TIMEOUT));
                let _ = sender.try_send(Work::Tcp { stream, peer });
            }
            Err(error) if error.kind() == ErrorKind::WouldBlock => thread::sleep(POLL_INTERVAL),
            Err(_) => thread::sleep(POLL_INTERVAL),
        }
    }
}

fn worker(receiver: Arc<Mutex<Receiver<Work>>>, runtime: Arc<Runtime>, stop: Arc<AtomicBool>) {
    while !stop.load(Ordering::Acquire) {
        let work = lock(&receiver).recv_timeout(POLL_INTERVAL);
        match work {
            Ok(Work::Udp { request, peer }) => handle_udp(&runtime, peer, &request),
            Ok(Work::Tcp { stream, peer }) => handle_tcp(&runtime, peer, stream),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
}

fn handle_udp(runtime: &Runtime, peer: SocketAddr, request: &[u8]) {
    let IpAddr::V4(cage_ip) = peer.ip() else {
        return;
    };
    let response = process_request(
        &runtime.registry,
        cage_ip,
        request,
        |raw| forward_udp(runtime.upstream, raw),
        &nft_add,
    );
    let _ = runtime.udp.send_to(&response, peer);
}

fn handle_tcp(runtime: &Runtime, peer: SocketAddr, mut stream: TcpStream) {
    let IpAddr::V4(cage_ip) = peer.ip() else {
        return;
    };
    while let Ok(Some(request)) = read_tcp_message(&mut stream) {
        let response = process_request(
            &runtime.registry,
            cage_ip,
            &request,
            |raw| forward_tcp(runtime.upstream, raw),
            &nft_add,
        );
        let Ok(frame) = tcp_frame(&response) else {
            return;
        };
        if stream.write_all(&frame).is_err() {
            return;
        }
    }
}

fn forward_udp(upstream: SocketAddr, request: &[u8]) -> io::Result<Vec<u8>> {
    let bind = match upstream {
        SocketAddr::V4(_) => "0.0.0.0:0",
        SocketAddr::V6(_) => "[::]:0",
    };
    let socket = UdpSocket::bind(bind)?;
    socket.set_read_timeout(Some(UPSTREAM_TIMEOUT))?;
    socket.set_write_timeout(Some(UPSTREAM_TIMEOUT))?;
    socket.connect(upstream)?;
    socket.send(request)?;
    let mut response = vec![0_u8; u16::MAX as usize];
    let length = socket.recv(&mut response)?;
    response.truncate(length);
    Ok(response)
}

fn forward_tcp(upstream: SocketAddr, request: &[u8]) -> io::Result<Vec<u8>> {
    let mut stream = TcpStream::connect_timeout(&upstream, UPSTREAM_TIMEOUT)?;
    stream.set_read_timeout(Some(UPSTREAM_TIMEOUT))?;
    stream.set_write_timeout(Some(UPSTREAM_TIMEOUT))?;
    stream.write_all(&tcp_frame(request)?)?;
    read_tcp_message(&mut stream)?.ok_or_else(|| {
        io::Error::new(
            ErrorKind::UnexpectedEof,
            "upstream closed before DNS response",
        )
    })
}

fn read_tcp_message(stream: &mut TcpStream) -> io::Result<Option<Vec<u8>>> {
    let mut length = [0_u8; 2];
    match stream.read(&mut length[..1]) {
        Ok(0) => return Ok(None),
        Ok(1) => {}
        Ok(_) => unreachable!("one-byte read returned more than one byte"),
        Err(error) => return Err(error),
    }
    stream.read_exact(&mut length[1..])?;
    let length = usize::from(u16::from_be_bytes(length));
    if length == 0 {
        return Err(io::Error::new(
            ErrorKind::InvalidData,
            "empty DNS TCP frame",
        ));
    }
    let mut payload = vec![0_u8; length];
    stream.read_exact(&mut payload)?;
    Ok(Some(payload))
}

fn tcp_frame(payload: &[u8]) -> io::Result<Vec<u8>> {
    let length = u16::try_from(payload.len()).map_err(|_| {
        io::Error::new(
            ErrorKind::InvalidData,
            format!("DNS TCP payload exceeds {MAX_DNS_MESSAGE} bytes"),
        )
    })?;
    let mut frame = Vec::with_capacity(payload.len() + 2);
    frame.extend_from_slice(&length.to_be_bytes());
    frame.extend_from_slice(payload);
    Ok(frame)
}

#[cfg(test)]
fn tcp_payload(frame: &[u8]) -> io::Result<&[u8]> {
    let [high, low, payload @ ..] = frame else {
        return Err(io::Error::new(
            ErrorKind::InvalidData,
            "missing DNS TCP length",
        ));
    };
    let expected = usize::from(u16::from_be_bytes([*high, *low]));
    if payload.len() != expected {
        return Err(io::Error::new(
            ErrorKind::InvalidData,
            "DNS TCP length does not match payload",
        ));
    }
    Ok(payload)
}

fn validate_domain_rule(rule: &DomainEgressRule) -> Result<(), CageError> {
    if rule.domain.is_empty() || rule.domain.len() > 253 {
        return Err(CageError::InvalidSpec(format!(
            "DNS domain {:?} must contain 1 to 253 bytes",
            rule.domain
        )));
    }
    for label in rule.domain.split('.') {
        let bytes = label.as_bytes();
        if bytes.is_empty() || bytes.len() > 63 {
            return Err(CageError::InvalidSpec(format!(
                "invalid DNS domain {:?}",
                rule.domain
            )));
        }
        if (!bytes[0].is_ascii_lowercase() && !bytes[0].is_ascii_digit())
            || (!bytes[bytes.len() - 1].is_ascii_lowercase()
                && !bytes[bytes.len() - 1].is_ascii_digit())
            || !bytes
                .iter()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
        {
            return Err(CageError::InvalidSpec(format!(
                "invalid DNS domain {:?}",
                rule.domain
            )));
        }
    }
    let mut unique = HashSet::new();
    if rule.ports.is_empty()
        || rule
            .ports
            .iter()
            .any(|port| *port == 0 || !unique.insert(*port))
    {
        return Err(CageError::InvalidSpec(format!(
            "DNS domain ports for {:?} must be nonzero and unique",
            rule.domain
        )));
    }
    Ok(())
}

fn network_error(operation: &'static str, source: io::Error) -> CageError {
    CageError::Network {
        operation: operation.into(),
        detail: source.to_string(),
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use hickory_proto::op::Query;
    use hickory_proto::rr::Name;
    use std::sync::atomic::{AtomicBool, AtomicUsize};

    fn query_bytes(id: u16, domain: &str, kind: RecordType) -> Vec<u8> {
        let mut query = Message::new(id, MessageType::Query, OpCode::Query);
        query.add_query(Query::query(Name::from_ascii(domain).unwrap(), kind));
        query.to_vec().unwrap()
    }

    fn answer_bytes(request: &[u8], address: Ipv4Addr, ttl: u32) -> Vec<u8> {
        let mut response = Message::from_vec(request).unwrap();
        response.metadata.message_type = MessageType::Response;
        response.metadata.response_code = ResponseCode::NoError;
        response.add_answer(hickory_proto::rr::Record::from_rdata(
            Name::from_ascii("registry.npmjs.org").unwrap(),
            ttl,
            RData::A(hickory_proto::rr::rdata::A(address)),
        ));
        response.to_vec().unwrap()
    }

    #[test]
    fn nameserver_selection_skips_gateway_and_invalid_lines() {
        let conf = "nameserver nope\nnameserver 100.64.0.1\nnameserver 192.0.2.53\n";
        assert_eq!(
            nameserver_from_resolv_conf(conf),
            Some(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 53)))
        );
    }

    #[test]
    fn tcp_framing_requires_exact_length() {
        let frame = tcp_frame(b"abc").unwrap();
        assert_eq!(tcp_payload(&frame).unwrap(), b"abc");
        assert!(tcp_payload(&[0, 4, b'a']).is_err());
        assert!(tcp_payload(&[0]).is_err());
    }

    #[test]
    fn update_happens_before_reply_and_ttl_is_clamped() {
        let registry = Arc::new(Registry::default());
        let rule = DomainEgressRule {
            domain: "registry.npmjs.org".into(),
            ports: vec![443],
        };
        let lease = registry.register("builder", 4242, &[rule]);
        let request = query_bytes(7, "registry.npmjs.org", RecordType::A);
        let upstream = answer_bytes(&request, Ipv4Addr::new(203, 0, 113, 7), 99_999);
        let order = Arc::new(Mutex::new(Vec::new()));
        let update_order = Arc::clone(&order);
        let update = move |pid, address, ttl| {
            update_order.lock().unwrap().push((pid, address, ttl));
            Ok(())
        };
        let response = process_request(
            &registry,
            lease.cage_ip(),
            &request,
            |raw| {
                assert_eq!(raw, request);
                Ok(upstream.clone())
            },
            &update,
        );
        assert_eq!(response, upstream);
        assert_eq!(
            order.lock().unwrap().as_slice(),
            &[(4242, Ipv4Addr::new(203, 0, 113, 7), MAX_TTL_SECS)]
        );
    }

    #[test]
    fn forbidden_answers_become_servfail_and_unregistered_becomes_refused() {
        let registry = Arc::new(Registry::default());
        let rule = DomainEgressRule {
            domain: "registry.npmjs.org".into(),
            ports: vec![443],
        };
        let lease = registry.register("builder", 4242, &[rule]);
        let request = query_bytes(8, "registry.npmjs.org", RecordType::A);
        let updates = AtomicUsize::new(0);
        for address in [
            Ipv4Addr::new(10, 0, 0, 1),
            Ipv4Addr::new(169, 254, 1, 1),
            Ipv4Addr::LOCALHOST,
            Ipv4Addr::new(224, 0, 0, 1),
            Ipv4Addr::new(100, 64, 1, 10),
        ] {
            let rejected = answer_bytes(&request, address, 30);
            let response = process_request(
                &registry,
                lease.cage_ip(),
                &request,
                |_| Ok(rejected),
                &|_, _, _| {
                    updates.fetch_add(1, Ordering::Relaxed);
                    Ok(())
                },
            );
            assert_eq!(
                Message::from_vec(&response).unwrap().metadata.response_code,
                ResponseCode::ServFail
            );
        }
        assert_eq!(updates.load(Ordering::Relaxed), 0);
        drop(lease);
        let response = process_request(
            &registry,
            cage_ipv4("builder"),
            &request,
            |_| panic!("unregistered request must not forward"),
            &|_, _, _| Ok(()),
        );
        assert_eq!(
            Message::from_vec(&response).unwrap().metadata.response_code,
            ResponseCode::Refused
        );
    }

    #[test]
    fn old_lease_drop_cannot_unregister_new_generation() {
        let registry = Arc::new(Registry::default());
        let first = registry.register(
            "builder",
            4242,
            &[DomainEgressRule {
                domain: "registry.npmjs.org".into(),
                ports: vec![443],
            }],
        );
        let second = registry.register(
            "builder",
            4343,
            &[DomainEgressRule {
                domain: "registry.npmjs.org".into(),
                ports: vec![443],
            }],
        );
        let ip = second.cage_ip();
        drop(first);
        let token = registry.authorize(ip, "registry.npmjs.org").unwrap();
        assert_eq!(token.pid, 4343);
        drop(second);
    }

    #[test]
    fn non_a_query_is_refused_without_forwarding() {
        let registry = Arc::new(Registry::default());
        let lease = registry.register(
            "builder",
            4242,
            &[DomainEgressRule {
                domain: "registry.npmjs.org".into(),
                ports: vec![443],
            }],
        );
        let called = AtomicBool::new(false);
        let request = query_bytes(9, "registry.npmjs.org", RecordType::AAAA);
        let response = process_request(
            &registry,
            lease.cage_ip(),
            &request,
            |_| {
                called.store(true, Ordering::Relaxed);
                Ok(Vec::new())
            },
            &|_, _, _| Ok(()),
        );
        assert!(!called.load(Ordering::Relaxed));
        assert_eq!(
            Message::from_vec(&response).unwrap().metadata.response_code,
            ResponseCode::Refused
        );
    }

    #[test]
    fn registrations_reject_invalid_rules() {
        let registry = Arc::new(Registry::default());
        let forwarder = DnsForwarder {
            registry,
            stop: Arc::new(AtomicBool::new(true)),
            sender: None,
            threads: Vec::new(),
        };
        assert!(
            forwarder
                .register(
                    "builder",
                    1,
                    &[DomainEgressRule {
                        domain: "Registry.npmjs.org".into(),
                        ports: vec![443]
                    }]
                )
                .is_err()
        );
        assert!(
            forwarder
                .register(
                    "builder",
                    1,
                    &[DomainEgressRule {
                        domain: "registry.npmjs.org".into(),
                        ports: vec![443, 443]
                    }]
                )
                .is_err()
        );
    }

    #[test]
    fn response_question_mismatch_servfails() {
        let registry = Arc::new(Registry::default());
        let lease = registry.register(
            "builder",
            4242,
            &[DomainEgressRule {
                domain: "registry.npmjs.org".into(),
                ports: vec![443],
            }],
        );
        let request = query_bytes(10, "registry.npmjs.org", RecordType::A);
        let mut mismatched =
            Message::from_vec(&query_bytes(10, "other.example", RecordType::A)).unwrap();
        mismatched.metadata.message_type = MessageType::Response;
        let mismatched = mismatched.to_vec().unwrap();
        let response = process_request(
            &registry,
            lease.cage_ip(),
            &request,
            |_| Ok(mismatched),
            &|_, _, _| Ok(()),
        );
        assert_eq!(
            Message::from_vec(&response).unwrap().metadata.response_code,
            ResponseCode::ServFail
        );
    }

    #[test]
    fn response_transaction_mismatch_servfails_without_update() {
        let registry = Arc::new(Registry::default());
        let lease = registry.register(
            "builder",
            4242,
            &[DomainEgressRule {
                domain: "registry.npmjs.org".into(),
                ports: vec![443],
            }],
        );
        let request = query_bytes(13, "registry.npmjs.org", RecordType::A);
        let mut mismatched =
            Message::from_vec(&answer_bytes(&request, Ipv4Addr::new(203, 0, 113, 9), 30)).unwrap();
        mismatched.metadata.id = 14;
        let response = process_request(
            &registry,
            lease.cage_ip(),
            &request,
            |_| Ok(mismatched.to_vec().unwrap()),
            &|_, _, _| panic!("mismatched transaction must not update nft"),
        );
        assert_eq!(
            Message::from_vec(&response).unwrap().metadata.response_code,
            ResponseCode::ServFail
        );
    }

    #[test]
    fn update_error_servfails() {
        let registry = Arc::new(Registry::default());
        let lease = registry.register(
            "builder",
            4242,
            &[DomainEgressRule {
                domain: "registry.npmjs.org".into(),
                ports: vec![443],
            }],
        );
        let request = query_bytes(11, "registry.npmjs.org", RecordType::A);
        let answer = answer_bytes(&request, Ipv4Addr::new(203, 0, 113, 8), 30);
        let response = process_request(
            &registry,
            lease.cage_ip(),
            &request,
            |_| Ok(answer),
            &|_, _, _| Err(io::Error::other("nft unavailable")),
        );
        assert_eq!(
            Message::from_vec(&response).unwrap().metadata.response_code,
            ResponseCode::ServFail
        );
    }

    #[test]
    fn no_answer_response_is_forwarded_without_nft_mutation() {
        let registry = Arc::new(Registry::default());
        let lease = registry.register(
            "builder",
            4242,
            &[DomainEgressRule {
                domain: "registry.npmjs.org".into(),
                ports: vec![443],
            }],
        );
        let request = query_bytes(12, "registry.npmjs.org", RecordType::A);
        let mut upstream_message = Message::from_vec(&request).unwrap();
        upstream_message.metadata.message_type = MessageType::Response;
        let upstream = upstream_message.to_vec().unwrap();
        let updates = AtomicUsize::new(0);
        let response = process_request(
            &registry,
            lease.cage_ip(),
            &request,
            |_| Ok(upstream.clone()),
            &|_, _, _| {
                updates.fetch_add(1, Ordering::Relaxed);
                Ok(())
            },
        );
        assert_eq!(response, upstream);
        assert_eq!(updates.load(Ordering::Relaxed), 0);
    }
}
