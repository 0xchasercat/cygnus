#![cfg(target_os = "linux")]

use std::env;
use std::os::unix::process::ExitStatusExt;
use std::process::{self, Command, ExitStatus};

use cygnus_cage::seccomp::{FilterMode, SeccompPlan};
use nix::libc;

const CHILD_ENV: &str = "CYGNUS_SECCOMP_CHILD";
const CHILD_ALLOWED: &str = "allowed";
const CHILD_DENIED: &str = "denied";
const SECCOMP_UNAVAILABLE: i32 = 96;
const FILTER_DID_NOT_KILL: i32 = 97;
const ALLOWED_SYSCALL_FAILED: i32 = 98;
const UNEXPECTED_APPLY_ERROR: i32 = 99;
const TEST_NAME: &str = "seccomp_filter_enforces_policy";

#[test]
fn seccomp_filter_enforces_policy() {
    if let Ok(mode) = env::var(CHILD_ENV) {
        run_child(&mode);
    }

    let allowed = spawn_child(CHILD_ALLOWED);
    if allowed.code() == Some(SECCOMP_UNAVAILABLE) {
        eprintln!("skipping seccomp integration test: seccomp is unavailable");
        return;
    }
    assert_eq!(
        allowed.code(),
        Some(0),
        "allowed syscall child failed with {allowed:?}"
    );

    let denied = spawn_child(CHILD_DENIED);
    if denied.code() == Some(SECCOMP_UNAVAILABLE) {
        eprintln!("skipping seccomp integration test: seccomp is unavailable");
        return;
    }
    assert_ne!(
        denied.code(),
        Some(FILTER_DID_NOT_KILL),
        "denied mount syscall returned after filter installation"
    );
    assert_eq!(
        denied.signal(),
        Some(libc::SIGSYS),
        "denied syscall child was not terminated by SIGSYS: {denied:?}"
    );
}

fn spawn_child(mode: &str) -> ExitStatus {
    Command::new(env::current_exe().expect("test executable path should be available"))
        .arg("--exact")
        .arg(TEST_NAME)
        .arg("--nocapture")
        .env(CHILD_ENV, mode)
        .status()
        .expect("seccomp child should start")
}

fn run_child(mode: &str) -> ! {
    let plan =
        SeccompPlan::new(FilterMode::Enforce).expect("enforce seccomp filter should compile");

    // SAFETY: This process was re-executed for the child path, and the test invokes apply
    // immediately before exercising the filtered syscall surface.
    match unsafe { plan.apply() } {
        Ok(()) => {}
        Err(errno) if errno == libc::ENOSYS || errno == libc::EINVAL => {
            process::exit(SECCOMP_UNAVAILABLE);
        }
        Err(_) => process::exit(UNEXPECTED_APPLY_ERROR),
    }

    match mode {
        CHILD_ALLOWED => {
            let mut now = libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            };

            // SAFETY: now points to writable storage for a timespec, and clock_gettime is
            // invoked with a valid clock identifier.
            let result = unsafe {
                libc::syscall(libc::SYS_clock_gettime, libc::CLOCK_MONOTONIC, &raw mut now)
            };
            if result != 0 {
                process::exit(ALLOWED_SYSCALL_FAILED);
            }

            process::exit(0);
        }
        CHILD_DENIED => {
            let null_arg: libc::c_ulong = 0;

            // SAFETY: Null arguments are valid for probing mount. The syscall should be
            // intercepted by seccomp before the kernel examines them.
            unsafe {
                libc::syscall(
                    libc::SYS_mount,
                    null_arg,
                    null_arg,
                    null_arg,
                    null_arg,
                    null_arg,
                );
            }

            process::exit(FILTER_DID_NOT_KILL);
        }
        _ => process::exit(UNEXPECTED_APPLY_ERROR),
    }
}
