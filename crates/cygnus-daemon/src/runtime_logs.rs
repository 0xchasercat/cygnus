//! Capture long-running cage output into daemon-owned, bounded live log files.
//!
//! The drainers deliberately retain only the current live file and one rotated
//! file. Readers elsewhere should open only `stdout.log` or `stderr.log`.

use std::ffi::{CString, OsStr};
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path};
use std::thread;

use cygnus_cage::{Cage, CageSpec};

const LOG_LIMIT: u64 = 4 * 1024 * 1024;
const DRAIN_BUFFER_SIZE: usize = 64 * 1024;

/// Prepare an app's log capture and boot its cage.
///
/// Callers that also need to prepare an upstream socket should use
/// [`boot_with_logs_and_prepare`].
#[allow(dead_code)]
pub(crate) fn boot_with_logs(
    spec: &CageSpec,
    logs_root: &Path,
    app: &str,
) -> Result<Cage, String> {
    boot_with_logs_and_prepare(spec, logs_root, app, |_| Ok(()))
}

/// Prepare an upstream, capture stdout/stderr, and boot a cage.
///
/// `prepare` runs before any pipe or drainer is created. Log directories are
/// opened component-by-component with `O_NOFOLLOW`; the final app directory is
/// daemon-owned and mode 0700. Each stream is drained independently into a
/// mode-0600 live file and rotated before it can exceed 4 MiB.
#[allow(dead_code)]
pub(crate) fn boot_with_logs_and_prepare(
    spec: &CageSpec,
    logs_root: &Path,
    app: &str,
    prepare: impl FnOnce(&CageSpec) -> Result<(), String>,
) -> Result<Cage, String> {
    validate_app_name(app)?;
    prepare(spec)?;

    let app_dir = open_log_directory(logs_root, app)
        .map_err(|error| format!("prepare logs for app {app:?}: {error}"))?;
    // Fail the boot synchronously if either live path is unsafe or unwritable.
    // The verified 0700 app directory prevents an untrusted replacement after
    // this check; drainers still reopen with O_NOFOLLOW after every rotation.
    drop(
        open_live_file(&app_dir, "stdout.log")
            .map_err(|error| format!("open stdout log for app {app:?}: {error}"))?,
    );
    drop(
        open_live_file(&app_dir, "stderr.log")
            .map_err(|error| format!("open stderr log for app {app:?}: {error}"))?,
    );
    let stdout_dir = duplicate_fd(&app_dir)
        .map_err(|error| format!("duplicate stdout log directory for app {app:?}: {error}"))?;
    let stderr_dir = duplicate_fd(&app_dir)
        .map_err(|error| format!("duplicate stderr log directory for app {app:?}: {error}"))?;
    let (stdout_read, stdout_write) =
        make_pipe().map_err(|error| format!("create stdout pipe for app {app:?}: {error}"))?;
    let (stderr_read, stderr_write) =
        make_pipe().map_err(|error| format!("create stderr pipe for app {app:?}: {error}"))?;

    spawn_drain(stdout_read, stdout_dir, "stdout.log")
        .map_err(|error| format!("start stdout drainer for app {app:?}: {error}"))?;
    spawn_drain(stderr_read, stderr_dir, "stderr.log")
        .map_err(|error| format!("start stderr drainer for app {app:?}: {error}"))?;

    // Cage takes ownership of the write ends. On any boot error, its arguments
    // are dropped, closing both writers so the detached drainers observe EOF.
    Cage::boot_with_output(spec.clone(), stdout_write, stderr_write)
        .map_err(|error| format!("boot app {app:?}: {error}"))
}

fn validate_app_name(app: &str) -> Result<(), String> {
    let mut bytes = app.bytes();
    let Some(first) = bytes.next() else {
        return Err("app name must not be empty".into());
    };
    if app.len() > 128 {
        return Err("app name must be at most 128 bytes".into());
    }
    if !first.is_ascii_alphanumeric() {
        return Err("app name must begin with an ASCII letter or digit".into());
    }
    if !bytes.all(|byte| {
        byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')
    }) {
        return Err("app name contains unsupported characters".into());
    }
    Ok(())
}

fn open_log_directory(logs_root: &Path, app: &str) -> io::Result<OwnedFd> {
    if !logs_root.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "logs root must be absolute",
        ));
    }

    let mut current = open_directory(Path::new("/"))?;
    for component in logs_root.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(name) => {
                current = open_or_create_directory(&current, name, 0o700)?;
            }
            Component::CurDir | Component::ParentDir | Component::Prefix(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "logs root must not contain '.', '..', or a platform prefix",
                ));
            }
        }
    }

    let app_dir = open_or_create_directory(&current, OsStr::new(app), 0o700)?;
    require_owned_directory(&app_dir)?;
    set_mode(&app_dir, 0o700)?;
    Ok(app_dir)
}

