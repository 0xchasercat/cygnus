//! Minimal PID 1 for a Cygnus cage.
//!
//! The cage execs this program as PID 1 with the target application and its arguments in `argv`.
//! Linux gives PID 1 special signal semantics, so the init forwards termination signals to the
//! application and reaps every child to prevent orphaned processes from accumulating as zombies.
//! Static musl linking for the approximately 50 KiB packaged binary is a packaging-time concern.

use std::env;
use std::ffi::{CString, OsString};
use std::os::unix::ffi::OsStringExt;
use std::process;
use std::ptr;
use std::sync::atomic::{AtomicI32, Ordering};

use nix::errno::Errno;
use nix::libc::{self, c_char, c_int};
use nix::sys::signal::{
    SaFlags, SigAction, SigHandler, SigSet, SigmaskHow, Signal, kill, sigaction, sigprocmask,
};
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use nix::unistd::{ForkResult, Pid, fork};

const FORWARD_SIGNALS: [Signal; 4] = [
    Signal::SIGTERM,
    Signal::SIGINT,
    Signal::SIGQUIT,
    Signal::SIGHUP,
];

static PENDING_SIGNAL: AtomicI32 = AtomicI32::new(0);

struct Command {
    target: OsString,
    args: Vec<OsString>,
}

struct ExecPlan {
    _argv: Vec<CString>,
    target: *const c_char,
    argv_ptrs: Vec<*const c_char>,
}

impl ExecPlan {
    fn new(command: Command) -> Result<Self, std::ffi::NulError> {
        let argv = std::iter::once(command.target)
            .chain(command.args)
            .map(|arg| CString::new(arg.into_vec()))
            .collect::<Result<Vec<_>, _>>()?;
        let mut argv_ptrs = argv.iter().map(|arg| arg.as_ptr()).collect::<Vec<_>>();
        argv_ptrs.push(ptr::null());

        let target = argv[0].as_ptr();
        Ok(Self {
            _argv: argv,
            target,
            argv_ptrs,
        })
    }

    fn exec(&self) -> ! {
        unsafe {
            libc::execvp(self.target, self.argv_ptrs.as_ptr());
            libc::_exit(127);
        }
    }
}

extern "C" fn record_signal(signal: c_int) {
    PENDING_SIGNAL.store(signal, Ordering::Relaxed);
}

extern "C" fn record_child_exit(_: c_int) {}

fn split_command(argv: Vec<OsString>) -> Option<Command> {
    let mut args = argv.into_iter().skip(1);
    let target = args.next()?;
    Some(Command {
        target,
        args: args.collect(),
    })
}

fn child_exit_code(status: WaitStatus) -> Option<i32> {
    match status {
        WaitStatus::Exited(_, code) => Some(code),
        WaitStatus::Signaled(_, signal, _) => Some(128 + signal as i32),
        _ => None,
    }
}

fn handled_signals() -> SigSet {
    let mut signals = SigSet::empty();
    for signal in FORWARD_SIGNALS {
        signals.add(signal);
    }
    signals.add(Signal::SIGCHLD);
    signals
}

fn install_signal_handlers() -> nix::Result<()> {
    let forward = SigAction::new(
        SigHandler::Handler(record_signal),
        SaFlags::empty(),
        SigSet::empty(),
    );
    for signal in FORWARD_SIGNALS {
        unsafe {
            sigaction(signal, &forward)?;
        }
    }

    let child = SigAction::new(
        SigHandler::Handler(record_child_exit),
        SaFlags::empty(),
        SigSet::empty(),
    );
    unsafe {
        sigaction(Signal::SIGCHLD, &child)?;
    }
    Ok(())
}

fn reset_child_signals(old_mask: &SigSet) -> nix::Result<()> {
    let default = SigAction::new(SigHandler::SigDfl, SaFlags::empty(), SigSet::empty());
    for signal in FORWARD_SIGNALS {
        unsafe {
            sigaction(signal, &default)?;
        }
    }
    unsafe {
        sigaction(Signal::SIGCHLD, &default)?;
    }
    sigprocmask(SigmaskHow::SIG_SETMASK, Some(old_mask), None)
}

