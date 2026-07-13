//! G4 seccomp conformance gate.
//!
//! G4 requires zero filter violations across Bun's test suite and the real-world application
//! corpus. This harness isolates the production filter as the variable under test: it compiles the
//! filter in the parent, then forks, applies it in the single-threaded child immediately before
//! `execve`, and classifies the child's wait status. The filter is defense in depth for
//! operator-adjacent code, not a hostile-multitenancy boundary. Enforce mode is the real gate;
//! Audit mode is a sanity pass that logs violations while allowing the command to continue.
//!
//! The built-in corpus is a smoke set of ubiquitous system binaries. Full G4 conformance requires
//! passing Bun's test suite and the spec §12 application corpus, including Express, SvelteKit SSR,
//! a Postgres client, and a native-addon app, through repeated `--app` arguments. App strings are
//! split on whitespace; quoting and escaping inside a string are not supported.
//!
//! Per-syscall attribution is deferred because it needs audit-log scraping for `SECCOMP_RET_LOG` or
//! a ptrace supervisor. This harness identifies the command that violated the filter, not the
//! syscall. Running the corpus inside the full cage is also deferred: bare fork isolates filter
//! conformance, and this path will unify with `Cage` once the boot path installs the filter. The
//! actual Bun application corpus remains operator-supplied through `--app`.

#[cfg(target_os = "linux")]
mod linux_harness {
    use std::env;
    use std::ffi::{CString, OsString};
    use std::fs;
    use std::process::ExitCode;
    use std::ptr;
    use std::thread;
    use std::time::{Duration, Instant};

    use cygnus_cage::{FilterMode, SeccompPlan};
    use nix::libc;

    const DEFAULT_TIMEOUT_MS: u64 = 10_000;
    const WAIT_POLL_INTERVAL: Duration = Duration::from_millis(1);
    const CHILD_EXEC_FAILED: i32 = 94;
    const CHILD_FILTER_FAILED: i32 = 95;
    const CHILD_SECCOMP_UNAVAILABLE: i32 = 96;

    pub fn main() -> ExitCode {
        match run() {
            Ok(exit_code) => exit_code,
            Err(message) => {
                eprintln!("g4: {message}");
                ExitCode::from(2)
            }
        }
    }

    fn run() -> Result<ExitCode, String> {
        let options = Options::parse(env::args_os().skip(1))?;
        let commands = prepare_commands(options.apps)?;
        if commands.is_empty() {
            return Err("no runnable commands in the corpus".into());
        }
        let plan = SeccompPlan::new(options.mode)
            .map_err(|error| format!("failed to compile the seccomp filter: {error}"))?;

        let mut results = Vec::with_capacity(commands.len());
        for command in &commands {
            results.push(run_command(command, &plan, options.timeout)?);
        }

        print_report(&results, options.mode);
        Ok(gate_exit_code(&results))
    }

    #[derive(Debug, Eq, PartialEq)]
    struct Options {
        mode: FilterMode,
        timeout: Duration,
        apps: Vec<OsString>,
    }

    impl Options {
        fn parse(arguments: impl IntoIterator<Item = OsString>) -> Result<Self, String> {
            let mut mode = FilterMode::Enforce;
            let mut timeout = Duration::from_millis(DEFAULT_TIMEOUT_MS);
            let mut apps = Vec::new();
            let mut arguments = arguments.into_iter();

            while let Some(argument) = arguments.next() {
                if argument == "--help" || argument == "-h" {
                    return Err(usage());
                }
                if argument == "--mode" {
                    let value = arguments
                        .next()
                        .ok_or_else(|| "--mode requires enforce or audit".to_owned())?;
                    mode = match value.to_str() {
                        Some("enforce") => FilterMode::Enforce,
                        Some("audit") => FilterMode::Audit,
                        _ => return Err("--mode requires enforce or audit".into()),
                    };
                    continue;
                }
                if argument == "--app" {
                    let value = arguments
                        .next()
                        .ok_or_else(|| "--app requires a command string".to_owned())?;
                    apps.push(value);
                    continue;
                }
                if argument == "--timeout-ms" {
                    let value = arguments
                        .next()
                        .ok_or_else(|| "--timeout-ms requires a positive integer".to_owned())?;
                    let text = value
                        .to_str()
                        .ok_or_else(|| "--timeout-ms must be valid UTF-8".to_owned())?;
                    let milliseconds = text
                        .parse::<u64>()
                        .map_err(|_| "--timeout-ms requires a positive integer".to_owned())?;
                    if milliseconds == 0 {
                        return Err("--timeout-ms must be greater than zero".into());
                    }
                    timeout = Duration::from_millis(milliseconds);
                    continue;
                }
                return Err(format!("unknown option {:?}\n{}", argument, usage()));
            }

            Ok(Self {
                mode,
                timeout,
                apps,
            })
        }
    }

