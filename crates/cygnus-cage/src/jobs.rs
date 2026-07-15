//! Finite, captured cage jobs.
//!
//! Jobs are deliberately separate from the warm server cage: they always have
//! a deadline, bounded output capture, and deterministic teardown before the
//! result is returned.

use crate::Cage;
use crate::error::CageError;
use crate::spec::{BuildOutputSpec, CageSpec, CgroupLimits, EgressMode, FilterMode, RootfsSpec};
use nix::fcntl::{FcntlArg, FdFlag, fcntl};
use nix::unistd::pipe;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::{self, Read};
use std::os::fd::{FromRawFd, IntoRawFd, OwnedFd};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// The terminal outcome of a finite job.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JobExitOutcome {
    /// The process returned a normal exit code.
    Exited(i32),
    /// The process was terminated by a signal (Unix) or an equivalent
    /// platform termination status.
    Signaled(i32),
    /// The deadline elapsed and the cage was killed.
    TimedOut,
    /// A stream or total output cap was reached and the cage was killed.
    OutputLimitExceeded,
}

/// Configuration for one finite captured job.
#[derive(Clone, Debug)]
pub struct JobConfig {
    pub name: String,
    pub command: OsString,
    pub args: Vec<OsString>,
    pub env: BTreeMap<OsString, OsString>,
    pub limits: CgroupLimits,
    pub rootfs: Option<RootfsSpec>,
    pub build_output: Option<BuildOutputSpec>,
    pub working_dir: Option<PathBuf>,
    pub seccomp: Option<FilterMode>,
    pub egress: EgressMode,
    pub init: Option<PathBuf>,
    /// Maximum bytes retained from stdout. Bytes beyond this are discarded,
    /// while the reader continues draining until the process is killed.
    pub stdout_limit: usize,
    /// Maximum bytes retained from stderr.
    pub stderr_limit: usize,
    /// Optional combined stdout+stderr limit.
    pub total_output_limit: Option<usize>,
    pub timeout: Duration,
}

impl JobConfig {
    pub const DEFAULT_OUTPUT_LIMIT: usize = 1024 * 1024;
    pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5 * 60);

    pub fn new(name: impl Into<String>, command: impl Into<OsString>) -> Self {
        Self {
            name: name.into(),
            command: command.into(),
            args: Vec::new(),
            env: BTreeMap::new(),
            limits: CgroupLimits::default(),
            rootfs: None,
            build_output: None,
            working_dir: None,
            seccomp: Some(FilterMode::Enforce),
            egress: EgressMode::None,
            init: None,
            stdout_limit: Self::DEFAULT_OUTPUT_LIMIT,
            stderr_limit: Self::DEFAULT_OUTPUT_LIMIT,
            total_output_limit: None,
            timeout: Self::DEFAULT_TIMEOUT,
        }
    }

    /// Start with the exact command/cage fields from an existing specification.
    pub fn from_cage_spec(spec: CageSpec) -> Self {
        Self {
            name: spec.name,
            command: spec.command,
            args: spec.args,
            env: spec.env,
            limits: spec.limits,
            rootfs: spec.rootfs,
            build_output: spec.build_output,
            working_dir: spec.working_dir,
            seccomp: spec.seccomp,
            egress: spec.egress,
            init: spec.init,
            stdout_limit: Self::DEFAULT_OUTPUT_LIMIT,
            stderr_limit: Self::DEFAULT_OUTPUT_LIMIT,
            total_output_limit: None,
            timeout: Self::DEFAULT_TIMEOUT,
        }
    }

    pub fn with_output_limit(mut self, bytes: usize) -> Self {
        self.total_output_limit = Some(bytes);
        self
    }

    pub fn with_stream_limits(mut self, stdout: usize, stderr: usize) -> Self {
        self.stdout_limit = stdout;
        self.stderr_limit = stderr;
        self
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn validate(&self) -> Result<(), CageError> {
        if self.timeout.is_zero() {
            return Err(CageError::InvalidSpec(
                "job timeout must be greater than zero".into(),
            ));
        }
        if self.stdout_limit == 0 || self.stderr_limit == 0 {
            return Err(CageError::InvalidSpec(
                "job output limits must be greater than zero".into(),
            ));
        }
        if self.total_output_limit == Some(0) {
            return Err(CageError::InvalidSpec(
                "job total output limit must be greater than zero".into(),
            ));
        }
        if self.init.is_some() {
            // The static init is meaningful for long-running server cages but
            // is intentionally not accepted for finite jobs: the direct child
            // is the process whose exit outcome is reported.
            return Err(CageError::InvalidSpec(
                "finite jobs do not support a PID-1 init".into(),
            ));
        }
        self.cage_spec().validate()
    }

    fn cage_spec(&self) -> CageSpec {
        CageSpec {
            name: self.name.clone(),
            command: self.command.clone(),
            args: self.args.clone(),
            env: self.env.clone(),
            limits: self.limits.clone(),
            rootfs: self.rootfs.clone(),
            ingress: None,
            build_output: self.build_output.clone(),
            working_dir: self.working_dir.clone(),
            seccomp: self.seccomp,
            egress: self.egress.clone(),
            init: self.init.clone(),
            readiness_uds: None,
            readiness_timeout: self.timeout,
        }
    }
}

