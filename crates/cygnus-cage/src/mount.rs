use std::env;
use std::ffi::CString;
use std::fs;
use std::io;
use std::mem::MaybeUninit;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::ptr;

use nix::errno::Errno;

use crate::error::CageError;
use crate::net;
use crate::spec::{
    BUILD_OUTPUT_CAGE_DIR, BuildOutputSpec, INGRESS_CAGE_DIR, IngressSpec, RootfsSpec,
};

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
    ingress: Option<IngressPlan>,
    build_output: Option<BuildOutputPlan>,
    proc_source: CString,
    proc_target: CString,
    proc_fstype: CString,
}

impl MountPlan {
    /// Build the mount plan for one cage. All path strings are allocated here,
    /// before the child is cloned.
    pub(crate) fn new(
        rootfs: Option<&StagedRootfs>,
        ingress: Option<&IngressSpec>,
        build_output: Option<&BuildOutputSpec>,
    ) -> Result<Self, CageError> {
        let ingress_plan = match (rootfs, ingress) {
            (Some(_), Some(ingress)) => Some(IngressPlan::new(ingress)?),
            (None, Some(_)) => {
                return Err(CageError::InvalidSpec("ingress requires a rootfs".into()));
            }
            (Some(_), None) | (None, None) => None,
        };
        let build_output_plan = match (rootfs, build_output) {
            (Some(_), Some(output)) => Some(BuildOutputPlan::new(output)?),
            (None, Some(_)) => {
                return Err(CageError::InvalidSpec(
                    "build output requires a rootfs".into(),
                ));
            }
            (Some(_), None) | (None, None) => None,
        };
        let overlay = rootfs.map(OverlayPlan::new).transpose()?;
        // These literals are constant and contain no interior NUL byte.
        Ok(Self {
            root: CString::new("/").expect("root path has no NUL byte"),
            overlay,
            ingress: ingress_plan,
            build_output: build_output_plan,
            proc_source: CString::new("proc").expect("proc source has no NUL byte"),
            proc_target: CString::new("/proc").expect("proc target has no NUL byte"),
            proc_fstype: CString::new("proc").expect("proc fstype has no NUL byte"),
        })
    }

    /// Apply the plan in the current mount namespace.
    ///
    /// First the whole tree is made private so later changes never propagate
    /// back to the host. With an overlay root configured, the cage assembles
    /// the overlay, mounts a fresh `procfs` into the merged tree, and then
    /// pivots; without an overlay, `procfs` is mounted directly at `/proc`.
    /// The writable surface stays `nosuid,nodev,noexec`.
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
            unsafe {
                overlay.apply(
                    &self.root,
                    self.ingress.as_ref(),
                    self.build_output.as_ref(),
                    &self.proc_source,
                    &self.proc_fstype,
                )?
            };
        } else {
            unsafe { mount_proc(&self.proc_source, &self.proc_target, &self.proc_fstype)? };
        }

        Ok(())
    }
}

/// Hardening flags applied to the ingress bind mount's remount operation.
const INGRESS_REMOUNT_FLAGS: nix::libc::c_ulong = nix::libc::MS_BIND
    | nix::libc::MS_REMOUNT
    | nix::libc::MS_NOSUID
    | nix::libc::MS_NODEV
    | nix::libc::MS_NOEXEC;

#[derive(Debug)]
struct IngressPlan {
    source: CString,
    target_parent: CString,
    target: CString,
    remount_flags: nix::libc::c_ulong,
}

impl IngressPlan {
    /// Verify the host source and prebuild every path used after `clone`.
    fn new(ingress: &IngressSpec) -> Result<Self, CageError> {
        Self::from_host_dir(&ingress.host_dir, INGRESS_CAGE_DIR, "ingress")
    }

