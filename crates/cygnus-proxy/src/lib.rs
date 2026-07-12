//! Kernel-side byte relay between client TCP connections and per-app Unix
//! domain sockets.
//!
//! This crate deliberately does not parse HTTP or terminate TLS. Those layers
//! sit above the mechanism-agnostic relay API added here.

mod relay;

use std::error::Error as StdError;
use std::fmt;
use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;

pub use relay::{Proxy, ProxyHandle};

/// Configuration for a proxy listener.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Config {
    /// TCP address on which client connections are accepted.
    pub listen_addr: SocketAddr,
    /// Unix domain socket to which each client connection is relayed.
    pub upstream_path: PathBuf,
}

impl Config {
    /// Creates a proxy configuration.
    pub fn new(listen_addr: SocketAddr, upstream_path: impl Into<PathBuf>) -> Self {
        Self {
            listen_addr,
            upstream_path: upstream_path.into(),
        }
    }
}

/// Why the io_uring data path is unavailable on this host.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UnavailableReason {
    /// The running kernel does not implement io_uring.
    NotImplemented,
    /// A security policy denied io_uring setup.
    PermissionDenied,
    /// The running kernel does not support an opcode required by the relay.
    MissingOpcode(&'static str),
}

/// Errors returned by the proxy data path.
#[derive(Debug)]
pub enum ProxyError {
    /// io_uring cannot be used in this environment.
    IoUringUnavailable {
        /// The detected reason.
        reason: UnavailableReason,
        /// The setup error, when the kernel returned one.
        source: Option<io::Error>,
    },
    /// An operating-system I/O operation failed.
    Io(io::Error),
}

impl ProxyError {
    /// Returns whether the operation can be skipped because io_uring is not
    /// available in the current environment.
    pub fn is_io_uring_unavailable(&self) -> bool {
        matches!(self, Self::IoUringUnavailable { .. })
    }

    /// Returns the detected io_uring availability failure, if any.
    pub fn unavailable_reason(&self) -> Option<UnavailableReason> {
        match self {
            Self::IoUringUnavailable { reason, .. } => Some(*reason),
            Self::Io(_) => None,
        }
    }
}

impl fmt::Display for ProxyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::IoUringUnavailable { reason, .. } => match reason {
                UnavailableReason::NotImplemented => {
                    write!(formatter, "io_uring is not implemented by this kernel")
                }
                UnavailableReason::PermissionDenied => {
                    write!(formatter, "io_uring setup was denied by the host security policy")
                }
                UnavailableReason::MissingOpcode(opcode) => {
                    write!(formatter, "io_uring opcode {opcode} is not supported")
                }
            },
            Self::Io(error) => error.fmt(formatter),
        }
    }
}

impl StdError for ProxyError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::IoUringUnavailable { source, .. } => {
                source.as_ref().map(|error| error as &dyn StdError)
            }
            Self::Io(error) => Some(error),
        }
    }
}

impl From<io::Error> for ProxyError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// Result type used by the proxy data path.
pub type Result<T> = std::result::Result<T, ProxyError>;
