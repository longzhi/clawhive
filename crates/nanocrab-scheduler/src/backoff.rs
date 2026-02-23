const ERROR_BACKOFF_MS: &[u64] = &[30_000, 60_000, 5 * 60_000, 15 * 60_000, 60 * 60_000];

pub fn error_backoff_ms(consecutive_errors: u32) -> u64 {
    let idx = (consecutive_errors.saturating_sub(1) as usize).min(ERROR_BACKOFF_MS.len() - 1);
    ERROR_BACKOFF_MS[idx]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_backoff_escalation() {
        assert_eq!(error_backoff_ms(1), 30_000);
        assert_eq!(error_backoff_ms(2), 60_000);
        assert_eq!(error_backoff_ms(5), 60 * 60_000);
        assert_eq!(error_backoff_ms(100), 60 * 60_000);
    }
}
