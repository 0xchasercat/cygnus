//! Seccomp allowlist construction for cage processes.
//!
//! The filter is defense in depth for operator-adjacent code, not a hostile-multitenancy
//! boundary. JavaScriptCore requires an anonymous writable-executable JIT pool, so anonymous RWX
//! mappings are accepted while direct file-backed writable-executable mappings are rejected.
//! `mprotect` cannot reveal a mapping's backing object; the `mmap` rules and noexec writable mounts
//! provide the corresponding enforcement. The filter installs after the mount stack, immediately
//! before `execve`, so the mount family it denies is already spent by then. Audit mode logs
//! non-matching calls while allowing them so the shipped policy can be derived empirically from
//! corpus runs and reviewed by hand.

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
compile_error!("cygnus-cage seccomp supports only x86_64 and aarch64");

use std::collections::BTreeMap;

use nix::{errno::Errno, libc};
use seccompiler::{
    BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter,
    SeccompRule, TargetArch,
};

/// The action applied when a syscall does not match the allowlist.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FilterMode {
    /// Terminate the process on the first non-matching syscall.
    Enforce,
    /// Log non-matching syscalls to the kernel audit log and allow them to continue.
    Audit,
}

/// A seccomp BPF program compiled in the parent for installation in the cage child.
pub struct SeccompPlan {
    program: BpfProgram,
    program_len: libc::c_ushort,
}

impl SeccompPlan {
    /// Compile the cage syscall allowlist for the current architecture.
    pub fn new(mode: FilterMode) -> Result<Self, seccompiler::Error> {
        let filter = SeccompFilter::new(
            build_rules()?,
            match mode {
                FilterMode::Enforce => SeccompAction::KillProcess,
                FilterMode::Audit => SeccompAction::Log,
            },
            SeccompAction::Allow,
            target_arch(),
        )?;
        let program: BpfProgram = filter.try_into()?;
        let program_len = libc::c_ushort::try_from(program.len())
            .map_err(|_| seccompiler::BackendError::FilterTooLarge(program.len()))?;

        Ok(Self {
            program,
            program_len,
        })
    }

    /// Install the precompiled filter in the calling cage child.
    ///
    /// # Safety
    ///
    /// This must run in the cloned child after mounts are complete and immediately before
    /// `execve`. The caller must ensure the child is single-threaded. This method performs no heap
    /// allocation and takes no locks; it only issues raw syscalls over data built in the parent.
    pub unsafe fn apply(&self) -> Result<(), i32> {
        let no_new_privs = unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) };
        if no_new_privs != 0 {
            return Err(Errno::last_raw());
        }

        let program = libc::sock_fprog {
            len: self.program_len,
            filter: self.program.as_ptr().cast::<libc::sock_filter>().cast_mut(),
        };
        let result = unsafe {
            libc::syscall(
                libc::SYS_seccomp,
                libc::SECCOMP_SET_MODE_FILTER,
                0,
                &raw const program,
            )
        };
        if result != 0 {
            return Err(Errno::last_raw());
        }

        Ok(())
    }
}

/// Return the syscall numbers represented by the cage allowlist on the current architecture.
///
/// Syscalls with argument filters are included in this slice. A number's presence does not imply
/// that every invocation of that syscall is accepted.
pub fn allowlisted_syscalls() -> &'static [i64] {
    ALLOWLISTED_SYSCALLS
}

#[cfg(target_arch = "x86_64")]
fn target_arch() -> TargetArch {
    TargetArch::x86_64
}

#[cfg(target_arch = "aarch64")]
fn target_arch() -> TargetArch {
    TargetArch::aarch64
}

fn build_rules() -> Result<BTreeMap<i64, Vec<SeccompRule>>, seccompiler::Error> {
    let mut rules = allowlisted_syscalls()
        .iter()
        .copied()
        .map(|syscall| (syscall, Vec::new()))
        .collect::<BTreeMap<_, _>>();

    rules.insert(libc::SYS_clone, clone_rules()?);
    rules.insert(libc::SYS_socket, socket_rules()?);
    rules.insert(libc::SYS_socketpair, socketpair_rules()?);
    rules.insert(libc::SYS_mmap, mmap_rules()?);
    rules.insert(libc::SYS_ioctl, ioctl_rules()?);

    Ok(rules)
}

fn clone_rules() -> Result<Vec<SeccompRule>, seccompiler::Error> {
    let required = libc::CLONE_VM
        | libc::CLONE_FS
        | libc::CLONE_FILES
        | libc::CLONE_SIGHAND
        | libc::CLONE_THREAD;

    Ok(vec![masked_rule(0, required as u64, required as u64)?])
}

