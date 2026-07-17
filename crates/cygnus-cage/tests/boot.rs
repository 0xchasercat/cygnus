use std::env;
use std::ffi::OsString;
use std::fs;
#[cfg(target_os = "linux")]
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::time::Duration;

#[cfg(target_os = "linux")]
use cygnus_cage::{AdminSocketSpec, IngressSpec};
use cygnus_cage::{Cage, CageError, CageSpec, RootfsSpec};

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
fn unrooted_child_has_no_capabilities_and_no_new_privs() {
    let name = unique_name("capabilities");
    let mut spec = CageSpec::new(&name, env::current_exe().expect("test executable path"));
    spec.args.extend([
        OsString::from("--exact"),
        OsString::from("cage_fixture_process"),
        OsString::from("--nocapture"),
    ]);
    spec.env.insert(
        OsString::from("CYGNUS_FIXTURE_MODE"),
        OsString::from("capabilities"),
    );
    spec.seccomp = None;

    let cage = match Cage::boot(spec) {
        Ok(cage) => cage,
        Err(error) if environment_unavailable(&error) => {
            eprintln!("skipping privileged capability test: {error}");
            return;
        }
        Err(error) => panic!("cage boot failed: {error}"),
    };

    let pid = cage.host_pid().expect("cage has a host PID");
    let status = fs::read_to_string(format!("/proc/{pid}/status"))
        .expect("read unrooted cage process status");
    assert_capabilities_are_dropped(&status);
    cage.teardown().expect("tear down capability cage");
}

#[cfg(target_os = "linux")]
#[test]
fn overlay_rootfs_contains_writes_and_pivots_proc() {
    let name = unique_name("overlay");
    let fixture = env::temp_dir().join(format!("cygnus-overlay-{name}"));
    let _ = fs::remove_dir_all(&fixture);
    let lower = build_fixture_root(&fixture).expect("build fixture root");

    let mut spec = CageSpec::new(&name, "/fixture");
    spec.args.extend([
        OsString::from("--exact"),
        OsString::from("cage_fixture_process"),
        OsString::from("--nocapture"),
    ]);
    spec.env.insert(
        OsString::from("CYGNUS_FIXTURE_MODE"),
        OsString::from("sleep"),
    );
    spec.rootfs = Some(RootfsSpec::new(vec![lower]));
    spec.seccomp = None;

    let cage = match Cage::boot(spec) {
        Ok(cage) => cage,
        Err(error)
            if environment_unavailable(&error)
                && env::var_os("CYGNUS_REQUIRE_PRIVILEGED").is_none() =>
        {
            eprintln!("skipping privileged overlay rootfs test: {error}");
            let _ = fs::remove_dir_all(&fixture);
            return;
        }
        Err(error) => panic!("cage boot failed: {error}"),
    };

    let pid = cage.host_pid().expect("cage has a host PID");
    let cage_root = PathBuf::from(format!("/proc/{pid}/root"));
    let probe_name = format!("cygnus-overlay-probe-{}", std::process::id());

    fs::write(cage_root.join(&probe_name), b"upper")
        .expect("write through the cage root into the upper layer");
    let host_probe = PathBuf::from(format!("/{probe_name}"));
    assert!(!host_probe.exists(), "cage write leaked into the host root");

    let comm = fs::read_to_string(cage_root.join("proc/1/comm")).expect("read cage /proc/1/comm");
    assert!(comm.trim().starts_with("fixture"));

    assert!(cage.timings().mounts > Duration::ZERO);
    cage.teardown().expect("tear down overlay cage");
    fs::remove_dir_all(&fixture).expect("remove overlay fixture");
}

#[cfg(target_os = "linux")]
#[test]
fn rooted_child_has_no_capabilities_and_no_new_privs() {
    let name = unique_name("rootfs-capabilities");
    let fixture = env::temp_dir().join(format!("cygnus-capabilities-{name}"));
    let _ = fs::remove_dir_all(&fixture);
    let lower = build_fixture_root(&fixture).expect("build fixture root");

    let mut spec = CageSpec::new(&name, "/fixture");
    spec.args.extend([
        OsString::from("--exact"),
        OsString::from("cage_fixture_process"),
        OsString::from("--nocapture"),
    ]);
    spec.env.insert(
        OsString::from("CYGNUS_FIXTURE_MODE"),
        OsString::from("capabilities"),
    );
    spec.rootfs = Some(RootfsSpec::new(vec![lower]));
    spec.seccomp = None;

    let cage = match Cage::boot(spec) {
        Ok(cage) => cage,
        Err(error)
            if environment_unavailable(&error)
                && env::var_os("CYGNUS_REQUIRE_PRIVILEGED").is_none() =>
        {
            eprintln!("skipping privileged rooted capability test: {error}");
            let _ = fs::remove_dir_all(&fixture);
            return;
        }
        Err(error) => panic!("cage boot failed: {error}"),
    };

    let pid = cage.host_pid().expect("cage has a host PID");
    let status =
        fs::read_to_string(format!("/proc/{pid}/status")).expect("read rooted cage process status");
    assert_capabilities_are_dropped(&status);
    cage.teardown().expect("tear down rooted capability cage");
    fs::remove_dir_all(&fixture).expect("remove capability fixture");
}

