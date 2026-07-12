//! The data path: proxying between client connections and per-app
//! Unix domain sockets.
//!
//! Headers are parsed (routing, logging, limits); body bytes move with
//! `splice()` through an io_uring event loop so payloads stay kernel-side.
//! See `docs/spec.md` §6.
