//! Dashboard pages and fragments (phase 4.2): askama templates filled from the rollups, the store and the gateway.

use std::fmt::Write as _;
use std::sync::Arc;

use askama::Template;
use askama_web::WebTemplate;

use super::charts::compact;
use super::views::{self, Overview, Row, Window};
use crate::server::admin::Admin;
use crate::store::{Bodies, CacheRow, CacheStats, ExchangeRow};

#[derive(Clone)]
pub struct TileView {
    pub label: String,
    pub value: String,
    pub note: String,
}

fn tile(label: &str, value: String, note: String) -> TileView {
    TileView { label: label.into(), value, note }
}

pub struct LegendItem {
    pub name: String,
    pub class: &'static str,
    /// `""` for a line key, `"box"` for a bar key.
    pub kind: &'static str,
}

pub struct ChartView {
    pub title: String,
    pub subtitle: String,
    pub svg: String,
    pub legend: Vec<LegendItem>,
    pub head: Vec<String>,
    pub rows: Vec<Vec<String>>,
}

pub struct WindowLink {
    pub label: &'static str,
    pub href: String,
    pub current: bool,
}

pub struct AppOption {
    pub name: String,
    pub selected: bool,
}

pub fn window_links(base: &str, current: Window, app: Option<&str>) -> Vec<WindowLink> {
    Window::ALL
        .into_iter()
        .map(|w| {
            let mut href = format!("{base}?window={}", w.as_str());
            if let Some(app) = app {
                let _ = write!(href, "&app={app}");
            }
            WindowLink { label: w.as_str(), href, current: w == current }
        })
        .collect()
}

#[derive(Template, WebTemplate)]
#[template(path = "overview.html")]
pub struct OverviewPage {
    pub title: String,
    pub nav: &'static str,
    pub heading: String,
    pub windows: Vec<WindowLink>,
    pub window: &'static str,
    pub app: String,
    pub apps: Vec<AppOption>,
    pub fragment: String,
    pub body: String,
}

#[derive(Template)]
#[template(path = "overview_body.html")]
pub struct OverviewBody {
    pub tiles: Vec<TileView>,
    pub charts: Vec<ChartView>,
}

