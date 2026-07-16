use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::prelude::{BASE64_URL_SAFE_NO_PAD, Engine};
use cygnus_router::{RequestHead, normalize_host};
use instant_acme::{
    Account, AccountCredentials, ChallengeType, Identifier, NewAccount, NewOrder, RetryPolicy,
};
use parking_lot::RwLock;
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use thiserror::Error;
use x509_parser::parse_x509_certificate;

use crate::edge::{AcmeConfig, CertificateInput};

const CHALLENGE_PREFIX: &str = "/.well-known/acme-challenge/";
const MAX_CHALLENGE_TOKEN_BYTES: usize = 256;
const MAX_KEY_AUTHORIZATION_BYTES: usize = 1024;

#[derive(Clone, Debug, Default)]
pub struct Http01Challenges {
    inner: Arc<RwLock<BTreeMap<(String, String), String>>>,
}

impl Http01Challenges {
    pub fn insert(&self, host: &str, token: &str, authorization: &str) -> Result<(), AcmeError> {
        let host = normalize_host(host);
        if host.is_empty()
            || token.is_empty()
            || token.len() > MAX_CHALLENGE_TOKEN_BYTES
            || authorization.is_empty()
            || authorization.len() > MAX_KEY_AUTHORIZATION_BYTES
            || token
                .bytes()
                .any(|byte| !byte.is_ascii_alphanumeric() && !matches!(byte, b'-' | b'_'))
            || authorization.bytes().any(|byte| byte.is_ascii_control())
        {
            return Err(AcmeError::Invalid(
                "HTTP-01 challenge host, token, or authorization is invalid".into(),
            ));
        }
        self.inner
            .write()
            .insert((host, token.into()), authorization.into());
        Ok(())
    }

    pub fn remove(&self, host: &str, token: &str) {
        self.inner
            .write()
            .remove(&(normalize_host(host), token.into()));
    }

    pub fn response(&self, request: &RequestHead) -> Option<Vec<u8>> {
        if request.method != "GET" {
            return None;
        }
        let token = request.target.strip_prefix(CHALLENGE_PREFIX)?;
        if token.is_empty() || token.contains('/') || token.len() > MAX_CHALLENGE_TOKEN_BYTES {
            return None;
        }
        let host = normalize_host(request.host.as_deref()?);
        let authorization = self.inner.read().get(&(host, token.into()))?.clone();
        Some(format!(
            "HTTP/1.1 200 OK\r\nconnection: close\r\ncontent-type: application/octet-stream\r\ncontent-length: {}\r\n\r\n{}",
            authorization.len(), authorization
        ).into_bytes())
    }
}

pub trait Dns01Provider: Send + Sync + 'static {
    fn present(&self, record: &str, value: &str) -> Result<(), AcmeError>;
    fn cleanup(&self, record: &str, value: &str) -> Result<(), AcmeError>;
}

pub struct CloudflareDnsProvider {
    token: String,
    api_base: String,
    dns_query_base: String,
}

impl CloudflareDnsProvider {
    pub fn from_environment() -> Result<Self, AcmeError> {
        let token = std::env::var("CYGNUS_CLOUDFLARE_API_TOKEN")
            .map_err(|_| AcmeError::Dns("CYGNUS_CLOUDFLARE_API_TOKEN is not set".into()))?;
        if token.trim().is_empty() {
            return Err(AcmeError::Dns("Cloudflare API token is empty".into()));
        }
        Ok(Self {
            token,
            api_base: "https://api.cloudflare.com/client/v4".into(),
            dns_query_base: "https://cloudflare-dns.com/dns-query".into(),
        })
    }

    #[cfg(test)]
    fn with_endpoint(token: &str, api_base: String) -> Self {
        Self {
            token: token.into(),
            dns_query_base: format!("{api_base}/dns-query"),
            api_base,
        }
    }

