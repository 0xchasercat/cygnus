use std::env;
use std::error::Error;
use std::fs;
use std::io::{self, ErrorKind, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use cygnus_proxy::{Config, Proxy};

const MODE_ECHO: u8 = 1;
const MODE_BULK: u8 = 2;
const DEFAULT_ROUND_TRIPS: usize = 20_000;
const DEFAULT_CONNECTIONS: usize = 8;
const DEFAULT_BULK_BYTES: u64 = 1 << 30;
const PAYLOAD_BYTES: usize = 64;
const WARMUP_ROUND_TRIPS: usize = 500;
const GATE_ADDED_P50_MS: f64 = 0.5;
const GATE_THROUGHPUT_GBPS: f64 = 5.0;

fn main() -> Result<(), Box<dyn Error>> {
    let options = Options::parse()?;
    let socket_path = unique_socket_path("cygnus-g2");
    let upstream = UpstreamServer::start(&socket_path)?;
    let proxy = match Proxy::bind(Config::new("127.0.0.1:0".parse()?, socket_path.clone())) {
        Ok(proxy) => proxy,
        Err(error) if error.is_io_uring_unavailable() => {
            println!("G2 skipped: {error}");
            upstream.shutdown()?;
            return Ok(());
        }
        Err(error) => return Err(error.into()),
    };

    let proxy_addr = proxy.local_addr();
    let proxy_handle = proxy.handle();
    let proxy_thread = thread::Builder::new()
        .name("cygnus-g2-proxy".into())
        .spawn(move || proxy.run())?;

    let direct_latency = measure_latency(
        || BenchStream::direct(&socket_path, MODE_ECHO),
        options.round_trips,
        options.connections,
    )?;
    let proxy_latency = measure_latency(
        || BenchStream::proxied(proxy_addr, MODE_ECHO),
        options.round_trips,
        options.connections,
    )?;
    let direct_throughput = measure_throughput(
        BenchStream::direct(&socket_path, MODE_BULK)?,
        options.bulk_bytes,
    )?;
    let proxy_throughput = measure_throughput(
        BenchStream::proxied(proxy_addr, MODE_BULK)?,
        options.bulk_bytes,
    )?;

    proxy_handle.shutdown()?;
    proxy_thread
        .join()
        .map_err(|_| io::Error::other("proxy thread panicked"))??;
    upstream.shutdown()?;

    print_report(
        &options,
        direct_latency,
        proxy_latency,
        direct_throughput,
        proxy_throughput,
    );
    Ok(())
}

#[derive(Clone, Copy, Debug)]
struct Options {
    round_trips: usize,
    connections: usize,
    bulk_bytes: u64,
}

impl Options {
    fn parse() -> io::Result<Self> {
        let mut options = Self {
            round_trips: DEFAULT_ROUND_TRIPS,
            connections: DEFAULT_CONNECTIONS,
            bulk_bytes: DEFAULT_BULK_BYTES,
        };
        let mut arguments = env::args().skip(1);

        while let Some(argument) = arguments.next() {
            match argument.as_str() {
                "--round-trips" => {
                    options.round_trips = parse_usize(&argument, arguments.next())?;
                }
                "--connections" => {
                    options.connections = parse_usize(&argument, arguments.next())?;
                }
                "--bulk-bytes" => {
                    options.bulk_bytes = parse_u64(&argument, arguments.next())?;
                }
                "--help" | "-h" => {
                    print_usage();
                    process::exit(0);
                }
                _ => {
                    return Err(io::Error::new(
                        ErrorKind::InvalidInput,
                        format!("unknown argument: {argument}"),
                    ));
                }
            }
        }

        if options.round_trips == 0 || options.connections == 0 || options.bulk_bytes == 0 {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                "round trips, connections, and bulk bytes must be non-zero",
            ));
        }
        Ok(options)
    }
}

fn print_usage() {
    println!("Usage: g2 [--round-trips N] [--connections M] [--bulk-bytes BYTES]");
    println!();
    println!("Defaults:");
    println!("  --round-trips {DEFAULT_ROUND_TRIPS}");
    println!("  --connections {DEFAULT_CONNECTIONS}");
    println!("  --bulk-bytes {DEFAULT_BULK_BYTES}");
}

fn parse_usize(name: &str, value: Option<String>) -> io::Result<usize> {
    let value = required_value(name, value)?;
    value.parse().map_err(|error| {
        io::Error::new(
            ErrorKind::InvalidInput,
            format!("invalid value for {name}: {error}"),
        )
    })
}

fn parse_u64(name: &str, value: Option<String>) -> io::Result<u64> {
    let value = required_value(name, value)?;
    value.parse().map_err(|error| {
        io::Error::new(
            ErrorKind::InvalidInput,
            format!("invalid value for {name}: {error}"),
        )
    })
}

