use std::env;
use std::ffi::OsString;
use std::process::ExitCode;
use std::thread;
use std::time::Duration;

#[cfg(target_os = "linux")]
use std::ffi::{c_int, c_long};
#[cfg(target_os = "linux")]
use std::{fs, io};

use cygnus_cage::{Cage, CageError, CageSpec};

const DEFAULT_COUNT: usize = 200;
const UNDER_LOAD_PROBES: usize = 20;
const MEMORY_GATE_PERCENT: f64 = 60.0;
// Used only when sysconf cannot report the runtime page size.
#[cfg(target_os = "linux")]
const FALLBACK_PAGE_SIZE: u64 = 4096;
#[cfg(target_os = "linux")]
const LINUX_SC_PAGESIZE: c_int = 30;

#[cfg(target_os = "linux")]
unsafe extern "C" {
    fn sysconf(name: c_int) -> c_long;
}

fn main() -> ExitCode {
    match run() {
        Ok(RunOutcome::Completed) => ExitCode::SUCCESS,
        Ok(RunOutcome::Skipped(reason)) => {
            println!("G3 density results");
            println!("isolation: {}", cygnus_cage::ISOLATION);
            println!("SKIP: {reason}");
            ExitCode::SUCCESS
        }
        Err(message) => {
            eprintln!("g3: {message}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<RunOutcome, String> {
    let options = Options::parse(env::args_os().skip(1))?;
    let memory_before = match sample_host_memory() {
        Ok(sample) => sample,
        Err(error) => {
            return Ok(RunOutcome::Skipped(format!(
                "host memory is unavailable: {error}"
            )));
        }
    };

    let mut cages = Vec::with_capacity(options.count);
    let mut unloaded_boots = Vec::with_capacity(options.count);

    for index in 0..options.count {
        let spec = cage_spec(&options, "baseline", index);
        match Cage::boot(spec) {
            Ok(cage) => {
                unloaded_boots.push(cage.timings().total);
                cages.push(cage);
            }
            Err(error) if index == 0 && environment_unavailable(&error) => {
                return Ok(RunOutcome::Skipped(format!(
                    "environment cannot create cages: {error}. Linux density runs require user namespaces and a writable delegated cgroup v2 subtree"
                )));
            }
            Err(error) => {
                let achieved = cages.len();
                let message = format!(
                    "baseline cage {} of {} failed to boot with {achieved} cages live: {error}",
                    index + 1,
                    options.count
                );
                return Err(error_after_cleanup(message, [("baseline", cages)]));
            }
        }
    }

    if !options.warmup.is_zero() {
        thread::sleep(options.warmup);
    }

    let memory_after = match sample_host_memory() {
        Ok(sample) => sample,
        Err(error) => {
            let message = format!("failed to sample host memory after warmup: {error}");
            return Err(error_after_cleanup(message, [("baseline", cages)]));
        }
    };
    let rss_samples = match sample_cage_rss(&cages) {
        Ok(samples) => samples,
        Err(error) => {
            let message = format!("failed to sample per-cage RSS after warmup: {error}");
            return Err(error_after_cleanup(message, [("baseline", cages)]));
        }
    };

    let mut under_load_boots = Vec::with_capacity(UNDER_LOAD_PROBES);
    for index in 0..UNDER_LOAD_PROBES {
        let spec = cage_spec(&options, "under-load", index);
        let cage = match Cage::boot(spec) {
            Ok(cage) => cage,
            Err(error) => {
                let completed = under_load_boots.len();
                let message = format!(
                    "under-load probe {} of {UNDER_LOAD_PROBES} failed with {} baseline cages live after {completed} completed probes: {error}",
                    index + 1,
                    cages.len()
                );
                return Err(error_after_cleanup(message, [("baseline", cages)]));
            }
        };
        let elapsed = cage.timings().total;
        if let Err(error) = cage.teardown() {
            let message = format!(
                "under-load probe {} of {UNDER_LOAD_PROBES} failed to tear down with {} baseline cages live: {error}",
                index + 1,
                cages.len()
            );
            return Err(error_after_cleanup(message, [("baseline", cages)]));
        }
        under_load_boots.push(elapsed);
    }

    let achieved = cages.len();
    let baseline_cleanup = teardown_cages("baseline", cages);

    print_report(Report {
        target_count: options.count,
        achieved,
        warmup: options.warmup,
        memory_before,
        memory_after,
        rss_samples: rss_samples.as_deref(),
        unloaded_boots: &unloaded_boots,
        under_load_boots: &under_load_boots,
    });

    let mut cleanup_errors = Vec::new();
    if let Err(error) = baseline_cleanup {
        cleanup_errors.push(error);
    }
    if cleanup_errors.is_empty() {
        Ok(RunOutcome::Completed)
    } else {
        Err(cleanup_errors.join("; "))
    }
}

enum RunOutcome {
    Completed,
    Skipped(String),
}

#[derive(Debug, Eq, PartialEq)]
struct Options {
    count: usize,
    warmup: Duration,
    command: OsString,
    args: Vec<OsString>,
}

impl Options {
    fn parse(arguments: impl IntoIterator<Item = OsString>) -> Result<Self, String> {
        let mut count = DEFAULT_COUNT;
        let mut warmup = Duration::ZERO;
        let mut arguments = arguments.into_iter();

        loop {
            let argument = arguments.next().ok_or_else(usage)?;
            if argument == "--" {
                break;
            }
            if argument == "--help" || argument == "-h" {
                return Err(usage());
            }
            if argument == "--count" {
                let value = arguments
                    .next()
                    .ok_or_else(|| "--count requires a positive integer".to_owned())?;
                count = parse_positive_usize("--count", &value)?;
                continue;
            }
            if argument == "--warmup-ms" {
                let value = arguments
                    .next()
                    .ok_or_else(|| "--warmup-ms requires a non-negative integer".to_owned())?;
                let text = value
                    .to_str()
                    .ok_or_else(|| "--warmup-ms must be valid UTF-8".to_owned())?;
                let millis = text
                    .parse()
                    .map_err(|_| "--warmup-ms requires a non-negative integer".to_owned())?;
                warmup = Duration::from_millis(millis);
                continue;
            }
            return Err(format!("unknown option {:?}\n{}", argument, usage()));
        }

        let command = arguments
            .next()
            .ok_or_else(|| format!("missing command\n{}", usage()))?;
        let args = arguments.collect();
        Ok(Self {
            count,
            warmup,
            command,
            args,
        })
    }
}

fn parse_positive_usize(option: &str, value: &OsString) -> Result<usize, String> {
    let text = value
        .to_str()
        .ok_or_else(|| format!("{option} must be valid UTF-8"))?;
    let parsed = text
        .parse()
        .map_err(|_| format!("{option} requires a positive integer"))?;
    if parsed == 0 {
        return Err(format!("{option} must be greater than zero"));
    }
    Ok(parsed)
}

fn usage() -> String {
    "usage: g3 [--count N] [--warmup-ms MS] -- <cmd> [args...]".into()
}

fn cage_spec(options: &Options, phase: &str, index: usize) -> CageSpec {
    let mut spec = CageSpec::new(
        format!("g3-{}-{phase}-{index}", std::process::id()),
        options.command.clone(),
    );
    spec.args = options.args.clone();
    spec.env = env::vars_os().collect();
    spec
}

fn environment_unavailable(error: &CageError) -> bool {
    matches!(
        error,
        CageError::NamespaceUnavailable { .. }
            | CageError::CgroupUnavailable(_)
            | CageError::CgroupControllerUnavailable(_)
            | CageError::Io { .. }
    )
}

fn error_after_cleanup<const N: usize>(
    message: String,
    groups: [(&str, Vec<Cage>); N],
) -> String {
    let mut errors = vec![message];
    for (label, cages) in groups {
        if let Err(error) = teardown_cages(label, cages) {
            errors.push(error);
        }
    }
    errors.join("; ")
}

fn teardown_cages(label: &str, cages: Vec<Cage>) -> Result<(), String> {
    let mut errors = Vec::new();
    for (index, cage) in cages.into_iter().enumerate() {
        if let Err(error) = cage.teardown() {
            errors.push(format!("{}: {error}", index + 1));
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "failed to tear down {label} cages ({})",
            errors.join(", ")
        ))
    }
}

#[derive(Clone, Copy)]
struct HostMemory {
    total_bytes: u64,
    available_bytes: u64,
}

fn sample_host_memory() -> Result<Option<HostMemory>, String> {
    #[cfg(target_os = "linux")]
    {
        let contents = fs::read_to_string("/proc/meminfo")
            .map_err(|error| format!("failed to read /proc/meminfo: {error}"))?;
        let total_bytes = parse_meminfo_bytes(&contents, "MemTotal:")?;
        let available_bytes = parse_meminfo_bytes(&contents, "MemAvailable:")?;
        Ok(Some(HostMemory {
            total_bytes,
            available_bytes,
        }))
    }

    #[cfg(not(target_os = "linux"))]
    {
        Ok(None)
    }
}

#[cfg(target_os = "linux")]
fn parse_meminfo_bytes(contents: &str, key: &str) -> Result<u64, String> {
    let line = contents
        .lines()
        .find(|line| line.starts_with(key))
        .ok_or_else(|| format!("/proc/meminfo does not contain {key}"))?;
    let mut fields = line.split_whitespace();
    let _ = fields.next();
    let kibibytes = fields
        .next()
        .ok_or_else(|| format!("{key} has no value in /proc/meminfo"))?
        .parse::<u64>()
        .map_err(|_| format!("{key} has an invalid value in /proc/meminfo"))?;
    match fields.next() {
        Some("kB") => {}
        Some(unit) => return Err(format!("{key} uses unexpected unit {unit} in /proc/meminfo")),
        None => return Err(format!("{key} has no unit in /proc/meminfo")),
    }
    kibibytes
        .checked_mul(1024)
        .ok_or_else(|| format!("{key} overflows bytes"))
}

fn sample_cage_rss(cages: &[Cage]) -> Result<Option<Vec<u64>>, String> {
    #[cfg(target_os = "linux")]
    {
        let page_size = linux_page_size();
        let mut samples = Vec::with_capacity(cages.len());
        for (index, cage) in cages.iter().enumerate() {
            let pid = cage.host_pid().ok_or_else(|| {
                format!("cage {} has no host PID on the Linux backend", index + 1)
            })?;
            let path = format!("/proc/{pid}/statm");
            let contents = match fs::read_to_string(&path) {
                Ok(contents) => contents,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    return Err(format!(
                        "cage {} process {pid} exited before RSS sampling",
                        index + 1
                    ));
                }
                Err(error) => return Err(format!("failed to read {path}: {error}")),
            };
            let resident_pages = parse_resident_pages(&contents, &path)?;
            let resident_bytes = resident_pages
                .checked_mul(page_size)
                .ok_or_else(|| format!("resident size in {path} overflows bytes"))?;
            samples.push(resident_bytes);
        }
        Ok(Some(samples))
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = cages;
        Ok(None)
    }
}

#[cfg(target_os = "linux")]
fn parse_resident_pages(contents: &str, path: &str) -> Result<u64, String> {
    contents
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| format!("{path} does not contain a resident-page field"))?
        .parse()
        .map_err(|_| format!("{path} contains an invalid resident-page field"))
}

