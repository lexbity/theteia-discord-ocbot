//! The real system clock (spec P4): the gate re-ping policy is authored against
//! the [`Clock`] port so a fake clock can drive it deterministically in tests.

use ocbot_core::ports::Clock;

use super::now_unix;

/// Wall-clock seconds since the Unix epoch.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> i64 {
        now_unix()
    }
}
