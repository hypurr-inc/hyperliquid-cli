use hypersdk::Decimal;
use hypersdk::hypercore::BasicOrder;
use hypersdk::hypercore::types::{OrderResponseStatus, OrderType, TimeInForce};
use serde::Serialize;

use super::args::TpslGroupingArg;
use super::exchange::{Placement, Rejected, status_error};
use super::prepared::{PreparedOrder, PreparedTpslLeg};
use super::status::Status;
use crate::commands::rows::{ListRow, Row};
use crate::errors::{CliError, NextCommand, partial_results_fields};
use crate::hints::CHECK_LIVE_COMMAND;
use crate::output::{self, TableData};

/// What the exchange did with one placed order leg. A fill reports the price
/// and size it filled at; any other status keeps the planned ones. A rejected
/// leg carries the exchange's error, so one bad leg never hides the legs that
/// were placed.
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

fn paint_column(
    headers: &[&str],
    mut rows: Vec<Vec<String>>,
    header: &str,
    applies: fn(&str) -> bool,
    paint: fn(&str) -> String,
) -> Vec<Vec<String>> {
    if let Some(column) = headers.iter().position(|name| *name == header) {
        for row in &mut rows {
            if let Some(cell) = row.get_mut(column)
                && applies(cell)
            {
                *cell = paint(cell);
            }
        }
    }
    rows
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
    /// The columns before the error column.
    const HEADERS: &'static [&'static str];
    /// The error column shows even when no leg failed.
    const ERROR_ALWAYS: bool = false;

    fn cells(&self) -> Vec<String>;

    fn error(&self) -> Option<&Rejected>;
}

/// The legs of a batch result, as a table and as a JSON array.
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

    /// Build the row for a single order; a rejection is an error.
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

/// What a batch was doing when some of its legs failed.
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

/// Per-leg result of a batch where some legs succeeded and some failed.
///
/// In JSON it is one object that also carries the standard error keys
/// ([`partial_results_fields`]), so the caller prints one document and the
/// top-level error path stays silent for `PartialResults`.
pub(super) struct PartialBatchOutput {
    pub(super) legs: Box<dyn TableData>,
    pub(super) succeeded: usize,
    pub(super) failed: usize,
    pub(super) headline: String,
}

impl PartialBatchOutput {
    /// Error returned after the result document is printed.
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

/// One modification of a batch: the planned replacement and what the
/// exchange said of it.
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

/// One open or historical order, as `orders open`, `orders history` and
/// `account orders` list it.
///
/// The exchange reports `triggerPx: "0.0"` and `triggerCondition: "N/A"` on an
/// order that is not a trigger; those placeholders read as no trigger here.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(super) struct OrderListRow {
    coin: String,
    side: String,
    #[serde(with = "rust_decimal::serde::str")]
    limit_price: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    size: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    original_size: Decimal,
    oid: u64,
    order_type: String,
    tif: Option<String>,
    reduce_only: bool,
    timestamp: u64,
    cloid: Option<String>,
    is_trigger: Option<bool>,
    #[serde(with = "rust_decimal::serde::str_option")]
    trigger_price: Option<Decimal>,
    trigger_condition: Option<String>,
    is_position_tpsl: Option<bool>,
    /// The exchange's lifecycle status, from `historicalOrders` only.
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    status_timestamp: Option<u64>,
    /// The TP/SL orders attached to this one, from `frontendOpenOrders`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    children: Vec<OrderListRow>,
}

impl From<BasicOrder> for OrderListRow {
    fn from(order: BasicOrder) -> Self {
        let is_trigger = order.is_trigger;
        let trigger = is_trigger != Some(false);
        Self {
            coin: order.coin,
            side: order.side.to_string(),
            limit_price: order.limit_px,
            size: order.sz,
            original_size: order.orig_sz,
            oid: order.oid,
            order_type: order_type_label(&order.order_type).to_string(),
            tif: order.tif.map(|tif| tif_label(tif).to_string()),
            reduce_only: order.reduce_only,
            timestamp: order.timestamp,
            cloid: order.cloid.map(|cloid| cloid.to_string()),
            is_trigger,
            trigger_price: order.trigger_px.filter(|_| trigger),
            trigger_condition: order.trigger_condition.filter(|_| trigger),
            is_position_tpsl: order.is_position_tpsl,
            status: None,
            status_timestamp: None,
            children: Vec::new(),
        }
    }
}

impl OrderListRow {
    /// Show `coin` instead of the exchange's spelling, for this order and its children.
    pub(super) fn with_display_coin(mut self, display: &impl Fn(&str) -> String) -> Self {
        self.coin = display(&self.coin);
        self.children = self
            .children
            .into_iter()
            .map(|child| child.with_display_coin(display))
            .collect();
        self
    }

