//! Bounded, in-memory telemetry for the daemon's administrative API.
//!
//! Writers take one short mutex acquisition. Rolling-window maintenance and
//! bounded-ring updates happen under that lock; percentile and per-app work is
//! deferred until a snapshot is requested.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use cygnus_cage::BootTimings;
use serde::{Deserialize, Serialize};

pub const REQUEST_RING_CAPACITY: usize = 2_048;
pub const BOOT_RING_CAPACITY: usize = 256;
pub const EVENT_RING_CAPACITY: usize = 512;
pub const METRIC_MINUTES: usize = 60;
pub const MAX_LIST_LIMIT: usize = 500;

const WINDOW_SECONDS: u64 = 3_600;
const MILLIS_PER_MINUTE: u64 = 60_000;
const MAX_PATH_BYTES: usize = 200;
const MAX_BUCKET_SAMPLES: usize = 512;
const BOOT_PHASE_NAMES: [&str; 6] = [
    "namespaces_cgroup",
    "network",
    "mounts",
    "seccomp",
    "exec_runtime_init",
    "socket_ready",
];

/// One request as exposed by the admin request-list endpoint.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RequestRecord {
    pub time_ms: u64,
    pub request_id: String,
    pub method: String,
    pub host: String,
    pub app: String,
    pub path: String,
    pub status: u16,
    pub duration_ms: f64,
    pub cold: bool,
    pub protocol: String,
    pub bytes_in: u64,
    pub bytes_out: u64,
}

