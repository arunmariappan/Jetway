//! Per-app budgets over a day and a month (plan D13, phase 9.2): requests, tokens and GPU-seconds of upstream calls.
//! Cache hits and joined requests use none. Usage is kept in memory, written to SQLite every few seconds and at
//! shutdown, and restored at startup. A request that starts with budget left may overshoot it, since an answer's
//! length isn't known in advance.

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use jiff::tz::TimeZone;
use jiff::{Timestamp, ToSpan, Zoned};
use serde::Serialize;

use super::{AppLimits, thousands};
use crate::cache::CacheResult;
use crate::proxy::record::{Finished, Outcome, Sink};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Period {
    Day,
    Month,
}

impl Period {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Day => "day",
            Self::Month => "month",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "day" => Some(Self::Day),
            "month" => Some(Self::Month),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Usage {
    pub requests: u64,
    pub tokens: u64,
    pub gpu_ms: u64,
}

/// One app's usage in one period, as stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UsageRow {
    pub app: String,
    pub period: Period,
    /// `2026-10-26` for a day, `2026-10` for a month, in the budget time zone.
    pub key: String,
    #[serde(flatten)]
    pub usage: Usage,
}

/// Why a request was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Exceeded {
    pub message: String,
    pub retry_after: Duration,
}

pub struct Budgets {
    tz: TimeZone,
    usage: Mutex<HashMap<(String, Period, String), Usage>>,
}

/// The current period keys and when they end.
struct Periods {
    day: String,
    month: String,
    day_ends: Zoned,
    month_ends: Zoned,
}

impl Budgets {
    /// `time_zone` is `system` or an IANA name (validated with the configuration).
    pub fn new(time_zone: &str) -> Self {
        let tz =
            if time_zone == "system" { TimeZone::system() } else { TimeZone::get(time_zone).unwrap_or(TimeZone::UTC) };
        Self { tz, usage: Mutex::new(HashMap::new()) }
    }

    fn periods(&self, now: Timestamp) -> Periods {
        let zoned = now.to_zoned(self.tz.clone());
        let day_start = zoned.start_of_day().unwrap_or_else(|_| zoned.clone());
        let month_start = zoned.first_of_month().and_then(|m| m.start_of_day()).unwrap_or_else(|_| day_start.clone());
        Periods {
            day: zoned.strftime("%Y-%m-%d").to_string(),
            month: zoned.strftime("%Y-%m").to_string(),
            day_ends: day_start.checked_add(1.day()).unwrap_or_else(|_| zoned.clone()),
            month_ends: month_start.checked_add(1.month()).unwrap_or_else(|_| zoned.clone()),
        }
    }

    fn get(&self, app: &str, period: Period, key: &str) -> Usage {
        let usage = self.usage.lock().unwrap_or_else(PoisonError::into_inner);
        usage.get(&(app.to_string(), period, key.to_string())).copied().unwrap_or_default()
    }

    /// `Err` when any of the app's budgets is used up.
    pub fn check(&self, app: &str, limits: &AppLimits, now: Timestamp) -> Result<(), Exceeded> {
        let p = self.periods(now);
        let day = self.get(app, Period::Day, &p.day);
        let month = self.get(app, Period::Month, &p.month);
        let checks = [
            (limits.requests_per_day, day.requests, "requests", "today", &p.day_ends),
            (limits.tokens_per_day, day.tokens, "tokens", "today", &p.day_ends),
            (limits.gpu_seconds_per_day, day.gpu_ms / 1000, "GPU-seconds", "today", &p.day_ends),
            (limits.tokens_per_month, month.tokens, "tokens", "this month", &p.month_ends),
        ];
        for (limit, used, what, when, ends) in checks {
            if let Some(limit) = limit
                && used >= limit
            {
                let retry = Duration::from_secs(
                    u64::try_from(ends.timestamp().as_second() - now.as_second()).unwrap_or(0).max(1),
                );
                let resets = if when == "today" {
                    ends.strftime("%H:%M").to_string()
                } else {
                    ends.strftime("%Y-%m-%d %H:%M").to_string()
                };
                return Err(Exceeded {
                    message: format!(
                        "budget exceeded: {app} used {} of {} {what} {when}; resets at {resets}",
                        thousands(used),
                        thousands(limit)
                    ),
                    retry_after: retry,
                });
            }
        }
        Ok(())
    }

    /// Adds an upstream call's usage to its app's current day and month.
    pub fn add(&self, app: &str, usage: Usage, at: Timestamp) {
        let p = self.periods(at);
        let mut map = self.usage.lock().unwrap_or_else(PoisonError::into_inner);
        for (period, key) in [(Period::Day, p.day), (Period::Month, p.month)] {
            let entry = map.entry((app.to_string(), period, key)).or_default();
            entry.requests += usage.requests;
            entry.tokens += usage.tokens;
            entry.gpu_ms += usage.gpu_ms;
        }
    }

