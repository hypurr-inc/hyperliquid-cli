//! Time-windowed `/info` history reads that the API cuts off at a fixed page
//! size without saying so.
//!
//! A history request answers its oldest rows first and stops at a cap. A page
//! that is exactly the cap may have more behind it, so the reader either says
//! so (naming the `--start` that continues) or, under `--all`, asks again from
//! the last returned timestamp until a short page arrives.
//!
//! The API paginates by "the last returned timestamp as the next `startTime`",
//! which is inclusive, so the rows already seen at that millisecond come back
//! once more and are dropped here; stepping one millisecond past it instead
//! would skip rows that share it.

use serde_json::Value;

use crate::errors::CliError;
use crate::http_api::ApiClient;
use crate::invocation::Invocation;
use crate::output::Notice;
use crate::wire::InfoRequest;

/// `--all` stops after this many requests even if the API still answers full pages.
const MAX_PAGES: usize = 50;

/// A history endpoint and the page size it truncates at.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Endpoint {
    /// The `type` of the request, as the operator names it in a notice.
    pub label: &'static str,
    /// Rows per response before the API stops (verified against the live API).
    pub cap: usize,
}

pub(crate) const USER_FILLS_BY_TIME: Endpoint = Endpoint {
    label: "fills",
    cap: 2000,
};
pub(crate) const USER_FUNDING: Endpoint = Endpoint {
    label: "funding payments",
    cap: 500,
};
pub(crate) const USER_LEDGER: Endpoint = Endpoint {
    label: "ledger updates",
    cap: 2000,
};
pub(crate) const FUNDING_HISTORY: Endpoint = Endpoint {
    label: "funding rates",
    cap: 500,
};
/// `userFills` has no time window: it answers the most recent rows only, up to
/// the same page size as the windowed read.
pub(crate) const USER_FILLS_RECENT: Endpoint = USER_FILLS_BY_TIME;

/// What lies beyond the rows a read returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum More {
    Nothing,
    /// A full page: later rows may exist from this `--start`.
    From(u64),
    /// A full page of a request that cannot be continued (no time window), so
    /// there is no `--start` to name.
    Unwindowed,
}

/// The rows read and what lies beyond them.
#[derive(Debug)]
pub(crate) struct Window {
    pub rows: Vec<Value>,
    more: More,
}

/// Read the window starting at `start`: one page, or every page under `all`.
///
/// `request` builds the request body for a given start time.
pub(crate) async fn read_window(
    api: &ApiClient,
    endpoint: Endpoint,
    start: u64,
    all: bool,
    request: impl Fn(u64) -> InfoRequest,
    context: &'static str,
) -> Result<Window, CliError> {
    let mut rows: Vec<Value> = Vec::new();
    let mut from = start;
    let mut boundary: Vec<Value> = Vec::new();
    for _ in 0..MAX_PAGES {
        let page: Vec<Value> = api.info(&request(from), context).await?;
        let full = page.len() >= endpoint.cap;
        let last = page.last().and_then(row_time);
        let before = rows.len();
        rows.extend(page.into_iter().filter(|row| !boundary.contains(row)));
        let Some(last) = last.filter(|_| full) else {
            return Ok(Window {
                rows,
                more: More::Nothing,
            });
        };
        if !all {
            return Ok(Window {
                rows,
                more: More::From(last),
            });
        }
        // A page that added nothing new sits wholly inside one millisecond that
        // is already read; the only way forward is past it.
        let progressed = rows.len() > before;
        boundary = rows
            .iter()
            .filter(|row| row_time(row) == Some(last))
            .cloned()
            .collect();
        from = if progressed {
            last
        } else {
            last.saturating_add(1)
        };
    }
    Ok(Window {
        more: rows
            .last()
            .and_then(row_time)
            .map_or(More::Nothing, More::From),
        rows,
    })
}

fn row_time(row: &Value) -> Option<u64> {
    row.get("time").and_then(Value::as_u64)
}

/// Read a page of an endpoint with no time window (`userFills`).
pub(crate) async fn read_recent(
    api: &ApiClient,
    endpoint: Endpoint,
    request: &InfoRequest,
    context: &'static str,
) -> Result<Window, CliError> {
    let rows: Vec<Value> = api.info(request, context).await?;
    let more = if rows.len() >= endpoint.cap {
        More::Unwindowed
    } else {
        More::Nothing
    };
    Ok(Window { rows, more })
}

/// Say on stderr that `window` stopped at the API's cap. A person reads one
/// sentence; a JSON run gets one compact object, so stdout keeps the shape it
/// always had.
pub(crate) fn announce_truncation(
    inv: &Invocation,
    endpoint: Endpoint,
    window: &Window,
    all: bool,
) {
    let returned = window.rows.len();
    let (message, next_start) = match window.more {
        More::From(next) if all => (
            format!(
                "Stopped --all after {MAX_PAGES} requests with {returned} {label}; more remain. \
                 Continue with --start {next} (rows at that exact millisecond may repeat).",
                label = endpoint.label,
            ),
            Some(next),
        ),
        More::From(next) => (
            format!(
                "Returned {returned} {label}, the API's per-request limit, so later ones may exist. \
                 Continue with --start {next} (rows at that exact millisecond may repeat) or pass --all.",
                label = endpoint.label,
            ),
            Some(next),
        ),
        More::Unwindowed => (
            format!(
                "Returned the {returned} most recent {label}, the API's per-request limit, so earlier ones are not shown. \
                 Pass --start to page through a time window.",
                label = endpoint.label,
            ),
            None,
        ),
        More::Nothing => return,
    };
    Notice::Truncated {
        message,
        returned,
        cap: endpoint.cap,
        next_start,
    }
    .eprint(inv.mode().machine_format());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_without_a_numeric_time_have_no_cursor() {
        assert_eq!(row_time(&serde_json::json!({"time": 5})), Some(5));
        assert_eq!(row_time(&serde_json::json!({"time": "5"})), None);
        assert_eq!(row_time(&serde_json::json!({})), None);
    }
}
