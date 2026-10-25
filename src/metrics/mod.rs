//! Live metrics (phase 2.4): per-minute rollups for each app and model with HDR histograms, kept in memory for a
//! day and written to SQLite as minutes close, plus cumulative counters and fixed-bucket histograms rendered in
//! Prometheus' text format. Updated by the request-log sink, never on the forward path.

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use hdrhistogram::Histogram;
use hdrhistogram::serialization::{Serializer, V2Serializer};

use crate::cache::CacheResult;
use crate::proxy::record::{Finished, Outcome};
use crate::store::RollupRow;

const MINUTE_MS: i64 = 60_000;
/// Minutes kept in memory.
const KEEP_MINUTES: i64 = 24 * 60;
/// Prometheus buckets for request and first-token latency, in seconds.
const LATENCY_BUCKETS: [f64; 15] =
    [0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0];
/// Prometheus buckets for Jetway's own time before the upstream call, in seconds.
const ADDED_BUCKETS: [f64; 9] = [0.000_05, 0.000_1, 0.000_25, 0.000_5, 0.001, 0.002_5, 0.005, 0.01, 0.05];

fn histogram() -> Histogram<u64> {
    // Microseconds from 1 µs to one hour, two significant figures; `saturating_record` clamps anything beyond.
    Histogram::new_with_bounds(1, 3_600_000_000, 2)
        .unwrap_or_else(|_| Histogram::new(2).unwrap_or_else(|_| unreachable!()))
}

/// One minute of one app and model.
#[derive(Debug, Clone)]
pub struct Minute {
    pub requests: u64,
    pub outcomes: [u64; 5],
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub cache_hits: u64,
    pub cache_joined: u64,
    /// Tokens and GPU time that hits saved.
    pub saved_tokens: u64,
    pub saved_gpu_ms: u64,
    /// Microseconds.
    pub latency: Histogram<u64>,
    pub ttft: Histogram<u64>,
    pub added: Histogram<u64>,
    /// Output tokens per second, ×100.
    pub tokens_per_second: Histogram<u64>,
}

impl Default for Minute {
    fn default() -> Self {
        Self {
            requests: 0,
            outcomes: [0; 5],
            prompt_tokens: 0,
            completion_tokens: 0,
            cache_hits: 0,
            cache_joined: 0,
            saved_tokens: 0,
            saved_gpu_ms: 0,
            latency: histogram(),
            ttft: histogram(),
            added: histogram(),
            tokens_per_second: histogram(),
        }
    }
}

impl Minute {
    pub fn count(&self, outcome: Outcome) -> u64 {
        self.outcomes[outcome_index(outcome)]
    }

    pub fn merge(&mut self, other: &Minute) {
        self.requests += other.requests;
        for (a, b) in self.outcomes.iter_mut().zip(other.outcomes) {
            *a += b;
        }
        self.prompt_tokens += other.prompt_tokens;
        self.completion_tokens += other.completion_tokens;
        self.cache_hits += other.cache_hits;
        self.cache_joined += other.cache_joined;
        self.saved_tokens += other.saved_tokens;
        self.saved_gpu_ms += other.saved_gpu_ms;
        let _ = self.latency.add(&other.latency);
        let _ = self.ttft.add(&other.ttft);
        let _ = self.added.add(&other.added);
        let _ = self.tokens_per_second.add(&other.tokens_per_second);
    }
}

fn outcome_index(outcome: Outcome) -> usize {
    Outcome::ALL.iter().position(|o| *o == outcome).unwrap_or(0)
}

/// Cumulative fixed-bucket histogram for Prometheus.
#[derive(Debug, Clone)]
struct Buckets {
    bounds: &'static [f64],
    counts: Vec<u64>,
    sum: f64,
    count: u64,
}

impl Buckets {
    fn new(bounds: &'static [f64]) -> Self {
        Self { bounds, counts: vec![0; bounds.len()], sum: 0.0, count: 0 }
    }

    fn observe(&mut self, seconds: f64) {
        for (bound, count) in self.bounds.iter().zip(&mut self.counts) {
            if seconds <= *bound {
                *count += 1;
            }
        }
        self.sum += seconds;
        self.count += 1;
    }
}

type Key = (String, String);