    fn zone(&self, record: &str) -> Result<CloudflareZone, AcmeError> {
        let domain = record.strip_prefix("_acme-challenge.").unwrap_or(record);
        let labels = domain.split('.').collect::<Vec<_>>();
        for index in 0..labels.len().saturating_sub(1) {
            let name = labels[index..].join(".");
            let url = format!("{}/zones?name={name}&status=active", self.api_base);
            let mut response = ureq::get(&url)
                .header("Authorization", &format!("Bearer {}", self.token))
                .header("Accept", "application/json")
                .call()
                .map_err(|error| {
                    AcmeError::Dns(format!("Cloudflare zone lookup failed: {error}"))
                })?;
            let envelope: CloudflareEnvelope<Vec<CloudflareZone>> =
                response.body_mut().read_json().map_err(|error| {
                    AcmeError::Dns(format!("decode Cloudflare zone response: {error}"))
                })?;
            envelope.ensure_success()?;
            if let Some(zone) = envelope.result.into_iter().next() {
                return Ok(zone);
            }
        }
        Err(AcmeError::Dns(format!(
            "no accessible Cloudflare zone owns {record:?}"
        )))
    }

    fn records(&self, zone: &str, record: &str) -> Result<Vec<CloudflareRecord>, AcmeError> {
        let url = format!(
            "{}/zones/{zone}/dns_records?type=TXT&name={record}",
            self.api_base
        );
        let mut response = ureq::get(&url)
            .header("Authorization", &format!("Bearer {}", self.token))
            .header("Accept", "application/json")
            .call()
            .map_err(|error| AcmeError::Dns(format!("Cloudflare record lookup failed: {error}")))?;
        let envelope: CloudflareEnvelope<Vec<CloudflareRecord>> =
            response.body_mut().read_json().map_err(|error| {
                AcmeError::Dns(format!("decode Cloudflare record response: {error}"))
            })?;
        envelope.ensure_success()?;
        Ok(envelope.result)
    }

    fn wait_for_propagation(&self, record: &str, value: &str) -> Result<(), AcmeError> {
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            let url = format!("{}?name={record}&type=TXT", self.dns_query_base);
            let result = ureq::get(&url)
                .header("Accept", "application/dns-json")
                .call()
                .ok()
                .and_then(|mut response| response.body_mut().read_json::<DnsJsonResponse>().ok());
            if result.is_some_and(|response| {
                response
                    .answers
                    .into_iter()
                    .flatten()
                    .any(|answer| answer.data.trim_matches('"') == value)
            }) {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(AcmeError::Dns(format!(
                    "TXT record {record:?} did not propagate within 120 seconds"
                )));
            }
            thread::sleep(Duration::from_secs(2));
        }
    }
}
impl Dns01Provider for CloudflareDnsProvider {
    fn present(&self, record: &str, value: &str) -> Result<(), AcmeError> {
        let zone = self.zone(record)?;
        if self
            .records(&zone.id, record)?
            .iter()
            .any(|existing| existing.content == value)
        {
            return self.wait_for_propagation(record, value);
        }
        let url = format!("{}/zones/{}/dns_records", self.api_base, zone.id);
        let mut response = ureq::post(&url)
            .header("Authorization", &format!("Bearer {}", self.token))
            .header("Content-Type", "application/json")
            .send_json(json!({ "type": "TXT", "name": record, "content": value, "ttl": 60 }))
            .map_err(|error| AcmeError::Dns(format!("create Cloudflare TXT record: {error}")))?;
        let envelope: CloudflareEnvelope<CloudflareRecord> =
            response.body_mut().read_json().map_err(|error| {
                AcmeError::Dns(format!("decode Cloudflare create response: {error}"))
            })?;
        envelope.ensure_success()?;
        if let Err(error) = self.wait_for_propagation(record, value) {
            let _ = self.cleanup(record, value);
            return Err(error);
        }
        Ok(())
    }

    fn cleanup(&self, record: &str, value: &str) -> Result<(), AcmeError> {
        let zone = self.zone(record)?;
        for existing in self
            .records(&zone.id, record)?
            .into_iter()
            .filter(|item| item.content == value)
        {
            let url = format!(
                "{}/zones/{}/dns_records/{}",
                self.api_base, zone.id, existing.id
            );
            let mut response = ureq::delete(&url)
                .header("Authorization", &format!("Bearer {}", self.token))
                .call()
                .map_err(|error| {
                    AcmeError::Dns(format!("delete Cloudflare TXT record: {error}"))
                })?;
            let envelope: CloudflareEnvelope<serde_json::Value> =
                response.body_mut().read_json().map_err(|error| {
                    AcmeError::Dns(format!("decode Cloudflare delete response: {error}"))
                })?;
            envelope.ensure_success()?;
        }
        Ok(())
    }
}

