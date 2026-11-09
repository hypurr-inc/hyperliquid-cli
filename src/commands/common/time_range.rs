//! The `--start`/`--end` window every time-ranged read takes: the one parser
//! for a timestamp flag and the one check that the window is not backwards.

use chrono::DateTime;

use crate::errors::CliError;

pub fn parse_time_millis(raw: &str) -> Result<u64, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("time value cannot be empty".to_string());
    }

    if trimmed.chars().all(|ch| ch.is_ascii_digit()) {
        return trimmed
            .parse::<u64>()
            .map_err(|err| format!("invalid epoch millisecond timestamp: {err}"));
    }

    let parsed = DateTime::parse_from_rfc3339(trimmed)
        .map_err(|err| format!("invalid RFC3339 or epoch millisecond timestamp: {err}"))?;
    let millis = parsed.timestamp_millis();
    if millis < 0 {
        return Err("time must be at or after the Unix epoch".to_string());
    }
    Ok(millis as u64)
}

pub(crate) fn validate_optional_time_range(
    start: Option<u64>,
    end: Option<u64>,
) -> Result<(), CliError> {
    if let (Some(start), Some(end)) = (start, end)
        && end < start
    {
        return Err(CliError::Configuration(
            "--end must be greater than or equal to --start".to_string(),
        ));
    }
    Ok(())
}
