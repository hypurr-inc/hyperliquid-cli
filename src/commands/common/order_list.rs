//! The open and historical order list `orders open`, `orders history` and
//! `account orders` print, and the `frontendOpenOrders` read behind the first
//! and the last.
//!
//! Layer rule: below the commands; `account` does not reach into `orders` for
//! it.

use hypersdk::Address;
use hypersdk::Decimal;
use hypersdk::hypercore::BasicOrder;
use hypersdk::hypercore::types::{OrderType, TimeInForce};
use serde::{Deserialize, Serialize};

use crate::assets::AssetResolver;
use crate::commands::common::rows::paint_column;
use crate::http_api::ApiClient;
use crate::output::{self, TableData};
use crate::wire::InfoRequest;

/// The exchange reports `triggerPx: "0.0"` and `triggerCondition: "N/A"` on an
/// order that is not a trigger; those placeholders read as no trigger here.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct OrderListRow {
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
    pub(crate) fn with_display_coin(mut self, display: &impl Fn(&str) -> String) -> Self {
        self.coin = display(&self.coin);
        self.children = self
            .children
            .into_iter()
            .map(|child| child.with_display_coin(display))
            .collect();
        self
    }

    pub(crate) fn with_children(mut self, children: Vec<OrderListRow>) -> Self {
        self.children = children;
        self
    }

    pub(crate) fn with_status(mut self, status: String, status_timestamp: u64) -> Self {
        self.status = Some(status);
        self.status_timestamp = Some(status_timestamp);
        self
    }
}

/// Spelled out here, not taken from the SDK's `Debug`, so a renamed SDK variant cannot
/// change the output; a new variant fails to compile until it is given a label.
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

fn tif_label(tif: TimeInForce) -> &'static str {
    match tif {
        TimeInForce::Alo => "Alo",
        TimeInForce::Ioc => "Ioc",
        TimeInForce::Gtc => "Gtc",
        TimeInForce::FrontendMarket => "FrontendMarket",
        TimeInForce::LiquidationMarket => "LiquidationMarket",
    }
}

/// `empty_message` is what a run with no orders says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderListOutput {
    rows: Vec<OrderListRow>,
    empty_message: &'static str,
}

impl OrderListOutput {
    pub(crate) fn from_rows(rows: Vec<OrderListRow>, empty_message: &'static str) -> Self {
        Self {
            rows,
            empty_message,
        }
    }

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

const NO_TIF: &str = "n/a";

/// An open order as `frontendOpenOrders` lists it: the SDK's order plus the
/// child TP/SL orders it drops.
#[derive(Debug, Deserialize)]
struct FrontendOrder {
    #[serde(flatten)]
    order: BasicOrder,
    #[serde(default)]
    children: Vec<FrontendOrder>,
}

impl FrontendOrder {
    fn into_row(self, resolver: &AssetResolver) -> OrderListRow {
        let children = self
            .children
            .into_iter()
            .map(|child| child.into_row(resolver))
            .collect();
        display_row(resolver, self.order).with_children(children)
    }
}

/// Trigger fields and TP/SL children are included; `account orders` and `orders open`
/// list the same rows.
pub async fn open_orders_output(
    api: &ApiClient,
    resolver: &AssetResolver,
    user: Address,
    dex: Option<&str>,
) -> Result<OrderListOutput, anyhow::Error> {
    let request =
        InfoRequest::user("frontendOpenOrders", user).field_if("dex", dex.map(str::to_string));
    let orders = api
        .info::<Vec<FrontendOrder>>(&request, "loading open orders")
        .await?;
    Ok(OrderListOutput::from_rows(
        orders
            .into_iter()
            .map(|order| order.into_row(resolver))
            .collect(),
        "No open orders.",
    ))
}

pub(crate) fn display_row(resolver: &AssetResolver, order: BasicOrder) -> OrderListRow {
    OrderListRow::from(order).with_display_coin(&|coin| resolver.display_coin(coin))
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
}
