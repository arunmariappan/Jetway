//! The admin listener on its own port (plan D3): `/metrics` and `/healthz` now; the dashboard and admin API arrive
//! in phase 4.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use axum::Router;
use axum::extract::State;
use axum::http::header::CONTENT_TYPE;
use axum::response::IntoResponse;
use axum::routing::get;
use tokio::time::Instant;

use super::Gateway;
use crate::metrics::{self, Gauges, Metrics};
use crate::store::Store;

pub struct Admin {
    pub gateway: Arc<Gateway>,
    pub metrics: Arc<Metrics>,
    pub store: Option<Arc<Store>>,
    pub started: Instant,
}

impl Admin {
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
}

pub fn router(admin: Arc<Admin>) -> Router {
    Router::new().route("/metrics", get(prometheus)).route("/healthz", get(|| async { "ok" })).with_state(admin)
}

async fn prometheus(State(admin): State<Arc<Admin>>) -> impl IntoResponse {
    let body = admin.metrics.render(admin.gauges());
    ([(CONTENT_TYPE, "text/plain; version=0.0.4; charset=utf-8")], body)
}
