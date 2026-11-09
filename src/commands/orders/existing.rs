use hypersdk::hypercore::types::{
    Action, BasicOrder, BatchCancel, BatchCancelCloid, BatchModify, Cancel, CancelByCloid, Modify,
    OrderRequest, OrderType, OrderTypePlacement, Side, TimeInForce, TpSl,
};
use hypersdk::{Address, Decimal};

use super::args::{BatchModifyArgs, CancelAllArgs, CancelArgs, ModifyArgs};
use super::cloid::{cloid_text, wire_cloid};
use super::identifier::{BatchModifyOrder, OrderIdentifier};
use super::live_order::CloidAsset;
use super::preview::OrderDryRunPlan;
use super::rendering::ModifyConfirmation;
use super::status::Status;
use super::validation::MarketPrecision;
use crate::assets::{
    AssetQuery, AssetResolver, ListedSpot, Market, outcome_coin, parse_asset_query,
    qualify_dex_asset,
};
use crate::errors::CliError;
use crate::hints::{
    CANCEL_ORDER_COMMAND, CANCEL_TWAP_COMMAND, CREATE_TWAP_COMMAND, TRAILING_STOP_COMMAND,
};
use crate::http_api::ApiClient;
use crate::invocation::Invocation;

async fn lookup_order(
    client: &ApiClient,
    user: Address,
    identifier: &OrderIdentifier,
) -> Result<BasicOrder, CliError> {
    let order = client
        .sdk()
        .order_status(user, identifier.to_status_identifier())
        .await?
        .ok_or_else(|| CliError::Unsupported(format!("unknown order {}", identifier.display())))?;
    Ok(order.order)
}

struct OrderAsset {
    index: usize,
    precision: Option<MarketPrecision>,
}

impl OrderAsset {
    fn price(&self, price: Decimal, coin: &str) -> Result<Decimal, CliError> {
        self.precision
            .map_or(Ok(price), |precision| precision.price(price, coin))
    }

    fn size(&self, size: Decimal, coin: &str) -> Result<Decimal, CliError> {
        self.precision
            .map_or(Ok(size), |precision| precision.size(size, coin))
    }
}

async fn order_asset(
    client: &ApiClient,
    resolver: &AssetResolver,
    order: &BasicOrder,
) -> Result<OrderAsset, CliError> {
    if let Some(spot) = resolver.listed_spot(&order.coin) {
        return Ok(OrderAsset {
            index: spot.asset_index(),
            precision: match spot {
                ListedSpot::Known(spot) => Some(MarketPrecision::of(&Market::Spot(spot))),
                ListedSpot::Unknown { .. } => None,
            },
        });
    }

    let market = resolver.resolve_or_fetch(client, &order.coin).await?;
    Ok(OrderAsset {
        index: market.asset_index()?,
        precision: Some(MarketPrecision::of(&market)),
    })
}

pub(super) struct CancelOrderPlan {
    pub(super) action: Action,
    pub(super) coin: String,
    pub(super) order_id: Option<u64>,
    pub(super) cloid: Option<String>,
}

pub(super) async fn prepare_cancel_order_plan(
    client: &ApiClient,
    resolver: &AssetResolver,
    user: Address,
    args: &CancelArgs,
) -> Result<CancelOrderPlan, CliError> {
    let identifier = OrderIdentifier::for_cancel(args.order_id, args.cloid.as_deref())?;
    let order = lookup_order(client, user, &identifier).await?;
    let asset = order_asset(client, resolver, &order).await?.index;

    let action = match &identifier {
        OrderIdentifier::Oid(oid) => Action::Cancel(BatchCancel {
            cancels: vec![Cancel { asset, oid: *oid }],
            fast: false,
        }),
        OrderIdentifier::Cloid { parsed, .. } => Action::CancelByCloid(BatchCancelCloid {
            cancels: vec![CancelByCloid {
                asset: CloidAsset::new(asset)?.index(),
                cloid: *parsed,
            }],
            fast: false,
        }),
    };

    Ok(CancelOrderPlan {
        action,
        coin: resolver.display_coin(&order.coin),
        order_id: args.order_id,
        cloid: args.cloid.clone(),
    })
}

pub(super) fn cancel_dry_run_plan(args: &CancelArgs) -> Result<OrderDryRunPlan, CliError> {
    let identifier = OrderIdentifier::for_cancel(args.order_id, args.cloid.as_deref())?;
    Ok(OrderDryRunPlan::new(
        "cancel_order",
        serde_json::json!({
            "order_id": args.order_id,
            "cloid": args.cloid,
            "identifier": identifier.display(),
        }),
    ))
}

/// What `cancel-all --coin` cancels: orders on `coin`, as the exchange lists
/// them, which sit in the open orders of `dex` (the default dex when `None`).
pub(super) struct CoinFilter {
    pub(super) coin: String,
    pub(super) dex: Option<String>,
}

