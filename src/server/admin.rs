//! The admin listener's shared state (plan D3): the dashboard, admin API and `/metrics` read from it. The routes
//! are in [`crate::web`].

use std::sync::Arc;
use std::sync::atomic::Ordering;

use tokio::time::Instant;

use super::Gateway;
use crate::alerts::Alerts;
use crate::limits::health::UpstreamStatus;
use crate::metrics::{self, Gauges, Metrics};
use crate::store::Store;
use crate::web::feed::Feed;

pub struct Admin {
    pub gateway: Arc<Gateway>,
    pub metrics: Arc<Metrics>,
    pub store: Option<Arc<Store>>,
    pub feed: Feed,
    pub alerts: Arc<Alerts>,
    pub started: Instant,
}

impl Admin {
    pub fn new(
        gateway: Arc<Gateway>,
        metrics: Arc<Metrics>,
        store: Option<Arc<Store>>,
        feed: Feed,
        alerts: Arc<Alerts>,
    ) -> Self {
        Self { gateway, metrics, store, feed, alerts, started: Instant::now() }
    }

    pub fn gauges(&self) -> Gauges {
        let counters = self.store.as_ref().map(|s| &s.counters);
        let (cache_entries, cache_bytes) = self.gateway.cache().memory_usage();
        Gauges {
            in_flight: self.gateway.inflight(),
            log_dropped: counters.map_or(0, |c| c.dropped.load(Ordering::Relaxed)),
            log_write_errors: counters.map_or(0, |c| c.write_errors.load(Ordering::Relaxed)),
            memory_bytes: metrics::memory_bytes(),
            uptime: self.started.elapsed(),
            cache_entries,
            cache_bytes,
            flights: self.gateway.cache().flights.len(),
            queue: self.gateway.scheduler().depth(),
            circuit_open: self.gateway.health().is_open(),
        }
    }

    /// The upstream's status from the last probe or request (the probe task runs every `health.probe_seconds`).
    pub fn upstream_status(&self) -> UpstreamStatus {
        self.gateway.health().status()
    }
}
