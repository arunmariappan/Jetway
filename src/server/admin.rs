//! The admin listener's shared state (plan D3): the dashboard, admin API and `/metrics` read from it. The routes
//! are in [`crate::web`].

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tokio::time::Instant;

use super::Gateway;
use crate::metrics::{self, Gauges, Metrics};
use crate::store::Store;
use crate::web::feed::Feed;

const STATUS_FOR: Duration = Duration::from_secs(5);

/// Whether the upstream answered its last probe.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct UpstreamStatus {
    pub up: bool,
    pub version: Option<String>,
    pub error: Option<String>,
}

pub struct Admin {
    pub gateway: Arc<Gateway>,
    pub metrics: Arc<Metrics>,
    pub store: Option<Arc<Store>>,
    pub feed: Feed,
    pub started: Instant,
    status: Mutex<Option<(Instant, UpstreamStatus)>>,
}

impl Admin {
    pub fn new(gateway: Arc<Gateway>, metrics: Arc<Metrics>, store: Option<Arc<Store>>, feed: Feed) -> Self {
        Self { gateway, metrics, store, feed, started: Instant::now(), status: Mutex::new(None) }
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
        }
    }

    /// The upstream's status, probed at most every five seconds.
    pub async fn upstream_status(&self) -> UpstreamStatus {
        if let Some((at, status)) = self.status.lock().unwrap_or_else(PoisonError::into_inner).clone()
            && at.elapsed() < STATUS_FOR
        {
            return status;
        }
        let status = match self.gateway.probe_upstream().await {
            Ok(version) => UpstreamStatus { up: true, version, error: None },
            Err(error) => UpstreamStatus { up: false, version: None, error: Some(error) },
        };
        *self.status.lock().unwrap_or_else(PoisonError::into_inner) = Some((Instant::now(), status.clone()));
        status
    }
}