fn required_value(name: &str, value: Option<String>) -> io::Result<String> {
    value
        .ok_or_else(|| io::Error::new(ErrorKind::InvalidInput, format!("missing value for {name}")))
}

struct Latency {
    p50: Duration,
    p99: Duration,
}

fn measure_latency<F>(
    mut connect: F,
    round_trips: usize,
    connection_count: usize,
) -> io::Result<Latency>
where
    F: FnMut() -> io::Result<BenchStream>,
{
    let mut connections = (0..connection_count)
        .map(|_| connect())
        .collect::<io::Result<Vec<_>>>()?;
    let request = [0x5a; PAYLOAD_BYTES];
    let mut response = [0; PAYLOAD_BYTES];

    for index in 0..WARMUP_ROUND_TRIPS.min(round_trips) {
        round_trip(
            &mut connections[index % connection_count],
            &request,
            &mut response,
        )?;
    }

    let mut samples = Vec::with_capacity(round_trips);
    for index in 0..round_trips {
        let started = Instant::now();
        round_trip(
            &mut connections[index % connection_count],
            &request,
            &mut response,
        )?;
        samples.push(started.elapsed());
    }
    samples.sort_unstable();

    Ok(Latency {
        p50: percentile(&samples, 50),
        p99: percentile(&samples, 99),
    })
}

fn round_trip(stream: &mut BenchStream, request: &[u8], response: &mut [u8]) -> io::Result<()> {
    stream.write_all(&(request.len() as u32).to_be_bytes())?;
    stream.write_all(request)?;
    stream.flush()?;
    stream.read_exact(response)?;
    if response != request {
        return Err(io::Error::new(
            ErrorKind::InvalidData,
            "echo response did not match request",
        ));
    }
    Ok(())
}

fn percentile(samples: &[Duration], percentile: usize) -> Duration {
    let rank = (samples.len() * percentile).div_ceil(100);
    samples[rank.saturating_sub(1).min(samples.len() - 1)]
}

struct Throughput {
    bytes: u64,
    elapsed: Duration,
}

impl Throughput {
    fn megabytes_per_second(&self) -> f64 {
        self.bytes as f64 / 1_000_000.0 / self.elapsed.as_secs_f64()
    }

    fn gigabits_per_second(&self) -> f64 {
        self.bytes as f64 * 8.0 / 1_000_000_000.0 / self.elapsed.as_secs_f64()
    }
}

fn measure_throughput(mut stream: BenchStream, bytes: u64) -> io::Result<Throughput> {
    stream.write_all(&bytes.to_be_bytes())?;
    stream.flush()?;

    let started = Instant::now();
    let copied = io::copy(&mut stream, &mut io::sink())?;
    let elapsed = started.elapsed();
    if copied != bytes {
        return Err(io::Error::new(
            ErrorKind::UnexpectedEof,
            format!("bulk stream ended after {copied} of {bytes} bytes"),
        ));
    }

    Ok(Throughput {
        bytes: copied,
        elapsed,
    })
}

fn print_report(
    options: &Options,
    direct_latency: Latency,
    proxy_latency: Latency,
    direct_throughput: Throughput,
    proxy_throughput: Throughput,
) {
    let direct_p50_ms = milliseconds(direct_latency.p50);
    let direct_p99_ms = milliseconds(direct_latency.p99);
    let proxy_p50_ms = milliseconds(proxy_latency.p50);
    let proxy_p99_ms = milliseconds(proxy_latency.p99);
    let added_p50_ms = proxy_p50_ms - direct_p50_ms;
    let added_p99_ms = proxy_p99_ms - direct_p99_ms;
    let latency_gate = added_p50_ms <= GATE_ADDED_P50_MS;
    let throughput_gate = proxy_throughput.gigabits_per_second() >= GATE_THROUGHPUT_GBPS;

    println!("Cygnus G2 proxy overhead");
    println!("========================");
    println!("backend: {}", cygnus_proxy::BACKEND);
    println!(
        "workload: {} round trips over {} connections; {} bulk bytes",
        options.round_trips, options.connections, options.bulk_bytes
    );
    println!();
    println!("latency (small framed echo)");
    println!("  direct UDS p50: {direct_p50_ms:.3} ms");
    println!("  direct UDS p99: {direct_p99_ms:.3} ms");
    println!("  proxy TCP p50:  {proxy_p50_ms:.3} ms");
    println!("  proxy TCP p99:  {proxy_p99_ms:.3} ms");
    println!("  added p50:      {added_p50_ms:+.3} ms");
    println!("  added p99:      {added_p99_ms:+.3} ms");
    println!();
    println!("throughput (zero stream)");
    println!(
        "  direct UDS: {:.1} MB/s ({:.2} Gbps)",
        direct_throughput.megabytes_per_second(),
        direct_throughput.gigabits_per_second()
    );
    println!(
        "  proxy TCP:  {:.1} MB/s ({:.2} Gbps)",
        proxy_throughput.megabytes_per_second(),
        proxy_throughput.gigabits_per_second()
    );
    println!();
    println!("gates");
    println!(
        "  added p50 <= {GATE_ADDED_P50_MS:.1} ms: {}",
        pass_fail(latency_gate)
    );
    println!(
        "  proxy throughput >= {GATE_THROUGHPUT_GBPS:.1} Gbps: {}",
        pass_fail(throughput_gate)
    );
    println!("  CPU overhead < 10%: NOT MEASURED (optional in this slice)");
}

