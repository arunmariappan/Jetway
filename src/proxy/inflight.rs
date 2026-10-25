//! Exchanges in progress: a count for metrics and shutdown, and a list for the dashboard's live page (phase 4).

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use serde::Serialize;
use tokio::time::Instant;
use ulid::Ulid;

use super::record::Exchange;

#[derive(Debug, Clone, Serialize)]
pub struct InflightEntry {
    pub id: String,
    pub app: String,
    pub method: String,
    pub path: String,
    pub started_at_ms: i64,
    #[serde(skip)]
    pub started: Instant,
}

#[derive(Default)]
pub struct Inflight {
    count: AtomicUsize,
    entries: Mutex<HashMap<Ulid, InflightEntry>>,
}

impl Inflight {
    /// Registers an exchange; it stays listed until the guard drops.
    pub fn enter(self: &Arc<Self>, exchange: &Exchange) -> InflightGuard {
        self.count.fetch_add(1, Ordering::Relaxed);
        let entry = InflightEntry {
            id: exchange.id.to_string(),
            app: exchange.app.clone(),
            method: exchange.method.to_string(),
            path: exchange.path.clone(),
            started_at_ms: exchange.started_at.as_millisecond(),
            started: exchange.started,
        };
        self.entries.lock().unwrap_or_else(PoisonError::into_inner).insert(exchange.id, entry);
        InflightGuard { inflight: Arc::clone(self), id: exchange.id }
    }

    pub fn len(&self) -> usize {
        self.count.load(Ordering::Relaxed)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The exchanges in progress, oldest first.
    pub fn list(&self) -> Vec<InflightEntry> {
        let mut entries: Vec<InflightEntry> =
            self.entries.lock().unwrap_or_else(PoisonError::into_inner).values().cloned().collect();
        entries.sort_by(|a, b| a.id.cmp(&b.id));
        entries
    }
}

/// Keeps one exchange listed and counted; dropping it ends that.
pub struct InflightGuard {
    inflight: Arc<Inflight>,
    id: Ulid,
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        self.inflight.count.fetch_sub(1, Ordering::Relaxed);
        self.inflight.entries.lock().unwrap_or_else(PoisonError::into_inner).remove(&self.id);
    }
}
