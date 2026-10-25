//! The recorder (phases 1.6 and 2.1): every exchange ends as exactly one [`Finished`] handed to a [`Sink`].
//! Response pieces reach the recorder through a bounded channel, so recording never slows forwarding: when the
//! channel is full a piece is skipped and the exchange is marked `incomplete`. The recorder decodes lines or events
//! as they arrive, which times the first answer and thinking tokens and fills the response summary.

use std::sync::Arc;
use std::time::Duration;

use axum::http::Method;
use bytes::{Bytes, BytesMut};
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;
use ulid::Ulid;

use crate::cache::CacheNote;
use crate::protocol::decode::{NdjsonDecoder, SseDecoder};
use crate::protocol::summary::{RequestSummary, ResponseSummary};
use crate::protocol::{Framing, Protocol};
use crate::server::routes::PathClass;
use crate::telemetry::SpanIds;

/// Recorded response bytes are capped; forwarding goes on past the cap, and the exchange is marked `truncated`.
pub const RECORD_LIMIT: usize = 8 * 1024 * 1024;
const CHANNEL_PIECES: usize = 1024;

/// What is known about an exchange when its request has arrived.
#[derive(Debug, Clone)]
pub struct Exchange {
    pub id: Ulid,
    pub app: String,
    pub method: Method,
    /// The path after any `/a/{app}` prefix, with the query string.
    pub path: String,
    pub protocol: Protocol,
    pub class: PathClass,
    pub started: Instant,
    pub started_at: jiff::Timestamp,
    /// The request body as the client sent it.
    pub request: Bytes,
    /// Parsed for generation and embedding requests.
    pub summary: Option<RequestSummary>,
    pub traceparent: Option<String>,
    /// Set when spans are exported (`otel.endpoint`).
    pub spans: Option<SpanIds>,
    /// What the cache did, for generation and embedding requests.
    pub cache: Option<CacheNote>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Outcome {
    /// The upstream's complete response reached the client.
    Ok,
    /// Unreachable, a failed request, or a failure mid-stream.
    UpstreamError,
    /// The client went away before the end.
    ClientCancelled,
    /// No first byte, or a silence between chunks longer than the idle timeout.
    Timeout,
    /// Refused by Jetway before reaching the upstream (unknown path, not allowed, too large).
    Rejected,
}

impl Outcome {
    pub const ALL: [Outcome; 5] = [Self::Ok, Self::UpstreamError, Self::ClientCancelled, Self::Timeout, Self::Rejected];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::UpstreamError => "upstream_error",
            Self::ClientCancelled => "client_cancelled",
            Self::Timeout => "timeout",
            Self::Rejected => "rejected",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|o| o.as_str() == text)
    }
}

/// Offsets from [`Exchange::started`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Timings {
    pub upstream_sent: Option<Duration>,
    pub headers: Option<Duration>,
    /// The first response byte forwarded to the client.
    pub first_chunk: Option<Duration>,
    /// The first piece of the answer (content or a tool call), as the client receives it.
    pub first_token: Option<Duration>,
    pub first_thinking: Option<Duration>,
    pub done: Option<Duration>,
}

/// One finished exchange.
#[derive(Debug, Clone)]
pub struct Finished {
    pub exchange: Exchange,
    /// The status the client received.
    pub status: u16,
    pub outcome: Outcome,
    pub error: Option<String>,
    pub timings: Timings,
    pub framing: Framing,
    /// The recorded response as the client received it, up to [`RECORD_LIMIT`].
    pub response: Bytes,
    /// Bytes sent to the client, recorded or not.
    pub response_bytes: u64,
    pub summary: ResponseSummary,
    pub truncated: bool,
    pub incomplete: bool,
}

pub trait Sink: Send + Sync + 'static {
    fn finished(&self, finished: Finished);
}

/// One log line per exchange (`jetway serve` adds it next to the store).
pub struct LogSink;

