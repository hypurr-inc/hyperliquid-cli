//! The wall clock, in the one unit the exchange speaks: milliseconds since the
//! Unix epoch.

use std::time::Duration;

use chrono::Utc;

#[must_use]
pub fn now_ms() -> u64 {
    Utc::now().timestamp_millis() as u64
}

/// A suffix a duration may end in, and how long one of it is.
#[derive(Debug, Clone, Copy)]
pub struct Unit {
    suffix: &'static str,
    /// What the unit is called where a message names it.
    name: &'static str,
    millis: u64,
}

pub const MILLISECOND: Unit = Unit::new("ms", "milliseconds", 1);
pub const SECOND: Unit = Unit::new("s", "seconds", 1_000);
pub const MINUTE: Unit = Unit::new("m", "minutes", 60 * 1_000);
pub const HOUR: Unit = Unit::new("h", "hours", 60 * 60 * 1_000);
pub const DAY: Unit = Unit::new("d", "days", 24 * 60 * 60 * 1_000);
pub const WEEK: Unit = Unit::new("w", "weeks", 7 * 24 * 60 * 60 * 1_000);

impl Unit {
    const fn new(suffix: &'static str, name: &'static str, millis: u64) -> Self {
        Self {
            suffix,
            name,
            millis,
        }
    }
}

/// Which spellings of a duration a command takes: a whole, positive count
/// followed by one of `units`.
#[derive(Debug, Clone, Copy)]
pub struct Grammar {
    units: &'static [Unit],
    /// What a count with no suffix means; no unit means it is refused.
    bare: Option<Unit>,
    /// Whether `5M` is `5m`.
    ignore_case: bool,
    /// Which fault a text with several is reported for: a bad count outranks
    /// a bad unit when set, the other way round when not.
    count_first: bool,
}

/// `30s`, `5m`, `2h` or `1d`; a bare number counts seconds. The spelling
/// `--expires-after`, `orders schedule-cancel --in` and the order timeouts take.
pub const LIFETIME: Grammar = Grammar {
    units: &[SECOND, MINUTE, HOUR, DAY],
    bare: Some(SECOND),
    ignore_case: true,
    count_first: false,
};

/// `500ms` or `15s`; a bare number counts seconds. The poll interval of
/// `watch risk`.
pub const POLL_INTERVAL: Grammar = Grammar {
    units: &[MILLISECOND, SECOND],
    bare: Some(SECOND),
    ignore_case: false,
    count_first: false,
};

/// `30m`, `12h`, `30d` or `1w`, always with a unit. The expiry of an API wallet.
pub const AGENT_EXPIRY: Grammar = Grammar {
    units: &[MINUTE, HOUR, DAY, WEEK],
    bare: None,
    ignore_case: false,
    count_first: true,
};

/// Why a text is no duration of its [`Grammar`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DurationError {
    Empty,
    /// It ends in none of the grammar's units.
    Unit,
    /// What precedes the unit is no whole number; the unit it was read with.
    Count(&'static str),
    Zero,
    TooLarge,
}

pub fn parse_duration(input: &str, grammar: &Grammar) -> Result<Duration, DurationError> {
    let text = input.trim();
    let Some(last) = text.chars().last() else {
        return Err(DurationError::Empty);
    };
    let suffixed = grammar
        .units
        .iter()
        .filter(|unit| ends_with(text, unit.suffix, grammar.ignore_case))
        .max_by_key(|unit| unit.suffix.len());
    let (digits, unit) = match suffixed {
        Some(unit) => (&text[..text.len() - unit.suffix.len()], Some(*unit)),
        None if last.is_ascii_digit() && grammar.bare.is_some() => (text, grammar.bare),
        None => (&text[..text.len() - last.len_utf8()], None),
    };
    if unit.is_none() && !grammar.count_first {
        return Err(DurationError::Unit);
    }
    let count = digits
        .parse::<u64>()
        .map_err(|_| DurationError::Count(unit.map_or("", |unit| unit.name)))?;
    if count == 0 {
        return Err(DurationError::Zero);
    }
    let Some(unit) = unit else {
        return Err(DurationError::Unit);
    };
    let millis = u128::from(count) * u128::from(unit.millis);
    let seconds = u64::try_from(millis / 1_000).map_err(|_| DurationError::TooLarge)?;
    Ok(Duration::new(
        seconds,
        u32::try_from(millis % 1_000).unwrap_or(0) * 1_000_000,
    ))
}