fn open_directory(path: &Path) -> io::Result<OwnedFd> {
    let path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))?;
    cvt_fd(unsafe {
        // SAFETY: `path` is a valid, NUL-terminated C string. The returned fd
        // is uniquely owned by the `OwnedFd` constructed in `cvt_fd`.
        libc::open(
            path.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    })
}

fn open_or_create_directory(
    parent: &OwnedFd,
    name: &OsStr,
    mode: libc::mode_t,
) -> io::Result<OwnedFd> {
    let name = c_name(name)?;
    let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    let opened = unsafe {
        // SAFETY: `parent` is live for this call and `name` is NUL-terminated.
        libc::openat(parent.as_raw_fd(), name.as_ptr(), flags)
    };
    if opened >= 0 {
        return cvt_fd(opened);
    }

    let error = io::Error::last_os_error();
    if error.kind() != io::ErrorKind::NotFound {
        return Err(error);
    }
    let created = unsafe {
        // SAFETY: arguments meet mkdirat(2)'s requirements.
        libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), mode)
    };
    if created < 0 {
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::AlreadyExists {
            return Err(error);
        }
    }
    cvt_fd(unsafe {
        // SAFETY: `parent` is live for this call and `name` is NUL-terminated.
        libc::openat(parent.as_raw_fd(), name.as_ptr(), flags)
    })
}

fn require_owned_directory(directory: &OwnedFd) -> io::Result<()> {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::zeroed();
    let result = unsafe {
        // SAFETY: `stat` points to writable storage and `directory` is live.
        libc::fstat(directory.as_raw_fd(), stat.as_mut_ptr())
    };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    let stat = unsafe {
        // SAFETY: fstat succeeded and initialized the structure.
        stat.assume_init()
    };
    if stat.st_uid != unsafe { libc::geteuid() } {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "app log directory is not owned by the daemon user",
        ));
    }
    if stat.st_mode & libc::S_IFMT != libc::S_IFDIR {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "app log path is not a directory",
        ));
    }
    Ok(())
}

fn set_mode(fd: &OwnedFd, mode: libc::mode_t) -> io::Result<()> {
    if unsafe {
        // SAFETY: `fd` is live and mode is a valid permission bit mask.
        libc::fchmod(fd.as_raw_fd(), mode)
    } < 0
    {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn duplicate_fd(fd: &OwnedFd) -> io::Result<OwnedFd> {
    cvt_fd(unsafe {
        // SAFETY: `fd` is live. F_DUPFD_CLOEXEC returns an independent fd.
        libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0)
    })
}

fn make_pipe() -> io::Result<(File, File)> {
    let mut fds = [-1; 2];
    if unsafe {
        // SAFETY: `fds` provides storage for the two descriptors returned by
        // pipe2(2).
        libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC)
    } < 0
    {
        return Err(io::Error::last_os_error());
    }
    let read = unsafe {
        // SAFETY: pipe2 succeeded and ownership of this fd is transferred once.
        File::from_raw_fd(fds[0])
    };
    let write = unsafe {
        // SAFETY: pipe2 succeeded and ownership of this fd is transferred once.
        File::from_raw_fd(fds[1])
    };
    Ok((read, write))
}

fn spawn_drain(
    mut pipe: File,
    directory: OwnedFd,
    live_name: &'static str,
) -> io::Result<()> {
    let thread_name = format!("cygnus-{}-drain", live_name.trim_end_matches(".log"));
    thread::Builder::new()
        .name(thread_name)
        .spawn(move || {
            if let Err(error) = drain(&mut pipe, &directory, live_name) {
                eprintln!("cygnus: {live_name} drainer stopped: {error}");
            }
        })?;
    Ok(())
}

fn drain(
    pipe: &mut impl Read,
    directory: &OwnedFd,
    live_name: &str,
) -> io::Result<()> {
    let rotated_name = format!("{live_name}.1");
    let mut live = open_live_file(directory, live_name)?;
    let mut length = live.metadata()?.len();
    if length > LOG_LIMIT {
        rotate(directory, live_name, &rotated_name, live)?;
        live = open_live_file(directory, live_name)?;
        length = 0;
    }

    let mut buffer = [0_u8; DRAIN_BUFFER_SIZE];
    loop {
        let read = match pipe.read(&mut buffer) {
            Ok(0) => return Ok(()),
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        let mut offset = 0;
        while offset < read {
            if length == LOG_LIMIT {
                rotate(directory, live_name, &rotated_name, live)?;
                live = open_live_file(directory, live_name)?;
                length = 0;
            }
            let available = (LOG_LIMIT - length) as usize;
            let count = available.min(read - offset);
            live.write_all(&buffer[offset..offset + count])?;
            length += count as u64;
            offset += count;
        }
    }
}

fn open_live_file(directory: &OwnedFd, name: &str) -> io::Result<File> {
    let name = c_name(OsStr::new(name))?;
    let flags = libc::O_WRONLY
        | libc::O_APPEND
        | libc::O_CREAT
        | libc::O_NOFOLLOW
        | libc::O_CLOEXEC;
    let fd = unsafe {
        // SAFETY: `directory` is live and `name` is NUL-terminated.
        libc::openat(directory.as_raw_fd(), name.as_ptr(), flags, 0o600)
    };
    let file = File::from(cvt_fd(fd)?);
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "live log is not a regular file",
        ));
    }
    set_mode_file(&file, 0o600)?;
    Ok(file)
}

