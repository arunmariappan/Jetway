//! Fair use of one GPU (plan D13, D14, phase 9): per-app rate limits and budgets, a priority queue for upstream
//! slots with load shedding, an output cap against runaway generations, and a circuit breaker for a dead upstream.

pub mod budget;
pub mod cap;
pub mod health;
pub mod rate;
pub mod scheduler;

use axum::http::StatusCode;
use serde::Serialize;

use crate::config::{AppConfig, Config, Priority};

/// Why Jetway refused a request for fair use (all are recorded with the `rejected` outcome).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Refusal {
    /// Over the app's `requests_per_minute` (429).
    RateLimit,
    /// A day's or month's budget used up (429).
    Budget,
    /// It would have waited longer than its class allows for an upstream slot (503).
    Queue,
    /// The upstream is down and the circuit is open (503).
    CircuitOpen,
}

impl Refusal {
    pub const ALL: [Refusal; 4] = [Self::RateLimit, Self::Budget, Self::Queue, Self::CircuitOpen];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::RateLimit => "rate_limit",
            Self::Budget => "budget",
            Self::Queue => "queue",
            Self::CircuitOpen => "circuit_open",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|r| r.as_str() == text)
    }

    pub fn status(self) -> StatusCode {
        match self {
            Self::RateLimit | Self::Budget => StatusCode::TOO_MANY_REQUESTS,
            Self::Queue | Self::CircuitOpen => StatusCode::SERVICE_UNAVAILABLE,
        }
    }
}

/// What the limits did to one exchange.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LimitNote {
    pub priority: Priority,
    pub refusal: Option<Refusal>,
    /// The output cap Jetway added to the request, if it added one.
    pub output_cap: Option<u32>,
}

/// One app's limits: its own settings, else `[apps.default]`'s, else none.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AppLimits {
    pub priority: Priority,
    pub requests_per_minute: Option<u32>,
    pub burst: Option<u32>,
    pub requests_per_day: Option<u64>,
    pub tokens_per_day: Option<u64>,
    pub tokens_per_month: Option<u64>,
    pub gpu_seconds_per_day: Option<u64>,
    pub max_output_tokens: Option<u32>,
}

impl AppLimits {
    pub fn for_app(config: &Config, app: &str) -> Self {
        let own = config.apps.get(app);
        let fallback = config.apps.get(crate::server::identity::DEFAULT_APP);
        fn pick<T: Copy>(
            own: Option<&AppConfig>,
            fallback: Option<&AppConfig>,
            f: impl Fn(&AppConfig) -> Option<T>,
        ) -> Option<T> {
            own.and_then(&f).or_else(|| fallback.and_then(&f))
        }
        Self {
            priority: pick(own, fallback, |a| a.priority).unwrap_or_default(),
            requests_per_minute: pick(own, fallback, |a| a.requests_per_minute),
            burst: pick(own, fallback, |a| a.burst),
            requests_per_day: pick(own, fallback, |a| a.requests_per_day),
            tokens_per_day: pick(own, fallback, |a| a.tokens_per_day),
            tokens_per_month: pick(own, fallback, |a| a.tokens_per_month),
            gpu_seconds_per_day: pick(own, fallback, |a| a.gpu_seconds_per_day),
            max_output_tokens: pick(own, fallback, |a| a.max_output_tokens),
        }
    }
}

/// 3000000 → "3,000,000".
pub fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn own_settings_then_default_then_none() {
        let mut config = Config::default();
        config.apps.insert(
            "default".into(),
            AppConfig { requests_per_minute: Some(60), tokens_per_day: Some(1000), ..AppConfig::default() },
        );
        config.apps.insert(
            "pp".into(),
            AppConfig { priority: Some(Priority::Interactive), tokens_per_day: Some(5000), ..AppConfig::default() },
        );
        let pp = AppLimits::for_app(&config, "pp");
        assert_eq!(
            (pp.priority, pp.requests_per_minute, pp.tokens_per_day),
            (Priority::Interactive, Some(60), Some(5000))
        );
        let other = AppLimits::for_app(&config, "unlisted");
        assert_eq!(
            (other.priority, other.tokens_per_day, other.gpu_seconds_per_day),
            (Priority::Normal, Some(1000), None)
        );
        assert_eq!(AppLimits::for_app(&Config::default(), "x"), AppLimits::default());
    }

    #[test]
    fn thousands_separators() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(3_000_000), "3,000,000");
        assert_eq!(thousands(12_345), "12,345");
    }
}
