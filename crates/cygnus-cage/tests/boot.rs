use std::env;
use std::ffi::OsString;
use std::fs;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use cygnus_cage::{Cage, CageError, CageSpec, IngressSpec, RootfsSpec};

#[test]
fn boots_and_tears_down_with_exec_readiness() {
    let name = unique_name("exec");
    let mut spec = CageSpec::new(&name, "/bin/sleep");
    spec.args.push(OsString::from("30"));
    spec.env = env::vars_os().collect();
    // This test exercises the no-filter path explicitly, independent of the
    // default; the default (Enforce) filter has its own test below.
    spec.seccomp = None;

    let cage = match Cage::boot(spec) {
        Ok(cage) => cage,
        Err(error) if environment_unavailable(&error) => {
            eprintln!("skipping privileged cage boot test: {error}");
            return;
        }
        Err(error) => panic!("cage boot failed: {error}"),
    };

    assert!(cage.host_pid().is_some_and(|pid| pid > 0));
    #[cfg(target_os = "linux")]
    assert!(cage.cgroup_path().is_some_and(|path| path.ends_with(&name)));
    #[cfg(not(target_os = "linux"))]
    assert!(cage.cgroup_path().is_none());
    // The Linux backend mounts a private /proc; the plain-process backend does
    // no mounting.
    #[cfg(target_os = "linux")]
    assert!(cage.timings().mounts > Duration::ZERO);
    #[cfg(not(target_os = "linux"))]
    assert_eq!(cage.timings().mounts, Duration::ZERO);
    // No filter was requested, so the seccomp phase does nothing.
    assert_eq!(cage.timings().seccomp, Duration::ZERO);
    assert!(cage.timings().total >= cage.timings().namespaces_cgroup);

    if let Err(error) = cage.teardown() {
        panic!("cage teardown failed: {error}");
    }
}

#[cfg(target_os = "linux")]
#[test]
fn overlay_rootfs_contains_writes_and_pivots_proc() {
    let name = unique_name("overlay");
    let mut spec = CageSpec::new(&name, "/bin/sleep");
    spec.args.push(OsString::from("30"));
    spec.env = env::vars_os().collect();
    // The host root as the single read-only lower layer: the merged tree
    // looks like the host, but every write lands in the cage-private tmpfs.
    spec.rootfs = Some(RootfsSpec::new(vec![PathBuf::from("/")]));
    spec.seccomp = None;

    let cage = match Cage::boot(spec) {
        Ok(cage) => cage,
        Err(error) if environment_unavailable(&error) => {
            eprintln!("skipping privileged overlay rootfs test: {error}");
            return;
        }
        Err(error) => panic!("cage boot failed: {error}"),
    };

    let pid = cage.host_pid().expect("cage has a host PID");
    let cage_root = PathBuf::from(format!("/proc/{pid}/root"));
    let probe_name = format!("cygnus-overlay-probe-{}", std::process::id());

    // The pivoted root is writable through the upper layer...
    fs::write(cage_root.join(&probe_name), b"upper")
        .expect("write through the cage root into the upper layer");
    // ...and the write stays out of the host tree.
    let host_probe = PathBuf::from(format!("/{probe_name}"));
    assert!(!host_probe.exists(), "cage write leaked into the host root");

    // The fresh procfs reflects the cage's PID namespace: the target is PID 1.
    let comm = fs::read_to_string(cage_root.join("proc/1/comm")).expect("read cage /proc/1/comm");
    assert_eq!(comm.trim(), "sleep");

    assert!(cage.timings().mounts > Duration::ZERO);
    if let Err(error) = cage.teardown() {
        panic!("cage teardown failed: {error}");
    }
}

#[cfg(target_os = "linux")]
#[test]
fn ingress_socket_created_in_pivoted_root_is_host_visible() {
    let Some(python) = python3_path() else {
        eprintln!("skipping ingress test: python3 is unavailable");
        return;
    };
    let name = unique_name("ingress");
    let host_dir = env::temp_dir().join(format!("cygnus-ingress-{name}"));
    let socket_path = host_dir.join("app.sock");
    let _ = fs::remove_dir_all(&host_dir);
    fs::create_dir_all(&host_dir).expect("create ingress host directory");

    let script = r#"
import os
import socket
import time

path = "/cygnus/io/app.sock"
try:
    os.unlink(path)
except FileNotFoundError:
    pass
server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
server.bind(path)
server.listen(1)
time.sleep(30)
"#;
    let mut spec = CageSpec::new(&name, python);
    spec.args.push(OsString::from("-c"));
    spec.args.push(OsString::from(script));
    spec.rootfs = Some(RootfsSpec::new(vec![PathBuf::from("/")]));
    spec.ingress = Some(IngressSpec::new(host_dir.clone()));
    spec.readiness_uds = Some(socket_path.clone());
    spec.seccomp = None;

    let cage = match Cage::boot(spec) {
        Ok(cage) => cage,
        Err(error)
            if environment_unavailable(&error)
                && env::var_os("CYGNUS_REQUIRE_PRIVILEGED").is_none() =>
        {
            eprintln!("skipping privileged ingress test: {error}");
            let _ = fs::remove_dir_all(&host_dir);
            return;
        }
        Err(error) => {
            let _ = fs::remove_dir_all(&host_dir);
            panic!("cage boot failed: {error}");
        }
    };

    assert!(socket_path.exists(), "pivoted cage socket is not host-visible");
    UnixStream::connect(&socket_path).expect("connect through host ingress path");

    if let Err(error) = cage.teardown() {
        let _ = fs::remove_dir_all(&host_dir);
        panic!("cage teardown failed: {error}");
    }
    fs::remove_dir_all(&host_dir).expect("remove ingress host directory");
}

