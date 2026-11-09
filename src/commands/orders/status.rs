//! The `status` an order command's result reports. One vocabulary, so a new
//! status is added here and every row, JSON document and test that names one
//! reads it from this list.

use std::fmt;

use serde::{Serialize, Serializer};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Status {
    // What the exchange did with a placed order.
    Accepted,
    WaitingForTrigger,
    WaitingForFill,
    Resting,
    Filled,
    Rejected,
    // What happened to an existing order, TWAP or scheduled cancel.
    Cancelled,
    Failed,
    Modified,
    Running,
    Scheduled,
    Cleared,
    // A batch where some legs failed.
    Partial,
    // Why a chase ended (`Filled` is the fourth).
    Timeout,
    MaxChase,
    MinNotional,
    // How a bracket ended.
    Armed,
    EntryCancelledProtected,
    EntryCancelled,
    EntryUnfilled,
    EntryOpenUnprotected,
    EntryFilledUnprotected,
}

impl Status {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::WaitingForTrigger => "waiting_for_trigger",
            Self::WaitingForFill => "waiting_for_fill",
            Self::Resting => "resting",
            Self::Filled => "filled",
            Self::Rejected => "rejected",
            Self::Cancelled => "cancelled",
            Self::Failed => "failed",
            Self::Modified => "modified",
            Self::Running => "running",
            Self::Scheduled => "scheduled",
            Self::Cleared => "cleared",
            Self::Partial => "partial",
            Self::Timeout => "timeout",
            Self::MaxChase => "max_chase",
            Self::MinNotional => "min_notional",
            Self::Armed => "armed",
            Self::EntryCancelledProtected => "entry_cancelled_protected",
            Self::EntryCancelled => "entry_cancelled",
            Self::EntryUnfilled => "entry_unfilled",
            Self::EntryOpenUnprotected => "entry_open_unprotected",
            Self::EntryFilledUnprotected => "entry_filled_unprotected",
        }
    }
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Serialize for Status {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}
