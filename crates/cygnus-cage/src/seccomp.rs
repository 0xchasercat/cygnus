//! Seccomp policy for cage processes: a Docker-parity denylist.
//!
//! The threat model for a Class A cage is operator-adjacent code, not a hostile
//! multitenant boundary (that is the microVM tier). The job of seccomp here is
//! the same as Docker's default profile: shrink the kernel's attack surface as
//! defense in depth, without getting in the way of real workloads. So the
//! filter mirrors Docker's shape rather than a bespoke allowlist — every
//! syscall is allowed by default, and a small, stable set of dangerous ones is
//! blocked. `Enforce` returns `EPERM` for a blocked syscall (matching Docker,
//! and letting runtimes degrade — Bun's `io_uring` probe falls back to `epoll`
//! rather than dying); `Audit` logs the attempt and allows it, for observing a
//! corpus before enforcing.
//!
//! Namespaces, the user namespace (cage root maps to an unprivileged host UID),
//! and cgroups do the primary containment; this list is the surface-reduction
//! layer on top. It is deliberately not a tight allowlist: that bought strength
//! this tier does not need at the cost of breaking on every runtime and libc.

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
compile_error!("cygnus-cage seccomp supports only x86_64 and aarch64");

use std::collections::BTreeMap;

use nix::libc;
use seccompiler::{BpfProgram, SeccompAction, SeccompFilter, SeccompRule, TargetArch};

pub use crate::spec::FilterMode;

/// A seccomp BPF program compiled in the parent for installation in the cage child.
pub struct SeccompPlan {
    program: BpfProgram,
    program_len: libc::c_ushort,
}

impl SeccompPlan {
    /// Compile the cage seccomp denylist for the current architecture.
    pub fn new(mode: FilterMode) -> Result<Self, seccompiler::Error> {
        let blocked_action = match mode {
            // EPERM, like Docker's default profile: the call fails cleanly
            // instead of killing the process, so runtimes can fall back.
            FilterMode::Enforce => SeccompAction::Errno(libc::EPERM as u32),
            FilterMode::Audit => SeccompAction::Log,
        };
        // Default action Allow; only the listed syscalls take `blocked_action`.
        let filter = SeccompFilter::new(
            build_rules(),
            SeccompAction::Allow,
            blocked_action,
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
            return Err(nix::errno::Errno::last_raw());
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
            return Err(nix::errno::Errno::last_raw());
        }

        Ok(())
    }
}

/// The syscall numbers the cage blocks on the current architecture.
///
/// A number's presence means the cage denies it; everything else is allowed.
pub fn denied_syscalls() -> &'static [i64] {
    DENIED_SYSCALLS
}

#[cfg(target_arch = "x86_64")]
fn target_arch() -> TargetArch {
    TargetArch::x86_64
}

#[cfg(target_arch = "aarch64")]
fn target_arch() -> TargetArch {
    TargetArch::aarch64
}

fn build_rules() -> BTreeMap<i64, Vec<SeccompRule>> {
    // An empty rule vector means the action applies unconditionally to the
    // syscall number, regardless of arguments.
    denied_syscalls()
        .iter()
        .copied()
        .map(|syscall| (syscall, Vec::new()))
        .collect()
}

/// The blocked set: kernel-attack-surface and host-privilege syscalls that a
/// normal application never needs, mirroring the dangerous entries in Docker's
/// default profile. Ancient syscalls already removed from modern kernels are
/// omitted (they return `ENOSYS` on their own); this covers the live surface.
const DENIED_SYSCALLS: &[i64] = &[
    // Kernel module loading.
    libc::SYS_init_module,
    libc::SYS_finit_module,
    libc::SYS_delete_module,
    // Kernel replacement and reboot.
    libc::SYS_kexec_load,
    libc::SYS_kexec_file_load,
    libc::SYS_reboot,
    // Filesystem mounting and host-filesystem escape.
    libc::SYS_mount,
    libc::SYS_umount2,
    libc::SYS_pivot_root,
    libc::SYS_move_mount,
    libc::SYS_fsopen,
    libc::SYS_fsconfig,
    libc::SYS_fsmount,
    libc::SYS_open_tree,
    libc::SYS_open_by_handle_at,
    libc::SYS_swapon,
    libc::SYS_swapoff,
    // Namespace manipulation from inside the cage.
    libc::SYS_setns,
    libc::SYS_unshare,
    // Process introspection and tracing across the cage boundary.
    libc::SYS_ptrace,
    libc::SYS_process_vm_readv,
    libc::SYS_process_vm_writev,
    libc::SYS_kcmp,
    libc::SYS_perf_event_open,
    // High-risk kernel interfaces.
    libc::SYS_bpf,
    libc::SYS_io_uring_setup,
    libc::SYS_io_uring_enter,
    libc::SYS_io_uring_register,
    libc::SYS_userfaultfd,
    // Kernel keyring.
    libc::SYS_add_key,
    libc::SYS_request_key,
    libc::SYS_keyctl,
    // Host-global clock and identity.
    libc::SYS_settimeofday,
    libc::SYS_clock_settime,
    libc::SYS_clock_adjtime,
    libc::SYS_adjtimex,
    libc::SYS_sethostname,
    libc::SYS_setdomainname,
    // Accounting and quotas.
    libc::SYS_acct,
    libc::SYS_quotactl,
    // x86-specific privileged interfaces with no analogue on aarch64.
    #[cfg(target_arch = "x86_64")]
    libc::SYS_iopl,
    #[cfg(target_arch = "x86_64")]
    libc::SYS_ioperm,
    #[cfg(target_arch = "x86_64")]
    libc::SYS_modify_ldt,
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
    fn denylist_blocks_the_dangerous_surface() {
        for syscall in [
            libc::SYS_mount,
            libc::SYS_ptrace,
            libc::SYS_bpf,
            libc::SYS_io_uring_setup,
            libc::SYS_add_key,
            libc::SYS_kexec_load,
            libc::SYS_setns,
        ] {
            assert!(denied_syscalls().contains(&syscall), "{syscall} should be denied");
        }
    }

    #[test]
    fn denylist_leaves_ordinary_syscalls_alone() {
        // The workloads run unfiltered on these; only the dangerous set is
        // blocked, so none of these may appear in the denylist.
        for syscall in [
            libc::SYS_openat,
            libc::SYS_read,
            libc::SYS_write,
            libc::SYS_futex,
            libc::SYS_mmap,
            libc::SYS_clone,
            libc::SYS_socket,
            libc::SYS_epoll_create1,
        ] {
            assert!(
                !denied_syscalls().contains(&syscall),
                "{syscall} must not be denied"
            );
        }
    }
}