#[cfg(target_os = "linux")]
fn linux_page_size() -> u64 {
    let page_size = unsafe { sysconf(LINUX_SC_PAGESIZE) };
    u64::try_from(page_size)
        .ok()
        .filter(|value| *value > 0)
        .unwrap_or(FALLBACK_PAGE_SIZE)
}

struct Report<'a> {
    target_count: usize,
    achieved: usize,
    warmup: Duration,
    memory_before: Option<HostMemory>,
    memory_after: Option<HostMemory>,
    rss_samples: Option<&'a [u64]>,
    unloaded_boots: &'a [Duration],
    under_load_boots: &'a [Duration],
}

fn print_report(report: Report<'_>) {
    println!("G3 density results");
    println!("isolation: {}", cygnus_cage::ISOLATION);
    println!("target count: {}", report.target_count);
    println!("count achieved: {}", report.achieved);
    println!("warmup: {}", format_duration(report.warmup));

    match (report.memory_before, report.memory_after) {
        (Some(before), Some(after)) => print_memory_report(before, after),
        _ => {
            println!("MemTotal: n/a");
            println!("MemAvailable before: n/a");
            println!("MemAvailable after: n/a");
            println!("MemAvailable delta (before - after): n/a");
            println!("delta as % of MemTotal: n/a");
            println!(
                "memory gate (< 60% MemTotal used; G3 target: 200 cages on 16 GiB): n/a (Linux /proc required)"
            );
        }
    }

    match report.rss_samples {
        Some(samples) => {
            let aggregate = samples
                .iter()
                .copied()
                .fold(0_u64, u64::saturating_add);
            println!("aggregate cage RSS: {}", format_bytes(aggregate));
            println!("per-cage RSS mean: {}", format_bytes(mean_u64(samples)));
            println!(
                "per-cage RSS p99: {}",
                format_bytes(percentile(samples, 99))
            );
        }
        None => {
            println!("aggregate cage RSS: n/a");
            println!("per-cage RSS mean: n/a");
            println!("per-cage RSS p99: n/a");
        }
    }

    println!(
        "unloaded boot: mean {}, p50 {}, p99 {}",
        format_duration(mean_duration(report.unloaded_boots)),
        format_duration(percentile(report.unloaded_boots, 50)),
        format_duration(percentile(report.unloaded_boots, 99))
    );
    println!(
        "boot under load (revival proxy; scale-to-zero not implemented): mean {}, p50 {}, p99 {} ({} probes)",
        format_duration(mean_duration(report.under_load_boots)),
        format_duration(percentile(report.under_load_boots, 50)),
        format_duration(percentile(report.under_load_boots, 99)),
        report.under_load_boots.len()
    );
}

