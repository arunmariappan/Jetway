//! The live feed (phase 4.2): every finished exchange is broadcast to the dashboard's open live pages.

use std::sync::Arc;

use tokio::sync::broadcast;

use crate::proxy::record::{Finished, Sink};
use crate::store::ExchangeRow;

const CAPACITY: usize = 256;

#[derive(Clone)]
pub struct Feed {
    tx: broadcast::Sender<Arc<ExchangeRow>>,
}

impl Default for Feed {
    fn default() -> Self {
        let (tx, _) = broadcast::channel(CAPACITY);
        Self { tx }
    }
}

impl Feed {
    pub fn subscribe(&self) -> broadcast::Receiver<Arc<ExchangeRow>> {
        self.tx.subscribe()
    }
}

impl Sink for Feed {
    fn finished(&self, finished: Finished) {
        // Nobody watching is fine; a slow page skips what it missed.
        let _ = self.tx.send(Arc::new(ExchangeRow::from_finished(&finished)));
    }
}
