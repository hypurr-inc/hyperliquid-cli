//! The wall clock, in the one unit the exchange speaks: milliseconds since the
//! Unix epoch.

use chrono::Utc;

#[must_use]
pub fn now_ms() -> u64 {
    Utc::now().timestamp_millis() as u64
}
