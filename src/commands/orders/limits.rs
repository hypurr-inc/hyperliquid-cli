//! What the exchange itself bounds on the order commands: the smallest and
//! largest values it accepts, named once for the validation, planning and chase
//! code that checks them.

/// `scheduleCancel` refuses a trigger time less than this many seconds ahead.
pub(super) const SCHEDULE_CANCEL_MIN_SECONDS: u64 = 5;

/// A TWAP runs at least this many seconds.
pub(super) const TWAP_MIN_SECONDS: u64 = 300;

/// A TWAP's duration is a whole number of these.
pub(super) const TWAP_STEP_SECONDS: u64 = 60;

/// A TWAP runs at most seven days.
pub(super) const TWAP_MAX_SECONDS: u64 = 7 * 24 * 60 * 60;

/// The exchange's minimum order value, in USDC: chase stops when the unfilled
/// remainder is worth less.
pub(super) const MIN_NOTIONAL_USDC: i64 = 10;
