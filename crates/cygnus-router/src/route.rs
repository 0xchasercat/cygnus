//! The route table: Host/authority to a per-app upstream, behind a lock-free
//! `ArcSwap` so the hot path never blocks and a deploy swaps the whole table
//! atomically (spec §6).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use arc_swap::ArcSwap;

/// Where a matched request is sent: the app that owns it and the Unix socket
/// its cage listens on.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Route {
    pub app: String,
    pub upstream: PathBuf,
}

/// An immutable set of host patterns mapped to routes. Built off the control
/// plane's state and installed into a [`Router`] atomically.
///
/// Exact hosts win over wildcards. A `*.suffix` pattern matches exactly one
/// label in front of the suffix (DNS-style), so `*.apps.example.com` matches
/// `blog.apps.example.com` but neither `apps.example.com` nor
/// `a.b.apps.example.com`.
#[derive(Debug, Default)]
pub struct RouteTable {
    exact: HashMap<String, Arc<Route>>,
    wildcards: Vec<(String, Arc<Route>)>,
}

impl RouteTable {
    /// An empty table.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a route for a host pattern. A leading `*.` marks a single-label
    /// wildcard; anything else is an exact host. Patterns are normalized, and a
    /// later insert for the same pattern replaces the earlier one.
    pub fn insert(&mut self, pattern: &str, route: Route) {
        let route = Arc::new(route);
        if let Some(suffix) = pattern.strip_prefix("*.") {
            let dotted = format!(".{}", normalize_host(suffix));
            if let Some(slot) = self.wildcards.iter_mut().find(|entry| entry.0 == dotted) {
                slot.1 = route;
            } else {
                self.wildcards.push((dotted, route));
            }
        } else {
            self.exact.insert(normalize_host(pattern), route);
        }
    }

    /// Resolve a request host (as received, possibly with a port) to a route.
    pub fn resolve(&self, host: &str) -> Option<Arc<Route>> {
        let host = normalize_host(host);
        if let Some(route) = self.exact.get(&host) {
            return Some(Arc::clone(route));
        }
        for (suffix, route) in &self.wildcards {
            if let Some(label) = host.strip_suffix(suffix.as_str())
                && !label.is_empty()
                && !label.contains('.')
            {
                return Some(Arc::clone(route));
            }
        }
        None
    }
}

/// The live routing table: lock-free reads, atomic swap on deploy.
pub struct Router {
    table: ArcSwap<RouteTable>,
}

impl Router {
    /// Wrap an initial table.
    pub fn new(table: RouteTable) -> Self {
        Self {
            table: ArcSwap::from_pointee(table),
        }
    }

    /// Resolve a host to its route without blocking writers or readers.
    pub fn resolve(&self, host: &str) -> Option<Arc<Route>> {
        self.table.load().resolve(host)
    }

    /// Atomically replace the routing table (a deploy or config reload).
    pub fn install(&self, table: RouteTable) {
        self.table.store(Arc::new(table));
    }
}

/// Normalize a routing host: trim, drop a trailing dot, strip a numeric port,
/// and lowercase. Leaves IPv6 literals (which routing rarely uses) untouched
/// apart from case.
pub fn normalize_host(host: &str) -> String {
    let host = host.trim().trim_end_matches('.');
    let without_port = match host.rsplit_once(':') {
        Some((head, port))
            if !head.is_empty()
                && !head.contains(':')
                && !port.is_empty()
                && port.bytes().all(|b| b.is_ascii_digit()) =>
        {
            head
        }
        _ => host,
    };
    without_port.to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route(app: &str) -> Route {
        Route {
            app: app.to_owned(),
            upstream: PathBuf::from(format!("/run/cygnus/{app}.sock")),
        }
    }

    fn table() -> RouteTable {
        let mut table = RouteTable::new();
        table.insert("api.example.com", route("api"));
        table.insert("*.apps.example.com", route("preview"));
        table
    }

    #[test]
    fn exact_hosts_resolve() {
        let table = table();
        assert_eq!(table.resolve("api.example.com").unwrap().app, "api");
        assert!(table.resolve("nope.example.com").is_none());
    }

    #[test]
    fn ports_and_case_and_trailing_dot_are_normalized() {
        let table = table();
        assert_eq!(table.resolve("API.example.com:8443").unwrap().app, "api");
        assert_eq!(table.resolve("api.example.com.").unwrap().app, "api");
    }

    #[test]
    fn wildcards_match_one_label_only() {
        let table = table();
        assert_eq!(table.resolve("blog.apps.example.com").unwrap().app, "preview");
        // The apex and deeper names do not match a single-label wildcard.
        assert!(table.resolve("apps.example.com").is_none());
        assert!(table.resolve("a.b.apps.example.com").is_none());
    }

    #[test]
    fn exact_wins_over_wildcard() {
        let mut table = RouteTable::new();
        table.insert("*.apps.example.com", route("wild"));
        table.insert("special.apps.example.com", route("exact"));
        assert_eq!(
            table.resolve("special.apps.example.com").unwrap().app,
            "exact"
        );
        assert_eq!(table.resolve("other.apps.example.com").unwrap().app, "wild");
    }

    #[test]
    fn install_swaps_the_table_atomically() {
        let router = Router::new(table());
        assert_eq!(router.resolve("api.example.com").unwrap().app, "api");

        let mut next = RouteTable::new();
        next.insert("api.example.com", route("api-v2"));
        router.install(next);
        assert_eq!(router.resolve("api.example.com").unwrap().app, "api-v2");
        // The old wildcard is gone after the swap.
        assert!(router.resolve("blog.apps.example.com").is_none());
    }

    #[test]
    fn reinserting_a_pattern_replaces_it() {
        let mut table = RouteTable::new();
        table.insert("api.example.com", route("old"));
        table.insert("api.example.com", route("new"));
        assert_eq!(table.resolve("api.example.com").unwrap().app, "new");

        table.insert("*.apps.example.com", route("w1"));
        table.insert("*.apps.example.com", route("w2"));
        assert_eq!(table.resolve("x.apps.example.com").unwrap().app, "w2");
    }
}