    pub(super) fn with_children(mut self, children: Vec<OrderListRow>) -> Self {
        self.children = children;
        self
    }

    pub(super) fn with_status(mut self, status: String, status_timestamp: u64) -> Self {
        self.status = Some(status);
        self.status_timestamp = Some(status_timestamp);
        self
    }
}

/// The text an order's type has in listings. It is spelled out here, not taken
/// from the SDK's `Debug`, so a renamed SDK variant cannot change the output;
/// a new variant fails to compile until it is given a label.
fn order_type_label(order_type: &OrderType) -> &'static str {
    match order_type {
        OrderType::Limit => "Limit",
        OrderType::Market => "Market",
        OrderType::Trigger => "Trigger",
        OrderType::StopMarket => "StopMarket",
        OrderType::StopLimit => "StopLimit",
        OrderType::TakeProfitMarket => "TakeProfitMarket",
        OrderType::TakeProfitLimit => "TakeProfitLimit",
        OrderType::TrailingStopMarket => "TrailingStopMarket",
        OrderType::TwapSlice => "TwapSlice",
        OrderType::VaultClose => "VaultClose",
        OrderType::SpotDustConversion => "SpotDustConversion",
    }
}

/// The text an order's time in force has in listings; see [`order_type_label`].
fn tif_label(tif: TimeInForce) -> &'static str {
    match tif {
        TimeInForce::Alo => "Alo",
        TimeInForce::Ioc => "Ioc",
        TimeInForce::Gtc => "Gtc",
        TimeInForce::FrontendMarket => "FrontendMarket",
        TimeInForce::LiquidationMarket => "LiquidationMarket",
    }
}

/// A list of orders. `empty_message` is what a run with no orders says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderListOutput {
    rows: Vec<OrderListRow>,
    empty_message: &'static str,
}

impl OrderListOutput {
    pub(super) fn from_rows(rows: Vec<OrderListRow>, empty_message: &'static str) -> Self {
        Self {
            rows,
            empty_message,
        }
    }

    /// Only `orders history` rows carry a status, so only its table has the columns.
    fn has_status(&self) -> bool {
        self.rows.iter().any(|order| order.status.is_some())
    }
}

impl TableData for OrderListOutput {
    fn headers(&self) -> Vec<&str> {
        let mut headers = vec![
            "Coin",
            "Side",
            "Limit Price",
            "Size",
            "Original Size",
            "OID",
            "Type",
            "TIF",
            "Reduce Only",
            "Timestamp",
            "Trigger Price",
            "Trigger Condition",
            "Position TP/SL",
            "Children",
        ];
        if self.has_status() {
            headers.extend(["Status", "Status Time"]);
        }
        headers
    }

    fn rows(&self) -> Vec<Vec<String>> {
        let has_status = self.has_status();
        self.rows
            .iter()
            .map(|order| {
                let mut row = vec![
                    order.coin.clone(),
                    order.side.clone(),
                    order.limit_price.to_string(),
                    order.size.to_string(),
                    order.original_size.to_string(),
                    order.oid.to_string(),
                    order.order_type.clone(),
                    order.tif.clone().unwrap_or_else(|| NO_TIF.to_string()),
                    order.reduce_only.to_string(),
                    order.timestamp.to_string(),
                    order
                        .trigger_price
                        .map(|price| price.to_string())
                        .unwrap_or_default(),
                    order.trigger_condition.clone().unwrap_or_default(),
                    order
                        .is_position_tpsl
                        .map(|value| value.to_string())
                        .unwrap_or_default(),
                    order
                        .children
                        .iter()
                        .map(|child| child.oid.to_string())
                        .collect::<Vec<_>>()
                        .join(","),
                ];
                if has_status {
                    row.push(order.status.clone().unwrap_or_default());
                    row.push(
                        order
                            .status_timestamp
                            .map(|time| time.to_string())
                            .unwrap_or_default(),
                    );
                }
                row
            })
            .collect()
    }

    fn empty_message(&self) -> Option<&str> {
        Some(self.empty_message)
    }

    fn pretty_rows(&self) -> Vec<Vec<String>> {
        paint_column(
            &self.headers(),
            self.rows(),
            "TIF",
            |tif| tif == NO_TIF,
            output::colors::gray,
        )
    }

    fn to_json_value(&self) -> serde_json::Value {
        output::serialize_row(&self.rows)
    }
}