#[derive(Deserialize)]
struct CloudflareEnvelope<T> {
    success: bool,
    result: T,
    #[serde(default)]
    errors: Vec<CloudflareApiError>,
}

impl<T> CloudflareEnvelope<T> {
    fn ensure_success(&self) -> Result<(), AcmeError> {
        if self.success {
            return Ok(());
        }
        let detail = self
            .errors
            .iter()
            .map(|error| error.message.as_str())
            .collect::<Vec<_>>()
            .join("; ");
        Err(AcmeError::Dns(format!(
            "Cloudflare API rejected the request: {detail}"
        )))
    }
}

#[derive(Deserialize)]
struct CloudflareApiError {
    message: String,
}

#[derive(Deserialize)]
struct CloudflareZone {
    id: String,
}

#[derive(Deserialize)]
struct CloudflareRecord {
    id: String,
    content: String,
}

#[derive(Deserialize)]
struct DnsJsonResponse {
    #[serde(rename = "Answer")]
    answers: Option<Vec<DnsJsonAnswer>>,
}

#[derive(Deserialize)]
struct DnsJsonAnswer {
    data: String,
}

pub struct AcmeManager {
    config: AcmeConfig,
    account_store: AccountStore,
    challenges: Http01Challenges,
    dns: Option<Arc<dyn Dns01Provider>>,
}

impl AcmeManager {
    pub fn new(
        config: AcmeConfig,
        state_database: &Path,
        challenges: Http01Challenges,
        dns: Option<Arc<dyn Dns01Provider>>,
    ) -> Result<Self, AcmeError> {
        if config.email.trim().is_empty()
            || !config.email.contains('@')
            || !config.directory_url.starts_with("https://")
        {
            return Err(AcmeError::Invalid(
                "ACME email or directory URL is invalid".into(),
            ));
        }
        if config.dns_provider.is_some() && dns.is_none() {
            return Err(AcmeError::Invalid(
                "configured DNS-01 provider is unavailable".into(),
            ));
        }
        Ok(Self {
            config,
            account_store: AccountStore::for_database(state_database),
            challenges,
            dns,
        })
    }

    pub fn issue(&self, domains: &[String]) -> Result<CertificateInput, AcmeError> {
        let domains = normalized_domains(domains)?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .build()?;
        runtime.block_on(self.issue_async(domains))
    }

    async fn issue_async(&self, domains: Vec<String>) -> Result<CertificateInput, AcmeError> {
        let account = self.account().await?;
        let identifiers = domains
            .iter()
            .cloned()
            .map(Identifier::Dns)
            .collect::<Vec<_>>();
        let mut order = account.new_order(&NewOrder::new(&identifiers)).await?;
        let mut active = Vec::new();
        let setup = async {
            let mut authorizations = order.authorizations();
            while let Some(result) = authorizations.next().await {
                let mut authorization = result?;
                let domain = authorization.identifier().to_string();
                let base_domain = domain.strip_prefix("*.").unwrap_or(&domain).to_owned();
                let challenge_type =
                    if domain.starts_with("*.") || self.config.dns_provider.is_some() {
                        ChallengeType::Dns01
                    } else {
                        ChallengeType::Http01
                    };
                let mut challenge =
                    authorization
                        .challenge(challenge_type.clone())
                        .ok_or_else(|| {
                            AcmeError::Invalid(format!(
                                "ACME server did not offer {challenge_type:?} for {domain}"
                            ))
                        })?;
                let key_authorization = challenge.key_authorization();
                match challenge_type {
                    ChallengeType::Http01 => {
                        let token = challenge.token.clone();
                        self.challenges
                            .insert(&base_domain, &token, key_authorization.as_str())?;
                        active.push(ActiveChallenge::Http {
                            host: base_domain,
                            token,
                        });
                    }
                    ChallengeType::Dns01 => {
                        let provider = self.dns.as_ref().ok_or_else(|| {
                            AcmeError::Invalid(format!(
                                "wildcard domain {domain:?} requires a DNS-01 provider"
                            ))
                        })?;
                        let record = format!("_acme-challenge.{base_domain}");
                        let value = key_authorization.dns_value();
                        provider.present(&record, &value)?;
                        active.push(ActiveChallenge::Dns { record, value });
                    }
                    _ => return Err(AcmeError::Invalid("unsupported ACME challenge type".into())),
                }
                challenge.set_ready().await?;
            }
            order.poll_ready(&RetryPolicy::default()).await?;
            let private_key_pem = order.finalize().await?;
            let certificate_pem = order.poll_certificate(&RetryPolicy::default()).await?;
            Ok::<_, AcmeError>((private_key_pem, certificate_pem))
        }
        .await;
        self.cleanup(&active);
        let (private_key_pem, certificate_pem) = setup?;
        let not_after_unix = certificate_not_after(&certificate_pem)?;
        let mut hasher = Sha256::new();
        for domain in &domains {
            hasher.update(domain.as_bytes());
        }
        let id = format!(
            "acme-{}",
            BASE64_URL_SAFE_NO_PAD.encode(&hasher.finalize()[..8])
        );
        Ok(CertificateInput {
            id,
            domains,
            certificate_pem: certificate_pem.into_bytes(),
            private_key_pem: private_key_pem.into_bytes(),
            not_after_unix,
        })
    }

