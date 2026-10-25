//! Everything `jetway serve` runs, started together and stopped together: the gateway and admin listeners, the
//! store, metrics and optional span export, and the background tasks that write rollups and apply retention.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::metrics::Metrics;
use crate::proxy::record::{Sink, Sinks};
use crate::server::admin::Admin;
use crate::server::{self, Gateway};
use crate::store::sink::RequestLog;
use crate::store::{Store, StoreError};
use crate::telemetry::Telemetry;

const ROLLUP_EVERY: Duration = Duration::from_secs(15);
const PRUNE_EVERY: Duration = Duration::from_secs(3600);
const DAY_MS: i64 = 24 * 3600 * 1000;

#[derive(Debug, thiserror::Error)]
pub enum StartError {
    #[error("couldn't open the request log: {0}")]
    Store(#[from] StoreError),
    #[error("couldn't build the upstream client: {0}")]
    Client(#[from] reqwest::Error),
    #[error("couldn't read a listener's address: {0}")]
    Address(#[from] std::io::Error),
}

pub struct Running {
    pub gateway_addr: SocketAddr,
    pub admin_addr: SocketAddr,
    pub gateway: Arc<Gateway>,
    pub store: Arc<Store>,
    pub metrics: Arc<Metrics>,
    token: CancellationToken,
    servers: Vec<JoinHandle<std::io::Result<()>>>,
    tasks: Vec<JoinHandle<()>>,
}

fn now_ms() -> i64 {
    jiff::Timestamp::now().as_millisecond()
}

/// Starts everything on the given listeners. `extra` sinks see every finished exchange too (a log, a test).
pub async fn start(
    config: Config,
    gateway_listener: TcpListener,
    admin_listener: TcpListener,
    extra: Vec<Arc<dyn Sink>>,
) -> Result<Running, StartError> {
    let gateway_addr = gateway_listener.local_addr()?;
    let admin_addr = admin_listener.local_addr()?;
    let store = Arc::new(Store::open(&config.store.path)?);
    let metrics = Arc::new(Metrics::default());

    let mut sinks: Vec<Arc<dyn Sink>> =
        vec![Arc::clone(&metrics) as Arc<dyn Sink>, Arc::new(RequestLog::new(Arc::clone(&store), config.store.bodies))];
    if !config.otel.endpoint.is_empty() {
        sinks.push(Arc::new(Telemetry::start(&config.otel.endpoint, &config.otel.service_name, &config.upstream.url)));
    }
    sinks.extend(extra);
    let gateway = Gateway::new(config.clone(), Arc::new(Sinks(sinks)))?;
    let admin = Arc::new(Admin {
        gateway: Arc::clone(&gateway),
        metrics: Arc::clone(&metrics),
        store: Some(Arc::clone(&store)),
        started: Instant::now(),
    });

    let token = CancellationToken::new();
    let servers = vec![
        tokio::spawn(server::run(gateway_listener, Arc::clone(&gateway), token.clone().cancelled_owned())),
        tokio::spawn(server::run_admin(admin_listener, admin, token.clone().cancelled_owned())),
    ];

    let rollups = {
        let (token, store, metrics) = (token.clone(), Arc::clone(&store), Arc::clone(&metrics));
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    () = token.cancelled() => return,
                    () = tokio::time::sleep(ROLLUP_EVERY) => store.write_rollups(metrics.take_closed(now_ms())),
                }
            }
        })
    };
    let retention = {
        let (token, store) = (token.clone(), Arc::clone(&store));
        let (days, rollup_days) =
            (i64::from(config.store.retention_days), i64::from(config.store.rollup_retention_days));
        tokio::spawn(async move {
            loop {
                if days > 0 || rollup_days > 0 {
                    let exchanges_before = if days > 0 { now_ms() - days * DAY_MS } else { i64::MIN };
                    let rollups_before = if rollup_days > 0 { now_ms() - rollup_days * DAY_MS } else { i64::MIN };
                    match store.prune(exchanges_before, rollups_before).await {
                        Ok((0, 0)) => {}
                        Ok((exchanges, rollups)) => tracing::info!(exchanges, rollups, "pruned old records"),
                        Err(error) => tracing::warn!(%error, "couldn't prune old records"),
                    }
                }
                tokio::select! {
                    () = token.cancelled() => return,
                    () = tokio::time::sleep(PRUNE_EVERY) => {}
                }
            }
        })
    };

    Ok(Running { gateway_addr, admin_addr, gateway, store, metrics, token, servers, tasks: vec![rollups, retention] })
}

impl Running {
    /// Asks everything to stop; in-flight streams get the grace period.
    pub fn stop(&self) {
        self.token.cancel();
    }

    /// Waits for the listeners to stop, then writes the last rollups and everything still queued.
    pub async fn wait(self) -> std::io::Result<()> {
        let mut result = Ok(());
        for server in self.servers {
            match server.await {
                Ok(Err(error)) => result = Err(error),
                Err(error) => result = Err(std::io::Error::other(error)),
                Ok(Ok(())) => {}
            }
        }
        for task in self.tasks {
            let _ = task.await;
        }
        // Recorders finish just after their streams; give the last ones a moment.
        tokio::time::sleep(Duration::from_millis(50)).await;
        self.store.write_rollups(self.metrics.take_closed(now_ms() + 60_000));
        if let Err(error) = self.store.flush().await {
            tracing::warn!(%error, "couldn't write the last records");
        }
        result
    }
}