#[cfg(not(target_os = "linux"))]
#[test]
fn rootfs_is_inert_on_the_plain_process_backend() {
    let name = unique_name("overlay");
    let mut spec = CageSpec::new(&name, "/bin/sleep");
    spec.args.push(OsString::from("30"));
    spec.env = env::vars_os().collect();
    spec.rootfs = Some(RootfsSpec::new(vec![PathBuf::from("/")]));
    spec.seccomp = None;

    let cage = Cage::boot(spec).expect("plain process boot");
    assert_eq!(cage.timings().mounts, Duration::ZERO);
    if let Err(error) = cage.teardown() {
        panic!("cage teardown failed: {error}");
    }
}

// The default cage installs the Enforce denylist. A normal target runs fine
// under it (the denylist only blocks dangerous syscalls), so this confirms the
// out-of-the-box sandbox does not break an ordinary boot.
#[cfg(target_os = "linux")]
#[test]
fn boots_under_the_default_enforce_filter() {
    let name = unique_name("seccomp-default");
    let mut spec = CageSpec::new(&name, "/bin/sleep");
    spec.args.push(OsString::from("30"));
    spec.env = env::vars_os().collect();
    // Leave spec.seccomp at its default of Some(FilterMode::Enforce).

    let cage = match Cage::boot(spec) {
        Ok(cage) => cage,
        Err(error) if environment_unavailable(&error) || seccomp_unavailable(&error) => {
            eprintln!("skipping default seccomp boot test: {error}");
            return;
        }
        Err(error) => panic!("cage boot failed: {error}"),
    };

    assert!(cage.host_pid().is_some_and(|pid| pid > 0));
    if let Err(error) = cage.teardown() {
        panic!("cage teardown failed: {error}");
    }
}

// Audit mode installs the same denylist but logs blocked syscalls instead of
// failing them, for observing a workload before enforcing.
#[cfg(target_os = "linux")]
#[test]
fn boots_under_an_audit_seccomp_filter() {
    use cygnus_cage::FilterMode;

    let name = unique_name("seccomp");
    let mut spec = CageSpec::new(&name, "/bin/sleep");
    spec.args.push(OsString::from("30"));
    spec.env = env::vars_os().collect();
    spec.seccomp = Some(FilterMode::Audit);

    let cage = match Cage::boot(spec) {
        Ok(cage) => cage,
        Err(error) if environment_unavailable(&error) || seccomp_unavailable(&error) => {
            eprintln!("skipping seccomp boot test: {error}");
            return;
        }
        Err(error) => panic!("cage boot failed: {error}"),
    };

    // The child installed the filter and went on to exec the target.
    assert!(cage.host_pid().is_some_and(|pid| pid > 0));
    assert!(cage.timings().total >= cage.timings().mounts + cage.timings().seccomp);
    if let Err(error) = cage.teardown() {
        panic!("cage teardown failed: {error}");
    }
}

#[test]
fn waits_until_a_unix_socket_accepts_connections() {
    let Some(python) = python3_path() else {
        eprintln!("skipping UDS readiness test: python3 is unavailable");
        return;
    };
    let name = unique_name("uds");
    let socket_path = env::temp_dir().join(format!("{name}.sock"));
    let _ = fs::remove_file(&socket_path);

    let script = r#"
import os
import socket
import time

path = os.environ["CYGNUS_TEST_SOCKET"]
try:
    os.unlink(path)
except FileNotFoundError:
    pass
server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
server.bind(path)
server.listen(1)
time.sleep(30)
"#;

    let mut spec = CageSpec::new(&name, python);
    spec.args.push(OsString::from("-c"));
    spec.args.push(OsString::from(script));
    spec.env = env::vars_os().collect();
    spec.env.insert(
        OsString::from("CYGNUS_TEST_SOCKET"),
        socket_path.as_os_str().to_os_string(),
    );
    spec.readiness_uds = Some(socket_path.clone());
    spec.seccomp = None;

    let cage = match Cage::boot(spec) {
        Ok(cage) => cage,
        Err(error) if environment_unavailable(&error) => {
            eprintln!("skipping privileged UDS readiness test: {error}");
            let _ = fs::remove_file(&socket_path);
            return;
        }
        Err(error) => {
            let _ = fs::remove_file(&socket_path);
            panic!("cage boot failed: {error}");
        }
    };

    assert!(cage.timings().socket_ready > Duration::ZERO);
    if let Err(error) = cage.teardown() {
        let _ = fs::remove_file(&socket_path);
        panic!("cage teardown failed: {error}");
    }
    let _ = fs::remove_file(&socket_path);
}

fn unique_name(kind: &str) -> String {
    format!("test-{kind}-{}", std::process::id())
}

fn python3_path() -> Option<PathBuf> {
    ["/usr/bin/python3", "/bin/python3"]
        .into_iter()
        .map(PathBuf::from)
        .find(|path| path.is_file())
}

fn environment_unavailable(error: &CageError) -> bool {
    matches!(
        error,
        CageError::NamespaceUnavailable { .. }
            | CageError::CgroupUnavailable(_)
            | CageError::CgroupControllerUnavailable(_)
            | CageError::Io { .. }
    )
}

// Some CI sandboxes forbid the seccomp syscall entirely; a filter install that
// fails at the seccomp stage is an environment limitation, not a wiring fault.
#[cfg(target_os = "linux")]
fn seccomp_unavailable(error: &CageError) -> bool {
    matches!(
        error,
        CageError::ChildSetup {
            stage: "seccomp filter",
            ..
        } | CageError::SeccompFilter(_)
    )
}