#[derive(Default)]
struct State {
    minutes: BTreeMap<(i64, String, String), Minute>,
    requests: HashMap<(String, String, Outcome), u64>,
    tokens: HashMap<(String, String, &'static str), u64>,
    latency: HashMap<Key, Buckets>,
    ttft: HashMap<Key, Buckets>,
    added: HashMap<Key, Buckets>,
    cache: HashMap<(String, CacheResult), u64>,
    saved_tokens: HashMap<String, u64>,
    saved_gpu_ms: HashMap<String, u64>,
    incomplete: u64,
    truncated: u64,
    /// Minutes already written to the store and not changed since.
    flushed_until: i64,
}

#[derive(Default)]
pub struct Metrics {
    state: Mutex<State>,
}

/// Gauges that live elsewhere, passed in when rendering.
#[derive(Debug, Clone, Copy, Default)]
pub struct Gauges {
    pub in_flight: usize,
    pub log_dropped: u64,
    pub log_write_errors: u64,
    pub memory_bytes: u64,
    pub uptime: Duration,
    pub cache_entries: u64,
    pub cache_bytes: u64,
    pub flights: usize,
}

fn micros(d: Option<Duration>) -> Option<u64> {
    d.map(|d| u64::try_from(d.as_micros()).unwrap_or(u64::MAX).max(1))
}

impl Metrics {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn observe(&self, f: &Finished) {
        let started_ms = f.exchange.started_at.as_millisecond();
        let minute = started_ms - started_ms.rem_euclid(MINUTE_MS);
        let app = f.exchange.app.clone();
        let model = f.exchange.summary.as_ref().and_then(|s| s.model.clone()).unwrap_or_default();
        let prompt = f.summary.prompt_tokens.unwrap_or(0);
        let completion = f.summary.completion_tokens.unwrap_or(0);
        let latency = micros(f.timings.done);
        let ttft = micros(f.timings.first_token);
        let added = micros(f.timings.upstream_sent);
        // Ollama's own generation rate when it reports one, otherwise the rate the client saw after the first token.
        let generation =
            f.summary.eval_ns.map(Duration::from_nanos).or_else(|| match (f.timings.first_token, f.timings.done) {
                (Some(first), Some(done)) if done > first => Some(done - first),
                _ => None,
            });
        let tps = generation
            .filter(|g| !g.is_zero() && completion > 0)
            .map(|g| (completion as f64 / g.as_secs_f64() * 100.0).round() as u64);
        let note = f.exchange.cache.as_ref();
        let cache_result = note.map(|n| n.result);
        let saved_tokens =
            note.map_or(0, |n| n.saved_prompt_tokens.unwrap_or(0) + n.saved_completion_tokens.unwrap_or(0));
        let saved_gpu_ms = note.and_then(|n| n.saved_gpu_ms).unwrap_or(0);

        let mut state = self.lock();
        let entry = state.minutes.entry((minute, app.clone(), model.clone())).or_default();
        entry.requests += 1;
        entry.outcomes[outcome_index(f.outcome)] += 1;
        entry.prompt_tokens += prompt;
        entry.completion_tokens += completion;
        entry.cache_hits += u64::from(cache_result == Some(CacheResult::Hit));
        entry.cache_joined += u64::from(cache_result == Some(CacheResult::Joined));
        entry.saved_tokens += saved_tokens;
        entry.saved_gpu_ms += saved_gpu_ms;
        for (histogram, value) in [
            (&mut entry.latency, latency),
            (&mut entry.ttft, ttft),
            (&mut entry.added, added),
            (&mut entry.tokens_per_second, tps),
        ] {
            if let Some(value) = value {
                histogram.saturating_record(value.max(1));
            }
        }
        if minute < state.flushed_until {
            // A late exchange changed a minute already written; write it again.
            state.flushed_until = minute;
        }

        *state.requests.entry((app.clone(), model.clone(), f.outcome)).or_default() += 1;
        if let Some(result) = cache_result {
            *state.cache.entry((app.clone(), result)).or_default() += 1;
            *state.saved_tokens.entry(app.clone()).or_default() += saved_tokens;
            *state.saved_gpu_ms.entry(app.clone()).or_default() += saved_gpu_ms;
        }
        *state.tokens.entry((app.clone(), model.clone(), "prompt")).or_default() += prompt;
        *state.tokens.entry((app.clone(), model.clone(), "completion")).or_default() += completion;
        let key = (app, model);
        if let Some(us) = latency {
            state.latency.entry(key.clone()).or_insert_with(|| Buckets::new(&LATENCY_BUCKETS)).observe(us as f64 / 1e6);
        }
        if let Some(us) = ttft {
            state.ttft.entry(key.clone()).or_insert_with(|| Buckets::new(&LATENCY_BUCKETS)).observe(us as f64 / 1e6);
        }
        if let Some(us) = added {
            state.added.entry(key).or_insert_with(|| Buckets::new(&ADDED_BUCKETS)).observe(us as f64 / 1e6);
        }
        state.incomplete += u64::from(f.incomplete);
        state.truncated += u64::from(f.truncated);
    }

