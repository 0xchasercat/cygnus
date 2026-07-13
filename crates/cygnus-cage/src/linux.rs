use std::ffi::{CString, OsStr, OsString};
use std::fs;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use nix::errno::Errno;
use nix::fcntl::{FcntlArg, OFlag, fcntl};
use nix::sched::{CloneFlags, clone};
use nix::sys::signal::{Signal, kill};
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use nix::unistd::{Pid, getegid, geteuid, pipe2, read, write};

use crate::error::CageError;
use crate::mount::{MountPlan, StagedRootfs};
use crate::seccomp::SeccompPlan;
use crate::net;
use crate::spec::{BootTimings, CageSpec, CgroupLimits, EgressMode};

const CLONE_STACK_SIZE: usize = 1024 * 1024;
const CYGNUS_CGROUP: &str = "cygnus";
const CHILD_RELEASE: u8 = 1;
const CHILD_ABORT: u8 = 2;
const CHILD_ERROR_LEN: usize = 5;
const CHILD_STAGE_RELEASE: u8 = 1;
const CHILD_STAGE_MOUNT: u8 = 2;
const CHILD_STAGE_SECCOMP: u8 = 3;
const CHILD_STAGE_EXEC: u8 = 4;
const POLL_INTERVAL: Duration = Duration::from_millis(1);

/// A running cage and the measurements captured while it booted.
#[derive(Debug)]
pub struct Cage {
    pid: Option<Pid>,
    cgroup: Option<Cgroup>,
    staging: Option<StagedRootfs>,
    veth: Option<String>,
    timings: BootTimings,
}

