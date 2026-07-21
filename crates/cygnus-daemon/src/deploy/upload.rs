//! Bounded, daemon-owned spooling for admin deployment uploads.
//!
//! Uploads are reserved up front, written in strict offset order, and remain in
//! the private spool after finalization so an asynchronous deployment worker can
//! consume them. Callers must remove a finalized upload when that worker no
//! longer needs it.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

/// Maximum compressed size of one deployment archive.
pub const MAX_UPLOAD_BYTES: u64 = 64 * 1024 * 1024;
/// Default maximum number of concurrent incomplete uploads.
pub const MAX_ACTIVE_UPLOADS: usize = 8;
/// Default aggregate reservation across concurrent incomplete uploads.
pub const MAX_RESERVED_UPLOAD_BYTES: u64 = MAX_UPLOAD_BYTES;
/// Maximum decoded size of one admin upload chunk.
pub const MAX_UPLOAD_CHUNK_BYTES: usize = 48 * 1024;
/// Incomplete uploads idle for this long are expired.
pub const UPLOAD_STALE_AFTER: Duration = Duration::from_secs(15 * 60);

/// Wire-safe encoded chunk bound, leaving one KiB for the admin JSON envelope.
pub const MAX_UPLOAD_CHUNK_BASE64_CHARS: usize = 63 * 1024;
const UPLOAD_DIRECTORY: &str = "deploy-uploads";
const UPLOAD_ID_BYTES: usize = 32;
const UPLOAD_ID_HEX_LEN: usize = UPLOAD_ID_BYTES * 2;

/// Metadata supplied when an upload is begun.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UploadMetadata {
    pub app: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub env: std::collections::BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
}

/// A complete archive retained in the spool for an asynchronous worker.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct FinalizedUpload {
    pub upload_id: String,
    pub archive_path: PathBuf,
    /// Lowercase hexadecimal SHA-256 of the compressed archive.
    pub digest: String,
    pub metadata: UploadMetadata,
}

