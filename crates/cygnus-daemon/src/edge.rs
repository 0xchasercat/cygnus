use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

pub const DEFAULT_ACME_DIRECTORY: &str = "https://acme-v02.api.letsencrypt.org/directory";
const MAX_CERTIFICATE_BYTES: usize = 1024 * 1024;
const MAX_PRIVATE_KEY_BYTES: usize = 256 * 1024;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SslMode {
    Acme,
    #[default]
    SelfSigned,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct EdgeConfig {
    #[serde(default)]
    pub https_listen: Option<std::net::SocketAddr>,
    #[serde(default)]
    pub apps_domain: Option<String>,
    #[serde(default)]
    pub dashboard_domain: Option<String>,
    #[serde(default)]
    pub apex_domain: Option<String>,
    #[serde(default)]
    pub ssl_mode: SslMode,
    #[serde(default)]
    pub acme: Option<AcmeConfig>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AcmeConfig {
    pub email: String,
    #[serde(default = "default_acme_directory")]
    pub directory_url: String,
    #[serde(default)]
    pub dns_provider: Option<String>,
}

fn default_acme_directory() -> String {
    DEFAULT_ACME_DIRECTORY.into()
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CertificateInput {
    pub id: String,
    pub domains: Vec<String>,
    pub certificate_pem: Vec<u8>,
    pub private_key_pem: Vec<u8>,
    pub not_after_unix: i64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CertificateRecord {
    pub id: String,
    pub domains: Vec<String>,
    pub generation: String,
    pub certificate_path: PathBuf,
    pub private_key_path: PathBuf,
    pub not_after_unix: i64,
    pub installed_at: String,
}

#[derive(Debug, Error)]
pub enum CertificateStoreError {
    #[error("invalid certificate material: {0}")]
    Invalid(String),
    #[error("certificate store filesystem error: {0}")]
    Io(#[from] io::Error),
}

#[derive(Clone, Debug)]
pub(crate) struct PublishedCertificate {
    pub generation: String,
    pub certificate_path: PathBuf,
    pub private_key_path: PathBuf,
}

#[derive(Clone, Debug)]
pub(crate) struct CertificateStore {
    root: PathBuf,
}

impl CertificateStore {
    pub(crate) fn for_state_database(path: &Path) -> Self {
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        Self {
            root: parent.join("certificates"),
        }
    }

    pub(crate) fn paths(&self, id: &str, generation: &str) -> PublishedCertificate {
        let directory = self.root.join(id).join(generation);
        PublishedCertificate {
            generation: generation.into(),
            certificate_path: directory.join("fullchain.pem"),
            private_key_path: directory.join("key.pem"),
        }
    }

    pub(crate) fn resolve(
        &self,
        id: &str,
        generation: &str,
    ) -> Result<PublishedCertificate, CertificateStoreError> {
        validate_id(id)?;
        validate_generation(generation)?;
        verify_secure_directory(&self.root)?;
        verify_secure_directory(&self.root.join(id))?;
        let published = self.paths(id, generation);
        let directory = published
            .certificate_path
            .parent()
            .expect("certificate path has a generation directory");
        verify_secure_directory(directory)?;
        verify_secure_files(&published.certificate_path, &published.private_key_path)?;
        Ok(published)
    }

    pub(crate) fn publish(
        &self,
        id: &str,
        certificate_pem: &[u8],
        private_key_pem: &[u8],
    ) -> Result<PublishedCertificate, CertificateStoreError> {
        validate_id(id)?;
        validate_pem(certificate_pem, private_key_pem)?;
        prepare_secure_directory(&self.root)?;
        let certificate_root = self.root.join(id);
        prepare_secure_directory(&certificate_root)?;

        let generation = generation(certificate_pem, private_key_pem);
        let published = self.paths(id, &generation);
        let final_directory = published
            .certificate_path
            .parent()
            .expect("certificate path has a generation directory");
        if final_directory.exists() {
            validate_existing(final_directory, certificate_pem, private_key_pem)?;
            return Ok(published);
        }

        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let temporary = certificate_root.join(format!(".tmp-{}-{nonce}", std::process::id()));
        fs::create_dir(&temporary)?;
        fs::set_permissions(&temporary, fs::Permissions::from_mode(0o700))?;
        let result: Result<(), CertificateStoreError> = (|| {
            write_secure(&temporary.join("fullchain.pem"), certificate_pem)?;
            write_secure(&temporary.join("key.pem"), private_key_pem)?;
            File::open(&temporary)?.sync_all()?;
            match fs::rename(&temporary, final_directory) {
                Ok(()) => {}
                Err(_) if final_directory.exists() => {
                    fs::remove_dir_all(&temporary)?;
                    validate_existing(final_directory, certificate_pem, private_key_pem)?;
                }
                Err(error) => return Err(error.into()),
            }
            File::open(&certificate_root)?.sync_all()?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_dir_all(&temporary);
        }
        result?;
        Ok(published)
    }
}

fn validate_id(id: &str) -> Result<(), CertificateStoreError> {
    if id.is_empty()
        || id.len() > 64
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err(CertificateStoreError::Invalid(
            "certificate id must be 1-64 lowercase alphanumeric or '-' characters".into(),
        ));
    }
    Ok(())
}

fn validate_pem(
    certificate_pem: &[u8],
    private_key_pem: &[u8],
) -> Result<(), CertificateStoreError> {
    if certificate_pem.is_empty()
        || certificate_pem.len() > MAX_CERTIFICATE_BYTES
        || !certificate_pem
            .windows(b"-----BEGIN CERTIFICATE-----".len())
            .any(|window| window == b"-----BEGIN CERTIFICATE-----")
        || !certificate_pem
            .windows(b"-----END CERTIFICATE-----".len())
            .any(|window| window == b"-----END CERTIFICATE-----")
    {
        return Err(CertificateStoreError::Invalid(
            "certificate PEM is missing a bounded certificate block".into(),
        ));
    }
    if private_key_pem.is_empty()
        || private_key_pem.len() > MAX_PRIVATE_KEY_BYTES
        || !private_key_pem
            .windows(b"-----BEGIN ".len())
            .any(|window| window == b"-----BEGIN ")
        || !private_key_pem
            .windows(b"PRIVATE KEY-----".len())
            .any(|window| window == b"PRIVATE KEY-----")
    {
        return Err(CertificateStoreError::Invalid(
            "private-key PEM is missing a bounded private-key block".into(),
        ));
    }
    Ok(())
}

fn generation(certificate_pem: &[u8], private_key_pem: &[u8]) -> String {
    let mut digest = Sha256::new();
    digest.update((certificate_pem.len() as u64).to_be_bytes());
    digest.update(certificate_pem);
    digest.update((private_key_pem.len() as u64).to_be_bytes());
    digest.update(private_key_pem);
    format!("{:x}", digest.finalize())
}

fn validate_generation(generation: &str) -> Result<(), CertificateStoreError> {
    if generation.len() != 64
        || !generation
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(CertificateStoreError::Invalid(
            "certificate generation must be lowercase SHA-256 hex".into(),
        ));
    }
    Ok(())
}

fn prepare_secure_directory(path: &Path) -> Result<(), CertificateStoreError> {
    match fs::symlink_metadata(path) {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::create_dir_all(path)?;
            fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
        }
        Err(error) => return Err(error.into()),
    }
    verify_secure_directory(path)
}

fn verify_secure_directory(path: &Path) -> Result<(), CertificateStoreError> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_dir()
        || metadata.permissions().mode() & 0o777 != 0o700
        || metadata.uid() != unsafe { libc::geteuid() }
    {
        return Err(CertificateStoreError::Invalid(format!(
            "certificate directory {} must be daemon-owned with mode 0700",
            path.display()
        )));
    }
    Ok(())
}

fn write_secure(path: &Path, contents: &[u8]) -> Result<(), CertificateStoreError> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(contents)?;
    file.sync_all()?;
    Ok(())
}

fn verify_secure_files(
    certificate_path: &Path,
    private_key_path: &Path,
) -> Result<(), CertificateStoreError> {
    for path in [certificate_path, private_key_path] {
        let metadata = fs::symlink_metadata(path)?;
        if !metadata.file_type().is_file()
            || metadata.permissions().mode() & 0o777 != 0o600
            || metadata.uid() != unsafe { libc::geteuid() }
        {
            return Err(CertificateStoreError::Invalid(format!(
                "certificate file {} must be daemon-owned regular mode 0600 content",
                path.display()
            )));
        }
    }
    Ok(())
}

fn validate_existing(
    directory: &Path,
    certificate_pem: &[u8],
    private_key_pem: &[u8],
) -> Result<(), CertificateStoreError> {
    verify_secure_directory(directory)?;
    let certificate_path = directory.join("fullchain.pem");
    let private_key_path = directory.join("key.pem");
    verify_secure_files(&certificate_path, &private_key_path)?;
    if fs::read(certificate_path)? != certificate_pem
        || fs::read(private_key_path)? != private_key_pem
    {
        return Err(CertificateStoreError::Invalid(
            "published certificate generation does not match its content hash".into(),
        ));
    }
    Ok(())
}
