//! The sink that writes each finished exchange to the store.

use std::sync::Arc;

use super::{Bodies, ExchangeRow, Store};
use crate::config::BodyPolicy;
use crate::proxy::record::{Finished, Outcome, Sink};

pub struct RequestLog {
    store: Arc<Store>,
    bodies: BodyPolicy,
}

impl RequestLog {
    pub fn new(store: Arc<Store>, bodies: BodyPolicy) -> Self {
        Self { store, bodies }
    }
}

impl Sink for RequestLog {
    fn finished(&self, f: Finished) {
        let keep = match self.bodies {
            BodyPolicy::None => false,
            BodyPolicy::Errors => f.outcome != Outcome::Ok || !(200..300).contains(&f.status),
            BodyPolicy::All => true,
        };
        let row = ExchangeRow::from_finished(&f);
        let bodies = keep.then(|| Bodies { request: f.exchange.request.clone(), response: f.response.clone() });
        self.store.record(row, bodies);
    }
}