#[cfg(target_os = "linux")]
#[test]
fn ingress_socket_created_in_pivoted_root_is_host_visible() {
    let name = unique_name("ingress");
    let fixture = env::temp_dir().join(format!("cygnus-ingress-{name}"));
    let host_dir = fixture.join("io");
    let socket_path = host_dir.join("app.sock");
    let _ = fs::remove_dir_all(&fixture);
    fs::create_dir_all(&host_dir).expect("create ingress host directory");
    let lower = build_fixture_root(&fixture).expect("build fixture root");

    let mut spec = CageSpec::new(&name, "/fixture");
    spec.args.extend([
        OsString::from("--exact"),
        OsString::from("cage_fixture_process"),
        OsString::from("--nocapture"),
    ]);
    spec.env
        .insert(OsString::from("CYGNUS_FIXTURE_MODE"), OsString::from("uds"));
    spec.rootfs = Some(RootfsSpec::new(vec![lower]));
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
            let _ = fs::remove_dir_all(&fixture);
            return;
        }
        Err(error) => panic!("cage boot failed: {error}"),
    };

    assert!(
        socket_path.exists(),
        "pivoted cage socket is not host-visible"
    );
    UnixStream::connect(&socket_path).expect("connect through host ingress path");

    cage.teardown().expect("tear down ingress cage");
    fs::remove_dir_all(&fixture).expect("remove ingress fixture");
}