fn ends_with(text: &str, suffix: &str, ignore_case: bool) -> bool {
    let Some(start) = text.len().checked_sub(suffix.len()) else {
        return false;
    };
    text.get(start..).is_some_and(|tail| {
        if ignore_case {
            tail.eq_ignore_ascii_case(suffix)
        } else {
            tail == suffix
        }
    })
}

/// A [`LIFETIME`], as the clap value parser of an option that takes one.
pub fn parse_lifetime(input: &str) -> Result<Duration, String> {
    parse_duration(input, &LIFETIME).map_err(|fault| match fault {
        DurationError::Empty => "duration cannot be empty".to_string(),
        DurationError::Unit => {
            "duration must be a positive integer with optional s/m/h/d suffix".to_string()
        }
        DurationError::Count(unit) => format!("duration {unit} must be a positive integer"),
        DurationError::Zero => "duration must be greater than zero".to_string(),
        DurationError::TooLarge => "duration is too large".to_string(),
    })
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
    fn every_lifetime_fault_keeps_its_message() {
        let fault = |input| parse_lifetime(input).unwrap_err();
        assert_eq!(fault(""), "duration cannot be empty");
        for bad in ["5x", "x", "0x", "ax", "é"] {
            assert_eq!(
                fault(bad),
                "duration must be a positive integer with optional s/m/h/d suffix",
                "{bad}"
            );
        }
        assert_eq!(fault("m"), "duration minutes must be a positive integer");
        assert_eq!(fault("-5s"), "duration seconds must be a positive integer");
        assert_eq!(fault("1.5m"), "duration minutes must be a positive integer");
        assert_eq!(fault("5h5"), "duration seconds must be a positive integer");
        assert_eq!(
            fault("99999999999999999999d"),
            "duration days must be a positive integer"
        );
        assert_eq!(fault("0s"), "duration must be greater than zero");
        assert_eq!(fault("0"), "duration must be greater than zero");
        assert_eq!(fault("9999999999999999d"), "duration is too large");
        assert_eq!(
            parse_lifetime("213503982334601d"),
            Ok(Duration::from_secs(213_503_982_334_601 * 86_400))
        );
    }

    #[test]
    fn a_poll_interval_is_milliseconds_or_seconds_and_nothing_else() {
        let parse = |input| parse_duration(input, &POLL_INTERVAL);
        assert_eq!(parse("500ms"), Ok(Duration::from_millis(500)));
        assert_eq!(parse("15s"), Ok(Duration::from_secs(15)));
        assert_eq!(parse(" 15 "), Ok(Duration::from_secs(15)));
        assert_eq!(parse("1"), Ok(Duration::from_secs(1)));
        assert_eq!(parse("0"), Err(DurationError::Zero));
        assert_eq!(parse("0ms"), Err(DurationError::Zero));
        for bad in ["", "ms", "s", "5m", "5S", "5MS", "1.5s", "-1s", "5 s"] {
            assert!(
                matches!(parse(bad), Err(fault) if fault != DurationError::Zero),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn an_agent_expiry_always_has_a_unit_and_a_bad_count_outranks_a_bad_unit() {
        let parse = |input| parse_duration(input, &AGENT_EXPIRY);
        assert_eq!(parse("30m"), Ok(Duration::from_secs(30 * 60)));
        assert_eq!(parse("12h"), Ok(Duration::from_secs(12 * 3_600)));
        assert_eq!(parse("30d"), Ok(Duration::from_secs(30 * 86_400)));
        assert_eq!(parse("1w"), Ok(Duration::from_secs(7 * 86_400)));
        assert_eq!(parse("0d"), Err(DurationError::Zero));
        assert_eq!(parse("5x"), Err(DurationError::Unit));
        assert_eq!(parse("5s"), Err(DurationError::Unit));
        assert_eq!(parse("5D"), Err(DurationError::Unit));
        assert_eq!(parse("30"), Err(DurationError::Unit));
        assert_eq!(parse("05"), Err(DurationError::Zero));
        assert_eq!(parse("0x"), Err(DurationError::Zero));
        assert_eq!(parse("ax"), Err(DurationError::Count("")));
        assert_eq!(parse("1.5d"), Err(DurationError::Count("days")));
        assert_eq!(
            parse("99999999999999999999d"),
            Err(DurationError::Count("days"))
        );
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