    fn usage() -> String {
        "usage: g4 [--mode enforce|audit] [--timeout-ms N] [--app 'PROGRAM ARG1 ARG2']...\n\
         --app values are split on whitespace; embedded quoting and escaping are not supported"
            .into()
    }

    struct PreparedCommand {
        display: String,
        program: CString,
        _arguments: Vec<CString>,
        argument_pointers: Vec<*const libc::c_char>,
    }

    impl PreparedCommand {
        fn new(value: &OsString) -> Result<Self, String> {
            let text = value
                .to_str()
                .ok_or_else(|| "--app values must be valid UTF-8".to_owned())?;
            let fields: Vec<_> = text.split_whitespace().collect();
            let program = fields
                .first()
                .ok_or_else(|| "--app requires a non-empty command string".to_owned())?;
            let arguments = fields
                .iter()
                .map(|field| {
                    CString::new(*field)
                        .map_err(|_| format!("--app contains an interior NUL byte: {text:?}"))
                })
                .collect::<Result<Vec<_>, _>>()?;
            let program = CString::new(*program)
                .map_err(|_| format!("--app contains an interior NUL byte: {text:?}"))?;
            let mut argument_pointers = arguments
                .iter()
                .map(|argument| argument.as_ptr())
                .collect::<Vec<_>>();
            argument_pointers.push(ptr::null());

            Ok(Self {
                display: fields.join(" "),
                program,
                _arguments: arguments,
                argument_pointers,
            })
        }
    }

    fn prepare_commands(apps: Vec<OsString>) -> Result<Vec<PreparedCommand>, String> {
        if apps.is_empty() {
            return default_commands()
                .into_iter()
                .filter(|value| {
                    value
                        .to_str()
                        .and_then(|text| text.split_whitespace().next())
                        .is_some_and(|program| fs::metadata(program).is_ok())
                })
                .map(|value| PreparedCommand::new(&value))
                .collect();
        }

        apps.iter().map(PreparedCommand::new).collect()
    }

    fn default_commands() -> [OsString; 3] {
        [
            OsString::from("/bin/true"),
            OsString::from("/bin/echo cygnus-g4"),
            OsString::from("/usr/bin/env"),
        ]
    }

    #[derive(Debug)]
    struct CommandResult {
        command: String,
        outcome: Outcome,
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum Outcome {
        Pass { exit_code: i32 },
        Violation,
        SeccompUnavailable,
        HarnessError { exit_code: i32 },
        OtherSignal { signal: i32 },
        Timeout,
    }

    fn run_command(
        command: &PreparedCommand,
        plan: &SeccompPlan,
        timeout: Duration,
    ) -> Result<CommandResult, String> {
        let child_pid = unsafe { libc::fork() };
        if child_pid < 0 {
            return Err(format!(
                "failed to fork for {:?}: errno {}",
                command.display,
                last_errno()
            ));
        }
        if child_pid == 0 {
            child_exec(command, plan);
        }

        let outcome = wait_for_child(child_pid, timeout)?;
        Ok(CommandResult {
            command: command.display.clone(),
            outcome,
        })
    }

    fn child_exec(command: &PreparedCommand, plan: &SeccompPlan) -> ! {
        let apply_result = unsafe { plan.apply() };
        if let Err(errno) = apply_result {
            let exit_code = if errno == libc::ENOSYS || errno == libc::EINVAL {
                CHILD_SECCOMP_UNAVAILABLE
            } else {
                CHILD_FILTER_FAILED
            };
            unsafe { libc::_exit(exit_code) }
        }

        unsafe {
            libc::execv(command.program.as_ptr(), command.argument_pointers.as_ptr());
            libc::_exit(CHILD_EXEC_FAILED);
        }
    }

