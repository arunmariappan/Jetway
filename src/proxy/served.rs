//! Response bodies that don't come straight from the upstream (phase 3): cache hits, and clients reading a flight.
//! Each records its own exchange when it ends, or as cancelled if the client leaves first.

use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use futures::Stream;
use tokio::time::{Instant, Sleep};

use super::record::{Exchange, Finished, Outcome, Sink, Timings};
use super::tee::InflightGuard;
use crate::cache::flight::{End, Reader};
use crate::protocol::Framing;
use crate::protocol::summary::ResponseSummary;

/// What a served body needs to record its exchange.
pub struct Report {
    pub exchange: Exchange,
    pub sink: Arc<dyn Sink>,
    pub status: u16,
    pub framing: Framing,
    pub timings: Timings,
    pub inflight: InflightGuard,
}

impl Report {
    /// Records the exchange. `first` is when the first byte was handed over; `bytes` how many in all.
    pub fn finish(self, outcome: Outcome, error: Option<String>, first: Option<Duration>, bytes: u64) {
        let done = self.exchange.started.elapsed();
        let timings = Timings { first_chunk: first, first_token: first, done: Some(done), ..self.timings };
        self.sink.finished(Finished {
            exchange: self.exchange,
            status: self.status,
            outcome,
            error,
            timings,
            framing: self.framing,
            // The bytes are the cache entry's or the flight's; they aren't stored again.
            response: Bytes::new(),
            response_bytes: bytes,
            summary: ResponseSummary::default(),
            truncated: false,
            incomplete: false,
        });
        drop(self.inflight);
    }
}

/// A cache hit: its pieces, sent at once or paced.
pub struct ServedBody {
    pieces: VecDeque<Bytes>,
    gap: Duration,
    sleep: Option<Pin<Box<Sleep>>>,
    report: Option<Report>,
    first: Option<Duration>,
    bytes: u64,
}

impl ServedBody {
    pub fn new(pieces: Vec<Bytes>, gap: Duration, report: Report) -> Self {
        Self { pieces: pieces.into(), gap, sleep: None, report: Some(report), first: None, bytes: 0 }
    }

    fn end(&mut self, outcome: Outcome) {
        if let Some(report) = self.report.take() {
            report.finish(outcome, None, self.first, self.bytes);
        }
    }
}

impl Stream for ServedBody {
    type Item = Result<Bytes, std::io::Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if let Some(sleep) = &mut this.sleep {
            if sleep.as_mut().poll(cx).is_pending() {
                return Poll::Pending;
            }
            this.sleep = None;
        }
        let Some(piece) = this.pieces.pop_front() else {
            this.end(Outcome::Ok);
            return Poll::Ready(None);
        };
        if let Some(report) = &this.report {
            this.first.get_or_insert_with(|| report.exchange.started.elapsed());
        }
        this.bytes += piece.len() as u64;
        if this.pieces.is_empty() {
            // Delivered: the server may never poll again (it stops after a declared Content-Length).
            this.end(Outcome::Ok);
        } else if !this.gap.is_zero() {
            this.sleep = Some(Box::pin(tokio::time::sleep(this.gap)));
        }
        Poll::Ready(Some(Ok(piece)))
    }
}

impl Drop for ServedBody {
    fn drop(&mut self) {
        self.end(Outcome::ClientCancelled);
    }
}

/// A client reading a flight. The leader's body has no report (the upstream tee records its exchange); a joined
/// client's records its own.
pub struct FlightBody {
    reader: Reader,
    report: Option<Report>,
    first: Option<Duration>,
    bytes: u64,
    /// The response's `Content-Length`: the server stops reading once it has sent that much.
    expected_len: Option<u64>,
    ended: bool,
}

impl FlightBody {
    pub fn leader(reader: Reader) -> Self {
        Self { reader, report: None, first: None, bytes: 0, expected_len: None, ended: false }
    }

    pub fn joined(reader: Reader, report: Report, expected_len: Option<u64>) -> Self {
        Self { reader, report: Some(report), first: None, bytes: 0, expected_len, ended: false }
    }

    fn end(&mut self, outcome: Outcome, error: Option<String>) {
        self.ended = true;
        if let Some(report) = self.report.take() {
            report.finish(outcome, error, self.first, self.bytes);
        }
    }
}

impl Stream for FlightBody {
    type Item = Result<Bytes, std::io::Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if this.ended {
            return Poll::Ready(None);
        }
        match Pin::new(&mut this.reader).poll_next(cx) {
            Poll::Ready(Some(Ok(chunk))) => {
                if this.first.is_none() {
                    this.first = this.report.as_ref().map(|r| r.exchange.started.elapsed());
                }
                this.bytes += chunk.len() as u64;
                if this.expected_len.is_some_and(|len| this.bytes >= len) {
                    this.end(Outcome::Ok, None);
                }
                Poll::Ready(Some(Ok(chunk)))
            }
            Poll::Ready(Some(Err(End::Complete))) | Poll::Ready(None) => {
                this.end(Outcome::Ok, None);
                Poll::Ready(None)
            }
            Poll::Ready(Some(Err(end))) => {
                let message = match end {
                    End::Abandoned => "every client left the request this one joined",
                    _ => "the upstream stream failed",
                };
                this.end(Outcome::UpstreamError, Some(message.into()));
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl Drop for FlightBody {
    fn drop(&mut self) {
        if !self.ended {
            self.end(Outcome::ClientCancelled, None);
        }
    }
}

/// The started instant of an exchange, for callers that need "now" relative to it.
pub fn since(started: Instant) -> Duration {
    started.elapsed()
}
