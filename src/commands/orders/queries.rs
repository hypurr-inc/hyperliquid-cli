use hypersdk::Address;
use hypersdk::hypercore::types::{BasicOrder, OrderStatus};
use serde::{Deserialize, Serialize};

use super::args::StatusArgs;
use super::cloid::parse_lookup_cloid;
use super::rendering::{OrderListOutput, OrderListRow};
use crate::assets::AssetResolver;
use crate::errors::{CliError, from_sdk};
use crate::http_api::ApiClient;
use crate::invocation::Invocation;
use crate::output::JsonValueOutput;
use crate::wire::InfoRequest;

pub async fn open(
    inv: &Invocation,
    resolver: &AssetResolver,
    user: Address,
    dex: Option<&str>,
) -> Result<(), anyhow::Error> {
    inv.print(&open_orders_output(inv.remote()?.api(), resolver, user, dex).await?);
    Ok(())
}

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

/// Fetch an account's open orders, trigger fields and TP/SL children
/// included, without printing them. `account orders` and `orders open` list
/// the same rows.
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

pub async fn status(
    inv: &Invocation,
    user: Address,
    args: &StatusArgs,
) -> Result<(), anyhow::Error> {
    let oid = match (args.oid, args.cloid.as_deref()) {
        (Some(oid), None) => serde_json::json!(oid),
        (None, Some(cloid)) => {
            let cloid = parse_lookup_cloid(cloid)?;
            serde_json::json!(format!("{cloid:#x}"))
        }
        _ => {
            return Err(CliError::Configuration(
                "orders status requires --oid or --cloid".to_string(),
            )
            .into());
        }
    };
    let request = OrderStatusRequest {
        request_type: "orderStatus",
        user,
        oid,
    };
    let value = inv
        .remote()?
        .api()
        .info::<serde_json::Value>(&request, "loading order status")
        .await?;
    inv.print(&JsonValueOutput::new(value, "No order status."));
    Ok(())
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct OrderStatusRequest {
    #[serde(rename = "type")]
    request_type: &'static str,
    user: Address,
    oid: serde_json::Value,
}

pub async fn history(
    inv: &Invocation,
    resolver: &AssetResolver,
    user: Address,
) -> Result<(), anyhow::Error> {
    let rows = historical_orders(inv.remote()?.api(), user)
        .await?
        .into_iter()
        .map(|entry| {
            let row = display_row(resolver, entry.order);
            match entry.status {
                Some((status, time)) => row.with_status(status, time),
                None => row,
            }
        })
        .collect();

    inv.print(&OrderListOutput::from_rows(rows, "No historical orders."));
    Ok(())
}

fn display_row(resolver: &AssetResolver, order: BasicOrder) -> OrderListRow {
    OrderListRow::from(order).with_display_coin(&|coin| resolver.display_coin(coin))
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct HistoricalOrdersRequest {
    #[serde(rename = "type")]
    request_type: &'static str,
    user: Address,
}

/// An order from `historicalOrders` with its status and the time it took it.
/// A bare order list, which has no status, reads with `None`.
struct HistoricalOrder {
    order: BasicOrder,
    status: Option<(String, u64)>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum HistoricalOrderItem {
    Wrapped {
        order: BasicOrder,
        status: String,
        #[serde(rename = "statusTimestamp")]
        status_timestamp: u64,
    },
    Plain(BasicOrder),
}

impl HistoricalOrderItem {
    fn into_order(self) -> HistoricalOrder {
        match self {
            Self::Wrapped {
                order,
                status,
                status_timestamp,
            } => HistoricalOrder {
                order,
                status: Some((status, status_timestamp)),
            },
            Self::Plain(order) => HistoricalOrder {
                order,
                status: None,
            },
        }
    }
}

async fn historical_orders(
    api: &ApiClient,
    user: Address,
) -> Result<Vec<HistoricalOrder>, CliError> {
    match api.sdk().historical_orders(user).await {
        Ok(updates) => Ok(updates
            .into_iter()
            .map(|update| HistoricalOrder {
                order: update.order,
                status: Some((status_label(update.status), update.status_timestamp)),
            })
            .collect()),
        Err(err) if should_fallback_to_raw_historical_orders(&err) => {
            historical_orders_raw(api, user).await
        }
        Err(err) => Err(from_sdk(err)),
    }
}

/// The status as the exchange spells it (`marginCanceled`), not the SDK's `Display`.
fn status_label(status: OrderStatus) -> String {
    match serde_json::to_value(status) {
        Ok(serde_json::Value::String(label)) => label,
        _ => status.to_string(),
    }
}

/// hypersdk decodes `historicalOrders` as `OrderUpdate<BasicOrder>`; when the
/// payload is a bare order list the decode fails with a serde error, and the raw
/// path accepts both shapes.
fn should_fallback_to_raw_historical_orders(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause
            .downcast_ref::<serde_json::Error>()
            .is_some_and(|decode| decode.classify() == serde_json::error::Category::Data)
    })
}

async fn historical_orders_raw(
    api: &ApiClient,
    user: Address,
) -> Result<Vec<HistoricalOrder>, CliError> {
    let request = HistoricalOrdersRequest {
        request_type: "historicalOrders",
        user,
    };
    let items = api
        .info::<Vec<HistoricalOrderItem>>(&request, "loading historical orders")
        .await?;
    Ok(items
        .into_iter()
        .map(HistoricalOrderItem::into_order)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode_error(text: &str) -> anyhow::Error {
        anyhow::Error::from(serde_json::from_str::<Vec<u8>>(text).unwrap_err())
            .context("[historical_orders] body=...")
    }

    #[test]
    fn only_a_shape_mismatch_falls_back_to_the_raw_historical_list() {
        assert!(should_fallback_to_raw_historical_orders(&decode_error(
            r#"{"not":"a list"}"#
        )));
        assert!(!should_fallback_to_raw_historical_orders(&decode_error(
            "[1, 2"
        )));
        assert!(!should_fallback_to_raw_historical_orders(&anyhow::anyhow!(
            "connection reset"
        )));
    }
}