    async fn account(&self) -> Result<Account, AcmeError> {
        let builder = match std::env::var_os("CYGNUS_ACME_ROOT_CA") {
            Some(path) => Account::builder_with_root(PathBuf::from(path))?,
            None => Account::builder()?,
        };
        if let Some(credentials) = self.account_store.load()? {
            return Ok(builder.from_credentials(credentials).await?);
        }
        let contact = format!("mailto:{}", self.config.email);
        let (account, credentials) = builder
            .create(
                &NewAccount {
                    contact: &[&contact],
                    terms_of_service_agreed: true,
                    only_return_existing: false,
                },
                self.config.directory_url.clone(),
                None,
            )
            .await?;
        self.account_store.save(&credentials)?;
        Ok(account)
    }

    fn cleanup(&self, active: &[ActiveChallenge]) {
        for challenge in active {
            match challenge {
                ActiveChallenge::Http { host, token } => self.challenges.remove(host, token),
                ActiveChallenge::Dns { record, value } => {
                    if let Some(provider) = &self.dns {
                        let _ = provider.cleanup(record, value);
                    }
                }
            }
        }
    }
}

enum ActiveChallenge {
    Http { host: String, token: String },
    Dns { record: String, value: String },
}

fn normalized_domains(domains: &[String]) -> Result<Vec<String>, AcmeError> {
    let mut normalized = domains
        .iter()
        .map(|domain| {
            let wildcard = domain.starts_with("*.");
            let base = normalize_host(domain.trim_start_matches("*."));
            if wildcard { format!("*.{base}") } else { base }
        })
        .collect::<Vec<_>>();
    normalized.sort();
    normalized.dedup();
    if normalized.is_empty()
        || normalized.iter().any(|domain| {
            let base = domain.trim_start_matches("*.");
            base.is_empty()
                || !base.contains('.')
                || base
                    .bytes()
                    .any(|byte| !byte.is_ascii_alphanumeric() && !matches!(byte, b'-' | b'.'))
        })
    {
        return Err(AcmeError::Invalid(
            "ACME certificate domains are invalid".into(),
        ));
    }
    Ok(normalized)
}

fn certificate_not_after(certificate_pem: &str) -> Result<i64, AcmeError> {
    let mut reader = io::BufReader::new(certificate_pem.as_bytes());
    let certificate = rustls_pemfile::certs(&mut reader)
        .next()
        .transpose()?
        .ok_or_else(|| AcmeError::Invalid("issued certificate chain is empty".into()))?;
    let (_, certificate) = parse_x509_certificate(certificate.as_ref())
        .map_err(|error| AcmeError::Invalid(format!("parse issued certificate: {error}")))?;
    Ok(certificate.validity().not_after.timestamp())
}

