//! Cage egress networking: addressing, per-cage nftables policy, and host NAT.
//!
//! Spec §7. Addressing and the per-cage policy ruleset are pure and
//! unit-tested; the veth fabric, the host bridge, the host NAT masquerade, and
//! loading the ruleset run during boot. The host-side DNS forwarder and
//! `resolv.conf` wiring are the remaining deferred pieces.
//!
//! The policy is enforced in the cage's own network namespace as an output
//! chain with a default-drop policy, so containment is by construction: a rule
//! the operator did not ask for cannot let traffic out. `Public` is SSRF-safe
//! by default — cloud metadata, the node itself, sibling cages, and RFC1918 are
//! denied, leaving the public internet and the host DNS forwarder.

use std::fmt::Write as _;
use std::io::Write as _;
use std::net::Ipv4Addr;
use std::process::{Command, Stdio};

use crate::error::CageError;
use crate::spec::EgressMode;

/// Host bridge that every cage veth pair attaches to.
pub const BRIDGE_NAME: &str = "cygnus0";
/// Bridge gateway address, and the host DNS forwarder endpoint (spec §7).
pub const GATEWAY: Ipv4Addr = Ipv4Addr::new(100, 64, 0, 1);
/// Prefix length of the cage subnet, `100.64.0.0/16`.
pub const SUBNET_PREFIX: u8 = 16;
/// Network address of the cage subnet.
pub const SUBNET_BASE: Ipv4Addr = Ipv4Addr::new(100, 64, 0, 0);
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
    if matches!(mode, EgressMode::BuildDomains { .. }) {
        let _ = writeln!(script, "\tset dns_v4 {{");
        let _ = writeln!(script, "\t\ttype ipv4_addr;");
        let _ = writeln!(script, "\t\tflags timeout;");
        let _ = writeln!(script, "\t\tsize 1024;");
        let _ = writeln!(script, "\t}}");
    }
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
        EgressMode::BuildDomains { allow } => {
            for private in ["10.0.0.0/8", "172.16.0.0/12", "192.168.0.0/16"] {
                let _ = writeln!(script, "\t\tip daddr {private} drop");
            }
            let ports = allow
                .iter()
                .flat_map(|rule| rule.ports.iter().copied())
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect::<Vec<_>>();
            if !ports.is_empty() {
                let ports = ports
                    .iter()
                    .map(|port| port.to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                let _ = writeln!(
                    script,
                    "\t\tip daddr @dns_v4 tcp dport {{ {ports} }} accept"
                );
            }
        }
    }

    let _ = writeln!(script, "\t}}");
    let _ = writeln!(script, "}}");
    Some(script)
}

/// The host NAT ruleset: masquerade cage-subnet traffic leaving the node so it
/// can reach the internet, with return traffic tracked back to the cage.
///
/// Written as flush-and-recreate so repeated cage boots never stack duplicate
/// rules: the table is ensured to exist, deleted, then defined fresh.
fn nat_ruleset() -> String {
    format!(
        "table ip cygnus_nat {{}}\n\
         delete table ip cygnus_nat\n\
         table ip cygnus_nat {{\n\
         \tchain postrouting {{\n\
         \t\ttype nat hook postrouting priority srcnat; policy accept;\n\
         \t\tip saddr {SUBNET_BASE}/{SUBNET_PREFIX} masquerade\n\
         \t}}\n\
         }}\n"
    )
}

/// Temporary host-side name of the cage-side veth end, before it is moved into
/// the cage netns and renamed to [`CAGE_INTERFACE`]. Distinct from the bridge
/// end so both can exist briefly in the host namespace.
fn peer_veth_name(name: &str) -> String {
    format!("cyp{:012x}", fnv1a(name.as_bytes()) & 0xFFFF_FFFF_FFFF)
}

/// Idempotent host bridge setup: the `cygnus0` bridge, the gateway address, and
/// bringing it up. IP forwarding and the NAT masquerade are handled by
/// [`ensure_host_nat`]; this establishes only the local L2/L3 fabric the cage
/// veths attach to.
fn bridge_setup_commands() -> Vec<Vec<String>> {
    vec![
        argv(&["ip", "link", "add", "name", BRIDGE_NAME, "type", "bridge"]),
        argv(&[
            "ip",
            "addr",
            "add",
            &format!("{GATEWAY}/{SUBNET_PREFIX}"),
            "dev",
            BRIDGE_NAME,
        ]),
        argv(&["ip", "link", "set", BRIDGE_NAME, "up"]),
    ]
}

