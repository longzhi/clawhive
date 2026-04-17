use std::collections::HashMap;
use std::time::{Duration, Instant};

const FAILURE_THRESHOLD: u32 = 2;
const COOLDOWN_DURATION: Duration = Duration::from_secs(300); // 5 minutes

pub struct CircuitBreaker {
    states: HashMap<String, ProviderState>,
}

struct ProviderState {
    consecutive_failures: u32,
    cooldown_until: Option<Instant>,
}

impl Default for CircuitBreaker {
    fn default() -> Self {
        Self::new()
    }
}

impl CircuitBreaker {
    pub fn new() -> Self {
        Self {
            states: HashMap::new(),
        }
    }

    pub fn is_available(&self, provider: &str) -> bool {
        match self.states.get(provider) {
            None => true,
            Some(state) => match state.cooldown_until {
                None => true,
                Some(until) => Instant::now() >= until,
            },
        }
    }

    pub fn record_failure(&mut self, provider: &str) {
        let state = self
            .states
            .entry(provider.to_string())
            .or_insert(ProviderState {
                consecutive_failures: 0,
                cooldown_until: None,
            });
        state.consecutive_failures += 1;
        if state.consecutive_failures >= FAILURE_THRESHOLD {
            state.cooldown_until = Some(Instant::now() + COOLDOWN_DURATION);
        }
    }

    pub fn record_success(&mut self, provider: &str) {
        self.states.remove(provider);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_provider_is_available() {
        let cb = CircuitBreaker::new();
        assert!(cb.is_available("brave"));
    }

    #[test]
    fn single_failure_does_not_trip() {
        let mut cb = CircuitBreaker::new();
        cb.record_failure("brave");
        assert!(cb.is_available("brave"));
    }

    #[test]
    fn consecutive_failures_trip_breaker() {
        let mut cb = CircuitBreaker::new();
        cb.record_failure("brave");
        cb.record_failure("brave");
        assert!(!cb.is_available("brave"));
    }

    #[test]
    fn success_resets_failures() {
        let mut cb = CircuitBreaker::new();
        cb.record_failure("brave");
        cb.record_failure("brave");
        assert!(!cb.is_available("brave"));
        cb.record_success("brave");
        assert!(cb.is_available("brave"));
    }

    #[test]
    fn cooldown_expires() {
        let mut cb = CircuitBreaker::new();
        cb.record_failure("brave");
        cb.record_failure("brave");
        // Manually set cooldown to the past
        if let Some(state) = cb.states.get_mut("brave") {
            state.cooldown_until = Some(Instant::now() - Duration::from_secs(1));
        }
        assert!(cb.is_available("brave"));
    }

    #[test]
    fn independent_providers() {
        let mut cb = CircuitBreaker::new();
        cb.record_failure("brave");
        cb.record_failure("brave");
        assert!(!cb.is_available("brave"));
        assert!(cb.is_available("tavily"));
    }
}
