//! What the dashboard shows, assembled from the in-memory rollups (phase 4.2): time buckets for a window, the stat
//! tiles over that window, and the rows behind each chart's table view. The admin JSON API returns the same data.

use std::collections::BTreeMap;

use hdrhistogram::Histogram;
use serde::Serialize;

use super::charts::{self, Series, Unit};
use crate::metrics::{Metrics, Minute};
use crate::proxy::record::Outcome;

const MINUTE_MS: i64 = 60_000;

/// The time windows the filter row offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Window {
    #[serde(rename = "15m")]
    Minutes15,
    #[serde(rename = "1h")]
    Hour,
    #[serde(rename = "24h")]
    Day,
}

impl Window {
    pub const ALL: [Window; 3] = [Self::Minutes15, Self::Hour, Self::Day];

    pub fn parse(text: Option<&str>) -> Self {
        match text {
            Some("15m") => Self::Minutes15,
            Some("24h") => Self::Day,
            _ => Self::Hour,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Minutes15 => "15m",
            Self::Hour => "1h",
            Self::Day => "24h",
        }
    }

    pub fn minutes(self) -> i64 {
        match self {
            Self::Minutes15 => 15,
            Self::Hour => 60,
            Self::Day => 24 * 60,
        }
    }

    /// Minutes per chart slot: one per minute, or 15 for a day (96 slots).
    pub fn bucket_minutes(self) -> i64 {
        if self == Self::Day { 15 } else { 1 }
    }
}

/// One chart slot: its start, label and the merged rollup.
pub struct Bucket {
    pub start_ms: i64,
    pub label: String,
    pub minute: Minute,
}

pub fn local_time(ms: i64) -> String {
    jiff::Timestamp::from_millisecond(ms)
        .map(|t| t.to_zoned(jiff::tz::TimeZone::system()).strftime("%H:%M").to_string())
        .unwrap_or_default()
}

pub fn local_datetime(ms: i64) -> String {
    jiff::Timestamp::from_millisecond(ms)
        .map(|t| t.to_zoned(jiff::tz::TimeZone::system()).strftime("%Y-%m-%d %H:%M:%S").to_string())
        .unwrap_or_default()
}

