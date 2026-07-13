use std::env;
use std::ffi::CString;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::ptr;

use nix::errno::Errno;

use crate::error::CageError;
use crate::spec::RootfsSpec;

/// Directory name the old root is pivoted onto before it is detached.
const OLD_ROOT_NAME: &str = ".cygnus-old-root";

/// Host-side staging directory for a cage's overlay root.
///
/// The directory itself is the only host-visible artifact. The tmpfs mounted
/// over it, and the `upper`, `work`, and `merged` directories created inside
/// that tmpfs, exist only in the cage's mount namespace; removing the empty
/// staging directory at teardown is the entire host-side cleanup.
#[derive(Debug)]
pub(crate) struct StagedRootfs {
    staging: PathBuf,
    tmpfs_data: String,
    overlay_data: Vec<u8>,
    removed: bool,
}

impl StagedRootfs {
    /// Create the staging directory and precompute the mount option strings.
    pub(crate) fn create(name: &str, rootfs: &RootfsSpec) -> Result<Self, CageError> {
        let base = rootfs.staging_dir.clone().unwrap_or_else(env::temp_dir);
        let staging = base.join(format!("cygnus-cage-{name}"));
        let staging_bytes = staging.as_os_str().as_bytes();
        if staging_bytes
            .iter()
            .any(|&byte| matches!(byte, b':' | b',' | b'\\' | 0))
        {
            return Err(CageError::InvalidSpec(format!(
                "rootfs staging path {staging:?} contains bytes that cannot appear in overlay \
                 mount options"
            )));
        }

        fs::create_dir_all(&base)
            .map_err(|source| CageError::io("create rootfs staging base", &base, source))?;
        fs::create_dir(&staging)
            .map_err(|source| CageError::io("create rootfs staging directory", &staging, source))?;
        fs::set_permissions(&staging, fs::Permissions::from_mode(0o700))
            .map_err(|source| CageError::io("restrict rootfs staging", &staging, source))?;

        let tmpfs_data = format!("size={},mode=0700", rootfs.tmpfs_size);
        let mut overlay_data = Vec::new();
        overlay_data.extend_from_slice(b"lowerdir=");
        for (index, lower) in rootfs.lowerdirs.iter().enumerate() {
            if index > 0 {
                overlay_data.push(b':');
            }
            overlay_data.extend_from_slice(lower.as_os_str().as_bytes());
        }
        overlay_data.extend_from_slice(b",upperdir=");
        overlay_data.extend_from_slice(staging.join("upper").as_os_str().as_bytes());
        overlay_data.extend_from_slice(b",workdir=");
        overlay_data.extend_from_slice(staging.join("work").as_os_str().as_bytes());

        Ok(Self {
            staging,
            tmpfs_data,
            overlay_data,
            removed: false,
        })
    }

    /// Remove the staging directory. Idempotent; called from cage teardown.
    pub(crate) fn remove(&mut self) -> Result<(), CageError> {
        if self.removed {
            return Ok(());
        }
        match fs::remove_dir_all(&self.staging) {
            Ok(()) => {
                self.removed = true;
                Ok(())
            }
            Err(source) if source.kind() == io::ErrorKind::NotFound => {
                self.removed = true;
                Ok(())
            }
            Err(source) => Err(CageError::io(
                "remove rootfs staging directory",
                &self.staging,
                source,
            )),
        }
    }
}

impl Drop for StagedRootfs {
    fn drop(&mut self) {
        let _ = self.remove();
    }
}

/// Mount operations applied inside a cage's mount namespace before the target
/// is executed.
///
/// The C strings are built in the parent, before `clone`, so the child touches
/// nothing but prebuilt pointers and raw syscalls between fork and exec.
#[derive(Debug)]
pub(crate) struct MountPlan {
    root: CString,
    overlay: Option<OverlayPlan>,
    proc_source: CString,
    proc_target: CString,
    proc_fstype: CString,
}

impl MountPlan {
    /// Build the mount plan for one cage.
    pub(crate) fn new(rootfs: Option<&StagedRootfs>) -> Result<Self, CageError> {
        let overlay = rootfs.map(OverlayPlan::new).transpose()?;
        // These literals are constant and contain no interior NUL byte.
        Ok(Self {
            root: CString::new("/").expect("root path has no NUL byte"),
            overlay,
            proc_source: CString::new("proc").expect("proc source has no NUL byte"),
            proc_target: CString::new("/proc").expect("proc target has no NUL byte"),
            proc_fstype: CString::new("proc").expect("proc fstype has no NUL byte"),
        })
    }

