use hypersdk::Decimal;
use hypersdk::hypercore::types::OrderResponseStatus;
use serde::Serialize;

use super::args::TpslGroupingArg;
use super::exchange::{Placement, Rejected, status_error};
use super::prepared::{PreparedOrder, PreparedTpslLeg};
use super::status::Status;
use crate::commands::common::rows::{ListRow, Row, paint_column};
use crate::errors::{CliError, NextCommand, partial_results_fields};
use crate::hints::CHECK_LIVE_COMMAND;
use crate::output::{self, TableData};

/// A fill reports the price and size it filled at; any other status keeps the planned
/// ones. A rejected leg carries the exchange's error, so one bad leg never hides the
/// legs that were placed.
#[derive(Debug, Clone, Serialize)]
struct LegOutcome {
    status: Status,
    order_id: Option<u64>,
    price: String,
    size: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<Rejected>,
}

impl LegOutcome {
    fn from_status(
        status: OrderResponseStatus,
        planned_price: Decimal,
        planned_size: Decimal,
    ) -> Self {
        let placed = Placement::classify(status);
        let (price, size) = match &placed {
            Placement::Filled {
                avg_px, total_sz, ..
            } => (*avg_px, *total_sz),
            Placement::Open { .. } | Placement::Rejected(_) => (planned_price, planned_size),
        };
        let (status, order_id) = (placed.status(), placed.oid());
        let error = match placed {
            Placement::Rejected(rejected) => Some(rejected),
            Placement::Open { .. } | Placement::Filled { .. } => None,
        };
        Self {
            status,
            order_id,
            price: price.to_string(),
            size: size.to_string(),
            error,
        }
    }
}

fn id_cell(order_id: Option<u64>) -> String {
    order_id.map(|oid| oid.to_string()).unwrap_or_default()
}

fn paint_warnings(headers: &[&str], rows: Vec<Vec<String>>) -> Vec<Vec<String>> {
    paint_column(
        headers,
        rows,
        "Warning",
        |warning| !warning.is_empty(),
        output::colors::yellow,
    )
}

/// One leg of a batch result: its columns and the error the exchange gave it,
/// if any. The one description of how a leg table grows its error column.
pub(super) trait LegRow: Serialize {
    const HEADERS: &'static [&'static str];
    const ERROR_ALWAYS: bool = false;

    fn cells(&self) -> Vec<String>;

    fn error(&self) -> Option<&Rejected>;
}

pub(super) struct Legs<T> {
    pub(super) rows: Vec<T>,
}

impl<T: LegRow> Legs<T> {
    pub(super) fn new(rows: Vec<T>) -> Self {
        Self { rows }
    }

    fn shows_error(&self) -> bool {
        T::ERROR_ALWAYS || self.rows.iter().any(|row| row.error().is_some())
    }

    pub(super) fn rejection_errors(&self) -> Vec<Rejected> {
        self.rows
            .iter()
            .filter_map(|row| row.error().cloned())
            .collect()
    }
}

impl<T: LegRow> TableData for Legs<T> {
    fn headers(&self) -> Vec<&str> {
        let mut headers = T::HEADERS.to_vec();
        if self.shows_error() {
            headers.push("Error");
        }
        headers
    }

    fn rows(&self) -> Vec<Vec<String>> {
        let with_errors = self.shows_error();
        self.rows
            .iter()
            .map(|row| {
                let mut cells = row.cells();
                if with_errors {
                    cells.push(row.error().map(Rejected::to_string).unwrap_or_default());
                }
                cells
            })
            .collect()
    }

    fn pretty_rows(&self) -> Vec<Vec<String>> {
        paint_warnings(&self.headers(), self.rows())
    }