    /// Current-period rows for every app (the store keeps them; the dashboard shows them).
    pub fn rows(&self, now: Timestamp) -> Vec<UsageRow> {
        let p = self.periods(now);
        let map = self.usage.lock().unwrap_or_else(PoisonError::into_inner);
        let mut rows: Vec<UsageRow> = map
            .iter()
            .filter(|((_, period, key), _)| match period {
                Period::Day => *key == p.day,
                Period::Month => *key == p.month,
            })
            .map(|((app, period, key), usage)| UsageRow {
                app: app.clone(),
                period: *period,
                key: key.clone(),
                usage: *usage,
            })
            .collect();
        rows.sort_by(|a, b| a.app.cmp(&b.app).then(a.period.as_str().cmp(b.period.as_str())));
        rows
    }

    /// The current periods' keys, to load stored usage at startup.
    pub fn current_keys(&self, now: Timestamp) -> (String, String) {
        let p = self.periods(now);
        (p.day, p.month)
    }

    /// When the current day and month end, as epoch milliseconds.
    pub fn resets_ms(&self, now: Timestamp) -> (i64, i64) {
        let p = self.periods(now);
        (p.day_ends.timestamp().as_millisecond(), p.month_ends.timestamp().as_millisecond())
    }

    /// Puts stored usage back (at startup); stored values replace what is in memory.
    pub fn restore(&self, rows: Vec<UsageRow>) {
        let mut map = self.usage.lock().unwrap_or_else(PoisonError::into_inner);
        for row in rows {
            map.insert((row.app, row.period, row.key), row.usage);
        }
    }
}

impl Sink for Budgets {
    /// Counts upstream calls only: hits, joined requests and refusals used no GPU.
    fn finished(&self, f: Finished) {
        let served_by_cache =
            f.exchange.cache.as_ref().is_some_and(|n| matches!(n.result, CacheResult::Hit | CacheResult::Joined));
        if served_by_cache || f.outcome == Outcome::Rejected || f.timings.upstream_sent.is_none() {
            return;
        }
        let tokens = f.summary.prompt_tokens.unwrap_or(0) + f.summary.completion_tokens.unwrap_or(0);
        let gpu_ms = f.summary.total_ns.unwrap_or(0) / 1_000_000;
        self.add(&f.exchange.app, Usage { requests: 1, tokens, gpu_ms }, f.exchange.started_at);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(text: &str) -> Timestamp {
        text.parse().unwrap()
    }

    #[test]
    fn a_used_up_budget_refuses_until_the_period_resets() {
        let budgets = Budgets::new("Asia/Kolkata");
        let limits = AppLimits { tokens_per_day: Some(1000), requests_per_day: Some(5), ..AppLimits::default() };
        let now = at("2026-10-26T10:00:00+05:30");
        assert!(budgets.check("pp", &limits, now).is_ok());
        budgets.add("pp", Usage { requests: 1, tokens: 1000, gpu_ms: 4000 }, now);
        let exceeded = budgets.check("pp", &limits, now).unwrap_err();
        assert_eq!(exceeded.message, "budget exceeded: pp used 1,000 of 1,000 tokens today; resets at 00:00");
        assert_eq!(exceeded.retry_after, Duration::from_secs(14 * 3600));
        // Another app, and the next day, start fresh.
        assert!(budgets.check("other", &limits, now).is_ok());
        assert!(budgets.check("pp", &limits, at("2026-10-27T00:00:01+05:30")).is_ok());
    }

    #[test]
    fn months_and_gpu_seconds() {
        let budgets = Budgets::new("UTC");
        let limits = AppLimits { tokens_per_month: Some(50), gpu_seconds_per_day: Some(10), ..AppLimits::default() };
        budgets.add("pp", Usage { requests: 1, tokens: 20, gpu_ms: 9_999 }, at("2026-10-01T00:00:00Z"));
        assert!(budgets.check("pp", &limits, at("2026-10-01T12:00:00Z")).is_ok(), "9.999 s is under 10 s");
        budgets.add("pp", Usage { requests: 1, tokens: 30, gpu_ms: 1 }, at("2026-10-02T00:00:00Z"));
        let exceeded = budgets.check("pp", &limits, at("2026-10-31T23:59:00Z")).unwrap_err();
        assert!(
            exceeded.message.contains("50 of 50 tokens this month; resets at 2026-11-01 00:00"),
            "{}",
            exceeded.message
        );
        assert!(budgets.check("pp", &limits, at("2026-11-01T00:00:00Z")).is_ok());
    }

    #[test]
    fn rows_round_trip_through_restore() {
        let budgets = Budgets::new("UTC");
        let now = at("2026-10-26T10:00:00Z");
        budgets.add("pp", Usage { requests: 2, tokens: 300, gpu_ms: 5000 }, now);
        let rows = budgets.rows(now);
        assert_eq!(rows.len(), 2);
        assert_eq!((rows[0].period, rows[0].key.as_str()), (Period::Day, "2026-10-26"));
        let restored = Budgets::new("UTC");
        restored.restore(rows.clone());
        assert_eq!(restored.rows(now), rows);
    }
}