impl CoinFilter {
    fn on_default_dex(coin: String) -> Self {
        Self { coin, dex: None }
    }
}

/// The filter `--coin` and `--dex` name, or `None` for every open order. The
/// market must exist: an unknown coin or dex fails here instead of listing an
/// empty set of orders to cancel.
pub(super) async fn resolve_cancel_all_coin_filter(
    inv: &Invocation,
    client: &ApiClient,
    resolver: &AssetResolver,
    args: &CancelAllArgs,
) -> Result<Option<CoinFilter>, CliError> {
    let Some(coin) = args.coin.as_deref() else {
        return Ok(None);
    };
    if let Some(spot) = resolver.listed_spot(coin) {
        // `@N` is how a spot pair is listed, except PURR/USDC, which the
        // exchange lists by name.
        return Ok(Some(CoinFilter::on_default_dex(spot.info_coin())));
    }

    let query = match parse_asset_query(coin) {
        AssetQuery::Outcome(_) => coin.to_string(),
        _ => {
            let dex = inv.dex_name(args.dex.as_ref()).await?;
            qualify_dex_asset(dex.as_deref(), coin)
        }
    };
    Ok(Some(
        match resolver.resolve_or_fetch(client, &query).await? {
            Market::Perp(perp) => CoinFilter {
                coin: perp.display_name(),
                dex: perp.dex.clone(),
            },
            Market::Spot(spot) => CoinFilter::on_default_dex(spot.info_coin()),
            Market::Outcome(outcome) => {
                CoinFilter::on_default_dex(outcome_coin(outcome.parse()?.encoding))
            }
        },
    ))
}

fn filter_orders_by_coin(orders: Vec<BasicOrder>, coin: Option<&str>) -> Vec<BasicOrder> {
    let Some(coin) = coin else {
        return orders;
    };
    orders
        .into_iter()
        .filter(|order| order.coin.eq_ignore_ascii_case(coin))
        .collect()
}

pub(super) struct CancelAllOrdersPlan {
    cancels: Vec<Cancel>,
    pub(super) summary_coin: String,
}

impl CancelAllOrdersPlan {
    pub(super) fn order_ids(&self) -> Vec<u64> {
        self.cancels.iter().map(|cancel| cancel.oid).collect()
    }

    /// The cancel to send, or `None` when there is nothing to cancel.
    pub(super) fn action(&self) -> Option<Action> {
        (!self.cancels.is_empty()).then(|| {
            Action::Cancel(BatchCancel {
                cancels: self.cancels.clone(),
                fast: false,
            })
        })
    }
}

pub(super) async fn prepare_cancel_all_orders_plan(
    inv: &Invocation,
    client: &ApiClient,
    resolver: &AssetResolver,
    user: Address,
    args: &CancelAllArgs,
) -> Result<CancelAllOrdersPlan, CliError> {
    let coin_filter = resolve_cancel_all_coin_filter(inv, client, resolver, args).await?;
    let open_orders = client
        .sdk()
        .open_orders(
            user,
            coin_filter.as_ref().and_then(|filter| filter.dex.clone()),
        )
        .await?;
    let orders = filter_orders_by_coin(
        open_orders,
        coin_filter.as_ref().map(|filter| filter.coin.as_str()),
    );

    let mut cancels = Vec::with_capacity(orders.len());
    for order in &orders {
        cancels.push(Cancel {
            asset: order_asset(client, resolver, order).await?.index,
            oid: order.oid,
        });
    }

    Ok(CancelAllOrdersPlan {
        cancels,
        summary_coin: args.coin.clone().unwrap_or_else(|| "ALL".to_string()),
    })
}

pub(super) async fn cancel_all_dry_run_plan(
    inv: &Invocation,
    client: &ApiClient,
    resolver: &AssetResolver,
    args: &CancelAllArgs,
) -> Result<OrderDryRunPlan, CliError> {
    let coin_filter = resolve_cancel_all_coin_filter(inv, client, resolver, args).await?;
    Ok(OrderDryRunPlan::new(
        "cancel_open_orders",
        serde_json::json!({
            "coin": args.coin,
            "dex": args.dex,
            "resolved_coin_filter": coin_filter.map(|filter| filter.coin),
        }),
    ))
}

pub(super) struct ModifyOrderPlan {
    pub(super) modify: Modify,
    pub(super) confirmation: ModifyConfirmation,
}

pub(super) fn validate_modify_args(args: &ModifyArgs) -> Result<OrderIdentifier, CliError> {
    let identifier = OrderIdentifier::for_modify(args.order_id, args.cloid.as_deref())?;
    super::validation::positive_price(args.price, "price")?;
    super::validation::positive_price(args.trigger_price, "trigger-price")?;
    super::validation::positive_quantity(args.size, "size")?;
    if args.price.is_none() && args.trigger_price.is_none() && args.size.is_none() {
        return Err(CliError::Configuration(
            "orders modify requires --price, --trigger-price, or --size".to_string(),
        ));
    }
    Ok(identifier)
}