    fn to_json_value(&self) -> serde_json::Value {
        output::serialize_row(&self.rows)
    }
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct OrderConfirmation {
    coin: String,
    side: String,
    order_type: String,
    trigger_price: Option<String>,
    amount: Option<String>,
    amount_unit: Option<String>,
    tif: Option<String>,
    reduce_only: bool,
    warning: Option<String>,
    #[serde(flatten)]
    outcome: LegOutcome,
}

impl OrderConfirmation {
    pub(super) fn from_leg_status(prepared: &PreparedOrder, status: OrderResponseStatus) -> Self {
        Self {
            coin: prepared.coin.clone(),
            side: prepared.side.to_string(),
            order_type: prepared.order_type.to_string(),
            trigger_price: prepared.trigger_price.map(|price| price.to_string()),
            amount: prepared
                .amount
                .as_ref()
                .map(|amount| amount.value.to_string()),
            amount_unit: prepared.amount.as_ref().map(|amount| amount.unit.clone()),
            tif: prepared.tif.map(|tif| tif.to_string()),
            reduce_only: prepared.request.reduce_only,
            warning: prepared.warning.clone(),
            outcome: LegOutcome::from_status(status, prepared.price(), prepared.size()),
        }
    }

    pub(super) fn from_status(
        prepared: &PreparedOrder,
        status: OrderResponseStatus,
    ) -> Result<Self, CliError> {
        let row = Self::from_leg_status(prepared, status);
        match &row.outcome.error {
            Some(rejected) => Err(status_error("place order", rejected.raw())),
            None => Ok(row),
        }
    }
}

impl LegRow for OrderConfirmation {
    const HEADERS: &'static [&'static str] = &[
        "Coin",
        "Side",
        "Type",
        "Status",
        "Order ID",
        "Price",
        "Trigger Price",
        "Size",
        "Amount",
        "Amount Unit",
        "TIF",
        "Reduce Only",
        "Warning",
    ];

    fn cells(&self) -> Vec<String> {
        vec![
            self.coin.clone(),
            self.side.clone(),
            self.order_type.clone(),
            self.outcome.status.to_string(),
            id_cell(self.outcome.order_id),
            self.outcome.price.clone(),
            self.trigger_price.clone().unwrap_or_default(),
            self.outcome.size.clone(),
            self.amount.clone().unwrap_or_default(),
            self.amount_unit.clone().unwrap_or_default(),
            self.tif.clone().unwrap_or_default(),
            self.reduce_only.to_string(),
            self.warning.clone().unwrap_or_default(),
        ]
    }

    fn error(&self) -> Option<&Rejected> {
        self.outcome.error.as_ref()
    }
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct TpslOrderConfirmation {
    grouping: String,
    grouping_wire: serde_json::Value,
    leg: String,
    coin: String,
    side: String,
    order_type: String,
    reduce_only: bool,
    tif: Option<String>,
    warning: Option<String>,
    #[serde(flatten)]
    outcome: LegOutcome,
}

impl TpslOrderConfirmation {
    pub(super) fn from_tpsl_status(
        prepared: &PreparedTpslLeg,
        grouping: TpslGroupingArg,
        status: OrderResponseStatus,
    ) -> Self {
        Self {
            grouping: grouping.to_string(),
            grouping_wire: grouping.wire_value(),
            leg: prepared.leg.to_string(),
            coin: prepared.coin.clone(),
            side: prepared.side.to_string(),
            order_type: prepared.order_type.to_string(),
            reduce_only: prepared.reduce_only,
            tif: prepared.tif.map(|tif| tif.to_string()),
            warning: prepared.warning.clone(),
            outcome: LegOutcome::from_status(status, prepared.price, prepared.size),
        }
    }
}

impl LegRow for TpslOrderConfirmation {
    const HEADERS: &'static [&'static str] = &[
        "Grouping",
        "Leg",
        "Coin",
        "Side",
        "Type",
        "Status",
        "Order ID",
        "Price",
        "Size",
        "Reduce Only",
        "TIF",
        "Warning",
    ];

