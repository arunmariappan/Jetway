//! The recorder (phase 1.6): every exchange ends as exactly one [`Finished`] handed to a [`Sink`]. Response chunks
//! reach the recorder through a bounded channel, so recording never slows forwarding: when the channel is full the
//! chunk is skipped and the exchange is marked `incomplete`. Phase 2 parses and stores what the recorder collects.

use std::sync::Arc;
use std::time::Duration;

use axum::http::Method;
use bytes::{Bytes, BytesMut};
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;
use ulid::Ulid;

use crate::protocol::{Framing, Protocol};
use crate::server::routes::PathClass;

/// Recorded response bytes are capped; forwarding goes on past the cap, and the exchange is marked `truncated`.
pub const RECORD_LIMIT: usize = 8 * 1024 * 1024;
const CHANNEL_CHUNKS: usize = 1024;

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
    pub request: Bytes,
    pub traceparent: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::UpstreamError => "upstream_error",
            Self::ClientCancelled => "client_cancelled",
            Self::Timeout => "timeout",
            Self::Rejected => "rejected",
        }
    }
}

/// Offsets from [`Exchange::started`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Timings {
    pub upstream_sent: Option<Duration>,
    pub headers: Option<Duration>,
    pub first_chunk: Option<Duration>,
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
    /// The recorded response, up to [`RECORD_LIMIT`].
    pub response: Bytes,
    /// Bytes sent to the client, recorded or not.
    pub response_bytes: u64,
    pub truncated: bool,
    pub incomplete: bool,
}

pub trait Sink: Send + Sync + 'static {
    fn finished(&self, finished: Finished);
}

/// Phase 1's sink: one log line per exchange.
pub struct LogSink;

impl Sink for LogSink {
    fn finished(&self, f: Finished) {
        let ms = |d: Option<Duration>| d.map_or_else(|| "-".to_string(), |d| format!("{:.1}", d.as_secs_f64() * 1e3));
        tracing::info!(
            id = %f.exchange.id,
            app = %f.exchange.app,
            status = f.status,
            outcome = f.outcome.as_str(),
            first_chunk_ms = %ms(f.timings.first_chunk),
            total_ms = %ms(f.timings.done),
            bytes = f.response_bytes,
            error = f.error.as_deref().unwrap_or(""),
            "{} {}",
            f.exchange.method,
            f.exchange.path,
        );
    }
}

/// How an exchange ended, sent once by whoever ends it.
#[derive(Debug)]
pub struct End {
    pub status: u16,
    pub outcome: Outcome,
    pub error: Option<String>,
    pub timings: Timings,
    pub framing: Framing,
    pub response_bytes: u64,
}

/// The forwarding side's handle on one exchange's recorder task.
pub struct Recorder {
    chunks: mpsc::Sender<Bytes>,
    end: Option<oneshot::Sender<(End, bool)>>,
    skipped: bool,
}

impl Recorder {
    /// Starts the recorder task for one exchange.
    pub fn spawn(exchange: Exchange, sink: Arc<dyn Sink>) -> Self {
        let (chunks, rx) = mpsc::channel(CHANNEL_CHUNKS);
        let (end, end_rx) = oneshot::channel();
        tokio::spawn(collect(exchange, rx, end_rx, sink));
        Self { chunks, end: Some(end), skipped: false }
    }

    /// Hands a chunk to the recorder without waiting. A clone of `Bytes` is a reference count, not a copy.
    pub fn chunk(&mut self, chunk: &Bytes) {
        if self.chunks.try_send(chunk.clone()).is_err() {
            self.skipped = true;
        }
    }

    pub fn end(mut self, end: End) {
        if let Some(tx) = self.end.take() {
            let _ = tx.send((end, self.skipped));
        }
    }
}

async fn collect(
    exchange: Exchange,
    mut chunks: mpsc::Receiver<Bytes>,
    end: oneshot::Receiver<(End, bool)>,
    sink: Arc<dyn Sink>,
) {
    let mut response = BytesMut::new();
    let mut truncated = false;
    // The channel closes when the `Recorder` is dropped, which happens right after it sends the end.
    while let Some(chunk) = chunks.recv().await {
        if response.len() + chunk.len() <= RECORD_LIMIT {
            response.extend_from_slice(&chunk);
        } else {
            truncated = true;
        }
    }
    let (end, incomplete) = end.await.unwrap_or_else(|_| {
        // The forwarding side vanished without saying how it ended (a panic): count it as a cancellation.
        let end = End {
            status: 0,
            outcome: Outcome::ClientCancelled,
            error: Some("the exchange ended without a report".into()),
            timings: Timings::default(),
            framing: Framing::Whole,
            response_bytes: response.len() as u64,
        };
        (end, true)
    });
    sink.finished(Finished {
        exchange,
        status: end.status,
        outcome: end.outcome,
        error: end.error,
        timings: end.timings,
        framing: end.framing,
        response: response.freeze(),
        response_bytes: end.response_bytes,
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
    sink.finished(Finished {
        exchange,
        status,
        outcome,
        error: Some(error),
        timings,
        framing: Framing::Whole,
        response: body,
        response_bytes,
        truncated: false,
        incomplete: false,
    });
}
