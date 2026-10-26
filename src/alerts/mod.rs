//! Alerts (phase 9.5). Every few seconds the rules look at the last five minutes, the budgets, the upstream's health
//! and the request log's drop count, and say which conditions hold. A condition that starts holding fires an alert;
//! one that stops resolves it. Alerts are shown on the dashboard and, optionally, sent to Telegram: once when they
//! fire (again only after the cooldown, so a flapping condition doesn't flood the chat), when they escalate from
//! warning to critical, and when a sent one resolves.

pub mod telegram;

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::{Mutex, PoisonError};

use serde::Serialize;

use crate::config::Config;
use crate::limits::budget::{Period, UsageRow};
use crate::limits::health::UpstreamStatus;
use crate::limits::{AppLimits, thousands};
use crate::metrics::Minute;
use crate::proxy::record::Outcome;

/// Resolved alerts kept for the alerts page.
const HISTORY: usize = 100;
/// The window the error-rate and first-token rules look at, and how long a drop in the request log stays an alert.
pub const WINDOW_MS: i64 = 5 * 60_000;
/// The fewest first-token measurements for the p95 rule.
const MIN_TTFT_SAMPLES: u64 = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Warning,
    Critical,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Warning => "warning",
            Self::Critical => "critical",
        }
    }
}

/// A condition that holds now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Firing {
    /// Identifies the condition across evaluations, such as `budget:pp:tokens:day`.
    pub key: String,
    pub severity: Severity,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Alert {
    pub key: String,
    pub severity: Severity,
    pub message: String,
    pub since_ms: i64,
    pub resolved_ms: Option<i64>,
    /// Whether a notification went out for it.
    pub notified: bool,
}

/// What the rules look at.
pub struct Inputs<'a> {
    pub config: &'a Config,
    /// Each app's last five minutes, merged.
    pub recent: &'a [(String, Minute)],
    /// Current-period budget usage.
    pub budgets: &'a [UsageRow],
    pub upstream: &'a UpstreamStatus,
    pub upstream_name: &'a str,
    /// The request log's dropped-record count since start.
    pub log_dropped: u64,
}

#[derive(Default)]
struct State {
    active: BTreeMap<String, Alert>,
    history: VecDeque<Alert>,
    /// When each key last sent a notification.
    notified_at: HashMap<String, i64>,
    /// The drop count seen last, and when it last grew.
    dropped_seen: Option<u64>,
    dropped_grew_ms: Option<i64>,
}

pub struct Alerts {
    cooldown_ms: i64,
    state: Mutex<State>,
}