    fn cells(&self) -> Vec<String> {
        vec![
            self.grouping.clone(),
            self.leg.clone(),
            self.coin.clone(),
            self.side.clone(),
            self.order_type.clone(),
            self.outcome.status.to_string(),
            id_cell(self.outcome.order_id),
            self.outcome.price.clone(),
            self.outcome.size.clone(),
            self.reduce_only.to_string(),
            self.tif.clone().unwrap_or_default(),
            self.warning.clone().unwrap_or_default(),
        ]
    }

    fn error(&self) -> Option<&Rejected> {
        self.outcome.error.as_ref()
    }
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct CancelConfirmation {
    pub(super) coin: String,
    pub(super) status: Status,
    pub(super) order_id: Option<u64>,
    pub(super) cloid: Option<String>,
}

impl Row for CancelConfirmation {
    const HEADERS: &'static [&'static str] = &["Coin", "Status", "Order ID", "CLOID"];

    fn cells(&self) -> Vec<String> {
        vec![
            self.coin.clone(),
            self.status.to_string(),
            id_cell(self.order_id),
            self.cloid.clone().unwrap_or_default(),
        ]
    }
}

impl ListRow for CancelConfirmation {
    const EMPTY: &'static str = "No order cancelled.";
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct CancelAllSummary {
    pub(super) coin: String,
    pub(super) cancelled_orders: usize,
}

impl Row for CancelAllSummary {
    const HEADERS: &'static [&'static str] = &["Coin", "Cancelled Orders"];

    fn cells(&self) -> Vec<String> {
        vec![self.coin.clone(), self.cancelled_orders.to_string()]
    }
}

impl ListRow for CancelAllSummary {
    const EMPTY: &'static str = "No orders cancelled.";
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct CancelLeg {
    pub(super) order_id: u64,
    pub(super) status: Status,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) error: Option<Rejected>,
}

impl CancelLeg {
    /// The leg for one cancelled order. The exchange answers a cancel with
    /// `success` or an error; any other status did not confirm the cancel, so
    /// it is reported as a failure rather than as a cancel.
    pub(super) fn from_status(order_id: u64, status: OrderResponseStatus) -> Self {
        match status {
            OrderResponseStatus::Success => Self {
                order_id,
                status: Status::Cancelled,
                error: None,
            },
            OrderResponseStatus::Error(err) => Self {
                order_id,
                status: Status::Failed,
                error: Some(Rejected::new(err)),
            },
            other @ (OrderResponseStatus::Resting { .. }
            | OrderResponseStatus::Filled { .. }
            | OrderResponseStatus::WaitingForTrigger
            | OrderResponseStatus::WaitingForFill) => Self {
                order_id,
                status: Status::Failed,
                error: Some(Rejected::new(format!(
                    "unexpected {} status for a cancel",
                    Placement::classify(other).status()
                ))),
            },
        }
    }
}

impl LegRow for CancelLeg {
    const HEADERS: &'static [&'static str] = &["Order ID", "Status"];
    const ERROR_ALWAYS: bool = true;

    fn cells(&self) -> Vec<String> {
        vec![self.order_id.to_string(), self.status.to_string()]
    }

    fn error(&self) -> Option<&Rejected> {
        self.error.as_ref()
    }
}

/// The exchange answered with a different number of statuses than orders sent,
/// so some orders may be live without a matching row.
fn status_count_mismatch(statuses: usize, orders: usize) -> CliError {
    CliError::Internal(anyhow::anyhow!(
        "The exchange returned {statuses} {} for {orders} {}. Check what is live:\n    {CHECK_LIVE_COMMAND}",
        output::format::noun(statuses, "status", "statuses"),
        output::format::noun(orders, "order", "orders"),
    ))
}

/// Each of `legs` with the status the exchange gave it. The one place the
/// count is checked: a short or long answer fails instead of misattributing.
pub(super) fn pair_statuses<T>(
    legs: &[T],
    statuses: Vec<OrderResponseStatus>,
) -> Result<Vec<(&T, OrderResponseStatus)>, CliError> {
    if statuses.len() != legs.len() {
        return Err(status_count_mismatch(statuses.len(), legs.len()));
    }
    Ok(legs.iter().zip(statuses).collect())
}