#[cfg(target_os = "linux")]
#[test]
fn rooted_tenant_connects_through_read_only_admin_mount() {
    use std::io::{Read, Write};

    let name = unique_name("tenant-admin");
    let fixture = env::temp_dir().join(format!("cygnus-tenant-admin-{name}"));
    let ingress_dir = fixture.join("io");
    let admin_dir = fixture.join("admin");
    let readiness = ingress_dir.join("app.sock");
    let admin_socket = admin_dir.join("admin.sock");
    let _ = fs::remove_dir_all(&fixture);
    fs::create_dir_all(&ingress_dir).expect("create ingress directory");
    fs::create_dir_all(&admin_dir).expect("create admin directory");
    fs::set_permissions(
        &admin_dir,
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .expect("restrict admin directory");
    let admin_listener = UnixListener::bind(&admin_socket).expect("bind host admin socket");
    let admin_thread = std::thread::spawn(move || {
        let (mut stream, _) = admin_listener.accept().expect("accept admin connection");
        let mut request = [0_u8; 4];
        stream.read_exact(&mut request).expect("read admin probe");
        assert_eq!(&request, b"ping");
        stream.write_all(b"pong").expect("write admin probe");
    });
    let lower = build_fixture_root(&fixture).expect("build fixture root");

    let mut spec = CageSpec::new(&name, "/fixture");
    spec.args.extend([
        OsString::from("--exact"),
        OsString::from("cage_fixture_process"),
        OsString::from("--nocapture"),
    ]);
    spec.env.insert(
        OsString::from("CYGNUS_FIXTURE_MODE"),
        OsString::from("admin-connect"),
    );
    spec.rootfs = Some(RootfsSpec::new(vec![lower]));
    spec.ingress = Some(IngressSpec::new(ingress_dir));
    spec.admin_socket = Some(AdminSocketSpec::new(admin_dir));
    spec.readiness_uds = Some(readiness);
    spec.seccomp = None;

    let cage = match Cage::boot(spec) {
        Ok(cage) => cage,
        Err(error) => {
            let mut stream = UnixStream::connect(&admin_socket).expect("release admin probe");
            stream.write_all(b"ping").expect("write release probe");
            let mut response = [0_u8; 4];
            stream
                .read_exact(&mut response)
                .expect("read release probe");
            admin_thread.join().expect("join skipped admin probe");
            let _ = fs::remove_dir_all(&fixture);
            if environment_unavailable(&error) && env::var_os("CYGNUS_REQUIRE_PRIVILEGED").is_none()
            {
                eprintln!("skipping privileged Tenant admin mount test: {error}");
                return;
            }
            panic!("cage boot failed: {error}");
        }
    };

    admin_thread.join().expect("join admin probe");
    cage.teardown().expect("tear down Tenant admin cage");
    fs::remove_dir_all(&fixture).expect("remove Tenant admin fixture");
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

#[cfg(target_os = "linux")]
#[test]
fn cage_fixture_process() {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixListener;
    use std::thread;

    match env::var("CYGNUS_FIXTURE_MODE").as_deref() {
        Err(_) => {}
        Ok("capabilities") => {
            let status = fs::read_to_string("/proc/self/status")
                .expect("read fixture process capability status");
            assert_capabilities_are_dropped(&status);
            loop {
                thread::sleep(Duration::from_secs(60));
            }
        }
        Ok("sleep") => loop {
            thread::sleep(Duration::from_secs(60));
        },
        Ok("uds") => {
            let listener = UnixListener::bind("/cygnus/io/app.sock").expect("bind fixture UDS");
            for stream in listener.incoming() {
                let mut stream = stream.expect("accept fixture UDS");
                let mut request = [0_u8; 4096];
                let read = stream.read(&mut request).unwrap_or(0);
                if read > 0 {
                    let body = b"overlay request reached the cage\n";
                    write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nconnection: close\r\ncontent-length: {}\r\n\r\n",
                        body.len()
                    )
                    .expect("write fixture response head");
                    stream.write_all(body).expect("write fixture response body");
                }
            }
        }
        Ok("admin-connect") => {
            assert!(
                fs::write("/cygnus/admin/probe", b"must fail").is_err(),
                "Tenant admin mount unexpectedly allowed writes"
            );
            let mut admin = UnixStream::connect("/cygnus/admin/admin.sock")
                .expect("connect mounted admin socket");
            admin.write_all(b"ping").expect("write admin probe");
            let mut response = [0_u8; 4];
            admin.read_exact(&mut response).expect("read admin probe");
            assert_eq!(&response, b"pong");
            drop(admin);

            let listener = UnixListener::bind("/cygnus/io/app.sock").expect("bind fixture UDS");
            for stream in listener.incoming() {
                let _ = stream.expect("accept fixture UDS");
            }
        }
        Ok(mode) => panic!("unknown fixture mode {mode:?}"),
    }
}

#[cfg(target_os = "linux")]
fn assert_capabilities_are_dropped(status: &str) {
    for field in ["CapInh:", "CapPrm:", "CapEff:", "CapBnd:", "CapAmb:"] {
        let value = status
            .lines()
            .find_map(|line| {
                let mut fields = line.split_whitespace();
                (fields.next() == Some(field)).then(|| fields.next())
            })
            .flatten()
            .unwrap_or_else(|| panic!("{field} missing from /proc/self/status"));
        let value = u64::from_str_radix(value, 16).expect("capability field is hexadecimal");
        assert_eq!(value, 0, "{field} should be zero");
    }

    let no_new_privs = status
        .lines()
        .find_map(|line| {
            let mut fields = line.split_whitespace();
            (fields.next() == Some("NoNewPrivs:")).then(|| fields.next())
        })
        .flatten()
        .unwrap_or_else(|| panic!("NoNewPrivs missing from /proc/self/status"));
    assert_eq!(no_new_privs, "1", "NoNewPrivs should be enabled");
}

#[cfg(target_os = "linux")]
fn build_fixture_root(base: &std::path::Path) -> std::io::Result<PathBuf> {
    let root = base.join("root");
    fs::create_dir_all(&root)?;
    let executable = env::current_exe()?;
    fs::copy(&executable, root.join("fixture"))?;

    let output = std::process::Command::new("ldd")
        .arg(&executable)
        .output()?;
    if !output.status.success() {
        return Err(std::io::Error::other("ldd failed for cage fixture"));
    }
    let dependencies = String::from_utf8(output.stdout)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    for line in dependencies.lines() {
        let Some(source) = line
            .split_whitespace()
            .find(|field| field.starts_with('/'))
            .map(PathBuf::from)
        else {
            continue;
        };
        let relative = source.strip_prefix("/").expect("absolute ldd dependency");
        let destination = root.join(relative);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(source, destination)?;
    }
    Ok(root)
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
