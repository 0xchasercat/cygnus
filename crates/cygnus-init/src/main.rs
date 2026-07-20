//! PID 1 for a Cygnus cage.
//!
//! A cage runs in its own PID namespace, so the first process it execs becomes
//! PID 1 — and PID 1 has special kernel semantics. It must reap orphaned
//! children, or zombies accumulate in the namespace, and it receives no default
//! signal dispositions, so a plain app as PID 1 silently ignores `SIGTERM`.
//! This init sits at PID 1, execs the real app as its child, forwards
//! termination signals to it, and reaps every process that exits in the cage.
//!
//! The cage boot path execs this init with the app as its arguments
//! (`cygnus-init /path/to/app arg...`). Packaging links it against the same
//! glibc hostlib as Bun (loader at `/lib64/ld-linux-*.so.*`); no separate musl
//! static build is required.

use std::env;
use std::ffi::{CString, OsString};
use std::os::unix::ffi::OsStrExt;
use std::process::ExitCode;
use std::sync::atomic::{AtomicI32, Ordering};

use nix::errno::Errno;
use nix::libc;
use nix::sys::signal::{
    SaFlags, SigAction, SigHandler, SigSet, SigmaskHow, Signal, kill, sigaction, sigprocmask,
};
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use nix::unistd::{ForkResult, Pid, execvp, fork};

/// Signals init forwards to the app and, in the app, resets to their defaults.
const FORWARDED: [Signal; 4] = [
    Signal::SIGTERM,
    Signal::SIGINT,
    Signal::SIGQUIT,
    Signal::SIGHUP,
];

/// The last forwardable signal init received, or zero. Written only by the
/// signal handler (an async-signal-safe atomic store) and drained by the loop.
static PENDING: AtomicI32 = AtomicI32::new(0);

extern "C" fn record_signal(signal: libc::c_int) {
    PENDING.store(signal, Ordering::SeqCst);
}

extern "C" fn wake(_signal: libc::c_int) {
    // No state: exists only so `SIGCHLD` delivery interrupts `sigsuspend`
    // instead of being discarded by the default disposition.
}

fn main() -> ExitCode {
    let raw: Vec<OsString> = env::args_os().skip(1).collect();
    let Some((program, arguments)) = split_target(&raw) else {
        eprintln!("cygnus-init: usage: cygnus-init <program> [args...]");
        return ExitCode::from(2);
    };

    let Some(program) = to_cstring(program) else {
        eprintln!("cygnus-init: program path contains an interior NUL byte");
        return ExitCode::from(2);
    };
    let mut argv = Vec::with_capacity(arguments.len() + 1);
    argv.push(program.clone());
    for argument in arguments {
        let Some(argument) = to_cstring(argument) else {
            eprintln!("cygnus-init: argument contains an interior NUL byte");
            return ExitCode::from(2);
        };
        argv.push(argument);
    }

    // Block the forwardable set plus SIGCHLD before forking, so no signal is
    // lost between the fork and the wait loop.
    let mut blocked = SigSet::empty();
    for signal in FORWARDED {
        blocked.add(signal);
    }
    blocked.add(Signal::SIGCHLD);
    if let Err(errno) = sigprocmask(SigmaskHow::SIG_BLOCK, Some(&blocked), None) {
        eprintln!("cygnus-init: failed to block signals: {errno}");
        return ExitCode::from(1);
    }

    if let Err(errno) = install_handlers() {
        eprintln!("cygnus-init: failed to install signal handlers: {errno}");
        return ExitCode::from(1);
    }

    // SAFETY: single-threaded; the child only resets signal state and execs,
    // and the argv storage is built above, before the fork.
    match unsafe { fork() } {
        Ok(ForkResult::Child) => run_child(&program, &argv),
        Ok(ForkResult::Parent { child }) => supervise(child),
        Err(errno) => {
            eprintln!("cygnus-init: fork failed: {errno}");
            ExitCode::from(1)
        }
    }
}

/// Split argv into the program to exec and its arguments.
fn split_target(argv: &[OsString]) -> Option<(&OsString, &[OsString])> {
    argv.split_first()
}

fn to_cstring(value: &OsString) -> Option<CString> {
    CString::new(value.as_bytes()).ok()
}

/// The child restores default signal behaviour, unblocks everything, and execs
/// the app so it sees a pristine signal environment.
fn run_child(program: &CString, argv: &[CString]) -> ! {
    let default = SigAction::new(SigHandler::SigDfl, SaFlags::empty(), SigSet::empty());
    for signal in FORWARDED {
        // SAFETY: restoring the default disposition is always sound.
        let _ = unsafe { sigaction(signal, &default) };
    }
    let _ = sigprocmask(SigmaskHow::SIG_SETMASK, Some(&SigSet::empty()), None);

    let _ = execvp(program, argv);
    // Only reached if execvp failed.
    // SAFETY: _exit is async-signal-safe and terminates the failed child.
    unsafe { libc::_exit(127) }
}