impl Sink for LogSink {
    fn finished(&self, f: Finished) {
        let ms = |d: Option<Duration>| d.map_or_else(|| "-".to_string(), |d| format!("{:.1}", d.as_secs_f64() * 1e3));
        let tokens = |n: Option<u64>| n.map_or_else(|| "-".to_string(), |n| n.to_string());
        tracing::info!(
            id = %f.exchange.id,
            app = %f.exchange.app,
            status = f.status,
            outcome = f.outcome.as_str(),
            first_token_ms = %ms(f.timings.first_token),
            total_ms = %ms(f.timings.done),
            tokens_in = %tokens(f.summary.prompt_tokens),
            tokens_out = %tokens(f.summary.completion_tokens),
            error = f.error.as_deref().unwrap_or(""),
            "{} {}",
            f.exchange.method,
            f.exchange.path,
        );
    }
}

/// Several sinks in turn.
pub struct Sinks(pub Vec<Arc<dyn Sink>>);

impl Sink for Sinks {
    fn finished(&self, finished: Finished) {
        if let Some((last, rest)) = self.0.split_last() {
            for sink in rest {
                sink.finished(finished.clone());
            }
            last.finished(finished);
        }
    }
}

/// How an exchange ended, sent once by whoever ends it.
#[derive(Debug)]
pub struct End {
    pub status: u16,
    pub outcome: Outcome,
    pub error: Option<String>,
    pub timings: Timings,
    pub response_bytes: u64,
}

enum Piece {
    /// Bytes the client received, and when.
    Sent(Instant, Bytes),
    /// A complete event Jetway removed from the stream (OpenAI usage the client didn't ask for); parsed, not stored.
    Hidden(Bytes),
}

/// The forwarding side's handle on one exchange's recorder task.
pub struct Recorder {
    pieces: mpsc::Sender<Piece>,
    end: Option<oneshot::Sender<(End, bool)>>,
    skipped: bool,
}

impl Recorder {
    /// Starts the recorder task for one exchange whose response has the given framing.
    pub fn spawn(exchange: Exchange, framing: Framing, sink: Arc<dyn Sink>) -> Self {
        let (pieces, rx) = mpsc::channel(CHANNEL_PIECES);
        let (end, end_rx) = oneshot::channel();
        tokio::spawn(collect(exchange, framing, rx, end_rx, sink));
        Self { pieces, end: Some(end), skipped: false }
    }

    /// Hands sent bytes to the recorder without waiting. A clone of `Bytes` is a reference count, not a copy.
    pub fn sent(&mut self, at: Instant, chunk: &Bytes) {
        if self.pieces.try_send(Piece::Sent(at, chunk.clone())).is_err() {
            self.skipped = true;
        }
    }

    pub fn hidden(&mut self, event: Bytes) {
        if self.pieces.try_send(Piece::Hidden(event)).is_err() {
            self.skipped = true;
        }
    }

    pub fn end(mut self, end: End) {
        if let Some(tx) = self.end.take() {
            let _ = tx.send((end, self.skipped));
        }
    }
}

/// Splits recorded bytes into lines or events and feeds the summary.
enum Decoder {
    Ndjson(NdjsonDecoder),
    Sse(SseDecoder),
    Whole(BytesMut),
}

impl Decoder {
    fn new(framing: Framing) -> Self {
        match framing {
            Framing::Ndjson => Self::Ndjson(NdjsonDecoder::default()),
            Framing::Sse => Self::Sse(SseDecoder::default()),
            Framing::Whole => Self::Whole(BytesMut::new()),
        }
    }

    fn push(&mut self, chunk: &[u8], items: &mut Vec<Vec<u8>>) {
        match self {
            Self::Ndjson(d) => d.push(chunk, items),
            Self::Sse(d) => d.push(chunk, items),
            Self::Whole(buffer) => {
                if buffer.len() + chunk.len() <= RECORD_LIMIT {
                    buffer.extend_from_slice(chunk);
                }
            }
        }
    }

