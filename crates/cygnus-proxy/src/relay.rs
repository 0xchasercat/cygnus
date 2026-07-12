use std::collections::{HashMap, VecDeque};
use std::io;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use io_uring::{IoUring, Probe, opcode, squeue, types};

use crate::{Config, ProxyError, Result, UnavailableReason};

const RING_ENTRIES: u32 = 1024;
const SPLICE_CHUNK: u32 = 64 * 1024;
const ACCEPT_TOKEN: u64 = 0;
const TOKEN_KIND_BITS: u32 = 3;
const TOKEN_KIND_MASK: u64 = (1 << TOKEN_KIND_BITS) - 1;

#[derive(Clone, Copy, Debug)]
#[repr(u64)]
enum OperationKind {
    ClientToUpstreamRead = 1,
    ClientToUpstreamWrite = 2,
    UpstreamToClientRead = 3,
    UpstreamToClientWrite = 4,
}

impl OperationKind {
    fn from_token(token: u64) -> io::Result<Self> {
        match token & TOKEN_KIND_MASK {
            1 => Ok(Self::ClientToUpstreamRead),
            2 => Ok(Self::ClientToUpstreamWrite),
            3 => Ok(Self::UpstreamToClientRead),
            4 => Ok(Self::UpstreamToClientWrite),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "completion carried an unknown operation token",
            )),
        }
    }

    fn token(self, connection_id: u64) -> u64 {
        (connection_id << TOKEN_KIND_BITS) | self as u64
    }
}

/// A bound io_uring proxy server.
pub struct Proxy {
    config: Config,
    listener: TcpListener,
    local_addr: SocketAddr,
    ring: IoUring,
    submissions: VecDeque<squeue::Entry>,
    connections: HashMap<u64, Connection>,
    next_connection_id: u64,
    accept_pending: bool,
    shutdown: Arc<AtomicBool>,
}

impl Proxy {
    /// Binds the TCP listener and initializes the io_uring data path.
    pub fn bind(config: Config) -> Result<Self> {
        let ring = new_ring()?;
        let listener = TcpListener::bind(config.listen_addr)?;
        let local_addr = listener.local_addr()?;
        let shutdown = Arc::new(AtomicBool::new(false));

        let mut proxy = Self {
            config,
            listener,
            local_addr,
            ring,
            submissions: VecDeque::new(),
            connections: HashMap::new(),
            next_connection_id: 1,
            accept_pending: false,
            shutdown,
        };
        proxy.queue_accept();
        Ok(proxy)
    }

    /// Returns the address on which the proxy is listening.
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Returns a handle that can request graceful shutdown of the event loop.
    pub fn handle(&self) -> ProxyHandle {
        ProxyHandle {
            listen_addr: self.local_addr,
            shutdown: Arc::clone(&self.shutdown),
        }
    }

    /// Runs the single-threaded event loop until shutdown is requested and all
    /// active byte streams have drained.
    pub fn run(mut self) -> Result<()> {
        loop {
            self.flush_submissions();
            if self.should_stop() {
                return Ok(());
            }

            self.ring.submit_and_wait(1)?;
            let completions = {
                let completion = self.ring.completion();
                completion
                    .map(|entry| (entry.user_data(), entry.result()))
                    .collect::<Vec<_>>()
            };

            for (token, result) in completions {
                self.handle_completion(token, result)?;
            }
        }
    }

    fn should_stop(&self) -> bool {
        self.shutdown.load(Ordering::Acquire)
            && !self.accept_pending
            && self.connections.is_empty()
            && self.submissions.is_empty()
    }

    fn flush_submissions(&mut self) {
        let mut submission = self.ring.submission();
        while let Some(entry) = self.submissions.pop_front() {
            if unsafe { submission.push(&entry) }.is_err() {
                self.submissions.push_front(entry);
                break;
            }
        }
    }

