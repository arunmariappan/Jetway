//! Upstream health and the circuit breaker (phase 9.4). A probe asks the upstream for its version every few seconds;
//! after `failures_to_open` failed connects in a row (probes or real requests) the circuit opens and requests fail at
//! once with 503, instead of each waiting for a connect error (about 2 s on Windows for a refused loopback port,
//! found in phase 1). The next successful probe or request closes it.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Mutex, PoisonError};

use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UpstreamStatus {
    pub up: bool,
    pub circuit_open: bool,
    pub version: Option<String>,
    pub error: Option<String>,
    pub checked_ms: Option<i64>,
}

pub struct Health {
    threshold: u32,
    failures: AtomicU32,
    open: AtomicBool,
    last: Mutex<UpstreamStatus>,
}

impl Health {
    pub fn new(failures_to_open: u32) -> Self {
        Self {
            threshold: failures_to_open.max(1),
            failures: AtomicU32::new(0),
            open: AtomicBool::new(false),
            last: Mutex::new(UpstreamStatus {
                up: true,
                circuit_open: false,
                version: None,
                error: None,
                checked_ms: None,
            }),
        }
    }

    pub fn is_open(&self) -> bool {
        self.open.load(Ordering::Relaxed)
    }

    /// A probe or request reached the upstream.
    pub fn success(&self, version: Option<String>) {
        self.failures.store(0, Ordering::Relaxed);
        if self.open.swap(false, Ordering::Relaxed) {
            tracing::info!("the upstream answers again; circuit closed");
        }
        let mut last = self.last.lock().unwrap_or_else(PoisonError::into_inner);
        last.up = true;
        last.circuit_open = false;
        last.error = None;
        if version.is_some() {
            last.version = version;
        }
        last.checked_ms = Some(jiff::Timestamp::now().as_millisecond());
    }

    /// A probe or request couldn't connect.
    pub fn failure(&self, error: &str) {
        let failures = self.failures.fetch_add(1, Ordering::Relaxed) + 1;
        if failures >= self.threshold && !self.open.swap(true, Ordering::Relaxed) {
            tracing::warn!(failures, "the upstream isn't answering; circuit open, requests fail fast");
        }
        let mut last = self.last.lock().unwrap_or_else(PoisonError::into_inner);
        last.up = false;
        last.circuit_open = self.is_open();
        last.error = Some(error.to_string());
        last.checked_ms = Some(jiff::Timestamp::now().as_millisecond());
    }

    pub fn status(&self) -> UpstreamStatus {
        self.last.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opens_after_the_threshold_and_closes_on_success() {
        let health = Health::new(3);
        health.failure("refused");
        health.failure("refused");
        assert!(!health.is_open());
        health.failure("refused");
        assert!(health.is_open());
        assert!(health.status().circuit_open);
        health.success(Some("0.40.11".into()));
        assert!(!health.is_open());
        let status = health.status();
        assert_eq!((status.up, status.version.as_deref()), (true, Some("0.40.11")));
        // A success resets the count.
        health.failure("refused");
        assert!(!health.is_open());
    }
}
