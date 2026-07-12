use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use cygnus_cage::{Cage, CageError, CageSpec, RootfsSpec};

#[test]
fn boots_and_tears_down_with_exec_readiness() {
    let name = unique_name("exec");
    let mut spec = CageSpec::new(&name, "/bin/sleep");
    spec.args.push(OsString::from("30"));
    spec.env = env::vars_os().collect();

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

#[cfg(not(target_os = "linux"))]
#[test]
fn rootfs_is_inert_on_the_plain_process_backend() {
    let name = unique_name("overlay");
    let mut spec = CageSpec::new(&name, "/bin/sleep");
    spec.args.push(OsString::from("30"));
    spec.env = env::vars_os().collect();
    spec.rootfs = Some(RootfsSpec::new(vec![PathBuf::from("/")]));

    let cage = Cage::boot(spec).expect("plain process boot");
    assert_eq!(cage.timings().mounts, Duration::ZERO);
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