    fn from_host_dir(host_dir: &Path, target: &str, label: &str) -> Result<Self, CageError> {
        if !host_dir.is_absolute() {
            return Err(CageError::InvalidSpec(format!(
                "{label} host directory must be absolute"
            )));
        }
        if host_dir == Path::new("/") {
            return Err(CageError::InvalidSpec(format!(
                "{label} host directory must not be the host root"
            )));
        }
        if host_dir.as_os_str().as_bytes().contains(&0) {
            return Err(CageError::InvalidSpec(format!(
                "{label} host directory must not contain a NUL byte"
            )));
        }
        if host_dir
            .components()
            .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
        {
            return Err(CageError::InvalidSpec(format!(
                "{label} host directory must not contain '.' or '..' components"
            )));
        }
        let operation = if label == "build output" {
            "verify build output host directory"
        } else {
            "verify ingress host directory"
        };
        let metadata = fs::symlink_metadata(host_dir)
            .map_err(|source| CageError::io(operation, host_dir, source))?;
        if !metadata.file_type().is_dir() {
            return Err(CageError::InvalidSpec(format!(
                "{label} host path {host_dir:?} must be a directory, not a symlink or file"
            )));
        }
        let relative = host_dir.strip_prefix("/").map_err(|_| {
            CageError::InvalidSpec(format!("{label} host directory must be absolute"))
        })?;
        let mut source_path = PathBuf::from(format!("/{OLD_ROOT_NAME}"));
        source_path.push(relative);
        Ok(Self {
            source: path_cstring(&source_path)?,
            target_parent: CString::new("/cygnus").expect("bind parent has no NUL byte"),
            target: CString::new(target).expect("bind target has no NUL byte"),
            remount_flags: INGRESS_REMOUNT_FLAGS,
        })
    }

    /// Create the fixed target and expose the host directory in the pivoted root.
    ///
    /// # Safety
    ///
    /// Cloned-child contract: all pointers are prebuilt and this uses raw
    /// syscalls only; no allocation or lock acquisition occurs here.
    unsafe fn apply(&self) -> Result<(), i32> {
        unsafe {
            mkdir_allow_exists(&self.target_parent, 0o755)?;
            require_directory(&self.target_parent)?;
            mkdir_allow_exists(&self.target, 0o755)?;
            require_directory(&self.target)?;
        }
        let bind = unsafe {
            nix::libc::mount(
                self.source.as_ptr(),
                self.target.as_ptr(),
                ptr::null(),
                nix::libc::MS_BIND,
                ptr::null(),
            )
        };
        if bind != 0 {
            return Err(Errno::last_raw());
        }
        let remount = unsafe {
            nix::libc::mount(
                ptr::null(),
                self.target.as_ptr(),
                ptr::null(),
                self.remount_flags,
                ptr::null(),
            )
        };
        if remount != 0 {
            return Err(Errno::last_raw());
        }
        Ok(())
    }
}

#[derive(Debug)]
struct BuildOutputPlan(IngressPlan);

impl BuildOutputPlan {
    fn new(output: &BuildOutputSpec) -> Result<Self, CageError> {
        Ok(Self(IngressPlan::from_host_dir(
            &output.host_dir,
            BUILD_OUTPUT_CAGE_DIR,
            "build output",
        )?))
    }

    unsafe fn apply(&self) -> Result<(), i32> {
        unsafe { self.0.apply() }
    }
}

/// Host character devices intentionally exposed inside an overlay cage.
///
/// Keep this list closed: adding a path here changes the cage's device
/// surface and must be an explicit security decision.
const APPROVED_DEVICE_PATHS: [(&str, &str); 4] = [
    ("null", "/dev/null"),
    ("zero", "/dev/zero"),
    ("random", "/dev/random"),
    ("urandom", "/dev/urandom"),
];

/// Device bind mounts must clear `nodev` while retaining the other hardening
/// flags. The overlay and writable tmpfs remain globally `nodev`.
const DEVICE_REMOUNT_FLAGS: nix::libc::c_ulong = nix::libc::MS_BIND
    | nix::libc::MS_REMOUNT
    | nix::libc::MS_NOSUID
    | nix::libc::MS_NOEXEC;

#[derive(Debug)]
struct DevicePlan {
    source: CString,
    placeholder: CString,
    target: CString,
    expected_rdev: nix::libc::dev_t,
}

impl DevicePlan {
    fn all(upper_dev: &Path, merged_dev: &Path) -> Result<Vec<Self>, CageError> {
        APPROVED_DEVICE_PATHS
            .iter()
            .map(|(name, source)| Self::new(name, source, upper_dev, merged_dev))
            .collect()
    }