/// Captured output and the terminal outcome for a completed job.
#[derive(Debug)]
pub struct JobResult {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub outcome: JobExitOutcome,
    pub duration: Duration,
}

impl JobResult {
    pub fn success(&self) -> bool {
        matches!(self.outcome, JobExitOutcome::Exited(0))
    }
}

/// Run one finite job, draining stdout and stderr concurrently and always
/// releasing the cage resources before returning.
pub fn run_job(config: JobConfig) -> Result<JobResult, CageError> {
    config.validate()?;
    let started = Instant::now();
    let (stdout_read, stdout_write) = make_pipe("create job stdout pipe")?;
    let (stderr_read, stderr_write) = make_pipe("create job stderr pipe")?;

    let total = Arc::new(AtomicUsize::new(0));
    let exceeded = Arc::new(AtomicBool::new(false));
    let stdout = spawn_reader(
        stdout_read,
        config.stdout_limit,
        config.total_output_limit,
        Arc::clone(&total),
        Arc::clone(&exceeded),
    );
    let stderr = spawn_reader(
        stderr_read,
        config.stderr_limit,
        config.total_output_limit,
        Arc::clone(&total),
        Arc::clone(&exceeded),
    );

    let spec = config.cage_spec();
    let mut cage = match Cage::boot_with_capture(spec, stdout_write, stderr_write) {
        Ok(cage) => cage,
        Err(error) => {
            let _ = stdout.join();
            let _ = stderr.join();
            return Err(error);
        }
    };

    let deadline = started
        .checked_add(config.timeout)
        .ok_or_else(|| CageError::InvalidSpec("job timeout is too large".into()))?;
    let outcome = loop {
        if exceeded.load(Ordering::Acquire) {
            break JobExitOutcome::OutputLimitExceeded;
        }
        if let Some(status) = cage.try_job_status()? {
            break status;
        }
        if Instant::now() >= deadline {
            break JobExitOutcome::TimedOut;
        }
        thread::sleep(Duration::from_millis(1));
    };

    // teardown performs the kill-if-needed and exactly one reap, then removes
    // cgroup, veth, and rootfs resources. It is called on every terminal path.
    let cleanup = cage.teardown();
    let stdout = join_capture(stdout)?;
    let stderr = join_capture(stderr)?;
    cleanup?;
    let outcome = if exceeded.load(Ordering::Acquire) {
        JobExitOutcome::OutputLimitExceeded
    } else {
        outcome
    };
    Ok(JobResult {
        stdout,
        stderr,
        outcome,
        duration: started.elapsed(),
    })
}

fn make_pipe(operation: &'static str) -> Result<(OwnedFd, OwnedFd), CageError> {
    let (read, write) = pipe().map_err(|source| CageError::Spawn {
        operation,
        source: io::Error::from_raw_os_error(source as i32),
    })?;
    fcntl(&read, FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC)).map_err(|source| CageError::Spawn {
        operation,
        source: io::Error::from_raw_os_error(source as i32),
    })?;
    fcntl(&write, FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC)).map_err(|source| CageError::Spawn {
        operation,
        source: io::Error::from_raw_os_error(source as i32),
    })?;
    Ok((read, write))
}