    /// Minutes in `[from_ms, to_ms)`, for one app or all, merged per minute (the dashboard reads this in phase 4).
    pub fn minutes(&self, from_ms: i64, to_ms: i64, app: Option<&str>) -> BTreeMap<i64, Minute> {
        let state = self.lock();
        let mut merged: BTreeMap<i64, Minute> = BTreeMap::new();
        for ((minute, a, _), m) in state.minutes.range((from_ms, String::new(), String::new())..) {
            if *minute >= to_ms {
                break;
            }
            if app.is_none_or(|app| app == a) {
                merged.entry(*minute).or_default().merge(m);
            }
        }
        merged
    }

    /// Rollup rows for minutes that have closed since the last call (and any that late exchanges changed), and
    /// forgets minutes older than a day. `now_ms` is the current time.
    pub fn take_closed(&self, now_ms: i64) -> Vec<RollupRow> {
        let current = now_ms - now_ms.rem_euclid(MINUTE_MS);
        let mut state = self.lock();
        let from = state.flushed_until;
        let rows = state
            .minutes
            .range((from, String::new(), String::new())..)
            .take_while(|((minute, _, _), _)| *minute < current)
            .map(|((minute, app, model), m)| to_row(*minute, app, model, m))
            .collect();
        state.flushed_until = current;
        let oldest = current - KEEP_MINUTES * MINUTE_MS;
        state.minutes.retain(|(minute, _, _), _| *minute >= oldest);
        rows
    }