fn milliseconds(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1_000.0
}

fn pass_fail(passed: bool) -> &'static str {
    if passed { "PASS" } else { "FAIL" }
}

enum BenchStream {
    Direct(UnixStream),
    Proxied(TcpStream),
}

impl BenchStream {
    fn direct(path: &Path, mode: u8) -> io::Result<Self> {
        let mut stream = UnixStream::connect(path)?;
        stream.write_all(&[mode])?;
        Ok(Self::Direct(stream))
    }

    fn proxied(address: impl ToSocketAddrs, mode: u8) -> io::Result<Self> {
        let mut stream = TcpStream::connect(address)?;
        stream.set_nodelay(true)?;
        stream.write_all(&[mode])?;
        Ok(Self::Proxied(stream))
    }
}

impl Read for BenchStream {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Direct(stream) => stream.read(buffer),
            Self::Proxied(stream) => stream.read(buffer),
        }
    }
}

impl Write for BenchStream {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        match self {
            Self::Direct(stream) => stream.write(buffer),
            Self::Proxied(stream) => stream.write(buffer),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Direct(stream) => stream.flush(),
            Self::Proxied(stream) => stream.flush(),
        }
    }
}

struct UpstreamServer {
    path: PathBuf,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<io::Result<()>>>,
}

impl UpstreamServer {
    fn start(path: &Path) -> io::Result<Self> {
        remove_socket(path)?;
        let listener = UnixListener::bind(path)?;
        listener.set_nonblocking(true)?;
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let thread = thread::Builder::new()
            .name("cygnus-g2-upstream".into())
            .spawn(move || serve_upstream(listener, thread_stop))?;
        Ok(Self {
            path: path.to_path_buf(),
            stop,
            thread: Some(thread),
        })
    }

    fn shutdown(mut self) -> io::Result<()> {
        self.stop.store(true, Ordering::Release);
        let result = self.join();
        let cleanup = remove_socket(&self.path);
        result.and(cleanup)
    }

    fn join(&mut self) -> io::Result<()> {
        match self.thread.take() {
            Some(thread) => thread
                .join()
                .map_err(|_| io::Error::other("upstream thread panicked"))?,
            None => Ok(()),
        }
    }
}

impl Drop for UpstreamServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = self.join();
        let _ = remove_socket(&self.path);
    }
}

fn serve_upstream(listener: UnixListener, stop: Arc<AtomicBool>) -> io::Result<()> {
    let mut handlers = Vec::new();
    let accept_error = loop {
        if stop.load(Ordering::Acquire) {
            break None;
        }

        match listener.accept() {
            Ok((stream, _)) => handlers.push(thread::spawn(move || handle_upstream(stream))),
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(1));
            }
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            Err(error) => break Some(error),
        }
    };

    for handler in handlers {
        handler
            .join()
            .map_err(|_| io::Error::other("upstream connection thread panicked"))??;
    }
    match accept_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

fn handle_upstream(mut stream: UnixStream) -> io::Result<()> {
    let mut mode = [0; 1];
    stream.read_exact(&mut mode)?;
    match mode[0] {
        MODE_ECHO => serve_echo(stream),
        MODE_BULK => serve_bulk(stream),
        mode => Err(io::Error::new(
            ErrorKind::InvalidData,
            format!("unknown upstream mode {mode}"),
        )),
    }
}

fn serve_echo(mut stream: UnixStream) -> io::Result<()> {
    loop {
        let mut length = [0; 4];
        match stream.read_exact(&mut length) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::UnexpectedEof => return Ok(()),
            Err(error) => return Err(error),
        }
        let length = u32::from_be_bytes(length) as usize;
        let mut payload = vec![0; length];
        stream.read_exact(&mut payload)?;
        stream.write_all(&payload)?;
    }
}

fn serve_bulk(mut stream: UnixStream) -> io::Result<()> {
    let mut length = [0; 8];
    stream.read_exact(&mut length)?;
    let mut remaining = u64::from_be_bytes(length);
    let chunk = [0; 64 * 1024];
    while remaining > 0 {
        let count = remaining.min(chunk.len() as u64) as usize;
        stream.write_all(&chunk[..count])?;
        remaining -= count as u64;
    }
    Ok(())
}

fn unique_socket_path(prefix: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    env::temp_dir().join(format!("{prefix}-{}-{nonce}.sock", process::id()))
}

fn remove_socket(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}
