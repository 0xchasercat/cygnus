//! Bounded build-publication directories.
//!
//! On Linux, a [`PublishDir`] is a private `tmpfs` mount.  The mount carries
//! byte and inode limits and is mounted with the flags that keep device nodes,
//! set-id bits, and executable mappings out of the build output.  Other
//! platforms use a private directory only; the bounded backend is currently a
//! Linux capability.

#[cfg(target_os = "linux")]
use std::ffi::CString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

#[cfg(target_os = "linux")]
use std::os::unix::ffi::OsStrExt;
#[cfg(unix)]
use std::os::unix::fs::DirBuilderExt;

/// A daemon-owned directory used for one bounded publication attempt.
///
/// The Linux implementation owns both the mount point and its mount.  Callers
/// should call [`PublishDir::close`] when the temporary output is no longer
/// needed; dropping the value is the best-effort recovery path for unwinding.
/// Non-Linux builds retain the same API, but use an ordinary mode-0700
/// directory because the enforced backend is Linux-only.
#[derive(Debug)]
pub(super) struct PublishDir {
    path: PathBuf,
    #[cfg(target_os = "linux")]
    path_c: CString,
    #[cfg(target_os = "linux")]
    mounted: bool,
}

impl PublishDir {
    /// Create a private publication path under `root`.
    ///
    /// The path is named `.publish-{id}` to keep the mount namespace separate
    /// from the durable `.building-{id}` artifact namespace.  `id` is treated
    /// as one path component; NUL bytes and path separators are rejected
    /// before any filesystem operation.
    pub(super) fn create(
        root: impl AsRef<Path>,
        id: &str,
        byte_limit: u64,
        inode_limit: u64,
    ) -> io::Result<Self> {
        validate_limits(byte_limit, inode_limit)?;
        validate_id(id)?;

        let path = root.as_ref().join(format!(".publish-{id}"));

        #[cfg(target_os = "linux")]
        let path_c = path_cstring(&path)?;
        #[cfg(target_os = "linux")]
        let mount_data = tmpfs_mount_data(byte_limit, inode_limit)?;

        create_private_dir(&path)?;

        #[cfg(target_os = "linux")]
        {
            let flags = libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC;
            let result = unsafe {
                libc::mount(
                    c"tmpfs".as_ptr(),
                    path_c.as_ptr(),
                    c"tmpfs".as_ptr(),
                    flags,
                    mount_data.as_ptr().cast(),
                )
            };
            if result != 0 {
                let error = io::Error::last_os_error();
                let _ = fs::remove_dir(&path);
                return Err(error);
            }

            return Ok(Self {
                path,
                path_c,
                mounted: true,
            });
        }

        #[cfg(not(target_os = "linux"))]
        Ok(Self { path })
    }

    /// Return the exact daemon-owned publication path.
    pub(super) fn path(&self) -> &Path {
        &self.path
    }

    /// Unmount (when applicable) and remove the publication directory.
    ///
    /// The normal Linux unmount is deliberately non-detaching, so an
    /// unmount failure is returned to the caller.  `Drop` is the only path
    /// that attempts `MNT_DETACH`.
    pub(super) fn close(self) -> io::Result<()> {
        #[cfg(target_os = "linux")]
        {
            return self.close_linux();
        }

        #[cfg(not(target_os = "linux"))]
        fs::remove_dir(&self.path)
    }

    #[cfg(target_os = "linux")]
    fn close_linux(mut self) -> io::Result<()> {
        let result = unsafe { libc::umount2(self.path_c.as_ptr(), 0) };
        if result != 0 {
            return Err(io::Error::last_os_error());
        }
        self.mounted = false;
        fs::remove_dir(&self.path)
    }
}

impl Drop for PublishDir {
    fn drop(&mut self) {
        #[cfg(target_os = "linux")]
        if self.mounted {
            // A drop cannot report an error.  Detach only as the fallback for
            // an abandoned live mount; explicit close() never uses it.
            let _ = unsafe { libc::umount2(self.path_c.as_ptr(), libc::MNT_DETACH) };
            self.mounted = false;
        }

        let _ = fs::remove_dir(&self.path);
    }
}

fn validate_limits(byte_limit: u64, inode_limit: u64) -> io::Result<()> {
    if byte_limit == 0 {
        return Err(invalid_input("publication byte limit must be non-zero"));
    }
    if inode_limit == 0 {
        return Err(invalid_input("publication inode limit must be non-zero"));
    }
    Ok(())
}

fn validate_id(id: &str) -> io::Result<()> {
    if id.bytes().any(|byte| byte == 0) {
        return Err(invalid_input("publication id contains an interior NUL"));
    }
    if id.bytes().any(|byte| byte == b'/' || byte == b'\\') {
        return Err(invalid_input("publication id must be one path component"));
    }
    Ok(())
}

fn invalid_input(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

fn create_private_dir(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        let mut builder = fs::DirBuilder::new();
        builder.mode(0o700).create(path)
    }
    #[cfg(not(unix))]
    {
        fs::create_dir(path)
    }
}

#[cfg(target_os = "linux")]
fn path_cstring(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes())
        .map_err(|_| invalid_input("publication path contains an interior NUL"))
}

#[cfg(target_os = "linux")]
fn tmpfs_mount_data(byte_limit: u64, inode_limit: u64) -> io::Result<CString> {
    validate_limits(byte_limit, inode_limit)?;
    CString::new(format!("size={byte_limit},nr_inodes={inode_limit}"))
        .map_err(|_| invalid_input("tmpfs mount options contain an interior NUL"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_zero_limits_before_filesystem_work() {
        let missing_root = Path::new("/definitely/missing/cygnus-publish-tests");
        assert_eq!(
            PublishDir::create(missing_root, "id", 0, 1)
                .expect_err("zero byte limit must fail")
                .kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            PublishDir::create(missing_root, "id", 1, 0)
                .expect_err("zero inode limit must fail")
                .kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn rejects_interior_nul_before_filesystem_work() {
        let missing_root = Path::new("/definitely/missing/cygnus-publish-tests");
        let error = PublishDir::create(missing_root, "bad\0id", 1, 1)
            .expect_err("interior NUL must fail");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn builds_exact_tmpfs_mount_options_without_mounting() {
        let options = tmpfs_mount_data(4 * 1024 * 1024, 123).unwrap();
        assert_eq!(options.as_bytes_with_nul(), b"size=4194304,nr_inodes=123\0");
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn portable_directory_lifecycle() {
        let root = test_root("lifecycle");
        let dir = PublishDir::create(&root, "id", 1, 1).unwrap();
        let path = dir.path().to_path_buf();
        assert!(path.is_dir());
        dir.close().unwrap();
        assert!(!path.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_tmpfs_lifecycle_if_mounting_is_permitted() {
        let root = test_root("linux-mount");
        let result = PublishDir::create(&root, "id", 4 * 1024 * 1024, 128);
        match result {
            Ok(dir) => {
                let path = dir.path().to_path_buf();
                dir.close().unwrap();
                assert!(!path.exists());
            }
            Err(error) if error.raw_os_error() == Some(libc::EPERM) => {}
            Err(error) => panic!("tmpfs mount failed unexpectedly: {error}"),
        }
        fs::remove_dir_all(root).unwrap();
    }

    fn test_root(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "cygnus-publish-{label}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir(&root).unwrap();
        root
    }
}
