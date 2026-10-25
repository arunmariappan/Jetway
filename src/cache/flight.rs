//! Joined requests (plan D14, phase 3.5). A cacheable request that isn't a hit becomes a *flight*: a task reads the
//! upstream response and keeps every chunk, and each client (the first and any that join while it runs) reads from
//! the flight. The upstream call goes on while at least one client still reads; when the last one leaves, the task
//! drops the upstream stream, which stops Ollama.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll, Waker};

use axum::http::{HeaderMap, StatusCode};
use bytes::Bytes;
use futures::{Stream, StreamExt};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use super::key::CacheKey;

/// How the flight's upstream request started.
#[derive(Debug, Clone)]
pub enum Start {
    /// Headers arrived; chunks follow.
    Response { status: StatusCode, headers: HeaderMap },
    /// It failed before any response; joiners get the same error.
    Failed { status: StatusCode, headers: HeaderMap, body: Bytes },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum End {
    Complete,
    /// The upstream failed or fell silent; the error line, if any, is the last chunk.
    Failed,
    /// Every client left.
    Abandoned,
}

#[derive(Default)]
struct State {
    chunks: Vec<Bytes>,
    end: Option<End>,
    wakers: Vec<Waker>,
}

pub struct Flight {
    pub leader: ulid::Ulid,
    start: watch::Sender<Option<Start>>,
    state: Mutex<State>,
    readers: AtomicUsize,
    /// Cancelled when the last reader leaves before the end.
    pub abandoned: CancellationToken,
}

impl Flight {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn started(&self, start: Start) {
        self.start.send_replace(Some(start));
    }

    /// Waits for the upstream's response (or failure).
    pub async fn wait_start(&self) -> Option<Start> {
        let mut rx = self.start.subscribe();
        let started = rx.wait_for(Option::is_some).await.ok()?;
        started.clone()
    }

    pub fn push(&self, chunk: Bytes) {
        let wakers = {
            let mut state = self.lock();
            state.chunks.push(chunk);
            std::mem::take(&mut state.wakers)
        };
        for waker in wakers {
            waker.wake();
        }
    }

    pub fn finish(&self, end: End) {
        let wakers = {
            let mut state = self.lock();
            state.end.get_or_insert(end);
            std::mem::take(&mut state.wakers)
        };
        for waker in wakers {
            waker.wake();
        }
    }

    pub fn end(&self) -> Option<End> {
        self.lock().end
    }

    /// A new reader, unless the flight has already been abandoned.
    pub fn reader(self: &Arc<Self>) -> Option<Reader> {
        if self.abandoned.is_cancelled() {
            return None;
        }
        self.readers.fetch_add(1, Ordering::AcqRel);
        Some(Reader { flight: Arc::clone(self), next: 0, done: false })
    }
}

/// One client's view of a flight: every chunk from the start, then the live tail.
pub struct Reader {
    flight: Arc<Flight>,
    next: usize,
    done: bool,
}

impl Reader {
    pub fn flight(&self) -> &Arc<Flight> {
        &self.flight
    }
}

impl Stream for Reader {
    /// A chunk, or how the flight ended (`Err`), after which the stream is over.
    type Item = Result<Bytes, End>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if this.done {
            return Poll::Ready(None);
        }
        let mut state = this.flight.lock();
        if let Some(chunk) = state.chunks.get(this.next) {
            this.next += 1;
            return Poll::Ready(Some(Ok(chunk.clone())));
        }
        if let Some(end) = state.end {
            this.done = true;
            return Poll::Ready(Some(Err(end)));
        }
        state.wakers.push(cx.waker().clone());
        Poll::Pending
    }
}

impl Drop for Reader {
    fn drop(&mut self) {
        let left = self.flight.readers.fetch_sub(1, Ordering::AcqRel) - 1;
        if left == 0 && self.flight.end().is_none() {
            self.flight.abandoned.cancel();
        }
    }
}

/// Flights by join key.
#[derive(Default)]
pub struct Flights {
    map: Mutex<HashMap<CacheKey, Arc<Flight>>>,
}

pub enum Joined {
    /// This request leads a new flight.
    Leader(Arc<Flight>),
    /// This request reads an existing one.
    Follower(Reader),
}