/// The one status the exchange gave a single order. Anything else is the same
/// count mismatch [`pair_statuses`] reports.
pub(super) fn single_status(
    statuses: Vec<OrderResponseStatus>,
) -> Result<OrderResponseStatus, CliError> {
    match <[OrderResponseStatus; 1]>::try_from(statuses) {
        Ok([status]) => Ok(status),
        Err(statuses) => Err(status_count_mismatch(statuses.len(), 1)),
    }
}

#[derive(Debug, Clone, Copy)]
pub(super) enum BatchVerb {
    PlaceOrders,
    PlaceTpsl,
    CancelAll,
    ModifyOrders,
}

impl BatchVerb {
    /// Leads the headline: "{done} 1 of 2 orders".
    pub(super) fn done(self) -> &'static str {
        match self {
            Self::PlaceOrders | Self::PlaceTpsl => "Placed",
            Self::CancelAll => "Cancelled",
            Self::ModifyOrders => "Modified",
        }
    }

    /// Completes "Couldn't ..." when every leg failed.
    pub(super) fn action(self) -> &'static str {
        match self {
            Self::PlaceOrders => "place orders",
            Self::PlaceTpsl => "place TP/SL orders",
            Self::CancelAll => "cancel all orders",
            Self::ModifyOrders => "modify orders",
        }
    }
}

/// The error (exit 15) that follows a printed partial result: `headline`, then
/// `lead` and the `commands` that say what to do next. Pretty and table runs
/// show it on stderr; in JSON the printed result is the only document.
pub(super) fn partial_results_error(headline: &str, lead: &str, commands: &[String]) -> CliError {
    let commands = commands
        .iter()
        .map(|command| format!("    {command}"))
        .collect::<Vec<_>>()
        .join("\n");
    CliError::PartialResults(format!("{headline}\n\n{lead}\n{commands}"))
}

/// In JSON it is one object that also carries the standard error keys
/// ([`partial_results_fields`]), so the caller prints one document and the top-level
/// error path stays silent for `PartialResults`.
pub(super) struct PartialBatchOutput {
    pub(super) legs: Box<dyn TableData>,
    pub(super) succeeded: usize,
    pub(super) failed: usize,
    pub(super) headline: String,
}

impl PartialBatchOutput {
    pub(super) fn error(&self) -> CliError {
        partial_results_error(
            &self.headline,
            "Check what is live:",
            &[CHECK_LIVE_COMMAND.to_string()],
        )
    }
}

impl TableData for PartialBatchOutput {
    fn headers(&self) -> Vec<&str> {
        self.legs.headers()
    }

    fn rows(&self) -> Vec<Vec<String>> {
        self.legs.rows()
    }

    fn pretty_rows(&self) -> Vec<Vec<String>> {
        self.legs.pretty_rows()
    }

    fn to_json_value(&self) -> serde_json::Value {
        let mut document = partial_results_fields(
            self.headline.clone(),
            vec![NextCommand::new(CHECK_LIVE_COMMAND)],
        );
        document.insert("status".into(), Status::Partial.as_str().into());
        document.insert("atomic".into(), false.into());
        document.insert("succeeded".into(), self.succeeded.into());
        document.insert("failed".into(), self.failed.into());
        document.insert("total".into(), (self.succeeded + self.failed).into());
        document.insert("orders".into(), self.legs.to_json_value());
        serde_json::Value::Object(document)
    }
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct ModifyConfirmation {
    pub(super) coin: String,
    pub(super) status: Status,
    pub(super) order_id: Option<u64>,
    pub(super) cloid: Option<String>,
    pub(super) price: String,
    pub(super) size: String,
}

impl Row for ModifyConfirmation {
    const HEADERS: &'static [&'static str] =
        &["Coin", "Status", "Order ID", "CLOID", "Price", "Size"];