pub fn local_date_time_short(ms: i64) -> String {
    jiff::Timestamp::from_millisecond(ms)
        .map(|t| t.to_zoned(jiff::tz::TimeZone::system()).strftime("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_default()
}

/// Buckets ending with the current one.
pub fn buckets(metrics: &Metrics, window: Window, app: Option<&str>, now_ms: i64) -> Vec<Bucket> {
    let size = window.bucket_minutes() * MINUTE_MS;
    let current = now_ms - now_ms.rem_euclid(size);
    let count = window.minutes() / window.bucket_minutes();
    let first = current - (count - 1) * size;
    let minutes: BTreeMap<i64, Minute> = metrics.minutes(first, current + size, app);
    (0..count)
        .map(|i| {
            let start = first + i * size;
            let mut merged = Minute::default();
            for (_, m) in minutes.range(start..start + size) {
                merged.merge(m);
            }
            Bucket { start_ms: start, label: local_time(start), minute: merged }
        })
        .collect()
}

fn quantile_ms(histogram: &Histogram<u64>, q: f64) -> Option<f64> {
    (!histogram.is_empty()).then(|| histogram.value_at_quantile(q) as f64 / 1000.0)
}

fn errors(m: &Minute) -> u64 {
    m.count(Outcome::UpstreamError) + m.count(Outcome::Timeout)
}

/// The window's headline numbers.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Tiles {
    pub requests: u64,
    pub requests_per_minute: f64,
    pub p50_ms: Option<f64>,
    pub p95_ms: Option<f64>,
    pub ttft_p50_ms: Option<f64>,
    pub tokens_per_second: Option<f64>,
    pub cache_hit_rate: Option<f64>,
    pub error_rate: Option<f64>,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub saved_tokens: u64,
    pub saved_gpu_seconds: f64,
}

/// One row of a chart's table view.
#[derive(Debug, Clone, Serialize)]
pub struct Row {
    pub start_ms: i64,
    pub time: String,
    pub requests: u64,
    pub ok: u64,
    pub errors: u64,
    pub other: u64,
    pub p50_ms: Option<f64>,
    pub p95_ms: Option<f64>,
    pub p99_ms: Option<f64>,
    pub ttft_p50_ms: Option<f64>,
    pub ttft_p95_ms: Option<f64>,
    pub cache_hits: u64,
    pub cache_joined: u64,
    pub sent_upstream: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Overview {
    pub window: Window,
    pub app: Option<String>,
    pub tiles: Tiles,
    pub rows: Vec<Row>,
    #[serde(skip)]
    pub charts: Charts,
}

#[derive(Debug, Clone, Default)]
pub struct Charts {
    pub latency: String,
    pub ttft: String,
    pub outcomes: String,
    pub cache: String,
}

pub fn tiles(total: &Minute, window: Window) -> Tiles {
    // Rollups written before the cacheable count existed have hits but no count; never divide by less than them.
    let looked_at = total.cacheable.max(total.cache_hits + total.cache_joined);
    let tps =
        (!total.tokens_per_second.is_empty()).then(|| total.tokens_per_second.value_at_quantile(0.5) as f64 / 100.0);
    Tiles {
        requests: total.requests,
        requests_per_minute: total.requests as f64 / window.minutes() as f64,
        p50_ms: quantile_ms(&total.latency, 0.5),
        p95_ms: quantile_ms(&total.latency, 0.95),
        ttft_p50_ms: quantile_ms(&total.ttft, 0.5),
        tokens_per_second: tps,
        cache_hit_rate: (looked_at > 0).then(|| total.cache_hits as f64 / looked_at as f64),
        error_rate: (total.requests > 0).then(|| errors(total) as f64 / total.requests as f64),
        prompt_tokens: total.prompt_tokens,
        completion_tokens: total.completion_tokens,
        saved_tokens: total.saved_tokens,
        saved_gpu_seconds: total.saved_gpu_ms as f64 / 1000.0,
    }
}

pub fn overview(metrics: &Metrics, window: Window, app: Option<&str>, now_ms: i64) -> Overview {
    let buckets = buckets(metrics, window, app, now_ms);
    let mut total = Minute::default();
    for b in &buckets {
        total.merge(&b.minute);
    }
    let rows: Vec<Row> = buckets
        .iter()
        .map(|b| {
            let m = &b.minute;
            let errors = errors(m);
            let ok = m.count(Outcome::Ok);
            Row {
                start_ms: b.start_ms,
                time: b.label.clone(),
                requests: m.requests,
                ok,
                errors,
                other: m.requests.saturating_sub(ok + errors),
                p50_ms: quantile_ms(&m.latency, 0.5),
                p95_ms: quantile_ms(&m.latency, 0.95),
                p99_ms: quantile_ms(&m.latency, 0.99),
                ttft_p50_ms: quantile_ms(&m.ttft, 0.5),
                ttft_p95_ms: quantile_ms(&m.ttft, 0.95),
                cache_hits: m.cache_hits,
                cache_joined: m.cache_joined,
                sent_upstream: m.cacheable.saturating_sub(m.cache_hits + m.cache_joined),
            }
        })
        .collect();
    let labels: Vec<String> = rows.iter().map(|r| r.time.clone()).collect();
    let column = |f: fn(&Row) -> Option<f64>| rows.iter().map(f).collect::<Vec<_>>();
    let counts = |f: fn(&Row) -> u64| rows.iter().map(|r| Some(f(r) as f64).filter(|v| *v > 0.0)).collect::<Vec<_>>();
    let charts = Charts {
        latency: charts::lines(
            "Request latency",
            &labels,
            &[
                Series { name: "p50".into(), class: "o1", values: column(|r| r.p50_ms) },
                Series { name: "p95".into(), class: "o2", values: column(|r| r.p95_ms) },
                Series { name: "p99".into(), class: "o3", values: column(|r| r.p99_ms) },
            ],
            Unit::Millis,
        ),
        ttft: charts::lines(
            "Time to first token",
            &labels,
            &[
                Series { name: "p50".into(), class: "o1", values: column(|r| r.ttft_p50_ms) },
                Series { name: "p95".into(), class: "o2", values: column(|r| r.ttft_p95_ms) },
            ],
            Unit::Millis,
        ),
        outcomes: charts::stacked(
            "Requests by outcome",
            &labels,
            &[
                Series { name: "ok".into(), class: "s1", values: counts(|r| r.ok) },
                Series { name: "errors".into(), class: "critical", values: counts(|r| r.errors) },
                Series { name: "cancelled or refused".into(), class: "muted", values: counts(|r| r.other) },
            ],
            Unit::Count,
        ),
        cache: charts::stacked(
            "Cache results",
            &labels,
            &[
                Series { name: "cache hits".into(), class: "s1", values: counts(|r| r.cache_hits) },
                Series { name: "joined".into(), class: "s2", values: counts(|r| r.cache_joined) },
                Series { name: "sent upstream".into(), class: "s3", values: counts(|r| r.sent_upstream) },
            ],
            Unit::Count,
        ),
    };
    Overview { window, app: app.map(str::to_string), tiles: tiles(&total, window), rows, charts }
}

/// One app's line in the apps table.
#[derive(Debug, Clone, Serialize)]
pub struct AppRow {
    pub app: String,
    #[serde(flatten)]
    pub tiles: Tiles,
}

pub fn apps(metrics: &Metrics, window: Window, now_ms: i64) -> Vec<AppRow> {
    let from = now_ms - window.minutes() * MINUTE_MS;
    metrics
        .apps(from, now_ms + MINUTE_MS)
        .into_iter()
        .map(|app| {
            let mut total = Minute::default();
            for m in metrics.minutes(from, now_ms + MINUTE_MS, Some(&app)).values() {
                total.merge(m);
            }
            AppRow { tiles: tiles(&total, window), app }
        })
        .collect()
}

/// Formatting helpers for templates.
pub fn ms(value: Option<f64>) -> String {
    value.map_or_else(|| "–".into(), |v| Unit::Millis.format(v))
}

pub fn percent(value: Option<f64>) -> String {
    value.map_or_else(|| "–".into(), |v| format!("{:.0}%", v * 100.0))
}

pub fn count(value: u64) -> String {
    charts::compact(value as f64)
}