pub(super) async fn prepare_modify_order_plan(
    client: &ApiClient,
    resolver: &AssetResolver,
    user: Address,
    identifier: &OrderIdentifier,
    args: &ModifyArgs,
    command: &str,
) -> Result<ModifyOrderPlan, CliError> {
    let order = lookup_order(client, user, identifier).await?;
    let asset = order_asset(client, resolver, &order).await?;
    let coin = resolver.display_coin(&order.coin);
    let new_price = args
        .price
        .map(|price| asset.price(price, &coin))
        .transpose()?;
    let new_trigger = args
        .trigger_price
        .map(|price| asset.price(price, &coin))
        .transpose()?;
    let new_size = args.size.map(|size| asset.size(size, &coin)).transpose()?;
    let price = new_price.unwrap_or(order.limit_px);
    let size = new_size.unwrap_or(order.sz);
    let order_type = order_type_placement(&order, new_trigger, new_price)?;
    // The exchange rejects an action hashed with `a: false` unless the
    // replacement is a non-trigger ALO or a non-executable GTC order, so fail
    // here rather than after the cancel succeeds.
    if !args.always_place {
        let needs_always_place = match &order_type {
            OrderTypePlacement::Trigger { .. } => true,
            OrderTypePlacement::Limit { tif } => {
                matches!(tif, TimeInForce::Ioc | TimeInForce::FrontendMarket)
            }
        };
        if needs_always_place {
            return Err(CliError::Configuration(format!(
                "{command} without --always-place can only place an ALO or non-executable GTC replacement; add --always-place to modify trigger orders or place an IOC replacement"
            )));
        }
    }
    let replacement = OrderRequest {
        asset: asset.index,
        is_buy: matches!(order.side, Side::Bid),
        limit_px: price,
        sz: size,
        reduce_only: order.reduce_only,
        order_type,
        cloid: wire_cloid(order.cloid),
    };
    Ok(ModifyOrderPlan {
        modify: Modify {
            oid: identifier.to_status_identifier(),
            order: replacement,
        },
        confirmation: ModifyConfirmation {
            coin,
            status: Status::Modified,
            order_id: Some(order.oid),
            cloid: args.cloid.clone().or_else(|| order.cloid.map(cloid_text)),
            price: price.to_string(),
            size: size.to_string(),
        },
    })
}

pub(super) fn modify_dry_run_plan(args: &ModifyArgs) -> OrderDryRunPlan {
    OrderDryRunPlan::new(
        "modify_order",
        serde_json::json!({
            "order_id": args.order_id,
            "cloid": args.cloid,
            "price": args.price.map(|value| value.to_string()),
            "trigger_price": args.trigger_price.map(|value| value.to_string()),
            "size": args.size.map(|value| value.to_string()),
            "always_place": args.always_place,
        }),
    )
}

pub(super) fn batch_modify_action(modifies: Vec<Modify>, always_place: bool) -> Action {
    Action::BatchModify(BatchModify {
        modifies,
        always_place,
    })
}

pub(super) fn batch_modify_dry_run_plan(
    orders: &[BatchModifyOrder],
    args: &BatchModifyArgs,
) -> OrderDryRunPlan {
    let modifications: Vec<_> = orders
        .iter()
        .enumerate()
        .map(|(index, order)| {
            let modify = order.modify_args(args.always_place);
            serde_json::json!({
                "leg_index": index,
                "order_id": modify.order_id,
                "cloid": modify.cloid,
                "price": modify.price.map(|value| value.to_string()),
                "trigger_price": modify.trigger_price.map(|value| value.to_string()),
                "size": modify.size.map(|value| value.to_string()),
            })
        })
        .collect();
    OrderDryRunPlan::new(
        "modify_orders",
        serde_json::json!({
            "orders_file": args.orders_file,
            "order_count": orders.len(),
            "always_place": args.always_place,
            "modifications": modifications,
        }),
    )
}