/// The TIF cell of an order that has none, such as a trigger order.
const NO_TIF: &str = "n/a";

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
    use hypersdk::hypercore::types::Side;

    use super::*;

    fn order(order_type: OrderType, tif: Option<TimeInForce>) -> BasicOrder {
        BasicOrder {
            timestamp: 1,
            coin: "BTC".to_string(),
            side: Side::Bid,
            limit_px: Decimal::from(100),
            sz: Decimal::ONE,
            oid: 5,
            orig_sz: Decimal::ONE,
            cloid: None,
            order_type,
            tif,
            reduce_only: false,
            is_trigger: None,
            trigger_px: None,
            trigger_condition: None,
            is_position_tpsl: None,
        }
    }

    fn list(
        orders: impl IntoIterator<Item = BasicOrder>,
        empty_message: &'static str,
    ) -> OrderListOutput {
        OrderListOutput::from_rows(
            orders.into_iter().map(OrderListRow::from).collect(),
            empty_message,
        )
    }

    #[test]
    fn order_labels_match_the_sdk_debug_text_they_replaced() {
        for (order_type, tif) in [
            (OrderType::Limit, TimeInForce::Alo),
            (OrderType::Market, TimeInForce::Ioc),
            (OrderType::Trigger, TimeInForce::Gtc),
            (OrderType::StopMarket, TimeInForce::FrontendMarket),
            (OrderType::StopLimit, TimeInForce::LiquidationMarket),
            (OrderType::TakeProfitMarket, TimeInForce::Gtc),
            (OrderType::TakeProfitLimit, TimeInForce::Gtc),
            (OrderType::TrailingStopMarket, TimeInForce::Gtc),
            (OrderType::TwapSlice, TimeInForce::Gtc),
            (OrderType::VaultClose, TimeInForce::Gtc),
            (OrderType::SpotDustConversion, TimeInForce::Gtc),
        ] {
            assert_eq!(order_type_label(&order_type), format!("{order_type:?}"));
            assert_eq!(tif_label(tif), format!("{tif:?}"));
        }
    }

    #[test]
    fn order_list_is_empty_with_its_message_and_missing_tif_reads_na() {
        let empty = list(Vec::new(), "No orders.");
        assert!(empty.rows().is_empty());
        assert_eq!(empty.empty_message(), Some("No orders."));
        assert!(empty.to_json_value().as_array().unwrap().is_empty());

        let list = list([order(OrderType::Limit, None)], "No orders.");
        assert_eq!(list.rows()[0][7], NO_TIF);
        assert_eq!(list.to_json_value()[0]["tif"], serde_json::Value::Null);
    }

    #[test]
    fn an_order_row_lists_trigger_fields_children_and_status_and_a_plain_order_lists_none() {
        let mut stop = order(OrderType::StopMarket, None);
        stop.is_trigger = Some(true);
        stop.trigger_px = Some(Decimal::from(95));
        stop.trigger_condition = Some("Price below 95".to_string());
        stop.is_position_tpsl = Some(false);
        let child = OrderListRow::from(stop.clone());
        let parent = OrderListRow::from(stop)
            .with_children(vec![child])
            .with_status("triggered".to_string(), 7);
        let json = serde_json::to_value(&parent).unwrap();
        assert_eq!(json["trigger_price"], "95");
        assert_eq!(json["trigger_condition"], "Price below 95");
        assert_eq!(json["is_trigger"], true);
        assert_eq!(json["children"][0]["oid"], 5);
        assert_eq!(json["status"], "triggered");
        assert_eq!(json["status_timestamp"], 7);

        let mut plain = order(OrderType::Limit, Some(TimeInForce::Gtc));
        plain.is_trigger = Some(false);
        plain.trigger_px = Some(Decimal::ZERO);
        plain.trigger_condition = Some("N/A".to_string());
        let json = serde_json::to_value(OrderListRow::from(plain)).unwrap();
        assert_eq!(json["trigger_price"], serde_json::Value::Null);
        assert_eq!(json["trigger_condition"], serde_json::Value::Null);
        assert!(json.get("children").is_none());
        assert!(json.get("status").is_none());
    }

    #[test]
    fn only_a_list_with_a_status_has_the_status_columns() {
        let plain = list([order(OrderType::Limit, None)], "No orders.");
        assert!(!plain.headers().contains(&"Status"));
        let with_status = OrderListOutput::from_rows(
            vec![OrderListRow::from(order(OrderType::Limit, None)).with_status("filled".into(), 3)],
            "No orders.",
        );
        assert_eq!(with_status.headers().len(), with_status.rows()[0].len());
        assert_eq!(with_status.rows()[0].last().unwrap(), "3");
    }

    #[test]
    fn partial_batch_document_carries_the_partial_results_keys() {
        let partial = PartialBatchOutput {
            legs: Box::new(list(Vec::new(), "none")),
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