    fn cells(&self) -> Vec<String> {
        vec![
            self.coin.clone(),
            self.status.to_string(),
            id_cell(self.order_id),
            self.cloid.clone().unwrap_or_default(),
            self.price.clone(),
            self.size.clone(),
        ]
    }
}

impl ListRow for ModifyConfirmation {
    const EMPTY: &'static str = "No order modified.";
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct ModifyLeg {
    #[serde(flatten)]
    confirmation: ModifyConfirmation,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) error: Option<Rejected>,
}

impl ModifyLeg {
    /// The leg for one modification. The exchange answers each with an order
    /// status, carrying the replacement's order id, or an error.
    pub(super) fn from_status(
        mut confirmation: ModifyConfirmation,
        status: OrderResponseStatus,
    ) -> Self {
        match status {
            OrderResponseStatus::Error(err) => {
                confirmation.status = Status::Failed;
                Self {
                    confirmation,
                    error: Some(Rejected::new(err)),
                }
            }
            other => {
                confirmation.order_id = other.oid().or(confirmation.order_id);
                Self {
                    confirmation,
                    error: None,
                }
            }
        }
    }
}

impl LegRow for ModifyLeg {
    const HEADERS: &'static [&'static str] = ModifyConfirmation::HEADERS;
    const ERROR_ALWAYS: bool = true;

    fn cells(&self) -> Vec<String> {
        self.confirmation.cells()
    }

    fn error(&self) -> Option<&Rejected> {
        self.error.as_ref()
    }
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct TwapCreateConfirmation {
    pub(super) coin: String,
    pub(super) side: String,
    #[serde(with = "rust_decimal::serde::str")]
    pub(super) size: Decimal,
    pub(super) duration_seconds: u64,
    pub(super) duration_minutes: u64,
    pub(super) status: Status,
    pub(super) twap_id: u64,
}

impl Row for TwapCreateConfirmation {
    const HEADERS: &'static [&'static str] = &[
        "Coin",
        "Side",
        "Size",
        "Duration Seconds",
        "Duration Minutes",
        "Status",
        "TWAP ID",
    ];

    fn cells(&self) -> Vec<String> {
        vec![
            self.coin.clone(),
            self.side.clone(),
            self.size.to_string(),
            self.duration_seconds.to_string(),
            self.duration_minutes.to_string(),
            self.status.to_string(),
            self.twap_id.to_string(),
        ]
    }
}

impl ListRow for TwapCreateConfirmation {
    const EMPTY: &'static str = "No TWAP created.";
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct TwapCancelConfirmation {
    pub(super) coin: String,
    pub(super) status: Status,
    pub(super) twap_id: u64,
}

impl Row for TwapCancelConfirmation {
    const HEADERS: &'static [&'static str] = &["Coin", "Status", "TWAP ID"];

    fn cells(&self) -> Vec<String> {
        vec![
            self.coin.clone(),
            self.status.to_string(),
            self.twap_id.to_string(),
        ]
    }
}

impl ListRow for TwapCancelConfirmation {
    const EMPTY: &'static str = "No TWAP cancelled.";
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct ScheduleCancelConfirmation {
    pub(super) status: Status,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) scheduled_time: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) scheduled_time_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) in_seconds: Option<u64>,
}

impl Row for ScheduleCancelConfirmation {
    const HEADERS: &'static [&'static str] = &[
        "Status",
        "Scheduled Time",
        "Scheduled Time MS",
        "In Seconds",
    ];

    fn cells(&self) -> Vec<String> {
        vec![
            self.status.to_string(),
            self.scheduled_time.clone().unwrap_or_default(),
            self.scheduled_time_ms
                .map(|value| value.to_string())
                .unwrap_or_default(),
            self.in_seconds
                .map(|value| value.to_string())
                .unwrap_or_default(),
        ]
    }
}

