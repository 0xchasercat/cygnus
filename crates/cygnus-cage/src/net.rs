//! Cage egress networking: addressing and per-cage nftables policy.
//!
//! This is the pure model behind spec §7 — deterministic per-app addressing on
//! the node's CGNAT bridge, and the nftables ruleset that encodes each cage's
//! egress policy. Creating the veth pair, attaching the bridge, configuring the
//! host NAT, and loading the ruleset live in the boot path and land in a
//! following slice; everything here is host-independent and unit-tested.
//!
//! The policy is enforced in the cage's own network namespace as an output
//! chain with a default-drop policy, so containment is by construction: a rule
//! the operator did not ask for cannot let traffic out. `Public` is SSRF-safe
//! by default — cloud metadata, the node itself, sibling cages, and RFC1918 are
//! denied, leaving the public internet and the host DNS forwarder.

use std::fmt::Write as _;
use std::net::Ipv4Addr;

use crate::spec::EgressMode;

/// Host bridge that every cage veth pair attaches to.
pub const BRIDGE_NAME: &str = "cygnus0";
/// Bridge gateway address, and the host DNS forwarder endpoint (spec §7).
pub const GATEWAY: Ipv4Addr = Ipv4Addr::new(100, 64, 0, 1);
/// Prefix length of the cage subnet, `100.64.0.0/16`.
pub const SUBNET_PREFIX: u8 = 16;
/// Interface name the peer veth end takes inside the cage.
pub const CAGE_INTERFACE: &str = "eth0";

/// First assignable host value in the subnet (`100.64.0.2`); `.0` is the
/// network address and `.1` is the gateway.
const MIN_HOST: u32 = 2;
/// Last assignable host value (`100.64.255.254`); `.255.255` is broadcast.
const MAX_HOST: u32 = 0xFFFE;

/// Deterministic per-app address inside `100.64.0.0/16`.
///
/// The same name always maps to the same address, and the result never
/// collides with the network address, the gateway, or the broadcast address.
/// Distinct names can still collide (birthday bound over ~65k hosts); the
/// supervisor that owns allocation will detect and resolve that when live
/// addressing lands. This function is the stable default placement.
pub fn cage_ipv4(name: &str) -> Ipv4Addr {
    let span = MAX_HOST - MIN_HOST + 1;
    let host = MIN_HOST + (fnv1a(name.as_bytes()) % u64::from(span)) as u32;
    let octets = (host & 0xFFFF) as u16;
    Ipv4Addr::new(100, 64, (octets >> 8) as u8, (octets & 0xFF) as u8)
}

/// Host-side veth interface name for a cage, kept within the 15-character
/// interface-name limit (`cyv` + 12 hex digits).
pub fn host_veth_name(name: &str) -> String {
    format!("cyv{:012x}", fnv1a(name.as_bytes()) & 0xFFFF_FFFF_FFFF)
}

/// Build the nftables ruleset for one cage's network namespace.
///
/// Returns `None` for [`EgressMode::None`], which has no veth and therefore no
/// ruleset. Otherwise the script defines an `inet cygnus` table with a
/// default-drop `egress` output chain: established/related and loopback are
/// always allowed, DNS to the gateway forwarder is allowed, and the remaining
/// rules follow the mode.
pub fn nftables_ruleset(cage_ip: Ipv4Addr, mode: &EgressMode) -> Option<String> {
    if matches!(mode, EgressMode::None) {
        return None;
    }

    let mut script = String::new();
    let _ = writeln!(script, "# cage {cage_ip}");
    let _ = writeln!(script, "table inet cygnus {{");
    let _ = writeln!(script, "\tchain egress {{");
    let _ = writeln!(
        script,
        "\t\ttype filter hook output priority filter; policy drop;"
    );
    let _ = writeln!(script, "\t\tct state established,related accept");
    let _ = writeln!(script, "\t\toifname \"lo\" accept");
    // DNS to the host forwarder. This precedes the bridge-subnet drop below
    // because the gateway lives inside that subnet.
    let _ = writeln!(script, "\t\tip daddr {GATEWAY} udp dport 53 accept");
    let _ = writeln!(script, "\t\tip daddr {GATEWAY} tcp dport 53 accept");

    // Denied on every networked mode: cloud metadata and sibling cages.
    let _ = writeln!(script, "\t\tip daddr 169.254.0.0/16 drop");
    let _ = writeln!(script, "\t\tip daddr 100.64.0.0/16 drop");
    // Reserved and non-unicast space is never a valid egress target.
    let _ = writeln!(script, "\t\tip daddr 224.0.0.0/4 drop");
    let _ = writeln!(script, "\t\tip daddr 240.0.0.0/4 drop");

    match mode {
        EgressMode::None => unreachable!("handled above"),
        EgressMode::Public => {
            for private in ["10.0.0.0/8", "172.16.0.0/12", "192.168.0.0/16"] {
                let _ = writeln!(script, "\t\tip daddr {private} drop");
            }
            let _ = writeln!(script, "\t\tmeta l4proto {{ tcp, udp }} accept");
        }
        EgressMode::Open => {
            // RFC1918 stays reachable; metadata and neighbours were denied above.
            let _ = writeln!(script, "\t\tmeta l4proto {{ tcp, udp }} accept");
        }
        EgressMode::Restricted { allow } => {
            for rule in allow {
                match rule.ports.as_slice() {
                    [] => {
                        let _ = writeln!(script, "\t\tip daddr {} accept", rule.cidr);
                    }
                    ports => {
                        let list = ports
                            .iter()
                            .map(u16::to_string)
                            .collect::<Vec<_>>()
                            .join(", ");
                        let _ = writeln!(
                            script,
                            "\t\tip daddr {} th dport {{ {list} }} accept",
                            rule.cidr
                        );
                    }
                }
            }
        }
    }

    let _ = writeln!(script, "\t}}");
    let _ = writeln!(script, "}}");
    Some(script)
}

