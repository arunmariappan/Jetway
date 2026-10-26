//! The upstream queue (plan D14, phase 9.3). At most `slots` requests go upstream at once; the rest wait by
//! priority (interactive, then normal, then batch, first come first served within a class), and a waiting request
//! moves up one class for every `aging` it has waited, so batch work isn't starved. A request that would wait longer
//! than its class allows is refused (load shedding); one whose client leaves while it waits simply drops out.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use tokio::sync::oneshot;
use tokio::time::Instant;

use crate::config::Priority;

/// A slot handed to a waiter; it becomes a [`Permit`] once the waiter takes it. A bare grant has no `Drop`, so the
/// scheduler can get it back from a waiter that has gone without releasing anything twice.
struct Grant;

struct Waiter {
    seq: u64,
    priority: Priority,
    since: Instant,
    tx: oneshot::Sender<Grant>,
}

#[derive(Default)]
struct State {
    in_use: usize,
    waiters: Vec<Waiter>,
    next_seq: u64,
    /// The most in use and waiting since the last sample, for the dashboard's depth chart.
    peak: Depth,
}

impl State {
    fn depth(&self, slots: usize) -> Depth {
        let count = |p: Priority| self.waiters.iter().filter(|w| w.priority == p).count();
        Depth {
            in_use: self.in_use,
            slots,
            interactive: count(Priority::Interactive),
            normal: count(Priority::Normal),
            batch: count(Priority::Batch),
        }
    }

    fn note_peak(&mut self, slots: usize) {
        let now = self.depth(slots);
        let peak = &mut self.peak;
        peak.in_use = peak.in_use.max(now.in_use);
        peak.interactive = peak.interactive.max(now.interactive);
        peak.normal = peak.normal.max(now.normal);
        peak.batch = peak.batch.max(now.batch);
    }
}

/// Samples kept for the depth chart: an hour at one every 10 seconds.
const HISTORY: usize = 360;

pub struct Scheduler {
    slots: usize,
    aging: Duration,
    state: Mutex<State>,
    history: Mutex<VecDeque<(i64, Depth)>>,
}

/// An upstream slot; dropping it frees the slot for the next waiter.
pub struct Permit {
    scheduler: Arc<Scheduler>,
}

impl Drop for Permit {
    fn drop(&mut self) {
        let mut state = self.scheduler.lock();
        state.in_use = state.in_use.saturating_sub(1);
        self.scheduler.dispatch(&mut state);
    }
}

/// The request waited `waited` without getting a slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Shed {
    pub waited: Duration,
}

/// Queue depth by class, for the dashboard and metrics.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct Depth {
    pub in_use: usize,
    pub slots: usize,
    pub interactive: usize,
    pub normal: usize,
    pub batch: usize,
}

/// Removes a waiter whose future is dropped; if its grant had already been sent, frees that slot.
struct WaitGuard {
    scheduler: Arc<Scheduler>,
    seq: u64,
    armed: bool,
}

impl Drop for WaitGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let mut state = self.scheduler.lock();
        let before = state.waiters.len();
        state.waiters.retain(|w| w.seq != self.seq);
        if state.waiters.len() == before {
            // Granted but never taken: give the slot to someone else.
            state.in_use = state.in_use.saturating_sub(1);
            self.scheduler.dispatch(&mut state);
        }
    }
}

