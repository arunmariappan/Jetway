//! The gateway listener: app identity, the path allow-list (phase 1), the exact cache and joined requests
//! (phase 3), forwarding with the stream tee, and graceful shutdown.

pub mod admin;
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

use crate::cache::answer::{self, Entry};
use crate::cache::flight::{Joined, Reader, Start, fly};
use crate::cache::{CACHE_HEADER, Cache, CacheKey, hit_note, joined_note};
use crate::config::{Config, Replay};
use crate::protocol::summary::RequestSummary;
use crate::protocol::{Framing, Protocol};
use crate::proxy::record::{Exchange, Outcome, Recorder, Sink, Sinks, Timings, finish_without_response};
use crate::proxy::served::{FlightBody, Report, ServedBody};
use crate::proxy::tee::{InflightGuard, TeeBody, TeeParts};
use crate::proxy::usage::{self, UsageFilter};
use crate::proxy::{Upstream, errors, headers};
use crate::store::Store;
use crate::telemetry::SpanIds;
use identity::{DEFAULT_APP, KeyMap, identify};
use routes::{PathClass, classify};

pub const ID_HEADER: &str = "x-jetway-id";

/// Exchange ids increase strictly, even within one millisecond, so ordering by id is ordering by arrival (the
/// request log pages by id).
pub fn next_id() -> Ulid {
    static GENERATOR: std::sync::Mutex<ulid::Generator> = std::sync::Mutex::new(ulid::Generator::new());
    let mut generator = GENERATOR.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    generator.generate().unwrap_or_else(|_| Ulid::generate())
}

/// Everything a request handler needs, shared by all requests.
pub struct Gateway {
    config: Config,
    upstream: Upstream,
    keys: KeyMap,
    /// The cache first, then the sink the gateway was built with.
    sink: Arc<dyn Sink>,
    cache: Arc<Cache>,
    inflight: Arc<AtomicUsize>,
    /// Span ids are chosen per request only when spans are exported.
    spans: bool,
}

impl Gateway {
    /// `store` persists the cache across restarts; without one the cache lives in memory only.
    pub fn new(config: Config, sink: Arc<dyn Sink>, store: Option<Arc<Store>>) -> Result<Arc<Self>, reqwest::Error> {
        let upstream = Upstream::new(&config.upstream)?;
        let keys = KeyMap::from_config(&config);
        let spans = !config.otel.endpoint.is_empty();
        let cache = Arc::new(Cache::new(&config, store));
        // The cache stores answers before anything else sees the exchange.
        let sink: Arc<dyn Sink> = Arc::new(Sinks(vec![Arc::clone(&cache) as Arc<dyn Sink>, sink]));
        Ok(Arc::new(Self { config, upstream, keys, sink, cache, inflight: Arc::new(AtomicUsize::new(0)), spans }))
    }

    pub fn cache(&self) -> &Arc<Cache> {
        &self.cache
    }

    pub fn inflight(&self) -> usize {
        self.inflight.load(Ordering::Relaxed)
    }

    pub fn upstream_url(&self) -> &str {
        self.upstream.base()
    }