fn print_memory_report(before: HostMemory, after: HostMemory) {
    let total = after.total_bytes;
    let available_delta = i128::from(before.available_bytes) - i128::from(after.available_bytes);
    let delta_percent = percent(available_delta as f64, total);
    let used_after = total.saturating_sub(after.available_bytes.min(total));
    let used_percent = percent(used_after as f64, total);
    let gate = if used_percent < MEMORY_GATE_PERCENT {
        "PASS"
    } else {
        "FAIL"
    };

    println!("MemTotal: {}", format_bytes(total));
    println!(
        "MemAvailable before: {}",
        format_bytes(before.available_bytes)
    );
    println!(
        "MemAvailable after: {}",
        format_bytes(after.available_bytes)
    );
    println!(
        "MemAvailable delta (before - after): {}",
        format_signed_bytes(available_delta)
    );
    println!("delta as % of MemTotal: {delta_percent:.3}%");
    println!(
        "memory gate (< 60% MemTotal used; G3 target: 200 cages on 16 GiB): {gate} ({used_percent:.3}% used after warmup)"
    );
}

fn percent(value: f64, total: u64) -> f64 {
    if total == 0 {
        f64::NAN
    } else {
        value * 100.0 / total as f64
    }
}

fn percentile<T: Copy + Ord>(samples: &[T], percentile: usize) -> T {
    assert!(!samples.is_empty());
    assert!((1..=100).contains(&percentile));
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    let rank = percentile.saturating_mul(sorted.len()).div_ceil(100);
    sorted[rank.saturating_sub(1)]
}