fn socket_rules() -> Result<Vec<SeccompRule>, seccompiler::Error> {
    [libc::AF_UNIX, libc::AF_INET, libc::AF_INET6]
        .into_iter()
        .map(|domain| equality_rule(0, domain as u64))
        .collect()
}

fn socketpair_rules() -> Result<Vec<SeccompRule>, seccompiler::Error> {
    Ok(vec![equality_rule(0, libc::AF_UNIX as u64)?])
}

fn mmap_rules() -> Result<Vec<SeccompRule>, seccompiler::Error> {
    let write_exec = (libc::PROT_WRITE | libc::PROT_EXEC) as u64;
    let private_anonymous = (libc::MAP_PRIVATE | libc::MAP_ANONYMOUS) as u64;

    // Seccompiler joins conditions within a rule with AND and rules for one syscall with OR.
    // The first rule accepts JSC's anonymous private RWX pool. The other two accept mappings with
    // no executable bit or no writable bit, including file-backed read-execute native add-ons.
    Ok(vec![
        SeccompRule::new(vec![
            masked_condition(2, write_exec, write_exec)?,
            masked_condition(3, private_anonymous, private_anonymous)?,
        ])?,
        masked_rule(2, libc::PROT_EXEC as u64, 0)?,
        masked_rule(2, libc::PROT_WRITE as u64, 0)?,
    ])
}

fn ioctl_rules() -> Result<Vec<SeccompRule>, seccompiler::Error> {
    [
        libc::FIONBIO as u64,
        libc::FIONREAD as u64,
        libc::FIOCLEX as u64,
        libc::FIONCLEX as u64,
        libc::TCGETS as u64,
    ]
    .into_iter()
    .map(|request| equality_rule(1, request))
    .collect()
}

fn equality_rule(argument: u8, value: u64) -> Result<SeccompRule, seccompiler::Error> {
    Ok(SeccompRule::new(vec![SeccompCondition::new(
        argument,
        SeccompCmpArgLen::Dword,
        SeccompCmpOp::Eq,
        value,
    )?])?)
}

fn masked_rule(argument: u8, mask: u64, value: u64) -> Result<SeccompRule, seccompiler::Error> {
    Ok(SeccompRule::new(vec![masked_condition(
        argument, mask, value,
    )?])?)
}

fn masked_condition(
    argument: u8,
    mask: u64,
    value: u64,
) -> Result<SeccompCondition, seccompiler::Error> {
    Ok(SeccompCondition::new(
        argument,
        SeccompCmpArgLen::Dword,
        SeccompCmpOp::MaskedEq(mask),
        value,
    )?)
}

