//! Orders that already exist: finding them, and planning their cancel or
//! replacement.

use either::Either;
use hypersdk::hypercore::types::{
    Action, BasicOrder, BatchCancel, BatchCancelCloid, BatchModify, Cancel, CancelByCloid, Modify,
    OrderRequest, OrderType, OrderTypePlacement, Side, TimeInForce, TpSl,
};
use hypersdk::hypercore::{Cloid, HttpClient, OidOrCloid};
use hypersdk::{Address, Decimal};

use super::args::{CancelAllArgs, CancelArgs, ModifyArgs};
use super::cloid::{parse_cloid, parse_lookup_cloid, wire_cloid};
use super::live_order::CloidAsset;
use super::planning::OrderDryRunPlan;
use super::rendering::ModifyConfirmation;
use super::validation::MarketPrecision;
use crate::assets::asset_id::{
    outcome_coin, parse_spot_coin_key, spot_asset_index, spot_coin_key, spot_internal_coin_key,
};
use crate::assets::{AssetQuery, AssetResolver, Market, parse_asset_query, qualify_dex_asset};
use crate::commands::support::canonical_dex;
use crate::errors::{CliError, from_sdk};

#[derive(Debug, Clone)]
pub(super) enum OrderIdentifier {
    Oid(u64),
    Cloid { raw: String, parsed: Cloid },
}

impl OrderIdentifier {
    /// The order a cancel names. A short or unprefixed CLOID is padded, like a
    /// lookup.
    pub(super) fn for_cancel(order_id: Option<u64>, cloid: Option<&str>) -> Result<Self, CliError> {
        Self::from_flags(order_id, cloid, "cancel", parse_lookup_cloid)
    }

    /// The order a modify names. The CLOID must be exactly 16 bytes, since the
    /// replacement is sent under it.
    pub(super) fn for_modify(order_id: Option<u64>, cloid: Option<&str>) -> Result<Self, CliError> {
        Self::from_flags(order_id, cloid, "modify", parse_cloid)
    }

    fn from_flags(
        order_id: Option<u64>,
        cloid: Option<&str>,
        command: &str,
        parse: fn(&str) -> Result<Cloid, CliError>,
    ) -> Result<Self, CliError> {
        if let Some(order_id) = order_id {
            return Ok(Self::Oid(order_id));
        }
        let raw = cloid.ok_or_else(|| {
            CliError::Configuration(format!("orders {command} requires ORDER_ID or --cloid"))
        })?;
        Ok(Self::Cloid {
            raw: raw.to_string(),
            parsed: parse(raw)?,
        })
    }

    fn to_status_identifier(&self) -> OidOrCloid {
        match self {
            Self::Oid(oid) => Either::Left(*oid),
            Self::Cloid { parsed, .. } => Either::Right(*parsed),
        }
    }

    fn display(&self) -> String {
        match self {
            Self::Oid(oid) => oid.to_string(),
            Self::Cloid { raw, .. } => raw.clone(),
        }
    }
}

async fn lookup_order(
    client: &HttpClient,
    user: Address,
    identifier: &OrderIdentifier,
) -> Result<BasicOrder, CliError> {
    let order = client
        .order_status(user, identifier.to_status_identifier())
        .await
        .map_err(from_sdk)?
        .ok_or_else(|| CliError::Unsupported(format!("unknown order {}", identifier.display())))?;
    Ok(order.order)
}

