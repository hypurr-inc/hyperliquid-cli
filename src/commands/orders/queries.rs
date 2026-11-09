use hypersdk::Address;
use hypersdk::hypercore::types::{BasicOrder, OrderStatus};
use serde::Deserialize;

use super::args::StatusArgs;
use super::cloid::{cloid_text, parse_lookup_cloid};
use crate::assets::AssetResolver;
use crate::commands::common::order_list::{OrderListOutput, display_row, open_orders_output};
use crate::errors::CliError;
use crate::http_api::ApiClient;
use crate::invocation::Invocation;
use crate::output::JsonValueOutput;
use crate::wire::InfoRequest;

pub(super) async fn open(
    inv: &Invocation,
    resolver: &AssetResolver,
    user: Address,
    dex: Option<&str>,
) -> Result<(), anyhow::Error> {
    inv.print(&open_orders_output(inv.remote()?.api(), resolver, user, dex).await?);
    Ok(())
}

pub(super) async fn status(
    inv: &Invocation,
    user: Address,
    args: &StatusArgs,
) -> Result<(), anyhow::Error> {
    let oid = match (args.oid, args.cloid.as_deref()) {
        (Some(oid), None) => serde_json::json!(oid),
        (None, Some(cloid)) => serde_json::json!(cloid_text(parse_lookup_cloid(cloid)?)),
        _ => {
            return Err(CliError::Configuration(
                "orders status requires --oid or --cloid".to_string(),
            )
            .into());
        }
    };
    let request = InfoRequest::user("orderStatus", user).field("oid", oid);
    let value = inv
        .remote()?
        .api()
        .info::<serde_json::Value>(&request, "loading order status")
        .await?;
    inv.print(&JsonValueOutput::new(value, "No order status."));
    Ok(())
}

pub(super) async fn history(
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
    match api.sdk().historical_orders(user).await? {
        Some(updates) => Ok(updates
            .into_iter()
            .map(|update| HistoricalOrder {
                order: update.order,
                status: Some((status_label(update.status), update.status_timestamp)),
            })
            .collect()),
        None => historical_orders_raw(api, user).await,
    }
}

/// The status as the exchange spells it (`marginCanceled`), not the SDK's `Display`.
fn status_label(status: OrderStatus) -> String {
    match serde_json::to_value(status) {
        Ok(serde_json::Value::String(label)) => label,
        _ => status.to_string(),
    }
}

/// The raw path accepts both the SDK's `OrderUpdate` list and a bare order list.
async fn historical_orders_raw(
    api: &ApiClient,
    user: Address,
) -> Result<Vec<HistoricalOrder>, CliError> {
    let request = InfoRequest::user("historicalOrders", user);
    let items = api
        .info::<Vec<HistoricalOrderItem>>(&request, "loading historical orders")
        .await?;
    Ok(items
        .into_iter()
        .map(HistoricalOrderItem::into_order)
        .collect())
}