#[derive(Debug, Error)]
pub enum UploadError {
    #[error("invalid upload: {0}")]
    InvalidInput(&'static str),
    #[error("upload session capacity exceeded")]
    Capacity,
    #[error("upload session does not exist")]
    NotFound,
    #[error("upload chunk is out of order: expected offset {expected}, got {actual}")]
    OutOfOrder { expected: u64, actual: u64 },
    #[error("upload contains more bytes than promised")]
    Overflow,
    #[error("upload is incomplete: expected {expected} bytes, received {received}")]
    Incomplete { expected: u64, received: u64 },
    #[error(transparent)]
    Io(#[from] io::Error),
}

struct UploadSession {
    path: PathBuf,
    expected: u64,
    received: u64,
    metadata: UploadMetadata,
    updated: SystemTime,
}

struct UploadState {
    active: HashMap<String, UploadSession>,
    finalized: HashMap<String, FinalizedUpload>,
    reserved: u64,
}

struct UploadInner {
    directory: PathBuf,
    max_active: usize,
    max_reserved: u64,
    state: Mutex<UploadState>,
}

impl Drop for UploadInner {
    fn drop(&mut self) {
        if let Ok(state) = self.state.get_mut() {
            for session in state.active.values() {
                let _ = fs::remove_file(&session.path);
            }
        }
    }
}

/// Thread-safe manager for bounded deployment upload sessions.
#[derive(Clone)]
pub struct UploadManager {
    inner: Arc<UploadInner>,
}

impl UploadManager {
    /// Open the daemon's private `deploy-uploads` directory below `daemon_root`.
    ///
    /// Incomplete files left by a prior daemon process are removed. Finalized
    /// archives are retained for deployment workers or explicit cleanup.
    pub fn new(daemon_root: impl AsRef<Path>) -> Result<Self, UploadError> {
        Self::with_limits(daemon_root, MAX_ACTIVE_UPLOADS, MAX_RESERVED_UPLOAD_BYTES)
    }

    /// Construct a manager with explicit concurrency and reservation limits.
    ///
    /// This is primarily useful for installations that need a smaller bound and
    /// for tests. The per-upload 64 MiB bound is never configurable.
    pub fn with_limits(
        daemon_root: impl AsRef<Path>,
        max_active: usize,
        max_reserved: u64,
    ) -> Result<Self, UploadError> {
        if max_active == 0 || max_reserved == 0 {
            return Err(UploadError::InvalidInput(
                "upload capacity limits must be non-zero",
            ));
        }

        let directory = daemon_root.as_ref().join(UPLOAD_DIRECTORY);
        create_private_directory(&directory)?;
        remove_abandoned_partials(&directory)?;

        Ok(Self {
            inner: Arc::new(UploadInner {
                directory,
                max_active,
                max_reserved,
                state: Mutex::new(UploadState {
                    active: HashMap::new(),
                    finalized: HashMap::new(),
                    reserved: 0,
                }),
            }),
        })
    }

    /// Return the private spool directory.
    pub fn directory(&self) -> &Path {
        &self.inner.directory
    }

    /// Reserve and begin a new compressed archive upload.
    pub fn begin(&self, metadata: UploadMetadata, total_bytes: u64) -> Result<String, UploadError> {
        validate_metadata(&metadata)?;
        if total_bytes == 0 || total_bytes > MAX_UPLOAD_BYTES {
            return Err(UploadError::InvalidInput(
                "total_bytes must be between 1 byte and 64 MiB",
            ));
        }
        self.cleanup_expired()?;

        let mut state = self.lock_state()?;
        let reserved = state
            .reserved
            .checked_add(total_bytes)
            .ok_or(UploadError::Capacity)?;
        if state.active.len() >= self.inner.max_active || reserved > self.inner.max_reserved {
            return Err(UploadError::Capacity);
        }

        let (upload_id, path) = self.create_partial_file()?;
        state.reserved = reserved;
        state.active.insert(
            upload_id.clone(),
            UploadSession {
                path,
                expected: total_bytes,
                received: 0,
                metadata,
                updated: SystemTime::now(),
            },
        );
        Ok(upload_id)
    }

    /// Decode and append one base64 chunk at the session's next byte offset.
    ///
    /// This is the wire-protocol operation: upload chunks are strictly serial and
    /// the daemon, rather than the client, owns the current offset.
    pub fn append_next(&self, upload_id: &str, chunk_base64: &str) -> Result<u64, UploadError> {
        self.append_at(upload_id, None, chunk_base64)
    }

    /// Decode and append one base64 chunk at the exact expected byte offset.
    ///
    /// Invalid base64, an incorrect offset, overflow beyond `total_bytes`, or a
    /// write failure aborts the session and removes its partial file.
    pub fn append(
        &self,
        upload_id: &str,
        offset: u64,
        chunk_base64: &str,
    ) -> Result<u64, UploadError> {
        self.append_at(upload_id, Some(offset), chunk_base64)
    }

    fn append_at(
        &self,
        upload_id: &str,
        offset: Option<u64>,
        chunk_base64: &str,
    ) -> Result<u64, UploadError> {
        validate_upload_id(upload_id)?;
        self.cleanup_expired()?;

        if chunk_base64.is_empty() || chunk_base64.len() > MAX_UPLOAD_CHUNK_BASE64_CHARS {
            self.abort_validated(upload_id)?;
            return Err(UploadError::InvalidInput(
                "upload chunk must decode to between 1 byte and 48 KiB",
            ));
        }
        let bytes = match BASE64.decode(chunk_base64) {
            Ok(bytes) if !bytes.is_empty() && bytes.len() <= MAX_UPLOAD_CHUNK_BYTES => bytes,
            _ => {
                self.abort_validated(upload_id)?;
                return Err(UploadError::InvalidInput(
                    "upload chunk must be non-empty valid base64 of at most 48 KiB",
                ));
            }
        };

        let mut state = self.lock_state()?;
        let Some(session) = state.active.get(upload_id) else {
            return Err(UploadError::NotFound);
        };
        if let Some(offset) = offset
            && offset != session.received
        {
            let expected = session.received;
            remove_active(&mut state, upload_id);
            return Err(UploadError::OutOfOrder {
                expected,
                actual: offset,
            });
        }
        let Some(next) = session.received.checked_add(bytes.len() as u64) else {
            remove_active(&mut state, upload_id);
            return Err(UploadError::Overflow);
        };
        if next > session.expected || next > MAX_UPLOAD_BYTES {
            remove_active(&mut state, upload_id);
            return Err(UploadError::Overflow);
        }

        let path = session.path.clone();
        let expected_length = session.received;
        let write_result = (|| {
            let mut file = OpenOptions::new()
                .append(true)
                .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
                .open(&path)?;
            let file_metadata = file.metadata()?;
            if !file_metadata.file_type().is_file() || file_metadata.len() != expected_length {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "upload partial changed during intake",
                ));
            }
            file.write_all(&bytes)?;
            file.sync_data()
        })();
        if let Err(error) = write_result {
            remove_active(&mut state, upload_id);
            return Err(UploadError::Io(error));
        }

        if let Some(session) = state.active.get_mut(upload_id) {
            session.received = next;
            session.updated = SystemTime::now();
        }
        Ok(next)
    }