impl Alerts {
    pub fn new(cooldown_minutes: u64) -> Self {
        let cooldown_ms = i64::try_from(cooldown_minutes.saturating_mul(60_000)).unwrap_or(i64::MAX);
        Self { cooldown_ms, state: Mutex::new(State::default()) }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The conditions that hold now.
    pub fn evaluate(&self, inputs: &Inputs<'_>, now_ms: i64) -> Vec<Firing> {
        let mut firing = Vec::new();
        let rules = &inputs.config.alerts;

        if inputs.upstream.circuit_open || !inputs.upstream.up {
            let error = inputs.upstream.error.as_deref().unwrap_or("no answer");
            let state = if inputs.upstream.circuit_open { "down; requests fail fast" } else { "not answering" };
            firing.push(Firing {
                key: "upstream".into(),
                severity: if inputs.upstream.circuit_open { Severity::Critical } else { Severity::Warning },
                message: format!("{} is {state} ({error})", inputs.upstream_name),
            });
        }

        for row in inputs.budgets {
            let limits = AppLimits::for_app(inputs.config, &row.app);
            let checks: &[(Option<u64>, u64, &str)] = match row.period {
                Period::Day => &[
                    (limits.requests_per_day, row.usage.requests, "requests"),
                    (limits.tokens_per_day, row.usage.tokens, "tokens"),
                    (limits.gpu_seconds_per_day, row.usage.gpu_ms / 1000, "GPU-seconds"),
                ],
                Period::Month => &[(limits.tokens_per_month, row.usage.tokens, "tokens")],
            };
            let when = if row.period == Period::Day { "today" } else { "this month" };
            for &(limit, used, what) in checks {
                let Some(limit) = limit.filter(|l| *l > 0) else { continue };
                let share = used as f64 / limit as f64;
                if share < 0.8 {
                    continue;
                }
                let (severity, message) = if share >= 1.0 {
                    (
                        Severity::Critical,
                        format!(
                            "{} has used its budget: {} of {} {what} {when}",
                            row.app,
                            thousands(used),
                            thousands(limit)
                        ),
                    )
                } else {
                    (
                        Severity::Warning,
                        format!(
                            "{} has used {:.0}% of its budget: {} of {} {what} {when}",
                            row.app,
                            share * 100.0,
                            thousands(used),
                            thousands(limit)
                        ),
                    )
                };
                firing.push(Firing {
                    key: format!("budget:{}:{what}:{}", row.app, row.period.as_str()),
                    severity,
                    message,
                });
            }
        }

        for (app, minute) in inputs.recent {
            let failed = minute.count(Outcome::UpstreamError) + minute.count(Outcome::Timeout);
            let upstream_calls = minute.requests.saturating_sub(minute.count(Outcome::Rejected));
            if upstream_calls >= rules.min_requests && upstream_calls > 0 {
                let rate = failed as f64 / upstream_calls as f64;
                if rate > rules.error_rate {
                    firing.push(Firing {
                        key: format!("errors:{app}"),
                        severity: Severity::Warning,
                        message: format!(
                            "{app}: {:.0}% of {upstream_calls} requests failed in the last 5 minutes",
                            rate * 100.0
                        ),
                    });
                }
            }
            if minute.ttft.len() >= MIN_TTFT_SAMPLES {
                let p95_ms = minute.ttft.value_at_quantile(0.95) / 1000;
                if p95_ms > rules.ttft_p95_ms {
                    firing.push(Firing {
                        key: format!("ttft:{app}"),
                        severity: Severity::Warning,
                        message: format!(
                            "{app}: the first token takes {:.1} s at p95 over the last 5 minutes (alert above {:.1} s)",
                            p95_ms as f64 / 1000.0,
                            rules.ttft_p95_ms as f64 / 1000.0
                        ),
                    });
                }
            }
        }

        let mut state = self.lock();
        if let Some(seen) = state.dropped_seen
            && inputs.log_dropped > seen
        {
            state.dropped_grew_ms = Some(now_ms);
        }
        state.dropped_seen = Some(inputs.log_dropped);
        if state.dropped_grew_ms.is_some_and(|at| now_ms - at < WINDOW_MS) {
            firing.push(Firing {
                key: "log_dropped".into(),
                severity: Severity::Warning,
                message: format!(
                    "the request log is dropping records ({} since start): its writer can't keep up",
                    thousands(inputs.log_dropped)
                ),
            });
        }
        firing
    }

    /// Fires, updates and resolves alerts from one evaluation. Returns the notifications to send.
    pub fn apply(&self, firing: Vec<Firing>, now_ms: i64) -> Vec<String> {
        let mut notices = Vec::new();
        let mut state = self.lock();
        let holding: Vec<String> = firing.iter().map(|f| f.key.clone()).collect();
        for f in firing {
            let cooled = state.notified_at.get(&f.key).is_none_or(|at| now_ms - at >= self.cooldown_ms);
            match state.active.get_mut(&f.key) {
                Some(alert) => {
                    let escalated = f.severity > alert.severity;
                    alert.severity = alert.severity.max(f.severity);
                    alert.message = f.message;
                    if escalated {
                        alert.notified = true;
                        notices.push(format!("[{}] {}", alert.severity.as_str(), alert.message));
                        state.notified_at.insert(f.key, now_ms);
                    }
                }
                None => {
                    if cooled {
                        notices.push(format!("[{}] {}", f.severity.as_str(), f.message));
                        state.notified_at.insert(f.key.clone(), now_ms);
                    }
                    let alert = Alert {
                        key: f.key.clone(),
                        severity: f.severity,
                        message: f.message,
                        since_ms: now_ms,
                        resolved_ms: None,
                        notified: cooled,
                    };
                    state.active.insert(f.key, alert);
                }
            }
        }
        let gone: Vec<String> = state.active.keys().filter(|k| !holding.contains(k)).cloned().collect();
        for key in gone {
            if let Some(mut alert) = state.active.remove(&key) {
                alert.resolved_ms = Some(now_ms);
                if alert.notified {
                    notices.push(format!("[resolved] {}", alert.message));
                }
                if state.history.len() == HISTORY {
                    state.history.pop_back();
                }
                state.history.push_front(alert);
            }
        }
        notices
    }

    /// Alerts that hold now, critical first.
    pub fn active(&self) -> Vec<Alert> {
        let mut active: Vec<Alert> = self.lock().active.values().cloned().collect();
        active.sort_by(|a, b| b.severity.cmp(&a.severity).then(a.since_ms.cmp(&b.since_ms)));
        active
    }

    /// Resolved alerts, newest first.
    pub fn resolved(&self) -> Vec<Alert> {
        self.lock().history.iter().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppConfig;
    use crate::limits::budget::Usage;

    fn fire(key: &str, severity: Severity) -> Firing {
        Firing { key: key.into(), severity, message: format!("{key} {}", severity.as_str()) }
    }

    #[test]
    fn deduplicated_escalated_resolved_and_cooled_down() {
        let alerts = Alerts::new(15);
        let minute = 60_000;
        assert_eq!(alerts.apply(vec![fire("a", Severity::Warning)], 0), ["[warning] a warning"]);
        // Still holding: no new notification.
        assert!(alerts.apply(vec![fire("a", Severity::Warning)], minute).is_empty());
        // Escalating is news, whatever the cooldown.
        assert_eq!(alerts.apply(vec![fire("a", Severity::Critical)], 2 * minute), ["[critical] a critical"]);
        assert_eq!(alerts.active().len(), 1);
        // Resolving a notified alert is news too.
        assert_eq!(alerts.apply(vec![], 3 * minute), ["[resolved] a critical"]);
        assert!(alerts.active().is_empty());
        assert_eq!(alerts.resolved()[0].resolved_ms, Some(3 * minute));
        // Firing again within the cooldown shows the alert but sends nothing, and so does its resolution.
        assert!(alerts.apply(vec![fire("a", Severity::Warning)], 4 * minute).is_empty());
        assert!(!alerts.active()[0].notified);
        assert!(alerts.apply(vec![], 5 * minute).is_empty());
        // After the cooldown it notifies again.
        assert_eq!(alerts.apply(vec![fire("a", Severity::Warning)], 18 * minute), ["[warning] a warning"]);
    }

    #[test]
    fn rules() {
        let mut config = Config::default();
        config.apps.insert("pp".into(), AppConfig { tokens_per_day: Some(1000), ..AppConfig::default() });
        let budgets = [
            UsageRow {
                app: "pp".into(),
                period: Period::Day,
                key: "2026-10-26".into(),
                usage: Usage { requests: 3, tokens: 850, gpu_ms: 0 },
            },
            UsageRow {
                app: "free".into(),
                period: Period::Day,
                key: "2026-10-26".into(),
                usage: Usage { requests: 3, tokens: 9_999, gpu_ms: 0 },
            },
        ];
        let mut minute = Minute { requests: 40, ..Minute::default() };
        minute.outcomes[1] = 3; // upstream errors: 3 of 40 is 7.5%
        for _ in 0..10 {
            minute.ttft.saturating_record(20_000_000);
        }
        let recent = [("pp".to_string(), minute)];
        let upstream = UpstreamStatus { up: true, circuit_open: false, version: None, error: None, checked_ms: None };
        let alerts = Alerts::new(15);
        let mut inputs = Inputs {
            config: &config,
            recent: &recent,
            budgets: &budgets,
            upstream: &upstream,
            upstream_name: "ollama",
            log_dropped: 0,
        };
        let keys = |firing: Vec<Firing>| firing.into_iter().map(|f| f.key).collect::<Vec<_>>();
        assert_eq!(keys(alerts.evaluate(&inputs, 0)), ["budget:pp:tokens:day", "errors:pp", "ttft:pp"]);
        let firing = alerts.evaluate(&inputs, 0);
        assert_eq!(firing[0].message, "pp has used 85% of its budget: 850 of 1,000 tokens today");

        let down = UpstreamStatus {
            up: false,
            circuit_open: true,
            version: None,
            error: Some("refused".into()),
            checked_ms: None,
        };
        inputs.upstream = &down;
        inputs.recent = &[];
        inputs.budgets = &[];
        inputs.log_dropped = 4;
        let firing = alerts.evaluate(&inputs, 1_000);
        assert_eq!(firing.len(), 2, "{firing:?}");
        assert_eq!(
            (firing[0].severity, firing[0].message.as_str()),
            (Severity::Critical, "ollama is down; requests fail fast (refused)")
        );
        assert_eq!(firing[1].key, "log_dropped");
        // Drops that stopped growing five minutes ago no longer alert.
        inputs.upstream = &upstream;
        assert!(alerts.evaluate(&inputs, 1_000 + WINDOW_MS).is_empty());
    }
}
