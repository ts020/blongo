//! Reconnect policy (t3code's `connection/supervisor.ts`): wait 3 s, 4 s,
//! 8 s, then 16 s between attempts; a connection that stayed up for 30 s
//! resets the ladder.

use std::time::Duration;

pub const RETRY_DELAYS: [Duration; 4] = [
    Duration::from_secs(3),
    Duration::from_secs(4),
    Duration::from_secs(8),
    Duration::from_secs(16),
];
pub const RESET_AFTER: Duration = Duration::from_secs(30);

#[derive(Clone, Debug, Default)]
pub struct Backoff {
    failures: usize,
    /// Scale for tests (1.0 in production).
    scale: Option<f64>,
}

impl Backoff {
    pub fn new() -> Self {
        Self::default()
    }

    /// All delays multiplied by `scale` (tests run the ladder quickly).
    pub fn scaled(scale: f64) -> Self {
        Self {
            failures: 0,
            scale: Some(scale),
        }
    }

    /// The connection ended after being up for `connected_for` (`None`:
    /// it never came up). Returns how long to wait before the next try.
    pub fn next_delay(&mut self, connected_for: Option<Duration>) -> Duration {
        if connected_for.is_some_and(|d| d >= self.scale_dur(RESET_AFTER)) {
            self.failures = 0;
        }
        let delay = RETRY_DELAYS[self.failures.min(RETRY_DELAYS.len() - 1)];
        self.failures += 1;
        self.scale_dur(delay)
    }

    pub fn failures(&self) -> usize {
        self.failures
    }

    fn scale_dur(&self, d: Duration) -> Duration {
        match self.scale {
            Some(s) => d.mul_f64(s),
            None => d,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ladder_then_plateau_then_reset_after_healthy() {
        let mut b = Backoff::new();
        let secs = |d: Duration| d.as_secs();
        assert_eq!(secs(b.next_delay(None)), 3);
        assert_eq!(secs(b.next_delay(None)), 4);
        assert_eq!(secs(b.next_delay(Some(Duration::from_secs(5)))), 8);
        assert_eq!(secs(b.next_delay(None)), 16);
        assert_eq!(secs(b.next_delay(None)), 16);
        assert_eq!(secs(b.next_delay(Some(Duration::from_secs(29)))), 16);
        // Up for 30 s: back to the first rung.
        assert_eq!(secs(b.next_delay(Some(Duration::from_secs(30)))), 3);
        assert_eq!(secs(b.next_delay(None)), 4);
    }

    #[test]
    fn scaled_for_tests() {
        let mut b = Backoff::scaled(0.01);
        assert_eq!(b.next_delay(None), Duration::from_millis(30));
        assert_eq!(
            b.next_delay(Some(Duration::from_millis(300))),
            Duration::from_millis(30)
        );
    }
}
