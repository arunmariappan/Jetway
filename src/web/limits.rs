//! The limits and alerts pages (phase 9.6): upstream health and the circuit, the queue's depth and waits by class,
//! budget usage against each app's limits, refusals, and alerts.

use askama::Template;
use askama_web::WebTemplate;
use serde::Serialize;

use super::charts::{self, Series, Unit};
use super::pages::{ChartView, LegendItem, TileView, WindowLink, window_links};
use super::views::{self, Window};
use crate::alerts::Alert;
use crate::config::{Config, Priority};
use crate::limits::budget::{Period, UsageRow};
use crate::limits::health::UpstreamStatus;
use crate::limits::scheduler::Depth;
use crate::limits::{AppLimits, Refusal, thousands};
use crate::metrics::Minute;
use crate::server::admin::Admin;
use crate::server::identity::DEFAULT_APP;

const MINUTE_MS: i64 = 60_000;

/// Classes in their order, each with its step of the ordinal ramp: the darker, the sooner it is served.
const CLASSES: [(Priority, &str); 3] =
    [(Priority::Interactive, "o3"), (Priority::Normal, "o2"), (Priority::Batch, "o1")];

/// One budget against its limit.
#[derive(Debug, Clone, Serialize)]
pub struct BudgetView {
    pub app: String,
    /// `tokens today`, `GPU-seconds today`, `tokens this month`, ...
    pub budget: String,
    pub used: u64,
    pub limit: u64,
    pub share: f64,
    /// `ok`, `warn` (80% or more) or `critical` (used up).
    pub level: &'static str,
    pub resets: String,
    #[serde(skip)]
    pub used_text: String,
    #[serde(skip)]
    pub limit_text: String,
    /// The meter's fill, 0 to 100.
    #[serde(skip)]
    pub fill: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct AppLimitView {
    pub app: String,
    pub priority: &'static str,
    pub rate: String,
    pub output_cap: String,
    pub max_wait: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct RefusalView {
    pub app: String,
    pub rate_limit: u64,
    pub budget: u64,
    pub queue: u64,
    pub circuit_open: u64,
}

/// Every budget an app has, with what it has used this period.
pub fn budgets(config: &Config, usage: &[UsageRow], resets_ms: (i64, i64)) -> Vec<BudgetView> {
    let mut apps: Vec<String> = config.apps.keys().filter(|a| a.as_str() != DEFAULT_APP).cloned().collect();
    apps.extend(usage.iter().map(|r| r.app.clone()));
    apps.sort();
    apps.dedup();
    let used = |app: &str, period: Period| {
        usage.iter().find(|r| r.app == app && r.period == period).map(|r| r.usage).unwrap_or_default()
    };
    let (day_resets, month_resets) = (views::local_time(resets_ms.0), views::local_date_time_short(resets_ms.1));
    let mut rows = Vec::new();
    for app in apps {
        let limits = AppLimits::for_app(config, &app);
        let (day, month) = (used(&app, Period::Day), used(&app, Period::Month));
        let kinds = [
            (limits.requests_per_day, day.requests, "requests today", &day_resets),
            (limits.tokens_per_day, day.tokens, "tokens today", &day_resets),
            (limits.gpu_seconds_per_day, day.gpu_ms / 1000, "GPU-seconds today", &day_resets),
            (limits.tokens_per_month, month.tokens, "tokens this month", &month_resets),
        ];
        for (limit, used, budget, resets) in kinds {
            let Some(limit) = limit.filter(|l| *l > 0) else { continue };
            let share = used as f64 / limit as f64;
            let level = if share >= 1.0 {
                "critical"
            } else if share >= 0.8 {
                "warn"
            } else {
                "ok"
            };
            rows.push(BudgetView {
                app: app.clone(),
                budget: budget.into(),
                used,
                limit,
                share,
                level,
                resets: resets.clone(),
                used_text: thousands(used),
                limit_text: thousands(limit),
                fill: (share.min(1.0) * 100.0).round() as u32,
            });
        }
    }
    rows
}

/// The configured apps' priorities and limits; `default` covers apps that aren't listed.
pub fn app_limits(config: &Config) -> Vec<AppLimitView> {
    let row = |app: &str, label: String| {
        let l = AppLimits::for_app(config, app);
        let rate = match l.requests_per_minute {
            Some(rpm) => {
                let burst = l.burst.unwrap_or_else(|| (rpm / 10).max(1));
                format!("{} a minute, burst {}", thousands(u64::from(rpm)), thousands(u64::from(burst)))
            }
            None => "none".into(),
        };
        AppLimitView {
            app: label,
            priority: l.priority.as_str(),
            rate,
            output_cap: l
                .max_output_tokens
                .map_or_else(|| "none".into(), |c| format!("{} tokens", thousands(u64::from(c)))),
            max_wait: format!("{} s", config.queue.max_wait_seconds.get(l.priority)),
        }
    };
    let mut rows = vec![row(DEFAULT_APP, format!("{DEFAULT_APP} (and apps not listed)"))];
    rows.extend(config.apps.keys().filter(|n| n.as_str() != DEFAULT_APP).map(|n| row(n, n.clone())));
    rows
}

pub fn refusals(counts: Vec<(String, Refusal, u64)>) -> Vec<RefusalView> {
    let mut rows: Vec<RefusalView> = Vec::new();
    for (app, reason, n) in counts {
        let index = match rows.iter().position(|r| r.app == app) {
            Some(i) => i,
            None => {
                rows.push(RefusalView { app, rate_limit: 0, budget: 0, queue: 0, circuit_open: 0 });
                rows.len() - 1
            }
        };
        let row = &mut rows[index];
        match reason {
            Refusal::RateLimit => row.rate_limit += n,
            Refusal::Budget => row.budget += n,
            Refusal::Queue => row.queue += n,
            Refusal::CircuitOpen => row.circuit_open += n,
        }
    }
    rows.sort_by(|a, b| a.app.cmp(&b.app));
    rows
}

/// The most waiting per class in each of the last 60 minutes, from the queue's 10-second samples.
fn depth_chart(history: &[(i64, Depth)], now_ms: i64) -> ChartView {
    let current = now_ms - now_ms.rem_euclid(MINUTE_MS);
    let first = current - 59 * MINUTE_MS;
    let mut slots = vec![(0usize, 0usize, 0usize, false); 60];
    for (at, depth) in history {
        if *at < first {
            continue;
        }
        let slot = usize::try_from((at - first) / MINUTE_MS).unwrap_or(0).min(59);
        let s = &mut slots[slot];
        *s = (s.0.max(depth.interactive), s.1.max(depth.normal), s.2.max(depth.batch), true);
    }
    let labels: Vec<String> = (0..60).map(|i| views::local_time(first + i * MINUTE_MS)).collect();
    let value = |n: usize, seen: bool| (seen && n > 0).then_some(n as f64);
    let series = vec![
        Series { name: "interactive".into(), class: "o3", values: slots.iter().map(|s| value(s.0, s.3)).collect() },
        Series { name: "normal".into(), class: "o2", values: slots.iter().map(|s| value(s.1, s.3)).collect() },
        Series { name: "batch".into(), class: "o1", values: slots.iter().map(|s| value(s.2, s.3)).collect() },
    ];
    let rows = labels
        .iter()
        .zip(&slots)
        .rev()
        .filter(|(_, s)| s.3)
        .map(|(label, s)| vec![label.clone(), s.0.to_string(), s.1.to_string(), s.2.to_string()])
        .collect();
    ChartView {
        title: "Waiting for a slot".into(),
        subtitle: "the most requests waiting at once, per minute over the last hour".into(),
        svg: charts::stacked("Requests waiting for a slot", &labels, &series, Unit::Count),
        legend: legend_by_class("box"),
        head: vec!["Time".into(), "Interactive".into(), "Normal".into(), "Batch".into()],
        rows,
    }
}

fn legend_by_class(kind: &'static str) -> Vec<LegendItem> {
    CLASSES.iter().map(|(p, class)| LegendItem { name: p.as_str().into(), class, kind }).collect()
}

/// Queue wait p95 per class over the window: each app's minutes merged into its class.
fn wait_chart(admin: &Admin, window: Window, now_ms: i64) -> (ChartView, Minute) {
    let config = admin.gateway.config();
    let from = now_ms - window.minutes() * MINUTE_MS;
    let mut by_class: Vec<Vec<Minute>> = Vec::new();
    let mut labels = Vec::new();
    let mut total = Minute::default();
    for (priority, _) in CLASSES {
        let mut merged: Vec<Minute> = Vec::new();
        for app in admin.metrics.apps(from, now_ms + MINUTE_MS) {
            if AppLimits::for_app(config, &app).priority != priority {
                continue;
            }
            let buckets = views::buckets(&admin.metrics, window, Some(&app), now_ms);
            if labels.is_empty() {
                labels = buckets.iter().map(|b| b.label.clone()).collect();
            }
            if merged.is_empty() {
                merged = vec![Minute::default(); buckets.len()];
            }
            for (slot, bucket) in merged.iter_mut().zip(&buckets) {
                slot.merge(&bucket.minute);
                total.merge(&bucket.minute);
            }
        }
        by_class.push(merged);
    }
    if labels.is_empty() {
        labels = views::buckets(&admin.metrics, window, None, now_ms).iter().map(|b| b.label.clone()).collect();
    }
    let p95 = |m: &Minute| (!m.queue_wait.is_empty()).then(|| m.queue_wait.value_at_quantile(0.95) as f64 / 1000.0);
    let series: Vec<Series> = CLASSES
        .iter()
        .zip(&by_class)
        .map(|((priority, class), minutes)| Series {
            name: priority.as_str().into(),
            class,
            values: if minutes.is_empty() { vec![None; labels.len()] } else { minutes.iter().map(p95).collect() },
        })
        .collect();
    let rows = (0..labels.len())
        .rev()
        .filter(|i| series.iter().any(|s| s.values[*i].is_some()))
        .map(|i| {
            let mut row = vec![labels[i].clone()];
            row.extend(series.iter().map(|s| views::ms(s.values[i])));
            row
        })
        .collect();
    let slot = if window.bucket_minutes() == 1 { "minute" } else { "15 minutes" };
    let chart = ChartView {
        title: "Queue wait".into(),
        subtitle: format!("p95 per {slot} for requests that went upstream, by the app's priority"),
        svg: charts::lines("Queue wait p95 by class", &labels, &series, Unit::Millis),
        legend: legend_by_class(""),
        head: vec!["Time".into(), "Interactive".into(), "Normal".into(), "Batch".into()],
        rows,
    };
    (chart, total)
}

#[derive(Template, WebTemplate)]
#[template(path = "limits.html")]
pub struct LimitsPage {
    pub title: String,
    pub nav: &'static str,
    pub windows: Vec<WindowLink>,
    pub tiles: Vec<TileView>,
    pub charts: Vec<ChartView>,
    pub budgets: Vec<BudgetView>,
    pub apps: Vec<AppLimitView>,
    pub refusals: Vec<RefusalView>,
}

/// What the limits page and `/api/limits` show.
#[derive(Debug, Clone, Serialize)]
pub struct LimitsData {
    pub upstream: UpstreamStatus,
    pub queue: Depth,
    pub budgets: Vec<BudgetView>,
    pub apps: Vec<AppLimitView>,
    pub refusals: Vec<RefusalView>,
}

pub fn data(admin: &Admin) -> LimitsData {
    let gateway = &admin.gateway;
    let now = jiff::Timestamp::now();
    let budgets = budgets(gateway.config(), &gateway.budgets().rows(now), gateway.budgets().resets_ms(now));
    LimitsData {
        upstream: admin.upstream_status(),
        queue: gateway.scheduler().depth(),
        budgets,
        apps: app_limits(gateway.config()),
        refusals: refusals(admin.metrics.refusals()),
    }
}

pub fn limits_page(admin: &Admin, window: Window, now_ms: i64) -> LimitsPage {
    let data = data(admin);
    let (wait, total) = wait_chart(admin, window, now_ms);
    let upstream = &data.upstream;
    let q = data.queue;
    let tile = |label: &str, value: String, note: String| TileView { label: label.into(), value, note };
    let refused: u64 = data.refusals.iter().map(|r| r.rate_limit + r.budget + r.queue + r.circuit_open).sum();
    let tiles = vec![
        tile(
            "Upstream",
            if upstream.circuit_open {
                "down".into()
            } else if upstream.up {
                "up".into()
            } else {
                "not answering".into()
            },
            if upstream.circuit_open {
                "circuit open: requests fail fast".into()
            } else {
                upstream.version.clone().map_or_else(String::new, |v| format!("version {v}"))
            },
        ),
        tile("Slots in use", format!("{} of {}", q.in_use, q.slots), "requests running upstream".into()),
        tile(
            "Waiting",
            (q.interactive + q.normal + q.batch).to_string(),
            format!("{} interactive, {} normal, {} batch", q.interactive, q.normal, q.batch),
        ),
        tile(
            "Queue wait p95",
            views::ms((!total.queue_wait.is_empty()).then(|| total.queue_wait.value_at_quantile(0.95) as f64 / 1000.0)),
            format!("over the last {}", window.as_str()),
        ),
        tile("Model loads", views::count(total.model_loads), "calls that loaded their model first".into()),
        tile("Refused", views::count(refused), "by a limit since Jetway started".into()),
    ];
    LimitsPage {
        title: "Limits".into(),
        nav: "limits",
        windows: window_links("/limits", window, None),
        tiles,
        charts: vec![depth_chart(&admin.gateway.scheduler().history(), now_ms), wait],
        budgets: data.budgets,
        apps: data.apps,
        refusals: data.refusals,
    }
}

pub struct AlertView {
    pub severity: &'static str,
    pub badge: &'static str,
    pub message: String,
    pub since: String,
    pub resolved: String,
    pub notified: bool,
}

fn alert_view(alert: &Alert) -> AlertView {
    AlertView {
        severity: alert.severity.as_str(),
        badge: if alert.severity == crate::alerts::Severity::Critical { "error" } else { "warn" },
        message: alert.message.clone(),
        since: views::local_datetime(alert.since_ms),
        resolved: alert.resolved_ms.map(views::local_datetime).unwrap_or_default(),
        notified: alert.notified,
    }
}

#[derive(Template, WebTemplate)]
#[template(path = "alerts.html")]
pub struct AlertsPage {
    pub title: String,
    pub nav: &'static str,
    pub telegram: bool,
    pub enabled: bool,
    pub active: Vec<AlertView>,
    pub resolved: Vec<AlertView>,
}

pub fn alerts_page(admin: &Admin) -> AlertsPage {
    let config = &admin.gateway.config().alerts;
    AlertsPage {
        title: "Alerts".into(),
        nav: "alerts",
        telegram: config.telegram,
        enabled: config.enabled,
        active: admin.alerts.active().iter().map(alert_view).collect(),
        resolved: admin.alerts.resolved().iter().map(alert_view).collect(),
    }
}

/// The banner under the header while alerts are active.
#[derive(Template, WebTemplate)]
#[template(path = "banner.html")]
pub struct Banner {
    pub first: Option<AlertView>,
    pub more: usize,
}

pub fn banner(admin: &Admin) -> Banner {
    let active = admin.alerts.active();
    Banner { first: active.first().map(alert_view), more: active.len().saturating_sub(1) }
}