fn set_mode_file(file: &File, mode: libc::mode_t) -> io::Result<()> {
    if unsafe {
        // SAFETY: `file` owns a live descriptor and mode contains permissions.
        libc::fchmod(file.as_raw_fd(), mode)
    } < 0
    {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn rotate(
    directory: &OwnedFd,
    live_name: &str,
    rotated_name: &str,
    live: File,
) -> io::Result<()> {
    live.sync_data()?;
    drop(live);

    let live_name = c_name(OsStr::new(live_name))?;
    let rotated_name = c_name(OsStr::new(rotated_name))?;
    let removed = unsafe {
        // SAFETY: unlinkat removes the directory entry itself and never follows
        // a symlink at the rotated name.
        libc::unlinkat(directory.as_raw_fd(), rotated_name.as_ptr(), 0)
    };
    if removed < 0 {
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::NotFound {
            return Err(error);
        }
    }
    if unsafe {
        // SAFETY: both names are NUL-terminated and resolved beneath the same
        // already-open, verified app directory.
        libc::renameat(
            directory.as_raw_fd(),
            live_name.as_ptr(),
            directory.as_raw_fd(),
            rotated_name.as_ptr(),
        )
    } < 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn c_name(name: &OsStr) -> io::Result<CString> {
    let bytes = name.as_bytes();
    if bytes.is_empty() || bytes.contains(&0) || bytes.contains(&b'/') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "directory entry name is empty or contains '/' or NUL",
        ));
    }
    CString::new(bytes)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "name contains NUL"))
}

fn cvt_fd(fd: RawFd) -> io::Result<OwnedFd> {
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe {
            // SAFETY: a non-negative fd returned by the immediately preceding
            // libc call is uniquely transferred to this OwnedFd.
            OwnedFd::from_raw_fd(fd)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "cygnus-runtime-logs-{label}-{}-{nonce}",
                std::process::id()
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn rotates_at_four_mib_and_retains_tail_in_live_file() {
        let root = TempDir::new("rotation");
        let directory = open_log_directory(&root.0, "app").unwrap();
        let input = vec![b'x'; LOG_LIMIT as usize + 17];

        drain(&mut input.as_slice(), &directory, "stdout.log").unwrap();

        let app_dir = root.0.join("app");
        assert_eq!(
            fs::metadata(app_dir.join("stdout.log.1")).unwrap().len(),
            LOG_LIMIT
        );
        assert_eq!(
            fs::read(app_dir.join("stdout.log")).unwrap(),
            vec![b'x'; 17]
        );
    }

    #[test]
    fn creates_private_directory_and_log_permissions() {
        let root = TempDir::new("permissions");
        fs::set_permissions(&root.0, fs::Permissions::from_mode(0o755)).unwrap();
        let directory = open_log_directory(&root.0, "app").unwrap();
        drain(&mut &b"hello"[..], &directory, "stderr.log").unwrap();

        let app_dir = root.0.join("app");
        assert_eq!(fs::metadata(&app_dir).unwrap().mode() & 0o777, 0o700);
        assert_eq!(
            fs::metadata(app_dir.join("stderr.log")).unwrap().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn rejects_unsafe_app_names() {
        for name in [
            "",
            ".",
            "..",
            "../escape",
            "/absolute",
            "has/slash",
            "has space",
            "é",
        ] {
            assert!(validate_app_name(name).is_err(), "accepted {name:?}");
        }
        for name in ["app", "app-1", "A_b.c"] {
            assert!(validate_app_name(name).is_ok(), "rejected {name:?}");
        }
    }

    #[test]
    fn refuses_symlink_log_files() {
        let root = TempDir::new("symlink");
        let directory = open_log_directory(&root.0, "app").unwrap();
        let outside = root.0.join("outside");
        fs::write(&outside, b"unchanged").unwrap();
        symlink(&outside, root.0.join("app/stdout.log")).unwrap();

        let error = drain(&mut &b"attack"[..], &directory, "stdout.log").unwrap_err();
        assert!(error.raw_os_error().is_some());
        assert_eq!(fs::read(outside).unwrap(), b"unchanged");
    }

    #[test]
    fn app_directory_is_owned_by_current_user() {
        let root = TempDir::new("ownership");
        let _directory = open_log_directory(&root.0, "app").unwrap();
        assert_eq!(
            fs::metadata(root.0.join("app")).unwrap().uid(),
            unsafe { libc::geteuid() }
        );
    }
}