/// PID 1's loop: forward pending signals to the app, reap every exited child,
/// and exit mirroring the app once it terminates.
fn supervise(child: Pid) -> ExitCode {
    loop {
        let pending = PENDING.swap(0, Ordering::SeqCst);
        if pending != 0
            && let Ok(signal) = Signal::try_from(pending)
        {
            let _ = kill(child, signal);
        }

        loop {
            match waitpid(Pid::from_raw(-1), Some(WaitPidFlag::WNOHANG)) {
                Ok(WaitStatus::StillAlive) => break,
                Ok(status) => {
                    if reaped_main(child, &status)
                        && let Some(code) = terminal_code(&status)
                    {
                        return ExitCode::from(code);
                    }
                }
                Err(Errno::ECHILD) => return ExitCode::SUCCESS,
                Err(Errno::EINTR) => continue,
                Err(errno) => {
                    eprintln!("cygnus-init: waitpid failed: {errno}");
                    return ExitCode::from(1);
                }
            }
        }

        // Block until a signal arrives (a forwarded signal or SIGCHLD). The
        // relevant signals are blocked outside this call, so any that arrived
        // since the checks above are pending and are delivered the instant the
        // mask is lifted here — no lost wakeup.
        suspend_until_signal();
    }
}

/// Whether this wait status belongs to the app init forked (as opposed to an
/// orphan it inherited and is reaping).
fn reaped_main(child: Pid, status: &WaitStatus) -> bool {
    status.pid() == Some(child)
}

/// The process-exit code to mirror for a terminal status: the app's own code,
/// or `128 + signal` when it was killed, matching shell convention. Returns
/// `None` for non-terminal (stopped/continued) statuses.
fn terminal_code(status: &WaitStatus) -> Option<u8> {
    match status {
        WaitStatus::Exited(_, code) => Some(*code as u8),
        WaitStatus::Signaled(_, signal, _) => Some(128u8.wrapping_add(*signal as u8)),
        _ => None,
    }
}

fn install_handlers() -> Result<(), Errno> {
    let forward = SigAction::new(
        SigHandler::Handler(record_signal),
        SaFlags::SA_RESTART,
        SigSet::empty(),
    );
    for signal in FORWARDED {
        // SAFETY: the handler only performs an atomic store.
        unsafe { sigaction(signal, &forward)? };
    }
    let child = SigAction::new(
        SigHandler::Handler(wake),
        SaFlags::SA_RESTART,
        SigSet::empty(),
    );
    // SAFETY: the handler is a no-op.
    unsafe { sigaction(Signal::SIGCHLD, &child)? };
    Ok(())
}

/// `sigsuspend` with an empty mask: unblock everything and wait for the next
/// delivered signal, then restore the caller's mask. Always returns via `EINTR`
/// once a handler has run.
fn suspend_until_signal() {
    // SAFETY: `mask` is initialised by `sigemptyset` before `sigsuspend` reads
    // it; `sigsuspend` only ever returns -1 with `EINTR`.
    unsafe {
        let mut mask: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&raw mut mask);
        libc::sigsuspend(&raw const mask);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    #[test]
    fn splits_program_from_arguments() {
        let argv = os(&["/bin/app", "--flag", "value"]);
        let (program, arguments) = split_target(&argv).expect("a program");
        assert_eq!(program, "/bin/app");
        assert_eq!(arguments, &os(&["--flag", "value"])[..]);
    }

    #[test]
    fn split_rejects_an_empty_argv() {
        assert!(split_target(&[]).is_none());
    }

    #[test]
    fn program_with_no_arguments_has_an_empty_tail() {
        let argv = os(&["/bin/app"]);
        let (program, arguments) = split_target(&argv).expect("a program");
        assert_eq!(program, "/bin/app");
        assert!(arguments.is_empty());
    }

    #[test]
    fn exit_code_mirrors_a_normal_exit() {
        assert_eq!(
            terminal_code(&WaitStatus::Exited(Pid::from_raw(2), 0)),
            Some(0)
        );
        assert_eq!(
            terminal_code(&WaitStatus::Exited(Pid::from_raw(2), 42)),
            Some(42)
        );
    }

    #[test]
    fn exit_code_maps_a_signal_death_to_128_plus_signal() {
        assert_eq!(
            terminal_code(&WaitStatus::Signaled(
                Pid::from_raw(2),
                Signal::SIGKILL,
                false
            )),
            Some(128 + libc::SIGKILL as u8)
        );
    }

    #[test]
    fn a_stop_is_not_a_terminal_status() {
        assert_eq!(
            terminal_code(&WaitStatus::Stopped(Pid::from_raw(2), Signal::SIGSTOP)),
            None
        );
    }
}
