//! The stream tee (plan D15, phase 1.6): forwards each upstream chunk the moment it arrives, hands a reference-counted
//! copy to the recorder, and ends the stream cleanly when the upstream fails, falls silent or the client leaves.

use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use futures::Stream;
use futures::stream::BoxStream;
use tokio::time::{Instant, Sleep};

use super::errors;
use super::inflight::InflightGuard;
use super::record::{End, Outcome, Recorder, Timings};
use super::usage::UsageFilter;
use crate::limits::scheduler::Permit;
use crate::protocol::{Framing, Protocol};

pub struct TeeBody {
    upstream: BoxStream<'static, reqwest::Result<Bytes>>,
    idle: Pin<Box<Sleep>>,
    idle_timeout: Duration,
    started: Instant,
    timings: Timings,
    status: u16,
    framing: Framing,
    protocol: Protocol,
    /// The last two bytes sent, so an error line can close a partial line first.
    tail: [u8; 2],
    tail_len: usize,
    bytes: u64,
    /// The upstream's `Content-Length`, when it sent one: the server stops reading the body after that many bytes,
    /// so the exchange is complete then, not when the stream reports its end.
    expected_len: Option<u64>,
    /// Set when Jetway asked for OpenAI usage that the client didn't (plan D21).
    usage_filter: Option<UsageFilter>,
    /// The upstream has ended; the next poll finishes the exchange.
    upstream_done: bool,
    recorder: Option<Recorder>,
    /// The upstream slot (phase 9), freed as soon as the upstream's part ends rather than when the body is dropped.
    slot: Option<Permit>,
    _inflight: InflightGuard,
}

pub struct TeeParts {
    pub upstream: BoxStream<'static, reqwest::Result<Bytes>>,
    pub idle_timeout: Duration,
    pub started: Instant,
    pub timings: Timings,
    pub status: u16,
    pub framing: Framing,
    pub protocol: Protocol,
    pub expected_len: Option<u64>,
    pub usage_filter: Option<UsageFilter>,
    pub recorder: Recorder,
    pub slot: Option<Permit>,
    pub inflight: InflightGuard,
}

impl TeeBody {
    pub fn new(parts: TeeParts) -> Self {
        let mut tee = Self {
            upstream: parts.upstream,
            idle: Box::pin(tokio::time::sleep(parts.idle_timeout)),
            idle_timeout: parts.idle_timeout,
            started: parts.started,
            timings: parts.timings,
            status: parts.status,
            framing: parts.framing,
            protocol: parts.protocol,
            tail: [0; 2],
            tail_len: 0,
            bytes: 0,
            expected_len: parts.expected_len,
            usage_filter: parts.usage_filter,
            upstream_done: false,
            recorder: Some(parts.recorder),
            slot: parts.slot,
            _inflight: parts.inflight,
        };
        if tee.expected_len == Some(0) {
            tee.finish(Outcome::Ok, None);
        }
        tee
    }

    fn remember_tail(&mut self, chunk: &[u8]) {
        match chunk.len() {
            0 => {}
            1 => {
                self.tail = [self.tail[1], chunk[0]];
                self.tail_len = (self.tail_len + 1).min(2);
            }
            n => {
                self.tail = [chunk[n - 2], chunk[n - 1]];
                self.tail_len = 2;
            }
        }
    }

    fn forward(&mut self, chunk: &Bytes) {
        let now = Instant::now();
        if self.timings.first_chunk.is_none() {
            self.timings.first_chunk = Some(now.duration_since(self.started));
        }
        self.bytes += chunk.len() as u64;
        self.remember_tail(chunk);
        if let Some(recorder) = &mut self.recorder {
            recorder.sent(now, chunk);
        }
    }

    /// Ends the exchange once; later calls do nothing.
    fn finish(&mut self, outcome: Outcome, error: Option<String>) {
        self.slot = None;
        if let Some(recorder) = self.recorder.take() {
            self.timings.done = Some(self.started.elapsed());
            recorder.end(End {
                status: self.status,
                outcome,
                error,
                timings: self.timings,
                response_bytes: self.bytes,
            });
        }
    }

    /// Ends the stream after a failure: an error line in the stream's framing, if it has one.
    fn fail(&mut self, outcome: Outcome, message: String) -> Option<Bytes> {
        let tail = &self.tail[2 - self.tail_len..];
        let line = errors::stream_error(self.framing, self.protocol, tail, &message);
        if let Some(line) = &line {
            self.forward(line);
        }
        self.finish(outcome, Some(message));
        line
    }
}

impl Stream for TeeBody {
    type Item = Result<Bytes, std::io::Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if this.recorder.is_none() {
            return Poll::Ready(None);
        }
        if this.upstream_done {
            this.finish(Outcome::Ok, None);
            return Poll::Ready(None);
        }
        loop {
            match this.upstream.as_mut().poll_next(cx) {
                Poll::Ready(Some(Ok(mut chunk))) => {
                    let deadline = Instant::now() + this.idle_timeout;
                    this.idle.as_mut().reset(deadline);
                    if let Some(filter) = &mut this.usage_filter {
                        let filtered = filter.push(chunk);
                        if let Some(recorder) = &mut this.recorder {
                            for event in filtered.hidden {
                                recorder.hidden(event);
                            }
                        }
                        if filtered.sent.is_empty() {
                            continue;
                        }
                        chunk = filtered.sent;
                    }
                    this.forward(&chunk);
                    if this.expected_len.is_some_and(|len| this.bytes >= len) {
                        this.finish(Outcome::Ok, None);
                    }
                    return Poll::Ready(Some(Ok(chunk)));
                }
                Poll::Ready(Some(Err(error))) => {
                    let message = format!("the upstream stream failed: {error}");
                    return Poll::Ready(this.fail(Outcome::UpstreamError, message).map(Ok));
                }
                Poll::Ready(None) => {
                    let rest = this.usage_filter.as_mut().map(UsageFilter::finish).unwrap_or_default();
                    if rest.is_empty() {
                        this.finish(Outcome::Ok, None);
                        return Poll::Ready(None);
                    }
                    this.forward(&rest);
                    this.upstream_done = true;
                    return Poll::Ready(Some(Ok(rest)));
                }
                Poll::Pending => {
                    return match this.idle.as_mut().poll(cx) {
                        Poll::Ready(()) => {
                            let message = format!("no data from the upstream for {} s", this.idle_timeout.as_secs());
                            Poll::Ready(this.fail(Outcome::Timeout, message).map(Ok))
                        }
                        Poll::Pending => Poll::Pending,
                    };
                }
            }
        }
    }
}

impl Drop for TeeBody {
    /// The server drops the body when the client disconnects; dropping `upstream` with it closes the upstream
    /// connection, which makes Ollama stop generating (spike S1).
    fn drop(&mut self) {
        self.finish(Outcome::ClientCancelled, None);
    }
}