/// 64-bit FNV-1a. Stable across platforms and runs, which is what makes the
/// derived addresses and interface names deterministic.
fn fnv1a(bytes: &[u8]) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::EgressRule;

    #[test]
    fn addresses_are_deterministic() {
        assert_eq!(cage_ipv4("api"), cage_ipv4("api"));
        assert_ne!(cage_ipv4("api"), cage_ipv4("web"));
    }

    #[test]
    fn addresses_stay_in_the_assignable_range() {
        for name in ["a", "web", "api", "preview-123", "tenant-0", "x".repeat(64).as_str()] {
            let ip = cage_ipv4(name);
            let octets = ip.octets();
            assert_eq!([octets[0], octets[1]], [100, 64], "{name} left the subnet");
            assert_ne!(ip, Ipv4Addr::new(100, 64, 0, 0), "{name} hit the network address");
            assert_ne!(ip, GATEWAY, "{name} hit the gateway");
            assert_ne!(
                ip,
                Ipv4Addr::new(100, 64, 255, 255),
                "{name} hit the broadcast address"
            );
        }
    }

    #[test]
    fn veth_names_fit_the_interface_limit() {
        let name = host_veth_name(&"x".repeat(200));
        assert_eq!(name.len(), 15);
        assert!(name.starts_with("cyv"));
        assert_ne!(host_veth_name("api"), host_veth_name("web"));
    }

    #[test]
    fn none_mode_has_no_ruleset() {
        assert!(nftables_ruleset(cage_ipv4("api"), &EgressMode::None).is_none());
    }

    #[test]
    fn public_denies_private_and_metadata_then_allows_the_internet() {
        let script = nftables_ruleset(cage_ipv4("api"), &EgressMode::Public).expect("ruleset");
        assert!(script.contains("policy drop;"));
        assert!(script.contains("ip daddr 169.254.0.0/16 drop"));
        assert!(script.contains("ip daddr 10.0.0.0/8 drop"));
        assert!(script.contains("ip daddr 192.168.0.0/16 drop"));
        assert!(script.contains("meta l4proto { tcp, udp } accept"));

        // DNS to the gateway must be accepted before the bridge subnet is
        // dropped, or name resolution would break.
        let dns = script.find("ip daddr 100.64.0.1 udp dport 53").expect("dns rule");
        let bridge_drop = script.find("ip daddr 100.64.0.0/16 drop").expect("bridge drop");
        assert!(dns < bridge_drop, "DNS accept must precede the bridge drop");
    }

    #[test]
    fn open_allows_private_ranges_but_still_denies_metadata() {
        let script = nftables_ruleset(cage_ipv4("api"), &EgressMode::Open).expect("ruleset");
        assert!(script.contains("ip daddr 169.254.0.0/16 drop"));
        assert!(script.contains("ip daddr 100.64.0.0/16 drop"));
        assert!(!script.contains("ip daddr 10.0.0.0/8 drop"));
        assert!(script.contains("meta l4proto { tcp, udp } accept"));
    }

    #[test]
    fn restricted_allows_only_listed_destinations() {
        let mode = EgressMode::Restricted {
            allow: vec![
                EgressRule {
                    cidr: "203.0.113.0/24".into(),
                    ports: vec![443],
                },
                EgressRule {
                    cidr: "198.51.100.7/32".into(),
                    ports: Vec::new(),
                },
            ],
        };
        let script = nftables_ruleset(cage_ipv4("api"), &mode).expect("ruleset");
        assert!(script.contains("ip daddr 203.0.113.0/24 th dport { 443 } accept"));
        assert!(script.contains("ip daddr 198.51.100.7/32 accept"));
        // Deny-by-default: no blanket internet allow in restricted mode.
        assert!(!script.contains("meta l4proto { tcp, udp } accept"));
    }
}