    fn queue_accept(&mut self) {
        if self.accept_pending || self.shutdown.load(Ordering::Acquire) {
            return;
        }

        let entry = opcode::Accept::new(
            types::Fd(self.listener.as_raw_fd()),
            ptr::null_mut(),
            ptr::null_mut(),
        )
        .flags(libc::SOCK_CLOEXEC)
        .build()
        .user_data(ACCEPT_TOKEN);
        self.submissions.push_back(entry);
        self.accept_pending = true;
    }

    fn handle_completion(&mut self, token: u64, result: i32) -> Result<()> {
        if token == ACCEPT_TOKEN {
            return self.handle_accept(result);
        }

        let connection_id = token >> TOKEN_KIND_BITS;
        let kind = OperationKind::from_token(token)?;
        let action = {
            let connection = self.connections.get_mut(&connection_id).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "completion referenced an unknown connection",
                )
            })?;
            connection.complete(kind, result)
        };

        self.apply_action(connection_id, kind, action);
        if self
            .connections
            .get(&connection_id)
            .is_some_and(Connection::is_done)
        {
            self.connections.remove(&connection_id);
        }
        Ok(())
    }

    fn handle_accept(&mut self, result: i32) -> Result<()> {
        self.accept_pending = false;
        if result < 0 {
            let error = io::Error::from_raw_os_error(-result);
            match error.raw_os_error() {
                Some(libc::EINTR) | Some(libc::ECONNABORTED) => {
                    self.queue_accept();
                    return Ok(());
                }
                _ => return Err(error.into()),
            }
        }

        let client = unsafe { TcpStream::from_raw_fd(result) };
        if self.shutdown.load(Ordering::Acquire) {
            return Ok(());
        }

        client.set_nodelay(true)?;
        if let Ok(upstream) = UnixStream::connect(&self.config.upstream_path) {
            self.add_connection(client, upstream)?;
        }
        self.queue_accept();
        Ok(())
    }

    fn add_connection(&mut self, client: TcpStream, upstream: UnixStream) -> Result<()> {
        let connection_id = self.allocate_connection_id()?;
        let client_fd = client.as_raw_fd();
        let upstream_fd = upstream.as_raw_fd();
        let connection = Connection::new(client, upstream)?;
        let client_to_upstream_pipe = connection.client_to_upstream.pipe.write_fd();
        let upstream_to_client_pipe = connection.upstream_to_client.pipe.write_fd();
        self.connections.insert(connection_id, connection);

        self.submissions.push_back(splice_read_entry(
            client_fd,
            client_to_upstream_pipe,
            OperationKind::ClientToUpstreamRead.token(connection_id),
        ));
        self.submissions.push_back(splice_read_entry(
            upstream_fd,
            upstream_to_client_pipe,
            OperationKind::UpstreamToClientRead.token(connection_id),
        ));
        Ok(())
    }

    fn allocate_connection_id(&mut self) -> Result<u64> {
        let start = self.next_connection_id;
        loop {
            let candidate = self.next_connection_id;
            self.next_connection_id = self.next_connection_id.wrapping_add(1).max(1);
            if !self.connections.contains_key(&candidate) {
                return Ok(candidate);
            }
            if self.next_connection_id == start {
                return Err(io::Error::other("connection identifier space exhausted").into());
            }
        }
    }

    fn apply_action(
        &mut self,
        connection_id: u64,
        completed_kind: OperationKind,
        action: DirectionAction,
    ) {
        match action {
            DirectionAction::Read {
                source,
                pipe_write,
            } => {
                let kind = match completed_kind {
                    OperationKind::ClientToUpstreamRead
                    | OperationKind::ClientToUpstreamWrite => {
                        OperationKind::ClientToUpstreamRead
                    }
                    OperationKind::UpstreamToClientRead
                    | OperationKind::UpstreamToClientWrite => {
                        OperationKind::UpstreamToClientRead
                    }
                };
                self.submissions.push_back(splice_read_entry(
                    source,
                    pipe_write,
                    kind.token(connection_id),
                ));
            }
            DirectionAction::Write {
                pipe_read,
                sink,
                remaining,
            } => {
                let kind = match completed_kind {
                    OperationKind::ClientToUpstreamRead
                    | OperationKind::ClientToUpstreamWrite => {
                        OperationKind::ClientToUpstreamWrite
                    }
                    OperationKind::UpstreamToClientRead
                    | OperationKind::UpstreamToClientWrite => {
                        OperationKind::UpstreamToClientWrite
                    }
                };
                self.submissions.push_back(splice_write_entry(
                    pipe_read,
                    sink,
                    remaining,
                    kind.token(connection_id),
                ));
            }
            DirectionAction::ShutdownWrite { sink } => shutdown_write(sink),
        }
    }
}

