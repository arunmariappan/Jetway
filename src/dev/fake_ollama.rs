//! A fake Ollama for tests, load tests and demos (plan D18, phase 1.9). It answers both APIs from the golden files
//! recorded in spike S1. A request can carry a script in the `x-fake-script` header (JSON) to set the timing, the
//! number of chunks, an error status, a dropped connection or a stream that never ends. Everything it receives and
//! sends is logged, so tests can check what reached the upstream and when a connection closed.

use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::header::CONTENT_TYPE;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::Response;
use axum::serve::ListenerExt;
use bytes::Bytes;
use futures::Stream;
use serde::Deserialize;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;

pub const SCRIPT_HEADER: &str = "x-fake-script";

macro_rules! fixture {
    ($name:literal) => {
        include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/ollama/", $name))
    };
}

/// How the fake answers one request. Unset fields keep the golden file's behaviour.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Script {
    /// Answer with this status and an error body in the request's protocol.
    pub status: Option<u16>,
    /// Delay before the response headers.
    pub ttfb_ms: u64,
    /// Delay between stream chunks.
    pub gap_ms: u64,
    /// Number of content chunks in a stream (golden lines repeated as needed), before the final line(s).
    pub chunks: Option<usize>,
    /// After this many chunks, break the connection without ending the body.
    pub drop_after: Option<usize>,
    /// After this many chunks, stop sending without ending the body.
    pub hang_after: Option<usize>,
    /// Send each line in two chunks, split in the middle.
    pub split_lines: bool,
}

#[derive(Debug, Clone)]
pub enum Event {
    Request {
        id: usize,
        at: Instant,
        method: Method,
        path: String,
        headers: HeaderMap,
        body: Bytes,
    },
    ChunkSent {
        id: usize,
        at: Instant,
        bytes: Bytes,
    },
    Finished {
        id: usize,
        at: Instant,
    },
    /// The client went away before the response was complete.
    Disconnected {
        id: usize,
        at: Instant,
    },
}

#[derive(Default)]
struct Log {
    events: Vec<Event>,
    next_id: usize,
}

#[derive(Clone, Default)]
pub struct Events(Arc<Mutex<Log>>);

fn is_probe(headers: &HeaderMap) -> bool {
    headers.get("user-agent").and_then(|v| v.to_str().ok()).is_some_and(|ua| ua.starts_with(crate::server::PROBE_AGENT))
}

impl Events {
    fn push(&self, event: Event) {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).events.push(event);
    }

    fn next_id(&self) -> usize {
        let mut log = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        log.next_id += 1;
        log.next_id
    }

    pub fn all(&self) -> Vec<Event> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).events.clone()
    }

    /// The requests received for clients, oldest first: `(id, method, path, headers, body)`. Jetway's health probes
    /// are left out; [`Events::probes`] counts them.
    pub fn requests(&self) -> Vec<(usize, Method, String, HeaderMap, Bytes)> {
        self.all()
            .into_iter()
            .filter_map(|e| match e {
                Event::Request { id, method, path, headers, body, .. } if !is_probe(&headers) => {
                    Some((id, method, path, headers, body))
                }
                _ => None,
            })
            .collect()
    }

    /// Jetway's health probes received.
    pub fn probes(&self) -> usize {
        self.all().iter().filter(|e| matches!(e, Event::Request { headers, .. } if is_probe(headers))).count()
    }

    pub fn chunks_sent(&self, id: usize) -> Vec<(Instant, Bytes)> {
        self.all()
            .into_iter()
            .filter_map(|e| match e {
                Event::ChunkSent { id: i, at, bytes } if i == id => Some((at, bytes)),
                _ => None,
            })
            .collect()
    }

    pub fn disconnected_at(&self, id: usize) -> Option<Instant> {
        self.all().into_iter().find_map(|e| match e {
            Event::Disconnected { id: i, at } if i == id => Some(at),
            _ => None,
        })
    }

    /// Waits (polling) until request `id` was disconnected, up to `timeout`.
    pub async fn wait_disconnected(&self, id: usize, timeout: Duration) -> Option<Instant> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(at) = self.disconnected_at(id) {
                return Some(at);
            }
            if Instant::now() >= deadline {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
}