    /// The Prometheus text exposition of everything above plus the given gauges.
    pub fn render(&self, gauges: Gauges) -> String {
        let state = self.lock();
        let mut out = String::new();
        let label = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n");

        out.push_str("# HELP jetway_requests_total Exchanges finished, by app, model and outcome.\n");
        out.push_str("# TYPE jetway_requests_total counter\n");
        let mut requests: Vec<_> = state.requests.iter().collect();
        requests.sort_by(|a, b| a.0.0.cmp(&b.0.0).then(a.0.1.cmp(&b.0.1)).then(a.0.2.as_str().cmp(b.0.2.as_str())));
        for ((app, model, outcome), n) in requests {
            let _ = writeln!(
                out,
                "jetway_requests_total{{app=\"{}\",model=\"{}\",outcome=\"{}\"}} {n}",
                label(app),
                label(model),
                outcome.as_str()
            );
        }

        out.push_str("# HELP jetway_tokens_total Tokens reported by the upstream, by kind.\n");
        out.push_str("# TYPE jetway_tokens_total counter\n");
        let mut tokens: Vec<_> = state.tokens.iter().collect();
        tokens.sort_by(|a, b| a.0.cmp(b.0));
        for ((app, model, kind), n) in tokens {
            let _ = writeln!(
                out,
                "jetway_tokens_total{{app=\"{}\",model=\"{}\",kind=\"{kind}\"}} {n}",
                label(app),
                label(model)
            );
        }

        out.push_str("# HELP jetway_cache_total Generation and embedding requests by cache result.\n");
        out.push_str("# TYPE jetway_cache_total counter\n");
        let mut cache: Vec<_> = state.cache.iter().collect();
        cache.sort_by(|a, b| a.0.0.cmp(&b.0.0).then(a.0.1.as_str().cmp(b.0.1.as_str())));
        for ((app, result), n) in cache {
            let _ = writeln!(out, "jetway_cache_total{{app=\"{}\",result=\"{}\"}} {n}", label(app), result.as_str());
        }
        for (name, help, map, scale) in [
            ("jetway_cache_saved_tokens_total", "Tokens that cache hits saved.", &state.saved_tokens, 1.0),
            ("jetway_cache_saved_gpu_seconds_total", "Upstream time that cache hits saved.", &state.saved_gpu_ms, 1e-3),
        ] {
            let _ = writeln!(out, "# HELP {name} {help}\n# TYPE {name} counter");
            let mut entries: Vec<_> = map.iter().collect();
            entries.sort();
            for (app, n) in entries {
                let _ = writeln!(out, "{name}{{app=\"{}\"}} {}", label(app), *n as f64 * scale);
            }
        }

        for (name, help, map) in [
            ("jetway_request_duration_seconds", "Time from receiving a request to its last byte.", &state.latency),
            (
                "jetway_time_to_first_token_seconds",
                "Time from receiving a request to the first answer token.",
                &state.ttft,
            ),
            ("jetway_added_seconds", "Jetway's own time before the upstream call.", &state.added),
        ] {
            let _ = writeln!(out, "# HELP {name} {help}\n# TYPE {name} histogram");
            let mut entries: Vec<_> = map.iter().collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            for ((app, model), buckets) in entries {
                let labels = format!("app=\"{}\",model=\"{}\"", label(app), label(model));
                for (bound, count) in buckets.bounds.iter().zip(&buckets.counts) {
                    let _ = writeln!(out, "{name}_bucket{{{labels},le=\"{bound}\"}} {count}");
                }
                let _ = writeln!(out, "{name}_bucket{{{labels},le=\"+Inf\"}} {}", buckets.count);
                let _ = writeln!(out, "{name}_sum{{{labels}}} {}", buckets.sum);
                let _ = writeln!(out, "{name}_count{{{labels}}} {}", buckets.count);
            }
        }

        let counters = [
            ("jetway_log_dropped_total", "Records dropped because the store's queue was full.", gauges.log_dropped),
            ("jetway_log_write_errors_total", "Store batches that failed to write.", gauges.log_write_errors),
            ("jetway_recorder_incomplete_total", "Exchanges whose recording skipped a chunk.", state.incomplete),
            ("jetway_recorder_truncated_total", "Exchanges whose recording hit the size cap.", state.truncated),
        ];
        for (name, help, value) in counters {
            let _ = writeln!(out, "# HELP {name} {help}\n# TYPE {name} counter\n{name} {value}");
        }
        let gauge_values = [
            ("jetway_in_flight", "Exchanges in progress.", gauges.in_flight as f64),
            ("jetway_memory_bytes", "Jetway's resident memory.", gauges.memory_bytes as f64),
            ("jetway_uptime_seconds", "Seconds since Jetway started.", gauges.uptime.as_secs_f64()),
            ("jetway_cache_entries", "Cache entries held in memory.", gauges.cache_entries as f64),
            ("jetway_cache_memory_bytes", "Memory the cache's entries use.", gauges.cache_bytes as f64),
            ("jetway_flights", "Upstream calls that identical requests can join.", gauges.flights as f64),
        ];
        for (name, help, value) in gauge_values {
            let _ = writeln!(out, "# HELP {name} {help}\n# TYPE {name} gauge\n{name} {value}");
        }
        out
    }
}

fn serialize(histogram: &Histogram<u64>) -> Vec<u8> {
    let mut bytes = Vec::new();
    let _ = V2Serializer::new().serialize(histogram, &mut bytes);
    bytes
}

fn to_row(minute: i64, app: &str, model: &str, m: &Minute) -> RollupRow {
    let int = |n: u64| i64::try_from(n).unwrap_or(i64::MAX);
    RollupRow {
        minute_ms: minute,
        app: app.to_string(),
        model: model.to_string(),
        requests: int(m.requests),
        ok: int(m.count(Outcome::Ok)),
        upstream_errors: int(m.count(Outcome::UpstreamError)),
        cancelled: int(m.count(Outcome::ClientCancelled)),
        timeouts: int(m.count(Outcome::Timeout)),
        rejected: int(m.count(Outcome::Rejected)),
        prompt_tokens: int(m.prompt_tokens),
        completion_tokens: int(m.completion_tokens),
        cache_hits: int(m.cache_hits),
        cache_joined: int(m.cache_joined),
        saved_tokens: int(m.saved_tokens),
        saved_gpu_ms: int(m.saved_gpu_ms),
        latency: serialize(&m.latency),
        ttft: serialize(&m.ttft),
        added: serialize(&m.added),
        tokens_per_second: serialize(&m.tokens_per_second),
    }
}

impl crate::proxy::record::Sink for Metrics {
    fn finished(&self, finished: Finished) {
        self.observe(&finished);
    }
}

/// Jetway's resident memory, if the platform reports it.
pub fn memory_bytes() -> u64 {
    memory_stats::memory_stats().map_or(0, |m| m.physical_mem as u64)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use bytes::Bytes;
    use tokio::time::Instant;

    use super::*;
    use crate::protocol::summary::{RequestSummary, ResponseSummary};
    use crate::protocol::{Framing, Protocol};
    use crate::proxy::record::{Exchange, Timings};
    use crate::server::routes::PathClass;

    fn finished(at_ms: i64, app: &str, outcome: Outcome, done_ms: u64, eval_ns: Option<u64>) -> Finished {
        Finished {
            exchange: Exchange {
                id: ulid::Ulid::nil(),
                app: app.into(),
                method: axum::http::Method::POST,
                path: "/api/chat".into(),
                protocol: Protocol::Ollama,
                class: PathClass::Generation,
                started: Instant::now(),
                started_at: jiff::Timestamp::from_millisecond(at_ms).unwrap(),
                request: Bytes::new(),
                summary: Some(RequestSummary { model: Some("m".into()), ..RequestSummary::default() }),
                traceparent: None,
                spans: None,
                cache: None,
            },
            status: 200,
            outcome,
            error: None,
            timings: Timings {
                upstream_sent: Some(Duration::from_micros(80)),
                first_token: Some(Duration::from_millis(100)),
                done: Some(Duration::from_millis(done_ms)),
                ..Timings::default()
            },
            framing: Framing::Ndjson,
            response: Bytes::new(),
            response_bytes: 0,
            summary: ResponseSummary {
                prompt_tokens: Some(18),
                completion_tokens: Some(10),
                eval_ns,
                ..ResponseSummary::default()
            },
            truncated: false,
            incomplete: false,
        }
    }

    #[test]
    fn minutes_outcomes_and_rates() {
        let metrics = Metrics::default();
        // Two exchanges in the minute starting at 120 000 ms, one in the next.
        metrics.observe(&finished(120_500, "a", Outcome::Ok, 1_100, Some(500_000_000)));
        metrics.observe(&finished(179_999, "a", Outcome::Timeout, 2_000, None));
        metrics.observe(&finished(180_000, "b", Outcome::Ok, 600, Some(250_000_000)));

        let all = metrics.minutes(0, i64::MAX, None);
        assert_eq!(all.keys().copied().collect::<Vec<_>>(), [120_000, 180_000]);
        let first = &all[&120_000];
        assert_eq!((first.requests, first.count(Outcome::Ok), first.count(Outcome::Timeout)), (2, 1, 1));
        assert_eq!(first.completion_tokens, 20);
        // 10 tokens in 0.5 s from Ollama's eval_duration; 10 tokens in 1.9 s after the first token otherwise.
        // Two significant figures: values are exact to within 1%.
        assert!((first.tokens_per_second.max() as i64 - 2_000).abs() <= 20, "{}", first.tokens_per_second.max());
        assert!((first.tokens_per_second.min() as i64 - 526).abs() <= 5, "{}", first.tokens_per_second.min());
        assert!((first.latency.value_at_quantile(1.0) as i64 - 2_000_000).abs() < 20_000);
        assert_eq!(metrics.minutes(0, i64::MAX, Some("b")).len(), 1);
    }

    #[test]
    fn closed_minutes_are_taken_once_unless_a_late_exchange_changes_them() {
        let metrics = Metrics::default();
        metrics.observe(&finished(60_000, "a", Outcome::Ok, 100, None));
        metrics.observe(&finished(130_000, "a", Outcome::Ok, 100, None));
        // At 125 000 ms only the minute starting at 60 000 has closed.
        assert_eq!(metrics.take_closed(125_000).iter().map(|r| r.minute_ms).collect::<Vec<_>>(), [60_000]);
        assert!(metrics.take_closed(125_000).is_empty());
        // A late exchange for the closed minute makes it due again, with both exchanges.
        metrics.observe(&finished(61_000, "a", Outcome::Ok, 100, None));
        let rows = metrics.take_closed(185_000);
        assert_eq!(rows.iter().map(|r| (r.minute_ms, r.requests)).collect::<Vec<_>>(), [(60_000, 2), (120_000, 1)]);
    }

    #[test]
    fn prometheus_text() {
        let metrics = Metrics::default();
        metrics.observe(&finished(0, "a\"b", Outcome::Ok, 30, None));
        let text = metrics.render(Gauges { in_flight: 2, ..Gauges::default() });
        assert!(text.contains(r#"jetway_requests_total{app="a\"b",model="m",outcome="ok"} 1"#), "{text}");
        assert!(text.contains(r#"jetway_request_duration_seconds_bucket{app="a\"b",model="m",le="0.05"} 1"#));
        assert!(text.contains(r#"jetway_request_duration_seconds_bucket{app="a\"b",model="m",le="0.025"} 0"#));
        assert!(text.contains("jetway_in_flight 2"));
    }
}