/// Handle used to wake and gracefully stop a running proxy.
#[derive(Clone, Debug)]
pub struct ProxyHandle {
    listen_addr: SocketAddr,
    shutdown: Arc<AtomicBool>,
}

impl ProxyHandle {
    /// Requests shutdown. Existing streams are allowed to drain.
    pub fn shutdown(&self) -> io::Result<()> {
        if self.shutdown.swap(true, Ordering::AcqRel) {
            return Ok(());
        }

        let wake = TcpStream::connect(self.listen_addr)?;
        drop(wake);
        Ok(())
    }
}

pub(crate) fn new_ring() -> Result<IoUring> {
    let ring = IoUring::new(RING_ENTRIES).map_err(map_setup_error)?;
    let mut probe = Probe::new();
    ring.submitter()
        .register_probe(&mut probe)
        .map_err(map_setup_error)?;

    for (opcode, name) in [
        (opcode::Accept::CODE, "accept"),
        (opcode::Splice::CODE, "splice"),
    ] {
        if !probe.is_supported(opcode) {
            return Err(ProxyError::IoUringUnavailable {
                reason: UnavailableReason::MissingOpcode(name),
                source: None,
            });
        }
    }

    Ok(ring)
}

fn map_setup_error(error: io::Error) -> ProxyError {
    let reason = match error.raw_os_error() {
        Some(libc::ENOSYS) => Some(UnavailableReason::NotImplemented),
        Some(libc::EPERM) | Some(libc::EACCES) => Some(UnavailableReason::PermissionDenied),
        _ => None,
    };

    match reason {
        Some(reason) => ProxyError::IoUringUnavailable {
            reason,
            source: Some(error),
        },
        None => ProxyError::Io(error),
    }
}

struct Connection {
    _client: TcpStream,
    _upstream: UnixStream,
    client_to_upstream: Direction,
    upstream_to_client: Direction,
}

impl Connection {
    fn new(client: TcpStream, upstream: UnixStream) -> io::Result<Self> {
        let client_fd = client.as_raw_fd();
        let upstream_fd = upstream.as_raw_fd();
        Ok(Self {
            _client: client,
            _upstream: upstream,
            client_to_upstream: Direction::new(client_fd, upstream_fd)?,
            upstream_to_client: Direction::new(upstream_fd, client_fd)?,
        })
    }

    fn complete(&mut self, kind: OperationKind, result: i32) -> DirectionAction {
        match kind {
            OperationKind::ClientToUpstreamRead => {
                self.client_to_upstream.complete_read(result)
            }
            OperationKind::ClientToUpstreamWrite => {
                self.client_to_upstream.complete_write(result)
            }
            OperationKind::UpstreamToClientRead => {
                self.upstream_to_client.complete_read(result)
            }
            OperationKind::UpstreamToClientWrite => {
                self.upstream_to_client.complete_write(result)
            }
        }
    }

    fn is_done(&self) -> bool {
        self.client_to_upstream.is_done() && self.upstream_to_client.is_done()
    }
}

struct Direction {
    source: RawFd,
    sink: RawFd,
    pipe: Pipe,
    state: DirectionState,
}

impl Direction {
    fn new(source: RawFd, sink: RawFd) -> io::Result<Self> {
        Ok(Self {
            source,
            sink,
            pipe: Pipe::new()?,
            state: DirectionState::Reading,
        })
    }