fn forward_pending_signal(child: Pid) {
    let raw = PENDING_SIGNAL.swap(0, Ordering::Relaxed);
    if raw == 0 {
        return;
    }

    let signal = Signal::try_from(raw).expect("signal handler recorded a known signal");
    if let Err(error) = kill(child, signal)
        && error != Errno::ESRCH
    {
        eprintln!("cygnus-init: failed to forward {signal:?}: {error}");
    }
}

fn reap_children(main_child: Pid, main_status: &mut Option<WaitStatus>) -> nix::Result<()> {
    loop {
        match waitpid(Pid::from_raw(-1), Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::StillAlive) | Err(Errno::ECHILD) => return Ok(()),
            Ok(status) => {
                if status.pid() == Some(main_child) {
                    *main_status = Some(status);
                }
            }
            Err(Errno::EINTR) => continue,
            Err(error) => return Err(error),
        }
    }
}

fn supervise(main_child: Pid) -> i32 {
    let mut main_status = None;
    let wait_mask = SigSet::empty();

    loop {
        forward_pending_signal(main_child);
        if let Err(error) = reap_children(main_child, &mut main_status) {
            eprintln!("cygnus-init: waitpid failed: {error}");
            return 1;
        }

        if let Some(status) = main_status.take() {
            return child_exit_code(status).expect("main child has a terminal wait status");
        }

        // Relevant signals remain blocked while state is inspected. sigsuspend atomically
        // installs the empty mask and sleeps, so a signal cannot arrive between the check and
        // the wait.
        if let Err(error) = wait_mask.suspend() {
            eprintln!("cygnus-init: sigsuspend failed: {error}");
            return 1;
        }
    }
}

fn run(command: Command) -> i32 {
    let exec_plan = match ExecPlan::new(command) {
        Ok(plan) => plan,
        Err(error) => {
            eprintln!("cygnus-init: target arguments contain a null byte: {error}");
            return 2;
        }
    };

    let handled = handled_signals();
    let mut old_mask = SigSet::empty();
    if let Err(error) = sigprocmask(
        SigmaskHow::SIG_BLOCK,
        Some(&handled),
        Some(&mut old_mask),
    ) {
        eprintln!("cygnus-init: failed to block signals: {error}");
        return 1;
    }
    if let Err(error) = install_signal_handlers() {
        eprintln!("cygnus-init: failed to install signal handlers: {error}");
        return 1;
    }

    match unsafe { fork() } {
        Ok(ForkResult::Parent { child }) => supervise(child),
        Ok(ForkResult::Child) => {
            if reset_child_signals(&old_mask).is_err() {
                unsafe {
                    libc::_exit(127);
                }
            }
            exec_plan.exec();
        }
        Err(error) => {
            eprintln!("cygnus-init: failed to fork target: {error}");
            1
        }
    }
}

fn main() {
    let Some(command) = split_command(env::args_os().collect()) else {
        eprintln!("cygnus-init: missing target program");
        process::exit(2);
    };

    process::exit(run(command));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_target_and_arguments() {
        let command = split_command(vec![
            OsString::from("cygnus-init"),
            OsString::from("/app/server"),
            OsString::from("--port"),
            OsString::from("3000"),
        ])
        .expect("target is present");

        assert_eq!(command.target, OsString::from("/app/server"));
        assert_eq!(
            command.args,
            vec![OsString::from("--port"), OsString::from("3000")]
        );
    }

    #[test]
    fn rejects_missing_target() {
        assert!(split_command(vec![OsString::from("cygnus-init")]).is_none());
    }

    #[test]
    fn maps_normal_exit_status() {
        let status = WaitStatus::Exited(Pid::from_raw(42), 7);
        assert_eq!(child_exit_code(status), Some(7));
    }

    #[test]
    fn maps_signal_exit_status() {
        let status = WaitStatus::Signaled(Pid::from_raw(42), Signal::SIGTERM, false);
        assert_eq!(child_exit_code(status), Some(128 + Signal::SIGTERM as i32));
    }
}
