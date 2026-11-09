//! The wall clock, in the one unit the exchange speaks: milliseconds since the
//! Unix epoch.

use std::time::Duration;

use chrono::Utc;

#[must_use]
pub fn now_ms() -> u64 {
    Utc::now().timestamp_millis() as u64
}

/// A relative duration such as `30s`, `5m`, `2h` or `1d`; a bare number counts
/// seconds. Whole, positive units only, the same spelling `orders
/// schedule-cancel --in` takes.
pub fn parse_lifetime(input: &str) -> Result<Duration, String> {
    let trimmed = input.trim();
    let Some(last) = trimmed.chars().last() else {
        return Err("duration cannot be empty".to_string());
    };
    let (digits, seconds_per_unit, unit) = match last.to_ascii_lowercase() {
        's' => (&trimmed[..trimmed.len() - 1], 1_u64, "seconds"),
        'm' => (&trimmed[..trimmed.len() - 1], 60, "minutes"),
        'h' => (&trimmed[..trimmed.len() - 1], 60 * 60, "hours"),
        'd' => (&trimmed[..trimmed.len() - 1], 24 * 60 * 60, "days"),
        digit if digit.is_ascii_digit() => (trimmed, 1, "seconds"),
        _ => {
            return Err(
                "duration must be a positive integer with optional s/m/h/d suffix".to_string(),
            );
        }
    };
    let count = digits
        .parse::<u64>()
        .map_err(|_| format!("duration {unit} must be a positive integer"))?;
    if count == 0 {
        return Err("duration must be greater than zero".to_string());
    }
    count
        .checked_mul(seconds_per_unit)
        .map(Duration::from_secs)
        .ok_or_else(|| "duration is too large".to_string())
}

/// The `expiresAfter` timestamp for an action signed at `now_ms` that stays
/// valid for `lifetime`.
#[must_use]
pub fn expires_at_ms(now_ms: u64, lifetime: Duration) -> u64 {
    now_ms.saturating_add(u64::try_from(lifetime.as_millis()).unwrap_or(u64::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lifetimes_take_the_units_schedule_cancel_takes() {
        assert_eq!(parse_lifetime("30s"), Ok(Duration::from_secs(30)));
        assert_eq!(parse_lifetime("5m"), Ok(Duration::from_secs(300)));
        assert_eq!(parse_lifetime("2H"), Ok(Duration::from_secs(7_200)));
        assert_eq!(parse_lifetime("1d"), Ok(Duration::from_secs(86_400)));
        assert_eq!(parse_lifetime(" 45 "), Ok(Duration::from_secs(45)));
    }

    #[test]
    fn lifetimes_must_be_positive_whole_numbers() {
        for bad in ["", "0s", "-5s", "1.5m", "m", "5x", "99999999999999999999d"] {
            assert!(parse_lifetime(bad).is_err(), "{bad:?} should be refused");
        }
    }

    #[test]
    fn expiry_is_the_signing_time_plus_the_lifetime() {
        assert_eq!(
            expires_at_ms(1_700_000_000_000, Duration::from_secs(30)),
            1_700_000_030_000
        );
        assert_eq!(expires_at_ms(u64::MAX, Duration::from_secs(1)), u64::MAX);
    }
}
