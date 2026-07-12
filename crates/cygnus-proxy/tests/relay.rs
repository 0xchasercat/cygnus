use std::error::Error;
use std::fs;
use std::io::{self, ErrorKind, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process;
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use cygnus_proxy::{Config, Proxy, ProxyHandle};

const IO_TIMEOUT: Duration = Duration::from_secs(15);
const LARGE_TRANSFER_BYTES: u64 = 32 * 1024 * 1024;
const TRANSFER_CHUNK: usize = 64 * 1024;

#[test]
fn loopback_round_trip_preserves_bytes() -> Result<(), Box<dyn Error>> {
    let pattern = patterned_bytes(256 * 1024, 0);
    let expected = pattern.clone();
    let Some(harness) = Harness::start("loopback round trip", move |mut upstream| {
        configure_unix_stream(&upstream)?;
        let mut request = vec![0; expected.len()];
        upstream.read_exact(&mut request)?;
        if request != expected {
            return Err(io::Error::new(
                ErrorKind::InvalidData,
                "upstream received corrupted bytes",
            ));
        }
        upstream.write_all(&request)
    })?
    else {
        return Ok(());
    };

    let mut client = harness.connect()?;
    client.write_all(&pattern)?;
    let mut echoed = vec![0; pattern.len()];
    client.read_exact(&mut echoed)?;
    assert_eq!(echoed, pattern);
    client.shutdown(Shutdown::Write)?;
    drop(client);
    harness.finish()
}

#[test]
fn large_transfer_preserves_stream_hash() -> Result<(), Box<dyn Error>> {
    let Some(harness) = Harness::start("large transfer integrity", |mut upstream| {
        configure_unix_stream(&upstream)?;
        let mut buffer = vec![0; TRANSFER_CHUNK];
        loop {
            let count = match upstream.read(&mut buffer) {
                Ok(0) => return Ok(()),
                Ok(count) => count,
                Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            };
            upstream.write_all(&buffer[..count])?;
        }
    })?
    else {
        return Ok(());
    };

    let mut reader = harness.connect()?;
    let mut writer = reader.try_clone()?;
    let sender = thread::spawn(move || -> io::Result<(u64, Hash64)> {
        let mut offset = 0;
        let mut hash = Hash64::new();
        let mut buffer = vec![0; TRANSFER_CHUNK];
        while offset < LARGE_TRANSFER_BYTES {
            let count = (LARGE_TRANSFER_BYTES - offset).min(buffer.len() as u64) as usize;
            fill_pattern(&mut buffer[..count], offset);
            writer.write_all(&buffer[..count])?;
            hash.update(&buffer[..count]);
            offset += count as u64;
        }
        writer.shutdown(Shutdown::Write)?;
        Ok((offset, hash))
    });

    let mut received = 0;
    let mut received_hash = Hash64::new();
    let mut buffer = vec![0; TRANSFER_CHUNK];
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        received += count as u64;
        received_hash.update(&buffer[..count]);
    }

    let (sent, sent_hash) = sender
        .join()
        .map_err(|_| io::Error::other("large-transfer sender panicked"))??;
    assert_eq!(sent, LARGE_TRANSFER_BYTES);
    assert_eq!(received, LARGE_TRANSFER_BYTES);
    assert_eq!(received_hash, sent_hash);
    drop(reader);
    harness.finish()
}

#[test]
fn half_close_propagates_without_losing_response() -> Result<(), Box<dyn Error>> {
    let request = b"request body completed before response".to_vec();
    let expected_request = request.clone();
    let response = b"response emitted only after upstream observed EOF".to_vec();
    let expected_response = response.clone();
    let Some(harness) = Harness::start("half-close behavior", move |mut upstream| {
        configure_unix_stream(&upstream)?;
        let mut received = Vec::new();
        upstream.read_to_end(&mut received)?;
        if received != expected_request {
            return Err(io::Error::new(
                ErrorKind::InvalidData,
                "upstream received an incomplete half-closed request",
            ));
        }
        upstream.write_all(&response)
    })?
    else {
        return Ok(());
    };

    let mut client = harness.connect()?;
    client.write_all(&request)?;
    client.shutdown(Shutdown::Write)?;
    let mut received = Vec::new();
    client.read_to_end(&mut received)?;
    assert_eq!(received, expected_response);
    drop(client);
    harness.finish()
}

