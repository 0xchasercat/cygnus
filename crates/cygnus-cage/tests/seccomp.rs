#![cfg(target_os = "linux")]

use std::env;
use std::process::{self, Command, ExitStatus};

use cygnus_cage::seccomp::{FilterMode, SeccompPlan};
use nix::libc;

const CHILD_ENV: &str = "CYGNUS_SECCOMP_CHILD";
const CHILD_ALLOWED: &str = "allowed";
const CHILD_DENIED: &str = "denied";
const SECCOMP_UNAVAILABLE: i32 = 96;
const DENY_NOT_ENFORCED: i32 = 97;
const ALLOWED_SYSCALL_FAILED: i32 = 98;
const UNEXPECTED_APPLY_ERROR: i32 = 99;
const TEST_NAME: &str = "seccomp_denylist_blocks_dangerous_syscalls";

// The Docker-parity denylist blocks a dangerous syscall with EPERM (not a kill)
// and leaves ordinary syscalls untouched. This re-executes the test binary as a
// child, installs the Enforce filter, and checks both halves of that contract.
#[test]
fn seccomp_denylist_blocks_dangerous_syscalls() {
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
        "an allowed syscall failed under the filter: {allowed:?}"
    );

    let denied = spawn_child(CHILD_DENIED);
    if denied.code() == Some(SECCOMP_UNAVAILABLE) {
        eprintln!("skipping seccomp integration test: seccomp is unavailable");
        return;
    }
    assert_eq!(
        denied.code(),
        Some(0),
        "mount was not blocked with EPERM under the filter: {denied:?}"
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

    // SAFETY: this process was re-executed for the child path, and apply is
    // called immediately before exercising the filtered syscall surface.
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
            // SAFETY: `now` is writable storage for a timespec and the clock id
            // is valid. clock_gettime is not on the denylist, so it must work.
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
            // SAFETY: null arguments are valid for probing mount; the filter
            // intercepts the call before the kernel inspects them.
            let result = unsafe {
                libc::syscall(
                    libc::SYS_mount,
                    null_arg,
                    null_arg,
                    null_arg,
                    null_arg,
                    null_arg,
                )
            };
            // Enforce mode denies with EPERM: the call returns -1 and never
            // reaches the kernel's mount implementation.
            let denied_with_eperm =
                result == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
            if denied_with_eperm {
                process::exit(0);
            }
            process::exit(DENY_NOT_ENFORCED);
        }
        _ => process::exit(UNEXPECTED_APPLY_ERROR),
    }
}
