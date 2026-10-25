//! The gateway listener (phase 1): app identity, the path allow-list, forwarding with the stream tee, and graceful
//! shutdown.

pub mod identity;
pub mod routes;

use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::header::{CONTENT_LENGTH, CONTENT_TYPE};
use axum::http::{HeaderValue, Method, StatusCode};
use axum::response::Response;
use axum::serve::ListenerExt;
use bytes::Bytes;
use futures::StreamExt;
use tokio::net::TcpListener;
use tokio::sync::Notify;
use tokio::time::Instant;
use ulid::Ulid;

use crate::config::Config;
use crate::protocol::summary::RequestSummary;
use crate::protocol::{Framing, Protocol};
use crate::proxy::record::{Exchange, Outcome, Recorder, Sink, Timings, finish_without_response};
use crate::proxy::tee::{InflightGuard, TeeBody, TeeParts};
use crate::proxy::usage::{self, UsageFilter};
use crate::proxy::{Upstream, errors, headers};
use identity::{DEFAULT_APP, KeyMap, identify};
use routes::{PathClass, classify};

pub const ID_HEADER: &str = "x-jetway-id";

/// Everything a request handler needs, shared by all requests.
pub struct Gateway {
    config: Config,
    upstream: Upstream,
    keys: KeyMap,
    sink: Arc<dyn Sink>,
    inflight: Arc<AtomicUsize>,
}

impl Gateway {
    pub fn new(config: Config, sink: Arc<dyn Sink>) -> Result<Arc<Self>, reqwest::Error> {
        let upstream = Upstream::new(&config.upstream)?;
        let keys = KeyMap::from_config(&config);
        Ok(Arc::new(Self { config, upstream, keys, sink, inflight: Arc::new(AtomicUsize::new(0)) }))
    }

    pub fn inflight(&self) -> usize {
        self.inflight.load(Ordering::Relaxed)
    }

    pub fn upstream_url(&self) -> &str {
        self.upstream.base()
    }

    fn is_admin(&self, app: &str) -> bool {
        self.config.apps.get(app).is_some_and(|a| a.admin)
    }

    fn is_known(&self, app: &str) -> bool {
        app == DEFAULT_APP || self.config.apps.contains_key(app)
    }
}

pub fn router(gateway: Arc<Gateway>) -> Router {
    Router::new().fallback(handle).with_state(gateway)
}

/// Serves until `shutdown` completes, then lets in-flight streams finish for up to the grace period.
pub async fn run(
    listener: TcpListener,
    gateway: Arc<Gateway>,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    let grace = Duration::from_secs(gateway.config.server.shutdown_grace_seconds);
    let stopping = Arc::new(Notify::new());
    let signal = {
        let stopping = Arc::clone(&stopping);
        let gateway = Arc::clone(&gateway);
        async move {
            shutdown.await;
            tracing::info!(in_flight = gateway.inflight(), "shutting down; letting in-flight requests finish");
            stopping.notify_one();
        }
    };
    // Small stream chunks would otherwise wait for a delayed ACK (plan D17).
    let listener = listener.tap_io(|tcp| {
        let _ = tcp.set_nodelay(true);
    });
    let server = axum::serve(listener, router(Arc::clone(&gateway))).with_graceful_shutdown(signal);
    tokio::select! {
        result = server.into_future() => result,
        () = async { stopping.notified().await; tokio::time::sleep(grace).await } => {
            tracing::warn!(in_flight = gateway.inflight(), "grace period over; closing the remaining streams");
            Ok(())
        }
    }
}

/// Binds the configured address.
pub async fn bind(config: &Config) -> std::io::Result<TcpListener> {
    TcpListener::bind(SocketAddr::new(config.server.host, config.server.port)).await
}

/// Builds the error response for an exchange that ends before reaching the upstream, and records it.
fn reject(
    gateway: &Gateway,
    exchange: Exchange,
    status: StatusCode,
    outcome: Outcome,
    timings: Timings,
    message: String,
) -> Response {
    let body = errors::error_body(exchange.protocol, status, &message);
    let mut response = Response::new(Body::from(body.clone()));
    *response.status_mut() = status;
    response.headers_mut().insert(CONTENT_TYPE, HeaderValue::from_static("application/json; charset=utf-8"));
    set_id(&mut response, exchange.id);
    finish_without_response(gateway.sink.as_ref(), exchange, status.as_u16(), outcome, message, timings, body);
    response
}

fn set_id(response: &mut Response, id: Ulid) {
    if let Ok(value) = HeaderValue::from_str(&id.to_string()) {
        response.headers_mut().insert(ID_HEADER, value);
    }
}

fn is_body_too_large(error: &axum::Error) -> bool {
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(error);
    while let Some(e) = source {
        if e.is::<http_body_util::LengthLimitError>() {
            return true;
        }
        source = e.source();
    }
    false
}