    /// Finalize an exactly complete upload and return its archive description.
    ///
    /// Repeating `finish` for the same upload is safe and returns the same
    /// result. The archive remains in the spool until [`Self::remove`] is called.
    pub fn finish(&self, upload_id: &str) -> Result<FinalizedUpload, UploadError> {
        validate_upload_id(upload_id)?;
        self.cleanup_expired()?;

        let mut state = self.lock_state()?;
        if let Some(upload) = state.finalized.get(upload_id) {
            return Ok(upload.clone());
        }
        let Some(session) = state.active.remove(upload_id) else {
            return Err(UploadError::NotFound);
        };
        state.reserved = state.reserved.saturating_sub(session.expected);

        if session.received != session.expected {
            let _ = fs::remove_file(&session.path);
            return Err(UploadError::Incomplete {
                expected: session.expected,
                received: session.received,
            });
        }

        let partial_path = session.path.clone();
        let archive_path = archive_path(&self.inner.directory, upload_id);
        let mut renamed = false;
        let result = (|| {
            if fs::symlink_metadata(&archive_path).is_ok() {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "finalized upload path already exists",
                ));
            }
            let digest = digest_file(&partial_path, session.expected)?;
            fs::rename(&partial_path, &archive_path)?;
            renamed = true;
            sync_directory(&self.inner.directory)?;
            Ok(FinalizedUpload {
                upload_id: upload_id.to_owned(),
                archive_path: archive_path.clone(),
                digest,
                metadata: session.metadata,
            })
        })();

        match result {
            Ok(upload) => {
                state.finalized.insert(upload_id.to_owned(), upload.clone());
                Ok(upload)
            }
            Err(error) => {
                let _ = fs::remove_file(&partial_path);
                if renamed {
                    let _ = fs::remove_file(&archive_path);
                }
                Err(UploadError::Io(error))
            }
        }
    }

    /// Abort an incomplete upload and remove its partial file.
    pub fn abort(&self, upload_id: &str) -> Result<bool, UploadError> {
        validate_upload_id(upload_id)?;
        self.cleanup_expired()?;
        self.abort_validated(upload_id)
    }

    /// Remove either an incomplete upload or a retained finalized archive.
    ///
    /// Returns `true` when a tracked upload was removed. This operation is
    /// idempotent; an already removed upload returns `false`.
    pub fn remove(&self, upload_id: &str) -> Result<bool, UploadError> {
        validate_upload_id(upload_id)?;
        self.cleanup_expired()?;

        let mut state = self.lock_state()?;
        if state.active.contains_key(upload_id) {
            remove_active(&mut state, upload_id);
            return Ok(true);
        }
        if let Some(upload) = state.finalized.get(upload_id).cloned() {
            match fs::remove_file(&upload.archive_path) {
                Ok(()) => sync_directory(&self.inner.directory)?,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(UploadError::Io(error)),
            }
            state.finalized.remove(upload_id);
            return Ok(true);
        }

        // A worker may clean up after a daemon restart, when the in-memory
        // finalized map is necessarily empty. The strict opaque-id grammar
        // keeps this path confined to one known spool filename.
        let archive = archive_path(&self.inner.directory, upload_id);
        match fs::remove_file(archive) {
            Ok(()) => {
                sync_directory(&self.inner.directory)?;
                Ok(true)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(UploadError::Io(error)),
        }
    }

    /// Expire idle incomplete sessions and remove their partial files.
    pub fn cleanup_expired(&self) -> Result<usize, UploadError> {
        let cutoff = SystemTime::now()
            .checked_sub(UPLOAD_STALE_AFTER)
            .unwrap_or(UNIX_EPOCH);
        let mut state = self.lock_state()?;
        let expired = state
            .active
            .iter()
            .filter_map(|(id, session)| (session.updated <= cutoff).then_some(id.clone()))
            .collect::<Vec<_>>();
        for upload_id in &expired {
            remove_active(&mut state, upload_id);
        }
        Ok(expired.len())
    }

    fn create_partial_file(&self) -> Result<(String, PathBuf), UploadError> {
        for _ in 0..32 {
            let upload_id = random_upload_id()?;
            let partial = partial_path(&self.inner.directory, &upload_id);
            let archive = archive_path(&self.inner.directory, &upload_id);
            if fs::symlink_metadata(&archive).is_ok() {
                continue;
            }
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
                .open(&partial)
            {
                Ok(file) => {
                    file.sync_data()?;
                    return Ok((upload_id, partial));
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(UploadError::Io(error)),
            }
        }
        Err(UploadError::Io(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not allocate a unique upload id",
        )))
    }

    fn abort_validated(&self, upload_id: &str) -> Result<bool, UploadError> {
        let mut state = self.lock_state()?;
        if !state.active.contains_key(upload_id) {
            return Ok(false);
        }
        remove_active(&mut state, upload_id);
        Ok(true)
    }

    fn lock_state(&self) -> Result<MutexGuard<'_, UploadState>, UploadError> {
        self.inner
            .state
            .lock()
            .map_err(|_| UploadError::Io(io::Error::other("upload manager state lock is poisoned")))
    }
}