pub(super) fn order_type_placement(
    order: &BasicOrder,
    trigger_price: Option<Decimal>,
    legacy_price: Option<Decimal>,
) -> Result<OrderTypePlacement, CliError> {
    // A market trigger keeps the order's own trigger price; its limit price is
    // not the trigger and would silently move it on a size-only modify.
    let market_trigger = |tpsl| OrderTypePlacement::Trigger {
        is_market: true,
        trigger_px: trigger_price
            .or(legacy_price)
            .or(order.trigger_px)
            .unwrap_or(order.limit_px),
        tpsl,
    };
    match order.order_type {
        // A generic `Trigger` order reports no TP/SL side, so it is replaced as a stop.
        OrderType::StopMarket | OrderType::Trigger => Ok(market_trigger(TpSl::Sl)),
        OrderType::TakeProfitMarket => Ok(market_trigger(TpSl::Tp)),
        OrderType::StopLimit => Ok(OrderTypePlacement::Trigger {
            is_market: false,
            trigger_px: require_modify_trigger_price(trigger_price, "stop-limit")?,
            tpsl: TpSl::Sl,
        }),
        OrderType::TakeProfitLimit => Ok(OrderTypePlacement::Trigger {
            is_market: false,
            trigger_px: require_modify_trigger_price(trigger_price, "take-limit")?,
            tpsl: TpSl::Tp,
        }),
        OrderType::TrailingStopMarket => Err(CliError::Unsupported(format!(
            "orders modify can't change a Trailing Stop Market order. Cancel it with `{CANCEL_ORDER_COMMAND} <OID>`, then place a new one with `{TRAILING_STOP_COMMAND}`."
        ))),
        OrderType::TwapSlice => Err(CliError::Unsupported(format!(
            "orders modify can't change a Twap Slice order. Cancel the TWAP with `{CANCEL_TWAP_COMMAND}`, then re-create it with `{CREATE_TWAP_COMMAND}`."
        ))),
        OrderType::VaultClose => Err(CliError::Unsupported(format!(
            "orders modify can't change a Vault Close order. The exchange places it when a vault closes positions; cancel it with `{CANCEL_ORDER_COMMAND} <OID>` and place a regular order instead."
        ))),
        OrderType::SpotDustConversion => Err(CliError::Unsupported(format!(
            "orders modify can't change a Spot Dust Conversion order. Cancel it with `{CANCEL_ORDER_COMMAND} <OID>` and place a regular order instead."
        ))),
        OrderType::Limit | OrderType::Market => {
            if trigger_price.is_some() {
                return Err(CliError::Configuration(
                    "orders modify --trigger-price only applies to trigger orders".to_string(),
                ));
            }
            Ok(OrderTypePlacement::Limit {
                tif: order.tif.unwrap_or(TimeInForce::Gtc),
            })
        }
    }
}

fn require_modify_trigger_price(
    trigger_price: Option<Decimal>,
    order_type: &'static str,
) -> Result<Decimal, CliError> {
    trigger_price.ok_or_else(|| {
        CliError::Configuration(format!(
            "orders modify requires --trigger-price when modifying {order_type} orders so the trigger price is preserved"
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn basic_order(order_type: OrderType) -> BasicOrder {
        BasicOrder {
            timestamp: 0,
            coin: "BTC".to_string(),
            side: Side::Bid,
            limit_px: Decimal::from(50_000),
            sz: Decimal::ONE,
            oid: 7,
            orig_sz: Decimal::ONE,
            cloid: None,
            order_type,
            tif: None,
            reduce_only: false,
            is_trigger: None,
            trigger_px: None,
            trigger_condition: None,
            is_position_tpsl: None,
        }
    }

    #[test]
    fn modify_rejects_order_types_that_cannot_be_replaced() {
        for (order_type, name, hint) in [
            (
                OrderType::TrailingStopMarket,
                "Trailing Stop Market",
                "hyperliquid orders cancel <OID>",
            ),
            (OrderType::TwapSlice, "Twap Slice", "orders twap-cancel"),
            (OrderType::VaultClose, "Vault Close", "orders cancel <OID>"),
            (
                OrderType::SpotDustConversion,
                "Spot Dust Conversion",
                "orders cancel <OID>",
            ),
        ] {
            let err = order_type_placement(&basic_order(order_type), None, None).unwrap_err();

            assert!(matches!(err, CliError::Unsupported(_)));
            assert_eq!(err.exit_code(), 13);
            let message = err.to_string();
            assert!(message.contains(name), "{message}");
            assert!(message.contains(hint), "{message}");
        }
    }

    #[test]
    fn cancel_all_sends_exactly_the_order_ids_it_planned() {
        let plan = CancelAllOrdersPlan {
            cancels: vec![Cancel { asset: 0, oid: 11 }, Cancel { asset: 3, oid: 12 }],
            summary_coin: "ALL".to_string(),
        };
        assert_eq!(plan.order_ids(), [11, 12]);
        let Some(Action::Cancel(batch)) = plan.action() else {
            panic!("a plan with orders sends a cancel");
        };
        assert_eq!(
            batch
                .cancels
                .iter()
                .map(|cancel| (cancel.asset, cancel.oid))
                .collect::<Vec<_>>(),
            [(0, 11), (3, 12)]
        );

        let empty = CancelAllOrdersPlan {
            cancels: Vec::new(),
            summary_coin: "ALL".to_string(),
        };
        assert!(empty.order_ids().is_empty());
        assert!(empty.action().is_none());
    }
}
