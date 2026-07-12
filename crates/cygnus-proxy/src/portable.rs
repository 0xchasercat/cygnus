//! Portable threaded relay backend.
//!
//! Non-Linux hosts run the same public proxy API with a thread-per-direction
//! copy loop instead of io_uring and `splice()`. Semantics match the Linux
//! backend: bidirectional relay, half-close propagation, upstream connect
//! failures drop the client, and shutdown drains active streams. Raw
//! throughput is the io_uring backend's job, not this one's.

use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};

use crate::{Config, Result};

const COPY_BUFFER: usize = 64 * 1024;

/// A bound proxy server using the portable threaded backend.
pub struct Proxy {
    config: Config,
    listener: TcpListener,
    local_addr: SocketAddr,
    shutdown: Arc<AtomicBool>,
}

impl Proxy {
    /// Binds the TCP listener.
    pub fn bind(config: Config) -> Result<Self> {
        let listener = TcpListener::bind(config.listen_addr)?;
        let local_addr = listener.local_addr()?;
        Ok(Self {
            config,
            listener,
            local_addr,
            shutdown: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Returns the address on which the proxy is listening.
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Returns a handle that can request graceful shutdown of the accept loop.
    pub fn handle(&self) -> ProxyHandle {
        ProxyHandle {
            listen_addr: self.local_addr,
            shutdown: Arc::clone(&self.shutdown),
        }
    }

    /// Runs the accept loop until shutdown is requested, then drains active
    /// byte streams before returning.
    pub fn run(self) -> Result<()> {
        let mut workers: Vec<JoinHandle<()>> = Vec::new();
        loop {
            let (client, _) = match self.listener.accept() {
                Ok(accepted) => accepted,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error.into()),
            };
            if self.shutdown.load(Ordering::Acquire) {
                drop(client);
                break;
            }
            client.set_nodelay(true)?;
            let Ok(upstream) = UnixStream::connect(&self.config.upstream_path) else {
                continue;
            };
            workers.push(thread::spawn(move || relay(client, upstream)));
        }

        for worker in workers {
            let _ = worker.join();
        }
        Ok(())
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

fn relay(client: TcpStream, upstream: UnixStream) {
    let Ok(client_reader) = client.try_clone() else {
        return;
    };
    let Ok(upstream_reader) = upstream.try_clone() else {
        return;
    };

    let forward = thread::spawn(move || {
        relay_direction(client_reader, upstream, UnixStream::shutdown);
    });
    relay_direction(upstream_reader, client, TcpStream::shutdown);
    let _ = forward.join();
}

fn relay_direction<R, W>(mut source: R, mut sink: W, shutdown: fn(&W, Shutdown) -> io::Result<()>)
where
    R: Read,
    W: Write,
{
    let mut buffer = vec![0_u8; COPY_BUFFER];
    loop {
        let count = match source.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        if sink.write_all(&buffer[..count]).is_err() {
            break;
        }
    }
    let _ = shutdown(&sink, Shutdown::Write);
}
