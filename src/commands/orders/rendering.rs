use hypersdk::Decimal;
use hypersdk::hypercore::BasicOrder;
use hypersdk::hypercore::types::{OrderResponseStatus, OrderType, TimeInForce};
use serde::Serialize;

use super::exchange::{Placement, Rejected, status_error};
use super::{PreparedOrder, PreparedTpslLeg, TpslGroupingArg};
use crate::errors::{CliError, NextCommand, partial_results_fields};
use crate::output::{self, TableData};

/// What the exchange did with one placed order leg. A fill reports the price
/// and size it filled at; any other status keeps the planned ones. A rejected
/// leg carries the exchange's error, so one bad leg never hides the legs that
/// were placed.
#[derive(Debug, Clone, Serialize)]
struct LegOutcome {
    status: &'static str,
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
        let (status, order_id) = (placed.label(), placed.oid());
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

    fn error_cell(&self) -> String {
        self.error
            .as_ref()
            .map(Rejected::to_string)
            .unwrap_or_default()
    }
}

fn rejections<'a>(errors: impl Iterator<Item = &'a Option<Rejected>>) -> Vec<Rejected> {
    errors.filter_map(Option::clone).collect()
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
            amount: prepared.amount.map(|amount| amount.to_string()),
            amount_unit: prepared
                .amount
                .as_ref()
                .map(|_| prepared.amount_unit.clone()),
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

pub(super) struct OrderConfirmationOutput {
    pub(super) rows: Vec<OrderConfirmation>,
}

impl OrderConfirmationOutput {
    fn has_errors(&self) -> bool {
        self.rows.iter().any(|row| row.outcome.error.is_some())
    }

    pub(super) fn rejection_errors(&self) -> Vec<Rejected> {
        rejections(self.rows.iter().map(|row| &row.outcome.error))
    }
}

impl TableData for OrderConfirmationOutput {
    fn headers(&self) -> Vec<&str> {
        let mut headers = vec![
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
        if self.has_errors() {
            headers.push("Error");
        }
        headers
    }

    fn rows(&self) -> Vec<Vec<String>> {
        let with_errors = self.has_errors();
        self.rows
            .iter()
            .map(|row| {
                let mut cells = vec![
                    row.coin.clone(),
                    row.side.clone(),
                    row.order_type.clone(),
                    row.outcome.status.to_string(),
                    id_cell(row.outcome.order_id),
                    row.outcome.price.clone(),
                    row.trigger_price.clone().unwrap_or_default(),
                    row.outcome.size.clone(),
                    row.amount.clone().unwrap_or_default(),
                    row.amount_unit.clone().unwrap_or_default(),
                    row.tif.clone().unwrap_or_default(),
                    row.reduce_only.to_string(),
                    row.warning.clone().unwrap_or_default(),
                ];
                if with_errors {
                    cells.push(row.outcome.error_cell());
                }
                cells
            })
            .collect()
    }

    fn pretty_rows(&self) -> Vec<Vec<String>> {
        paint_warnings(&self.headers(), self.rows())
    }

    fn to_json_value(&self) -> serde_json::Value {
        serde_json::to_value(&self.rows).unwrap_or_else(|_| serde_json::json!([]))
    }
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct TpslOrderConfirmation {
    grouping: String,
    grouping_wire: String,
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
            grouping_wire: grouping.wire_value().to_string(),
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

pub(super) struct TpslOrderConfirmationOutput {
    pub(super) rows: Vec<TpslOrderConfirmation>,
}

impl TpslOrderConfirmationOutput {
    fn has_errors(&self) -> bool {
        self.rows.iter().any(|row| row.outcome.error.is_some())
    }

    pub(super) fn rejection_errors(&self) -> Vec<Rejected> {
        rejections(self.rows.iter().map(|row| &row.outcome.error))
    }
}

impl TableData for TpslOrderConfirmationOutput {
    fn headers(&self) -> Vec<&str> {
        let mut headers = vec![
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
        if self.has_errors() {
            headers.push("Error");
        }
        headers
    }

    fn rows(&self) -> Vec<Vec<String>> {
        let with_errors = self.has_errors();
        self.rows
            .iter()
            .map(|row| {
                let mut cells = vec![
                    row.grouping.clone(),
                    row.leg.clone(),
                    row.coin.clone(),
                    row.side.clone(),
                    row.order_type.clone(),
                    row.outcome.status.to_string(),
                    id_cell(row.outcome.order_id),
                    row.outcome.price.clone(),
                    row.outcome.size.clone(),
                    row.reduce_only.to_string(),
                    row.tif.clone().unwrap_or_default(),
                    row.warning.clone().unwrap_or_default(),
                ];
                if with_errors {
                    cells.push(row.outcome.error_cell());
                }
                cells
            })
            .collect()
    }

    fn pretty_rows(&self) -> Vec<Vec<String>> {
        paint_warnings(&self.headers(), self.rows())
    }

    fn to_json_value(&self) -> serde_json::Value {
        serde_json::to_value(&self.rows).unwrap_or_else(|_| serde_json::json!([]))
    }
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct CancelConfirmation {
    pub(super) coin: String,
    pub(super) status: String,
    pub(super) order_id: Option<u64>,
    pub(super) cloid: Option<String>,
}

pub(super) struct CancelConfirmationOutput {
    pub(super) rows: Vec<CancelConfirmation>,
}

impl TableData for CancelConfirmationOutput {
    fn headers(&self) -> Vec<&str> {
        vec!["Coin", "Status", "Order ID", "CLOID"]
    }

    fn rows(&self) -> Vec<Vec<String>> {
        self.rows
            .iter()
            .map(|row| {
                vec![
                    row.coin.clone(),
                    row.status.clone(),
                    row.order_id.map(|oid| oid.to_string()).unwrap_or_default(),
                    row.cloid.clone().unwrap_or_default(),
                ]
            })
            .collect()
    }

    fn to_json_value(&self) -> serde_json::Value {
        serde_json::to_value(&self.rows).unwrap_or_else(|_| serde_json::json!([]))
    }
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct CancelAllSummary {
    pub(super) coin: String,
    pub(super) cancelled_orders: usize,
}

pub(super) struct CancelAllSummaryOutput {
    pub(super) rows: Vec<CancelAllSummary>,
}

impl TableData for CancelAllSummaryOutput {
    fn headers(&self) -> Vec<&str> {
        vec!["Coin", "Cancelled Orders"]
    }

    fn rows(&self) -> Vec<Vec<String>> {
        self.rows
            .iter()
            .map(|row| vec![row.coin.clone(), row.cancelled_orders.to_string()])
            .collect()
    }

    fn to_json_value(&self) -> serde_json::Value {
        serde_json::to_value(&self.rows).unwrap_or_else(|_| serde_json::json!([]))
    }
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct CancelLeg {
    pub(super) order_id: u64,
    pub(super) status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) error: Option<Rejected>,
}

impl CancelLeg {
    pub(super) fn from_status(order_id: u64, status: &OrderResponseStatus) -> Self {
        match status {
            OrderResponseStatus::Error(err) => Self {
                order_id,
                status: "failed",
                error: Some(Rejected::new(err)),
            },
            _ => Self {
                order_id,
                status: "cancelled",
                error: None,
            },
        }
    }
}

pub(super) struct CancelLegsOutput {
    pub(super) rows: Vec<CancelLeg>,
}

impl TableData for CancelLegsOutput {
    fn headers(&self) -> Vec<&str> {
        vec!["Order ID", "Status", "Error"]
    }

    fn rows(&self) -> Vec<Vec<String>> {
        self.rows
            .iter()
            .map(|row| {
                vec![
                    row.order_id.to_string(),
                    row.status.to_string(),
                    row.error
                        .as_ref()
                        .map(Rejected::to_string)
                        .unwrap_or_default(),
                ]
            })
            .collect()
    }

    fn to_json_value(&self) -> serde_json::Value {
        serde_json::to_value(&self.rows).unwrap_or_else(|_| serde_json::json!([]))
    }
}

/// Command to run after a partial result to see what is live on the exchange.
pub(super) const CHECK_LIVE_COMMAND: &str = "hyperliquid orders open";

/// The ` --on-behalf-of <selector>` a suggested command needs to act on the
/// same account as this run; empty when the run acts for its own account.
pub(super) fn on_behalf_of_flag(on_behalf_of: Option<&str>) -> String {
    on_behalf_of
        .map(|selector| format!(" --on-behalf-of {selector}"))
        .unwrap_or_default()
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
    pub(super) legs_key: &'static str,
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
        document.insert("status".into(), "partial".into());
        document.insert("atomic".into(), false.into());
        document.insert("succeeded".into(), self.succeeded.into());
        document.insert("failed".into(), self.failed.into());
        document.insert("total".into(), (self.succeeded + self.failed).into());
        document.insert(self.legs_key.into(), self.legs.to_json_value());
        serde_json::Value::Object(document)
    }
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct ModifyConfirmation {
    pub(super) coin: String,
    pub(super) status: String,
    pub(super) order_id: Option<u64>,
    pub(super) cloid: Option<String>,
    pub(super) price: String,
    pub(super) size: String,
}

pub(super) struct ModifyConfirmationOutput {
    pub(super) rows: Vec<ModifyConfirmation>,
}

impl TableData for ModifyConfirmationOutput {
    fn headers(&self) -> Vec<&str> {
        vec!["Coin", "Status", "Order ID", "CLOID", "Price", "Size"]
    }

    fn rows(&self) -> Vec<Vec<String>> {
        self.rows
            .iter()
            .map(|row| {
                vec![
                    row.coin.clone(),
                    row.status.clone(),
                    row.order_id.map(|oid| oid.to_string()).unwrap_or_default(),
                    row.cloid.clone().unwrap_or_default(),
                    row.price.clone(),
                    row.size.clone(),
                ]
            })
            .collect()
    }

    fn to_json_value(&self) -> serde_json::Value {
        serde_json::to_value(&self.rows).unwrap_or_else(|_| serde_json::json!([]))
    }
}

/// One open or historical order, as `orders open`, `orders history` and
/// `account orders` list it.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
struct OrderListRow {
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
}

impl From<BasicOrder> for OrderListRow {
    fn from(order: BasicOrder) -> Self {
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
        }
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
    #[must_use]
    pub fn new(orders: impl IntoIterator<Item = BasicOrder>, empty_message: &'static str) -> Self {
        Self {
            rows: orders.into_iter().map(OrderListRow::from).collect(),
            empty_message,
        }
    }
}

impl TableData for OrderListOutput {
    fn headers(&self) -> Vec<&str> {
        vec![
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
        ]
    }

    fn rows(&self) -> Vec<Vec<String>> {
        self.rows
            .iter()
            .map(|order| {
                vec![
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
                ]
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
        serde_json::to_value(&self.rows).unwrap_or_else(|_| serde_json::json!([]))
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
    pub(super) status: String,
    pub(super) twap_id: u64,
}

pub(super) struct TwapCreateOutput {
    pub(super) rows: Vec<TwapCreateConfirmation>,
}

impl TableData for TwapCreateOutput {
    fn headers(&self) -> Vec<&str> {
        vec![
            "Coin",
            "Side",
            "Size",
            "Duration Seconds",
            "Duration Minutes",
            "Status",
            "TWAP ID",
        ]
    }

    fn rows(&self) -> Vec<Vec<String>> {
        self.rows
            .iter()
            .map(|row| {
                vec![
                    row.coin.clone(),
                    row.side.clone(),
                    row.size.to_string(),
                    row.duration_seconds.to_string(),
                    row.duration_minutes.to_string(),
                    row.status.clone(),
                    row.twap_id.to_string(),
                ]
            })
            .collect()
    }

    fn to_json_value(&self) -> serde_json::Value {
        serde_json::to_value(&self.rows).unwrap_or_else(|_| serde_json::json!([]))
    }
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct TwapCancelConfirmation {
    pub(super) coin: String,
    pub(super) status: String,
    pub(super) twap_id: u64,
}

pub(super) struct TwapCancelOutput {
    pub(super) rows: Vec<TwapCancelConfirmation>,
}

impl TableData for TwapCancelOutput {
    fn headers(&self) -> Vec<&str> {
        vec!["Coin", "Status", "TWAP ID"]
    }

    fn rows(&self) -> Vec<Vec<String>> {
        self.rows
            .iter()
            .map(|row| {
                vec![
                    row.coin.clone(),
                    row.status.clone(),
                    row.twap_id.to_string(),
                ]
            })
            .collect()
    }

    fn to_json_value(&self) -> serde_json::Value {
        serde_json::to_value(&self.rows).unwrap_or_else(|_| serde_json::json!([]))
    }
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct ScheduleCancelConfirmation {
    pub(super) status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) scheduled_time: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) scheduled_time_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) in_seconds: Option<u64>,
}

pub(super) struct ScheduleCancelOutput {
    pub(super) rows: Vec<ScheduleCancelConfirmation>,
}

impl TableData for ScheduleCancelOutput {
    fn headers(&self) -> Vec<&str> {
        vec![
            "Status",
            "Scheduled Time",
            "Scheduled Time MS",
            "In Seconds",
        ]
    }

    fn rows(&self) -> Vec<Vec<String>> {
        self.rows
            .iter()
            .map(|row| {
                vec![
                    row.status.clone(),
                    row.scheduled_time.clone().unwrap_or_default(),
                    row.scheduled_time_ms
                        .map(|value| value.to_string())
                        .unwrap_or_default(),
                    row.in_seconds
                        .map(|value| value.to_string())
                        .unwrap_or_default(),
                ]
            })
            .collect()
    }

    fn to_json_value(&self) -> serde_json::Value {
        serde_json::to_value(&self.rows).unwrap_or_else(|_| serde_json::json!([]))
    }
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
        let empty = OrderListOutput::new(Vec::new(), "No orders.");
        assert!(empty.rows().is_empty());
        assert_eq!(empty.empty_message(), Some("No orders."));
        assert!(empty.to_json_value().as_array().unwrap().is_empty());

        let list = OrderListOutput::new([order(OrderType::Limit, None)], "No orders.");
        assert_eq!(list.rows()[0][7], NO_TIF);
        assert_eq!(list.to_json_value()[0]["tif"], serde_json::Value::Null);
    }

    #[test]
    fn partial_batch_document_carries_the_partial_results_keys() {
        let partial = PartialBatchOutput {
            legs: Box::new(OrderListOutput::new(Vec::new(), "none")),
            legs_key: "orders",
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