    /// Apply the plan in the current mount namespace.
    ///
    /// First the whole tree is made private so later changes never propagate
    /// back to the host. With an overlay root configured, the cage then
    /// assembles the overlay and pivots into it, leaving the host tree behind
    /// entirely. Last, a fresh `procfs` is mounted so `/proc` reflects the
    /// cage's own PID namespace rather than the host's. The writable surface
    /// stays `nosuid,nodev,noexec`.
    ///
    /// Returns the raw `errno` of the first failing operation.
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

        if let Some(overlay) = &self.overlay {
            unsafe { overlay.apply(&self.root)? };
        }

        // Fresh procfs bound to this PID namespace. With an overlay root this
        // lands inside the pivoted root; without one it covers the host view.
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

/// Overlay root assembly: a size-capped tmpfs holding the writable layer, an
/// overlayfs mount stacking it over the read-only lower directories, and a
/// `pivot_root` that makes the merged tree the cage's root.
#[derive(Debug)]
struct OverlayPlan {
    tmpfs_source: CString,
    tmpfs_target: CString,
    tmpfs_fstype: CString,
    tmpfs_data: CString,
    upper: CString,
    work: CString,
    merged: CString,
    overlay_source: CString,
    overlay_fstype: CString,
    overlay_data: CString,
    merged_proc: CString,
    put_old: CString,
    old_root: CString,
}

impl OverlayPlan {
    fn new(staged: &StagedRootfs) -> Result<Self, CageError> {
        let merged = staged.staging.join("merged");
        Ok(Self {
            tmpfs_source: CString::new("tmpfs").expect("tmpfs source has no NUL byte"),
            tmpfs_target: path_cstring(&staged.staging)?,
            tmpfs_fstype: CString::new("tmpfs").expect("tmpfs fstype has no NUL byte"),
            tmpfs_data: CString::new(staged.tmpfs_data.as_str())
                .expect("tmpfs data has no NUL byte"),
            upper: path_cstring(&staged.staging.join("upper"))?,
            work: path_cstring(&staged.staging.join("work"))?,
            merged: path_cstring(&merged)?,
            overlay_source: CString::new("overlay").expect("overlay source has no NUL byte"),
            overlay_fstype: CString::new("overlay").expect("overlay fstype has no NUL byte"),
            overlay_data: CString::new(staged.overlay_data.clone())
                .map_err(|_| CageError::InvalidSpec("overlay options contain a NUL byte".into()))?,
            merged_proc: path_cstring(&merged.join("proc"))?,
            put_old: path_cstring(&merged.join(OLD_ROOT_NAME))?,
            old_root: CString::new(format!("/{OLD_ROOT_NAME}"))
                .expect("old root path has no NUL byte"),
        })
    }