async fn handle(State(gateway): State<Arc<Gateway>>, request: Request) -> Response {
    let started = Instant::now();
    let inflight = InflightGuard::new(&gateway.inflight);
    let (parts, body) = request.into_parts();
    let traceparent = parts.headers.get("traceparent").and_then(|v| v.to_str().ok()).map(str::to_string);

    let identity = identify(parts.uri.path(), &parts.headers, &gateway.keys);
    let (app, path) = match &identity {
        Ok(identity) => (identity.app.clone(), identity.path.clone()),
        Err(_) => (DEFAULT_APP.to_string(), parts.uri.path().to_string()),
    };
    let protocol = Protocol::for_path(&path);
    let class = classify(&path);
    let path_and_query = match parts.uri.query() {
        Some(query) => format!("{path}?{query}"),
        None => path.clone(),
    };
    let mut exchange = Exchange {
        id: Ulid::generate(),
        app,
        method: parts.method.clone(),
        path: path_and_query.clone(),
        protocol,
        class,
        started,
        started_at: jiff::Timestamp::now(),
        request: Bytes::new(),
        summary: None,
        traceparent,
    };
    let mut timings = Timings::default();

    if let Err(error) = identity {
        return reject(&gateway, exchange, StatusCode::BAD_REQUEST, Outcome::Rejected, timings, error.to_string());
    }
    if gateway.config.server.require_known_app && !gateway.is_known(&exchange.app) {
        let message = format!("unknown app {:?}; list it under [apps] in jetway.toml", exchange.app);
        return reject(&gateway, exchange, StatusCode::FORBIDDEN, Outcome::Rejected, timings, message);
    }
    match class {
        PathClass::Unknown => {
            let message = format!("Jetway doesn't forward {path}");
            return reject(&gateway, exchange, StatusCode::NOT_FOUND, Outcome::Rejected, timings, message);
        }
        PathClass::ModelManagement if !gateway.is_admin(&exchange.app) => {
            let message = format!("app {:?} may not use {path}; set admin = true for it in jetway.toml", exchange.app);
            return reject(&gateway, exchange, StatusCode::FORBIDDEN, Outcome::Rejected, timings, message);
        }
        _ => {}
    }

    let limit = gateway.config.server.max_request_bytes;
    let request_body = match axum::body::to_bytes(body, limit).await {
        Ok(bytes) => bytes,
        Err(error) if is_body_too_large(&error) => {
            let message = format!("the request body is larger than {limit} bytes");
            return reject(&gateway, exchange, StatusCode::PAYLOAD_TOO_LARGE, Outcome::Rejected, timings, message);
        }
        Err(error) => {
            let message = format!("couldn't read the request body: {error}");
            return reject(&gateway, exchange, StatusCode::BAD_REQUEST, Outcome::ClientCancelled, timings, message);
        }
    };
    exchange.request = request_body.clone();
    if matches!(class, PathClass::Generation | PathClass::Embedding) {
        exchange.summary = RequestSummary::parse(protocol, &request_body);
    }

    // Exact token counts for OpenAI streams: ask for usage the client didn't, and hide it again (plan D21).
    let mut upstream_body = request_body;
    let mut usage_filter = None;
    if protocol == Protocol::OpenAi
        && class == PathClass::Generation
        && exchange.summary.as_ref().is_some_and(|s| s.stream && !s.include_usage)
        && let Some(body) = usage::ask_for_usage(&upstream_body)
    {
        upstream_body = body;
        usage_filter = Some(UsageFilter::default());
    }

    let upstream = &gateway.upstream;
    let send = upstream
        .client
        .request(parts.method.clone(), upstream.url_for(&path_and_query))
        .headers(headers::to_upstream(&parts.headers))
        .body(upstream_body)
        .send();
    timings.upstream_sent = Some(started.elapsed());
    let response = match tokio::time::timeout(upstream.first_byte_timeout, send).await {
        Ok(Ok(response)) => response,
        Ok(Err(error)) => {
            let message = if error.is_connect() || error.is_timeout() {
                format!("Ollama isn't reachable at {}", upstream.base())
            } else {
                format!("the upstream request failed: {error}")
            };
            return reject(&gateway, exchange, StatusCode::BAD_GATEWAY, Outcome::UpstreamError, timings, message);
        }
        Err(_) => {
            let message = format!("no response from the upstream within {} s", upstream.first_byte_timeout.as_secs());
            return reject(&gateway, exchange, StatusCode::GATEWAY_TIMEOUT, Outcome::Timeout, timings, message);
        }
    };
    timings.headers = Some(started.elapsed());

    let status = response.status();
    let mut client_headers = headers::to_client(response.headers());
    let framing = Framing::from_content_type(response.headers().get(CONTENT_TYPE).and_then(|v| v.to_str().ok()));
    if framing.is_stream() {
        // A stream may get an error line appended, so its length isn't known in advance.
        client_headers.remove(CONTENT_LENGTH);
    }
    // A HEAD response, 204 or 304 has no body, whatever its Content-Length says, and the server never reads one.
    let bodyless =
        parts.method == Method::HEAD || status == StatusCode::NO_CONTENT || status == StatusCode::NOT_MODIFIED;
    let expected_len =
        if bodyless { Some(0) } else { client_headers.get(CONTENT_LENGTH).and_then(|v| v.to_str().ok()?.parse().ok()) };
    let id = exchange.id;
    let tee = TeeBody::new(TeeParts {
        upstream: response.bytes_stream().boxed(),
        idle_timeout: upstream.idle_timeout,
        started,
        timings,
        status: status.as_u16(),
        framing,
        protocol,
        expected_len,
        // Only an event stream can carry the usage event.
        usage_filter: usage_filter.filter(|_| framing == Framing::Sse),
        recorder: Recorder::spawn(exchange, framing, Arc::clone(&gateway.sink)),
        inflight,
    });

    let mut response = Response::new(Body::from_stream(tee));
    *response.status_mut() = status;
    *response.headers_mut() = client_headers;
    set_id(&mut response, id);
    response
}