/// The protocol asset an existing order trades, and how its prices and sizes are
/// constrained when the market is known.
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
    client: &HttpClient,
    resolver: &AssetResolver,
    order: &BasicOrder,
) -> Result<OrderAsset, CliError> {
    if let Some(internal_id) = parse_spot_coin_key(&order.coin) {
        return Ok(
            match resolver.spot_by_protocol_asset_id(spot_asset_index(internal_id)) {
                Some(spot) => OrderAsset {
                    index: spot.index,
                    precision: Some(MarketPrecision::of(&Market::Spot(spot))),
                },
                None => OrderAsset {
                    index: internal_id,
                    precision: None,
                },
            },
        );
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
    client: &HttpClient,
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
            "on_behalf_of": args.on_behalf_of,
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
    client: &HttpClient,
    resolver: &AssetResolver,
    args: &CancelAllArgs,
) -> Result<Option<CoinFilter>, CliError> {
    let Some(coin) = args.coin.as_deref() else {
        return Ok(None);
    };
    if let Some(internal_id) = parse_spot_coin_key(coin) {
        return Ok(Some(CoinFilter::on_default_dex(spot_internal_coin_key(
            internal_id,
        ))));
    }

    let query = match parse_asset_query(coin) {
        AssetQuery::Outcome(_) => coin.to_string(),
        _ => {
            let dex = canonical_dex(client, args.dex.as_deref()).await?;
            qualify_dex_asset(dex.as_deref(), coin)
        }
    };
    Ok(Some(
        match resolver.resolve_or_fetch(client, &query).await? {
            Market::Perp(perp) => CoinFilter {
                coin: perp.display_name(),
                dex: perp.dex.clone(),
            },
            Market::Spot(spot) => CoinFilter::on_default_dex(spot_coin_key(spot.index)),
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

/// The open orders `cancel-all` will cancel, each with the asset to cancel it on.
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
    client: &HttpClient,
    resolver: &AssetResolver,
    user: Address,
    args: &CancelAllArgs,
) -> Result<CancelAllOrdersPlan, CliError> {
    let coin_filter = resolve_cancel_all_coin_filter(client, resolver, args).await?;
    let open_orders = client
        .open_orders(
            user,
            coin_filter.as_ref().and_then(|filter| filter.dex.clone()),
        )
        .await
        .map_err(from_sdk)?;
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
    client: &HttpClient,
    resolver: &AssetResolver,
    args: &CancelAllArgs,
) -> Result<OrderDryRunPlan, CliError> {
    let coin_filter = resolve_cancel_all_coin_filter(client, resolver, args).await?;
    Ok(OrderDryRunPlan::new(
        "cancel_open_orders",
        serde_json::json!({
            "coin": args.coin,
            "dex": args.dex,
            "on_behalf_of": args.on_behalf_of,
            "resolved_coin_filter": coin_filter.map(|filter| filter.coin),
        }),
    ))
}

pub(super) struct ModifyOrderPlan {
    pub(super) action: Action,
    pub(super) confirmation: ModifyConfirmation,
}

/// Check the flags of `orders modify` before anything is read, and name the
/// order they modify.
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
    client: &HttpClient,
    resolver: &AssetResolver,
    user: Address,
    identifier: &OrderIdentifier,
    args: &ModifyArgs,
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
            return Err(CliError::Configuration(
                "orders modify without --always-place can only place an ALO or non-executable GTC replacement; add --always-place to modify trigger orders or place an IOC replacement"
                    .to_string(),
            ));
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
        action: Action::BatchModify(BatchModify {
            modifies: vec![Modify {
                oid: identifier.to_status_identifier(),
                order: replacement,
            }],
            always_place: args.always_place,
        }),
        confirmation: ModifyConfirmation {
            coin,
            status: "modified".to_string(),
            order_id: Some(order.oid),
            cloid: args
                .cloid
                .clone()
                .or_else(|| order.cloid.map(|cloid| format!("{cloid:#x}"))),
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
            "on_behalf_of": args.on_behalf_of,
            "price": args.price.map(|value| value.to_string()),
            "trigger_price": args.trigger_price.map(|value| value.to_string()),
            "size": args.size.map(|value| value.to_string()),
            "always_place": args.always_place,
        }),
    )
}

/// The placement a modified order is replaced with: the order's own type, with
/// any new trigger or limit price applied.
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
        // A generic `Trigger` order reports no TP/SL side; it has always been
        // replaced as a stop.
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
        OrderType::TrailingStopMarket => Err(CliError::Unsupported(
            "orders modify can't change a Trailing Stop Market order. Cancel it with `hyperliquid orders cancel <OID>`, then place a new one with `hyperliquid orders trailing-stop`."
                .to_string(),
        )),
        OrderType::TwapSlice => Err(CliError::Unsupported(
            "orders modify can't change a Twap Slice order. Cancel the TWAP with `hyperliquid orders twap-cancel`, then re-create it with `hyperliquid orders twap-create`."
                .to_string(),
        )),
        OrderType::VaultClose => Err(CliError::Unsupported(
            "orders modify can't change a Vault Close order. The exchange places it when a vault closes positions; cancel it with `hyperliquid orders cancel <OID>` and place a regular order instead."
                .to_string(),
        )),
        OrderType::SpotDustConversion => Err(CliError::Unsupported(
            "orders modify can't change a Spot Dust Conversion order. Cancel it with `hyperliquid orders cancel <OID>` and place a regular order instead."
                .to_string(),
        )),
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
    fn a_cancel_pads_a_short_cloid_and_a_modify_does_not() {
        let padded = "0x000000000000000000000000deadbeef"
            .parse::<Cloid>()
            .unwrap();
        match OrderIdentifier::for_cancel(None, Some("0xdeadbeef")).unwrap() {
            OrderIdentifier::Cloid { raw, parsed } => {
                assert_eq!(raw, "0xdeadbeef");
                assert_eq!(parsed, padded);
            }
            OrderIdentifier::Oid(_) => panic!("a cloid names the order"),
        }
        let err = OrderIdentifier::for_modify(None, Some("0xdeadbeef")).unwrap_err();
        assert!(err.to_string().contains("exactly 16 bytes"), "{err}");
    }

    #[test]
    fn an_order_id_wins_over_a_cloid_and_one_of_them_is_required() {
        assert!(matches!(
            OrderIdentifier::for_modify(Some(5), Some("not-a-cloid")).unwrap(),
            OrderIdentifier::Oid(5)
        ));
        let err = OrderIdentifier::for_cancel(None, None).unwrap_err();
        assert_eq!(
            err.to_string(),
            "Configuration error: orders cancel requires ORDER_ID or --cloid"
        );
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