    fn wait_for_child(child_pid: libc::pid_t, timeout: Duration) -> Result<Outcome, String> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| "--timeout-ms exceeds the platform clock range".to_owned())?;

        loop {
            let mut status = 0;
            let waited = unsafe { libc::waitpid(child_pid, &raw mut status, libc::WNOHANG) };
            if waited == child_pid {
                return Ok(classify_wait_status(status));
            }
            if waited < 0 {
                let errno = last_errno();
                if errno == libc::EINTR {
                    continue;
                }
                return Err(format!("waitpid failed for child {child_pid}: errno {errno}"));
            }
            if Instant::now() >= deadline {
                kill_and_reap(child_pid)?;
                return Ok(Outcome::Timeout);
            }
            thread::sleep(WAIT_POLL_INTERVAL);
        }
    }

    fn kill_and_reap(child_pid: libc::pid_t) -> Result<(), String> {
        let kill_result = unsafe { libc::kill(child_pid, libc::SIGKILL) };
        if kill_result != 0 {
            let errno = last_errno();
            if errno != libc::ESRCH {
                return Err(format!(
                    "failed to kill timed-out child {child_pid}: errno {errno}"
                ));
            }
        }

        loop {
            let mut status = 0;
            let waited = unsafe { libc::waitpid(child_pid, &raw mut status, 0) };
            if waited == child_pid {
                return Ok(());
            }
            if waited < 0 {
                let errno = last_errno();
                if errno == libc::EINTR {
                    continue;
                }
                return Err(format!(
                    "failed to reap timed-out child {child_pid}: errno {errno}"
                ));
            }
        }
    }

    fn classify_wait_status(status: libc::c_int) -> Outcome {
        if libc::WIFEXITED(status) {
            let exit_code = libc::WEXITSTATUS(status);
            return match exit_code {
                CHILD_SECCOMP_UNAVAILABLE => Outcome::SeccompUnavailable,
                CHILD_EXEC_FAILED | CHILD_FILTER_FAILED => Outcome::HarnessError { exit_code },
                _ => Outcome::Pass { exit_code },
            };
        }
        if libc::WIFSIGNALED(status) {
            let signal = libc::WTERMSIG(status);
            if signal == libc::SIGSYS {
                return Outcome::Violation;
            }
            return Outcome::OtherSignal { signal };
        }
        Outcome::HarnessError { exit_code: -1 }
    }

    fn last_errno() -> i32 {
        std::io::Error::last_os_error()
            .raw_os_error()
            .unwrap_or(libc::EIO)
    }

    fn gate_exit_code(results: &[CommandResult]) -> ExitCode {
        if results
            .iter()
            .any(|result| result.outcome == Outcome::Violation)
        {
            return ExitCode::from(1);
        }
        if results
            .iter()
            .any(|result| matches!(result.outcome, Outcome::Pass { .. }))
        {
            ExitCode::SUCCESS
        } else {
            ExitCode::from(2)
        }
    }

    fn print_report(results: &[CommandResult], mode: FilterMode) {
        println!("G4 seccomp conformance results");
        println!(
            "mode: {}",
            match mode {
                FilterMode::Enforce => "enforce",
                FilterMode::Audit => "audit",
            }
        );
        println!("{:<48} OUTCOME", "COMMAND");
        println!("{}", "-".repeat(80));
        for result in results {
            println!("{:<48} {}", result.command, format_outcome(result.outcome));
        }

        let violations = results
            .iter()
            .filter(|result| result.outcome == Outcome::Violation)
            .count();
        let ran = results
            .iter()
            .filter(|result| matches!(result.outcome, Outcome::Pass { .. }))
            .count();
        let inconclusive = results.len().saturating_sub(violations + ran);

        println!();
        println!("spec gate: zero filter violations across the corpus");
        println!("observed filter violations: {violations}");
        println!("commands run under the filter: {ran}");
        println!("inconclusive commands: {inconclusive}");
        if violations > 0 {
            println!("violating commands:");
            for result in results
                .iter()
                .filter(|result| result.outcome == Outcome::Violation)
            {
                println!("  {}", result.command);
            }
        }
    }

    fn format_outcome(outcome: Outcome) -> String {
        match outcome {
            Outcome::Pass { exit_code: 0 } => "PASS (exit 0)".into(),
            Outcome::Pass { exit_code } => format!("PASS (exit {exit_code}; nonzero app exit)"),
            Outcome::Violation => format!("VIOLATION (signal {} SIGSYS)", libc::SIGSYS),
            Outcome::SeccompUnavailable => {
                format!("INCONCLUSIVE (exit {CHILD_SECCOMP_UNAVAILABLE}; seccomp unavailable)")
            }
            Outcome::HarnessError { exit_code: CHILD_EXEC_FAILED } => {
                format!("INCONCLUSIVE (exit {CHILD_EXEC_FAILED}; exec failed)")
            }
            Outcome::HarnessError {
                exit_code: CHILD_FILTER_FAILED,
            } => format!(
                "INCONCLUSIVE (exit {CHILD_FILTER_FAILED}; filter installation failed)"
            ),
            Outcome::HarnessError { exit_code } => {
                format!("INCONCLUSIVE (unrecognized wait status {exit_code})")
            }
            Outcome::OtherSignal { signal } => {
                format!("INCONCLUSIVE (signal {signal} {})", signal_name(signal))
            }
            Outcome::Timeout => "INCONCLUSIVE (timeout; killed with SIGKILL)".into(),
        }
    }

    fn signal_name(signal: i32) -> &'static str {
        match signal {
            libc::SIGHUP => "SIGHUP",
            libc::SIGINT => "SIGINT",
            libc::SIGQUIT => "SIGQUIT",
            libc::SIGILL => "SIGILL",
            libc::SIGABRT => "SIGABRT",
            libc::SIGFPE => "SIGFPE",
            libc::SIGKILL => "SIGKILL",
            libc::SIGSEGV => "SIGSEGV",
            libc::SIGPIPE => "SIGPIPE",
            libc::SIGALRM => "SIGALRM",
            libc::SIGTERM => "SIGTERM",
            libc::SIGBUS => "SIGBUS",
            libc::SIGSYS => "SIGSYS",
            _ => "unknown signal",
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn os_arguments(values: &[&str]) -> Vec<OsString> {
            values.iter().map(OsString::from).collect()
        }

        #[test]
        fn parses_documented_arguments() {
            let options = Options::parse(os_arguments(&[
                "--mode",
                "audit",
                "--timeout-ms",
                "250",
                "--app",
                "/bin/echo hello",
                "--app",
                "/usr/bin/env node server.js",
            ]))
            .expect("valid command line");

            assert_eq!(options.mode, FilterMode::Audit);
            assert_eq!(options.timeout, Duration::from_millis(250));
            assert_eq!(options.apps.len(), 2);
        }

        #[test]
        fn builds_null_terminated_argv() {
            let command = PreparedCommand::new(&OsString::from("/bin/echo hello"))
                .expect("valid command");

            assert_eq!(command.argument_pointers.len(), 3);
            assert!(command.argument_pointers[2].is_null());
        }

        #[test]
        fn applies_gate_precedence() {
            let pass = CommandResult {
                command: "pass".into(),
                outcome: Outcome::Pass { exit_code: 0 },
            };
            let violation = CommandResult {
                command: "violation".into(),
                outcome: Outcome::Violation,
            };
            let unavailable = CommandResult {
                command: "unavailable".into(),
                outcome: Outcome::SeccompUnavailable,
            };

            assert_eq!(gate_exit_code(&[pass]), ExitCode::SUCCESS);
            assert_eq!(gate_exit_code(&[unavailable]), ExitCode::from(2));
            assert_eq!(gate_exit_code(&[pass, violation]), ExitCode::from(1));
        }
    }
}

#[cfg(target_os = "linux")]
fn main() -> std::process::ExitCode {
    linux_harness::main()
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("g4 conformance requires Linux; the seccomp filter is Linux-only");
    std::process::exit(2);
}
