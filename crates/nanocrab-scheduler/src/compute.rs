use std::str::FromStr;

use anyhow::{anyhow, Result};
use chrono::{DateTime, TimeZone, Utc};
use chrono_tz::Tz;
use cron::Schedule as CronSchedule;

use crate::ScheduleType;

pub fn compute_next_run_at_ms(schedule: &ScheduleType, now_ms: i64) -> Result<Option<i64>> {
    match schedule {
        ScheduleType::Cron { expr, tz } => {
            let tz: Tz = tz.parse().map_err(|_| anyhow!("invalid timezone: {tz}"))?;
            let cron = CronSchedule::from_str(&normalize_cron_expr(expr))?;
            let now_dt = tz
                .timestamp_millis_opt(now_ms)
                .single()
                .ok_or_else(|| anyhow!("invalid timestamp: {now_ms}"))?;
            let next = cron.after(&now_dt).next();
            Ok(next.map(|dt| dt.with_timezone(&Utc).timestamp_millis()))
        }
        ScheduleType::At { at } => {
            let at_ms = parse_absolute_or_relative_ms(at, now_ms)?;
            Ok((at_ms > now_ms).then_some(at_ms))
        }
        ScheduleType::Every {
            interval_ms,
            anchor_ms,
        } => {
            let interval = *interval_ms as i64;
            if interval <= 0 {
                return Err(anyhow!("interval_ms must be positive"));
            }

            let anchor = anchor_ms.map(|value| value as i64).unwrap_or(now_ms);
            if now_ms < anchor {
                return Ok(Some(anchor));
            }

            let elapsed = now_ms - anchor;
            let steps = (elapsed + interval - 1) / interval;
            Ok(Some(anchor + steps * interval))
        }
    }
}

fn normalize_cron_expr(expr: &str) -> String {
    if expr.split_whitespace().count() == 5 {
        format!("0 {expr}")
    } else {
        expr.to_string()
    }
}

fn parse_absolute_or_relative_ms(input: &str, now_ms: i64) -> Result<i64> {
    if let Some(ms) = try_parse_relative_ms(input) {
        return Ok(now_ms + ms);
    }

    let dt = DateTime::parse_from_rfc3339(input)
        .or_else(|_| DateTime::parse_from_str(input, "%Y-%m-%dT%H:%M:%S%z"))?;
    Ok(dt.with_timezone(&Utc).timestamp_millis())
}

fn try_parse_relative_ms(input: &str) -> Option<i64> {
    let input = input.trim();
    if input.len() < 2 {
        return None;
    }

    let (num_str, unit) = input.split_at(input.len() - 1);
    let num: i64 = num_str.parse().ok()?;

    match unit {
        "s" => Some(num * 1_000),
        "m" => Some(num * 60_000),
        "h" => Some(num * 3_600_000),
        "d" => Some(num * 86_400_000),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::*;

    #[test]
    fn test_cron_next_run() {
        let schedule = ScheduleType::Cron {
            expr: "* * * * *".into(),
            tz: "UTC".into(),
        };
        let now_ms = Utc::now().timestamp_millis();
        let next = compute_next_run_at_ms(&schedule, now_ms).unwrap().unwrap();
        assert!(next > now_ms);
        assert!(next - now_ms <= 60_000);
    }

    #[test]
    fn test_at_relative() {
        let schedule = ScheduleType::At { at: "20m".into() };
        let now_ms = 1_000_000;
        let next = compute_next_run_at_ms(&schedule, now_ms).unwrap().unwrap();
        assert_eq!(next, 1_000_000 + 20 * 60_000);
    }

    #[test]
    fn test_at_past_returns_none() {
        let schedule = ScheduleType::At {
            at: "2020-01-01T00:00:00Z".into(),
        };
        let now_ms = Utc::now().timestamp_millis();
        assert!(compute_next_run_at_ms(&schedule, now_ms).unwrap().is_none());
    }

    #[test]
    fn test_every_with_anchor() {
        let schedule = ScheduleType::Every {
            interval_ms: 60_000,
            anchor_ms: Some(0),
        };
        let next = compute_next_run_at_ms(&schedule, 90_000).unwrap().unwrap();
        assert_eq!(next, 120_000);
    }
}