impl Flights {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<CacheKey, Arc<Flight>>> {
        self.map.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Joins the flight for `key`, or starts one led by `leader`.
    pub fn join_or_lead(&self, key: CacheKey, leader: ulid::Ulid) -> Joined {
        let mut map = self.lock();
        if let Some(flight) = map.get(&key)
            && let Some(reader) = flight.reader()
        {
            return Joined::Follower(reader);
        }
        let flight = Arc::new(Flight {
            leader,
            start: watch::Sender::new(None),
            state: Mutex::new(State::default()),
            readers: AtomicUsize::new(0),
            abandoned: CancellationToken::new(),
        });
        map.insert(key, Arc::clone(&flight));
        Joined::Leader(flight)
    }

    /// Removes a finished flight, if it is still the one registered for `key`.
    pub fn remove(&self, key: &CacheKey, flight: &Arc<Flight>) {
        let mut map = self.lock();
        if map.get(key).is_some_and(|f| Arc::ptr_eq(f, flight)) {
            map.remove(key);
        }
    }

    pub fn len(&self) -> usize {
        self.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Drives a flight: reads `upstream` (the leader's tee, which records the upstream exchange) into the flight until it
/// ends or every reader has left.
pub async fn fly<S>(flight: Arc<Flight>, mut upstream: S)
where
    S: Stream<Item = Result<Bytes, std::io::Error>> + Unpin,
{
    let end = loop {
        tokio::select! {
            item = upstream.next() => match item {
                Some(Ok(chunk)) => flight.push(chunk),
                Some(Err(_)) => break End::Failed,
                None => break End::Complete,
            },
            () = flight.abandoned.cancelled() => break End::Abandoned,
        }
    };
    // Dropping the tee here ends the upstream exchange (as cancelled, when abandoned).
    drop(upstream);
    flight.finish(end);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(n: u8) -> CacheKey {
        CacheKey([n; 32])
    }

    #[tokio::test]
    async fn followers_get_everything_from_the_start() {
        let flights = Flights::default();
        let Joined::Leader(flight) = flights.join_or_lead(key(1), ulid::Ulid::nil()) else {
            panic!("expected to lead")
        };
        let mut first = flight.reader().unwrap();
        flight.push(Bytes::from_static(b"a"));
        let Joined::Follower(mut second) = flights.join_or_lead(key(1), ulid::Ulid::nil()) else {
            panic!("expected to join")
        };
        flight.push(Bytes::from_static(b"b"));
        flight.finish(End::Complete);
        for reader in [&mut first, &mut second] {
            let items: Vec<_> = reader.collect::<Vec<_>>().await;
            assert_eq!(items, vec![Ok(Bytes::from_static(b"a")), Ok(Bytes::from_static(b"b")), Err(End::Complete)]);
        }
    }

    #[tokio::test]
    async fn the_last_reader_leaving_abandons_the_flight() {
        let flights = Flights::default();
        let Joined::Leader(flight) = flights.join_or_lead(key(2), ulid::Ulid::nil()) else { panic!() };
        let first = flight.reader().unwrap();
        let second = flight.reader().unwrap();
        drop(first);
        assert!(!flight.abandoned.is_cancelled());
        drop(second);
        assert!(flight.abandoned.is_cancelled());
        // An abandoned flight takes no new readers; the next identical request leads a new one.
        assert!(matches!(flights.join_or_lead(key(2), ulid::Ulid::nil()), Joined::Leader(_)));
    }

    #[tokio::test]
    async fn fly_stops_when_abandoned() {
        let flights = Flights::default();
        let Joined::Leader(flight) = flights.join_or_lead(key(3), ulid::Ulid::nil()) else { panic!() };
        let reader = flight.reader().unwrap();
        let upstream = futures::stream::pending::<Result<Bytes, std::io::Error>>();
        let task = tokio::spawn(fly(Arc::clone(&flight), upstream));
        drop(reader);
        tokio::time::timeout(std::time::Duration::from_secs(1), task).await.unwrap().unwrap();
        assert_eq!(flight.end(), Some(End::Abandoned));
    }
}