#[derive(Clone)]
struct FakeState {
    events: Events,
    default_script: Script,
}

/// A running fake Ollama. Dropping it stops the server.
pub struct FakeOllama {
    pub addr: SocketAddr,
    pub events: Events,
    _stop: oneshot::Sender<()>,
}

impl FakeOllama {
    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }
}

/// Starts a fake Ollama on `listener`.
pub fn spawn(listener: TcpListener, default_script: Script) -> std::io::Result<FakeOllama> {
    let addr = listener.local_addr()?;
    let events = Events::default();
    let state = FakeState { events: events.clone(), default_script };
    let (stop, stopped) = oneshot::channel::<()>();
    let app = Router::new().fallback(handle).with_state(state);
    let listener = listener.tap_io(|tcp| {
        let _ = tcp.set_nodelay(true);
    });
    tokio::spawn(async move {
        let _ = axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = stopped.await;
            })
            .await;
    });
    Ok(FakeOllama { addr, events, _stop: stop })
}

/// Starts a fake Ollama on a free port of 127.0.0.1.
pub async fn spawn_local(default_script: Script) -> std::io::Result<FakeOllama> {
    spawn(TcpListener::bind("127.0.0.1:0").await?, default_script)
}

/// Records `Disconnected` if the request is dropped (the client left) before `done` is set.
struct DisconnectGuard {
    events: Events,
    id: usize,
    done: bool,
}

impl Drop for DisconnectGuard {
    fn drop(&mut self) {
        if !self.done {
            self.events.push(Event::Disconnected { id: self.id, at: Instant::now() });
        }
    }
}

async fn handle(State(state): State<FakeState>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let body = axum::body::to_bytes(body, usize::MAX).await.unwrap_or_default();
    let id = state.events.next_id();
    let path = parts.uri.path().to_string();
    state.events.push(Event::Request {
        id,
        at: Instant::now(),
        method: parts.method.clone(),
        path: path.clone(),
        headers: parts.headers.clone(),
        body: body.clone(),
    });
    let script = parts
        .headers
        .get(SCRIPT_HEADER)
        .and_then(|v| serde_json::from_slice::<Script>(v.as_bytes()).ok())
        .unwrap_or_else(|| state.default_script.clone());

    let mut guard = DisconnectGuard { events: state.events.clone(), id, done: false };
    if script.ttfb_ms > 0 {
        tokio::time::sleep(Duration::from_millis(script.ttfb_ms)).await;
    }
    let request: serde_json::Value = serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
    let openai = path.starts_with("/v1/");

    if let Some(status) = script.status {
        guard.done = true;
        state.events.push(Event::Finished { id, at: Instant::now() });
        let status = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let body = if openai {
            serde_json::json!({"error": {"message": "scripted failure", "type": "api_error", "param": null, "code": null}})
        } else {
            serde_json::json!({"error": "scripted failure"})
        };
        return respond(status, "application/json; charset=utf-8", Body::from(body.to_string()));
    }

    let Some(reply) = reply_for(&parts.method, &path, &request) else {
        guard.done = true;
        state.events.push(Event::Finished { id, at: Instant::now() });
        return respond(StatusCode::NOT_FOUND, "text/plain", Body::from("404 page not found"));
    };
    match reply {
        Reply::Whole(content_type, body) => {
            guard.done = true;
            state.events.push(Event::ChunkSent { id, at: Instant::now(), bytes: Bytes::from(body.clone()) });
            state.events.push(Event::Finished { id, at: Instant::now() });
            respond(StatusCode::OK, content_type, Body::from(body))
        }
        Reply::Stream { content_type, items, finals } => {
            guard.done = true; // the stream's own guard takes over from here
            let stream = scripted_stream(state.events.clone(), id, &script, items, finals);
            respond(StatusCode::OK, content_type, Body::from_stream(stream))
        }
    }
}

