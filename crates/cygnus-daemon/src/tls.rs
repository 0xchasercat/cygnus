use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, BufReader, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::sync::Arc;

use cygnus_router::normalize_host;
use rustls::crypto::ring::default_provider;
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use rustls::{ServerConfig, ServerConnection};
use thiserror::Error;

use crate::edge::CertificateRecord;

const RELAY_BUFFER_BYTES: usize = 16 * 1024;
const TLS_BUFFER_LIMIT: usize = 256 * 1024;

#[derive(Debug, Error)]
pub enum TlsError {
    #[error("TLS configuration is invalid: {0}")]
    Invalid(String),
    #[error("TLS certificate filesystem error: {0}")]
    Io(#[from] io::Error),
    #[error("rustls configuration error: {0}")]
    Rustls(#[from] rustls::Error),
}

#[derive(Clone, Debug)]
pub struct TlsServer {
    config: Arc<ServerConfig>,
}

impl TlsServer {
    pub fn from_certificates(certificates: &[CertificateRecord]) -> Result<Self, TlsError> {
        if certificates.is_empty() {
            return Err(TlsError::Invalid(
                "HTTPS requires at least one installed certificate".into(),
            ));
        }
        let provider = Arc::new(default_provider());
        let mut resolver = CertificateResolver::default();
        for certificate in certificates {
            let mut certificate_reader = BufReader::new(File::open(&certificate.certificate_path)?);
            let chain =
                rustls_pemfile::certs(&mut certificate_reader).collect::<Result<Vec<_>, _>>()?;
            if chain.is_empty() {
                return Err(TlsError::Invalid(format!(
                    "certificate {:?} has no X.509 certificate blocks",
                    certificate.id
                )));
            }
            let mut key_reader = BufReader::new(File::open(&certificate.private_key_path)?);
            let key = rustls_pemfile::private_key(&mut key_reader)?.ok_or_else(|| {
                TlsError::Invalid(format!(
                    "certificate {:?} has no supported private key",
                    certificate.id
                ))
            })?;
            let certified = Arc::new(CertifiedKey::from_der(chain, key, &provider)?);
            for domain in &certificate.domains {
                resolver.add(domain, Arc::clone(&certified))?;
            }
        }
        let mut config = ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()?
            .with_no_client_auth()
            .with_cert_resolver(Arc::new(resolver));
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        Ok(Self {
            config: Arc::new(config),
        })
    }

    pub(crate) fn connection(&self) -> Result<ServerConnection, TlsError> {
        let mut connection = ServerConnection::new(Arc::clone(&self.config))?;
        connection.set_buffer_limit(Some(TLS_BUFFER_LIMIT));
        Ok(connection)
    }
}

#[derive(Debug, Default)]
struct CertificateResolver {
    exact: BTreeMap<String, Arc<CertifiedKey>>,
    wildcard: BTreeMap<String, Arc<CertifiedKey>>,
}

impl CertificateResolver {
    fn add(&mut self, domain: &str, key: Arc<CertifiedKey>) -> Result<(), TlsError> {
        let domain = normalize_host(domain);
        let (map, name) = if let Some(suffix) = domain.strip_prefix("*.") {
            (&mut self.wildcard, suffix)
        } else {
            (&mut self.exact, domain.as_str())
        };
        if map.insert(name.into(), key).is_some() {
            return Err(TlsError::Invalid(format!(
                "duplicate TLS certificate domain {domain:?}"
            )));
        }
        Ok(())
    }

    fn find(&self, server_name: &str) -> Option<Arc<CertifiedKey>> {
        let server_name = normalize_host(server_name);
        if let Some(key) = self.exact.get(&server_name) {
            return Some(Arc::clone(key));
        }
        let (_, suffix) = server_name.split_once('.')?;
        self.wildcard.get(suffix).cloned()
    }
}

impl ResolvesServerCert for CertificateResolver {
    fn resolve(&self, client_hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        self.find(client_hello.server_name()?)
    }
}

pub(crate) fn relay_tls(
    mut connection: ServerConnection,
    mut client: TcpStream,
    mut upstream: UnixStream,
) -> io::Result<()> {
    client.set_nonblocking(true)?;
    upstream.set_nonblocking(true)?;

    let mut client_closed = false;
    let mut upstream_closed = false;
    let mut upstream_write_closed = false;
    let mut close_notify_sent = false;
    let mut to_upstream = Vec::new();
    let mut to_upstream_offset = 0;
    let mut to_client = Vec::new();
    let mut to_client_offset = 0;
    let mut buffer = [0_u8; RELAY_BUFFER_BYTES];

    loop {
        let mut progressed = false;

        if connection.wants_read() && !client_closed {
            match connection.read_tls(&mut client) {
                Ok(0) => {
                    client_closed = true;
                    progressed = true;
                }
                Ok(_) => {
                    let state = connection.process_new_packets().map_err(io::Error::other)?;
                    client_closed |= state.peer_has_closed();
                    progressed = true;
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        }

        if to_upstream_offset == to_upstream.len() {
            to_upstream.clear();
            to_upstream_offset = 0;
            match connection.reader().read(&mut buffer) {
                Ok(0) => {}
                Ok(read) => {
                    to_upstream.extend_from_slice(&buffer[..read]);
                    progressed = true;
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => return Err(error),
            }
        }

        if to_upstream_offset < to_upstream.len() {
            match upstream.write(&to_upstream[to_upstream_offset..]) {
                Ok(0) => upstream_closed = true,
                Ok(written) => {
                    to_upstream_offset += written;
                    progressed = true;
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset
                    ) =>
                {
                    upstream_closed = true;
                }
                Err(error) => return Err(error),
            }
        }

        if client_closed && to_upstream_offset == to_upstream.len() && !upstream_write_closed {
            let _ = upstream.shutdown(Shutdown::Write);
            upstream_write_closed = true;
            progressed = true;
        }

        if to_client_offset == to_client.len() && !upstream_closed {
            to_client.clear();
            to_client_offset = 0;
            match upstream.read(&mut buffer) {
                Ok(0) => {
                    upstream_closed = true;
                    progressed = true;
                }
                Ok(read) => {
                    to_client.extend_from_slice(&buffer[..read]);
                    progressed = true;
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::ConnectionReset | io::ErrorKind::BrokenPipe
                    ) =>
                {
                    upstream_closed = true;
                }
                Err(error) => return Err(error),
            }
        }

        if to_client_offset < to_client.len() {
            match connection.writer().write(&to_client[to_client_offset..]) {
                Ok(0) => {}
                Ok(written) => {
                    to_client_offset += written;
                    progressed = true;
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => return Err(error),
            }
        }

        if upstream_closed && to_client_offset == to_client.len() && !close_notify_sent {
            connection.send_close_notify();
            close_notify_sent = true;
            progressed = true;
        }

        while connection.wants_write() {
            match connection.write_tls(&mut client) {
                Ok(0) => break,
                Ok(_) => progressed = true,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset
                    ) =>
                {
                    client_closed = true;
                    break;
                }
                Err(error) => return Err(error),
            }
        }

        if close_notify_sent && !connection.wants_write() {
            let _ = client.shutdown(Shutdown::Write);
            return Ok(());
        }
        if client_closed && upstream_closed && !connection.wants_write() {
            return Ok(());
        }
        if !progressed {
            wait_for_io(
                &client,
                &upstream,
                connection.wants_read() && !client_closed,
                connection.wants_write(),
                !upstream_closed && to_client_offset == to_client.len(),
                to_upstream_offset < to_upstream.len(),
            )?;
        }
    }
}

fn wait_for_io(
    client: &TcpStream,
    upstream: &UnixStream,
    client_read: bool,
    client_write: bool,
    upstream_read: bool,
    upstream_write: bool,
) -> io::Result<()> {
    let mut descriptors = [
        libc::pollfd {
            fd: client.as_raw_fd(),
            events: (if client_read { libc::POLLIN } else { 0 })
                | (if client_write { libc::POLLOUT } else { 0 }),
            revents: 0,
        },
        libc::pollfd {
            fd: upstream.as_raw_fd(),
            events: (if upstream_read { libc::POLLIN } else { 0 })
                | (if upstream_write { libc::POLLOUT } else { 0 }),
            revents: 0,
        },
    ];
    loop {
        let result = unsafe { libc::poll(descriptors.as_mut_ptr(), descriptors.len() as _, 1_000) };
        if result >= 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::thread;

    use rcgen::{CertifiedKey as GeneratedKey, generate_simple_self_signed};
    use rustls::pki_types::ServerName;
    use rustls::{ClientConfig, ClientConnection, RootCertStore, StreamOwned};

    static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);

    fn certificate_fixture() -> (
        std::path::PathBuf,
        CertificateRecord,
        rustls::pki_types::CertificateDer<'static>,
    ) {
        let nonce = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let directory =
            std::env::temp_dir().join(format!("cygnus-tls-{}-{nonce}", std::process::id()));
        fs::create_dir(&directory).unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        let GeneratedKey { cert, signing_key } = generate_simple_self_signed([
            "api.example.com".to_owned(),
            "tenant.apps.example.com".to_owned(),
        ])
        .unwrap();
        let certificate_path = directory.join("fullchain.pem");
        let private_key_path = directory.join("key.pem");
        fs::write(&certificate_path, cert.pem()).unwrap();
        fs::write(&private_key_path, signing_key.serialize_pem()).unwrap();
        fs::set_permissions(&certificate_path, fs::Permissions::from_mode(0o600)).unwrap();
        fs::set_permissions(&private_key_path, fs::Permissions::from_mode(0o600)).unwrap();
        let record = CertificateRecord {
            id: "edge-test".into(),
            domains: vec!["api.example.com".into(), "*.apps.example.com".into()],
            generation: "a".repeat(64),
            certificate_path,
            private_key_path,
            not_after_unix: 4_102_444_800,
            installed_at: "test".into(),
        };
        (directory, record, cert.der().clone())
    }

    #[test]
    fn resolver_matches_exact_and_one_label_wildcards() {
        let (directory, record, _) = certificate_fixture();
        let tls = TlsServer::from_certificates(&[record]).unwrap();
        assert!(tls.connection().is_ok());

        let (_, record, _) = certificate_fixture();
        let provider = default_provider();
        let mut certificate_reader = BufReader::new(File::open(&record.certificate_path).unwrap());
        let chain = rustls_pemfile::certs(&mut certificate_reader)
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        let mut key_reader = BufReader::new(File::open(&record.private_key_path).unwrap());
        let key = rustls_pemfile::private_key(&mut key_reader)
            .unwrap()
            .unwrap();
        let key = Arc::new(CertifiedKey::from_der(chain, key, &provider).unwrap());
        let mut resolver = CertificateResolver::default();
        resolver.add("api.example.com", Arc::clone(&key)).unwrap();
        resolver.add("*.apps.example.com", key).unwrap();
        assert!(resolver.find("API.EXAMPLE.COM.").is_some());
        assert!(resolver.find("one.apps.example.com").is_some());
        assert!(resolver.find("two.one.apps.example.com").is_none());
        assert!(resolver.find("apps.example.com").is_none());

        fs::remove_dir_all(directory).unwrap();
        fs::remove_dir_all(record.certificate_path.parent().unwrap()).unwrap();
    }

    #[test]
    fn rustls_connection_relays_bidirectionally_to_uds() {
        let (directory, record, trust_anchor) = certificate_fixture();
        let tls = TlsServer::from_certificates(&[record]).unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (mut relay_upstream, mut application) = UnixStream::pair().unwrap();

        let server = thread::spawn(move || {
            let (client, _) = listener.accept().unwrap();
            let mut stream = StreamOwned::new(tls.connection().unwrap(), client);
            let mut request = [0_u8; 4];
            stream.read_exact(&mut request).unwrap();
            relay_upstream.write_all(&request).unwrap();
            let (connection, client) = stream.into_parts();
            relay_tls(connection, client, relay_upstream).unwrap();
        });
        let upstream = thread::spawn(move || {
            let mut request = [0_u8; 4];
            application.read_exact(&mut request).unwrap();
            assert_eq!(&request, b"ping");
            application.write_all(b"pong").unwrap();
            application.shutdown(Shutdown::Write).unwrap();
        });

        let mut roots = RootCertStore::empty();
        roots.add(trust_anchor).unwrap();
        let config = ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let name = ServerName::try_from("api.example.com".to_owned()).unwrap();
        let connection = ClientConnection::new(Arc::new(config), name).unwrap();
        let client = TcpStream::connect(address).unwrap();
        let mut client = StreamOwned::new(connection, client);
        client.write_all(b"ping").unwrap();
        client.flush().unwrap();
        let mut response = [0_u8; 4];
        client.read_exact(&mut response).unwrap();
        assert_eq!(&response, b"pong");

        upstream.join().unwrap();
        server.join().unwrap();
        fs::remove_dir_all(directory).unwrap();
    }
}
