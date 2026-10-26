//! Per-app rate limits (phase 9.1): `governor`'s GCRA, one limiter per app, checked before anything else.

use std::collections::HashMap;
use std::num::NonZeroU32;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use governor::clock::{Clock, DefaultClock};
use governor::{DefaultDirectRateLimiter, Quota, RateLimiter};

use super::AppLimits;

#[derive(Default)]
pub struct RateLimits {
    limiters: Mutex<HashMap<String, Arc<DefaultDirectRateLimiter>>>,
}

impl RateLimits {
    /// `Err(wait)` when the app has used its pace; `wait` is when the next request would be allowed.
    pub fn check(&self, app: &str, limits: &AppLimits) -> Result<(), Duration> {
        let Some(per_minute) = limits.requests_per_minute.and_then(NonZeroU32::new) else { return Ok(()) };
        let burst = limits
            .burst
            .and_then(NonZeroU32::new)
            .unwrap_or_else(|| NonZeroU32::new((per_minute.get() / 10).max(1)).unwrap_or(NonZeroU32::MIN));
        let limiter = {
            let mut limiters = self.limiters.lock().unwrap_or_else(PoisonError::into_inner);
            Arc::clone(
                limiters
                    .entry(app.to_string())
                    .or_insert_with(|| Arc::new(RateLimiter::direct(Quota::per_minute(per_minute).allow_burst(burst)))),
            )
        };
        limiter.check().map_err(|not_until| not_until.wait_time_from(DefaultClock::default().now()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_burst_then_the_pace() {
        let rates = RateLimits::default();
        let limits = AppLimits { requests_per_minute: Some(60), burst: Some(3), ..AppLimits::default() };
        for _ in 0..3 {
            assert!(rates.check("pp", &limits).is_ok());
        }
        let wait = rates.check("pp", &limits).unwrap_err();
        assert!(wait > Duration::ZERO && wait <= Duration::from_secs(1), "{wait:?}");
        // Another app has its own limiter; no limit means no limiter at all.
        assert!(rates.check("other", &limits).is_ok());
        assert!(rates.check("free", &AppLimits::default()).is_ok());
    }
}