fn mean_duration(samples: &[Duration]) -> Duration {
    assert!(!samples.is_empty());
    let total_ns: u128 = samples.iter().map(Duration::as_nanos).sum();
    duration_from_nanos(total_ns / samples.len() as u128)
}

fn duration_from_nanos(nanos: u128) -> Duration {
    let seconds = nanos / 1_000_000_000;
    let subsecond_nanos = (nanos % 1_000_000_000) as u32;
    Duration::new(seconds.min(u64::MAX as u128) as u64, subsecond_nanos)
}

fn mean_u64(samples: &[u64]) -> u64 {
    assert!(!samples.is_empty());
    let total: u128 = samples.iter().map(|value| u128::from(*value)).sum();
    (total / samples.len() as u128) as u64
}

fn format_duration(duration: Duration) -> String {
    let micros = duration.as_secs_f64() * 1_000_000.0;
    if micros >= 1_000.0 {
        format!("{:.3} ms", micros / 1_000.0)
    } else {
        format!("{micros:.3} us")
    }
}

fn format_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    const GIB: f64 = MIB * 1024.0;
    let bytes_float = bytes as f64;
    let human = if bytes_float >= GIB {
        format!("{:.3} GiB", bytes_float / GIB)
    } else if bytes_float >= MIB {
        format!("{:.3} MiB", bytes_float / MIB)
    } else if bytes_float >= KIB {
        format!("{:.3} KiB", bytes_float / KIB)
    } else {
        format!("{bytes} B")
    };
    format!("{human} ({bytes} bytes)")
}