impl ListRow for ScheduleCancelConfirmation {
    const EMPTY: &'static str = "No scheduled cancel.";
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::common::order_list::OrderListOutput;

    #[test]
    fn partial_batch_document_carries_the_partial_results_keys() {
        let partial = PartialBatchOutput {
            legs: Box::new(OrderListOutput::from_rows(Vec::new(), "none")),
            succeeded: 1,
            failed: 1,
            headline: "Placed 1 of 2 orders. 1 failed.".to_string(),
        };
        let document = partial.to_json_value();
        assert_eq!(document["category"], "partial_results");
        assert_eq!(document["reason"], "partial_failure");
        assert_eq!(document["exit_code"], 15);
        assert_eq!(document["total"], 2);
        assert_eq!(document["next"][0]["command"], CHECK_LIVE_COMMAND);
        assert!(document.get("next_command").is_none());
    }

    #[test]
    fn statuses_pair_with_legs_only_when_the_counts_agree() {
        let legs = [1, 2];
        let paired = pair_statuses(
            &legs,
            vec![
                OrderResponseStatus::Success,
                OrderResponseStatus::WaitingForFill,
            ],
        )
        .unwrap();
        assert_eq!(paired.len(), 2);
        assert_eq!(*paired[1].0, 2);

        for statuses in [Vec::new(), vec![OrderResponseStatus::Success; 3]] {
            let count = statuses.len();
            let err = pair_statuses(&legs, statuses).unwrap_err();
            assert!(
                err.to_string().contains(&format!(
                    "returned {count} {} for 2 orders",
                    if count == 1 { "status" } else { "statuses" }
                )),
                "{err}"
            );
            assert!(err.to_string().contains(CHECK_LIVE_COMMAND), "{err}");
        }
    }

    #[test]
    fn a_cancel_leg_is_cancelled_only_on_success() {
        let leg = |status| CancelLeg::from_status(7, status);
        assert_eq!(leg(OrderResponseStatus::Success).status, Status::Cancelled);

        let failed = leg(OrderResponseStatus::Error(
            "Order was never placed".to_string(),
        ));
        assert_eq!(failed.status, Status::Failed);
        assert!(failed.error.is_some());

        let unexpected = leg(OrderResponseStatus::Resting {
            oid: 7,
            cloid: None,
        });
        assert_eq!(unexpected.status, Status::Failed);
        assert!(unexpected.error.unwrap().raw().contains("resting"));
    }

    #[test]
    fn a_leg_table_grows_its_error_column_by_one_rule() {
        let sent = OrderResponseStatus::Success;
        let failed = OrderResponseStatus::Error("no".to_string());

        let cancels = Legs::new(vec![CancelLeg::from_status(1, sent.clone())]);
        assert_eq!(cancels.headers(), ["Order ID", "Status", "Error"]);
        assert_eq!(cancels.rows(), [["1", "cancelled", ""]]);

        let modify = |status| {
            ModifyLeg::from_status(
                ModifyConfirmation {
                    coin: "BTC".to_string(),
                    status: Status::Modified,
                    order_id: Some(9),
                    cloid: None,
                    price: "1".to_string(),
                    size: "2".to_string(),
                },
                status,
            )
        };
        let modified = Legs::new(vec![modify(sent), modify(failed)]);
        assert_eq!(modified.headers().last(), Some(&"Error"));
        assert_eq!(modified.rows()[1][1], "failed");
        assert_eq!(modified.rejection_errors().len(), 1);
    }

    #[test]
    fn warnings_are_found_by_header_not_by_position() {
        let rows = paint_column(
            &["Coin", "Warning"],
            vec![vec!["BTC".to_string(), "careful".to_string()]],
            "Warning",
            |warning| !warning.is_empty(),
            |text| format!("<{text}>"),
        );
        assert_eq!(rows[0], ["BTC", "<careful>"]);
    }
}