fn legend(items: &[(&str, &'static str)], kind: &'static str) -> Vec<LegendItem> {
    items.iter().map(|(name, class)| LegendItem { name: (*name).into(), class, kind }).collect()
}

fn slot_name(window: Window) -> &'static str {
    if window.bucket_minutes() == 1 { "minute" } else { "15 minutes" }
}

/// The tiles and charts of an overview.
pub fn overview_body(admin: &Admin, overview: &Overview) -> OverviewBody {
    let t = &overview.tiles;
    let gauges = admin.gauges();
    let tiles = vec![
        tile("Requests", views::count(t.requests), format!("{:.1} per minute", t.requests_per_minute)),
        tile("Latency p50", views::ms(t.p50_ms), format!("p95 {}", views::ms(t.p95_ms))),
        tile("First token p50", views::ms(t.ttft_p50_ms), "time to the first answer token".into()),
        tile(
            "Output tokens per second",
            t.tokens_per_second.map_or_else(|| "–".into(), |v| format!("{v:.0}")),
            "median of each answer's rate".into(),
        ),
        tile(
            "Cache hit rate",
            views::percent(t.cache_hit_rate),
            format!("{} tokens, {:.0} s of GPU saved", views::count(t.saved_tokens), t.saved_gpu_seconds),
        ),
        tile("Errors", views::percent(t.error_rate), "upstream errors and timeouts".into()),
        tile(
            "In progress",
            gauges.in_flight.to_string(),
            format!("Jetway uses {:.0} MB", gauges.memory_bytes as f64 / 1_048_576.0),
        ),
    ];
    let slot = slot_name(overview.window);
    let ms = views::ms;
    let rows = &overview.rows;
    let table = |f: &dyn Fn(&Row) -> Vec<String>| rows.iter().rev().map(f).collect::<Vec<_>>();
    let charts = vec![
        ChartView {
            title: "Request latency".into(),
            subtitle: format!("p50, p95 and p99 per {slot}, from receipt to the last byte"),
            svg: overview.charts.latency.clone(),
            legend: legend(&[("p50", "o1"), ("p95", "o2"), ("p99", "o3")], ""),
            head: vec!["Time".into(), "p50".into(), "p95".into(), "p99".into(), "Requests".into()],
            rows: table(&|r| vec![r.time.clone(), ms(r.p50_ms), ms(r.p95_ms), ms(r.p99_ms), r.requests.to_string()]),
        },
        ChartView {
            title: "Time to first token".into(),
            subtitle: format!("p50 and p95 per {slot}, as the client sees it"),
            svg: overview.charts.ttft.clone(),
            legend: legend(&[("p50", "o1"), ("p95", "o2")], ""),
            head: vec!["Time".into(), "p50".into(), "p95".into()],
            rows: table(&|r| vec![r.time.clone(), ms(r.ttft_p50_ms), ms(r.ttft_p95_ms)]),
        },
        ChartView {
            title: "Requests by outcome".into(),
            subtitle: format!("per {slot}"),
            svg: overview.charts.outcomes.clone(),
            legend: legend(&[("ok", "s1"), ("errors", "critical"), ("cancelled or refused", "muted")], "box"),
            head: vec!["Time".into(), "ok".into(), "Errors".into(), "Cancelled or refused".into()],
            rows: table(&|r| vec![r.time.clone(), r.ok.to_string(), r.errors.to_string(), r.other.to_string()]),
        },
        ChartView {
            title: "Cache results".into(),
            subtitle: format!("generation and embedding requests per {slot}"),
            svg: overview.charts.cache.clone(),
            legend: legend(&[("cache hits", "s1"), ("joined", "s2"), ("sent upstream", "s3")], "box"),
            head: vec!["Time".into(), "Hits".into(), "Joined".into(), "Sent upstream".into()],
            rows: table(&|r| {
                vec![r.time.clone(), r.cache_hits.to_string(), r.cache_joined.to_string(), r.sent_upstream.to_string()]
            }),
        },
    ];
    OverviewBody { tiles, charts }
}

pub fn overview_page(admin: &Admin, overview: &Overview, body: String, now_ms: i64) -> OverviewPage {
    let app = overview.app.clone().unwrap_or_default();
    let from = now_ms - Window::Day.minutes() * 60_000;
    let apps = admin
        .metrics
        .apps(from, now_ms + 60_000)
        .into_iter()
        .map(|name| AppOption { selected: name == app, name })
        .collect();
    let mut fragment = format!("/fragments/overview?window={}", overview.window.as_str());
    if !app.is_empty() {
        let _ = write!(fragment, "&app={app}");
    }
    OverviewPage {
        title: if app.is_empty() { "Overview".into() } else { app.clone() },
        nav: if app.is_empty() { "overview" } else { "apps" },
        heading: if app.is_empty() { "Overview".into() } else { format!("App {app}") },
        windows: window_links("/", overview.window, overview.app.as_deref()),
        window: overview.window.as_str(),
        app,
        apps,
        fragment,
        body,
    }
}

/// One exchange formatted for a table row or a page header.
#[derive(Clone)]
pub struct RowView {
    pub id: String,
    pub time: String,
    pub app: String,
    pub method: String,
    pub path: String,
    pub status: u16,
    pub outcome: String,
    pub outcome_class: &'static str,
    pub cache: String,
    pub first_token: String,
    pub total: String,
    pub tokens: String,
}

fn us_ms(us: Option<i64>) -> String {
    views::ms(us.map(|us| us as f64 / 1000.0))
}

fn opt<T: ToString>(value: Option<T>) -> String {
    value.map_or_else(|| "–".into(), |v| v.to_string())
}

/// The cache note's fields, from its JSON.
fn cache_fields(row: &ExchangeRow) -> Vec<(String, String)> {
    let Some(value) = row.cache.as_deref().and_then(|c| serde_json::from_str::<serde_json::Value>(c).ok()) else {
        return Vec::new();
    };
    let mut fields = Vec::new();
    let text = |key: &str| {
        value.get(key).filter(|v| !v.is_null()).map(|v| v.as_str().map_or_else(|| v.to_string(), str::to_string))
    };
    for (label, key) in [
        ("Result", "result"),
        ("Why", "reason"),
        ("Source", "source"),
        ("Age (s)", "age_s"),
        ("Saved prompt tokens", "saved_prompt_tokens"),
        ("Saved output tokens", "saved_completion_tokens"),
        ("Saved GPU time (ms)", "saved_gpu_ms"),
    ] {
        if let Some(v) = text(key) {
            fields.push((label.to_string(), v));
        }
    }
    fields
}

impl RowView {
    pub fn from_row(row: &ExchangeRow) -> Self {
        let outcome_class = match row.outcome.as_str() {
            "ok" if row.status < 400 => "ok",
            "ok" | "rejected" => "warn",
            "upstream_error" | "timeout" => "error",
            _ => "",
        };
        let cache = row
            .cache
            .as_deref()
            .and_then(|c| serde_json::from_str::<serde_json::Value>(c).ok())
            .and_then(|v| v.get("result").and_then(|r| r.as_str()).map(str::to_string))
            .unwrap_or_default();
        Self {
            id: row.id.clone(),
            time: views::local_datetime(row.started_at_ms),
            app: row.app.clone(),
            method: row.method.clone(),
            path: row.path.clone(),
            status: row.status,
            outcome: row.outcome.replace('_', " "),
            outcome_class,
            cache,
            first_token: us_ms(row.first_token_us),
            total: us_ms(row.done_us),
            tokens: format!("{} / {}", opt(row.prompt_tokens), opt(row.completion_tokens)),
        }
    }
}

#[derive(Template)]
#[template(path = "row.html")]
pub struct RowTemplate<'a> {
    pub r: &'a RowView,
}