impl Cage {
    /// Boot a target in fresh user, mount, PID, UTS, IPC, and network
    /// namespaces, with a private mount tree, an optional overlay root the
    /// cage pivots into, a `procfs` bound to the cage's own PID namespace, and
    /// an optional seccomp filter installed immediately before `execve`.
    pub fn boot(spec: CageSpec) -> Result<Self, CageError> {
        spec.validate()?;
        let child_exec = ChildExec::new(&spec)?;
        // Parent-side prework: the staging directory, every mount C string, and
        // the compiled seccomp program are built before the clock starts and
        // the clone happens, so the child only fires raw syscalls on prebuilt
        // data. A failure past this point drops the staging directory.
        let mut staging = match &spec.rootfs {
            Some(rootfs) => Some(StagedRootfs::create(&spec.name, rootfs)?),
            None => None,
        };
        let mount_plan = MountPlan::new(staging.as_ref())?;
        let seccomp_plan = match spec.seccomp {
            Some(mode) => Some(
                SeccompPlan::new(mode)
                    .map_err(|source| CageError::SeccompFilter(source.to_string()))?,
            ),
            None => None,
        };
        let boot_started = Instant::now();

        let (release_read, release_write) = pipe2(OFlag::O_CLOEXEC)
            .map_err(|source| CageError::nix("create parent release pipe", source))?;
        let (exec_read, exec_write) = pipe2(OFlag::O_CLOEXEC)
            .map_err(|source| CageError::nix("create exec status pipe", source))?;
        let (mount_read, mount_write) = pipe2(OFlag::O_CLOEXEC)
            .map_err(|source| CageError::nix("create mount status pipe", source))?;
        let (seccomp_read, seccomp_write) = pipe2(OFlag::O_CLOEXEC)
            .map_err(|source| CageError::nix("create seccomp status pipe", source))?;
        fcntl(&exec_read, FcntlArg::F_SETFL(OFlag::O_NONBLOCK))
            .map_err(|source| CageError::nix("make exec status pipe nonblocking", source))?;
        fcntl(&mount_read, FcntlArg::F_SETFL(OFlag::O_NONBLOCK))
            .map_err(|source| CageError::nix("make mount status pipe nonblocking", source))?;
        fcntl(&seccomp_read, FcntlArg::F_SETFL(OFlag::O_NONBLOCK))
            .map_err(|source| CageError::nix("make seccomp status pipe nonblocking", source))?;

        let parent_fds = ParentFds {
            release_write: release_write.as_raw_fd(),
            exec_read: exec_read.as_raw_fd(),
            mount_read: mount_read.as_raw_fd(),
            seccomp_read: seccomp_read.as_raw_fd(),
        };
        let mut stack = vec![0_u8; CLONE_STACK_SIZE];
        let mut child_exec = Some(child_exec);
        let mut child_plan = Some(ChildPlan {
            mounts: mount_plan,
            seccomp: seccomp_plan,
        });
        let mut channels = Some(ChildChannels {
            release_read,
            exec_write,
            mount_write,
            seccomp_write,
        });
        let callback = Box::new(move || {
            let child = child_exec
                .take()
                .expect("clone callback invoked more than once");
            let plan = child_plan
                .take()
                .expect("clone callback invoked more than once");
            let channels = channels
                .take()
                .expect("clone callback invoked more than once");
            child_main(child, plan, channels, parent_fds)
        });
        let flags = CloneFlags::CLONE_NEWUSER
            | CloneFlags::CLONE_NEWNS
            | CloneFlags::CLONE_NEWPID
            | CloneFlags::CLONE_NEWUTS
            | CloneFlags::CLONE_NEWIPC
            | CloneFlags::CLONE_NEWNET;
        // SAFETY: `stack` remains alive in the parent until clone returns. The
        // child callback performs only async-signal-safe descriptor I/O and
        // raw mount and seccomp syscalls before replacing itself with the
        // target process.
        let pid = unsafe { clone(callback, &mut stack, flags, Some(nix::libc::SIGCHLD)) }
            .map_err(|source| CageError::NamespaceUnavailable { source })?;

        let mut guard = BootGuard::new(pid);
        if let Err(error) = write_identity_maps(pid) {
            let _ = write_all_fd(&release_write, &[CHILD_ABORT]);
            return Err(error);
        }

        let cgroup = match Cgroup::create(&spec.name, &spec.limits, pid) {
            Ok(cgroup) => cgroup,
            Err(error) => {
                let _ = write_all_fd(&release_write, &[CHILD_ABORT]);
                return Err(error);
            }
        };
        guard.cgroup = Some(cgroup);
        let namespaces_cgroup = boot_started.elapsed();

        // Egress fabric: the veth onto the bridge and the per-cage nftables
        // policy, configured while the child is parked so the network is ready
        // before it execs. `guard.veth` is recorded first, so any failure past
        // this point tears the interface down on the way out.
        let network_started = Instant::now();
        if !matches!(spec.egress, EgressMode::None) {
            guard.veth = Some(net::host_veth_name(&spec.name));
            let cage_ip = net::cage_ipv4(&spec.name);
            let configured = net::configure_cage(&spec.name, cage_ip, pid.as_raw(), &spec.egress);
            if let Err(error) = configured {
                let _ = write_all_fd(&release_write, &[CHILD_ABORT]);
                return Err(error);
            }
        }
        let network = network_started.elapsed();

        if let Err(source) = write_all_fd(&release_write, &[CHILD_RELEASE]) {
            return Err(CageError::nix("release cage child", source));
        }
        drop(release_write);

        let deadline = Instant::now()
            .checked_add(spec.readiness_timeout)
            .ok_or_else(|| CageError::InvalidSpec("readiness_timeout is too large".into()))?;

        let mounts_started = Instant::now();
        wait_for_child_stage(
            &mount_read,
            pid,
            deadline,
            spec.readiness_timeout,
            "filesystem setup",
        )?;
        let mounts = mounts_started.elapsed();

        let seccomp_started = Instant::now();
        wait_for_child_stage(
            &seccomp_read,
            pid,
            deadline,
            spec.readiness_timeout,
            "seccomp filter",
        )?;
        let seccomp = seccomp_started.elapsed();

        let exec_started = Instant::now();
        wait_for_child_stage(
            &exec_read,
            pid,
            deadline,
            spec.readiness_timeout,
            "execve completion",
        )?;
        let exec_runtime_init = exec_started.elapsed();

        let socket_ready = if let Some(path) = &spec.readiness_uds {
            let socket_started = Instant::now();
            wait_for_socket(path, pid, deadline, spec.readiness_timeout)?;
            socket_started.elapsed()
        } else {
            Duration::ZERO
        };

        let timings = BootTimings {
            namespaces_cgroup,
            network,
            mounts,
            seccomp,
            exec_runtime_init,
            socket_ready,
            total: boot_started.elapsed(),
        };
        let mut cage = guard.finish(timings)?;
        cage.staging = staging.take();
        Ok(cage)
    }