fn remove_active(state: &mut UploadState, upload_id: &str) {
    if let Some(session) = state.active.remove(upload_id) {
        state.reserved = state.reserved.saturating_sub(session.expected);
        let _ = fs::remove_file(session.path);
    }
}

fn validate_upload_id(upload_id: &str) -> Result<(), UploadError> {
    if upload_id.len() != UPLOAD_ID_HEX_LEN
        || !upload_id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(UploadError::InvalidInput("upload id is invalid"));
    }
    Ok(())
}

fn validate_metadata(metadata: &UploadMetadata) -> Result<(), UploadError> {
    validate_text(&metadata.app, 128, "app is invalid")?;
    if let Some(domain) = &metadata.domain {
        validate_text(domain, 253, "domain is invalid")?;
    }
    if let Some(engine_version) = &metadata.engine_version {
        validate_text(engine_version, 128, "engine_version is invalid")?;
    }
    if let Some(entry) = &metadata.entry
        && (entry.as_os_str().is_empty()
            || entry.is_absolute()
            || entry.components().any(|component| {
                matches!(
                    component,
                    Component::ParentDir | Component::RootDir | Component::Prefix(_)
                )
            }))
    {
        return Err(UploadError::InvalidInput(
            "entry must be a relative path without parent traversal",
        ));
    }
    Ok(())
}

fn validate_text(value: &str, max_len: usize, message: &'static str) -> Result<(), UploadError> {
    if value.trim().is_empty() || value.len() > max_len || value.chars().any(char::is_control) {
        return Err(UploadError::InvalidInput(message));
    }
    Ok(())
}

fn random_upload_id() -> Result<String, UploadError> {
    let mut bytes = [0_u8; UPLOAD_ID_BYTES];
    getrandom::fill(&mut bytes).map_err(|error| {
        UploadError::Io(io::Error::other(format!(
            "could not generate upload id: {error}"
        )))
    })?;
    Ok(hex::encode(bytes))
}

fn partial_path(directory: &Path, upload_id: &str) -> PathBuf {
    directory.join(format!("{upload_id}.part"))
}

