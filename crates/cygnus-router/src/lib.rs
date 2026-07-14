//! The ingress routing plane for the Cygnus daemon.
//!
//! Requests are routed by Host/authority to a per-app upstream through a
//! lock-free `ArcSwap` table (spec §6): reads never block, and a deploy swaps
//! the whole table atomically. This crate is the routing model plus the
//! HTTP/1.1 request-head parsing that drives it. The io_uring accept loop, TLS
//! termination, and the splice handoff to the cage's Unix socket land in a
//! following slice, composing this with `cygnus-proxy` and `cygnus-supervisor`.

mod http;
mod route;

pub use http::{HeadParse, MAX_HEAD_LEN, RequestHead, parse_request_head};
pub use route::{Route, RouteTable, Router, normalize_host};