const ALLOWLISTED_SYSCALLS: &[i64] = &[
    // File I/O.
    libc::SYS_openat,
    libc::SYS_read,
    libc::SYS_write,
    libc::SYS_close,
    libc::SYS_pread64,
    libc::SYS_pwrite64,
    libc::SYS_readv,
    libc::SYS_writev,
    libc::SYS_preadv,
    libc::SYS_pwritev,
    libc::SYS_lseek,
    libc::SYS_fstat,
    libc::SYS_newfstatat,
    libc::SYS_statx,
    libc::SYS_faccessat,
    libc::SYS_faccessat2,
    libc::SYS_readlinkat,
    libc::SYS_getdents64,
    libc::SYS_fcntl,
    libc::SYS_dup,
    #[cfg(target_arch = "x86_64")]
    libc::SYS_dup2,
    libc::SYS_dup3,
    libc::SYS_ftruncate,
    libc::SYS_fallocate,
    libc::SYS_fsync,
    libc::SYS_fdatasync,
    libc::SYS_copy_file_range,
    libc::SYS_sendfile,
    libc::SYS_unlinkat,
    libc::SYS_mkdirat,
    #[cfg(target_arch = "x86_64")]
    libc::SYS_renameat,
    libc::SYS_renameat2,
    libc::SYS_utimensat,
    libc::SYS_fchmod,
    libc::SYS_fchown,
    libc::SYS_umask,
    libc::SYS_getcwd,
    libc::SYS_chdir,
    libc::SYS_fchdir,
    libc::SYS_flock,
    libc::SYS_memfd_create,
    libc::SYS_inotify_init1,
    libc::SYS_inotify_add_watch,
    libc::SYS_inotify_rm_watch,
    libc::SYS_statfs,
    libc::SYS_fstatfs,
    // Memory. mprotect is intentionally unconditional because seccomp cannot inspect a mapping's
    // backing object. mmap constrains direct writable-executable mappings, and writable mounts are
    // noexec.
    libc::SYS_mmap,
    libc::SYS_mprotect,
    libc::SYS_munmap,
    libc::SYS_mremap,
    libc::SYS_madvise,
    libc::SYS_brk,
    libc::SYS_mlock,
    libc::SYS_munlock,
    libc::SYS_membarrier,
    libc::SYS_mincore,
    // Threads and synchronization. clone is replaced with an argument-filtered rule during build.
    // clone3, fork, and vfork remain absent.
    libc::SYS_clone,
    libc::SYS_futex,
    libc::SYS_futex_waitv,
    libc::SYS_rseq,
    libc::SYS_sched_yield,
    libc::SYS_sched_getaffinity,
    libc::SYS_set_robust_list,
    libc::SYS_get_robust_list,
    libc::SYS_set_tid_address,
    libc::SYS_gettid,
    libc::SYS_tgkill,
    libc::SYS_prlimit64,
    #[cfg(target_arch = "x86_64")]
    libc::SYS_getrlimit,
    #[cfg(target_arch = "x86_64")]
    libc::SYS_setrlimit,
    // Events.
    libc::SYS_epoll_create1,
    libc::SYS_epoll_ctl,
    #[cfg(target_arch = "x86_64")]
    libc::SYS_epoll_wait,
    libc::SYS_epoll_pwait,
    libc::SYS_epoll_pwait2,
    libc::SYS_eventfd2,
    libc::SYS_timerfd_create,
    libc::SYS_timerfd_settime,
    libc::SYS_timerfd_gettime,
    libc::SYS_pipe2,
    #[cfg(target_arch = "x86_64")]
    libc::SYS_poll,
    libc::SYS_ppoll,
    libc::SYS_pselect6,
    #[cfg(target_arch = "x86_64")]
    libc::SYS_select,
    // Sockets. socket and socketpair are replaced with domain-filtered rules during build.
    libc::SYS_socket,
    libc::SYS_connect,
    libc::SYS_bind,
    libc::SYS_listen,
    libc::SYS_accept4,
    libc::SYS_sendto,
    libc::SYS_recvfrom,
    libc::SYS_sendmsg,
    libc::SYS_recvmsg,
    libc::SYS_sendmmsg,
    libc::SYS_recvmmsg,
    libc::SYS_shutdown,
    libc::SYS_getsockopt,
    libc::SYS_setsockopt,
    libc::SYS_getsockname,
    libc::SYS_getpeername,
    libc::SYS_socketpair,
    // Signals.
    libc::SYS_rt_sigaction,
    libc::SYS_rt_sigprocmask,
    libc::SYS_rt_sigreturn,
    libc::SYS_rt_sigpending,
    libc::SYS_rt_sigtimedwait,
    libc::SYS_rt_sigqueueinfo,
    libc::SYS_sigaltstack,
    libc::SYS_kill,
    libc::SYS_wait4,
    libc::SYS_waitid,
    // Time.
    libc::SYS_clock_gettime,
    libc::SYS_clock_getres,
    libc::SYS_clock_nanosleep,
    libc::SYS_nanosleep,
    libc::SYS_gettimeofday,
    // Read-only identity and process metadata.
    libc::SYS_getpid,
    libc::SYS_getppid,
    libc::SYS_getuid,
    libc::SYS_geteuid,
    libc::SYS_getgid,
    libc::SYS_getegid,
    libc::SYS_getgroups,
    #[cfg(target_arch = "x86_64")]
    libc::SYS_getpgrp,
    libc::SYS_getpgid,
    libc::SYS_getsid,
    libc::SYS_uname,
    libc::SYS_getrandom,
    libc::SYS_getpriority,
    libc::SYS_setpriority,
    // Process lifecycle. prctl is broad because Bun and JSC use several safe operations, including
    // thread naming, that cannot be covered by one stable argument filter.
    libc::SYS_execve,
    libc::SYS_exit,
    libc::SYS_exit_group,
    libc::SYS_prctl,
    #[cfg(target_arch = "x86_64")]
    libc::SYS_arch_prctl,
    // ioctl is replaced with request-filtered rules during build.
    libc::SYS_ioctl,
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plans_compile_in_both_modes() {
        let enforce = SeccompPlan::new(FilterMode::Enforce).expect("enforce filter should compile");
        let audit = SeccompPlan::new(FilterMode::Audit).expect("audit filter should compile");

        assert!(!enforce.program.is_empty());
        assert!(!audit.program.is_empty());
    }

    #[test]
    fn allowlist_exposes_expected_policy() {
        for syscall in [
            libc::SYS_openat,
            libc::SYS_futex,
            libc::SYS_epoll_create1,
            libc::SYS_membarrier,
        ] {
            assert!(allowlisted_syscalls().contains(&syscall));
        }

        for syscall in [
            libc::SYS_ptrace,
            libc::SYS_mount,
            libc::SYS_bpf,
            libc::SYS_io_uring_setup,
            libc::SYS_clone3,
            libc::SYS_execveat,
        ] {
            assert!(!allowlisted_syscalls().contains(&syscall));
        }
    }
}