fn archive_path(directory: &Path, upload_id: &str) -> PathBuf {
    directory.join(format!("{upload_id}.archive"))
}

fn create_private_directory(directory: &Path) -> io::Result<()> {
    loop {
        match fs::symlink_metadata(directory) {
            Ok(metadata) => {
                if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "deploy upload path is not a directory",
                    ));
                }
                return fs::set_permissions(directory, fs::Permissions::from_mode(0o700));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let mut builder = fs::DirBuilder::new();
                match builder.mode(0o700).create(directory) {
                    Ok(()) => return Ok(()),
                    // Lost a creation race: loop back and validate whatever
                    // now exists instead of failing initialization.
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(error) => return Err(error),
                }
            }
            Err(error) => return Err(error),
        }
    }
}

fn remove_abandoned_partials(directory: &Path) -> io::Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if name.ends_with(".part") {
            let metadata = fs::symlink_metadata(entry.path())?;
            if metadata.file_type().is_file() || metadata.file_type().is_symlink() {
                fs::remove_file(entry.path())?;
            }
        }
    }
    Ok(())
}

fn digest_file(path: &Path, expected: u64) -> io::Result<String> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file() || metadata.len() != expected {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "upload archive changed during finalization",
        ));
    }
    let mut hasher = Sha256::new();
    let mut copied = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        copied = copied
            .checked_add(read as u64)
            .ok_or_else(|| io::Error::other("upload archive size overflow while hashing"))?;
        if copied > expected {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "upload archive grew during hashing",
            ));
        }
        hasher.update(&buffer[..read]);
    }
    if copied != expected {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "upload archive changed during hashing",
        ));
    }
    Ok(hex::encode(hasher.finalize()))
}