impl Scheduler {
    pub fn new(slots: usize, aging: Duration) -> Arc<Self> {
        Arc::new(Self {
            slots: slots.max(1),
            aging: aging.max(Duration::from_millis(1)),
            state: Mutex::new(State::default()),
            history: Mutex::new(VecDeque::with_capacity(HISTORY)),
        })
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Hands free slots to the best waiters: the lowest class rank after aging, then the earliest.
    fn dispatch(&self, state: &mut State) {
        while state.in_use < self.slots && !state.waiters.is_empty() {
            let now = Instant::now();
            let aging = self.aging.as_secs_f64();
            let score = |w: &Waiter| w.priority.rank() as f64 - now.duration_since(w.since).as_secs_f64() / aging;
            let best = state
                .waiters
                .iter()
                .enumerate()
                .min_by(|(_, a), (_, b)| score(a).total_cmp(&score(b)).then(a.seq.cmp(&b.seq)))
                .map(|(i, _)| i);
            let Some(index) = best else { break };
            let waiter = state.waiters.remove(index);
            state.in_use += 1;
            if waiter.tx.send(Grant).is_err() {
                // The waiter is gone; its grant came back, so the slot is still free.
                state.in_use -= 1;
            }
        }
    }

    /// Waits for a slot for up to `max_wait`. Returns the permit and how long it waited.
    pub async fn acquire(self: &Arc<Self>, priority: Priority, max_wait: Duration) -> Result<(Permit, Duration), Shed> {
        let started = Instant::now();
        let (rx, seq) = {
            let mut state = self.lock();
            if state.in_use < self.slots && state.waiters.is_empty() {
                state.in_use += 1;
                state.note_peak(self.slots);
                return Ok((Permit { scheduler: Arc::clone(self) }, Duration::ZERO));
            }
            let (tx, rx) = oneshot::channel();
            let seq = state.next_seq;
            state.next_seq += 1;
            state.waiters.push(Waiter { seq, priority, since: started, tx });
            state.note_peak(self.slots);
            (rx, seq)
        };
        let mut guard = WaitGuard { scheduler: Arc::clone(self), seq, armed: true };
        let mut rx = rx;
        let granted = match tokio::time::timeout(max_wait, &mut rx).await {
            Ok(Ok(Grant)) => true,
            Ok(Err(_)) => false,
            Err(_) => {
                // Timed out: if the grant raced in, take it; otherwise leave the queue.
                let mut state = self.lock();
                let still_waiting = state.waiters.iter().any(|w| w.seq == seq);
                if still_waiting {
                    state.waiters.retain(|w| w.seq != seq);
                    false
                } else {
                    drop(state);
                    rx.try_recv().is_ok()
                }
            }
        };
        guard.armed = false;
        if granted {
            Ok((Permit { scheduler: Arc::clone(self) }, started.elapsed()))
        } else {
            Err(Shed { waited: started.elapsed() })
        }
    }

    pub fn depth(&self) -> Depth {
        self.lock().depth(self.slots)
    }

    /// Keeps the peak depth since the last sample, stamped `at_ms`, and starts the next one from now.
    pub fn sample(&self, at_ms: i64) {
        let peak = {
            let mut state = self.lock();
            let now = state.depth(self.slots);
            Depth { slots: self.slots, ..std::mem::replace(&mut state.peak, now) }
        };
        let mut history = self.history.lock().unwrap_or_else(PoisonError::into_inner);
        if history.len() == HISTORY {
            history.pop_front();
        }
        history.push_back((at_ms, peak));
    }

    /// The samples, oldest first.
    pub fn history(&self) -> Vec<(i64, Depth)> {
        self.history.lock().unwrap_or_else(PoisonError::into_inner).iter().copied().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LONG: Duration = Duration::from_secs(60);

    #[tokio::test(start_paused = true)]
    async fn priority_order_when_a_slot_frees() {
        let s = Scheduler::new(1, Duration::from_secs(30));
        let (first, _) = s.acquire(Priority::Normal, LONG).await.unwrap();
        let order = Arc::new(Mutex::new(Vec::new()));
        let mut tasks = Vec::new();
        for (name, priority) in
            [("batch", Priority::Batch), ("normal", Priority::Normal), ("interactive", Priority::Interactive)]
        {
            let (s, order) = (Arc::clone(&s), Arc::clone(&order));
            tasks.push(tokio::spawn(async move {
                let (permit, _) = s.acquire(priority, LONG).await.unwrap();
                order.lock().unwrap().push(name);
                tokio::time::sleep(Duration::from_millis(10)).await;
                drop(permit);
            }));
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        assert_eq!(s.depth().batch + s.depth().normal + s.depth().interactive, 3);
        drop(first);
        for task in tasks {
            task.await.unwrap();
        }
        assert_eq!(*order.lock().unwrap(), ["interactive", "normal", "batch"]);
        assert_eq!(s.depth().in_use, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn waiting_long_enough_beats_a_newer_higher_class() {
        let s = Scheduler::new(1, Duration::from_secs(30));
        let (first, _) = s.acquire(Priority::Normal, LONG).await.unwrap();
        let order = Arc::new(Mutex::new(Vec::new()));
        let take = |name: &'static str, priority: Priority| {
            let (s, order) = (Arc::clone(&s), Arc::clone(&order));
            tokio::spawn(async move {
                let (permit, waited) = s.acquire(priority, Duration::from_secs(600)).await.unwrap();
                order.lock().unwrap().push((name, waited));
                tokio::time::sleep(Duration::from_millis(10)).await;
                drop(permit);
            })
        };
        let batch = take("batch", Priority::Batch);
        // After 70 s the batch request has aged past two classes (30 s each), ahead of a fresh interactive one.
        tokio::time::sleep(Duration::from_secs(70)).await;
        let interactive = take("interactive", Priority::Interactive);
        tokio::time::sleep(Duration::from_millis(5)).await;
        drop(first);
        batch.await.unwrap();
        interactive.await.unwrap();
        let order = order.lock().unwrap();
        assert_eq!(order.iter().map(|(name, _)| *name).collect::<Vec<_>>(), ["batch", "interactive"]);
        assert!(order[0].1 >= Duration::from_secs(70));
    }

    #[tokio::test(start_paused = true)]
    async fn too_long_a_wait_is_shed_and_leaving_frees_the_place() {
        let s = Scheduler::new(1, Duration::from_secs(30));
        let (first, _) = s.acquire(Priority::Normal, LONG).await.unwrap();
        let shed = s.acquire(Priority::Batch, Duration::from_secs(5)).await.err().unwrap();
        assert_eq!(shed.waited, Duration::from_secs(5));
        assert_eq!(s.depth().batch, 0, "a shed request leaves the queue");

        // A waiter dropped before its turn (its client left) doesn't take the slot.
        let waiting = tokio::spawn({
            let s = Arc::clone(&s);
            async move { s.acquire(Priority::Normal, LONG).await.map(|_| ()) }
        });
        tokio::time::sleep(Duration::from_millis(5)).await;
        waiting.abort();
        let _ = waiting.await;
        assert_eq!(s.depth().normal, 0);
        drop(first);
        assert_eq!(s.depth().in_use, 0);
        assert!(s.acquire(Priority::Batch, Duration::from_millis(1)).await.is_ok(), "the slot is free again");
    }

    #[tokio::test(start_paused = true)]
    async fn samples_keep_the_peak_between_them() {
        let s = Scheduler::new(1, Duration::from_secs(30));
        let first = s.acquire(Priority::Normal, LONG).await.unwrap();
        let waiting = tokio::spawn({
            let s = Arc::clone(&s);
            async move { s.acquire(Priority::Batch, Duration::from_secs(1)).await.map(|_| ()) }
        });
        tokio::time::sleep(Duration::from_millis(5)).await;
        let _ = waiting.await;
        drop(first);
        // Nothing waits or runs now, but a batch request waited and a slot was in use since the last sample.
        s.sample(1_000);
        s.sample(2_000);
        let history = s.history();
        assert_eq!((history[0].1.batch, history[0].1.in_use, history[0].1.slots), (1, 1, 1));
        assert_eq!((history[1].0, history[1].1.batch, history[1].1.in_use), (2_000, 0, 0));
    }

    #[tokio::test]
    async fn several_slots() {
        let s = Scheduler::new(2, Duration::from_secs(30));
        let a = s.acquire(Priority::Normal, LONG).await.unwrap();
        let b = s.acquire(Priority::Normal, LONG).await.unwrap();
        assert_eq!(s.depth().in_use, 2);
        assert!(s.acquire(Priority::Normal, Duration::from_millis(20)).await.is_err());
        drop((a, b));
        assert_eq!(s.depth().in_use, 0);
    }
}
