//! Everything `jetway serve` runs, started together and stopped together: the gateway and admin listeners, the
//! store, metrics and optional span export, and the background tasks that write rollups and budgets, apply
//! retention, probe the upstream, sample the queue and evaluate alerts.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::alerts::telegram::Telegram;
use crate::alerts::{self, Alerts, Inputs};
use crate::config::Config;
use crate::metrics::{Metrics, Minute};
use crate::proxy::record::{Sink, Sinks};
use crate::server::admin::Admin;
use crate::server::{self, Gateway};
use crate::store::sink::RequestLog;
use crate::store::{Store, StoreError};
use crate::telemetry::Telemetry;
use crate::web::feed::Feed;

const ROLLUP_EVERY: Duration = Duration::from_secs(15);
const QUEUE_SAMPLE_EVERY: Duration = Duration::from_secs(10);
const ALERTS_EVERY: Duration = Duration::from_secs(15);
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
    pub alerts: Arc<Alerts>,
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
    // The dashboard's charts cover a day; bring back the minutes written before a restart.
    match store.rollups_since(now_ms() - DAY_MS).await {
        Ok(rows) => metrics.restore(rows, now_ms()),
        Err(error) => tracing::warn!(%error, "couldn't restore the last day's rollups"),
    }

    let feed = Feed::default();
    let mut sinks: Vec<Arc<dyn Sink>> = vec![
        Arc::clone(&metrics) as Arc<dyn Sink>,
        Arc::new(RequestLog::new(Arc::clone(&store), config.store.bodies)),
        Arc::new(feed.clone()),
    ];
    if !config.otel.endpoint.is_empty() {
        sinks.push(Arc::new(Telemetry::start(&config.otel.endpoint, &config.otel.service_name, &config.upstream.url)));
    }
    sinks.extend(extra);
    let gateway = Gateway::new(config.clone(), Arc::new(Sinks(sinks)), Some(Arc::clone(&store)))?;
    // Budgets carry on from where they were before a restart.
    let (day, month) = gateway.budgets().current_keys(jiff::Timestamp::now());
    match store.budgets_for(day, month).await {
        Ok(rows) => gateway.budgets().restore(rows),
        Err(error) => tracing::warn!(%error, "couldn't restore budget usage"),
    }
    let alerts = Arc::new(Alerts::new(config.alerts.cooldown_minutes));
    let admin = Arc::new(Admin::new(
        Arc::clone(&gateway),
        Arc::clone(&metrics),
        Some(Arc::clone(&store)),
        feed,
        Arc::clone(&alerts),
    ));

    let token = CancellationToken::new();
    let servers = vec![
        tokio::spawn(server::run(gateway_listener, Arc::clone(&gateway), token.clone().cancelled_owned())),
        tokio::spawn(server::run_admin(admin_listener, admin, token.clone().cancelled_owned())),
    ];

    let rollups = {
        let (token, store, metrics, gateway) =
            (token.clone(), Arc::clone(&store), Arc::clone(&metrics), Arc::clone(&gateway));
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    () = token.cancelled() => return,
                    () = tokio::time::sleep(ROLLUP_EVERY) => {
                        store.write_rollups(metrics.take_closed(now_ms()));
                        store.write_budgets(gateway.budgets().rows(jiff::Timestamp::now()));
                    }
                }
            }
        })
    };
    let probe = {
        let (token, gateway) = (token.clone(), Arc::clone(&gateway));
        let every = Duration::from_secs(config.health.probe_seconds);
        tokio::spawn(async move {
            loop {
                match gateway.probe_upstream().await {
                    Ok(version) => gateway.health().success(version),
                    Err(error) => gateway.health().failure(&error),
                }
                tokio::select! {
                    () = token.cancelled() => return,
                    () = tokio::time::sleep(every) => {}
                }
            }
        })
    };
    let queue = {
        let (token, gateway) = (token.clone(), Arc::clone(&gateway));
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    () = token.cancelled() => return,
                    () = tokio::time::sleep(QUEUE_SAMPLE_EVERY) => gateway.scheduler().sample(now_ms()),
                }
            }
        })
    };
    let alerting = config.alerts.enabled.then(|| {
        let telegram = if config.alerts.telegram {
            match Telegram::from_env(&config.alerts.telegram_api) {
                Ok(telegram) => Some(telegram),
                Err(reason) => {
                    tracing::warn!(%reason, "alerts.telegram is on, but alerts won't be sent");
                    None
                }
            }
        } else {
            None
        };
        let (token, gateway, metrics, store, alerts) =
            (token.clone(), Arc::clone(&gateway), Arc::clone(&metrics), Arc::clone(&store), Arc::clone(&alerts));
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    () = token.cancelled() => return,
                    () = tokio::time::sleep(ALERTS_EVERY) => {}
                }
                let notices = evaluate_alerts(&gateway, &metrics, &store, &alerts);
                for notice in notices {
                    tracing::warn!(alert = %notice, "alert");
                    if let Some(telegram) = &telegram
                        && let Err(error) = telegram.send(&notice).await
                    {
                        tracing::warn!(%error, "couldn't send an alert to Telegram");
                    }
                }
            }
        })
    });
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

    let mut tasks = vec![rollups, retention, probe, queue];
    tasks.extend(alerting);
    Ok(Running { gateway_addr, admin_addr, gateway, store, metrics, alerts, token, servers, tasks })
}

/// One evaluation of the alert rules; returns the notifications to send.
pub fn evaluate_alerts(gateway: &Gateway, metrics: &Metrics, store: &Store, alerts: &Alerts) -> Vec<String> {
    let now = now_ms();
    let from = now - alerts::WINDOW_MS;
    let recent: Vec<(String, Minute)> = metrics
        .apps(from, now + 1)
        .into_iter()
        .map(|app| {
            let mut merged = Minute::default();
            for minute in metrics.minutes(from, now + 1, Some(&app)).values() {
                merged.merge(minute);
            }
            (app, merged)
        })
        .collect();
    let budgets = gateway.budgets().rows(jiff::Timestamp::now());
    let upstream = gateway.health().status();
    let inputs = Inputs {
        config: gateway.config(),
        recent: &recent,
        budgets: &budgets,
        upstream: &upstream,
        upstream_name: gateway.upstream_name(),
        log_dropped: store.counters.dropped.load(std::sync::atomic::Ordering::Relaxed),
    };
    let firing = alerts.evaluate(&inputs, now);
    alerts.apply(firing, now)
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
        self.store.write_budgets(self.gateway.budgets().rows(jiff::Timestamp::now()));
        if let Err(error) = self.store.flush().await {
            tracing::warn!(%error, "couldn't write the last records");
        }
        result
    }
}
