use std::env;
use std::ffi::OsString;
use std::fmt::Write as _;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use cygnus_cage::{BootTimings, Cage, CageError, CageSpec};

const DEFAULT_RUNS: usize = 100;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("g1: {message}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let options = Options::parse(env::args_os().skip(1))?;
    let mut samples = Vec::with_capacity(options.runs);

    for run_index in 0..options.runs {
        let mut spec = CageSpec::new(
            format!("g1-{}-{run_index}", std::process::id()),
            options.command.clone(),
        );
        spec.args = options.args.clone();
        spec.env = env::vars_os().collect();
        spec.readiness_uds = options.readiness_uds.clone();

        let cage = match Cage::boot(spec) {
            Ok(cage) => cage,
            Err(error) if run_index == 0 && environment_unavailable(&error) => {
                return Err(format!(
                    "environment cannot create cages: {error}. Run on Linux with user \
                     namespaces enabled and a writable delegated cgroup v2 subtree"
                ));
            }
            Err(error) => return Err(format!("run {} failed to boot: {error}", run_index + 1)),
        };
        let timings = cage.timings();
        cage
            .teardown()
            .map_err(|error| format!("run {} failed to tear down: {error}", run_index + 1))?;
        samples.push(timings);
    }

    print_report(&samples);
    Ok(())
}

#[derive(Debug, Eq, PartialEq)]
struct Options {
    runs: usize,
    readiness_uds: Option<PathBuf>,
    command: OsString,
    args: Vec<OsString>,
}

impl Options {
    fn parse(arguments: impl IntoIterator<Item = OsString>) -> Result<Self, String> {
        let mut runs = DEFAULT_RUNS;
        let mut readiness_uds = None;
        let mut arguments = arguments.into_iter();

        loop {
            let argument = arguments.next().ok_or_else(usage)?;
            if argument == "--" {
                break;
            }
            if argument == "--help" || argument == "-h" {
                return Err(usage());
            }
            if argument == "--runs" {
                let value = arguments
                    .next()
                    .ok_or_else(|| "--runs requires a positive integer".to_owned())?;
                let text = value
                    .to_str()
                    .ok_or_else(|| "--runs must be valid UTF-8".to_owned())?;
                runs = text
                    .parse()
                    .map_err(|_| "--runs requires a positive integer".to_owned())?;
                if runs == 0 {
                    return Err("--runs must be greater than zero".into());
                }
                continue;
            }
            if argument == "--uds" {
                let value = arguments
                    .next()
                    .ok_or_else(|| "--uds requires an absolute socket path".to_owned())?;
                let path = PathBuf::from(value);
                if !path.is_absolute() {
                    return Err("--uds requires an absolute socket path".into());
                }
                readiness_uds = Some(path);
                continue;
            }
            return Err(format!("unknown option {:?}\n{}", argument, usage()));
        }

        let command = arguments
            .next()
            .ok_or_else(|| format!("missing command\n{}", usage()))?;
        let args = arguments.collect();
        Ok(Self {
            runs,
            readiness_uds,
            command,
            args,
        })
    }
}

fn usage() -> String {
    "usage: g1 [--runs N] [--uds /absolute/path.sock] -- <cmd> [args...]".into()
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

fn print_report(samples: &[BootTimings]) {
    let totals: Vec<_> = samples.iter().map(|sample| sample.total).collect();
    println!("G1 cold-start results");
    println!("runs: {}", samples.len());
    println!("total p50: {}", format_duration(percentile(&totals, 50)));
    println!("total p95: {}", format_duration(percentile(&totals, 95)));
    println!("total p99: {}", format_duration(percentile(&totals, 99)));
    println!("mean phases:");
    println!(
        "  namespaces+cgroup: {}",
        format_duration(mean_phase(samples, |sample| sample.namespaces_cgroup))
    );
    println!(
        "  mounts: {}",
        format_duration(mean_phase(samples, |sample| sample.mounts))
    );
    println!(
        "  exec+runtime-init: {}",
        format_duration(mean_phase(samples, |sample| sample.exec_runtime_init))
    );
    println!(
        "  socket-ready: {}",
        format_duration(mean_phase(samples, |sample| sample.socket_ready))
    );
}

fn percentile(samples: &[Duration], percentile: usize) -> Duration {
    assert!(!samples.is_empty());
    assert!((1..=100).contains(&percentile));
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    let rank = percentile.saturating_mul(sorted.len()).div_ceil(100);
    sorted[rank.saturating_sub(1)]
}

fn mean_phase(samples: &[BootTimings], select: impl Fn(&BootTimings) -> Duration) -> Duration {
    assert!(!samples.is_empty());
    let total_ns: u128 = samples.iter().map(|sample| select(sample).as_nanos()).sum();
    duration_from_nanos(total_ns / samples.len() as u128)
}

fn duration_from_nanos(nanos: u128) -> Duration {
    let seconds = nanos / 1_000_000_000;
    let subsecond_nanos = (nanos % 1_000_000_000) as u32;
    Duration::new(seconds.min(u64::MAX as u128) as u64, subsecond_nanos)
}

fn format_duration(duration: Duration) -> String {
    let mut output = String::new();
    let micros = duration.as_secs_f64() * 1_000_000.0;
    if micros >= 1_000.0 {
        let _ = write!(&mut output, "{:.3} ms", micros / 1_000.0);
    } else {
        let _ = write!(&mut output, "{micros:.3} us");
    }
    output
}
