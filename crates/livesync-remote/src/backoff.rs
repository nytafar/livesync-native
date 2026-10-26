//! Jittered exponential backoff for reconnects.

use rand::Rng;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct Backoff {
    base: Duration,
    max: Duration,
    attempt: u32,
}

impl Backoff {
    pub fn new(base: Duration, max: Duration) -> Self {
        Backoff { base, max, attempt: 0 }
    }

    /// Next delay: `min(max, base * 2^attempt)`, scaled by a random factor in
    /// [0.5, 1.0] ("equal jitter") so many clients do not reconnect in step.
    pub fn next_delay(&mut self) -> Duration {
        let exp = self.base.saturating_mul(1u32 << self.attempt.min(16));
        let capped = exp.min(self.max);
        self.attempt = self.attempt.saturating_add(1);
        let factor: f64 = rand::thread_rng().gen_range(0.5..=1.0);
        capped.mul_f64(factor)
    }

    pub fn reset(&mut self) {
        self.attempt = 0;
    }

    pub fn attempt(&self) -> u32 {
        self.attempt
    }
}

impl Default for Backoff {
    fn default() -> Self {
        Backoff::new(Duration::from_secs(1), Duration::from_secs(300))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grows_and_caps() {
        let mut b = Backoff::new(Duration::from_secs(1), Duration::from_secs(10));
        let d0 = b.next_delay();
        assert!(d0 >= Duration::from_millis(500) && d0 <= Duration::from_secs(1));
        for _ in 0..20 {
            let d = b.next_delay();
            assert!(d <= Duration::from_secs(10));
        }
        let d = b.next_delay();
        assert!(d >= Duration::from_secs(5));
        b.reset();
        assert!(b.next_delay() <= Duration::from_secs(1));
    }
}