/// One operator-visible lifecycle or security event.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EventRecord {
    pub time_ms: u64,
    #[serde(rename = "type")]
    pub r#type: String,
    pub app: Option<String>,
    pub message: String,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MetricsTotals {
    pub requests_1m: u64,
    pub rps_1m: f64,
    pub error_rate_1m: f64,
    pub p50_ms: f64,
    pub p99_ms: f64,
    pub requests_1h: u64,
    pub error_rate_1h: f64,
    pub cold_starts_1h: u64,
    pub boot_p50_ms: f64,
    pub boot_p99_ms: f64,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MetricsSeriesEntry {
    /// Inclusive UTC minute start, in Unix seconds.
    pub t: i64,
    pub requests: u64,
    pub errors: u64,
    pub p50_ms: f64,
    pub p99_ms: f64,
    pub cold_starts: u64,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BootPhase {
    pub name: String,
    pub p50_ms: f64,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BootPhases {
    pub sample_count: u64,
    pub phases: Vec<BootPhase>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AppMetrics {
    pub app: String,
    pub rps_1m: f64,
    pub requests_1h: u64,
    pub error_rate_1m: f64,
    pub p50_ms: f64,
    pub p99_ms: f64,
}

/// Exact metrics payload consumed by the frontend.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MetricsSnapshot {
    pub window_seconds: u64,
    pub totals: MetricsTotals,
    /// Exactly 60 entries, oldest first.
    pub series: Vec<MetricsSeriesEntry>,
    pub boot_phases: BootPhases,
    pub apps: Vec<AppMetrics>,
}

#[derive(Clone, Copy, Debug, Default)]
struct BootPhaseSamples {
    namespaces_cgroup: f32,
    network: f32,
    mounts: f32,
    seccomp: f32,
    exec_runtime_init: f32,
    socket_ready: f32,
}

#[derive(Clone, Debug)]
struct BootRecord {
    #[allow(dead_code)]
    time_ms: u64,
    #[allow(dead_code)]
    app: String,
    total_ms: f32,
    phases: BootPhaseSamples,
}

#[derive(Clone, Debug, Default)]
struct MinuteBucket {
    minute: u64,
    requests: u64,
    errors: u64,
    latency: Vec<f32>,
    cold_starts: u64,
    boot_samples: Vec<f32>,
}

impl MinuteBucket {
    fn new(minute: u64) -> Self {
        Self {
            minute,
            ..Self::default()
        }
    }
}

#[derive(Clone, Debug, Default)]
struct Inner {
    requests: VecDeque<RequestRecord>,
    boots: VecDeque<BootRecord>,
    events: VecDeque<EventRecord>,
    buckets: VecDeque<MinuteBucket>,
}

/// Cloneable, bounded telemetry storage shared by edge and control-plane workers.
#[derive(Clone, Debug, Default)]
pub struct MetricsHub {
    inner: Arc<Mutex<Inner>>,
}

impl MetricsHub {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a completed request. The path is truncated to at most 200 bytes at
    /// a valid UTF-8 boundary before the single metrics lock is acquired.
    pub fn record_request(&self, mut request: RequestRecord) {
        truncate_utf8(&mut request.path, MAX_PATH_BYTES);
        request.duration_ms = normalized_f64(request.duration_ms);

        let minute = request.time_ms / MILLIS_PER_MINUTE;
        let is_error = request.status >= 500;
        let latency = to_f32_ms(request.duration_ms);
        let cold = request.cold;

        let mut inner = self.lock();
        roll_buckets(&mut inner.buckets, minute);
        if let Some(bucket) = bucket_mut(&mut inner.buckets, minute) {
            bucket.requests = bucket.requests.saturating_add(1);
            bucket.errors = bucket.errors.saturating_add(u64::from(is_error));
            bucket.cold_starts = bucket.cold_starts.saturating_add(u64::from(cold));
            push_sample(&mut bucket.latency, latency);
        }
        push_bounded(&mut inner.requests, request, REQUEST_RING_CAPACITY);
    }

    /// Record a completed cold boot and all six boot phases.
    pub fn record_boot(&self, time_ms: u64, app: impl Into<String>, timings: BootTimings) {
        let record = BootRecord {
            time_ms,
            app: app.into(),
            total_ms: duration_ms(timings.total),
            phases: BootPhaseSamples {
                namespaces_cgroup: duration_ms(timings.namespaces_cgroup),
                network: duration_ms(timings.network),
                mounts: duration_ms(timings.mounts),
                seccomp: duration_ms(timings.seccomp),
                exec_runtime_init: duration_ms(timings.exec_runtime_init),
                socket_ready: duration_ms(timings.socket_ready),
            },
        };
        let minute = time_ms / MILLIS_PER_MINUTE;

        let mut inner = self.lock();
        roll_buckets(&mut inner.buckets, minute);
        if let Some(bucket) = bucket_mut(&mut inner.buckets, minute) {
            push_sample(&mut bucket.boot_samples, record.total_ms);
        }
        push_bounded(&mut inner.boots, record, BOOT_RING_CAPACITY);
    }

    pub fn record_event(&self, event: EventRecord) {
        let mut inner = self.lock();
        push_bounded(&mut inner.events, event, EVENT_RING_CAPACITY);
    }

    /// Return up to 500 requests, newest first.
    #[must_use]
    pub fn list_requests(&self, limit: usize) -> Vec<RequestRecord> {
        newest(&self.lock().requests, limit)
    }

    /// Return up to 500 events, newest first.
    #[must_use]
    pub fn list_events(&self, limit: usize) -> Vec<EventRecord> {
        newest(&self.lock().events, limit)
    }

    #[must_use]
    pub fn snapshot(&self) -> MetricsSnapshot {
        self.snapshot_at(unix_millis())
    }

    /// Alias used by admin handlers that name the payload after the endpoint.
    #[must_use]
    pub fn metrics(&self) -> MetricsSnapshot {
        self.snapshot()
    }

    fn snapshot_at(&self, time_ms: u64) -> MetricsSnapshot {
        let current_minute = time_ms / MILLIS_PER_MINUTE;
        let inner = {
            let mut inner = self.lock();
            roll_buckets(&mut inner.buckets, current_minute);
            inner.clone()
        };

        let current = inner.buckets.back().expect("rolling window is populated");
        let requests_1m = current.requests;
        let errors_1m = current.errors;
        let requests_1h = inner
            .buckets
            .iter()
            .fold(0_u64, |sum, bucket| sum.saturating_add(bucket.requests));
        let errors_1h = inner
            .buckets
            .iter()
            .fold(0_u64, |sum, bucket| sum.saturating_add(bucket.errors));
        let cold_starts_1h = inner
            .buckets
            .iter()
            .fold(0_u64, |sum, bucket| sum.saturating_add(bucket.cold_starts));
        let mut minute_latencies = current.latency.clone();
        let mut hour_boots = inner
            .buckets
            .iter()
            .flat_map(|bucket| bucket.boot_samples.iter().copied())
            .collect::<Vec<_>>();

        let series = inner
            .buckets
            .iter()
            .map(|bucket| {
                let mut latency = bucket.latency.clone();
                MetricsSeriesEntry {
                    t: seconds_i64(bucket.minute),
                    requests: bucket.requests,
                    errors: bucket.errors,
                    p50_ms: percentile(&mut latency, 50),
                    p99_ms: percentile(&mut latency, 99),
                    cold_starts: bucket.cold_starts,
                }
            })
            .collect();

        let totals = MetricsTotals {
            requests_1m,
            rps_1m: requests_1m as f64 / 60.0,
            error_rate_1m: rate(errors_1m, requests_1m),
            p50_ms: percentile(&mut minute_latencies, 50),
            p99_ms: percentile(&mut minute_latencies, 99),
            requests_1h,
            error_rate_1h: rate(errors_1h, requests_1h),
            cold_starts_1h,
            boot_p50_ms: percentile(&mut hour_boots, 50),
            boot_p99_ms: percentile(&mut hour_boots, 99),
        };

        MetricsSnapshot {
            window_seconds: WINDOW_SECONDS,
            totals,
            series,
            boot_phases: boot_phases(&inner.boots),
            apps: app_metrics(&inner.requests, time_ms),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

fn unix_millis() -> u64 {
    u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(u64::MAX)
}

fn roll_buckets(buckets: &mut VecDeque<MinuteBucket>, minute: u64) {
    match buckets.back().map(|bucket| bucket.minute) {
        None => fill_window_ending_at(buckets, minute),
        Some(last) if minute > last => {
            if minute - last >= METRIC_MINUTES as u64 {
                fill_window_ending_at(buckets, minute);
            } else {
                for next in last + 1..=minute {
                    push_bounded(buckets, MinuteBucket::new(next), METRIC_MINUTES);
                }
            }
        }
        Some(_) => {}
    }
}

fn fill_window_ending_at(buckets: &mut VecDeque<MinuteBucket>, minute: u64) {
    buckets.clear();
    let first = minute.saturating_sub((METRIC_MINUTES - 1) as u64);
    for bucket_minute in first..=minute {
        buckets.push_back(MinuteBucket::new(bucket_minute));
    }
    while buckets.len() < METRIC_MINUTES {
        buckets.push_front(MinuteBucket::new(0));
    }
}

fn bucket_mut(buckets: &mut VecDeque<MinuteBucket>, minute: u64) -> Option<&mut MinuteBucket> {
    buckets
        .iter_mut()
        .rev()
        .find(|bucket| bucket.minute == minute)
}

fn push_bounded<T>(ring: &mut VecDeque<T>, value: T, capacity: usize) {
    if ring.len() == capacity {
        ring.pop_front();
    }
    ring.push_back(value);
}

fn push_sample(samples: &mut Vec<f32>, value: f32) {
    if samples.len() == MAX_BUCKET_SAMPLES {
        samples.remove(0);
    }
    samples.push(value);
}

fn newest<T: Clone>(ring: &VecDeque<T>, limit: usize) -> Vec<T> {
    ring.iter()
        .rev()
        .take(limit.min(MAX_LIST_LIMIT))
        .cloned()
        .collect()
}

fn app_metrics(requests: &VecDeque<RequestRecord>, time_ms: u64) -> Vec<AppMetrics> {
    #[derive(Default)]
    struct Aggregate {
        requests_1m: u64,
        errors_1m: u64,
        requests_1h: u64,
        latency_1m: Vec<f32>,
    }

    let minute_start = time_ms.saturating_sub(MILLIS_PER_MINUTE);
    let hour_start = time_ms.saturating_sub(WINDOW_SECONDS * 1_000);
    let mut aggregates: BTreeMap<&str, Aggregate> = BTreeMap::new();

    for request in requests
        .iter()
        .filter(|request| request.time_ms >= hour_start && request.time_ms <= time_ms)
    {
        let aggregate = aggregates.entry(&request.app).or_default();
        aggregate.requests_1h = aggregate.requests_1h.saturating_add(1);
        if request.time_ms >= minute_start {
            aggregate.requests_1m = aggregate.requests_1m.saturating_add(1);
            aggregate.errors_1m = aggregate
                .errors_1m
                .saturating_add(u64::from(request.status >= 500));
            push_sample(&mut aggregate.latency_1m, to_f32_ms(request.duration_ms));
        }
    }

    aggregates
        .into_iter()
        .map(|(app, mut aggregate)| AppMetrics {
            app: app.to_owned(),
            rps_1m: aggregate.requests_1m as f64 / 60.0,
            requests_1h: aggregate.requests_1h,
            error_rate_1m: rate(aggregate.errors_1m, aggregate.requests_1m),
            p50_ms: percentile(&mut aggregate.latency_1m, 50),
            p99_ms: percentile(&mut aggregate.latency_1m, 99),
        })
        .collect()
}

fn boot_phases(boots: &VecDeque<BootRecord>) -> BootPhases {
    let mut phase_samples: [Vec<f32>; 6] = std::array::from_fn(|_| Vec::new());
    for boot in boots {
        phase_samples[0].push(boot.phases.namespaces_cgroup);
        phase_samples[1].push(boot.phases.network);
        phase_samples[2].push(boot.phases.mounts);
        phase_samples[3].push(boot.phases.seccomp);
        phase_samples[4].push(boot.phases.exec_runtime_init);
        phase_samples[5].push(boot.phases.socket_ready);
    }

    BootPhases {
        sample_count: boots.len() as u64,
        phases: BOOT_PHASE_NAMES
            .into_iter()
            .zip(phase_samples)
            .map(|(name, mut samples)| BootPhase {
                name: name.to_owned(),
                p50_ms: percentile(&mut samples, 50),
            })
            .collect(),
    }
}

fn rate(numerator: u64, denominator: u64) -> f64 {
    if denominator == 0 {
        0.0
    } else {
        numerator as f64 / denominator as f64
    }
}

/// Nearest-rank percentile. Empty populations yield zero.
fn percentile(samples: &mut [f32], percentile: usize) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    debug_assert!((1..=100).contains(&percentile));
    samples.sort_unstable_by(f32::total_cmp);
    let rank = percentile.saturating_mul(samples.len()).div_ceil(100);
    f64::from(samples[rank.saturating_sub(1)])
}

fn duration_ms(duration: std::time::Duration) -> f32 {
    duration.as_secs_f32() * 1_000.0
}

fn normalized_f64(value: f64) -> f64 {
    if value.is_finite() && value >= 0.0 {
        value.min(f64::from(f32::MAX))
    } else {
        0.0
    }
}

fn to_f32_ms(value: f64) -> f32 {
    normalized_f64(value) as f32
}

fn seconds_i64(minute: u64) -> i64 {
    i64::try_from(minute.saturating_mul(60)).unwrap_or(i64::MAX)
}

fn truncate_utf8(value: &mut String, max_bytes: usize) {
    if value.len() <= max_bytes {
        return;
    }
    let mut boundary = max_bytes;
    while !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    value.truncate(boundary);
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::time::Duration;

    use serde_json::{Value, json};

    use super::*;

    fn request(sequence: usize, time_ms: u64, duration_ms: f64) -> RequestRecord {
        RequestRecord {
            time_ms,
            request_id: format!("request-{sequence}"),
            method: "GET".into(),
            host: "example.test".into(),
            app: "app".into(),
            path: "/".into(),
            status: 200,
            duration_ms,
            cold: false,
            protocol: "http/1.1".into(),
            bytes_in: 10,
            bytes_out: 20,
        }
    }

    fn timings(total_ms: u64) -> BootTimings {
        BootTimings {
            namespaces_cgroup: Duration::from_millis(1),
            network: Duration::from_millis(2),
            mounts: Duration::from_millis(3),
            seccomp: Duration::from_millis(4),
            exec_runtime_init: Duration::from_millis(5),
            socket_ready: Duration::from_millis(6),
            total: Duration::from_millis(total_ms),
        }
    }

    #[test]
    fn bucket_rollover_fills_empty_minutes_and_is_stable() {
        let hub = MetricsHub::new();
        let minute = 10_000_u64;
        hub.record_request(request(1, minute * MILLIS_PER_MINUTE + 1, 5.0));
        hub.record_request(request(2, (minute + 2) * MILLIS_PER_MINUTE + 1, 7.0));

        let first = hub.snapshot_at((minute + 2) * MILLIS_PER_MINUTE + 10);
        let second = hub.snapshot_at((minute + 2) * MILLIS_PER_MINUTE + 20);
        assert_eq!(first.series.len(), METRIC_MINUTES);
        assert_eq!(first.series, second.series);
        let tail = &first.series[METRIC_MINUTES - 3..];
        assert_eq!(tail[0].requests, 1);
        assert_eq!(tail[1].requests, 0);
        assert_eq!(tail[2].requests, 1);
        assert!(tail.windows(2).all(|pair| pair[1].t - pair[0].t == 60));

        let far_future = hub.snapshot_at((minute + 100) * MILLIS_PER_MINUTE);
        assert_eq!(far_future.series.len(), METRIC_MINUTES);
        assert!(far_future.series.iter().all(|entry| entry.requests == 0));
    }

    #[test]
    fn all_rings_and_bucket_samples_enforce_capacities() {
        let hub = MetricsHub::new();
        let now = 12_345 * MILLIS_PER_MINUTE;
        for sequence in 0..=REQUEST_RING_CAPACITY {
            hub.record_request(request(sequence, now, sequence as f64));
        }
        for sequence in 0..=EVENT_RING_CAPACITY {
            hub.record_event(EventRecord {
                time_ms: sequence as u64,
                r#type: "test".into(),
                app: None,
                message: sequence.to_string(),
            });
        }
        for sequence in 0..=BOOT_RING_CAPACITY {
            hub.record_boot(now, "app", timings(sequence as u64));
        }

        let inner = hub.lock();
        assert_eq!(inner.requests.len(), REQUEST_RING_CAPACITY);
        assert_eq!(inner.requests.front().unwrap().request_id, "request-1");
        assert_eq!(inner.events.len(), EVENT_RING_CAPACITY);
        assert_eq!(inner.events.front().unwrap().message, "1");
        assert_eq!(inner.boots.len(), BOOT_RING_CAPACITY);
        let current = inner.buckets.back().unwrap();
        assert_eq!(current.latency.len(), MAX_BUCKET_SAMPLES);
        assert_eq!(
            current.boot_samples.len(),
            MAX_BUCKET_SAMPLES.min(BOOT_RING_CAPACITY + 1)
        );
        drop(inner);

        assert_eq!(hub.list_requests(usize::MAX).len(), MAX_LIST_LIMIT);
        assert_eq!(hub.list_events(usize::MAX).len(), MAX_LIST_LIMIT);
        assert_eq!(
            hub.list_requests(1)[0].request_id,
            format!("request-{REQUEST_RING_CAPACITY}")
        );
    }

    #[test]
    fn percentile_uses_nearest_rank_math() {
        let mut samples: Vec<f32> = (1..=100).rev().map(|value| value as f32).collect();
        assert_eq!(percentile(&mut samples, 50), 50.0);
        assert_eq!(percentile(&mut samples, 99), 99.0);

        let mut small = [1.0, 2.0, 100.0];
        assert_eq!(percentile(&mut small, 50), 2.0);
        assert_eq!(percentile(&mut small, 99), 100.0);
        assert_eq!(percentile(&mut [], 50), 0.0);
    }

    #[test]
    fn exact_json_field_names_match_the_frontend_contract() {
        let hub = MetricsHub::new();
        let now = 20_000 * MILLIS_PER_MINUTE + 10;
        hub.record_request(request(1, now, 12.0));
        hub.record_boot(now, "app", timings(21));
        hub.record_event(EventRecord {
            time_ms: now,
            r#type: "deploy".into(),
            app: Some("app".into()),
            message: "ready".into(),
        });

        let snapshot = serde_json::to_value(hub.snapshot_at(now)).unwrap();
        assert_fields(
            &snapshot,
            &["window_seconds", "totals", "series", "boot_phases", "apps"],
        );
        assert_fields(
            &snapshot["totals"],
            &[
                "requests_1m",
                "rps_1m",
                "error_rate_1m",
                "p50_ms",
                "p99_ms",
                "requests_1h",
                "error_rate_1h",
                "cold_starts_1h",
                "boot_p50_ms",
                "boot_p99_ms",
            ],
        );
        assert_fields(
            &snapshot["series"][0],
            &["t", "requests", "errors", "p50_ms", "p99_ms", "cold_starts"],
        );
        assert_fields(&snapshot["boot_phases"], &["sample_count", "phases"]);
        assert_fields(&snapshot["boot_phases"]["phases"][0], &["name", "p50_ms"]);
        assert_fields(
            &snapshot["apps"][0],
            &[
                "app",
                "rps_1m",
                "requests_1h",
                "error_rate_1m",
                "p50_ms",
                "p99_ms",
            ],
        );

        let request = serde_json::to_value(&hub.list_requests(1)[0]).unwrap();
        assert_fields(
            &request,
            &[
                "time_ms",
                "request_id",
                "method",
                "host",
                "app",
                "path",
                "status",
                "duration_ms",
                "cold",
                "protocol",
                "bytes_in",
                "bytes_out",
            ],
        );
        assert_eq!(request.get("outcome"), None);

        let event = serde_json::to_value(&hub.list_events(1)[0]).unwrap();
        assert_fields(&event, &["time_ms", "type", "app", "message"]);
        assert_eq!(
            event,
            json!({
                "time_ms": now,
                "type": "deploy",
                "app": "app",
                "message": "ready",
            })
        );
    }

    #[test]
    fn path_truncation_preserves_utf8() {
        let hub = MetricsHub::new();
        let mut sample = request(1, unix_millis(), 1.0);
        sample.path = format!("{}é", "a".repeat(199));
        hub.record_request(sample);
        let path = &hub.list_requests(1)[0].path;
        assert_eq!(path.len(), 199);
        assert!(path.is_char_boundary(path.len()));
    }

    #[test]
    fn app_aggregates_use_snapshot_relative_windows() {
        let hub = MetricsHub::new();
        let now = 100 * MILLIS_PER_MINUTE;
        let mut old = request(1, now - MILLIS_PER_MINUTE - 1, 40.0);
        old.status = 500;
        hub.record_request(old);
        let mut recent = request(2, now - 500, 10.0);
        recent.status = 500;
        hub.record_request(recent);
        hub.record_request(request(3, now - 250, 20.0));

        let app = &hub.snapshot_at(now).apps[0];
        assert_eq!(app.requests_1h, 3);
        assert_eq!(app.rps_1m, 2.0 / 60.0);
        assert_eq!(app.error_rate_1m, 0.5);
        assert_eq!(app.p50_ms, 10.0);
        assert_eq!(app.p99_ms, 20.0);
    }

    fn assert_fields(value: &Value, expected: &[&str]) {
        let actual = value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<BTreeSet<_>>();
        let expected = expected.iter().copied().collect::<BTreeSet<_>>();
        assert_eq!(actual, expected);
    }
}