struct Harness {
    address: SocketAddr,
    path: PathBuf,
    proxy_handle: ProxyHandle,
    proxy_thread: Option<JoinHandle<cygnus_proxy::Result<()>>>,
    upstream_thread: Option<JoinHandle<io::Result<()>>>,
}

impl Harness {
    fn start<F>(name: &str, handler: F) -> Result<Option<Self>, Box<dyn Error>>
    where
        F: FnOnce(UnixStream) -> io::Result<()> + Send + 'static,
    {
        let path = unique_socket_path(name);
        remove_socket(&path)?;
        let upstream = UnixListener::bind(&path)?;
        let proxy = match Proxy::bind(Config::new("127.0.0.1:0".parse()?, path.clone())) {
            Ok(proxy) => proxy,
            Err(error) if error.is_io_uring_unavailable() => {
                eprintln!("skipping {name}: {error}");
                drop(upstream);
                remove_socket(&path)?;
                return Ok(None);
            }
            Err(error) => return Err(error.into()),
        };

        let address = proxy.local_addr();
        let proxy_handle = proxy.handle();
        let proxy_thread = thread::Builder::new()
            .name(format!("proxy-{name}"))
            .spawn(move || proxy.run())?;
        let upstream_thread = thread::Builder::new()
            .name(format!("upstream-{name}"))
            .spawn(move || {
                let (stream, _) = upstream.accept()?;
                handler(stream)
            })?;

        Ok(Some(Self {
            address,
            path,
            proxy_handle,
            proxy_thread: Some(proxy_thread),
            upstream_thread: Some(upstream_thread),
        }))
    }

    fn connect(&self) -> io::Result<TcpStream> {
        let stream = TcpStream::connect(self.address)?;
        stream.set_nodelay(true)?;
        stream.set_read_timeout(Some(IO_TIMEOUT))?;
        stream.set_write_timeout(Some(IO_TIMEOUT))?;
        Ok(stream)
    }

    fn finish(mut self) -> Result<(), Box<dyn Error>> {
        self.proxy_handle.shutdown()?;
        if let Some(thread) = self.proxy_thread.take() {
            thread
                .join()
                .map_err(|_| io::Error::other("proxy thread panicked"))??;
        }
        if let Some(thread) = self.upstream_thread.take() {
            thread
                .join()
                .map_err(|_| io::Error::other("upstream thread panicked"))??;
        }
        remove_socket(&self.path)?;
        Ok(())
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = self.proxy_handle.shutdown();
        let _ = UnixStream::connect(&self.path);
        if let Some(thread) = self.proxy_thread.take() {
            let _ = thread.join();
        }
        if let Some(thread) = self.upstream_thread.take() {
            let _ = thread.join();
        }
        let _ = remove_socket(&self.path);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Hash64(u64);

impl Hash64 {
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;

    fn new() -> Self {
        Self(Self::OFFSET_BASIS)
    }

    fn update(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.0 ^= u64::from(*byte);
            self.0 = self.0.wrapping_mul(Self::PRIME);
        }
    }
}

fn configure_unix_stream(stream: &UnixStream) -> io::Result<()> {
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))
}

fn patterned_bytes(length: usize, offset: u64) -> Vec<u8> {
    let mut bytes = vec![0; length];
    fill_pattern(&mut bytes, offset);
    bytes
}

fn fill_pattern(bytes: &mut [u8], offset: u64) {
    for (index, byte) in bytes.iter_mut().enumerate() {
        let position = offset + index as u64;
        *byte = position
            .wrapping_mul(31)
            .wrapping_add(position.rotate_right(11)) as u8;
    }
}

fn unique_socket_path(test_name: &str) -> PathBuf {
    let sanitized = test_name.replace(' ', "-");
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "cygnus-proxy-{sanitized}-{}-{nonce}.sock",
        process::id()
    ))
}

fn remove_socket(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}
