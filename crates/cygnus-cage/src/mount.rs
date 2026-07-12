use std::ffi::CString;
use std::ptr;

use nix::errno::Errno;

/// Mount operations applied inside a cage's mount namespace before the target
/// is executed.
///
/// The C strings are built in the parent, before `clone`, so the child touches
/// nothing but prebuilt pointers and raw syscalls between fork and exec.
#[derive(Debug)]
pub(crate) struct MountPlan {
    root: CString,
    proc_source: CString,
    proc_target: CString,
    proc_fstype: CString,
}

impl MountPlan {
    /// Build the mount plan for one cage.
    pub(crate) fn new() -> Self {
        // These literals are constant and contain no interior NUL byte.
        Self {
            root: CString::new("/").expect("root path has no NUL byte"),
            proc_source: CString::new("proc").expect("proc source has no NUL byte"),
            proc_target: CString::new("/proc").expect("proc target has no NUL byte"),
            proc_fstype: CString::new("proc").expect("proc fstype has no NUL byte"),
        }
    }

    /// Apply the plan in the current mount namespace.
    ///
    /// First the whole tree is made private so later changes never propagate
    /// back to the host, then a fresh `procfs` is mounted so `/proc` reflects
    /// the cage's own PID namespace rather than the host's. The writable
    /// surface stays `nosuid,nodev,noexec`.
    ///
    /// Returns the raw `errno` of the first failing mount.
    ///
    /// # Safety
    ///
    /// Must run in the cloned child after the user-namespace maps are set and
    /// before `execve`. It calls only raw syscalls on prebuilt pointers, so it
    /// is async-signal-safe with respect to the multithreaded parent.
    pub(crate) unsafe fn apply(&self) -> Result<(), i32> {
        // Detach mount propagation: nothing done here reaches the host tree.
        let private = unsafe {
            nix::libc::mount(
                ptr::null(),
                self.root.as_ptr(),
                ptr::null(),
                nix::libc::MS_REC | nix::libc::MS_PRIVATE,
                ptr::null(),
            )
        };
        if private != 0 {
            return Err(Errno::last_raw());
        }

        // Fresh procfs bound to this PID namespace.
        let proc = unsafe {
            nix::libc::mount(
                self.proc_source.as_ptr(),
                self.proc_target.as_ptr(),
                self.proc_fstype.as_ptr(),
                nix::libc::MS_NOSUID | nix::libc::MS_NODEV | nix::libc::MS_NOEXEC,
                ptr::null(),
            )
        };
        if proc != 0 {
            return Err(Errno::last_raw());
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_paths_are_valid_c_strings() {
        let plan = MountPlan::new();
        assert_eq!(plan.root.to_bytes(), b"/");
        assert_eq!(plan.proc_source.to_bytes(), b"proc");
        assert_eq!(plan.proc_target.to_bytes(), b"/proc");
        assert_eq!(plan.proc_fstype.to_bytes(), b"proc");
    }
}