pub fn row_html(row: &ExchangeRow) -> String {
    RowTemplate { r: &RowView::from_row(row) }.render().unwrap_or_default().trim().to_string()
}

#[derive(Template, WebTemplate)]
#[template(path = "live.html")]
pub struct LivePage {
    pub title: String,
    pub nav: &'static str,
    pub app: String,
    pub events: String,
    pub inflight: String,
    pub rows: Vec<String>,
}

pub struct InflightView {
    pub time: String,
    pub app: String,
    pub method: String,
    pub path: String,
    pub elapsed: String,
}

#[derive(Template)]
#[template(path = "inflight.html")]
pub struct InflightFragment {
    pub entries: Vec<InflightView>,
}

pub fn inflight(admin: &Admin) -> InflightFragment {
    let entries = admin
        .gateway
        .inflight_list()
        .into_iter()
        .map(|e| InflightView {
            time: views::local_datetime(e.started_at_ms),
            elapsed: views::ms(Some(e.started.elapsed().as_secs_f64() * 1000.0)),
            app: e.app,
            method: e.method,
            path: e.path,
        })
        .collect();
    InflightFragment { entries }
}

#[derive(Template, WebTemplate)]
#[template(path = "status.html")]
pub struct StatusFragment {
    pub up: bool,
    pub circuit_open: bool,
    pub upstream: String,
    pub version: String,
    pub in_flight: usize,
}

#[derive(Template, WebTemplate)]
#[template(path = "exchange.html")]
pub struct ExchangePage {
    pub title: String,
    pub nav: &'static str,
    pub id: String,
    pub r: RowView,
    pub fields: Vec<(String, String)>,
    pub timings: Vec<(String, String)>,
    pub timeline: String,
    pub cache: Vec<(String, String)>,
    pub request: String,
    pub request_note: String,
    pub request_open: bool,
    pub response: String,
    pub response_note: String,
}

const SHOW_BYTES: usize = 64 * 1024;

fn body_text(bytes: &[u8], pretty_json: bool) -> (String, String) {
    if bytes.is_empty() {
        return (String::new(), "empty".into());
    }
    let note = format!("{} bytes", bytes.len());
    if pretty_json
        && bytes.len() <= SHOW_BYTES
        && let Ok(value) = serde_json::from_slice::<serde_json::Value>(bytes)
    {
        return (serde_json::to_string_pretty(&value).unwrap_or_default(), note);
    }
    let shown = &bytes[..bytes.len().min(SHOW_BYTES)];
    let mut text = String::from_utf8_lossy(shown).into_owned();
    if bytes.len() > SHOW_BYTES {
        text.push_str("\n… (the rest isn't shown)");
    }
    (text, note)
}