/// The ordered commands that attach one cage to the bridge and configure its
/// interface. `pid` is the cage's host-visible PID; the `nsenter` commands
/// enter that PID's network namespace to configure the moved-in peer.
fn veth_setup_commands(name: &str, ip: Ipv4Addr, pid: i32) -> Vec<Vec<String>> {
    let host = host_veth_name(name);
    let peer = peer_veth_name(name);
    let pid = pid.to_string();
    let cidr = format!("{ip}/{SUBNET_PREFIX}");
    let gateway = GATEWAY.to_string();
    vec![
        argv(&[
            "ip", "link", "add", &host, "type", "veth", "peer", "name", &peer,
        ]),
        argv(&["ip", "link", "set", &host, "master", BRIDGE_NAME]),
        argv(&["ip", "link", "set", &host, "up"]),
        argv(&["ip", "link", "set", &peer, "netns", &pid]),
        nsenter(&pid, &["ip", "link", "set", &peer, "name", CAGE_INTERFACE]),
        nsenter(&pid, &["ip", "addr", "add", &cidr, "dev", CAGE_INTERFACE]),
        nsenter(&pid, &["ip", "link", "set", CAGE_INTERFACE, "up"]),
        nsenter(&pid, &["ip", "link", "set", "lo", "up"]),
        nsenter(&pid, &["ip", "route", "add", "default", "via", &gateway]),
    ]
}

/// The command that removes a cage's host-side veth by interface name.
/// Deleting either end of the pair removes both; the peer usually vanishes with
/// the cage netns first, so callers tolerate a missing device.
fn veth_delete_command(interface: &str) -> Vec<String> {
    argv(&["ip", "link", "del", interface])
}

fn argv(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|part| (*part).to_owned()).collect()
}

fn nsenter(pid: &str, parts: &[&str]) -> Vec<String> {
    let mut command = argv(&["nsenter", "-t", pid, "-n", "--"]);
    command.extend(parts.iter().map(|part| (*part).to_owned()));
    command
}

/// Ensure the host bridge exists and is up. Tolerates the "already exists"
/// races that make repeated setup safe across cage boots.
pub(crate) fn ensure_bridge() -> Result<(), CageError> {
    for command in bridge_setup_commands() {
        run_tolerant(&command, "ensure host bridge", |stderr| {
            stderr.contains("exists")
        })?;
    }
    Ok(())
}

/// Ensure the node forwards and masquerades cage traffic, so a cage with egress
/// can reach the internet. Idempotent across boots.
pub(crate) fn ensure_host_nat() -> Result<(), CageError> {
    std::fs::write("/proc/sys/net/ipv4/ip_forward", "1\n").map_err(|source| {
        CageError::Network {
            operation: "enable IP forwarding".into(),
            detail: source.to_string(),
        }
    })?;
    nft_load(None, &nat_ruleset(), "load host NAT ruleset")
}

/// Attach a cage to the bridge, ensure host NAT, address its interface, and
/// load its egress policy. Runs while the cage child is parked, so the network
/// is ready before the app execs.
pub(crate) fn configure_cage(
    name: &str,
    ip: Ipv4Addr,
    pid: i32,
    mode: &EgressMode,
) -> Result<(), CageError> {
    ensure_bridge()?;
    ensure_host_nat()?;
    for command in veth_setup_commands(name, ip, pid) {
        run(&command, "configure cage network")?;
    }
    if let Some(ruleset) = nftables_ruleset(ip, mode) {
        nft_load(Some(pid), &ruleset, "load cage nftables ruleset")?;
    }
    Ok(())
}

/// Best-effort removal of a cage's host-side veth at teardown, by interface
/// name (the value returned by [`host_veth_name`]).
pub(crate) fn delete_veth(interface: &str) -> Result<(), CageError> {
    run_tolerant(
        &veth_delete_command(interface),
        "tear down cage network",
        |stderr| stderr.contains("Cannot find") || stderr.contains("does not exist"),
    )
}