    fn finish(&mut self) -> Option<Vec<u8>> {
        match self {
            Self::Ndjson(d) => d.finish(),
            Self::Sse(d) => d.finish(),
            Self::Whole(buffer) => (!buffer.is_empty()).then(|| buffer.to_vec()),
        }
    }
}

struct Collector {
    protocol: Protocol,
    started: Instant,
    decoder: Decoder,
    items: Vec<Vec<u8>>,
    summary: ResponseSummary,
    first_token: Option<Duration>,
    first_thinking: Option<Duration>,
}

impl Collector {
    fn feed(&mut self, at: Option<Instant>) {
        for item in self.items.drain(..) {
            let fed = self.summary.feed(self.protocol, &item);
            if let Some(at) = at {
                let offset = at.duration_since(self.started);
                if fed.content && self.first_token.is_none() {
                    self.first_token = Some(offset);
                }
                if fed.thinking && self.first_thinking.is_none() {
                    self.first_thinking = Some(offset);
                }
            }
        }
    }
}

async fn collect(
    exchange: Exchange,
    framing: Framing,
    mut pieces: mpsc::Receiver<Piece>,
    end: oneshot::Receiver<(End, bool)>,
    sink: Arc<dyn Sink>,
) {
    let mut response = BytesMut::new();
    let mut truncated = false;
    let mut collector = Collector {
        protocol: exchange.protocol,
        started: exchange.started,
        decoder: Decoder::new(framing),
        items: Vec::new(),
        summary: ResponseSummary::default(),
        first_token: None,
        first_thinking: None,
    };
    let mut last_at = None;
    // The channel closes when the `Recorder` is dropped, which happens right after it sends the end.
    while let Some(piece) = pieces.recv().await {
        match piece {
            Piece::Sent(at, chunk) => {
                if response.len() + chunk.len() <= RECORD_LIMIT {
                    response.extend_from_slice(&chunk);
                } else {
                    truncated = true;
                }
                collector.decoder.push(&chunk, &mut collector.items);
                collector.feed(Some(at));
                last_at = Some(at);
            }
            Piece::Hidden(event) => {
                let mut events = Vec::new();
                let mut decoder = SseDecoder::default();
                decoder.push(&event, &mut events);
                events.extend(decoder.finish());
                for item in events {
                    collector.summary.feed(collector.protocol, &item);
                }
            }
        }
    }
    if let Some(rest) = collector.decoder.finish() {
        collector.items.push(rest);
        collector.feed(last_at);
    }

    let (end, incomplete) = end.await.unwrap_or_else(|_| {
        // The forwarding side vanished without saying how it ended (a panic): count it as a cancellation.
        let end = End {
            status: 0,
            outcome: Outcome::ClientCancelled,
            error: Some("the exchange ended without a report".into()),
            timings: Timings::default(),
            response_bytes: response.len() as u64,
        };
        (end, true)
    });
    let timings =
        Timings { first_token: collector.first_token, first_thinking: collector.first_thinking, ..end.timings };
    sink.finished(Finished {
        exchange,
        status: end.status,
        outcome: end.outcome,
        error: end.error,
        timings,
        framing,
        response: response.freeze(),
        response_bytes: end.response_bytes,
        summary: collector.summary,
        truncated,
        incomplete,
    });
}

/// Ends an exchange that never got an upstream response (rejected, unreachable, timed out).
pub fn finish_without_response(
    sink: &dyn Sink,
    exchange: Exchange,
    status: u16,
    outcome: Outcome,
    error: String,
    mut timings: Timings,
    body: Bytes,
) {
    timings.done = Some(exchange.started.elapsed());
    let response_bytes = body.len() as u64;
    let mut summary = ResponseSummary::default();
    summary.feed(exchange.protocol, &body);
    sink.finished(Finished {
        exchange,
        status,
        outcome,
        error: Some(error),
        timings,
        framing: Framing::Whole,
        response: body,
        response_bytes,
        summary,
        truncated: false,
        incomplete: false,
    });
}