    /// Return the completed cold-start phase timings.
    pub const fn timings(&self) -> BootTimings {
        self.timings
    }

    /// Return the target's PID as seen by the host.
    pub fn host_pid(&self) -> Option<i32> {
        self.pid.map(Pid::as_raw)
    }

    /// Return the cage's cgroup v2 path.
    pub fn cgroup_path(&self) -> Option<&Path> {
        self.cgroup.as_ref().map(|cgroup| cgroup.path.as_path())
    }

    /// Kill the target, reap it, and remove its cgroup and rootfs staging.
    pub fn teardown(mut self) -> Result<(), CageError> {
        self.cleanup()
    }

    fn cleanup(&mut self) -> Result<(), CageError> {
        let mut first_error = None;

        if let Some(pid) = self.pid {
            if let Err(source) = kill(pid, Signal::SIGKILL)
                && source != Errno::ESRCH
            {
                first_error = Some(CageError::Signal { pid, source });
            }
            match reap(pid) {
                Ok(()) | Err(Errno::ECHILD) => self.pid = None,
                Err(source) => {
                    if first_error.is_none() {
                        first_error = Some(CageError::Wait { pid, source });
                    }
                }
            }
        }

        if let Some(cgroup) = &mut self.cgroup {
            if let Err(error) = cgroup.remove()
                && first_error.is_none()
            {
                first_error = Some(error);
            }
            if cgroup.removed {
                self.cgroup = None;
            }
        }

        if let Some(staging) = &mut self.staging {
            match staging.remove() {
                Ok(()) => self.staging = None,
                Err(error) => {
                    if first_error.is_none() {
                        first_error = Some(error);
                    }
                }
            }
        }

        // The cage netns is gone once the process is reaped, which usually
        // takes the veth pair with it; deleting the host end is tolerant of an
        // already-absent device.
        if let Some(veth) = &self.veth {
            match net::delete_veth(veth) {
                Ok(()) => self.veth = None,
                Err(error) => {
                    if first_error.is_none() {
                        first_error = Some(error);
                    }
                }
            }
        }

        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

impl Drop for Cage {
    fn drop(&mut self) {
        let _ = self.cleanup();
    }
}

/// Parent-owned pipe ends, duplicated into the child by `clone`, that the
/// child closes immediately so only the daemon holds them.
#[derive(Clone, Copy)]
struct ParentFds {
    release_write: i32,
    exec_read: i32,
    mount_read: i32,
    seccomp_read: i32,
}

/// Child-owned pipe ends used to receive the release signal and report the
/// outcome of the mount, seccomp, and exec stages.
struct ChildChannels {
    release_read: OwnedFd,
    exec_write: OwnedFd,
    mount_write: OwnedFd,
    seccomp_write: OwnedFd,
}

/// The prebuilt setup steps the child applies between clone and `execve`.
struct ChildPlan {
    mounts: MountPlan,
    seccomp: Option<SeccompPlan>,
}

#[derive(Debug)]
struct ChildExec {
    command: CString,
    args: Vec<CString>,
    env: Vec<CString>,
    argv: Vec<*const nix::libc::c_char>,
    envp: Vec<*const nix::libc::c_char>,
}

impl ChildExec {
    fn new(spec: &CageSpec) -> Result<Self, CageError> {
        let (program, argv_os) = exec_plan(spec)?;
        let command = cstring(&program, "cage command contains a NUL byte")?;
        let mut args = Vec::with_capacity(argv_os.len());
        for arg in &argv_os {
            args.push(cstring(arg, "cage argument contains a NUL byte")?);
        }

        let mut env = Vec::with_capacity(spec.env.len());
        for (key, value) in &spec.env {
            let mut entry = OsString::with_capacity(key.len() + value.len() + 1);
            entry.push(key);
            entry.push("=");
            entry.push(value);
            env.push(cstring(&entry, "environment entry contains a NUL byte")?);
        }

        let mut argv: Vec<_> = args.iter().map(|arg| arg.as_ptr()).collect();
        argv.push(std::ptr::null());
        let mut envp: Vec<_> = env.iter().map(|entry| entry.as_ptr()).collect();
        envp.push(std::ptr::null());

        Ok(Self {
            command,
            args,
            env,
            argv,
            envp,
        })
    }
}

/// Decide the program to exec as the cage's first process and its full argv.
///
/// With `spec.init` set, the cage execs that init binary as PID 1 and passes
/// the target command and its arguments as the init's arguments; the init then
/// execs the target itself, reaping descendants and forwarding signals. Without
/// it, the cage execs the target directly (argv[0] is the resolved program
/// path) and the target is PID 1. The init path is exec'd verbatim, so it must
/// resolve inside the cage's filesystem view; the target is resolved by the
/// init (via `PATH`) when an init is used, and here otherwise.
fn exec_plan(spec: &CageSpec) -> Result<(OsString, Vec<OsString>), CageError> {
    match &spec.init {
        Some(init) => {
            let program = init.as_os_str().to_os_string();
            let mut argv = Vec::with_capacity(spec.args.len() + 2);
            argv.push(program.clone());
            argv.push(spec.command.clone());
            argv.extend(spec.args.iter().cloned());
            Ok((program, argv))
        }
        None => {
            let program = resolve_command(spec)?;
            let mut argv = Vec::with_capacity(spec.args.len() + 1);
            argv.push(program.clone());
            argv.extend(spec.args.iter().cloned());
            Ok((program, argv))
        }
    }
}

fn resolve_command(spec: &CageSpec) -> Result<OsString, CageError> {
    if spec.command.as_bytes().contains(&b'/') {
        return Ok(spec.command.clone());
    }

    let path = spec
        .env
        .get(OsStr::new("PATH"))
        .cloned()
        .or_else(|| std::env::var_os("PATH"))
        .ok_or_else(|| CageError::InvalidSpec("command has no slash and PATH is not set".into()))?;
    for directory in std::env::split_paths(&path) {
        let candidate = directory.join(&spec.command);
        let Ok(metadata) = fs::metadata(&candidate) else {
            continue;
        };
        if metadata.is_file() && metadata.permissions().mode() & 0o111 != 0 {
            return Ok(candidate.into_os_string());
        }
    }

    Err(CageError::InvalidSpec(format!(
        "command {:?} was not found in PATH",
        spec.command
    )))
}

fn cstring(value: &OsStr, message: &'static str) -> Result<CString, CageError> {
    CString::new(value.as_bytes()).map_err(|_| CageError::InvalidSpec(message.into()))
}

fn child_main(
    child: ChildExec,
    plan: ChildPlan,
    channels: ChildChannels,
    parent_fds: ParentFds,
) -> isize {
    // SAFETY: `clone` copies every open descriptor even though these
    // parent-owned ends are not captured by the callback closure. The raw
    // descriptors remain valid in the child until explicitly closed here.
    unsafe {
        nix::libc::close(parent_fds.release_write);
        nix::libc::close(parent_fds.exec_read);
        nix::libc::close(parent_fds.mount_read);
        nix::libc::close(parent_fds.seccomp_read);
    }

    let ChildChannels {
        release_read,
        exec_write,
        mount_write,
        seccomp_write,
    } = channels;

    // Release-stage failures are reported on the mount pipe, which the parent
    // reads first.
    let mut release = [0_u8; 1];
    loop {
        match read(&release_read, &mut release) {
            Ok(1) if release[0] == CHILD_RELEASE => break,
            Ok(1) if release[0] == CHILD_ABORT => {
                write_child_error(&mount_write, CHILD_STAGE_RELEASE, nix::libc::ECANCELED);
                return 127;
            }
            Ok(0) => {
                write_child_error(&mount_write, CHILD_STAGE_RELEASE, nix::libc::EPIPE);
                return 127;
            }
            Ok(_) => {
                write_child_error(&mount_write, CHILD_STAGE_RELEASE, nix::libc::EPROTO);
                return 127;
            }
            Err(Errno::EINTR) => continue,
            Err(source) => {
                write_child_error(&mount_write, CHILD_STAGE_RELEASE, source as i32);
                return 127;
            }
        }
    }
    drop(release_read);

    // SAFETY: the maps are set, we hold CAP_SYS_ADMIN in the new user and mount
    // namespaces, and the plan touches only prebuilt pointers via raw syscalls.
    if let Err(errno) = unsafe { plan.mounts.apply() } {
        write_child_error(&mount_write, CHILD_STAGE_MOUNT, errno);
        return 127;
    }
    // Closing the mount pipe signals the parent that mounts are ready.
    drop(mount_write);

    // Seccomp installs after mounts and immediately before execve, so the
    // mount family the filter denies is already spent. With no filter the
    // stage is a no-op and the pipe closes at once.
    if let Some(seccomp) = &plan.seccomp {
        // SAFETY: the child is single-threaded here, mounts are complete, and
        // apply issues only raw syscalls over the program compiled in the
        // parent. No allocation or lock acquisition occurs.
        if let Err(errno) = unsafe { seccomp.apply() } {
            write_child_error(&seccomp_write, CHILD_STAGE_SECCOMP, errno);
            return 127;
        }
    }
    // Closing the seccomp pipe signals the parent that the filter is installed.
    drop(seccomp_write);

    // Keep the CString storage alive while the pointer arrays are passed to
    // libc. No allocation or lock acquisition occurs after clone.
    let _storage = (&child.args, &child.env);
    // This exec makes the target (or the init shim, when the spec configures
    // one) the cage's PID 1. With an init the shim reaps descendants and
    // forwards signals; without one the target runs as PID 1 directly.
    // SAFETY: both pointer arrays are null-terminated and point into the
    // CString storage kept alive above. `execve` does not retain the pointers.
    unsafe {
        nix::libc::execve(
            child.command.as_ptr(),
            child.argv.as_ptr(),
            child.envp.as_ptr(),
        );
    }
    let source = Errno::last_raw();
    write_child_error(&exec_write, CHILD_STAGE_EXEC, source);
    127
}

fn write_child_error(fd: &OwnedFd, stage: u8, errno: i32) {
    let mut message = [0_u8; CHILD_ERROR_LEN];
    message[0] = stage;
    message[1..].copy_from_slice(&errno.to_ne_bytes());
    let _ = write_all_fd(fd, &message);
}

fn write_all_fd(fd: &OwnedFd, mut bytes: &[u8]) -> Result<(), Errno> {
    while !bytes.is_empty() {
        match write(fd, bytes) {
            Ok(0) => return Err(Errno::EPIPE),
            Ok(written) => bytes = &bytes[written..],
            Err(Errno::EINTR) => continue,
            Err(source) => return Err(source),
        }
    }
    Ok(())
}

fn write_identity_maps(pid: Pid) -> Result<(), CageError> {
    let proc_dir = PathBuf::from(format!("/proc/{pid}"));
    let setgroups = proc_dir.join("setgroups");
    write_file(&setgroups, b"deny\n", "deny setgroups for user namespace")?;

    let uid_map = proc_dir.join("uid_map");
    let uid = geteuid().as_raw();
    write_file(
        &uid_map,
        format!("0 {uid} 1\n").as_bytes(),
        "write user namespace UID map",
    )?;

    let gid_map = proc_dir.join("gid_map");
    let gid = getegid().as_raw();
    write_file(
        &gid_map,
        format!("0 {gid} 1\n").as_bytes(),
        "write user namespace GID map",
    )
}

fn write_file(path: &Path, bytes: &[u8], operation: &'static str) -> Result<(), CageError> {
    fs::write(path, bytes).map_err(|source| CageError::io(operation, path, source))
}

#[derive(Debug)]
struct Cgroup {
    path: PathBuf,
    removed: bool,
}

impl Cgroup {
    fn create(name: &str, limits: &CgroupLimits, pid: Pid) -> Result<Self, CageError> {
        let root = cgroup2_mount()?;
        require_controllers(&root)?;
        enable_controllers(&root)?;

        let parent = root.join(CYGNUS_CGROUP);
        fs::create_dir_all(&parent)
            .map_err(|source| CageError::io("create Cygnus cgroup", &parent, source))?;
        require_controllers(&parent)?;
        enable_controllers(&parent)?;

        let path = cgroup_path(&root, name);
        match fs::create_dir(&path) {
            Ok(()) => {}
            Err(source) if source.kind() == io::ErrorKind::AlreadyExists => {
                return Err(CageError::CgroupExists(path));
            }
            Err(source) => {
                return Err(CageError::io("create cage cgroup", path, source));
            }
        }

        let result = (|| {
            write_file(
                &path.join("memory.max"),
                limits.memory_max.to_string().as_bytes(),
                "set cgroup memory.max",
            )?;
            write_file(
                &path.join("memory.high"),
                limits.memory_high.to_string().as_bytes(),
                "set cgroup memory.high",
            )?;
            write_file(
                &path.join("cpu.max"),
                format!("{} {}", limits.cpu_quota, limits.cpu_period).as_bytes(),
                "set cgroup cpu.max",
            )?;
            write_file(
                &path.join("pids.max"),
                limits.pids_max.to_string().as_bytes(),
                "set cgroup pids.max",
            )?;
            write_file(
                &path.join("cgroup.procs"),
                pid.as_raw().to_string().as_bytes(),
                "move cage child into cgroup",
            )?;
            Ok(())
        })();

        if let Err(error) = result {
            let _ = fs::remove_dir(&path);
            return Err(error);
        }

        Ok(Self {
            path,
            removed: false,
        })
    }

    fn remove(&mut self) -> Result<(), CageError> {
        if self.removed {
            return Ok(());
        }
        match fs::remove_dir(&self.path) {
            Ok(()) => {
                self.removed = true;
                Ok(())
            }
            Err(source) if source.kind() == io::ErrorKind::NotFound => {
                self.removed = true;
                Ok(())
            }
            Err(source) => Err(CageError::io("remove cage cgroup", &self.path, source)),
        }
    }
}

fn cgroup2_mount() -> Result<PathBuf, CageError> {
    let mountinfo_path = Path::new("/proc/self/mountinfo");
    let mountinfo = fs::read_to_string(mountinfo_path)
        .map_err(|source| CageError::io("read process mount table", mountinfo_path, source))?;
    for line in mountinfo.lines() {
        let Some((before_separator, after_separator)) = line.split_once(" - ") else {
            continue;
        };
        if after_separator.split_whitespace().next() != Some("cgroup2") {
            continue;
        }
        let Some(encoded_mount) = before_separator.split_whitespace().nth(4) else {
            continue;
        };
        return Ok(PathBuf::from(unescape_mount_field(encoded_mount)));
    }
    Err(CageError::CgroupUnavailable(
        "no cgroup2 mount appears in /proc/self/mountinfo".into(),
    ))
}

fn unescape_mount_field(field: &str) -> String {
    field
        .replace("\\040", " ")
        .replace("\\011", "\t")
        .replace("\\012", "\n")
        .replace("\\134", "\\")
}

fn require_controllers(path: &Path) -> Result<(), CageError> {
    let controllers_path = path.join("cgroup.controllers");
    let controllers = fs::read_to_string(&controllers_path)
        .map_err(|source| CageError::io("read cgroup controllers", controllers_path, source))?;
    for required in ["cpu", "memory", "pids"] {
        if !controllers.split_whitespace().any(|item| item == required) {
            return Err(CageError::CgroupControllerUnavailable(required));
        }
    }
    Ok(())
}

fn enable_controllers(path: &Path) -> Result<(), CageError> {
    let subtree_control = path.join("cgroup.subtree_control");
    write_file(
        &subtree_control,
        b"+cpu +memory +pids\n",
        "enable cgroup controllers",
    )
}

pub(crate) fn cgroup_path(root: &Path, name: &str) -> PathBuf {
    root.join(CYGNUS_CGROUP).join(name)
}

fn wait_for_child_stage(
    fd: &OwnedFd,
    pid: Pid,
    deadline: Instant,
    timeout: Duration,
    phase: &'static str,
) -> Result<(), CageError> {
    let mut message = [0_u8; CHILD_ERROR_LEN];
    let mut received = 0;

    loop {
        match read(fd, &mut message[received..]) {
            Ok(0) if received == 0 => return Ok(()),
            Ok(0) => return Err(CageError::MalformedChildStatus),
            Ok(count) => {
                received += count;
                if received == CHILD_ERROR_LEN {
                    let errno = i32::from_ne_bytes(
                        message[1..]
                            .try_into()
                            .map_err(|_| CageError::MalformedChildStatus)?,
                    );
                    let stage = match message[0] {
                        CHILD_STAGE_RELEASE => "parent release",
                        CHILD_STAGE_MOUNT => "filesystem setup",
                        CHILD_STAGE_SECCOMP => "seccomp filter",
                        CHILD_STAGE_EXEC => "execve",
                        _ => return Err(CageError::MalformedChildStatus),
                    };
                    return Err(CageError::ChildSetup { stage, errno });
                }
            }
            Err(Errno::EAGAIN) => {
                if let Some(status) = child_status(pid)? {
                    return Err(CageError::ChildExited(format!("{status:?}")));
                }
                if Instant::now() >= deadline {
                    return Err(CageError::ReadinessTimeout { phase, timeout });
                }
                thread::sleep(POLL_INTERVAL);
            }
            Err(Errno::EINTR) => continue,
            Err(source) => return Err(CageError::nix("read child status pipe", source)),
        }
    }
}

fn wait_for_socket(
    path: &Path,
    pid: Pid,
    deadline: Instant,
    timeout: Duration,
) -> Result<(), CageError> {
    loop {
        match UnixStream::connect(path) {
            Ok(stream) => {
                drop(stream);
                return Ok(());
            }
            Err(source) if retry_socket_error(&source) => {
                if let Some(status) = child_status(pid)? {
                    return Err(CageError::ChildExited(format!("{status:?}")));
                }
                if Instant::now() >= deadline {
                    return Err(CageError::ReadinessTimeout {
                        phase: "readiness socket",
                        timeout,
                    });
                }
                thread::sleep(POLL_INTERVAL);
            }
            Err(source) => {
                return Err(CageError::ReadinessSocket {
                    path: path.to_path_buf(),
                    source,
                });
            }
        }
    }
}

fn retry_socket_error(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::NotFound
            | io::ErrorKind::ConnectionRefused
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::WouldBlock
            | io::ErrorKind::Interrupted
    )
}

fn child_status(pid: Pid) -> Result<Option<WaitStatus>, CageError> {
    loop {
        match waitpid(pid, Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::StillAlive) => return Ok(None),
            Ok(status) => return Ok(Some(status)),
            Err(Errno::EINTR) => continue,
            Err(source) => return Err(CageError::Wait { pid, source }),
        }
    }
}