/// Load an nftables script from stdin, either on the host (`netns_pid` is
/// `None`) or inside a cage's network namespace (`Some(pid)`, via `nsenter`).
fn nft_load(
    netns_pid: Option<i32>,
    script: &str,
    operation: &'static str,
) -> Result<(), CageError> {
    let mut command = match netns_pid {
        Some(pid) => {
            let mut command = Command::new("nsenter");
            command.args(["-t", &pid.to_string(), "-n", "--", "nft", "-f", "-"]);
            command
        }
        None => {
            let mut command = Command::new("nft");
            command.args(["-f", "-"]);
            command
        }
    };
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|source| CageError::Network {
            operation: operation.into(),
            detail: format!("spawn nft: {source}"),
        })?;
    child
        .stdin
        .take()
        .expect("nft stdin was requested")
        .write_all(script.as_bytes())
        .map_err(|source| CageError::Network {
            operation: operation.into(),
            detail: format!("write nft script: {source}"),
        })?;
    let output = child
        .wait_with_output()
        .map_err(|source| CageError::Network {
            operation: operation.into(),
            detail: format!("run nft: {source}"),
        })?;
    if !output.status.success() {
        return Err(CageError::Network {
            operation: operation.into(),
            detail: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }
    Ok(())
}

fn run(command: &[String], operation: &'static str) -> Result<(), CageError> {
    run_tolerant(command, operation, |_| false)
}

fn run_tolerant(
    command: &[String],
    operation: &'static str,
    tolerate: impl Fn(&str) -> bool,
) -> Result<(), CageError> {
    let output = Command::new(&command[0])
        .args(&command[1..])
        .output()
        .map_err(|source| CageError::Network {
            operation: operation.into(),
            detail: format!("spawn {:?}: {source}", command[0]),
        })?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stderr = stderr.trim();
    if tolerate(stderr) {
        return Ok(());
    }
    Err(CageError::Network {
        operation: format!("{operation}: {}", command.join(" ")),
        detail: stderr.to_owned(),
    })
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
    use crate::spec::{DomainEgressRule, EgressRule};

    #[test]
    fn addresses_are_deterministic() {
        assert_eq!(cage_ipv4("api"), cage_ipv4("api"));
        assert_ne!(cage_ipv4("api"), cage_ipv4("web"));
    }

    #[test]
    fn addresses_stay_in_the_assignable_range() {
        for name in [
            "a",
            "web",
            "api",
            "preview-123",
            "tenant-0",
            "x".repeat(64).as_str(),
        ] {
            let ip = cage_ipv4(name);
            let octets = ip.octets();
            assert_eq!([octets[0], octets[1]], [100, 64], "{name} left the subnet");
            assert_ne!(
                ip,
                Ipv4Addr::new(100, 64, 0, 0),
                "{name} hit the network address"
            );
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
    fn peer_and_host_names_differ_and_fit() {
        assert_eq!(peer_veth_name(&"x".repeat(200)).len(), 15);
        assert!(peer_veth_name("api").starts_with("cyp"));
        assert_ne!(host_veth_name("api"), peer_veth_name("api"));
    }

    #[test]
    fn veth_setup_moves_the_peer_and_addresses_it_in_the_netns() {
        let ip = cage_ipv4("api");
        let commands = veth_setup_commands("api", ip, 4321);
        let joined: Vec<String> = commands.iter().map(|c| c.join(" ")).collect();

        // Host side: create the pair, enslave to the bridge, move the peer in.
        assert!(joined[0].starts_with("ip link add cyv"));
        assert!(joined[0].contains("peer name cyp"));
        assert!(joined.iter().any(|c| c.contains("master cygnus0")));
        assert!(joined.iter().any(|c| c.ends_with("netns 4321")));

        // Netns side runs under nsenter against the cage pid.
        let addr = joined
            .iter()
            .find(|c| c.contains("addr add"))
            .expect("address command");
        assert!(addr.starts_with("nsenter -t 4321 -n --"));
        assert!(addr.contains(&format!("{ip}/16")));
        assert!(addr.contains("dev eth0"));
        assert!(
            joined
                .iter()
                .any(|c| c.contains("route add default via 100.64.0.1"))
        );
    }

    #[test]
    fn bridge_setup_configures_the_gateway() {
        let joined: Vec<String> = bridge_setup_commands()
            .iter()
            .map(|c| c.join(" "))
            .collect();
        assert!(
            joined
                .iter()
                .any(|c| c == "ip link add name cygnus0 type bridge")
        );
        assert!(
            joined
                .iter()
                .any(|c| c == "ip addr add 100.64.0.1/16 dev cygnus0")
        );
        assert!(joined.iter().any(|c| c == "ip link set cygnus0 up"));
    }

    #[test]
    fn nat_masquerades_the_cage_subnet() {
        let script = nat_ruleset();
        assert!(script.contains("ip saddr 100.64.0.0/16 masquerade"));
        assert!(script.contains("type nat hook postrouting"));
        // Flush-and-recreate keeps repeated boots from stacking rules.
        assert!(script.contains("delete table ip cygnus_nat"));
    }

    #[test]
    fn teardown_removes_the_host_veth() {
        let iface = host_veth_name("api");
        assert_eq!(
            veth_delete_command(&iface).join(" "),
            format!("ip link del {iface}")
        );
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
        let dns = script
            .find("ip daddr 100.64.0.1 udp dport 53")
            .expect("dns rule");
        let bridge_drop = script
            .find("ip daddr 100.64.0.0/16 drop")
            .expect("bridge drop");
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

    #[test]
    fn build_domains_use_a_timed_ipv4_set_and_the_union_of_tcp_ports() {
        let mode = EgressMode::BuildDomains {
            allow: vec![
                DomainEgressRule {
                    domain: "registry.npmjs.org".into(),
                    ports: vec![443],
                },
                DomainEgressRule {
                    domain: "git.example.com".into(),
                    ports: vec![22, 443],
                },
            ],
        };
        let script = nftables_ruleset(cage_ipv4("builder"), &mode).expect("ruleset");

        assert!(script.contains("set dns_v4 {"));
        assert!(script.contains("type ipv4_addr;"));
        assert!(script.contains("flags timeout;"));
        assert!(script.contains("size 1024;"));
        assert!(script.contains("ip daddr @dns_v4 tcp dport { 22, 443 } accept"));
        assert!(!script.contains("ip daddr @dns_v4 udp"));
        assert!(!script.contains("meta l4proto { tcp, udp } accept"));
    }

    #[test]
    fn build_domain_denials_precede_dynamic_set_allow() {
        let mode = EgressMode::BuildDomains {
            allow: vec![DomainEgressRule {
                domain: "registry.npmjs.org".into(),
                ports: vec![443],
            }],
        };
        let script = nftables_ruleset(cage_ipv4("builder"), &mode).expect("ruleset");
        let dynamic_allow = script.find("ip daddr @dns_v4").expect("dynamic allow");

        for denial in [
            "ip daddr 169.254.0.0/16 drop",
            "ip daddr 100.64.0.0/16 drop",
            "ip daddr 10.0.0.0/8 drop",
            "ip daddr 172.16.0.0/12 drop",
            "ip daddr 192.168.0.0/16 drop",
            "ip daddr 224.0.0.0/4 drop",
        ] {
            assert!(
                script.find(denial).expect("required denial") < dynamic_allow,
                "{denial} must precede the dynamic allow"
            );
        }

        let dns = script
            .find("ip daddr 100.64.0.1 udp dport 53 accept")
            .expect("gateway DNS allow");
        let bridge = script
            .find("ip daddr 100.64.0.0/16 drop")
            .expect("bridge denial");
        assert!(dns < bridge);
    }

    #[test]
    fn empty_build_domain_policy_remains_valid_default_drop() {
        let script = nftables_ruleset(
            cage_ipv4("builder"),
            &EgressMode::BuildDomains { allow: Vec::new() },
        )
        .expect("ruleset");

        assert!(script.contains("set dns_v4 {"));
        assert!(script.contains("policy drop;"));
        assert!(!script.contains("tcp dport {  }"));
    }
}