fn sync_directory(directory: &Path) -> io::Result<()> {
    File::open(directory)?.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_NONCE: AtomicU64 = AtomicU64::new(0);

    struct TestRoot(PathBuf);

    impl TestRoot {
        fn new(label: &str) -> Self {
            let nonce = TEST_NONCE.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "cygnus-upload-{label}-{}-{nonce}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn metadata() -> UploadMetadata {
        UploadMetadata {
            app: "hello".into(),
            domain: Some("hello.example".into()),
            engine_version: Some("1.0.0".into()),
            entry: Some("src/index.ts".into()),
            env: Default::default(),
            preview: None,
        }
    }

    #[test]
    fn exact_64_mib_limit_is_accepted_and_one_more_is_rejected() {
        let root = TestRoot::new("limit");
        let manager = UploadManager::new(&root.0).unwrap();
        let upload_id = manager.begin(metadata(), MAX_UPLOAD_BYTES).unwrap();
        assert!(manager.abort(&upload_id).unwrap());
        assert!(matches!(
            manager.begin(metadata(), MAX_UPLOAD_BYTES + 1),
            Err(UploadError::InvalidInput(_))
        ));
    }

    #[test]
    fn chunk_overflow_aborts_and_cleans_partial() {
        let root = TestRoot::new("overflow");
        let manager = UploadManager::new(&root.0).unwrap();
        let upload_id = manager.begin(metadata(), 2).unwrap();
        let partial = partial_path(manager.directory(), &upload_id);
        let encoded = BASE64.encode(b"abc");
        assert!(matches!(
            manager.append(&upload_id, 0, &encoded),
            Err(UploadError::Overflow)
        ));
        assert!(!partial.exists());
        assert!(matches!(
            manager.finish(&upload_id),
            Err(UploadError::NotFound)
        ));
    }

    #[test]
    fn append_next_owns_the_wire_offset() {
        let root = TestRoot::new("append-next");
        let manager = UploadManager::new(&root.0).unwrap();
        let upload_id = manager.begin(metadata(), 4).unwrap();
        assert_eq!(
            manager
                .append_next(&upload_id, &BASE64.encode(b"ab"))
                .unwrap(),
            2
        );
        assert_eq!(
            manager
                .append_next(&upload_id, &BASE64.encode(b"cd"))
                .unwrap(),
            4
        );
        assert_eq!(manager.finish(&upload_id).unwrap().digest.len(), 64);
    }

    #[test]
    fn short_finish_fails_and_cleans_partial() {
        let root = TestRoot::new("short");
        let manager = UploadManager::new(&root.0).unwrap();
        let upload_id = manager.begin(metadata(), 3).unwrap();
        let partial = partial_path(manager.directory(), &upload_id);
        manager
            .append(&upload_id, 0, &BASE64.encode(b"ab"))
            .unwrap();
        assert!(matches!(
            manager.finish(&upload_id),
            Err(UploadError::Incomplete {
                expected: 3,
                received: 2
            })
        ));
        assert!(!partial.exists());
    }

    #[test]
    fn enforces_session_and_aggregate_capacity() {
        let root = TestRoot::new("capacity-count");
        let manager = UploadManager::with_limits(&root.0, 2, 100).unwrap();
        let first = manager.begin(metadata(), 1).unwrap();
        let second = manager.begin(metadata(), 1).unwrap();
        assert!(matches!(
            manager.begin(metadata(), 1),
            Err(UploadError::Capacity)
        ));
        manager.abort(&first).unwrap();
        manager.abort(&second).unwrap();

        let root = TestRoot::new("capacity-bytes");
        let manager = UploadManager::with_limits(&root.0, 3, 10).unwrap();
        let first = manager.begin(metadata(), 6).unwrap();
        assert!(matches!(
            manager.begin(metadata(), 5),
            Err(UploadError::Capacity)
        ));
        manager.abort(&first).unwrap();
        assert!(manager.begin(metadata(), 10).is_ok());
    }

    #[test]
    fn expires_stale_uploads_and_releases_reservation() {
        let root = TestRoot::new("expiry");
        let manager = UploadManager::with_limits(&root.0, 1, 4).unwrap();
        let upload_id = manager.begin(metadata(), 4).unwrap();
        let partial = partial_path(manager.directory(), &upload_id);
        {
            let mut state = manager.lock_state().unwrap();
            state.active.get_mut(&upload_id).unwrap().updated = UNIX_EPOCH;
        }
        assert_eq!(manager.cleanup_expired().unwrap(), 1);
        assert!(!partial.exists());
        assert!(manager.begin(metadata(), 4).is_ok());
    }

    #[test]
    fn creates_private_directory_and_files() {
        let root = TestRoot::new("permissions");
        let manager = UploadManager::new(&root.0).unwrap();
        let upload_id = manager.begin(metadata(), 1).unwrap();
        let directory_mode = fs::metadata(manager.directory())
            .unwrap()
            .permissions()
            .mode();
        let file_mode = fs::metadata(partial_path(manager.directory(), &upload_id))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(directory_mode & 0o7777, 0o700);
        assert_eq!(file_mode & 0o7777, 0o600);
    }

    #[test]
    fn rejects_unsafe_upload_ids_without_filesystem_access() {
        let root = TestRoot::new("unsafe-id");
        let manager = UploadManager::new(&root.0).unwrap();
        for upload_id in ["../outside", "/absolute", "ABCDEF", "", "00.part"] {
            assert!(matches!(
                manager.append(upload_id, 0, &BASE64.encode(b"x")),
                Err(UploadError::InvalidInput(_))
            ));
            assert!(matches!(
                manager.remove(upload_id),
                Err(UploadError::InvalidInput(_))
            ));
        }
    }

    #[test]
    fn duplicate_finish_returns_same_finalized_upload() {
        let root = TestRoot::new("duplicate-finish");
        let manager = UploadManager::new(&root.0).unwrap();
        let bytes = b"compressed archive";
        let upload_id = manager.begin(metadata(), bytes.len() as u64).unwrap();
        manager
            .append(&upload_id, 0, &BASE64.encode(bytes))
            .unwrap();

        let first = manager.finish(&upload_id).unwrap();
        let second = manager.finish(&upload_id).unwrap();
        assert_eq!(first, second);
        assert_eq!(first.digest, hex::encode(Sha256::digest(bytes)));
        assert_eq!(fs::read(&first.archive_path).unwrap(), bytes);
        assert!(manager.remove(&upload_id).unwrap());
        assert!(!first.archive_path.exists());
        assert!(!manager.remove(&upload_id).unwrap());
    }
}