fn reap(pid: Pid) -> Result<(), Errno> {
    loop {
        match waitpid(pid, None) {
            Ok(_) => return Ok(()),
            Err(Errno::EINTR) => continue,
            Err(source) => return Err(source),
        }
    }
}

#[derive(Debug)]
struct BootGuard {
    pid: Option<Pid>,
    cgroup: Option<Cgroup>,
    veth: Option<String>,
}

impl BootGuard {
    fn new(pid: Pid) -> Self {
        Self {
            pid: Some(pid),
            cgroup: None,
            veth: None,
        }
    }

    fn finish(mut self, timings: BootTimings) -> Result<Cage, CageError> {
        let pid = self
            .pid
            .take()
            .ok_or(CageError::Internal("missing process ID"))?;
        let cgroup = self
            .cgroup
            .take()
            .ok_or(CageError::Internal("missing cgroup"))?;
        Ok(Cage {
            pid: Some(pid),
            cgroup: Some(cgroup),
            staging: None,
            veth: self.veth.take(),
            timings,
        })
    }
}

impl Drop for BootGuard {
    fn drop(&mut self) {
        if let Some(pid) = self.pid.take() {
            let _ = kill(pid, Signal::SIGKILL);
            let _ = reap(pid);
        }
        if let Some(cgroup) = &mut self.cgroup {
            let _ = cgroup.remove();
        }
        if let Some(veth) = &self.veth {
            let _ = net::delete_veth(veth);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cgroup_path_is_confined_to_the_cygnus_subtree() {
        assert_eq!(
            cgroup_path(Path::new("/sys/fs/cgroup"), "app-1"),
            PathBuf::from("/sys/fs/cgroup/cygnus/app-1")
        );
    }

    #[test]
    fn mountinfo_fields_are_unescaped() {
        assert_eq!(
            unescape_mount_field("/sys/fs/cgroup\\040unified\\134slice"),
            "/sys/fs/cgroup unified\\slice"
        );
    }

    #[test]
    fn child_stage_codes_are_distinct() {
        let codes = [
            CHILD_STAGE_RELEASE,
            CHILD_STAGE_MOUNT,
            CHILD_STAGE_SECCOMP,
            CHILD_STAGE_EXEC,
        ];
        for (index, code) in codes.iter().enumerate() {
            assert!(!codes[index + 1..].contains(code));
        }
    }

    #[test]
    fn exec_plan_without_init_runs_the_command_directly() {
        let mut spec = CageSpec::new("app", "/bin/true");
        spec.args.push(OsString::from("--flag"));

        let (program, argv) = exec_plan(&spec).expect("exec plan");
        assert_eq!(program, OsString::from("/bin/true"));
        assert_eq!(
            argv,
            vec![OsString::from("/bin/true"), OsString::from("--flag")]
        );
    }

    #[test]
    fn exec_plan_with_init_wraps_the_command() {
        let mut spec = CageSpec::new("app", "/bin/true");
        spec.args.push(OsString::from("--flag"));
        spec.init = Some(PathBuf::from("/usr/bin/cygnus-init"));

        let (program, argv) = exec_plan(&spec).expect("exec plan");
        assert_eq!(program, OsString::from("/usr/bin/cygnus-init"));
        assert_eq!(
            argv,
            vec![
                OsString::from("/usr/bin/cygnus-init"),
                OsString::from("/bin/true"),
                OsString::from("--flag"),
            ]
        );
    }
}