    fn complete_read(&mut self, result: i32) -> DirectionAction {
        if !matches!(self.state, DirectionState::Reading) {
            self.state = DirectionState::Done;
            return DirectionAction::ShutdownWrite { sink: self.sink };
        }

        match result {
            result if result > 0 => {
                let remaining = result as u32;
                self.state = DirectionState::Writing { remaining };
                DirectionAction::Write {
                    pipe_read: self.pipe.read_fd(),
                    sink: self.sink,
                    remaining,
                }
            }
            0 => {
                self.state = DirectionState::Done;
                DirectionAction::ShutdownWrite { sink: self.sink }
            }
            result if is_retryable(result) => DirectionAction::Read {
                source: self.source,
                pipe_write: self.pipe.write_fd(),
            },
            _ => {
                self.state = DirectionState::Done;
                DirectionAction::ShutdownWrite { sink: self.sink }
            }
        }
    }

    fn complete_write(&mut self, result: i32) -> DirectionAction {
        let DirectionState::Writing { remaining } = self.state else {
            self.state = DirectionState::Done;
            return DirectionAction::ShutdownWrite { sink: self.sink };
        };

        match result {
            result if result > 0 && (result as u32) < remaining => {
                let remaining = remaining - result as u32;
                self.state = DirectionState::Writing { remaining };
                DirectionAction::Write {
                    pipe_read: self.pipe.read_fd(),
                    sink: self.sink,
                    remaining,
                }
            }
            result if result > 0 => {
                self.state = DirectionState::Reading;
                DirectionAction::Read {
                    source: self.source,
                    pipe_write: self.pipe.write_fd(),
                }
            }
            result if is_retryable(result) => DirectionAction::Write {
                pipe_read: self.pipe.read_fd(),
                sink: self.sink,
                remaining,
            },
            _ => {
                self.state = DirectionState::Done;
                DirectionAction::ShutdownWrite { sink: self.sink }
            }
        }
    }

    fn is_done(&self) -> bool {
        matches!(self.state, DirectionState::Done)
    }
}

#[derive(Clone, Copy)]
enum DirectionState {
    Reading,
    Writing { remaining: u32 },
    Done,
}

enum DirectionAction {
    Read {
        source: RawFd,
        pipe_write: RawFd,
    },
    Write {
        pipe_read: RawFd,
        sink: RawFd,
        remaining: u32,
    },
    ShutdownWrite {
        sink: RawFd,
    },
}

struct Pipe {
    read: OwnedFd,
    write: OwnedFd,
}

impl Pipe {
    fn new() -> io::Result<Self> {
        let mut descriptors = [-1; 2];
        let result = unsafe { libc::pipe2(descriptors.as_mut_ptr(), libc::O_CLOEXEC) };
        if result < 0 {
            return Err(io::Error::last_os_error());
        }

        Ok(Self {
            read: unsafe { OwnedFd::from_raw_fd(descriptors[0]) },
            write: unsafe { OwnedFd::from_raw_fd(descriptors[1]) },
        })
    }

    fn read_fd(&self) -> RawFd {
        self.read.as_raw_fd()
    }

    fn write_fd(&self) -> RawFd {
        self.write.as_raw_fd()
    }
}

fn splice_read_entry(source: RawFd, pipe_write: RawFd, token: u64) -> squeue::Entry {
    opcode::Splice::new(
        types::Fd(source),
        -1,
        types::Fd(pipe_write),
        -1,
        SPLICE_CHUNK,
    )
    .flags(libc::SPLICE_F_MOVE)
    .build()
    .user_data(token)
}

fn splice_write_entry(
    pipe_read: RawFd,
    sink: RawFd,
    remaining: u32,
    token: u64,
) -> squeue::Entry {
    opcode::Splice::new(
        types::Fd(pipe_read),
        -1,
        types::Fd(sink),
        -1,
        remaining,
    )
    .flags(libc::SPLICE_F_MOVE)
    .build()
    .user_data(token)
}

fn is_retryable(result: i32) -> bool {
    result == -libc::EINTR || result == -libc::EAGAIN
}

fn shutdown_write(descriptor: RawFd) {
    let _ = unsafe { libc::shutdown(descriptor, libc::SHUT_WR) };
}
