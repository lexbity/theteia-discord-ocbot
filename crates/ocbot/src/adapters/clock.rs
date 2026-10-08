//! The real system clock (spec P4): the gate re-ping policy is authored against
//! the [`Clock`] port so a fake clock can drive it deterministically in tests.

use std::time::{SystemTime, UNIX_EPOCH};

use ocbot_core::ports::Clock;

/// Wall-clock seconds since the Unix epoch.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| {
                i64::try_from(duration.as_secs()).unwrap_or(i64::MAX)
            })
    }
}