    fn new(
        name: &str,
        source: &str,
        upper_dev: &Path,
        merged_dev: &Path,
    ) -> Result<Self, CageError> {
        let source_path = Path::new(source);
        let expected_rdev = validate_character_device_source(source_path, name)?;
        Ok(Self {
            source: CString::new(source).expect("approved device path has no NUL byte"),
            placeholder: path_cstring(&upper_dev.join(name))?,
            target: path_cstring(&merged_dev.join(name))?,
            expected_rdev,
        })
    }

    /// Bind one validated host character device into the merged root.
    ///
    /// # Safety
    ///
    /// Cloned-child contract: all pointers are prebuilt and this uses raw
    /// syscalls only; no allocation or lock acquisition occurs here.
    unsafe fn apply(&self) -> Result<(), i32> {
        unsafe { require_character_device(&self.source, self.expected_rdev)? };
        let bind = unsafe {
            nix::libc::mount(
                self.source.as_ptr(),
                self.target.as_ptr(),
                ptr::null(),
                nix::libc::MS_BIND,
                ptr::null(),
            )
        };
        if bind != 0 {
            return Err(Errno::last_raw());
        }
        let remount = unsafe {
            nix::libc::mount(
                ptr::null(),
                self.target.as_ptr(),
                ptr::null(),
                DEVICE_REMOUNT_FLAGS,
                ptr::null(),
            )
        };
        if remount != 0 {
            return Err(Errno::last_raw());
        }
        Ok(())
    }
}

/// Validate a host source before cloning. Symlinks and non-character files
/// are rejected rather than silently broadening the cage's device surface.
fn validate_character_device_source(
    path: &Path,
    name: &str,
) -> Result<nix::libc::dev_t, CageError> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|source| CageError::io("verify cage device", path, source))?;
    if metadata.mode() & nix::libc::S_IFMT != nix::libc::S_IFCHR {
        return Err(CageError::InvalidSpec(format!(
            "cage device {name} at {path:?} must be a character device"
        )));
    }
    Ok(metadata.rdev() as nix::libc::dev_t)
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
    upper_dev: CString,
    upper_etc: CString,
    upper_resolv_conf: CString,
    resolv_conf: Vec<u8>,
    work: CString,
    merged: CString,
    merged_dev: CString,
    overlay_source: CString,
    overlay_fstype: CString,
    overlay_data: CString,
    devices: Vec<DevicePlan>,
    merged_proc: CString,
    put_old: CString,
    old_root: CString,
}