#[derive(Debug, Error)]
pub enum AcmeError {
    #[error("invalid ACME configuration: {0}")]
    Invalid(String),
    #[error("ACME protocol failure: {0}")]
    Protocol(#[from] instant_acme::Error),
    #[error("DNS-01 provider failure: {0}")]
    Dns(String),
    #[error("ACME account I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("ACME account encoding failed: {0}")]
    Json(#[from] serde_json::Error),
}

struct AccountStore {
    directory: PathBuf,
    path: PathBuf,
}

impl AccountStore {
    fn for_database(database: &Path) -> Self {
        let parent = database.parent().unwrap_or_else(|| Path::new("."));
        let directory = parent.join("acme");
        Self {
            path: directory.join("account.json"),
            directory,
        }
    }

    fn load(&self) -> Result<Option<AccountCredentials>, AcmeError> {
        let mut file = match File::open(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        reject_permissive_file(&file, &self.path)?;
        let mut encoded = Vec::new();
        file.read_to_end(&mut encoded)?;
        Ok(Some(serde_json::from_slice(&encoded)?))
    }

    fn save(&self, credentials: &AccountCredentials) -> Result<(), AcmeError> {
        prepare_private_directory(&self.directory)?;
        let encoded = serde_json::to_vec(credentials)?;
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| AcmeError::Invalid("system clock predates the Unix epoch".into()))?
            .as_nanos();
        let temporary = self
            .directory
            .join(format!(".account-{}-{nonce}.tmp", std::process::id()));
        let result = (|| {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temporary)?;
            file.write_all(&encoded)?;
            file.sync_all()?;
            fs::rename(&temporary, &self.path)?;
            File::open(&self.directory)?.sync_all()?;
            Ok::<_, io::Error>(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result?;
        Ok(())
    }
}

fn prepare_private_directory(path: &Path) -> io::Result<()> {
    fs::create_dir_all(path)?;
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || metadata.uid() != unsafe { libc::geteuid() }
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("unsafe ACME account directory {}", path.display()),
        ));
    }
    if metadata.permissions().mode() & 0o077 != 0 {
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn reject_permissive_file(file: &File, path: &Path) -> io::Result<()> {
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.permissions().mode() & 0o077 != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("unsafe ACME account file {}", path.display()),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::thread;

    fn request(host: &str, target: &str) -> RequestHead {
        RequestHead {
            method: "GET".into(),
            target: target.into(),
            host: Some(host.into()),
            head_len: 0,
        }
    }

    #[test]
    fn http_challenges_are_scoped_by_host_and_removed() {
        let challenges = Http01Challenges::default();
        challenges
            .insert("Example.COM.", "abc_123", "abc_123.thumbprint")
            .unwrap();
        let target = "/.well-known/acme-challenge/abc_123";
        let response = challenges
            .response(&request("example.com:80", target))
            .unwrap();
        assert!(response.ends_with(b"abc_123.thumbprint"));
        assert!(
            challenges
                .response(&request("other.example.com", target))
                .is_none()
        );
        challenges.remove("example.com", "abc_123");
        assert!(
            challenges
                .response(&request("example.com", target))
                .is_none()
        );
    }

    #[test]
    fn challenge_tokens_reject_path_traversal() {
        let challenges = Http01Challenges::default();
        assert!(
            challenges
                .insert("example.com", "../secret", "value")
                .is_err()
        );
    }

    #[test]
    fn domain_set_is_normalized_and_stable() {
        assert_eq!(
            normalized_domains(&["WWW.Example.com.".into(), "*.Example.com".into()]).unwrap(),
            vec!["*.example.com", "www.example.com"]
        );
        assert!(normalized_domains(&["localhost".into()]).is_err());
    }
    #[test]
    fn cloudflare_provider_creates_and_removes_exact_challenge_record() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}/client/v4", listener.local_addr().unwrap());
        let server = thread::spawn(move || {
            for index in 0..7 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = Vec::new();
                let mut chunk = [0_u8; 2048];
                loop {
                    let read = stream.read(&mut chunk).unwrap();
                    assert_ne!(read, 0);
                    request.extend_from_slice(&chunk[..read]);
                    let Some(head_end) = request
                        .windows(4)
                        .position(|window| window == b"\r\n\r\n")
                        .map(|offset| offset + 4)
                    else {
                        continue;
                    };
                    let head = String::from_utf8_lossy(&request[..head_end]);
                    let content_length = head
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length:")?
                                .trim()
                                .parse::<usize>()
                                .ok()
                        })
                        .unwrap_or(0);
                    let chunked_post_complete = index == 2
                        && request
                            .windows(b"dns-value".len())
                            .any(|window| window == b"dns-value")
                        && request
                            .windows(b"\"ttl\"".len())
                            .any(|window| window == b"\"ttl\"");
                    if (content_length > 0 && request.len() >= head_end + content_length)
                        || (content_length == 0 && (index != 2 || chunked_post_complete))
                    {
                        break;
                    }
                }
                let request_text = String::from_utf8_lossy(&request);
                if index != 3 {
                    assert!(
                        request_text
                            .to_ascii_lowercase()
                            .contains("authorization: bearer test-token")
                    );
                }
                let (method, expected_path, body) = match index {
                    0 | 4 => (
                        "GET",
                        "/client/v4/zones?name=api.example.com&status=active",
                        r#"{"success":true,"result":[{"id":"zone-1"}],"errors":[]}"#,
                    ),
                    1 | 5 => (
                        "GET",
                        "/client/v4/zones/zone-1/dns_records?type=TXT&name=_acme-challenge.api.example.com",
                        if index == 1 {
                            r#"{"success":true,"result":[],"errors":[]}"#
                        } else {
                            r#"{"success":true,"result":[{"id":"record-1","content":"dns-value"}],"errors":[]}"#
                        },
                    ),
                    2 => (
                        "POST",
                        "/client/v4/zones/zone-1/dns_records",
                        r#"{"success":true,"result":{"id":"record-1","content":"dns-value"},"errors":[]}"#,
                    ),
                    3 => (
                        "GET",
                        "/client/v4/dns-query?name=_acme-challenge.api.example.com&type=TXT",
                        r#"{"Answer":[{"data":"\"dns-value\""}]}"#,
                    ),
                    6 => (
                        "DELETE",
                        "/client/v4/zones/zone-1/dns_records/record-1",
                        r#"{"success":true,"result":{"id":"record-1"},"errors":[]}"#,
                    ),
                    _ => unreachable!(),
                };
                assert!(request_text.starts_with(&format!("{method} {expected_path} HTTP/1.1")));
                if index == 2 {
                    assert!(request_text.contains("\"content\": \"dns-value\""));
                    assert!(request_text.contains("\"ttl\": 60"));
                }
                write!(stream, "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}", body.len(), body).unwrap();
            }
        });
        let provider = CloudflareDnsProvider::with_endpoint("test-token", endpoint);
        provider
            .present("_acme-challenge.api.example.com", "dns-value")
            .unwrap();
        provider
            .cleanup("_acme-challenge.api.example.com", "dns-value")
            .unwrap();
        server.join().unwrap();
    }
    #[test]
    fn pebble_issues_and_persists_an_http01_certificate_when_enabled() {
        let Ok(directory_url) = std::env::var("CYGNUS_ACME_TEST_DIRECTORY") else {
            return;
        };
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory =
            std::env::temp_dir().join(format!("cygnus-acme-pebble-{}-{nonce}", std::process::id()));
        fs::create_dir(&directory).unwrap();
        let challenges = Http01Challenges::default();
        let manager = AcmeManager::new(
            AcmeConfig {
                email: "operator@example.com".into(),
                directory_url,
                dns_provider: None,
            },
            &directory.join("state.db"),
            challenges.clone(),
            None,
        )
        .unwrap();
        let certificate = manager.issue(&["acme-test.example.com".into()]).unwrap();
        assert_eq!(certificate.domains, vec!["acme-test.example.com"]);
        assert!(
            certificate_not_after(std::str::from_utf8(&certificate.certificate_pem).unwrap())
                .unwrap()
                > 0
        );
        let account = directory.join("acme/account.json");
        assert_eq!(
            fs::metadata(account).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let renewed = manager.issue(&["acme-renew.example.com".into()]).unwrap();
        assert_eq!(renewed.domains, vec!["acme-renew.example.com"]);
        assert!(
            challenges
                .response(&request(
                    "acme-test.example.com",
                    "/.well-known/acme-challenge/missing"
                ))
                .is_none()
        );
        fs::remove_dir_all(directory).unwrap();
    }
}
