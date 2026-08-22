//! Monotonic time. SPEC's `Millis` is "milliseconds since process start" and is
//! never wall-clock — suspend/resume and NTP steps would reorder the flight
//! recorder.

use std::sync::Arc;
use std::time::Instant;

/// Shared monotonic origin. Cloning is cheap and every clone agrees.
#[derive(Debug, Clone)]
pub struct Clock(Arc<Instant>);

impl Clock {
    pub fn new() -> Self {
        Clock(Arc::new(Instant::now()))
    }

    /// Monotonic milliseconds since this clock's origin.
    pub fn now(&self) -> wisp_proto::Millis {
        self.0.elapsed().as_millis() as u64
    }
}

impl Default for Clock {
    fn default() -> Self {
        Clock::new()
    }
}

/// Days since the Unix epoch, in UTC. Used only to decide when the consent
/// panel's "used N times today" counter rolls over — never for ordering.
pub fn utc_day() -> i64 {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    secs.div_euclid(86_400)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_is_monotonic_and_shared() {
        let a = Clock::new();
        let b = a.clone();
        let t0 = a.now();
        std::thread::sleep(std::time::Duration::from_millis(5));
        let t1 = b.now();
        assert!(t1 >= t0, "{t1} >= {t0}");
        assert!(t1 >= 4, "clock did not advance: {t1}");
    }

    #[test]
    fn utc_day_is_plausible() {
        // 2020-01-01 is day 18262; anything before that means a broken clock.
        assert!(utc_day() > 18_262);
    }
}