impl OverlayPlan {
    fn new(staged: &StagedRootfs) -> Result<Self, CageError> {
        let merged = staged.staging.join("merged");
        let upper = staged.staging.join("upper");
        let upper_dev = upper.join("dev");
        let merged_dev = merged.join("dev");
        Ok(Self {
            tmpfs_source: CString::new("tmpfs").expect("tmpfs source has no NUL byte"),
            tmpfs_target: path_cstring(&staged.staging)?,
            tmpfs_fstype: CString::new("tmpfs").expect("tmpfs fstype has no NUL byte"),
            tmpfs_data: CString::new(staged.tmpfs_data.as_str())
                .expect("tmpfs data has no NUL byte"),
            upper: path_cstring(&upper)?,
            upper_dev: path_cstring(&upper_dev)?,
            upper_etc: path_cstring(&upper.join("etc"))?,
            upper_resolv_conf: path_cstring(&upper.join("etc/resolv.conf"))?,
            resolv_conf: format!("nameserver {}\noptions edns0\n", net::GATEWAY).into_bytes(),
            work: path_cstring(&staged.staging.join("work"))?,
            merged: path_cstring(&merged)?,
            merged_dev: path_cstring(&merged_dev)?,
            overlay_source: CString::new("overlay").expect("overlay source has no NUL byte"),
            overlay_fstype: CString::new("overlay").expect("overlay fstype has no NUL byte"),
            overlay_data: CString::new(staged.overlay_data.clone())
                .map_err(|_| CageError::InvalidSpec("overlay options contain a NUL byte".into()))?,
            devices: DevicePlan::all(&upper_dev, &merged_dev)?,
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
    unsafe fn apply(
        &self,
        root: &CString,
        ingress: Option<&IngressPlan>,
        build_output: Option<&BuildOutputPlan>,
        proc_source: &CString,
        proc_fstype: &CString,
    ) -> Result<(), i32> {
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
            mkdir(&self.upper_dev, 0o755)?;
            mkdir(&self.upper_etc, 0o755)?;
            write_new_file(&self.upper_resolv_conf, &self.resolv_conf, 0o644)?;
            mkdir(&self.work, 0o700)?;
            mkdir(&self.merged, 0o700)?;
            for device in &self.devices {
                create_device_placeholder(&device.placeholder)?;
            }
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
        // provide them, so existing directories are fine. `/dev` is supplied
        // by our explicit device binds below, never by a host-wide dev mount.
        unsafe {
            mkdir_allow_exists(&self.merged_dev, 0o755)?;
            require_directory(&self.merged_dev)?;
        }
        for device in &self.devices {
            unsafe { device.apply()? };
        }
        unsafe {
            mkdir_allow_exists(&self.merged_proc, 0o555)?;
            mkdir_allow_exists(&self.put_old, 0o700)?;
        }

        // Mount procfs while the merged root is still reachable from the old
        // tree. The mount moves with the tree across pivot_root and reflects
        // this child’s PID namespace.
        unsafe { mount_proc(proc_source, &self.merged_proc, proc_fstype)? };

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

        // Keep the old root mounted while the fixed ingress target is created;
        // its source path resolves through the host tree at this point.
        if let Some(ingress) = ingress {
            unsafe { ingress.apply()? };
        }
        if let Some(output) = build_output {
            unsafe { output.apply()? };
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

/// Mount a fresh procfs for the cage's PID namespace.
///
/// # Safety
///
/// Cloned-child contract: prebuilt pointers and raw syscall only.
unsafe fn mount_proc(source: &CString, target: &CString, fstype: &CString) -> Result<(), i32> {
    let mounted = unsafe {
        nix::libc::mount(
            source.as_ptr(),
            target.as_ptr(),
            fstype.as_ptr(),
            nix::libc::MS_NOSUID | nix::libc::MS_NODEV | nix::libc::MS_NOEXEC,
            ptr::null(),
        )
    };
    if mounted != 0 {
        return Err(Errno::last_raw());
    }
    Ok(())
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

/// Create a private regular placeholder for a file bind mount. The host
/// character device is checked immediately before the bind and supplies the
/// actual device semantics after the mount.
///
/// # Safety
///
/// Cloned-child contract: prebuilt pointer and raw syscalls only.
unsafe fn create_device_placeholder(path: &CString) -> Result<(), i32> {
    let fd = unsafe {
        nix::libc::open(
            path.as_ptr(),
            nix::libc::O_WRONLY
                | nix::libc::O_CREAT
                | nix::libc::O_EXCL
                | nix::libc::O_CLOEXEC
                | nix::libc::O_NOFOLLOW,
            0o600,
        )
    };
    if fd < 0 {
        return Err(Errno::last_raw());
    }
    if unsafe { nix::libc::close(fd) } != 0 {
        return Err(Errno::last_raw());
    }
    Ok(())
}

/// Create and populate a private regular file in the overlay upper layer.
///
/// # Safety
///
/// Cloned-child contract: prebuilt path/content buffers and raw syscalls only.
unsafe fn write_new_file(
    path: &CString,
    contents: &[u8],
    mode: nix::libc::mode_t,
) -> Result<(), i32> {
    let fd = unsafe {
        nix::libc::open(
            path.as_ptr(),
            nix::libc::O_WRONLY
                | nix::libc::O_CREAT
                | nix::libc::O_EXCL
                | nix::libc::O_CLOEXEC
                | nix::libc::O_NOFOLLOW,
            mode,
        )
    };
    if fd < 0 {
        return Err(Errno::last_raw());
    }

    let mut offset = 0;
    while offset < contents.len() {
        let written = unsafe {
            nix::libc::write(
                fd,
                contents.as_ptr().add(offset).cast(),
                contents.len() - offset,
            )
        };
        if written < 0 {
            let error = Errno::last_raw();
            if error == nix::libc::EINTR {
                continue;
            }
            let _ = unsafe { nix::libc::close(fd) };
            return Err(error);
        }
        if written == 0 {
            let _ = unsafe { nix::libc::close(fd) };
            return Err(nix::libc::EIO);
        }
        offset += written as usize;
    }

    if unsafe { nix::libc::close(fd) } != 0 {
        return Err(Errno::last_raw());
    }
    Ok(())
}

/// Verify that a source is still the expected character device after clone.
/// This closes the pre-clone validation race without allocating in the child.
///
/// # Safety
///
/// Cloned-child contract: prebuilt pointer, stack storage, and raw syscall
/// only.
unsafe fn require_character_device(
    path: &CString,
    expected_rdev: nix::libc::dev_t,
) -> Result<(), i32> {
    let mut metadata = MaybeUninit::<nix::libc::stat>::uninit();
    if unsafe { nix::libc::lstat(path.as_ptr(), metadata.as_mut_ptr()) } != 0 {
        return Err(Errno::last_raw());
    }
    let metadata = unsafe { metadata.assume_init() };
    if metadata.st_mode & nix::libc::S_IFMT != nix::libc::S_IFCHR
        || metadata.st_rdev != expected_rdev
    {
        return Err(nix::libc::ENODEV);
    }
    Ok(())
}

/// Reject a pre-existing symlink or non-directory mount target.
///
/// # Safety
///
/// Cloned-child contract: prebuilt pointer, stack storage, and raw syscall only.
unsafe fn require_directory(path: &CString) -> Result<(), i32> {
    let mut metadata = MaybeUninit::<nix::libc::stat>::uninit();
    if unsafe { nix::libc::lstat(path.as_ptr(), metadata.as_mut_ptr()) } != 0 {
        return Err(Errno::last_raw());
    }
    let metadata = unsafe { metadata.assume_init() };
    if metadata.st_mode & nix::libc::S_IFMT != nix::libc::S_IFDIR {
        return Err(nix::libc::ENOTDIR);
    }
    Ok(())
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
        let plan = MountPlan::new(None, None, None).expect("plan without a rootfs");
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
        let plan = MountPlan::new(Some(&staged), None, None).expect("plan with a rootfs");
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
        assert_eq!(
            overlay.upper_resolv_conf.to_bytes(),
            staging.join("upper/etc/resolv.conf").as_os_str().as_bytes()
        );
        assert_eq!(
            overlay.resolv_conf,
            format!("nameserver {}\noptions edns0\n", net::GATEWAY).as_bytes()
        );


        drop(staged);
        assert!(!staging.exists());
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn overlay_plan_contains_only_approved_device_binds() {
        let base = unique_staging_base("device-plan");
        let mut rootfs = RootfsSpec::new(vec![PathBuf::from("/lower")]);
        rootfs.staging_dir = Some(base.clone());

        let staged = StagedRootfs::create("app", &rootfs).expect("stage rootfs");
        let plan = MountPlan::new(Some(&staged), None, None).expect("plan with devices");
        let overlay = plan.overlay.as_ref().expect("overlay plan present");
        assert_eq!(overlay.merged_dev.to_bytes(),
            base.join("cygnus-cage-app/merged/dev").as_os_str().as_bytes());
        assert_eq!(overlay.devices.len(), APPROVED_DEVICE_PATHS.len());
        for ((name, source), device) in APPROVED_DEVICE_PATHS.iter().zip(&overlay.devices) {
            assert_eq!(device.source.to_bytes(), source.as_bytes());
            assert_eq!(device.target.to_bytes(),
                base.join(format!("cygnus-cage-app/merged/dev/{name}")).as_os_str().as_bytes());
            assert_eq!(device.placeholder.to_bytes(),
                base.join(format!("cygnus-cage-app/upper/dev/{name}")).as_os_str().as_bytes());
        }
        assert_eq!(DEVICE_REMOUNT_FLAGS & nix::libc::MS_NODEV, 0);

        drop(staged);
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn device_source_validation_rejects_a_substituted_regular_file() {
        let base = unique_staging_base("device-source");
        fs::create_dir_all(&base).expect("create device test directory");
        let substituted = base.join("null");
        fs::write(&substituted, b"not a device").expect("create substituted source");

        let error = validate_character_device_source(&substituted, "null")
            .expect_err("regular file must not be accepted as a device");
        match error {
            CageError::InvalidSpec(message) => {
                assert!(message.contains("must be a character device"));
            }
            other => panic!("unexpected validation error: {other:?}"),
        }
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn ingress_plan_prebuilds_exact_source_target_and_flags() {
        let base = unique_staging_base("ingress-plan");
        let host_dir = base.join("host-app");
        fs::create_dir_all(&host_dir).expect("create ingress host directory");
        let mut rootfs = RootfsSpec::new(vec![PathBuf::from("/lower")]);
        rootfs.staging_dir = Some(base.join("staging"));

        let staged = StagedRootfs::create("app", &rootfs).expect("stage rootfs");
        let ingress = IngressSpec::new(host_dir.clone());
        let plan = MountPlan::new(Some(&staged), Some(&ingress), None).expect("ingress plan");
        let ingress_plan = plan.ingress.as_ref().expect("ingress plan present");

        assert_eq!(
            ingress_plan.source.to_bytes(),
            format!("/{OLD_ROOT_NAME}{}", host_dir.display()).as_bytes()
        );
        assert_eq!(ingress_plan.target_parent.to_bytes(), b"/cygnus");
        assert_eq!(ingress_plan.target.to_bytes(), INGRESS_CAGE_DIR.as_bytes());
        assert_eq!(
            ingress_plan.remount_flags,
            nix::libc::MS_BIND
                | nix::libc::MS_REMOUNT
                | nix::libc::MS_NOSUID
                | nix::libc::MS_NODEV
                | nix::libc::MS_NOEXEC
        );

        drop(staged);
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn build_output_plan_uses_the_fixed_hardened_mount() {
        let base = unique_staging_base("output-plan");
        let host_dir = base.join("host-output");
        fs::create_dir_all(&host_dir).expect("create output host directory");
        let mut rootfs = RootfsSpec::new(vec![PathBuf::from("/lower")]);
        rootfs.staging_dir = Some(base.join("staging"));

        let staged = StagedRootfs::create("app", &rootfs).expect("stage rootfs");
        let output = BuildOutputSpec::new(host_dir.clone());
        let plan = MountPlan::new(Some(&staged), None, Some(&output)).expect("output plan");
        let output_plan = plan.build_output.as_ref().expect("output plan present");
        assert_eq!(
            output_plan.0.target.to_bytes(),
            BUILD_OUTPUT_CAGE_DIR.as_bytes()
        );
        assert_eq!(
            output_plan.0.source.to_bytes(),
            format!("/{OLD_ROOT_NAME}{}", host_dir.display()).as_bytes()
        );
        assert_eq!(
            output_plan.0.remount_flags,
            nix::libc::MS_BIND
                | nix::libc::MS_REMOUNT
                | nix::libc::MS_NOSUID
                | nix::libc::MS_NODEV
                | nix::libc::MS_NOEXEC
        );
        drop(staged);
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn ingress_source_must_be_a_real_directory() {
        use std::os::unix::fs::symlink;

        let base = unique_staging_base("ingress-source");
        let real = base.join("real");
        let linked = base.join("linked");
        fs::create_dir_all(&real).expect("create real ingress directory");
        symlink(&real, &linked).expect("create ingress symlink");
        let ingress = IngressSpec::new(linked);

        assert!(IngressPlan::new(&ingress).is_err());
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn ingress_target_check_rejects_a_symlink() {
        use std::os::unix::fs::symlink;

        let base = unique_staging_base("ingress-target");
        let real = base.join("real");
        let linked = base.join("linked");
        fs::create_dir_all(&real).expect("create real target directory");
        symlink(&real, &linked).expect("create target symlink");
        let linked = path_cstring(&linked).expect("target C string");

        let error = unsafe { require_directory(&linked) }.expect_err("symlink must fail");
        assert_eq!(error, nix::libc::ENOTDIR);
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn staging_paths_unusable_in_overlay_options_are_rejected() {
        let mut rootfs = RootfsSpec::new(vec![PathBuf::from("/lower")]);
        rootfs.staging_dir = Some(PathBuf::from("/tmp/with:colon"));
        assert!(StagedRootfs::create("app", &rootfs).is_err());
    }
}
