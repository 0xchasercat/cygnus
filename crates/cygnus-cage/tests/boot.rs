use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use cygnus_cage::{Cage, CageError, CageSpec};

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
    assert!(
        cage.cgroup_path()
            .is_some_and(|path| path.ends_with(&name))
    );
    assert_eq!(cage.timings().mounts, Duration::ZERO);
    assert!(cage.timings().total >= cage.timings().namespaces_cgroup);

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
