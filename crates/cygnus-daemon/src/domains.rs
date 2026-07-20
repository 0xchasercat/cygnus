use std::collections::BTreeSet;
use std::io;
use std::net::{Ipv4Addr, ToSocketAddrs};
use std::time::Duration;

/// Injectable IPv4 resolver used by domain DNS prechecks.
pub trait DnsResolver: Send + Sync {
    fn resolve_ipv4(&self, host: &str) -> io::Result<Vec<Ipv4Addr>>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct StdDnsResolver;

impl DnsResolver for StdDnsResolver {
    fn resolve_ipv4(&self, host: &str) -> io::Result<Vec<Ipv4Addr>> {
        let addresses = (host, 0).to_socket_addrs()?;
        Ok(addresses
            .filter_map(|address| match address.ip() {
                std::net::IpAddr::V4(ip) => Some(ip),
                std::net::IpAddr::V6(_) => None,
            })
            .collect())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DnsPrecheck {
    pub expected_ip: Option<Ipv4Addr>,
    pub resolves_to: Vec<Ipv4Addr>,
    pub ok: bool,
}

pub fn dns_precheck(
    resolver: &dyn DnsResolver,
    host: &str,
    expected_ip: Option<Ipv4Addr>,
) -> DnsPrecheck {
    // Local-only hosts never need public DNS. Treat loopback and *.localhost as
    // already pointed at this node so native domains don't stay "pending".
    if is_local_host(host) {
        let loopback = Ipv4Addr::new(127, 0, 0, 1);
        return DnsPrecheck {
            expected_ip: Some(expected_ip.unwrap_or(loopback)),
            resolves_to: vec![loopback],
            ok: true,
        };
    }
    let resolves_to = resolver
        .resolve_ipv4(host)
        .unwrap_or_default()
        .into_iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let ok = expected_ip.is_some_and(|expected| resolves_to.contains(&expected));
    DnsPrecheck {
        expected_ip,
        resolves_to,
        ok,
    }
}

/// Hosts that are only meaningful on the local machine (no public A record).
pub fn is_local_host(host: &str) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    host == "localhost"
        || host.ends_with(".localhost")
        || host == "127.0.0.1"
        || host == "0.0.0.0"
        || host == "::1"
}

/// Determine this node's expected public IPv4. The explicit override always wins.
pub fn expected_public_ipv4() -> Option<Ipv4Addr> {
    if let Some(value) = std::env::var_os("CYGNUS_PUBLIC_IP") {
        return value.to_string_lossy().trim().parse().ok();
    }
    let config = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(3)))
        .build();
    let agent: ureq::Agent = config.into();
    let mut response = agent.get("https://api.ipify.org").call().ok()?;
    let value = response.body_mut().read_to_string().ok()?;
    value.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_hosts_skip_public_dns_and_are_ok() {
        let expected = Ipv4Addr::new(203, 0, 113, 8);
        // Even with a resolver that would fail, *.localhost is treated as local.
        let result = dns_precheck(&FixedResolver(vec![]), "app.apps.localhost", Some(expected));
        assert!(result.ok);
        assert_eq!(result.resolves_to, [Ipv4Addr::new(127, 0, 0, 1)]);
        assert!(is_local_host("apps.localhost"));
        assert!(is_local_host("Draco.localhost."));
        assert!(!is_local_host("example.com"));
    }
    struct FixedResolver(Vec<Ipv4Addr>);

    impl DnsResolver for FixedResolver {
        fn resolve_ipv4(&self, _host: &str) -> io::Result<Vec<Ipv4Addr>> {
            Ok(self.0.clone())
        }
    }

    #[test]
    fn precheck_is_sorted_deduplicated_and_requires_expected_match() {
        let expected = Ipv4Addr::new(203, 0, 113, 8);
        let result = dns_precheck(
            &FixedResolver(vec![expected, Ipv4Addr::new(192, 0, 2, 1), expected]),
            "app.example.com",
            Some(expected),
        );
        assert_eq!(result.resolves_to, [Ipv4Addr::new(192, 0, 2, 1), expected]);
        assert!(result.ok);
        assert!(!dns_precheck(&FixedResolver(vec![expected]), "app.example.com", None).ok);
    }
}