fn format_signed_bytes(bytes: i128) -> String {
    if bytes < 0 {
        format!("-{}", format_bytes((-bytes) as u64))
    } else {
        format_bytes(bytes as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os_arguments(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    #[test]
    fn parses_the_documented_command_line() {
        let options = Options::parse(os_arguments(&[
            "--count",
            "7",
            "--warmup-ms",
            "250",
            "--",
            "/bin/sleep",
            "3600",
        ]))
        .expect("valid command line");

        assert_eq!(options.count, 7);
        assert_eq!(options.warmup, Duration::from_millis(250));
        assert_eq!(options.command, OsString::from("/bin/sleep"));
        assert_eq!(options.args, os_arguments(&["3600"]));
    }

    #[test]
    fn applies_defaults() {
        let options = Options::parse(os_arguments(&["--", "/bin/sleep", "3600"]))
            .expect("valid command line");

        assert_eq!(options.count, DEFAULT_COUNT);
        assert_eq!(options.warmup, Duration::ZERO);
    }

    #[test]
    fn rejects_zero_count_and_invalid_warmup() {
        assert!(Options::parse(os_arguments(&["--count", "0", "--", "true"])).is_err());
        assert!(
            Options::parse(os_arguments(&["--warmup-ms", "later", "--", "true"])).is_err()
        );
    }

    #[test]
    fn percentile_uses_nearest_rank() {
        let samples: Vec<_> = (1..=100).map(Duration::from_millis).collect();

        assert_eq!(percentile(&samples, 50), Duration::from_millis(50));
        assert_eq!(percentile(&samples, 99), Duration::from_millis(99));
    }

    #[test]
    fn percentile_sorts_and_handles_small_samples() {
        let samples = [40_u64, 10, 30, 20];

        assert_eq!(percentile(&samples, 50), 20);
        assert_eq!(percentile(&samples, 99), 40);
    }

    #[test]
    fn mean_duration_uses_nanosecond_precision() {
        let samples = [Duration::from_nanos(1), Duration::from_nanos(2)];

        assert_eq!(mean_duration(&samples), Duration::from_nanos(1));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn parses_linux_memory_and_resident_pages() {
        let meminfo = "MemTotal:       16384 kB\nMemAvailable:    4096 kB\n";

        assert_eq!(
            parse_meminfo_bytes(meminfo, "MemTotal:").expect("MemTotal"),
            16 * 1024 * 1024
        );
        assert_eq!(
            parse_meminfo_bytes(meminfo, "MemAvailable:").expect("MemAvailable"),
            4 * 1024 * 1024
        );
        assert_eq!(parse_resident_pages("10 3 1 0", "statm").unwrap(), 3);
    }
}