/// A waterfall of the exchange's phases, one row each, on a shared time axis.
fn timeline(row: &ExchangeRow) -> String {
    let done = row.done_us.unwrap_or(0).max(1) as f64;
    let mut phases: Vec<(&str, i64, i64)> = Vec::new();
    let sent = row.upstream_sent_us;
    if let Some(sent) = sent {
        let waited = row.queue_wait_us.unwrap_or(0).clamp(0, sent);
        if waited > 0 {
            phases.push(("Waiting for an upstream slot", 0, waited));
        }
        phases.push(("Jetway, before the upstream", waited, sent));
        let answered = row.headers_us.unwrap_or(sent);
        phases.push(("Waiting for the upstream", sent, answered));
        if let Some(first) = row.first_token_us {
            phases.push(("Until the first token", answered, first.max(answered)));
            phases.push(("The rest of the answer", first.max(answered), row.done_us.unwrap_or(first)));
        } else {
            phases.push(("The response", answered, row.done_us.unwrap_or(answered)));
        }
    } else {
        phases.push(("Answered by Jetway", 0, row.done_us.unwrap_or(0)));
    }
    let (label_width, bar_left, width, row_height) = (190.0, 196.0, 640.0, 24.0);
    let bar_width = width - bar_left - 70.0;
    let height = row_height * phases.len() as f64 + 4.0;
    let mut svg =
        format!(r#"<svg class="chart" viewBox="0 0 {width} {height}" role="img" aria-label="Exchange timing">"#);
    for (i, (label, start, end)) in phases.iter().enumerate() {
        let y = 4.0 + i as f64 * row_height;
        let x = bar_left + bar_width * (*start as f64 / done).clamp(0.0, 1.0);
        let w = (bar_width * ((*end - *start) as f64 / done)).max(2.0);
        let duration = views::ms(Some((*end - *start) as f64 / 1000.0));
        let _ = write!(
            svg,
            r#"<text class="end" x="{}" y="{:.1}" text-anchor="end">{label}</text><rect class="bar s1" x="{x:.1}" y="{y:.1}" width="{w:.1}" height="12" rx="3"/><text class="tick" x="{:.1}" y="{:.1}">{duration}</text>"#,
            label_width,
            y + 10.0,
            x + w + 6.0,
            y + 10.0
        );
    }
    svg.push_str("</svg>");
    svg
}

pub fn exchange_page(row: &ExchangeRow, bodies: Option<&Bodies>) -> ExchangePage {
    let r = RowView::from_row(row);
    let yes = |b: bool| if b { "yes" } else { "no" }.to_string();
    let mut fields = vec![
        ("App".to_string(), row.app.clone()),
        ("Protocol".to_string(), row.protocol.clone()),
        ("Kind".to_string(), row.class.replace('_', " ")),
        ("Model".to_string(), row.model.clone().unwrap_or_else(|| "–".into())),
        ("Stream".to_string(), row.stream.map_or_else(|| "–".into(), yes)),
        ("Think".to_string(), row.think.clone().unwrap_or_else(|| "–".into())),
        ("Temperature".to_string(), opt(row.temperature)),
        ("Seed".to_string(), opt(row.seed)),
        ("Messages".to_string(), opt(row.messages)),
        (
            "Tools, format, images".to_string(),
            format!("{}, {}, {}", yes(row.has_tools), yes(row.has_format), yes(row.has_images)),
        ),
        ("Tokens in / out".to_string(), r.tokens.clone()),
        ("Cached prompt tokens".to_string(), opt(row.cached_prompt_tokens)),
        ("Done reason".to_string(), row.done_reason.clone().unwrap_or_else(|| "–".into())),
        (
            "Bytes in / out".to_string(),
            format!("{} / {}", compact(row.request_bytes as f64), compact(row.response_bytes as f64)),
        ),
    ];
    if let Some(priority) = &row.priority {
        fields.push(("Priority".to_string(), priority.clone()));
    }
    if let Some(refusal) = &row.refusal {
        fields.push(("Refused by".to_string(), refusal.replace('_', " ")));
    }
    if let Some(cap) = row.output_cap {
        fields.push(("Output cap added".to_string(), format!("{cap} tokens")));
    }
    if let Some(error) = &row.error {
        fields.push(("Error".to_string(), error.clone()));
    }
    if let Some(trace) = &row.trace_id {
        fields.push(("Trace".to_string(), trace.clone()));
    }
    if row.truncated || row.incomplete {
        fields.push(("Recording".to_string(), if row.truncated { "truncated" } else { "incomplete" }.into()));
    }
    let ns_ms = |ns: Option<i64>| views::ms(ns.map(|ns| ns as f64 / 1e6));
    let timings = vec![
        ("Waited for a slot".to_string(), us_ms(row.queue_wait_us)),
        ("Sent upstream".to_string(), us_ms(row.upstream_sent_us)),
        ("Headers".to_string(), us_ms(row.headers_us)),
        ("First byte".to_string(), us_ms(row.first_chunk_us)),
        ("First thinking token".to_string(), us_ms(row.first_thinking_us)),
        ("First answer token".to_string(), us_ms(row.first_token_us)),
        ("Done".to_string(), us_ms(row.done_us)),
        (
            "Ollama: load, prompt, generation".to_string(),
            format!("{}, {}, {}", ns_ms(row.load_ns), ns_ms(row.prompt_eval_ns), ns_ms(row.eval_ns)),
        ),
    ];
    let (request, request_note) =
        bodies.map_or_else(|| (String::new(), "not stored".into()), |b| body_text(&b.request, true));
    let (response, response_note) = bodies.map_or_else(
        || (String::new(), "not stored".into()),
        |b| {
            if b.response.is_empty() {
                (String::new(), "not stored again (see the source)".into())
            } else {
                body_text(&b.response, true)
            }
        },
    );
    ExchangePage {
        title: format!("{} {}", row.method, row.path),
        nav: "live",
        id: row.id.clone(),
        fields,
        timings,
        timeline: timeline(row),
        cache: cache_fields(row),
        request_open: request.len() < 4000,
        request,
        request_note,
        response,
        response_note,
        r,
    }
}

#[derive(Template, WebTemplate)]
#[template(path = "apps.html")]
pub struct AppsPage {
    pub title: String,
    pub nav: &'static str,
    pub windows: Vec<WindowLink>,
    pub window: &'static str,
    pub rows: Vec<Vec<String>>,
}

pub fn apps_page(admin: &Admin, window: Window, now_ms: i64) -> AppsPage {
    let rows = views::apps(&admin.metrics, window, now_ms)
        .into_iter()
        .map(|a| {
            let t = &a.tiles;
            vec![
                a.app.clone(),
                views::count(t.requests),
                format!("{:.1}", t.requests_per_minute),
                views::ms(t.p50_ms),
                views::ms(t.p95_ms),
                views::ms(t.ttft_p50_ms),
                views::percent(t.error_rate),
                views::percent(t.cache_hit_rate),
                format!("{} / {}", views::count(t.prompt_tokens), views::count(t.completion_tokens)),
                views::count(t.saved_tokens),
            ]
        })
        .collect();
    AppsPage {
        title: "Apps".into(),
        nav: "apps",
        windows: window_links("/apps", window, None),
        window: window.as_str(),
        rows,
    }
}

pub struct StatsView {
    pub app: String,
    pub entries: i64,
    pub bytes: String,
    pub hits: i64,
}

pub struct EntryView {
    pub app: String,
    pub path: String,
    pub form: &'static str,
    pub hits: i64,
    pub size: String,
    pub preview: String,
    pub source: String,
}

#[derive(Template, WebTemplate)]
#[template(path = "cache.html")]
pub struct CachePage {
    pub title: String,
    pub nav: &'static str,
    pub tiles: Vec<TileView>,
    pub stats: Vec<StatsView>,
    pub top: Vec<EntryView>,
}

pub fn cache_page(admin: &Arc<Admin>, stats: Vec<CacheStats>, top: Vec<CacheRow>) -> CachePage {
    let gauges = admin.gauges();
    let stored: i64 = stats.iter().map(|s| s.entries).sum();
    let hits: i64 = stats.iter().map(|s| s.hits).sum();
    let tiles = vec![
        tile(
            "In memory",
            views::count(gauges.cache_entries),
            format!("{:.1} MB", gauges.cache_bytes as f64 / 1_048_576.0),
        ),
        tile("Stored", views::count(stored.max(0) as u64), "entries that haven't expired".into()),
        tile("Hits on stored entries", views::count(hits.max(0) as u64), String::new()),
        tile("Joinable calls in flight", gauges.flights.to_string(), String::new()),
    ];
    let top = top
        .into_iter()
        .map(|e| EntryView {
            app: e.app,
            path: e.path,
            form: if e.stream { "stream" } else { "whole" },
            hits: e.hits,
            size: compact(e.size as f64),
            preview: e.preview,
            source: e.source,
        })
        .collect();
    let stats = stats
        .into_iter()
        .map(|s| StatsView { bytes: compact(s.bytes as f64), app: s.app, entries: s.entries, hits: s.hits })
        .collect();
    CachePage { title: "Cache".into(), nav: "cache", tiles, stats, top }
}