    /// Assemble the overlay and pivot into it.
    ///
    /// # Safety
    ///
    /// Same contract as [`MountPlan::apply`]: cloned child only, prebuilt
    /// pointers and raw syscalls only.
    unsafe fn apply(&self, root: &CString) -> Result<(), i32> {
        // The writable layer: a size-capped tmpfs visible only in this mount
        // namespace. Everything written at runtime lands here and is dropped
        // with the namespace at teardown.
        let tmpfs = unsafe {
            nix::libc::mount(
                self.tmpfs_source.as_ptr(),
                self.tmpfs_target.as_ptr(),
                self.tmpfs_fstype.as_ptr(),
                nix::libc::MS_NOSUID | nix::libc::MS_NODEV | nix::libc::MS_NOEXEC,
                self.tmpfs_data.as_ptr().cast(),
            )
        };
        if tmpfs != 0 {
            return Err(Errno::last_raw());
        }

        unsafe {
            mkdir(&self.upper, 0o700)?;
            mkdir(&self.work, 0o700)?;
            mkdir(&self.merged, 0o700)?;
        }

        // The merged root keeps exec: the engine and artifact live in the
        // read-only lower layers and native addons need file-backed exec. The
        // noexec writable surface is the tmpfs above.
        let overlay = unsafe {
            nix::libc::mount(
                self.overlay_source.as_ptr(),
                self.merged.as_ptr(),
                self.overlay_fstype.as_ptr(),
                nix::libc::MS_NOSUID | nix::libc::MS_NODEV,
                self.overlay_data.as_ptr().cast(),
            )
        };
        if overlay != 0 {
            return Err(Errno::last_raw());
        }

        // Mount points the pivoted root needs. The lower layers may already
        // provide them, so existing directories are fine.
        unsafe {
            mkdir_allow_exists(&self.merged_proc, 0o555)?;
            mkdir_allow_exists(&self.put_old, 0o700)?;
        }

        let pivot = unsafe {
            nix::libc::syscall(
                nix::libc::SYS_pivot_root,
                self.merged.as_ptr(),
                self.put_old.as_ptr(),
            )
        };
        if pivot != 0 {
            return Err(Errno::last_raw());
        }
        if unsafe { nix::libc::chdir(root.as_ptr()) } != 0 {
            return Err(Errno::last_raw());
        }

        // Drop the old root. The overlay holds its own references to the
        // lower directories and the tmpfs, so the lazy detach is safe.
        if unsafe { nix::libc::umount2(self.old_root.as_ptr(), nix::libc::MNT_DETACH) } != 0 {
            return Err(Errno::last_raw());
        }
        // The mount point stays behind as an empty directory in the upper
        // layer; removing it is cosmetic, so a failure is not fatal.
        let _ = unsafe { nix::libc::rmdir(self.old_root.as_ptr()) };

        Ok(())
    }
}

/// # Safety
///
/// Cloned-child contract: prebuilt pointer, raw syscall only.
unsafe fn mkdir(path: &CString, mode: nix::libc::mode_t) -> Result<(), i32> {
    if unsafe { nix::libc::mkdir(path.as_ptr(), mode) } != 0 {
        return Err(Errno::last_raw());
    }
    Ok(())
}

/// # Safety
///
/// Cloned-child contract: prebuilt pointer, raw syscall only.
unsafe fn mkdir_allow_exists(path: &CString, mode: nix::libc::mode_t) -> Result<(), i32> {
    match unsafe { mkdir(path, mode) } {
        Ok(()) => Ok(()),
        Err(errno) if errno == nix::libc::EEXIST => Ok(()),
        Err(errno) => Err(errno),
    }
}

fn path_cstring(path: &Path) -> Result<CString, CageError> {
    CString::new(path.as_os_str().as_bytes())
        .map_err(|_| CageError::InvalidSpec(format!("path {path:?} contains a NUL byte")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::RootfsSpec;

    fn unique_staging_base(tag: &str) -> PathBuf {
        env::temp_dir().join(format!("cygnus-mount-test-{tag}-{}", std::process::id()))
    }

    #[test]
    fn plan_paths_are_valid_c_strings() {
        let plan = MountPlan::new(None).expect("plan without a rootfs");
        assert_eq!(plan.root.to_bytes(), b"/");
        assert_eq!(plan.proc_source.to_bytes(), b"proc");
        assert_eq!(plan.proc_target.to_bytes(), b"/proc");
        assert_eq!(plan.proc_fstype.to_bytes(), b"proc");
        assert!(plan.overlay.is_none());
    }

    #[test]
    fn staged_rootfs_builds_overlay_options_in_lower_order() {
        let base = unique_staging_base("options");
        let mut rootfs = RootfsSpec::new(vec![PathBuf::from("/base"), PathBuf::from("/engine")]);
        rootfs.tmpfs_size = 1024;
        rootfs.staging_dir = Some(base.clone());

        let mut staged = StagedRootfs::create("app", &rootfs).expect("stage rootfs");
        let staging = base.join("cygnus-cage-app");
        assert!(staging.is_dir());
        assert_eq!(staged.tmpfs_data, "size=1024,mode=0700");

        let expected = format!(
            "lowerdir=/base:/engine,upperdir={0}/upper,workdir={0}/work",
            staging.display()
        );
        assert_eq!(staged.overlay_data, expected.as_bytes());

        staged.remove().expect("remove staging");
        assert!(!staging.exists());
        staged.remove().expect("second remove is idempotent");
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn staged_rootfs_populates_the_full_mount_plan() {
        let base = unique_staging_base("plan");
        let mut rootfs = RootfsSpec::new(vec![PathBuf::from("/lower")]);
        rootfs.staging_dir = Some(base.clone());

        let staged = StagedRootfs::create("app", &rootfs).expect("stage rootfs");
        let plan = MountPlan::new(Some(&staged)).expect("plan with a rootfs");
        let overlay = plan.overlay.as_ref().expect("overlay plan present");

        let staging = base.join("cygnus-cage-app");
        let merged = staging.join("merged");
        assert_eq!(
            overlay.tmpfs_target.to_bytes(),
            staging.as_os_str().as_bytes()
        );
        assert_eq!(overlay.merged.to_bytes(), merged.as_os_str().as_bytes());
        assert_eq!(
            overlay.put_old.to_bytes(),
            merged.join(OLD_ROOT_NAME).as_os_str().as_bytes()
        );
        assert_eq!(overlay.old_root.to_bytes(), b"/.cygnus-old-root");

        drop(staged);
        assert!(!staging.exists());
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn staging_paths_unusable_in_overlay_options_are_rejected() {
        let mut rootfs = RootfsSpec::new(vec![PathBuf::from("/lower")]);
        rootfs.staging_dir = Some(PathBuf::from("/tmp/with:colon"));
        assert!(StagedRootfs::create("app", &rootfs).is_err());
    }
}