fn respond(status: StatusCode, content_type: &'static str, body: Body) -> Response {
    let mut response = Response::new(body);
    *response.status_mut() = status;
    response.headers_mut().insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
    response
}

enum Reply {
    Whole(&'static str, String),
    /// Content items (cycled to the scripted count) and the final items that end the stream.
    Stream {
        content_type: &'static str,
        items: Vec<String>,
        finals: Vec<String>,
    },
}

const NDJSON: &str = "application/x-ndjson";
const SSE: &str = "text/event-stream";
const JSON: &str = "application/json; charset=utf-8";

/// Splits a golden stream into items that keep their terminators, then separates the final ones.
fn stream_reply(content_type: &'static str, text: &str, final_count: usize) -> Reply {
    let separator = if content_type == SSE { "\n\n" } else { "\n" };
    let mut items: Vec<String> = text.split_inclusive(separator).map(str::to_string).collect();
    let finals = items.split_off(items.len().saturating_sub(final_count));
    Reply::Stream { content_type, items, finals }
}

fn reply_for(method: &Method, path: &str, request: &serde_json::Value) -> Option<Reply> {
    let flag = |name: &str| request.get(name).and_then(serde_json::Value::as_bool);
    let model = request.get("model").and_then(serde_json::Value::as_str).unwrap_or("qwen3.5:4b");
    let has = |name: &str| request.get(name).is_some_and(|v| !v.is_null());
    // Ollama streams unless told not to; the OpenAI API streams only when asked (spike S1).
    let native_stream = flag("stream").unwrap_or(true);
    let openai_stream = flag("stream").unwrap_or(false);
    let think = flag("think").unwrap_or(false);
    Some(match path {
        "/" if method == Method::GET || method == Method::HEAD => {
            Reply::Whole("text/plain; charset=utf-8", "Ollama is running".into())
        }
        "/api/chat" if native_stream && think => stream_reply(NDJSON, fixture!("chat-think-stream.ndjson"), 1),
        "/api/chat" if native_stream => stream_reply(NDJSON, fixture!("chat-stream.ndjson"), 1),
        "/api/chat" if has("tools") => Reply::Whole(JSON, fixture!("chat-tools.json").into()),
        "/api/chat" if has("format") => Reply::Whole(JSON, fixture!("chat-format.json").into()),
        "/api/chat" if think => Reply::Whole(JSON, fixture!("chat-think.json").into()),
        "/api/chat" => Reply::Whole(JSON, fixture!("chat.json").into()),
        "/api/generate" if native_stream => stream_reply(NDJSON, fixture!("generate-stream.ndjson"), 1),
        "/api/generate" => Reply::Whole(JSON, fixture!("generate.json").into()),
        "/v1/chat/completions" if openai_stream => {
            let usage = request.pointer("/stream_options/include_usage").and_then(serde_json::Value::as_bool);
            if usage == Some(true) {
                stream_reply(SSE, fixture!("openai-chat-stream-usage.sse"), 3)
            } else {
                stream_reply(SSE, fixture!("openai-chat-stream.sse"), 2)
            }
        }
        "/v1/chat/completions" => Reply::Whole(JSON, fixture!("openai-chat.json").into()),
        "/v1/completions" if openai_stream => stream_reply(SSE, fixture!("openai-completions-stream.sse"), 2),
        "/v1/completions" => Reply::Whole(
            JSON,
            serde_json::json!({
                "id": "cmpl-1", "object": "text_completion", "created": 1_792_931_094u64, "model": model,
                "system_fingerprint": "fp_ollama",
                "choices": [{"text": "Hello there, friend!", "index": 0, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 16, "completion_tokens": 6, "total_tokens": 22}
            })
            .to_string(),
        ),
        // qwen3.5:4b can't embed (spike S1), so embeddings are synthetic: four dimensions per input.
        "/api/embed" | "/api/embeddings" => {
            let inputs = input_count(request);
            let embeddings: Vec<[f32; 4]> = (0..inputs).map(|i| [0.1 * (i as f32 + 1.0), 0.2, 0.3, 0.4]).collect();
            Reply::Whole(
                JSON,
                serde_json::json!({
                    "model": model, "embeddings": embeddings,
                    "total_duration": 14_000_000u64, "load_duration": 1_000_000u64, "prompt_eval_count": 4 * inputs
                })
                .to_string(),
            )
        }
        "/v1/embeddings" => {
            let data: Vec<serde_json::Value> = (0..input_count(request))
                .map(|i| serde_json::json!({"object": "embedding", "embedding": [0.1, 0.2, 0.3, 0.4], "index": i}))
                .collect();
            Reply::Whole(
                JSON,
                serde_json::json!({
                    "object": "list", "data": data, "model": model,
                    "usage": {"prompt_tokens": 4, "total_tokens": 4}
                })
                .to_string(),
            )
        }
        "/api/tags" => Reply::Whole(JSON, fixture!("tags.json").into()),
        "/api/ps" => Reply::Whole(JSON, fixture!("ps.json").into()),
        "/api/version" => Reply::Whole(JSON, fixture!("version.json").into()),
        "/api/show" => Reply::Whole(JSON, fixture!("show.json").into()),
        "/v1/models" => Reply::Whole(JSON, fixture!("openai-models.json").into()),
        _ => return None,
    })
}

fn input_count(request: &serde_json::Value) -> usize {
    match request.get("input").or_else(|| request.get("prompt")) {
        Some(serde_json::Value::Array(items)) => items.len(),
        _ => 1,
    }
}

/// Plays a stream according to the script on a task, so gaps happen in real time.
fn scripted_stream(events: Events, id: usize, script: &Script, items: Vec<String>, finals: Vec<String>) -> FakeBody {
    let content_count = script.chunks.unwrap_or(items.len());
    let mut sequence: Vec<String> =
        if items.is_empty() { Vec::new() } else { items.iter().cycle().take(content_count).cloned().collect() };
    sequence.extend(finals);
    let mut chunks: Vec<Bytes> = Vec::new();
    for item in sequence {
        if script.split_lines && item.len() > 1 {
            let mut middle = item.len() / 2;
            while !item.is_char_boundary(middle) {
                middle += 1;
            }
            chunks.push(Bytes::from(item[..middle].to_string()));
            chunks.push(Bytes::from(item[middle..].to_string()));
        } else {
            chunks.push(Bytes::from(item));
        }
    }

    let (tx, rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(1);
    let gap = Duration::from_millis(script.gap_ms);
    let drop_after = script.drop_after;
    let hang_after = script.hang_after;
    let task_events = events.clone();
    tokio::spawn(async move {
        for (n, chunk) in chunks.into_iter().enumerate() {
            if drop_after == Some(n) {
                let _ = tx.send(Err(std::io::Error::other("scripted connection drop"))).await;
                return;
            }
            if hang_after == Some(n) {
                tx.closed().await;
                return;
            }
            if n > 0 && !gap.is_zero() {
                tokio::time::sleep(gap).await;
            }
            let sent_at = Instant::now();
            if tx.send(Ok(chunk.clone())).await.is_err() {
                return;
            }
            task_events.push(Event::ChunkSent { id, at: sent_at, bytes: chunk });
        }
        task_events.push(Event::Finished { id, at: Instant::now() });
    });
    FakeBody { rx, guard: DisconnectGuard { events, id, done: false } }
}

struct FakeBody {
    rx: mpsc::Receiver<Result<Bytes, std::io::Error>>,
    guard: DisconnectGuard,
}

impl Stream for FakeBody {
    type Item = Result<Bytes, std::io::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let polled = self.rx.poll_recv(cx);
        match &polled {
            Poll::Ready(None) | Poll::Ready(Some(Err(_))) => self.guard.done = true,
            _ => {}
        }
        polled
    }
}