fn spawn_reader(
    fd: OwnedFd,
    stream_limit: usize,
    total_limit: Option<usize>,
    total: Arc<AtomicUsize>,
    exceeded: Arc<AtomicBool>,
) -> JoinHandle<Result<Vec<u8>, io::Error>> {
    thread::spawn(move || {
        // SAFETY: the OwnedFd is transferred to this reader and is not used
        // elsewhere after the spawn call.
        let mut reader = unsafe { std::fs::File::from_raw_fd(fd.into_raw_fd()) };
        let mut output = Vec::with_capacity(stream_limit.min(8192));
        let mut buffer = [0_u8; 8192];
        loop {
            let count = match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => count,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            };
            let stream_remaining = stream_limit.saturating_sub(output.len());
            let desired = count.min(stream_remaining);
            let retain = total_limit
                .map(|limit| reserve_total(&total, limit, desired))
                .unwrap_or(desired);
            output.extend_from_slice(&buffer[..retain]);
            if retain < count {
                exceeded.store(true, Ordering::Release);
            }
        }
        Ok(output)
    })
}

fn reserve_total(total: &AtomicUsize, limit: usize, desired: usize) -> usize {
    let mut current = total.load(Ordering::Acquire);
    loop {
        let remaining = limit.saturating_sub(current);
        let claim = desired.min(remaining);
        match total.compare_exchange_weak(
            current,
            current.saturating_add(claim),
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => return claim,
            Err(observed) => current = observed,
        }
    }
}

fn join_capture(handle: JoinHandle<Result<Vec<u8>, io::Error>>) -> Result<Vec<u8>, CageError> {
    let output = handle
        .join()
        .map_err(|_| CageError::Internal("job output reader panicked"))?;
    output.map_err(|source| CageError::Spawn {
        operation: "capture job output",
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(not(target_os = "linux"))]
    use std::fs;

    #[test]
    fn defaults_to_a_finite_isolated_job() {
        let config = JobConfig::new("build", "/bin/true");
        assert_eq!(config.seccomp, Some(FilterMode::Enforce));
        assert_eq!(config.egress, EgressMode::None);
        assert_eq!(config.timeout, JobConfig::DEFAULT_TIMEOUT);
        assert!(config.validate().is_ok());
    }

    #[cfg(not(target_os = "linux"))]
    fn shell(script: &str) -> JobConfig {
        let mut config = JobConfig::new("test-job", "/bin/sh");
        config.args = vec![OsString::from("-c"), OsString::from(script)];
        config.timeout = Duration::from_secs(3);
        config
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn captures_success_and_nonzero_exit() {
        let result = run_job(shell("printf out; printf err >&2")).expect("run success job");
        assert_eq!(result.outcome, JobExitOutcome::Exited(0));
        assert_eq!(result.stdout, b"out");
        assert_eq!(result.stderr, b"err");

        let result = run_job(shell("printf bad >&2; exit 7")).expect("run nonzero job");
        assert_eq!(result.outcome, JobExitOutcome::Exited(7));
        assert_eq!(result.stderr, b"bad");
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn drains_stdout_and_stderr_without_pipe_deadlock() {
        let result = run_job(shell("yes o | head -c 131072; yes e | head -c 131072 >&2"))
            .expect("run large-output job");
        assert_eq!(result.outcome, JobExitOutcome::Exited(0));
        assert_eq!(result.stdout.len(), 131_072);
        assert_eq!(result.stderr.len(), 131_072);
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn enforces_output_limit_and_timeout() {
        let mut output = shell("yes output");
        output.stdout_limit = 4096;
        output.stderr_limit = 4096;
        let result = run_job(output).expect("run output-limited job");
        assert_eq!(result.outcome, JobExitOutcome::OutputLimitExceeded);
        assert!(result.stdout.len() <= 4096);

        let result = run_job(shell("sleep 30").with_timeout(Duration::from_millis(30)))
            .expect("run timed-out job");
        assert_eq!(result.outcome, JobExitOutcome::TimedOut);
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn applies_working_directory() {
        let directory = std::env::temp_dir().join(format!(
            "cygnus-job-cwd-{}-{}",
            std::process::id(),
            Instant::now().elapsed().as_nanos()
        ));
        fs::create_dir_all(&directory).expect("create cwd");
        let mut config = shell("pwd");
        config.working_dir = Some(directory.clone());
        let result = run_job(config).expect("run cwd job");
        assert_eq!(result.outcome, JobExitOutcome::Exited(0));
        assert_eq!(
            PathBuf::from(String::from_utf8_lossy(&result.stdout).trim()),
            fs::canonicalize(&directory).expect("canonical cwd"),
        );
        fs::remove_dir_all(directory).expect("remove cwd");
    }
}