    pub fn config(&self) -> &Config {
        &self.config
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

/// Serves the admin listener until `shutdown` completes.
pub async fn run_admin(
    listener: TcpListener,
    admin: Arc<admin::Admin>,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    axum::serve(listener, admin::router(admin)).with_graceful_shutdown(shutdown).await
}

/// Binds the configured gateway address.
pub async fn bind(config: &Config) -> std::io::Result<TcpListener> {
    TcpListener::bind(SocketAddr::new(config.server.host, config.server.port)).await
}

/// Binds the configured admin address.
pub async fn bind_admin(config: &Config) -> std::io::Result<TcpListener> {
    TcpListener::bind(SocketAddr::new(config.server.host, config.server.admin_port)).await
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

fn set_header(response: &mut Response, name: &'static str, value: &str) {
    if let Ok(value) = HeaderValue::from_str(value) {
        response.headers_mut().insert(name, value);
    }
}

/// Serves a cache hit in the request's protocol and stream mode (phase 3.3).
fn serve_hit(
    gateway: &Gateway,
    mut exchange: Exchange,
    key: CacheKey,
    entry: &Entry,
    timings: Timings,
    inflight: InflightGuard,
) -> Response {
    let summary = exchange.summary.as_ref();
    let stream = exchange.class == PathClass::Generation && summary.is_some_and(|s| s.stream);
    let include_usage = summary.is_some_and(|s| s.include_usage);
    let served = answer::serve(entry, exchange.protocol, stream, include_usage);
    let note = hit_note(key, entry);
    let (age, source) = (note.age_s.unwrap_or(0), note.source.clone().unwrap_or_default());
    exchange.cache = Some(note);
    gateway.cache.hit(&key);

    let paced = stream && gateway.cache.replay(&exchange.app) == Replay::Paced;
    let (pieces, gap) = if paced {
        let items: Vec<Bytes> = served
            .pieces
            .iter()
            .flat_map(|piece| answer::split_items(piece, exchange.protocol == Protocol::OpenAi))
            .collect();
        let generation = Duration::from_nanos(entry.answer.eval_ns.unwrap_or(0));
        let gap = generation / u32::try_from(items.len().max(1)).unwrap_or(u32::MAX);
        (items, gap)
    } else {
        (served.pieces, Duration::ZERO)
    };
    let length: usize = pieces.iter().map(Bytes::len).sum();
    let framing = Framing::from_content_type(Some(&served.content_type));
    let id = exchange.id;
    let report = Report { exchange, sink: Arc::clone(&gateway.sink), status: 200, framing, timings, inflight };
    let mut response = Response::new(Body::from_stream(ServedBody::new(pieces, gap, report)));
    set_header(&mut response, "content-type", &served.content_type);
    if !paced {
        set_header(&mut response, "content-length", &length.to_string());
    }
    set_header(&mut response, CACHE_HEADER, "hit");
    set_header(&mut response, "x-jetway-cache-age", &age.to_string());
    set_header(&mut response, "x-jetway-source", &source);
    set_id(&mut response, id);
    response
}

/// Records a joined exchange as cancelled if its client leaves while it waits for the flight's response.
struct PendingReport(Option<Report>);

impl Drop for PendingReport {
    fn drop(&mut self) {
        if let Some(report) = self.0.take() {
            report.finish(Outcome::ClientCancelled, None, None, 0);
        }
    }
}

/// Serves a request that joined an identical one in flight (phase 3.5).
async fn serve_joined(
    gateway: &Gateway,
    mut exchange: Exchange,
    reader: Reader,
    timings: Timings,
    inflight: InflightGuard,
) -> Response {
    let flight = Arc::clone(reader.flight());
    exchange.cache = Some(joined_note(exchange.cache.as_ref().and_then(|n| n.key), &flight.leader));
    let id = exchange.id;
    let protocol = exchange.protocol;
    let sink = Arc::clone(&gateway.sink);
    let mut pending =
        PendingReport(Some(Report { exchange, sink, status: 0, framing: Framing::Whole, timings, inflight }));
    let start = flight.wait_start().await;
    let Some(mut report) = pending.0.take() else {
        return errors::error_response(protocol, StatusCode::BAD_GATEWAY, "the joined request lost its report");
    };
    let mut response = match start {
        Some(Start::Response { status, headers }) => {
            report.status = status.as_u16();
            report.framing = Framing::from_content_type(headers.get(CONTENT_TYPE).and_then(|v| v.to_str().ok()));
            let mut response = Response::new(Body::from_stream(FlightBody::joined(reader, report)));
            *response.status_mut() = status;
            *response.headers_mut() = headers;
            response
        }
        Some(Start::Failed { status, headers, body }) => {
            report.status = status.as_u16();
            report.finish(Outcome::UpstreamError, Some("the request it joined failed".into()), None, body.len() as u64);
            let mut response = Response::new(Body::from(body));
            *response.status_mut() = status;
            *response.headers_mut() = headers;
            response
        }
        None => {
            let message = "the request it joined ended without a response";
            report.status = 502;
            report.finish(Outcome::UpstreamError, Some(message.into()), None, 0);
            errors::error_response(protocol, StatusCode::BAD_GATEWAY, message)
        }
    };
    set_header(&mut response, CACHE_HEADER, "joined");
    set_id(&mut response, id);
    response
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
        id: next_id(),
        app,
        method: parts.method.clone(),
        path: path_and_query.clone(),
        protocol,
        class,
        started,
        started_at: jiff::Timestamp::now(),
        request: Bytes::new(),
        summary: None,
        spans: gateway.spans.then(|| SpanIds::new(traceparent.as_deref())),
        traceparent,
        cache: None,
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
    let body_value: Option<serde_json::Value> = matches!(class, PathClass::Generation | PathClass::Embedding)
        .then(|| serde_json::from_slice(&request_body).ok())
        .flatten();
    if let Some(value) = &body_value {
        exchange.summary = RequestSummary::from_value(protocol, value);
        if class == PathClass::Embedding
            && let Some(summary) = &mut exchange.summary
        {
            // Embedding endpoints never stream, whatever Ollama's default says for chat.
            summary.stream = false;
        }
    }

    // The exact cache, then joining an identical request in flight (phase 3).
    let decision = gateway.cache.decide(&exchange, body_value.as_ref(), &parts.headers);
    exchange.cache = decision.as_ref().map(|d| d.note.clone());
    let mut leader = None;
    if let Some(decision) = &decision
        && decision.lookup
    {
        if let Some(key) = decision.note.key
            && let Some(entry) = gateway.cache.lookup(&key).await
        {
            return serve_hit(&gateway, exchange, key, &entry, timings, inflight);
        }
        if let Some(join_key) = decision.join {
            match gateway.cache.flights.join_or_lead(join_key, exchange.id) {
                Joined::Follower(reader) => return serve_joined(&gateway, exchange, reader, timings, inflight).await,
                Joined::Leader(flight) => leader = Some((join_key, flight)),
            }
        }
    }
    // Joined requests waiting on a flight whose upstream call failed get the same error.
    let fail_flight = |status: StatusCode, message: &str| {
        if let Some((key, flight)) = &leader {
            let mut headers = axum::http::HeaderMap::new();
            headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json; charset=utf-8"));
            let body = errors::error_body(protocol, status, message);
            flight.started(Start::Failed { status, headers, body });
            gateway.cache.flights.remove(key, flight);
        }
    };

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

    let mut upstream_headers = headers::to_upstream(&parts.headers);
    if let Some(ids) = &exchange.spans
        && let Ok(value) = HeaderValue::from_str(&ids.upstream_traceparent())
    {
        // Jetway's client span becomes the parent of whatever the upstream does (plan D16).
        upstream_headers.insert("traceparent", value);
    }
    let upstream = &gateway.upstream;
    let send = upstream
        .client
        .request(parts.method.clone(), upstream.url_for(&path_and_query))
        .headers(upstream_headers)
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
            fail_flight(StatusCode::BAD_GATEWAY, &message);
            return reject(&gateway, exchange, StatusCode::BAD_GATEWAY, Outcome::UpstreamError, timings, message);
        }
        Err(_) => {
            let message = format!("no response from the upstream within {} s", upstream.first_byte_timeout.as_secs());
            fail_flight(StatusCode::GATEWAY_TIMEOUT, &message);
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
    let cache_result = exchange.cache.as_ref().map(|n| n.result.as_str());
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

    let body = match leader {
        Some((key, flight)) => {
            flight.started(Start::Response { status, headers: client_headers.clone() });
            match flight.reader() {
                Some(reader) => {
                    // A task reads the upstream for every client of the flight, this one included.
                    let gateway = Arc::clone(&gateway);
                    tokio::spawn(async move {
                        fly(Arc::clone(&flight), tee).await;
                        gateway.cache.flights.remove(&key, &flight);
                    });
                    Body::from_stream(FlightBody::leader(reader))
                }
                // A new flight has no readers to lose, so this can't happen; fall back to the plain stream.
                None => Body::from_stream(tee),
            }
        }
        None => Body::from_stream(tee),
    };

    let mut response = Response::new(body);
    *response.status_mut() = status;
    *response.headers_mut() = client_headers;
    if let Some(result) = cache_result {
        set_header(&mut response, CACHE_HEADER, result);
    }
    set_id(&mut response, id);
    response
}
